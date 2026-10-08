use std::{collections::BTreeMap, path::Path};

use anyhow::{ensure, Result};
use nasa::yml::{
    strict::{ConfigLoader, ConfigPath, FilePattern, LoadPolicy, SourceDocument, ValueHint},
    ConfigFormat,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use zeroize::Zeroizing;

use crate::{
    catalog::config::{valid_id, TelegramConfig},
    catalog::reader::FileReader,
};

const MAX_DOCUMENT: usize = 1_048_576;
pub(crate) const MAX_GENERATION: u64 = 9_007_199_254_740_991;

/// 引导文件固定来源与轮询预算，外部目录不能改变这些控制项。
#[derive(Clone)]
pub struct CatalogSource {
    bootstrap: Value,
    imports: Vec<FileImport>,
    pub poll_interval_ms: u64,
    pub load_timeout_ms: u64,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct FileImport {
    file: String,
    optional: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Imports {
    imports: Vec<FileImport>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Watch {
    poll_interval_ms: u64,
    load_timeout_ms: u64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SecretFile {
    file: String,
    sha256: String,
}

/// 完整配置与已经校验内容摘要的凭据只能一起交给运行目录。
#[derive(Serialize, Deserialize)]
pub struct Candidate {
    pub revision: u64,
    pub fingerprint: [u8; 32],
    pub config: TelegramConfig,
    materials: BTreeMap<String, Zeroizing<String>>,
}

impl Candidate {
    /// 业务作用：只允许凭据引用命中本候选已校验的文件材料。
    /// 参数说明：`reference` 是 secret:// 别名。
    /// 返回：返回可清零的材料副本；未声明的引用使整份候选失败。
    pub fn resolve(&self, reference: &str) -> Result<Zeroizing<String>> {
        let material = reference
            .strip_prefix("secret://")
            .and_then(|id| self.materials.get(id))
            .ok_or_else(|| anyhow::anyhow!("凭据引用未解析"))?;
        Ok(Zeroizing::new(material.to_string()))
    }

    /// 业务作用：拆分候选目录与同代材料供发送资源装配，避免再次读取可变文件。
    /// 参数说明：无。
    /// 返回：配置、凭据映射、部署代号及内容指纹的唯一所有权。
    pub fn into_parts(
        self,
    ) -> (
        TelegramConfig,
        BTreeMap<String, Zeroizing<String>>,
        u64,
        [u8; 32],
    ) {
        (self.config, self.materials, self.revision, self.fingerprint)
    }
}

/// 失败也保留已读到的部署代号，错误摘要不包含文件正文或路径。
pub struct LoadAttempt {
    pub desired_revision: Option<u64>,
    pub result: Result<Candidate>,
}

impl CatalogSource {
    /// 业务作用：通过受管读取者固定目录所需的可信引导设置。
    /// 参数说明：`reader` 隔离可能阻塞的文件系统操作。
    /// 返回：来源合法且引导文件不含业务目录时返回加载器；否则拒绝启动。
    pub(crate) async fn standard(reader: &mut FileReader) -> Result<Self> {
        let documents = bootstrap_documents(reader).await?;
        let bootstrap = bootstrap_tree(&documents)?;
        let imports: Imports =
            serde_json::from_value(bootstrap.get("yml").cloned().unwrap_or(Value::Null))
                .map_err(|_| anyhow::anyhow!("yml.imports 必须是显式 file 与 optional 列表"))?;
        ensure!(
            (1..=16).contains(&imports.imports.len())
                && imports.imports.iter().any(|i| !i.optional),
            "必须声明 1..=16 个文件来源并至少包含一个必需来源"
        );
        let mut paths = std::collections::BTreeSet::new();
        for import in &imports.imports {
            ensure!(
                Path::new(&import.file).is_absolute(),
                "导入来源必须使用绝对路径"
            );
            FilePattern::new(Path::new(&import.file))
                .map_err(|_| anyhow::anyhow!("导入路径或模式无效"))?;
            // 文件格式是来源声明的约束，不能因可选模式暂时为空而推迟到文件出现后才拒绝。
            ensure!(
                matches!(
                    Path::new(&import.file)
                        .extension()
                        .and_then(|ext| ext.to_str()),
                    Some("yml" | "yaml")
                ),
                "目录路径或模式必须使用 .yml 或 .yaml 扩展名"
            );
            ensure!(paths.insert(&import.file), "文件来源不能重复");
        }
        let watch: Watch = serde_json::from_value(
            bootstrap
                .get("catalog_watch")
                .cloned()
                .unwrap_or(Value::Null),
        )
        .map_err(|_| anyhow::anyhow!("catalog_watch 配置结构无效"))?;
        ensure!(
            bootstrap
                .pointer("/config_watch/enabled")
                .and_then(Value::as_bool)
                == Some(true)
                && (100..=1000).contains(&watch.poll_interval_ms)
                && (100..=3000).contains(&watch.load_timeout_ms),
            "配置观察必须启用，轮询为 100..=1000 毫秒，读取预算为 100..=3000 毫秒"
        );
        Ok(Self {
            bootstrap,
            imports: imports.imports,
            poll_interval_ms: watch.poll_interval_ms,
            load_timeout_ms: watch.load_timeout_ms,
        })
    }

    /// 业务作用：确认目录使用的引导设置与 napp 已启动的组件使用同一份配置。
    /// 参数说明：`application` 是框架初始化时固定的配置快照。
    /// 返回：引导树一致才允许发布资源；读取期间引导发生变化时拒绝启动。
    pub(crate) fn validate_bootstrap(&self, application: &Value) -> Result<()> {
        let mut bootstrap = application.clone();
        if let Some(object) = bootstrap.as_object_mut() {
            for key in ["telegram", "secrets", "generation"] {
                object.remove(key);
            }
        }
        // 两个读取者必须共享监听、身份和预算，不能让目录基于另一份引导配置开放接流。
        ensure!(
            bootstrap == self.bootstrap,
            "目录与应用的引导配置不一致，拒绝启动"
        );
        Ok(())
    }

    /// 业务作用：整批读取文件、环境覆盖与绑定摘要的凭据，形成可原子发布的候选。
    /// 参数说明：`reader` 提供本轮独占的隔离文件通道。
    /// 返回：所有文件部署代号一致且凭据摘要匹配才成功；失败保留可观测代号。
    pub(crate) async fn load(&self, reader: &mut FileReader) -> LoadAttempt {
        let mut desired_revision = None;
        let result = self.load_inner(&mut desired_revision, reader).await;
        LoadAttempt {
            desired_revision,
            result,
        }
    }

    /// 业务作用：按 naml 的分层规则合并已读取的内存文档，禁止业务文件越权改变引导项。
    /// 参数说明：`desired` 接收第一个有效部署代号；`reader` 只执行受限文件读取。
    /// 返回：配置、凭据和来源复验全部成功后返回完整候选。
    async fn load_inner(
        &self,
        desired: &mut Option<u64>,
        reader: &mut FileReader,
    ) -> Result<Candidate> {
        let mut documents = bootstrap_documents(reader).await?;
        let boot = bootstrap_tree(&documents)?;
        ensure!(
            boot == self.bootstrap,
            "引导配置或活动 profile 已改变，需要恢复原文件或重启"
        );
        let mut observed = Vec::new();
        let mut identities = std::collections::BTreeSet::new();
        let plan = self.expand(reader).await?;
        ensure!(plan.len() <= 64, "目录来源数量超过上限");
        let mut total_bytes = 0usize;
        for (path, optional) in &plan {
            let bytes = reader.read(path, MAX_DOCUMENT, *optional, false).await?;
            let Some(bytes) = bytes else {
                observed.push((path.clone(), *optional, None, None));
                continue;
            };
            ensure!(!bytes.is_empty(), "外部 YAML 文件不能为空");
            total_bytes = total_bytes.saturating_add(bytes.len());
            ensure!(total_bytes <= 8 * MAX_DOCUMENT, "目录总字节数超过上限");
            let identity = reader
                .identity()
                .ok_or_else(|| anyhow::anyhow!("目录来源身份缺失"))?;
            ensure!(identities.insert(identity), "目录来源身份不能重复");
            let source = SourceDocument::new("catalog", ConfigFormat::Yaml, bytes.to_vec());
            let parsed = source.parse(documents.len(), &catalog_policy()?)?;
            let document = parsed.tree();
            let object = document
                .as_object()
                .ok_or_else(|| anyhow::anyhow!("外部 YAML 必须是对象"))?;
            // 外部文件只能提供业务目录，不能接管应用身份、监听、观察器或追加导入来源。
            ensure!(
                object
                    .keys()
                    .all(|key| matches!(key.as_str(), "generation" | "telegram" | "secrets")),
                "外部 YAML 含未知字段或引导专用字段"
            );
            let revision = document
                .get("generation")
                .and_then(Value::as_u64)
                // 状态接口和浮点指标必须表达同一个精确代号，不能把越界值作为期望状态发布。
                .filter(|n| (1..=MAX_GENERATION).contains(n))
                .ok_or_else(|| anyhow::anyhow!("generation 必须为 1..=9007199254740991 的整数"))?;
            ensure!(
                desired.is_none_or(|n| n == revision),
                "导入文件的 generation 不一致"
            );
            *desired = Some(revision);
            documents.push(source);
            observed.push((path.clone(), *optional, Some(bytes), Some(identity)));
        }
        tokio::task::yield_now().await;
        let tree = merge_documents(&documents, true)?;
        let revision = desired.ok_or_else(|| anyhow::anyhow!("必需目录为空"))?;
        ensure!(
            tree.get("generation").and_then(Value::as_u64) == Some(revision),
            "环境覆盖不能改变 generation"
        );
        for (key, value) in self.bootstrap.as_object().into_iter().flatten() {
            ensure!(tree.get(key) == Some(value), "业务配置不能改变引导设置");
        }
        let config = TelegramConfig::parse(tree.get("telegram").cloned().unwrap_or(Value::Null))?;
        let shutdown_budget = self
            .bootstrap
            .pointer("/application/shutdown_timeout_ms")
            .and_then(Value::as_u64)
            .unwrap_or(15_000);
        ensure!(
            config.dispatcher.shutdown_timeout_ms.saturating_add(1000) <= shutdown_budget / 2,
            "应用停机预算的一半必须覆盖目录排空预算并留出 1000 毫秒余量"
        );
        let files: BTreeMap<String, SecretFile> = serde_json::from_value(
            tree.get("secrets")
                .cloned()
                .unwrap_or_else(|| serde_json::json!({})),
        )
        .map_err(|_| anyhow::anyhow!("secrets 仅接受 file 与 sha256 字段"))?;
        ensure!(files.len() <= 1152, "凭据数量超过目录上限");
        let mut materials = BTreeMap::new();
        for (id, file) in files {
            ensure!(valid_id(&id), "凭据别名格式无效");
            validate_path(&file.file)?;
            ensure!(
                file.sha256.len() == 64 && file.sha256.bytes().all(|b| b.is_ascii_hexdigit()),
                "凭据 sha256 格式无效"
            );
            let bytes = reader
                .read(&file.file, 513, false, false)
                .await?
                .unwrap_or_default();
            ensure!(!bytes.is_empty(), "凭据文件不能为空");
            let digest = format!("{:x}", Sha256::digest(bytes.as_slice()));
            // YAML 固定确切材料摘要，ConfigMap 与 Secret 分别切换时不可能发布新目录配旧凭据。
            ensure!(
                digest.eq_ignore_ascii_case(&file.sha256),
                "凭据内容与目录声明的 sha256 不一致"
            );
            let value = std::str::from_utf8(&bytes)
                .map_err(|_| anyhow::anyhow!("凭据必须是 UTF-8 文本"))?;
            materials.insert(
                id,
                Zeroizing::new(value.trim_end_matches(['\r', '\n']).to_owned()),
            );
        }
        // 再读导入链以识别原子替换和可选来源增删；一轮不能混用不同文件集合。
        ensure!(
            self.expand(reader).await? == plan,
            "配置来源集合在读取期间发生变化"
        );
        for (path, optional, previous, identity) in observed {
            ensure!(
                reader.read(&path, MAX_DOCUMENT, optional, false).await? == previous
                    && reader.identity() == identity,
                "配置来源在读取期间发生变化"
            );
        }
        ensure!(
            bootstrap_tree(&bootstrap_documents(reader).await?)? == self.bootstrap,
            "引导配置在读取期间发生变化"
        );
        config.validate_materials(&materials)?;
        let fingerprint = Sha256::digest(serde_json::to_vec(&tree)?).into();
        Ok(Candidate {
            revision,
            fingerprint,
            config,
            materials,
        })
    }
    /// 业务作用：通过隔离进程取得模式集合，保持声明顺序和单组自然排序。
    /// 参数说明：`reader` 是本轮有期限的读取者。
    /// 返回：有序路径及缺失策略；必需空组、重复路径或非法扩展名拒绝整轮。
    async fn expand(&self, reader: &mut FileReader) -> Result<Vec<(String, bool)>> {
        let mut output = Vec::new();
        let mut unique = std::collections::BTreeSet::new();
        let policy = catalog_policy()?;
        for import in &self.imports {
            let pattern = FilePattern::new(Path::new(&import.file))?;
            let paths = if pattern.is_glob() {
                let directory = pattern
                    .directory()
                    .to_str()
                    .ok_or_else(|| anyhow::anyhow!("导入目录必须为 UTF-8"))?;
                let names = reader.list(directory, import.optional).await?;
                let files = pattern.expand_names(names, &policy.limits)?;
                ensure!(
                    import.optional || !files.is_empty(),
                    "必需导入模式没有匹配文件"
                );
                files
            } else {
                vec![pattern.path()]
            };
            for path in paths {
                ensure!(
                    output.len() < 64 && unique.insert(path.clone()),
                    "目录来源过多或重复"
                );
                ensure!(
                    matches!(
                        path.extension().and_then(|ext| ext.to_str()),
                        Some("yml" | "yaml")
                    ),
                    "目录只接受 YAML 文件"
                );
                output.push((
                    path.to_str()
                        .ok_or_else(|| anyhow::anyhow!("导入路径必须为 UTF-8"))?
                        .to_owned(),
                    import.optional && !pattern.is_glob(),
                ));
            }
        }
        Ok(output)
    }
}

/// 业务作用：将一份凭据材料绑定到精确文件，拒绝路径穿越和多文件模式。
/// 参数说明：`value` 是凭据声明的绝对路径。
/// 返回：普通绝对路径成功，空路径、通配符或父目录跳转被拒绝。
fn validate_path(value: &str) -> Result<()> {
    let path = Path::new(value);
    ensure!(
        path.is_absolute()
            && !value.contains(['*', '?', '[', ']'])
            && !path
                .components()
                .any(|part| matches!(part, std::path::Component::ParentDir)),
        "凭据必须使用无通配符、无父目录跳转的绝对路径"
    );
    Ok(())
}

/// 业务作用：通过隔离通道读取主文件及活动 profile，拒绝缺失或多格式歧义。
/// 参数说明：`reader` 是本轮唯一文件读取者。
/// 返回：按主文件、profile 顺序返回纯内存文档；非法来源拒绝整轮加载。
async fn bootstrap_documents(reader: &mut FileReader) -> Result<Vec<SourceDocument>> {
    let loader = bootstrap_loader()?;
    let base = loader
        .base_path()
        .ok_or_else(|| anyhow::anyhow!("引导路径不可用"))?;
    let path = base
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("引导路径必须为 UTF-8"))?;
    let bytes = reader
        .read(path, MAX_DOCUMENT, false, false)
        .await?
        .ok_or_else(|| anyhow::anyhow!("引导文件不可用"))?;
    let mut documents = vec![SourceDocument::new(
        "application",
        ConfigFormat::Yaml,
        bytes.to_vec(),
    )];
    if let Some(profile) = loader.selected_profile()? {
        let directory = base
            .parent()
            .ok_or_else(|| anyhow::anyhow!("引导目录不可用"))?;
        let base = directory.join(format!("application-{profile}"));
        let mut candidates = vec![base.clone()];
        if base.extension().is_none() {
            candidates.extend(
                ["toml", "json", "yaml", "yml"]
                    .iter()
                    .map(|extension| base.with_extension(extension)),
            );
        }
        let mut selected = None;
        for path in candidates {
            let path_text = path
                .to_str()
                .ok_or_else(|| anyhow::anyhow!("profile 路径必须为 UTF-8"))?;
            let Some(bytes) = reader.read(path_text, MAX_DOCUMENT, true, false).await? else {
                continue;
            };
            ensure!(selected.is_none(), "活动 profile 同时命中多个文件");
            let format = ConfigFormat::from_extension(
                path.extension()
                    .and_then(|ext| ext.to_str())
                    .unwrap_or("yaml"),
            )?;
            selected = Some(SourceDocument::new("profile", format, bytes.to_vec()));
        }
        documents.push(selected.ok_or_else(|| anyhow::anyhow!("活动 profile 文件缺失"))?);
    }
    Ok(documents)
}

/// 业务作用：在隔离读取后复用 naml 的严格解析、合并、环境及表达式处理。
/// 参数说明：`documents` 按覆盖顺序给出；`environment` 指定是否执行最终环境层和表达式。
/// 返回：独立完整树，解析器不会重新打开来源或读取另一份进程环境。
fn merge_documents(documents: &[SourceDocument], environment: bool) -> Result<Value> {
    let loader = bootstrap_loader()?;
    let mut policy = catalog_policy()?;
    if !environment {
        policy.environment_overlay = false;
        policy.environment_fallback = false;
        policy.placeholders = false;
    }
    Ok(ConfigLoader::memory(loader.environment().clone())
        .policy(policy)
        .load_documents(documents)?
        .into_tree())
}

/// 业务作用：在宏 preflight 前固定同一环境与本地主路径，业务目录仍交给受管读取者。
/// 参数说明：无。
/// 返回：只读取引导文档的严格加载器；文件目录、generation 与凭据约束由目录生命周期执行。
pub fn bootstrap_loader() -> nasa::yml::strict::Result<ConfigLoader> {
    static LOADER: std::sync::OnceLock<nasa::yml::strict::Result<ConfigLoader>> =
        std::sync::OnceLock::new();
    LOADER
        .get_or_init(|| {
            let mut policy = LoadPolicy::default();
            for field in [
                "application.worker_threads",
                "application.startup_timeout_ms",
                "application.shutdown_timeout_ms",
                "server.port",
                "server.health",
                "server.graceful_shutdown_timeout_ms",
                "rest_discovery.enabled",
                "rest_discovery.registration.port",
                "config_watch.enabled",
                "catalog_watch.poll_interval_ms",
                "catalog_watch.load_timeout_ms",
            ] {
                policy
                    .hints
                    .insert(ConfigPath::parse(field)?, ValueHint::Scalar);
            }
            Ok(ConfigLoader::standard()?
                .local_imports(false)
                .policy(policy))
        })
        .clone()
}

/// 业务作用：为目录内存装配设置共享预算，不能由导入文件扩大。
/// 参数说明：无。
/// 返回：沿用引导类型规则的有界解析策略。
fn catalog_policy() -> Result<LoadPolicy> {
    let mut policy = bootstrap_loader()?.load_policy().clone();
    policy.limits.source_bytes = MAX_DOCUMENT;
    policy.limits.total_bytes = 10 * MAX_DOCUMENT;
    policy.limits.sources = 66;
    Ok(policy)
}

/// 业务作用：区分文件内禁止的业务配置与允许的最终环境覆盖，固定引导控制面。
/// 参数说明：`documents` 是已隔离读取的引导与 profile 文档。
/// 返回：仅含引导节点的解析树；文件中夹带目录或凭据时失败。
fn bootstrap_tree(documents: &[SourceDocument]) -> Result<Value> {
    ensure!(
        !std::env::vars_os().any(|(key, _)| key
            .to_string_lossy()
            .starts_with("TELEGRAM_BOOTSTRAP_FILE_ONLY")),
        "保留环境变量前缀不能用于部署配置"
    );
    let raw = merge_documents(documents, false)?;
    ensure!(
        ["telegram", "secrets", "generation"]
            .iter()
            .all(|key| raw.get(key).is_none()),
        "引导文件不能包含目录、凭据或部署代号"
    );
    let mut resolved = merge_documents(documents, true)?;
    let object = resolved
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("引导配置必须为对象"))?;
    for key in ["telegram", "secrets", "generation"] {
        object.remove(key);
    }
    Ok(resolved)
}

use std::{collections::BTreeMap, path::Path};

use anyhow::{ensure, Result};
use config::{Config, Environment, File, FileFormat};
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
#[derive(Clone, Serialize, Deserialize)]
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
            validate_path(&import.file)?;
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
        for import in &self.imports {
            let bytes = reader
                .read(&import.file, MAX_DOCUMENT, import.optional, false)
                .await?;
            let Some(bytes) = bytes else {
                observed.push((import, None));
                continue;
            };
            ensure!(!bytes.is_empty(), "外部 YAML 文件不能为空");
            let document: Value = serde_yaml::from_slice(&bytes)
                .map_err(|_| anyhow::anyhow!("外部 YAML 结构无效"))?;
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
            let content = String::from_utf8(bytes.to_vec())
                .map_err(|_| anyhow::anyhow!("外部 YAML 必须为 UTF-8"))?;
            documents.push((content, FileFormat::Yaml));
            observed.push((import, Some(bytes)));
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
        for (import, previous) in observed {
            ensure!(
                reader
                    .read(&import.file, MAX_DOCUMENT, import.optional, false)
                    .await?
                    == previous,
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
}

/// 业务作用：限制受信挂载路径语法，拒绝路径穿越和未实现的通配符。
/// 参数说明：`value` 是引导声明或凭据声明的绝对路径。
/// 返回：普通绝对路径成功，空路径、通配符或父目录跳转被拒绝。
fn validate_path(value: &str) -> Result<()> {
    let path = Path::new(value);
    ensure!(
        path.is_absolute()
            && !value.contains(['*', '?', '[', ']'])
            && !path
                .components()
                .any(|part| matches!(part, std::path::Component::ParentDir)),
        "配置与凭据必须使用无通配符、无父目录跳转的绝对路径"
    );
    Ok(())
}

/// 业务作用：通过隔离通道读取主文件及活动 profile，保持标准来源的格式优先级。
/// 参数说明：`reader` 是本轮唯一文件读取者。
/// 返回：按主文件、profile 顺序返回纯内存文档；非法来源拒绝整轮加载。
async fn bootstrap_documents(reader: &mut FileReader) -> Result<Vec<(String, FileFormat)>> {
    let bytes = reader
        .read("zcf/application.yml", MAX_DOCUMENT, false, false)
        .await?
        .ok_or_else(|| anyhow::anyhow!("引导文件不可用"))?;
    let mut documents = vec![(document_text(&bytes), FileFormat::Yaml)];
    if let Some(profile) = std::env::var("APP_PROFILE")
        .ok()
        .map(|p| p.trim().to_owned())
        .filter(|p| !p.is_empty())
    {
        let base = format!("zcf/application-{profile}");
        let mut candidates = vec![base.clone()];
        candidates.extend(
            ["toml", "json", "yaml", "yml"]
                .iter()
                .map(|extension| format!("{base}.{extension}")),
        );
        for path in candidates {
            let Some(bytes) = reader.read(&path, MAX_DOCUMENT, true, true).await? else {
                continue;
            };
            let format = match Path::new(&path)
                .extension()
                .and_then(|e| e.to_str())
                .map(str::to_ascii_lowercase)
                .as_deref()
            {
                Some("toml") => FileFormat::Toml,
                Some("json") => FileFormat::Json,
                Some("yml" | "yaml") => FileFormat::Yaml,
                _ => anyhow::bail!("活动 profile 文件格式无效"),
            };
            documents.push((document_text(&bytes), format));
            break;
        }
    }
    Ok(documents)
}

/// 业务作用：将引导来源字节转换为与 naml 标准加载一致的文本。
/// 参数说明：`bytes` 是已验证大小的文件内容。
/// 返回：去除 UTF-8 BOM 的文本，非法 UTF-8 字节按标准加载规则替换。
fn document_text(bytes: &[u8]) -> String {
    let bytes = bytes.strip_prefix(b"\xef\xbb\xbf").unwrap_or(bytes);
    String::from_utf8_lossy(bytes).into_owned()
}

/// 业务作用：在内存中完成叶子级合并、环境覆盖与 naml 占位符解析，避免解析器重新打开文件。
/// 参数说明：`documents` 是按优先级排列的完整文档；`environment` 决定是否应用最终环境覆盖及占位符。
/// 返回：完整配置树；解析失败只返回脱敏摘要，不发布半份目录。
fn merge_documents(documents: &[(String, FileFormat)], environment: bool) -> Result<Value> {
    let mut builder = Config::builder();
    for (text, format) in documents {
        builder = builder.add_source(File::from_str(text, *format));
    }
    let base = builder
        .build()
        .map_err(|_| anyhow::anyhow!("配置文档合并失败"))?;
    let baseline: Value = base
        .clone()
        .try_deserialize()
        .map_err(|_| anyhow::anyhow!("配置结构无效"))?;
    if !environment {
        return Ok(baseline);
    }
    let mut tree: Value = Config::builder()
        .add_source(base)
        .add_source(
            Environment::with_prefix("APP")
                .separator("__")
                .try_parsing(true),
        )
        .build()
        .and_then(|c| c.try_deserialize())
        .map_err(|_| anyhow::anyhow!("环境覆盖合并失败"))?;
    // 文件中已声明为字符串的叶子保持字符串类型，数字形式的凭据和标识不能被环境解析改成整数。
    for (key, value) in std::env::vars() {
        let normalized = key.to_ascii_lowercase();
        let Some(path) = normalized.strip_prefix("app__") else {
            continue;
        };
        let path = path.replace("__", ".");
        if path
            .split('.')
            .try_fold(&baseline, |node, key| node.get(key))
            .is_some_and(Value::is_string)
        {
            if let Some(target) = path
                .split('.')
                .try_fold(&mut tree, |node, key| node.get_mut(key))
            {
                *target = Value::String(value);
            }
        }
    }
    nasa::yml::resolve_placeholders(&mut tree)
        .map_err(|_| anyhow::anyhow!("完整目录占位符解析失败"))?;
    Ok(tree)
}

/// 业务作用：区分文件内禁止的业务配置与允许的最终环境覆盖，固定引导控制面。
/// 参数说明：`documents` 是已隔离读取的引导与 profile 文档。
/// 返回：仅含引导节点的解析树；文件中夹带目录或凭据时失败。
fn bootstrap_tree(documents: &[(String, FileFormat)]) -> Result<Value> {
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

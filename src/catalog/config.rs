use std::collections::{BTreeMap, BTreeSet};

use anyhow::{bail, ensure, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use zeroize::Zeroizing;

/// 机器人目录、调用方权限和有界 HTTP 资源的完整候选配置。
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TelegramConfig {
    #[serde(default)]
    pub http: HttpConfig,
    #[serde(default)]
    pub dispatcher: DispatcherConfig,
    pub bots: BTreeMap<String, BotConfig>,
    pub clients: BTreeMap<String, ClientConfig>,
}

/// 发送与入站缓冲预算；省略字段时递归应用默认值。
#[derive(Clone, Serialize, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct HttpConfig {
    pub api_base: String,
    pub connect_timeout_ms: u64,
    pub request_timeout_ms: u64,
    pub body_read_timeout_ms: u64,
    pub max_response_bytes: usize,
    pub max_body_bytes: usize,
    pub max_inflight: usize,
}

impl Default for HttpConfig {
    /// 业务作用：提供适合短文本通知的有界资源和网络等待预算。
    /// 参数说明：无。
    /// 返回：默认使用官方 HTTPS API、10 秒发送期限、5 秒正文读取期限及有限缓冲。
    fn default() -> Self {
        Self {
            api_base: "https://api.telegram.org".into(),
            connect_timeout_ms: 3_000,
            request_timeout_ms: 10_000,
            body_read_timeout_ms: 5_000,
            max_response_bytes: 65_536,
            max_body_bytes: 65_536,
            max_inflight: 64,
        }
    }
}

/// 一个稳定机器人身份及其可发送目的地；调用方不能提交任意 chat_id。
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BotConfig {
    #[serde(default)]
    pub description: String,
    pub token: String,
    pub destinations: BTreeMap<String, DestinationConfig>,
    #[serde(default)]
    pub default_destination: String,
    #[serde(default)]
    pub delivery: DeliveryConfig,
}

/// 机器人发送策略在每个实例内独立执行。
#[derive(Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct DeliveryConfig {
    pub min_interval_ms: u64,
    pub queue_capacity: usize,
    pub disable_notification: bool,
    pub protect_content: bool,
}

impl Default for DeliveryConfig {
    /// 业务作用：为群通知设置保守发送间隔，限制同一机器人并发占用。
    /// 参数说明：无。
    /// 返回：默认间隔 3100 毫秒、256 条待发消息，通知有声且允许保存。
    fn default() -> Self {
        Self {
            min_interval_ms: 3_100,
            queue_capacity: 256,
            disable_notification: false,
            protect_content: false,
        }
    }
}

/// 目录移除与应用停机的并行排空预算。
#[derive(Clone, Serialize, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct DispatcherConfig {
    pub shutdown_timeout_ms: u64,
}

impl Default for DispatcherConfig {
    /// 业务作用：为正常停机期间的内存消息排空设置有限等待时间。
    /// 参数说明：无。
    /// 返回：所有发送队列共享 20 秒排空期限，超时取消尚未完成的工作。
    fn default() -> Self {
        Self {
            shutdown_timeout_ms: 20_000,
        }
    }
}

/// 服务端维护的目标聊天与可选论坛话题。
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DestinationConfig {
    pub chat_id: ChatId,
    #[serde(default)]
    pub message_thread_id: Option<i64>,
}

/// 接受 Telegram 数字聊天 ID 或以 @ 开头的公开聊天名称。
#[derive(Serialize, Deserialize)]
#[serde(untagged)]
pub enum ChatId {
    Number(i64),
    Text(String),
}

impl ChatId {
    /// 业务作用：把 YAML 中两种聊天身份表示收敛为 Telegram 可接受的文本。
    /// 参数说明：无。
    /// 返回：保留有符号数字或公开名称，不包含机器人凭据。
    pub fn as_text(&self) -> String {
        match self {
            Self::Number(value) => value.to_string(),
            Self::Text(value) => value.clone(),
        }
    }
}

/// 每个调用方只拥有列出的机器人发送权限。
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClientConfig {
    pub credential: String,
    pub allowed_bots: BTreeSet<String>,
}

impl TelegramConfig {
    /// 业务作用：在基础设施启动前完成全表材料校验，避免无效身份进入资源装配阶段。
    /// 参数说明：`materials` 是已经通过文件摘要校验的完整材料映射。
    /// 返回：引用、机器人唯一身份和调用方凭据全部有效时成功。
    pub(crate) fn validate_materials(
        &self,
        materials: &BTreeMap<String, Zeroizing<String>>,
    ) -> Result<()> {
        let resolve = |reference: &str| {
            materials
                .get(reference.strip_prefix("secret://").unwrap_or_default())
                .ok_or_else(|| anyhow::anyhow!("凭据引用未解析"))
        };
        let mut identities = BTreeSet::new();
        for bot in self.bots.values() {
            ensure!(
                identities.insert(telegram_identity(resolve(&bot.token)?)?),
                "同一 Telegram 身份只能配置一个别名"
            );
        }
        let mut callers = BTreeSet::new();
        for caller in self.clients.values() {
            ensure!(
                callers.insert(credential_hash(resolve(&caller.credential)?)?),
                "不同调用方必须使用不同凭据"
            );
        }
        Ok(())
    }

    /// 业务作用：把 naml 已解析的业务配置树转换为强类型配置，阻止空值或未知键被静默忽略。
    /// 参数说明：`value` 是 napp 同代配置快照中的 telegram 节点，不是 YAML 原文。
    /// 返回：全表结构和约束有效时返回配置；错误只包含固定摘要，不回显输入值。
    pub fn parse(value: Value) -> Result<Self> {
        reject_null(&value)?;
        let config: Self = nasa::yml::strict::bind(value)
            .map_err(|_| anyhow::anyhow!("telegram 配置结构无效：请检查必填项、类型和未知字段"))?;
        config.validate()?;
        Ok(config)
    }

    /// 业务作用：在创建发送资源前确认身份、目的地、权限引用和预算一致。
    /// 参数说明：无。
    /// 返回：全部约束成立时成功；任何无效条目阻止整个目录发布。
    pub fn validate(&self) -> Result<()> {
        ensure!(self.bots.len() <= 128, "机器人数量不能超过 128");
        ensure!(self.clients.len() <= 1024, "调用方数量不能超过 1024");
        let http = &self.http;
        ensure!(
            (1..=60_000).contains(&http.request_timeout_ms),
            "request_timeout_ms 必须在 1..=60000"
        );
        ensure!(
            (1..=60_000).contains(&http.connect_timeout_ms),
            "connect_timeout_ms 必须在 1..=60000"
        );
        ensure!(
            (1..=60_000).contains(&http.body_read_timeout_ms),
            "body_read_timeout_ms 必须在 1..=60000"
        );
        ensure!(
            (1_024..=1_048_576).contains(&http.max_response_bytes),
            "max_response_bytes 必须在 1024..=1048576"
        );
        ensure!(
            (1_024..=1_048_576).contains(&http.max_body_bytes),
            "max_body_bytes 必须在 1024..=1048576"
        );
        ensure!(
            (1..=1024).contains(&http.max_inflight),
            "max_inflight 必须在 1..=1024"
        );
        ensure!(
            (1..=300_000).contains(&self.dispatcher.shutdown_timeout_ms),
            "shutdown_timeout_ms 必须在 1..=300000"
        );
        validate_api_base(&http.api_base)?;
        for (id, bot) in &self.bots {
            ensure!(valid_id(id), "机器人别名格式无效");
            ensure!(
                bot.description.chars().count() <= 256,
                "机器人 description 不能超过 256 字符"
            );
            validate_reference(&bot.token)?;
            ensure!(
                (1..=128).contains(&bot.destinations.len()),
                "每个机器人目的地数量必须在 1..=128"
            );
            ensure!(
                bot.default_destination.is_empty()
                    || bot.destinations.contains_key(&bot.default_destination),
                "默认目的地必须引用已配置别名"
            );
            ensure!(
                bot.delivery.min_interval_ms <= 60_000,
                "min_interval_ms 必须在 0..=60000"
            );
            ensure!(
                (1..=4096).contains(&bot.delivery.queue_capacity),
                "机器人 queue_capacity 必须在 1..=4096"
            );
            for (destination, target) in &bot.destinations {
                ensure!(valid_id(destination), "目的地别名格式无效");
                let chat_id = target.chat_id.as_text();
                let numeric = chat_id
                    .parse::<i64>()
                    .is_ok_and(|id| id != 0 && id.unsigned_abs() < (1_u64 << 52));
                let username = chat_id.strip_prefix('@').is_some_and(|name| {
                    (1..=64).contains(&name.len())
                        && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
                });
                ensure!(numeric || username, "chat_id 必须是非零聊天 ID 或 @名称");
                ensure!(
                    target.message_thread_id.is_none_or(|id| id > 0),
                    "message_thread_id 必须为正数"
                );
            }
        }
        ensure!(
            self.bots
                .values()
                .map(|bot| bot.delivery.queue_capacity)
                .sum::<usize>()
                <= 16_384,
            "全部机器人队列总容量不能超过 16384"
        );
        for (id, client) in &self.clients {
            ensure!(valid_id(id), "调用方别名格式无效");
            validate_reference(&client.credential)?;
            // 权限必须全部命中已发布目录，拼写错误不能隐式扩大到所有机器人。
            ensure!(
                client
                    .allowed_bots
                    .iter()
                    .all(|id| self.bots.contains_key(id)),
                "allowed_bots 必须引用已配置机器人"
            );
        }
        Ok(())
    }
}

/// 业务作用：校验机器人材料并提取跨 token 轮换保持稳定的发送身份。
/// 参数说明：`token` 为已经加载的私有材料。
/// 返回：规范化数字身份；错误摘要不包含材料。
pub(crate) fn telegram_identity(token: &str) -> Result<String> {
    let (identity, key) = token
        .split_once(':')
        .ok_or_else(|| anyhow::anyhow!("机器人 token 格式无效"))?;
    ensure!(
        token.len() <= 256
            && !identity.is_empty()
            && identity.bytes().all(|b| b.is_ascii_digit())
            && !key.is_empty()
            && key
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-'),
        "机器人 token 格式无效"
    );
    let identity = identity
        .parse::<u64>()
        .ok()
        .filter(|id| *id > 0)
        .ok_or_else(|| anyhow::anyhow!("机器人数字身份无效"))?;
    Ok(identity.to_string())
}

/// 业务作用：限定调用方材料格式并生成用于认证比较的摘要。
/// 参数说明：`credential` 为同代调用方材料。
/// 返回：无空白 ASCII 文本且长度有效时返回 SHA-256，否则拒绝候选。
pub(crate) fn credential_hash(credential: &str) -> Result<[u8; 32]> {
    ensure!(
        (32..=512).contains(&credential.len()) && credential.bytes().all(|b| b.is_ascii_graphic()),
        "调用方凭据必须为 32..=512 字节的无空白 ASCII 文本"
    );
    Ok(Sha256::digest(credential.as_bytes()).into())
}

/// 业务作用：限制外部可见别名的语法，使路径、授权和目录使用相同身份。
/// 参数说明：`value` 是机器人、目的地或调用方别名。
/// 返回：1..=64 个 ASCII 字母、数字、下划线或连字符且以字母数字开头时为 true。
pub fn valid_id(value: &str) -> bool {
    (1..=64).contains(&value.len())
        && value.as_bytes()[0].is_ascii_alphanumeric()
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

/// 业务作用：禁止业务配置空值绕过默认值或选项校验。
/// 参数说明：`value` 是配置树中的当前节点。
/// 返回：整棵树不含 null 时成功，否则返回固定错误。
fn reject_null(value: &Value) -> Result<()> {
    match value {
        Value::Null => bail!("telegram 配置不接受 null；可选字段请省略"),
        Value::Array(items) => items.iter().try_for_each(reject_null),
        Value::Object(items) => items.values().try_for_each(reject_null),
        _ => Ok(()),
    }
}

/// 业务作用：让凭据只从应用 secret 快照取得，避免普通配置持有真实 token。
/// 参数说明：`reference` 是 YAML 凭据引用。
/// 返回：引用非空且采用 secret:// 时成功，不回显原始材料。
fn validate_reference(reference: &str) -> Result<()> {
    ensure!(
        reference.strip_prefix("secret://").is_some_and(valid_id),
        "凭据必须使用 secret://别名"
    );
    Ok(())
}

/// 业务作用：限定 Telegram 出站来源，防止 URL 注入、跳转和非本机明文泄露凭据。
/// 参数说明：`base` 是部署者配置的 Bot API 根地址。
/// 返回：HTTPS 来源或本机 HTTP 来源有效时成功；拒绝用户信息、路径、查询及 fragment。
fn validate_api_base(base: &str) -> Result<()> {
    let url = reqwest::Url::parse(base).map_err(|_| anyhow::anyhow!("api_base URL 无效"))?;
    let loopback = url.host_str().is_some_and(|host| {
        host == "localhost"
            || host
                .trim_matches(['[', ']'])
                .parse::<std::net::IpAddr>()
                .is_ok_and(|ip| ip.is_loopback())
    });
    ensure!(
        url.scheme() == "https" || (url.scheme() == "http" && loopback),
        "api_base 必须使用 HTTPS，只有本机允许 HTTP"
    );
    ensure!(
        url.host_str().is_some()
            && url.username().is_empty()
            && url.password().is_none()
            && url.query().is_none()
            && url.fragment().is_none()
            && url.path() == "/",
        "api_base 只能包含协议、主机与端口"
    );
    Ok(())
}

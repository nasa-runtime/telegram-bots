use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
    time::{Duration, Instant},
};

use anyhow::{ensure, Result};
use http::header;
use nasa::web::{HeaderMap, StatusCode};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;
use zeroize::Zeroizing;

use crate::{
    catalog::config::{BotConfig, TelegramConfig},
    service::ApiError,
};

use crate::partition::{unavailable, Context, DeliveryGuard, Queue, Worker};

/// 只接受文本与预配置目的地选择，不接受凭据或任意 Telegram 参数。
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SendMessage {
    pub text: String,
    #[serde(default)]
    pub destination: Option<String>,
    #[serde(default)]
    pub parse_mode: Option<ParseMode>,
    #[serde(default)]
    pub disable_notification: Option<bool>,
    #[serde(default)]
    pub protect_content: Option<bool>,
}

/// 默认不解析实体；需要富文本时由调用方明确选择 Telegram 格式。
#[derive(Clone, Copy, Deserialize, Serialize)]
pub enum ParseMode {
    HTML,
    MarkdownV2,
}

/// 仅确认内存受理，不表示平台已经发送或用户已经收到。
#[derive(Serialize)]
pub struct AcceptedReceipt {
    pub bot_id: String,
    pub destination: String,
    pub accepted: bool,
}

/// 只发布业务侧选路信息，隐藏凭据与真实聊天身份。
#[derive(Serialize)]
pub struct BotSummary<'a> {
    pub bot_id: &'a str,
    pub description: &'a str,
    pub destinations: Vec<&'a str>,
    pub default_destination: Option<&'a str>,
    pub messages_path: String,
}

/// 认证后的权限集合绑定完整目录代。
pub struct Caller {
    allowed_bots: BTreeSet<String>,
    credential_hash: [u8; 32],
}

pub(crate) struct Bot {
    config: BotConfig,
    token: Zeroizing<String>,
    pub(crate) queue: Arc<Queue>,
}

/// 每个请求持有同一代目录与授权，发送队列按 Telegram 身份跨代复用。
pub struct Catalog {
    pub(crate) bots: BTreeMap<String, Arc<Bot>>,
    callers: BTreeMap<String, Arc<Caller>>,
    pub(crate) transport: Arc<Transport>,
    max_body_bytes: usize,
    body_read_timeout: Duration,
    pub(crate) context: Arc<Context>,
    pub revision: u64,
    pub(crate) fingerprint: [u8; 32],
    pub(crate) http: crate::catalog::config::HttpConfig,
    pub(crate) dispatcher: crate::catalog::config::DispatcherConfig,
}

pub(crate) struct Transport {
    client: reqwest::Client,
    api_base: String,
    timeout: Duration,
    max_response_bytes: usize,
}

impl Catalog {
    /// 业务作用：从一个配置及凭据快照构建全部机器人发送资源。
    /// 参数说明：`config` 是目录；`resolve` 解析同代材料；`existing` 提供已运行队列；`context` 提供全局预算；`revision` 和 `fingerprint` 标识候选；`transport` 复用固定连接池。
    /// 返回：全表有效时创建共享 HTTP 客户端；配置、重复 token 或凭据无效时拒绝发布。
    pub(crate) fn new<F>(
        config: TelegramConfig,
        resolve: F,
        existing: &BTreeMap<String, Arc<Queue>>,
        context: Arc<Context>,
        revision: u64,
        fingerprint: [u8; 32],
        transport: Option<Arc<Transport>>,
    ) -> Result<(Self, Vec<Worker>)>
    where
        F: Fn(&str) -> Result<Zeroizing<String>>,
    {
        config.validate()?;
        let mut callers = BTreeMap::new();
        let mut credentials = BTreeSet::new();
        for (id, caller) in config.clients {
            let credential = resolve(&caller.credential)?;
            let credential_hash = crate::catalog::config::credential_hash(&credential)?;
            ensure!(
                credentials.insert(credential_hash),
                "不同调用方必须使用不同凭据"
            );
            callers.insert(
                id,
                Arc::new(Caller {
                    credential_hash,
                    allowed_bots: caller.allowed_bots,
                }),
            );
        }
        let mut bots = BTreeMap::new();
        let mut workers = Vec::new();
        let mut identities = BTreeSet::new();
        for (id, mut bot) in config.bots {
            if bot.description.trim().is_empty() {
                bot.description.clone_from(&id);
            }
            // 单目的地可唯一推断；多目的地时保留显式选择门禁，避免发送到错误群组。
            if bot.default_destination.is_empty() && bot.destinations.len() == 1 {
                bot.default_destination =
                    bot.destinations.keys().next().cloned().unwrap_or_default();
            }
            let token = resolve(&bot.token)?;
            let identity = crate::catalog::config::telegram_identity(&token)?;
            ensure!(
                identities.insert(identity.clone()),
                "同一 Telegram 身份只能配置一个别名"
            );
            let queue = if let Some(queue) = existing.get(&identity) {
                queue.clone()
            } else {
                let (queue, worker) = Queue::new(identity);
                workers.push(worker);
                queue
            };
            bots.insert(
                id,
                Arc::new(Bot {
                    config: bot,
                    token,
                    queue,
                }),
            );
        }
        let transport = match transport {
            Some(transport) => transport,
            None => {
                // 发送是不可撤销的副作用，关闭重试、跳转与代理，避免重复通知和凭据跨来源转发。
                let client = reqwest::Client::builder()
                    .connect_timeout(Duration::from_millis(config.http.connect_timeout_ms))
                    .timeout(Duration::from_millis(config.http.request_timeout_ms))
                    .redirect(reqwest::redirect::Policy::none())
                    .retry(reqwest::retry::never())
                    .no_proxy()
                    .build()
                    .map_err(|_| anyhow::anyhow!("Telegram HTTP 客户端初始化失败"))?;
                Arc::new(Transport {
                    client,
                    api_base: config.http.api_base.trim_end_matches('/').to_owned(),
                    timeout: Duration::from_millis(config.http.request_timeout_ms),
                    max_response_bytes: config.http.max_response_bytes,
                })
            }
        };
        Ok((
            Self {
                bots,
                callers,
                transport,
                max_body_bytes: config.http.max_body_bytes,
                body_read_timeout: Duration::from_millis(config.http.body_read_timeout_ms),
                context,
                revision,
                fingerprint,
                http: config.http,
                dispatcher: config.dispatcher,
            },
            workers,
        ))
    }

    /// 业务作用：让 HTTP 层使用与本次请求目录一致的消息缓冲上限。
    /// 参数说明：无。
    /// 返回：最大入站 JSON 字节数。
    pub fn max_body_bytes(&self) -> usize {
        self.max_body_bytes
    }

    /// 业务作用：限制已认证调用方上传正文的等待时间，避免慢上传长期占用入站资源。
    /// 参数说明：无。
    /// 返回：从端点开始收集正文到读完的总期限，不包含排队或 Telegram 发送时间。
    pub fn body_read_timeout(&self) -> Duration {
        self.body_read_timeout
    }

    /// 业务作用：在解析消息和查询机器人前认证调用方。
    /// 参数说明：`headers` 必须包含唯一 X-Client-Id 与 Authorization: Bearer 凭据。
    /// 返回：认证成功返回本代权限；身份或凭据无效返回处理码 401。
    pub fn authenticate(&self, headers: &HeaderMap) -> Result<Arc<Caller>, ApiError> {
        let denied = || {
            ApiError::not_sent(
                StatusCode::UNAUTHORIZED,
                "unauthorized",
                "调用方身份或凭据无效",
            )
        };
        // 重复身份头可能在代理层产生不同解释，拒绝歧义凭据以保持授权边界一致。
        if headers.get_all("x-client-id").iter().count() != 1
            || headers.get_all(header::AUTHORIZATION).iter().count() != 1
        {
            return Err(denied());
        }
        let id = headers
            .get("x-client-id")
            .and_then(|h| h.to_str().ok())
            .ok_or_else(denied)?;
        let authorization = headers
            .get(header::AUTHORIZATION)
            .and_then(|h| h.to_str().ok())
            .ok_or_else(denied)?;
        let (scheme, token) = authorization.split_once(' ').ok_or_else(denied)?;
        if !scheme.eq_ignore_ascii_case("Bearer") || !(32..=512).contains(&token.len()) {
            return Err(denied());
        }
        let caller = self.callers.get(id).ok_or_else(denied)?;
        let hash: [u8; 32] = Sha256::digest(token.as_bytes()).into();
        // 只有凭据摘要匹配才能取得权限集合，外部响应不透露身份存在性与拒绝细节。
        if !bool::from(caller.credential_hash.ct_eq(&hash)) {
            return Err(denied());
        }
        Ok(Arc::clone(caller))
    }

    /// 业务作用：列出调用方有权选择的机器人与目的地别名。
    /// 参数说明：`caller` 是认证后的调用方。
    /// 返回：稳定排序的目录，不包含凭据、chat_id 或其它调用方权限。
    pub fn list_bots<'a>(&'a self, caller: &Caller) -> Vec<BotSummary<'a>> {
        self.bots
            .iter()
            .filter(|(id, _)| caller.allowed_bots.contains(*id))
            .map(|(id, bot)| BotSummary {
                bot_id: id,
                description: &bot.config.description,
                destinations: bot.config.destinations.keys().map(String::as_str).collect(),
                default_destination: (!bot.config.default_destination.is_empty())
                    .then_some(bot.config.default_destination.as_str()),
                messages_path: format!("/api/bots/{id}/messages"),
            })
            .collect()
    }

    /// 业务作用：验证通知后非阻塞移交给该 bot 的唯一保序消费域。
    /// 参数说明：`app` 提供生命周期门禁；`caller` 为认证身份；`bot_id` 为显式机器人别名；`request` 为消息。
    /// 返回：成功仅表示内存受理；容量不足、停机或权限无效立即拒绝，不等待 Telegram。
    pub fn enqueue(
        self: &Arc<Self>,
        app: &nasa::Application,
        caller: &Caller,
        bot_id: &str,
        request: SendMessage,
    ) -> Result<AcceptedReceipt, ApiError> {
        // 先判断权限再读取目录，未授权身份不能发送或枚举其它机器人的存在性。
        if !caller.allowed_bots.contains(bot_id) {
            return Err(ApiError::not_sent(
                StatusCode::FORBIDDEN,
                "bot_forbidden",
                "调用方未获该机器人授权",
            ));
        }
        let bot = self.bots.get(bot_id).ok_or_else(|| {
            ApiError::not_sent(StatusCode::NOT_FOUND, "bot_not_found", "机器人不存在")
        })?;
        let destination = request
            .destination
            .as_deref()
            .unwrap_or(&bot.config.default_destination);
        let target = bot.config.destinations.get(destination).ok_or_else(|| {
            ApiError::not_sent(
                StatusCode::BAD_REQUEST,
                "invalid_destination",
                "请选择该机器人的已配置目的地",
            )
        })?;
        // 不拆分超长消息，确保一次受理只产生一次发送尝试，不形成无法解释的部分成功。
        if request.text.trim().is_empty() || request.text.encode_utf16().count() > 4096 {
            return Err(ApiError::not_sent(
                StatusCode::BAD_REQUEST,
                "invalid_text",
                "text 必须非空且不超过 4096 个 UTF-16 单元",
            ));
        }
        // 入队与目录发布共用权威锁，慢上传不能在凭据撤销后继续使用旧权限。
        let authority = self
            .context
            .revision
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        if !app.is_ready() || *authority != Some(self.revision) {
            return Err(unavailable());
        }
        let receipt = AcceptedReceipt {
            bot_id: bot_id.into(),
            destination: destination.into(),
            accepted: true,
        };
        let body = TelegramRequest {
            chat_id: target.chat_id.as_text(),
            text: request.text,
            message_thread_id: target.message_thread_id,
            parse_mode: request.parse_mode,
            disable_notification: request
                .disable_notification
                .unwrap_or(bot.config.delivery.disable_notification),
            protect_content: request
                .protect_content
                .unwrap_or(bot.config.delivery.protect_content),
        };
        let transport = self.transport.clone();
        let context = self.context.clone();
        let owned_bot = bot.clone();
        let id = bot_id.to_owned();
        bot.queue.submit(
            bot.config.delivery.queue_capacity,
            &self.context,
            move |guard| async move {
                Catalog::deliver(transport, context, owned_bot, id, body, guard).await;
            },
        )?;
        Ok(receipt)
    }

    /// 业务作用：在 bot 的严格保序任务中等待发送时隙并执行一次外部转发。
    /// 参数说明：`transport` 是连接池；`context` 是全局预算；`bot` 是受理时资源；`bot_id` 为别名；`body` 为消息；`guard` 拥有终态与容量。
    /// 返回：记录脱敏终态后释放消息；失败不重试，后继消息继续推进。
    async fn deliver(
        transport: Arc<Transport>,
        context: Arc<Context>,
        bot: Arc<Bot>,
        bot_id: String,
        body: TelegramRequest,
        mut guard: DeliveryGuard,
    ) {
        let mut next = *bot
            .queue
            .next_send
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        // 收紧发送间隔时也约束新代第一条通知，不能沿用旧代更短的剩余时隙。
        if let Some(last) = *bot
            .queue
            .last_send
            .lock()
            .unwrap_or_else(|p| p.into_inner())
        {
            next = next.max(last + Duration::from_millis(bot.config.delivery.min_interval_ms));
        }
        tokio::time::sleep_until(next.into()).await;
        // 间隔等待不占用全局 HTTP 许可，冷却中的 bot 不挤占其它 bot 的出站容量。
        let Ok(_permit) = context.inflight.acquire().await else {
            return;
        };
        let started = Instant::now();
        *bot.queue
            .last_send
            .lock()
            .unwrap_or_else(|p| p.into_inner()) = Some(started);
        *bot.queue
            .next_send
            .lock()
            .unwrap_or_else(|p| p.into_inner()) =
            started + Duration::from_millis(bot.config.delivery.min_interval_ms);
        guard.started = true;
        let outcome = tokio::time::timeout(transport.timeout, transport.forward(&bot, &body))
            .await
            .unwrap_or_else(|_| Err(ApiError::unknown(true)));
        match outcome {
            Ok(_) => {
                guard.finish("sent");
            }
            Err(error) => {
                if error.code == "telegram_rate_limited" {
                    // 本条消息不重发；平台冷却约束后续消息，保持同一消费域的顺序。
                    let seconds = error.retry_after_seconds.unwrap_or(1).max(1);
                    if let Some(until) = Instant::now().checked_add(Duration::from_secs(seconds)) {
                        let mut next = bot
                            .queue
                            .next_send
                            .lock()
                            .unwrap_or_else(|p| p.into_inner());
                        *next = (*next).max(until);
                    }
                }
                if error.delivery == "unknown" {
                    guard.finish("unknown");
                } else {
                    guard.finish("failed");
                }
                tracing::warn!(
                    bot_id,
                    outcome = error.code,
                    delivery = error.delivery,
                    "Telegram 异步转发未确认成功"
                );
            }
        }
        guard.finished = true;
    }
}

impl Transport {
    /// 业务作用：发送单个 Bot API 请求并验证完整平台回执。
    /// 参数说明：`bot` 提供私有凭据；`body` 是已经通过校验的消息。
    /// 返回：仅 HTTP 成功、ok=true 且 message_id 有效时成功；其它状态保守分类。
    async fn forward(&self, bot: &Bot, body: &TelegramRequest) -> Result<i64, ApiError> {
        let url = Zeroizing::new(format!(
            "{}/bot{}/sendMessage",
            self.api_base,
            bot.token.as_str()
        ));
        let mut response = self
            .client
            .post(url.as_str())
            .json(body)
            .send()
            .await
            .map_err(|error| {
                // reqwest 错误可能包含带 token 的 URL，只检查类别，不格式化或挂接原始错误链。
                if error.is_connect() {
                    ApiError::not_sent(
                        StatusCode::SERVICE_UNAVAILABLE,
                        "telegram_unavailable",
                        "无法建立 Telegram 连接",
                    )
                } else {
                    ApiError::unknown(error.is_timeout())
                }
            })?;
        let status = response.status();
        let mut bytes = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|error| ApiError::unknown(error.is_timeout()))?
        {
            if chunk.len() > self.max_response_bytes.saturating_sub(bytes.len()) {
                return Err(ApiError::unknown(false));
            }
            bytes.extend_from_slice(&chunk);
        }
        let envelope: TelegramResponse =
            serde_json::from_slice(&bytes).map_err(|_| ApiError::unknown(false))?;
        if status.is_success() && envelope.ok {
            return envelope
                .result
                .filter(|result| result.message_id > 0)
                .map(|result| result.message_id)
                .ok_or_else(|| ApiError::unknown(false));
        }
        // 只有明确的 ok=false 回执可以宣告拒绝，矛盾或不完整的状态保留未知语义。
        if !envelope.ok {
            if let Some(code) = envelope.error_code {
                if status.is_success() || status.as_u16() == code {
                    return Err(ApiError::upstream(
                        code,
                        envelope.parameters.and_then(|p| p.retry_after),
                    ));
                }
            }
        }
        Err(ApiError::unknown(false))
    }
}

#[derive(Serialize)]
struct TelegramRequest {
    chat_id: String,
    text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    message_thread_id: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    parse_mode: Option<ParseMode>,
    disable_notification: bool,
    protect_content: bool,
}

#[derive(Deserialize)]
struct TelegramResponse {
    ok: bool,
    result: Option<TelegramMessage>,
    error_code: Option<u16>,
    parameters: Option<ResponseParameters>,
}
#[derive(Deserialize)]
struct TelegramMessage {
    message_id: i64,
}
#[derive(Deserialize)]
struct ResponseParameters {
    retry_after: Option<u64>,
}

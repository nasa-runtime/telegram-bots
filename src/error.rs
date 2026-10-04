use http::header;
use nasa::web::{IntoResponse, Json, Response, StatusCode};
use serde::Serialize;

/// 有限错误合同；绝不携带上游 URL、token、消息正文或响应原文。
#[derive(Debug, Serialize)]
pub struct ApiError {
    pub code: &'static str,
    pub message: &'static str,
    pub delivery: &'static str,
    pub retry_safe: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub retry_after_seconds: Option<u64>,
    #[serde(skip)]
    pub status: StatusCode,
}

impl ApiError {
    /// 业务作用：描述尚未触发 Telegram 发送的拒绝结果。
    /// 参数说明：`status`、`code`、`message` 是稳定 HTTP 状态、原因与说明。
    /// 返回：delivery=not_sent；容量不足或服务暂不可用时允许稍后重试。
    pub fn not_sent(status: StatusCode, code: &'static str, message: &'static str) -> Self {
        Self {
            code,
            message,
            status,
            delivery: "not_sent",
            retry_safe: status == StatusCode::TOO_MANY_REQUESTS
                || status == StatusCode::SERVICE_UNAVAILABLE,
            retry_after_seconds: None,
        }
    }

    /// 业务作用：保守表达请求发出后无法确认结果的状态，防止调用方自动重复通知。
    /// 参数说明：`timeout` 表示是否因总期限耗尽退出。
    /// 返回：HTTP 504 或 502，delivery=unknown 且 retry_safe=false。
    pub fn unknown(timeout: bool) -> Self {
        Self {
            code: if timeout {
                "telegram_timeout"
            } else {
                "telegram_result_unknown"
            },
            message: "无法确认 Telegram 是否已经发送；请勿自动重发",
            status: if timeout {
                StatusCode::GATEWAY_TIMEOUT
            } else {
                StatusCode::BAD_GATEWAY
            },
            delivery: "unknown",
            retry_safe: false,
            retry_after_seconds: None,
        }
    }

    /// 业务作用：把 Telegram 的明确拒绝投影成无敏感文本的稳定结果。
    /// 参数说明：`code` 是 Telegram 错误码；`retry_after` 是平台给出的限流等待秒数。
    /// 返回：限流允许等待后重试，其它拒绝需要调整权限或请求；未知服务器状态保持结果未知。
    pub fn upstream(code: u16, retry_after: Option<u64>) -> Self {
        let (status, reason, message) = match code {
            429 => (
                StatusCode::TOO_MANY_REQUESTS,
                "telegram_rate_limited",
                "Telegram 限流，请等待后重试",
            ),
            401 => (
                StatusCode::BAD_GATEWAY,
                "telegram_authentication",
                "Telegram 机器人凭据无效",
            ),
            403 => (
                StatusCode::BAD_GATEWAY,
                "telegram_forbidden",
                "Telegram 拒绝向该目的地发送",
            ),
            400 | 404 => (
                StatusCode::BAD_GATEWAY,
                "telegram_rejected",
                "Telegram 拒绝请求，请检查目的地及消息格式",
            ),
            _ => return Self::unknown(false),
        };
        Self {
            code: reason,
            message,
            status,
            delivery: "rejected",
            retry_safe: code == 429,
            retry_after_seconds: if code == 429 { retry_after } else { None },
        }
    }
}

impl IntoResponse for ApiError {
    /// 业务作用：统一返回机器可读错误和可选重试等待信息。
    /// 参数说明：无。
    /// 返回：固定 JSON 错误信封；鉴权失败携带 Bearer challenge。
    fn into_response(self) -> Response {
        let retry_after = self.retry_after_seconds;
        let status = self.status;
        let mut response = (status, Json(serde_json::json!({"error": self}))).into_response();
        if let Some(seconds) = retry_after {
            if let Ok(value) = seconds.to_string().parse() {
                response.headers_mut().insert(header::RETRY_AFTER, value);
            }
        }
        if status == StatusCode::UNAUTHORIZED {
            response.headers_mut().insert(
                header::WWW_AUTHENTICATE,
                header::HeaderValue::from_static("Bearer"),
            );
        }
        response
    }
}

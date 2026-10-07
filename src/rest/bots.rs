use std::sync::Arc;

use http_body_util::{BodyExt, LengthLimitError, Limited};
use nasa::base::BaseResponse;
use nasa::web::{get_mapping, post_mapping, Extension, Json, Path, Request, State, StatusCode};
use telegram_bots::service::{AcceptedReceipt, ApiError, Caller, Catalog, SendMessage};

use super::auth::authenticate;
use super::response::success;

/// 业务作用：帮助业务方选择其权限范围内的机器人和目的地。
/// 参数说明：`service` 是认证阶段固定的发送目录；`caller` 是认证身份。
/// 返回：机器人用途、目的地别名和发送路径，不暴露 Telegram 凭据或 chat_id。
#[get_mapping(path = "/api/bots", auth = "required", interceptors(authenticate))]
async fn list_bots(
    Extension(service): Extension<Arc<Catalog>>,
    Extension(caller): Extension<Arc<Caller>>,
) -> Json<BaseResponse<serde_json::Value>> {
    success(serde_json::json!({ "bots": service.list_bots(&caller) }))
}

/// 业务作用：接收有界消息并立即移交 bot 的保序内存队列，释放 HTTP 请求资源。
/// 参数说明：`app` 提供应用状态；`service` 为发送资源；`caller` 为认证身份；`bot_id` 为机器人别名；`request` 提供 JSON 正文。
/// 返回：所有处理结果均为 HTTP 200；code=200 表示内存受理，其它 code 表示拒绝，不等待后台发送。
#[post_mapping(
    path = "/api/bots/{bot_id}/messages",
    consumes = "application/json",
    auth = "required",
    interceptors(authenticate)
)]
async fn send_message(
    State(app): State<nasa::Application>,
    Extension(service): Extension<Arc<Catalog>>,
    Extension(caller): Extension<Arc<Caller>>,
    Path(bot_id): Path<String>,
    request: Request,
) -> Result<Json<BaseResponse<AcceptedReceipt>>, ApiError> {
    // 身份门禁已通过，再限制缓冲和读取总期限，避免大消息或慢上传长期占用请求资源。
    let bytes = tokio::time::timeout(
        service.body_read_timeout(),
        Limited::new(request.into_body(), service.max_body_bytes()).collect(),
    )
    .await
    .map_err(|_| {
        // 正文尚未完成校验，超时只取消读取，不产生入队或 Telegram 副作用。
        ApiError::not_sent(
            StatusCode::REQUEST_TIMEOUT,
            "body_read_timeout",
            "消息正文上传超时，未进入发送队列",
        )
    })?
    .map_err(|error| {
        let status = if error.is::<LengthLimitError>() {
            StatusCode::PAYLOAD_TOO_LARGE
        } else {
            StatusCode::BAD_REQUEST
        };
        ApiError::not_sent(status, "invalid_request", "消息正文读取失败或超出大小限制")
    })?
    .to_bytes();
    let message: SendMessage = serde_json::from_slice(&bytes).map_err(|_| {
        ApiError::not_sent(
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_request",
            "JSON 字段或类型无效",
        )
    })?;
    service
        .enqueue(&app, &caller, &bot_id, message)
        .map(success)
}

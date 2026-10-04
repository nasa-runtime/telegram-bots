use std::sync::Arc;

use http_body_util::{BodyExt, LengthLimitError, Limited};
use nasa::web::{
    auth::AuthContext, get_mapping, interceptor, post_mapping, Extension, IntoResponse, Json, Next,
    Path, Request, Response, State, StatusCode,
};
use telegram_bots::{
    error::ApiError,
    runtime::TelegramService,
    service::{AcceptedReceipt, Caller, Catalog, SendMessage},
};

/// 业务作用：在 naweb 身份阶段校验调用方并固定本次请求的权限与发送资源。
/// 参数说明：`app` 提供受管资源；`request` 包含候选凭据；`next` 是 required 身份门禁及后续端点。
/// 返回：认证有效时写入 AuthContext；无效或资源已关闭时立即拒绝，不读取消息正文。
#[interceptor(id = "telegram-client", kind = "auth", order = 100)]
async fn authenticate(
    State(app): State<nasa::Application>,
    mut request: Request,
    next: Next,
) -> Response {
    let service = match app.resource::<Arc<TelegramService>>().await {
        Ok(resource) => Arc::clone(&resource),
        Err(_) => {
            return ApiError::not_sent(
                StatusCode::SERVICE_UNAVAILABLE,
                "service_unavailable",
                "发送资源尚未就绪或已关闭",
            )
            .into_response()
        }
    };
    let runtime = service;
    let service = runtime.snapshot();
    let caller = match service.authenticate(request.headers()) {
        Ok(caller) => caller,
        Err(error) => return error.into_response(),
    };
    let subject: Arc<str> = Arc::from(
        request
            .headers()
            .get("x-client-id")
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default(),
    );
    // subject 已由凭据校验确认；required gate 只消费这里建立的可信身份，不接受正文自报身份。
    request.extensions_mut().insert(AuthContext {
        subject,
        tenant: Arc::from("telegram-bots"),
        authentication_kind: "service-key",
        principal: caller.clone(),
    });
    request.extensions_mut().insert(caller);
    request.extensions_mut().insert(service);
    request.extensions_mut().insert(runtime);
    next.run(request).await
}

/// 业务作用：帮助业务方选择其权限范围内的机器人和目的地。
/// 参数说明：`service` 是认证阶段固定的发送目录；`caller` 是认证身份。
/// 返回：机器人用途、目的地别名和发送路径，不暴露 Telegram 凭据或 chat_id。
#[get_mapping(path = "/api/bots", auth = "required", interceptors(authenticate))]
async fn list_bots(
    Extension(service): Extension<Arc<Catalog>>,
    Extension(caller): Extension<Arc<Caller>>,
) -> Json<serde_json::Value> {
    Json(serde_json::json!({ "bots": service.list_bots(&caller) }))
}

/// 业务作用：接收有界消息并立即移交 bot 的保序内存队列，释放 HTTP 请求资源。
/// 参数说明：`app` 提供应用状态；`service` 为发送资源；`caller` 为认证身份；`bot_id` 为机器人别名；`request` 提供 JSON 正文。
/// 返回：内存受理返回 202；校验、读取超时或容量拒绝返回错误，不等待后台发送。
#[post_mapping(
    path = "/api/bots/{bot_id}/messages",
    consumes = "application/json",
    auth = "required",
    interceptors(authenticate),
    success_status = 202
)]
async fn send_message(
    State(app): State<nasa::Application>,
    Extension(service): Extension<Arc<Catalog>>,
    Extension(caller): Extension<Arc<Caller>>,
    Path(bot_id): Path<String>,
    request: Request,
) -> Result<(StatusCode, Json<AcceptedReceipt>), ApiError> {
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
        .map(|receipt| (StatusCode::ACCEPTED, Json(receipt)))
}

/// 业务作用：让授权调用方查看目录部署、拒绝原因与机器人排空状态。
/// 参数说明：`service` 是受管目录资源。
/// 返回：脱敏控制面快照，不含 token、chat_id、文件路径或其它调用方权限。
#[get_mapping(
    path = "/api/config/status",
    auth = "required",
    interceptors(authenticate)
)]
async fn config_status(
    Extension(service): Extension<Arc<TelegramService>>,
) -> Json<telegram_bots::runtime::CatalogStatus> {
    Json(service.observation())
}

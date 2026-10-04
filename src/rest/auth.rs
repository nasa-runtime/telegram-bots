use std::sync::Arc;

use nasa::web::{
    auth::AuthContext, interceptor, IntoResponse, Next, Request, Response, State, StatusCode,
};
use telegram_bots::{application::TelegramHandle, service::ApiError};

/// 业务作用：在 naweb 身份阶段校验调用方并固定本次请求的权限与发送资源。
/// 参数说明：`app` 提供受管资源；`request` 包含候选凭据；`next` 是 required 身份门禁及后续端点。
/// 返回：认证有效时写入 AuthContext；无效或资源已关闭时立即拒绝，不读取消息正文。
#[interceptor(id = "telegram-client", kind = "auth", order = 100)]
pub(super) async fn authenticate(
    State(app): State<nasa::Application>,
    mut request: Request,
    next: Next,
) -> Response {
    let service = match app.resource::<TelegramHandle>().await {
        Ok(resource) => resource.service(),
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

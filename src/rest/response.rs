use std::{panic::AssertUnwindSafe, sync::Arc};

use anyhow::{ensure, Result};
use futures_util::FutureExt;
use nasa::{
    base::BaseResponse,
    web::{from_fn_with_state, IntoResponse, Json, Next, Request, Response, State, StatusCode},
};
use telegram_bots::service::ApiError;
use tokio::sync::Semaphore;

/// 业务作用：为业务接口统一成功外壳，省略未设置的提示字段。
/// 参数说明：`data` 是当前接口的业务结果，消息受理结果不代表 Telegram 已送达。
/// 返回：HTTP 200，code=200，省略 msg，data 保留业务结果。
pub(super) fn success<T>(data: T) -> Json<BaseResponse<T>> {
    Json(BaseResponse::ok(data))
}

/// 业务作用：在业务路由边界统一响应与并发受理预算，使过载也使用 JSON 处理结果。
/// 参数说明：`app` 是尚未封口的应用容器。
/// 返回：来源和预算合法时登记响应层；框架探针由 napp 单独管理，冲突的外层过载设置拒绝启动。
pub(crate) fn install(app: &nasa::Application) -> Result<()> {
    let config = app.config();
    // 框架的外层过载拒绝不会进入业务响应层，禁止两套入口预算生成不同的响应合同。
    ensure!(
        config
            .value()
            .pointer("/server/max_inflight_requests")
            .is_none_or(serde_json::Value::is_null),
        "请使用 api.max_inflight_requests 配置业务入口并发预算"
    );
    // 会在业务层之外直接生成响应的治理入口不能绕过统一处理码；这些策略由受信网关承担。
    ensure!(
        config
            .value()
            .pointer("/server/request_deadline_ms")
            .is_none_or(serde_json::Value::is_null),
        "本服务不支持 server.request_deadline_ms 外层响应期限"
    );
    for path in ["/server/rate_limit/enabled", "/server/cors/enabled"] {
        let enabled = config
            .value()
            .pointer(path)
            .cloned()
            .unwrap_or(serde_json::Value::Bool(false));
        ensure!(
            !nasa::yml::strict::bind::<bool>(enabled)?,
            "本服务不支持框架外层限流或 CORS 短路响应"
        );
    }
    let max_inflight = match config.value().pointer("/api/max_inflight_requests") {
        Some(value) => nasa::yml::strict::bind::<usize>(value.clone())?,
        None => 256,
    };
    ensure!(
        (1..=65536).contains(&max_inflight),
        "api.max_inflight_requests 必须为 1..=65536"
    );
    let limit = Arc::new(Semaphore::new(max_inflight));
    app.configure_router(move |router| router.layer(from_fn_with_state(limit, normalize_error)))?;
    Ok(())
}

/// 业务作用：将尚未采用业务错误外壳的路由拒绝转换为稳定、无敏感原文的结果。
/// 参数说明：`limit` 限制在途业务请求；`request` 是当前请求；`next` 执行路由与认证流水线。
/// 返回：HTTP 200 和统一 JSON；错误语义放入 code，保留认证与重试协议头。
async fn normalize_error(
    State(limit): State<Arc<Semaphore>>,
    request: Request,
    next: Next,
) -> Response {
    let Ok(_permit) = limit.try_acquire_owned() else {
        // 过载在读取正文和入队前拒绝，调用方可按 JSON code 退避，探针不占用这份预算。
        let mut error = ApiError::not_sent(
            StatusCode::SERVICE_UNAVAILABLE,
            "service_overloaded",
            "业务入口并发预算已满",
        );
        error.retry_after_seconds = Some(1);
        return error.into_response();
    };
    let mut response = match AssertUnwindSafe(next.run(request)).catch_unwind().await {
        Ok(response) => response,
        Err(payload) => {
            // 异常可能发生在副作用之后，禁止自动重发；丢弃不受信 panic 内容而不执行其析构。
            std::mem::forget(payload);
            let mut error = ApiError::not_sent(
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal_error",
                "请求处理未能完成",
            );
            error.delivery = "unknown";
            error.retry_safe = false;
            return error.into_response();
        }
    };
    // 成功目录和拒绝结果都可能依赖调用方权限，不允许中间缓存跨身份复用。
    response.headers_mut().insert(
        http::header::CACHE_CONTROL,
        http::HeaderValue::from_static("no-store"),
    );
    let status = response.status();
    if !status.is_client_error() && !status.is_server_error()
        || response.extensions().get::<ApiError>().is_some()
    {
        return response;
    }
    let (reason, message) = match status {
        StatusCode::NOT_FOUND => ("not_found", "请求路径不存在"),
        StatusCode::METHOD_NOT_ALLOWED => ("method_not_allowed", "请求方法不支持"),
        StatusCode::UNSUPPORTED_MEDIA_TYPE => {
            ("unsupported_media_type", "请求必须使用 application/json")
        }
        StatusCode::PAYLOAD_TOO_LARGE => ("payload_too_large", "请求正文超出大小限制"),
        StatusCode::BAD_REQUEST => ("invalid_request", "请求路径或参数无效"),
        StatusCode::UNAUTHORIZED => ("unauthorized", "调用方身份认证失败"),
        StatusCode::FORBIDDEN => ("forbidden", "调用方无权执行此请求"),
        _ => ("request_failed", "请求未能完成"),
    };
    let mut error = ApiError::not_sent(status, reason, message);
    if status.is_server_error() {
        // 未掌握业务副作用证据时，不能把框架异常解释为可以安全重发。
        error.delivery = "unknown";
        error.retry_safe = false;
    }
    let mut normalized = error.into_response();
    for (name, value) in response.headers() {
        if !matches!(
            name.as_str(),
            "content-type" | "content-length" | "content-encoding" | "transfer-encoding"
        ) {
            normalized.headers_mut().append(name, value.clone());
        }
    }
    normalized
}

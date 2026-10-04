use std::sync::Arc;

use nasa::web::{get_mapping, Extension, Json};
use telegram_bots::catalog::{CatalogStatus, TelegramService};

use super::auth::authenticate;

/// 业务作用：让授权调用方查看目录部署、拒绝原因与机器人排空状态。
/// 参数说明：`service` 是受管目录资源。
/// 返回：脱敏控制面快照，不含 token、chat_id、文件路径或其它调用方权限。
#[get_mapping(
    path = "/api/config/status",
    auth = "required",
    interceptors(authenticate)
)]
async fn config_status(Extension(service): Extension<Arc<TelegramService>>) -> Json<CatalogStatus> {
    Json(service.observation())
}

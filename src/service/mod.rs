//! 通知认证、目的地选择、消息受理与 Telegram 发送结果分类。

mod error;
mod notification;

pub use error::ApiError;
pub(crate) use notification::Transport;
pub use notification::{AcceptedReceipt, BotSummary, Caller, Catalog, ParseMode, SendMessage};

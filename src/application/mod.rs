//! 应用生命周期装配、初始化屏障与受管资源清理。

mod lifecycle;

pub use lifecycle::{install, TelegramHandle};

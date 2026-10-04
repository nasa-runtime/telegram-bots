//! 配置来源、材料校验与完整目录的原子发布。

pub mod config;
mod manager;
pub(crate) mod reader;
pub mod source;

pub use manager::{CatalogManager, CatalogStatus, TelegramService};

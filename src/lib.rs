//! 多机器人文本通知网关，集中维护凭据、目的地和调用权限。
//!
//! 外部 YAML 与摘要绑定的凭据文件组成完整候选，连续读取一致后原子发布目录。
//! 请求固定目录代，入队时复验权限仍然有效；同一 Telegram 身份跨代复用唯一消费域。
//! 只提供有界内存受理与一次发送尝试，不提供持久化、自动重发或跨实例去重。

pub mod config;
mod dispatch;
pub mod error;
pub mod isolated;
mod metrics;
pub mod runtime;
pub mod service;
pub mod source;

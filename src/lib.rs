//! 多机器人文本通知网关，集中维护凭据、目的地和调用权限。
//!
//! 主入口使用 `#[nasa::application]`，通过 app 装配业务，napp 管理运行时、日志、初始化屏障与停机。
//! 日志由 nalog 输出到控制台，配置目录后同时写滚动文件，文件日志在业务资源之后关闭并刷盘。
//! 目录作为 hosted initializer 登记受管资源和关键任务，消费者只在 Ready 后运行。
//! 文件字节由 fork 创建的只读子进程隔离读取，父进程在内存中合并与校验，启动取消由清理栈回收进程。
//!
//! 外部 YAML 与摘要绑定的凭据文件组成完整候选，连续读取一致后原子发布目录。
//! 请求固定目录代，入队时复验权限仍然有效；同一 Telegram 身份跨代复用唯一消费域。
//! 只提供有界内存受理与一次发送尝试，不提供持久化、自动重发或跨实例去重。

pub mod application;
pub mod catalog;
mod observability;
mod partition;
pub mod service;

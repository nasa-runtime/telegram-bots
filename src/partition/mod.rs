//! 按 Telegram 数字身份划分的保序消费域、容量预算与终态计数。

mod queue;

pub(crate) use queue::{unavailable, Context, Counters, DeliveryGuard, Queue, Worker};

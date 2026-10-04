//! 目录与通知消费域的指标采集，不持有业务资源的生命周期所有权。

mod metrics;

pub(crate) use metrics::CatalogMetrics;

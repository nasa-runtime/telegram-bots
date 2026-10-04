use crate::runtime::TelegramService;
use nasa::application::{
    LegacyMetricsSource, MetricDescriptor, MetricKind, MetricSample, MetricValue,
};
use std::{fmt::Write, sync::atomic::Ordering};

macro_rules! descriptor {
    ($id:ident, $name:literal, $help:literal, $kind:ident, $labels:expr) => {
        static $id: MetricDescriptor = MetricDescriptor {
            name: $name,
            help: $help,
            unit: "",
            kind: MetricKind::$kind,
            label_names: $labels,
            histogram_bounds: &[],
        };
    };
}
descriptor!(
    ACCEPTED,
    "telegram_bot_accepted_total",
    "当前机器人消费域累计受理数。",
    Counter,
    &["bot"]
);
descriptor!(
    DELIVERIES,
    "telegram_bot_deliveries_total",
    "当前机器人消费域终态，不代表用户已收到。",
    Counter,
    &["bot", "outcome"]
);
descriptor!(
    TOTAL,
    "telegram_messages_total",
    "跨目录保留的进程累计受理与终态。",
    Counter,
    &["outcome"]
);
descriptor!(
    DESIRED,
    "telegram_catalog_desired_generation",
    "最后读取到的部署代号，无法解析时为零。",
    Gauge,
    &[]
);
descriptor!(
    APPLIED,
    "telegram_catalog_applied_generation",
    "当前已应用部署代号。",
    Gauge,
    &[]
);
descriptor!(
    SUCCESS,
    "telegram_catalog_last_success_seconds",
    "最近一次发布的 Unix 秒数。",
    Gauge,
    &[]
);
descriptor!(
    CHECK,
    "telegram_catalog_last_check_seconds",
    "最近配置对账或超时的 Unix 秒数。",
    Gauge,
    &[]
);
descriptor!(
    REJECTED,
    "telegram_catalog_rejected_total",
    "候选拒绝次数，包括重复读取仍无效的候选。",
    Counter,
    &[]
);
descriptor!(
    ERROR,
    "telegram_catalog_rejected",
    "最近候选是否处于拒绝状态。",
    Gauge,
    &[]
);
descriptor!(
    DRAINING,
    "telegram_catalog_draining_bots",
    "已移除但尚未退出的消费域数量。",
    Gauge,
    &[]
);
static DESCRIPTORS: [&MetricDescriptor; 10] = [
    &ACCEPTED,
    &DELIVERIES,
    &TOTAL,
    &DESIRED,
    &APPLIED,
    &SUCCESS,
    &CHECK,
    &REJECTED,
    &ERROR,
    &DRAINING,
];

impl LegacyMetricsSource for TelegramService {
    /// 业务作用：把发送与配置状态纳入应用指标目录。
    /// 参数说明：无。
    /// 返回：指标标签不包含部署代号、凭据或任意错误文本。
    fn descriptors(&self) -> &'static [&'static MetricDescriptor] {
        &DESCRIPTORS
    }

    /// 业务作用：读取当前消费域与跨目录累计计数，保留移除机器人的损耗总量。
    /// 参数说明：无。
    /// 返回：发送终态及配置观测样本。
    fn snapshot(&self) -> Option<Vec<MetricSample>> {
        let catalog = TelegramService::snapshot(self);
        let mut samples = Vec::new();
        for (id, bot) in &catalog.bots {
            let counters = &bot.queue.counters;
            samples.push(MetricSample {
                name: ACCEPTED.name,
                labels: vec![("bot", id.clone())],
                value: MetricValue::Counter(counters.accepted.load(Ordering::Relaxed)),
            });
            for (outcome, counter) in [
                ("sent", &counters.sent),
                ("failed", &counters.failed),
                ("unknown", &counters.unknown),
                ("dropped", &counters.dropped),
            ] {
                samples.push(MetricSample {
                    name: DELIVERIES.name,
                    labels: vec![("bot", id.clone()), ("outcome", outcome.into())],
                    value: MetricValue::Counter(counter.load(Ordering::Relaxed)),
                });
            }
        }
        let counters = &self.context.totals;
        for (outcome, counter) in [
            ("accepted", &counters.accepted),
            ("sent", &counters.sent),
            ("failed", &counters.failed),
            ("unknown", &counters.unknown),
            ("dropped", &counters.dropped),
        ] {
            samples.push(MetricSample {
                name: TOTAL.name,
                labels: vec![("outcome", outcome.into())],
                value: MetricValue::Counter(counter.load(Ordering::Relaxed)),
            });
        }
        let state = self.observation();
        for (descriptor, value) in [
            (&DESIRED, state.desired_generation.unwrap_or(0)),
            (&APPLIED, state.applied_generation),
            (&SUCCESS, state.last_success_unix_seconds),
            (&CHECK, state.last_check_unix_seconds),
            (&ERROR, u64::from(state.last_rejection.is_some())),
            (&DRAINING, state.draining_bots.len() as u64),
        ] {
            samples.push(MetricSample {
                name: descriptor.name,
                labels: vec![],
                value: MetricValue::Gauge(value as f64),
            });
        }
        samples.push(MetricSample {
            name: REJECTED.name,
            labels: vec![],
            value: MetricValue::Counter(state.rejected_attempts),
        });
        Some(samples)
    }

    /// 业务作用：为文本监控出口追加与结构化采集一致的计数。
    /// 参数说明：`output` 是目标缓冲。
    /// 返回：追加 Prometheus 样本，不清空其它组件指标。
    fn render_prometheus(&self, output: &mut String) {
        if let Some(samples) = LegacyMetricsSource::snapshot(self) {
            for sample in samples {
                let value = match sample.value {
                    MetricValue::Counter(value) => value.to_string(),
                    MetricValue::Gauge(value) => value.to_string(),
                    _ => continue,
                };
                let labels = sample
                    .labels
                    .iter()
                    .map(|(name, value)| format!("{name}=\"{value}\""))
                    .collect::<Vec<_>>()
                    .join(",");
                if labels.is_empty() {
                    let _ = writeln!(output, "{} {value}", sample.name);
                } else {
                    let _ = writeln!(output, "{}{{{labels}}} {value}", sample.name);
                }
            }
        }
    }
}

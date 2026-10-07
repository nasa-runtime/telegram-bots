use std::{
    future::Future,
    pin::Pin,
    sync::{
        atomic::{AtomicU64, AtomicUsize, Ordering},
        Arc, Mutex,
    },
    time::Instant,
};
use tokio::sync::{mpsc, OwnedSemaphorePermit, Semaphore};

use crate::service::ApiError;
use nasa::web::StatusCode;

type Work = Pin<Box<dyn Future<Output = ()> + Send>>;

/// 进程级控制权与容量跨配置代保留，轮换不能重置限额。
pub(crate) struct Context {
    pub revision: Mutex<Option<u64>>,
    pub inflight: Semaphore,
    pub budget: Arc<Semaphore>,
    pub totals: Arc<Counters>,
}

/// 消息生命周期计数不持有正文、目的地或凭据。
#[derive(Default)]
pub(crate) struct Counters {
    pub accepted: AtomicU64,
    pub sent: AtomicU64,
    pub failed: AtomicU64,
    pub unknown: AtomicU64,
    pub dropped: AtomicU64,
}

/// 同一 Telegram 数字身份共享唯一消费队列，token 轮换不产生并行发送域。
pub(crate) struct Queue {
    pub identity: String,
    sender: Mutex<Option<mpsc::Sender<Work>>>,
    pending: AtomicUsize,
    pub next_send: Mutex<Instant>,
    pub last_send: Mutex<Option<Instant>>,
    pub counters: Arc<Counters>,
}

pub(crate) struct Worker {
    pub queue: Arc<Queue>,
    receiver: mpsc::Receiver<Work>,
}

impl Queue {
    /// 业务作用：准备有界单消费域，发布之前尚不执行任何发送。
    /// 参数说明：`identity` 为 token 中的 Telegram 数字身份。
    /// 返回：队列和唯一接收者；接收者必须移交受管任务集合。
    pub fn new(identity: String) -> (Arc<Self>, Worker) {
        let (sender, receiver) = mpsc::channel(4097);
        let queue = Arc::new(Self {
            identity,
            sender: Mutex::new(Some(sender)),
            pending: AtomicUsize::new(0),
            next_send: Mutex::new(Instant::now()),
            last_send: Mutex::new(None),
            counters: Arc::new(Counters::default()),
        });
        (queue.clone(), Worker { queue, receiver })
    }

    /// 业务作用：在不等待容量的前提下受理一条通知，统一保留队列与进程预算。
    /// 参数说明：`capacity` 为当前目录允许的排队容量；`context` 为跨代预算；`make` 构造一次发送。
    /// 返回：成功后计为受理；关闭或容量不足时不构造发送任务。
    pub fn submit<F, Fut>(
        self: &Arc<Self>,
        capacity: usize,
        context: &Context,
        make: F,
    ) -> Result<(), ApiError>
    where
        F: FnOnce(DeliveryGuard) -> Fut,
        Fut: Future<Output = ()> + Send + 'static,
    {
        let sender = self.sender.lock().unwrap_or_else(|p| p.into_inner());
        let sender = sender.as_ref().ok_or_else(unavailable)?;
        if self.pending.load(Ordering::Relaxed) > capacity {
            return Err(full());
        }
        let budget = context
            .budget
            .clone()
            .try_acquire_owned()
            .map_err(|_| full())?;
        let permit = sender.try_reserve().map_err(|_| full())?;
        self.pending.fetch_add(1, Ordering::Relaxed);
        self.counters.accepted.fetch_add(1, Ordering::Relaxed);
        context.totals.accepted.fetch_add(1, Ordering::Relaxed);
        permit.send(Box::pin(make(DeliveryGuard {
            queue: self.clone(),
            totals: context.totals.clone(),
            _budget: budget,
            started: false,
            finished: false,
        })));
        Ok(())
    }

    /// 业务作用：撤销队列受理权，保留已受理消息供唯一接收者排空。
    /// 参数说明：无。
    /// 返回：所有保留的目录句柄立即失去提交能力；重复关闭无额外效果。
    pub fn close(&self) {
        self.sender.lock().unwrap_or_else(|p| p.into_inner()).take();
    }
}

impl Worker {
    /// 业务作用：串行执行同一 Telegram 身份的通知，直至入口关闭且已受理工作耗尽。
    /// 参数说明：无。
    /// 返回：正常排空时返回身份；外层取消会释放所有未完成消息及其预算。
    pub async fn run(mut self) -> String {
        while let Some(work) = self.receiver.recv().await {
            work.await;
        }
        self.queue.identity.clone()
    }
}

/// 受理后即存在的终态守卫区分未发送丢弃与开始发送后结果未知。
pub(crate) struct DeliveryGuard {
    queue: Arc<Queue>,
    totals: Arc<Counters>,
    _budget: OwnedSemaphorePermit,
    pub started: bool,
    pub finished: bool,
}

impl DeliveryGuard {
    /// 业务作用：同时累计机器人与进程发送终态，移除目录后仍保留累计损耗。
    /// 参数说明：`outcome` 是 sent、failed 或 unknown。
    /// 返回：更新封闭指标集合并标记终态，取消时不再重复累计。
    pub fn finish(&mut self, outcome: &str) {
        for counters in [&self.queue.counters, &self.totals] {
            let counter = match outcome {
                "sent" => &counters.sent,
                "failed" => &counters.failed,
                _ => &counters.unknown,
            };
            counter.fetch_add(1, Ordering::Relaxed);
        }
        self.finished = true;
    }
}

impl Drop for DeliveryGuard {
    /// 业务作用：取消时清晰记录内存丢弃或出站结果未知，并归还全部容量。
    /// 参数说明：无。
    /// 返回：同步计数，不执行 I/O 或保留消息材料。
    fn drop(&mut self) {
        if !self.finished {
            for counters in [&self.queue.counters, &self.totals] {
                if self.started {
                    counters.unknown.fetch_add(1, Ordering::Relaxed);
                } else {
                    counters.dropped.fetch_add(1, Ordering::Relaxed);
                }
            }
        }
        self.queue.pending.fetch_sub(1, Ordering::Relaxed);
    }
}

/// 业务作用：在有界队列预算耗尽时拒绝新通知。
/// 参数说明：无。
/// 返回：处理码为 429 且尚未发送，业务 HTTP 响应仍为 200。
fn full() -> ApiError {
    ApiError::not_sent(
        StatusCode::TOO_MANY_REQUESTS,
        "queue_full",
        "通知队列或进程预算已满",
    )
}

/// 业务作用：在目录或队列已失去服务权时拒绝新通知。
/// 参数说明：无。
/// 返回：处理码为 503 且尚未发送，业务 HTTP 响应仍为 200。
pub(crate) fn unavailable() -> ApiError {
    ApiError::not_sent(
        StatusCode::SERVICE_UNAVAILABLE,
        "service_unavailable",
        "目录已切换、正在停机或消费域不可用",
    )
}

use std::{
    collections::BTreeMap,
    sync::{Arc, RwLock},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use anyhow::{ensure, Result};
use nasa::application::DependencyState;
use serde::Serialize;
use tokio::{
    sync::Semaphore,
    task::{AbortHandle, JoinSet},
};
use tokio_util::{sync::CancellationToken, task::AbortOnDropHandle};

use crate::{
    catalog::source::{Candidate, CatalogSource, LoadAttempt},
    partition::{Context, Counters, Queue, Worker},
    service::Catalog,
};

/// 配置控制面只返回部署状态与固定错误摘要，不暴露目录正文和凭据来源。
#[derive(Clone, Default, Serialize)]
pub struct CatalogStatus {
    pub desired_generation: Option<u64>,
    pub applied_generation: u64,
    pub last_success_unix_seconds: u64,
    pub last_check_unix_seconds: u64,
    pub last_rejection: Option<String>,
    pub rejected_attempts: u64,
    pub draining_bots: Vec<String>,
    pub stopping: bool,
}

/// REST 和指标入口共享当前目录，发布锁只覆盖指针切换。
pub struct TelegramService {
    catalog: RwLock<Arc<Catalog>>,
    pub(crate) status: RwLock<CatalogStatus>,
    pub(crate) context: Arc<Context>,
}

struct LiveWorker {
    alias: String,
    queue: Arc<Queue>,
    abort: AbortHandle,
    deadline: Option<Instant>,
}

/// 唯一目录管理任务拥有所有当前、排空中和候选工作者。
pub struct CatalogManager {
    readiness: nasa::application::ReadinessContributor,
    service: Arc<TelegramService>,
    source: CatalogSource,
    initial: Vec<Worker>,
    workers: BTreeMap<String, LiveWorker>,
    tasks: JoinSet<String>,
}

impl TelegramService {
    /// 业务作用：从已完整加载的候选准备启动目录及唯一生命周期管理器。
    /// 参数说明：`source` 固定配置来源；`candidate` 是启动前校验的目录与材料；`readiness` 控制目录就绪贡献。
    /// 返回：全部凭据及发送资源有效时返回 REST 资源与管理器，尚不产生 Telegram 请求。
    pub fn prepare(
        source: CatalogSource,
        candidate: Candidate,
        readiness: nasa::application::ReadinessContributor,
    ) -> Result<(Arc<Self>, CatalogManager)> {
        let context = Arc::new(Context {
            revision: std::sync::Mutex::new(None),
            inflight: Semaphore::new(candidate.config.http.max_inflight),
            budget: Arc::new(Semaphore::new(16_512)),
            totals: Arc::new(Counters::default()),
        });
        let (catalog, initial) =
            prepare_catalog(candidate, &BTreeMap::new(), context.clone(), None)?;
        *context.revision.lock().unwrap_or_else(|p| p.into_inner()) = Some(catalog.revision);
        let now = unix_seconds();
        let status = CatalogStatus {
            desired_generation: Some(catalog.revision),
            applied_generation: catalog.revision,
            last_success_unix_seconds: now,
            last_check_unix_seconds: now,
            ..CatalogStatus::default()
        };
        let service = Arc::new(Self {
            catalog: RwLock::new(Arc::new(catalog)),
            status: RwLock::new(status),
            context,
        });
        let manager = CatalogManager {
            readiness,
            service: service.clone(),
            source,
            initial,
            workers: BTreeMap::new(),
            tasks: JoinSet::new(),
        };
        // 初始化已具备完整资源，长期任务由 Ready 屏障激活；未知就绪状态不能阻塞该激活过程。
        manager
            .readiness
            .observe(DependencyState::Ready, "catalog_prepared", Instant::now());
        Ok((service, manager))
    }

    /// 业务作用：为一次认证和请求固定完整目录代。
    /// 参数说明：无。
    /// 返回：不可变目录句柄；旧句柄在切换后不能提交新消息。
    pub fn snapshot(&self) -> Arc<Catalog> {
        self.catalog
            .read()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
    }

    /// 业务作用：提供完整配置状态用于识别未生效部署和排空中的机器人。
    /// 参数说明：无。
    /// 返回：脱敏控制面状态副本。
    pub fn observation(&self) -> CatalogStatus {
        self.status
            .read()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
    }

    /// 业务作用：撤销所有目录代的受理权，关闭当前队列以允许消费者排空。
    /// 参数说明：无。
    /// 返回：重复调用安全；已被请求持有的旧目录也无法继续入队。
    pub(crate) fn close(&self) {
        // 权威先失效再关闭队列，避免并发请求把消息交给正在退出的消费者。
        *self
            .context
            .revision
            .lock()
            .unwrap_or_else(|p| p.into_inner()) = None;
        self.status
            .write()
            .unwrap_or_else(|p| p.into_inner())
            .stopping = true;
        for bot in self.snapshot().bots.values() {
            bot.queue.close();
        }
    }
}

impl CatalogManager {
    /// 业务作用：持续对账外部来源，并在应用停止时关闭受理、限时排空全部队列。
    /// 参数说明：`app` 提供 Ready 与停机状态；`cancel` 是应用监督器的取消令牌。
    /// 返回：正常停机排空或记账后成功；工作者意外退出触发应用失败停机。
    pub async fn run(mut self, app: nasa::Application, cancel: CancellationToken) -> Result<()> {
        let initial = std::mem::take(&mut self.initial);
        let catalog = self.service.snapshot();
        self.start_workers(initial, &catalog);
        let mut control = tokio::time::interval(Duration::from_millis(100));
        let mut ticks = tokio::time::interval(Duration::from_millis(self.source.poll_interval_ms));
        ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut loading: Option<AbortOnDropHandle<Result<LoadAttempt>>> = None;
        let read_cancel = cancel.child_token();
        let mut stable = None;
        let mut failure = None;
        loop {
            tokio::select! {
                biased;
                _ = cancel.cancelled() => break,
                _ = control.tick() => {
                    if matches!(app.state(), nasa::application::ApplicationState::Stopping | nasa::application::ApplicationState::Stopped | nasa::application::ApplicationState::Failed) { break; }
                    for worker in self.workers.values() {
                        if worker.deadline.is_some_and(|deadline| Instant::now() >= deadline) { worker.abort.abort(); }
                    }
                }
                result = self.tasks.join_next_with_id(), if !self.tasks.is_empty() => {
                    match result {
                        Some(Ok((_, identity))) if self.workers.remove(&identity).is_some_and(|worker| worker.deadline.is_none()) => {
                            failure = Some(anyhow::anyhow!("发送工作者意外结束")); break;
                        }
                        Some(Ok(_)) => {}
                        Some(Err(error)) => {
                            let identity = self.workers.iter().find(|(_, w)| w.abort.id() == error.id()).map(|(id, _)| id.clone());
                            let expected = identity.and_then(|id| self.workers.remove(&id)).is_some_and(|w| w.deadline.is_some()) && error.is_cancelled();
                            if !expected { failure = Some(anyhow::anyhow!("发送工作者意外退出")); break; }
                        }
                        None => {}
                    }
                    self.update_draining();
                }
                result = async { match loading.as_mut() { Some(job) => job.await, None => std::future::pending().await } } => {
                    loading = None;
                    // 不能确认辅助进程已回收时停止服务，不再建立新的读取执行域。
                    let attempt = match result {
                        Ok(Ok(attempt)) => attempt,
                        Ok(Err(error)) => { failure = Some(error); break; }
                        Err(_) => { failure = Some(anyhow::anyhow!("配置读取任务意外退出")); break; }
                    };
                    self.service.status.write().unwrap_or_else(|p| p.into_inner()).desired_generation = attempt.desired_revision;
                    match attempt.result {
                        Ok(candidate) => {
                            let current = self.service.snapshot();
                            if candidate.fingerprint == current.fingerprint {
                                stable = None;
                                self.service.status.write().unwrap_or_else(|p| p.into_inner()).last_rejection = None;
                            } else if stable == Some(candidate.fingerprint) {
                                // 停机权威高于候选发布；即使读取刚完成也不能重新开放目录。
                                if app.is_ready() && !cancel.is_cancelled() {
                                    if let Err(error) = self.publish(candidate) { self.reject(error); }
                                }
                            } else { stable = Some(candidate.fingerprint); }
                        }
                        Err(error) => { stable = None; self.reject(error); }
                    }
                    self.service.status.write().unwrap_or_else(|p| p.into_inner()).last_check_unix_seconds = unix_seconds();
                }
                _ = ticks.tick() => {
                    if loading.is_none() && app.is_ready() {
                        // 辅助进程完成或被终止回收后才释放本轮；同一时刻只存在一个读取执行域。
                        let source = self.source.clone();
                        let read_cancel = read_cancel.clone();
                        loading = Some(AbortOnDropHandle::new(tokio::spawn(crate::catalog::reader::reload(source, read_cancel))));
                    }
                }
            }
        }
        // 管理器退出即撤销接流，不能在等待读取回收或排空期间继续返回受理成功。
        self.readiness.observe(
            DependencyState::NotReady,
            "catalog_stopping",
            Instant::now(),
        );
        self.service.close();
        if let Some(job) = loading {
            // 先撤销读取并回收材料持有者，再进入发送队列排空；停机中不得留下新候选。
            read_cancel.cancel();
            if let Ok(Err(error)) = job.await {
                failure = Some(error);
            }
        }
        self.stop().await;
        if let Some(error) = failure {
            Err(error)
        } else {
            Ok(())
        }
    }

    /// 业务作用：完整准备新资源后线性化切换目录、权限与受理权。
    /// 参数说明：`candidate` 是连续两轮内容一致的完整候选。
    /// 返回：发布成功后被移除队列进入排空；任何失败不改变有效目录。
    fn publish(&mut self, candidate: Candidate) -> Result<()> {
        let current = self.service.snapshot();
        ensure!(
            candidate.revision > current.revision,
            "目录内容变化必须使用更大的 generation"
        );
        ensure!(
            candidate.config.http == current.http
                && candidate.config.dispatcher == current.dispatcher,
            "HTTP 与停机资源预算为启动设置，修改需要重启"
        );
        let existing = self
            .workers
            .iter()
            .filter(|(_, worker)| worker.deadline.is_none())
            .map(|(id, worker)| (id.clone(), worker.queue.clone()))
            .collect();
        let (next, workers) = prepare_catalog(
            candidate,
            &existing,
            self.service.context.clone(),
            Some(current.transport.clone()),
        )?;
        ensure!(
            workers
                .iter()
                .all(|worker| !self.workers.contains_key(&worker.queue.identity)),
            "机器人仍在排空，暂不能重新加入"
        );
        ensure!(
            self.workers.len() + workers.len() <= 256,
            "当前与排空中的机器人总数不能超过 256"
        );
        let next = Arc::new(next);
        self.start_workers(workers, &next);
        let active: BTreeMap<_, _> = next
            .bots
            .iter()
            .map(|(id, bot)| (bot.queue.identity.clone(), id.clone()))
            .collect();
        {
            // 同一权威锁先撤销旧代受理，再切换指针，避免旧授权落到新目的地。
            let mut authority = self
                .service
                .context
                .revision
                .lock()
                .unwrap_or_else(|p| p.into_inner());
            *authority = Some(next.revision);
            *self
                .service
                .catalog
                .write()
                .unwrap_or_else(|p| p.into_inner()) = next.clone();
            for (identity, worker) in &mut self.workers {
                if let Some(alias) = active.get(identity) {
                    worker.alias.clone_from(alias);
                } else if worker.deadline.is_none() {
                    worker.queue.close();
                    worker.deadline = Some(
                        Instant::now()
                            + Duration::from_millis(current.dispatcher.shutdown_timeout_ms),
                    );
                }
            }
        }
        let mut status = self
            .service
            .status
            .write()
            .unwrap_or_else(|p| p.into_inner());
        status.applied_generation = next.revision;
        status.last_success_unix_seconds = unix_seconds();
        status.last_rejection = None;
        drop(status);
        self.update_draining();
        tracing::info!(
            generation = next.revision,
            bots = next.bots.len(),
            "机器人目录已生效"
        );
        Ok(())
    }

    /// 业务作用：把准备好的消费域移交唯一任务集合，所有退出句柄都有明确所有者。
    /// 参数说明：`workers` 是尚未启动的接收者；`catalog` 提供公开机器人别名。
    /// 返回：每个队列只启动一个消费者。
    fn start_workers(&mut self, workers: Vec<Worker>, catalog: &Catalog) {
        for worker in workers {
            let queue = worker.queue.clone();
            let alias = catalog
                .bots
                .iter()
                .find(|(_, bot)| Arc::ptr_eq(&bot.queue, &queue))
                .map(|(id, _)| id.clone())
                .unwrap_or_default();
            let abort = self.tasks.spawn(worker.run());
            self.workers.insert(
                queue.identity.clone(),
                LiveWorker {
                    alias,
                    queue,
                    abort,
                    deadline: None,
                },
            );
        }
    }

    /// 业务作用：集中发布不含输入值的拒绝原因，保留上一份目录服务能力。
    /// 参数说明：`error` 只接受加载器和候选校验产生的固定摘要。
    /// 返回：增加拒绝次数并记录最近失败原因。
    fn reject(&self, error: anyhow::Error) {
        let reason = error.to_string();
        tracing::warn!(reason, "机器人目录未生效");
        let mut status = self
            .service
            .status
            .write()
            .unwrap_or_else(|p| p.into_inner());
        status.last_rejection = Some(reason);
        status.rejected_attempts = status.rejected_attempts.saturating_add(1);
        status.last_check_unix_seconds = unix_seconds();
    }

    /// 业务作用：发布尚有受理消息需要处理的已移除机器人别名。
    /// 参数说明：无。
    /// 返回：用有界排空集合替换控制面列表。
    fn update_draining(&self) {
        self.service
            .status
            .write()
            .unwrap_or_else(|p| p.into_inner())
            .draining_bots = self
            .workers
            .values()
            .filter(|worker| worker.deadline.is_some())
            .map(|worker| worker.alias.clone())
            .collect();
    }

    /// 业务作用：先撤销全部受理权，再并行排空，期限耗尽后取消并回收任务。
    /// 参数说明：无。
    /// 返回：所有工作者退出后记录累计未发送丢弃和结果未知数量。
    async fn stop(&mut self) {
        self.service.close();
        let deadline = Instant::now()
            + Duration::from_millis(self.service.snapshot().dispatcher.shutdown_timeout_ms);
        for worker in self.workers.values_mut() {
            worker.queue.close();
            worker.deadline = Some(
                worker
                    .deadline
                    .map_or(deadline, |existing| existing.min(deadline)),
            );
        }
        while !self.tasks.is_empty() {
            let now = Instant::now();
            if now >= deadline {
                self.tasks.abort_all();
                while self.tasks.join_next().await.is_some() {}
                break;
            }
            for worker in self.workers.values() {
                if worker.deadline.is_some_and(|deadline| now >= deadline) {
                    worker.abort.abort();
                }
            }
            let wake = self
                .workers
                .values()
                .filter_map(|worker| worker.deadline)
                .filter(|at| *at > now)
                .min()
                .unwrap_or(deadline.max(now));
            match tokio::time::timeout_at(wake.into(), self.tasks.join_next_with_id()).await {
                Ok(Some(Ok((_, identity)))) => {
                    self.workers.remove(&identity);
                }
                Ok(Some(Err(error))) => {
                    self.workers
                        .retain(|_, worker| worker.abort.id() != error.id());
                }
                Ok(None) => break,
                Err(_) => {}
            }
        }
        self.workers.clear();
        self.update_draining();
        use std::sync::atomic::Ordering;
        tracing::info!(
            dropped = self.service.context.totals.dropped.load(Ordering::Relaxed),
            unknown = self.service.context.totals.unknown.load(Ordering::Relaxed),
            "通知消费域已停止"
        );
    }
}

impl Drop for CatalogManager {
    /// 业务作用：外层任务被取消时同步撤销受理权，任务集合随后取消所有消费域。
    /// 参数说明：无。
    /// 返回：只关闭门禁与队列，不尝试阻塞等待。
    fn drop(&mut self) {
        self.readiness
            .observe(DependencyState::NotReady, "catalog_stopped", Instant::now());
        self.service.close();
        for worker in self.workers.values() {
            worker.queue.close();
        }
    }
}

/// 业务作用：仅使用候选已读取材料构造目录，资源准备失败时不触碰运行状态。
/// 参数说明：`candidate` 提供配置与材料；`existing` 是可复用队列；`context` 是进程预算；`transport` 为可复用连接池。
/// 返回：不可变目录与新增消费域。
fn prepare_catalog(
    candidate: Candidate,
    existing: &BTreeMap<String, Arc<Queue>>,
    context: Arc<Context>,
    transport: Option<Arc<crate::service::Transport>>,
) -> Result<(Catalog, Vec<Worker>)> {
    let (config, materials, revision, fingerprint) = candidate.into_parts();
    Catalog::new(
        config,
        |reference| {
            materials
                .get(reference.strip_prefix("secret://").unwrap_or_default())
                .map(|material| zeroize::Zeroizing::new(material.to_string()))
                .ok_or_else(|| anyhow::anyhow!("凭据引用未解析"))
        },
        existing,
        context,
        revision,
        fingerprint,
        transport,
    )
}

/// 业务作用：为配置运维状态提供跨进程可解释的秒级时刻。
/// 参数说明：无。
/// 返回：Unix 秒数，时钟早于纪元时为零。
fn unix_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

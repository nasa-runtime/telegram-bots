use std::sync::Arc;

use nasa::application::{
    Application, ApplicationError, ApplicationFuture, ApplicationPhase, ApplicationResult,
    ComponentId, Initialization, InitializationContext, InitializerKind, InitializerSpec,
    ManagedResource, ReadinessPolicy, ShutdownAction, ShutdownContext, WeakApplication,
};

use crate::{
    catalog::{CatalogManager, TelegramService},
    observability::CatalogMetrics,
};

/// napp 持有发送资源的生命周期所有权，请求仅获取同一资源的共享句柄。
pub struct TelegramHandle(Arc<TelegramService>);

impl TelegramHandle {
    /// 业务作用：为已进入业务流水线的请求保留目录资源。
    /// 参数说明：无。
    /// 返回：共享服务句柄；停机后其受理权已撤销，不能继续提交消息。
    pub fn service(&self) -> Arc<TelegramService> {
        self.0.clone()
    }
}

impl ManagedResource for TelegramHandle {
    /// 业务作用：在正常停机或启动回滚时撤销目录受理权并关闭队列。
    /// 参数说明：`_context` 是 napp 的清理预算；此处不等待网络或文件操作。
    /// 返回：门禁关闭后成功；消费任务的排空由框架监督的目录管理器承担。
    fn shutdown<'a>(&'a mut self, _context: &'a ShutdownContext) -> ApplicationFuture<'a> {
        Box::pin(async move {
            self.0.close();
            Ok(())
        })
    }
}

impl Drop for TelegramHandle {
    /// 业务作用：资源未进入异步清理或被提前释放时仍撤销受理权。
    /// 参数说明：无。
    /// 返回：同步关闭门禁，不等待后台任务。
    fn drop(&mut self) {
        self.0.close();
    }
}

struct CatalogInitialization {
    application: WeakApplication,
    metrics: Arc<CatalogMetrics>,
    manager: Option<CatalogManager>,
}

struct StartupReadCleanup(Arc<crate::catalog::reader::ReadProcess>);

impl ShutdownAction for StartupReadCleanup {
    /// 业务作用：为启动读取的反向清理提供稳定归属名称。
    /// 参数说明：无。
    /// 返回：不含文件路径或凭据的动作名称。
    fn label(&self) -> &'static str {
        "catalog-startup-reader"
    }

    /// 业务作用：初始化 future 被取消后仍等待读取进程终止，避免遗留材料持有者。
    /// 参数说明：`_context` 是 napp 的共享停机预算，实际等待另受一秒回收预算约束。
    /// 返回：进程已回收时成功；无法确认回收时记录停机失败。
    fn shutdown<'a>(&'a mut self, _context: &'a ShutdownContext) -> ApplicationFuture<'a> {
        Box::pin(async move {
            self.0.stop().await.map_err(|error| {
                ApplicationError::new(
                    ComponentId::Application,
                    ApplicationPhase::Stopping,
                    error.to_string(),
                )
            })
        })
    }
}

/// 业务作用：在 UserHook 登记目录生命周期与指标，不自行建立运行时或启动消费者。
/// 参数说明：`app` 是尚未封口的应用容器。
/// 返回：登记成功后由 napp 执行初始化、Ready 激活和反向清理；重复登记返回错误。
pub fn install(app: &Application) -> ApplicationResult<()> {
    let metrics = Arc::new(CatalogMetrics::default());
    app.register_metrics_source(metrics.clone())?;
    app.register_initializer(
        InitializerSpec::new("telegram-catalog").kind(InitializerKind::Hosted),
        CatalogInitialization {
            application: app.downgrade(),
            metrics,
            manager: None,
        },
    )
}

impl Initialization for CatalogInitialization {
    /// 业务作用：在统一初始化期限内验证目录，并发布与应用引导一致的受管资源。
    /// 参数说明：`context` 提供取消权威、引导快照和资源归属。
    /// 返回：目录和凭据完整有效才建立资源；失败阻止监听和长期任务启动。
    fn initialize<'a>(
        &'a mut self,
        context: &'a mut InitializationContext<'_>,
    ) -> ApplicationFuture<'a> {
        Box::pin(async move {
            let process = Arc::new(crate::catalog::reader::ReadProcess::default());
            // 先登记回收所有者再启动辅助进程，框架取消初始化 future 后仍可等待其退出。
            context.activate(Box::new(StartupReadCleanup(process.clone())));
            let (source, candidate) =
                crate::catalog::reader::preflight(context.cancellation_token(), &process)
                    .await
                    .map_err(initialization_error)?;
            source
                .validate_bootstrap(context.config().value())
                .map_err(initialization_error)?;
            let readiness =
                context.register_readiness("catalog", ReadinessPolicy::critical_immediate())?;
            let (service, manager) = TelegramService::prepare(source, candidate, readiness)
                .map_err(initialization_error)?;
            self.manager = Some(manager);
            context.register_managed_resource(None, TelegramHandle(service.clone()))?;
            self.metrics.bind(&service);
            Ok(())
        })
    }

    /// 业务作用：把目录管理器暂存为关键任务，使消费者仅在 napp Ready 之后运行。
    /// 参数说明：`context` 提供属于此 initializer 的受管任务登记入口。
    /// 返回：工厂登记成功后由框架持有任务；失败或后续启动失败时自动撤销目录。
    fn after<'a>(
        &'a mut self,
        context: &'a mut InitializationContext<'_>,
    ) -> ApplicationFuture<'a> {
        Box::pin(async move {
            let manager = self
                .manager
                .take()
                .ok_or_else(|| initialization_error(anyhow::anyhow!("目录资源尚未初始化")))?;
            let app = self
                .application
                .upgrade()
                .ok_or_else(|| initialization_error(anyhow::anyhow!("应用生命周期已结束")))?;
            // 任务工厂交给 napp，后续组件无法 Ready 时不得提前启动发送工作者。
            context.stage_critical("manager", move |cancel| manager.run(app, cancel))
        })
    }
}

/// 业务作用：把目录校验的脱敏摘要归入统一初始化失败报告。
/// 参数说明：`error` 是目录加载或资源装配返回的业务错误。
/// 返回：带有初始化阶段和应用归属的错误，不附加文件正文或凭据。
fn initialization_error(error: anyhow::Error) -> ApplicationError {
    ApplicationError::new(
        ComponentId::Application,
        ApplicationPhase::Initialization,
        error.to_string(),
    )
}

use std::process::ExitCode;

use nasa::application::{ApplicationSpec, ComponentId};
use telegram_bots::{isolated, runtime::TelegramService};

mod api;
nasa::web::mvc_router!(nasa::Application);

/// 业务作用：在应用组件启动前验证必需外部目录，再把进程生命周期交给 NASA。
/// 参数说明：无。
/// 返回：配置或运行失败返回非零退出码；正常信号停机返回成功。
fn main() -> ExitCode {
    if let Some(status) = isolated::worker_entry() {
        return status;
    }
    let prepared = isolated::preflight();
    let (source, candidate) = match prepared {
        Ok(prepared) => prepared,
        Err(error) => {
            eprintln!("目录启动失败：{error}");
            return ExitCode::FAILURE;
        }
    };
    nasa::application::run(
        ApplicationSpec::new(&[ComponentId::Web, ComponentId::NacosDiscovery])
            .with_default_name("telegram-bots")
            .with_web_route_meta(route_meta)
            .with_web_factory(build_router),
        move |app| async move {
            let (service, manager) = TelegramService::prepare(source, candidate)?;
            app.register_metrics_source(service.clone())?;
            app.register(service)?;
            let owner = app.clone();
            app.spawn_critical("telegram-catalog", move |cancel| manager.run(owner, cancel))
                .await?;
            Ok::<(), anyhow::Error>(())
        },
    )
}

/// 业务作用：向应用框架声明业务路由的认证与响应合同。
/// 参数说明：无。
/// 返回：由静态端点声明生成的完整路由元数据。
fn route_meta() -> ::std::vec::Vec<nasa::application::RouteMeta> {
    crate::__mvc::ROUTES
        .iter()
        .map(|entry| nasa::application::RouteMeta {
            method: entry.method,
            path: entry.path,
            handler: entry.handler,
            produces: entry.produces,
            consumes: entry.consumes,
            request_schema: entry.request_schema,
            response_schema: entry.response_schema,
            query_parameters: entry.query_parameters,
            header_parameters: entry.header_parameters,
            success_status: entry.success_status,
            additional_responses: entry.additional_responses,
            streaming: entry.streaming,
            auth_required: ::core::matches!(
                entry.policy.auth,
                nasa::web::AuthRequirement::Required
            ),
        })
        .collect()
}

/// 业务作用：构造只含自动收集端点、尚未补齐状态的业务路由。
///
/// 状态刻意不在这里补：`configure_router` 的定制与框架探针都必须先作用在
/// `Router<Application>` 上，`with_state` 由运行时在装配顺序末尾统一执行。
///
/// 参数说明：
/// - `context`：由 napp Ready 构造，保证 interceptor 与 handler 使用同一个 Application clone。
///
/// 返回：路由和安全流水线装配成功时返回统一状态 Router；冲突或合同错误时拒绝监听。
fn build_router(
    context: nasa::application::WebBuildContext,
) -> nasa::application::ApplicationResult<nasa::web::Router<nasa::application::Application>> {
    context.build(|router, mapping_runtime, mapping_plan, application| {
        crate::__mvc::try_register_all(router, mapping_runtime, mapping_plan, application)
    })
}

mod rest;

/// 业务作用：声明日志、Web 与注册发现组件，把通知目录的初始化、任务和资源交给 napp。
/// 参数说明：`app` 是完成基础组件启动、尚未开放业务入口的应用容器。
/// 返回：业务生命周期登记成功才继续初始化；目录不可用时由框架统一停止启动。
#[nasa::application("log", "web", "nacos-discovery")]
async fn main(app: nasa::Application) -> anyhow::Result<()> {
    telegram_bots::application::install(&app)?;
    Ok(())
}

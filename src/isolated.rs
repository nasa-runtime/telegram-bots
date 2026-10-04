use std::{
    io::{Read, Write},
    process::{ExitCode, Stdio},
    time::Duration,
};

use anyhow::{anyhow, ensure, Result};
use serde::{Deserialize, Serialize};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    process::{Child, Command},
    time::{timeout, timeout_at, Instant},
};
use tokio_util::sync::CancellationToken;
use zeroize::Zeroizing;

use crate::source::{Candidate, CatalogSource, LoadAttempt};

const WORKER_ARG: &str = "--catalog-loader";
const MAX_PACKET: usize = 16 * 1024 * 1024;
const BOOTSTRAP_TIMEOUT: Duration = Duration::from_secs(3);
const REAP_TIMEOUT: Duration = Duration::from_secs(1);

#[derive(Serialize, Deserialize)]
struct Reply {
    source: Option<CatalogSource>,
    desired_revision: Option<u64>,
    result: std::result::Result<Candidate, String>,
}

impl Reply {
    /// 业务作用：把隔离传输或读取失败转成可观测且不含材料的整批拒绝。
    /// 参数说明：`reason` 是固定错误摘要。
    /// 返回：没有候选与期望代号的失败响应，旧目录仍可继续服务。
    fn rejected(reason: &str) -> Self {
        Self {
            source: None,
            desired_revision: None,
            result: Err(reason.into()),
        }
    }
}

/// 业务作用：在监听和基础设施启动前隔离预检全部配置，并响应启动期间的终止信号。
/// 参数说明：无。
/// 返回：来源和候选完整有效才成功；超时、信号或子进程异常均拒绝启动。
pub fn preflight() -> Result<(CatalogSource, Candidate)> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|_| anyhow!("配置预检运行时无法创建"))?;
    runtime.block_on(async {
        let cancel = CancellationToken::new();
        #[cfg(unix)]
        let mut terminate =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                .map_err(|_| anyhow!("启动信号处理无法建立"))?;
        let interrupt = tokio::signal::ctrl_c();
        tokio::pin!(interrupt);
        let stopped = async {
            #[cfg(unix)]
            tokio::select! { _ = terminate.recv() => {}, _ = &mut interrupt => {} }
            #[cfg(not(unix))]
            {
                let _ = interrupt.await;
            }
        };
        let read = read_worker(None, cancel.clone());
        tokio::pin!(read);
        let reply = tokio::select! {
            biased;
            _ = stopped => {
                // 启动终止先撤销读取，再等待回收；不能遗留携带凭据的后台读取进程。
                cancel.cancel();
                let _ = read.await?;
                return Err(anyhow!("配置预检已终止"));
            }
            result = &mut read => result?,
        };
        let candidate = reply.result.map_err(anyhow::Error::msg)?;
        let source = reply.source.ok_or_else(|| anyhow!("配置来源响应不完整"))?;
        Ok((source, candidate))
    })
}

/// 业务作用：隔离一次运行期完整读取，使超时后可以安全开始新的对账。
/// 参数说明：`source` 是启动时冻结的来源；`cancel` 撤销本轮读取并回收辅助进程。
/// 返回：普通读取失败作为候选拒绝返回；不能回收进程时返回致命错误，禁止继续创建读取进程。
pub(crate) async fn reload(
    source: CatalogSource,
    cancel: CancellationToken,
) -> Result<LoadAttempt> {
    let reply = read_worker(Some(source), cancel).await?;
    Ok(LoadAttempt {
        desired_revision: reply.desired_revision,
        result: reply.result.map_err(anyhow::Error::msg),
    })
}

/// 业务作用：独占一个只读辅助进程，将文件系统等待限制在可终止的进程边界内。
/// 参数说明：`source` 为空时读取引导设置，否则复验冻结来源；`cancel` 是本轮控制权威。
/// 返回：已回收进程的完整响应；回收超过预算时返回致命错误。
async fn read_worker(source: Option<CatalogSource>, cancel: CancellationToken) -> Result<Reply> {
    let packet =
        Zeroizing::new(serde_json::to_vec(&source).map_err(|_| anyhow!("配置来源编码失败"))?);
    ensure!(packet.len() <= MAX_PACKET, "配置来源超过隔离传输上限");
    let started = Instant::now();
    let executable = std::env::current_exe().map_err(|_| anyhow!("配置读取程序无法定位"))?;
    let child = Command::new(executable)
        .arg(WORKER_ARG)
        .arg(std::process::id().to_string())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn();
    let mut child = match child {
        Ok(child) => child,
        Err(_) => return Ok(Reply::rejected("配置读取进程无法创建")),
    };
    let ceiling = source
        .as_ref()
        .map(|s| Duration::from_millis(s.load_timeout_ms))
        .unwrap_or(BOOTSTRAP_TIMEOUT);
    let result = tokio::select! {
        biased;
        _ = cancel.cancelled() => Err(anyhow!("配置读取已取消")),
        result = timeout_at(started + ceiling, exchange(&mut child, &packet, started)) => {
            result.unwrap_or_else(|_| Err(anyhow!("配置读取超时，保留当前目录")))
        }
    };
    match result {
        Ok(reply) => Ok(reply),
        Err(error) => {
            // 杀死并回收后才释放本轮所有权，迟到输出不能进入下一轮，也不能累积阻塞进程。
            terminate_and_reap(&mut child).await?;
            Ok(Reply::rejected(&error.to_string()))
        }
    }
}

/// 业务作用：通过匿名管道交换有界配置快照，在整个读取预算内等待辅助进程退出。
/// 参数说明：`child` 是本轮独占进程；`packet` 是冻结来源；`started` 是读取开始时刻。
/// 返回：完整且已退出进程的响应；截断、超限、超时或异常退出一律拒绝候选。
async fn exchange(child: &mut Child, packet: &[u8], started: Instant) -> Result<Reply> {
    let mut input = child
        .stdin
        .take()
        .ok_or_else(|| anyhow!("配置读取通道不可用"))?;
    let mut output = child
        .stdout
        .take()
        .ok_or_else(|| anyhow!("配置读取通道不可用"))?;
    input
        .write_all(packet)
        .await
        .map_err(|_| anyhow!("配置读取通道失败"))?;
    input
        .shutdown()
        .await
        .map_err(|_| anyhow!("配置读取通道失败"))?;
    drop(input);
    let budget = output
        .read_u64()
        .await
        .map_err(|_| anyhow!("配置读取进程异常退出"))?;
    ensure!((100..=3000).contains(&budget), "配置读取预算响应无效");
    // 首次引导解析受固定三秒上限保护；读到配置预算后收紧同一个起始时刻的期限。
    timeout_at(started + Duration::from_millis(budget), async {
        let size = output
            .read_u32()
            .await
            .map_err(|_| anyhow!("配置读取响应不完整"))? as usize;
        ensure!(size <= MAX_PACKET, "配置读取响应超过上限");
        let mut bytes = Zeroizing::new(vec![0; size]);
        output
            .read_exact(&mut bytes)
            .await
            .map_err(|_| anyhow!("配置读取响应不完整"))?;
        let reply = serde_json::from_slice(&bytes).map_err(|_| anyhow!("配置读取响应无效"))?;
        let status = child
            .wait()
            .await
            .map_err(|_| anyhow!("配置读取进程退出状态不可用"))?;
        ensure!(status.success(), "配置读取进程异常退出");
        Ok(reply)
    })
    .await
    .unwrap_or_else(|_| Err(anyhow!("配置读取超时，保留当前目录")))
}

/// 业务作用：强制结束已失去读取权威的进程，并确认其资源已由操作系统回收。
/// 参数说明：`child` 是唯一的在途读取进程。
/// 返回：一秒内回收成功才允许下一轮读取；内核无法回收时让应用进入失败停机。
async fn terminate_and_reap(child: &mut Child) -> Result<()> {
    let _ = child.start_kill();
    timeout(REAP_TIMEOUT, child.wait())
        .await
        .map_err(|_| anyhow!("配置读取进程无法按时回收，停止应用"))?
        .map_err(|_| anyhow!("配置读取进程回收失败，停止应用"))?;
    Ok(())
}

/// 业务作用：识别只读辅助角色，在业务运行时建立前执行隔离协议。
/// 参数说明：无。
/// 返回：普通启动返回 None；辅助进程完成协议返回成功，协议失败仅返回非零状态。
pub fn worker_entry() -> Option<ExitCode> {
    let mut args = std::env::args_os().skip(1);
    if args.next().as_deref() != Some(std::ffi::OsStr::new(WORKER_ARG)) {
        return None;
    }
    let parent = args
        .next()
        .and_then(|s| s.to_str().and_then(|s| s.parse::<u32>().ok()));
    let result = parent
        .filter(|_| args.next().is_none())
        .ok_or_else(|| anyhow!("读取进程身份无效"))
        .and_then(|parent| {
            guard_parent(parent)?;
            serve()
        });
    Some(if result.is_ok() {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    })
}

/// 业务作用：将辅助进程的存活绑定到启动它的服务，防止父进程退出后继续读取材料。
/// 参数说明：`parent` 是启动命令携带的父进程标识。
/// 返回：父进程身份一致才允许读取；父进程消失时由系统信号或存活检查结束辅助角色。
fn guard_parent(parent: u32) -> Result<()> {
    #[cfg(target_os = "linux")]
    {
        // 先设置父进程退出信号，再复验身份，覆盖设置期间父进程已经退出的窗口。
        ensure!(
            unsafe { libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) } == 0,
            "读取进程存活保护失败"
        );
        ensure!(
            unsafe { libc::getppid() } as u32 == parent,
            "读取进程父级已退出"
        );
    }
    #[cfg(all(unix, not(target_os = "linux")))]
    std::thread::Builder::new()
        .name("catalog-parent".into())
        .spawn(move || loop {
            if unsafe { libc::getppid() } as u32 != parent {
                std::process::exit(1);
            }
            std::thread::sleep(Duration::from_millis(100));
        })
        .map_err(|_| anyhow!("读取进程存活保护失败"))?;
    Ok(())
}

/// 业务作用：在辅助进程内完成整批引导、目录和材料读取，仅通过匿名管道返回已校验结果。
/// 参数说明：无。
/// 返回：响应成功写入才正常退出；不会建立监听、发送消息或把材料写入日志及磁盘。
fn serve() -> Result<()> {
    let mut bytes = Zeroizing::new(Vec::new());
    std::io::stdin()
        .take(MAX_PACKET as u64 + 1)
        .read_to_end(&mut bytes)?;
    ensure!(bytes.len() <= MAX_PACKET, "读取请求超过上限");
    let source: Option<CatalogSource> = serde_json::from_slice(&bytes)?;
    let startup = source.is_none();
    let source = source.map(Ok).unwrap_or_else(CatalogSource::standard);
    let mut output = std::io::stdout().lock();
    let budget = source.as_ref().map(|s| s.load_timeout_ms).unwrap_or(3000);
    output.write_all(&budget.to_be_bytes())?;
    output.flush()?;
    let reply = match source {
        Ok(source) => {
            let attempt = source.load();
            Reply {
                source: startup.then_some(source),
                desired_revision: attempt.desired_revision,
                result: attempt.result.map_err(|error| error.to_string()),
            }
        }
        Err(error) => Reply::rejected(&error.to_string()),
    };
    let packet = Zeroizing::new(serde_json::to_vec(&reply)?);
    ensure!(packet.len() <= MAX_PACKET, "读取响应超过上限");
    output.write_all(&(packet.len() as u32).to_be_bytes())?;
    output.write_all(&packet)?;
    output.flush()?;
    Ok(())
}

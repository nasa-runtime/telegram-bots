use std::{
    os::fd::{AsRawFd, RawFd},
    os::unix::net::UnixStream,
    time::Duration,
};

use anyhow::{anyhow, ensure, Result};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    time::{timeout, timeout_at, Instant},
};
use tokio_util::sync::CancellationToken;
use zeroize::Zeroizing;

use crate::catalog::source::{Candidate, CatalogSource, LoadAttempt};

const MAX_FILE: usize = 1_048_576;
const MAX_PATH: usize = 4095;
const REAP_TIMEOUT: Duration = Duration::from_secs(1);

/// 初始化清理栈持有读取进程，取消 future 后仍能确认回收。
#[derive(Default)]
pub(crate) struct ReadProcess(tokio::sync::Mutex<Option<FileReader>>);

impl ReadProcess {
    /// 业务作用：在初始化取消或失败后终止并回收仍存活的文件读取者。
    /// 参数说明：无。
    /// 返回：进程已回收才成功；超过一秒回收预算时报告致命错误。
    pub(crate) async fn stop(&self) -> Result<()> {
        let mut slot = self.0.lock().await;
        if let Some(reader) = slot.as_mut() {
            reader.stop().await?;
        }
        slot.take();
        Ok(())
    }
}

/// 一轮目录读取独占一个只执行文件系统调用的子进程和匿名通道。
pub(crate) struct FileReader {
    pid: Option<libc::pid_t>,
    stream: tokio::net::UnixStream,
    identity: Option<(u64, u64)>,
}

impl FileReader {
    /// 业务作用：创建不重新执行应用入口的只读进程，隔离不可取消的文件系统等待。
    /// 参数说明：无。
    /// 返回：父进程取得有界读取通道；创建失败不产生长期任务或第二个业务实例。
    fn start() -> Result<Self> {
        let (parent, child) = UnixStream::pair().map_err(|_| anyhow!("配置读取通道无法创建"))?;
        parent
            .set_nonblocking(true)
            .map_err(|_| anyhow!("配置读取通道无法配置"))?;
        let stream = tokio::net::UnixStream::from_std(parent)
            .map_err(|_| anyhow!("配置读取通道无法接管"))?;
        let mut bytes = vec![0u8; MAX_FILE + 1];
        let limit = unsafe { libc::sysconf(libc::_SC_OPEN_MAX) };
        ensure!(limit > 0 && limit <= i32::MAX as _, "文件描述符边界不可用");
        let parent_pid = unsafe { libc::getpid() };
        // 所有分配与运行时登记都在 fork 前完成；子进程只调用异步信号安全的系统接口，不能进入 Rust 分配器、日志或异步运行时。
        let pid = unsafe { libc::fork() };
        if pid == 0 {
            unsafe {
                read_loop(
                    child.as_raw_fd(),
                    bytes.as_mut_ptr(),
                    limit as i32,
                    parent_pid,
                )
            }
        }
        ensure!(pid > 0, "配置读取进程无法创建");
        drop(child);
        Ok(Self {
            pid: Some(pid),
            stream,
            identity: None,
        })
    }

    /// 业务作用：在独占读取进程内打开并读取一个有界普通文件。
    /// 参数说明：`path` 为来源路径；`limit` 是字节上限；`optional` 允许文件缺失；`probe` 允许 profile 候选不是普通文件。
    /// 返回：完整文件字节、可跳过来源的 None，或不含路径及材料的拒绝原因；空文件是否有效由来源语义决定。
    pub(crate) async fn read(
        &mut self,
        path: &str,
        limit: usize,
        optional: bool,
        probe: bool,
    ) -> Result<Option<Zeroizing<Vec<u8>>>> {
        self.request(path, limit, optional, probe, false).await
    }

    /// 业务作用：在可终止进程内枚举单层目录，文件名不会交给 shell 解释。
    /// 参数说明：`path` 是固定目录；`optional` 只允许目录不存在。
    /// 返回：最多 4096 个 UTF-8 文件名；权限、类型、编码与规模错误拒绝整轮。
    pub(crate) async fn list(&mut self, path: &str, optional: bool) -> Result<Vec<String>> {
        let Some(bytes) = self.request(path, MAX_FILE, optional, false, true).await? else {
            return Ok(Vec::new());
        };
        let mut names = Vec::new();
        for name in bytes
            .split(|byte| *byte == 0)
            .filter(|name| !name.is_empty())
        {
            ensure!(names.len() < 4096, "配置目录条目超过上限");
            names.push(
                std::str::from_utf8(name)
                    .map_err(|_| anyhow!("配置目录文件名必须为 UTF-8"))?
                    .to_owned(),
            );
        }
        Ok(names)
    }

    /// 业务作用：返回最近一次成功读取的文件身份，识别别名和硬链接重复来源。
    /// 参数说明：无。
    /// 返回：设备与 inode；缺失或拒绝后没有有效身份。
    pub(crate) fn identity(&self) -> Option<(u64, u64)> {
        self.identity
    }

    /// 业务作用：通过有界协议请求文件或目录读取，不在业务进程执行文件系统遍历。
    /// 参数说明：`path/limit/optional/probe` 固定来源约束；`directory` 选择单层目录读取。
    /// 返回：完整有界响应或安全错误，失败后没有残留来源身份。
    async fn request(
        &mut self,
        path: &str,
        limit: usize,
        optional: bool,
        probe: bool,
        directory: bool,
    ) -> Result<Option<Zeroizing<Vec<u8>>>> {
        self.identity = None;
        ensure!(
            !path.is_empty()
                && path.len() <= MAX_PATH
                && !path.as_bytes().contains(&0)
                && limit <= MAX_FILE,
            "配置读取参数超出范围"
        );
        self.stream
            .write_u32(path.len() as u32)
            .await
            .map_err(|_| anyhow!("配置读取通道失败"))?;
        self.stream
            .write_u32(limit as u32 | if directory { 1 << 31 } else { 0 })
            .await
            .map_err(|_| anyhow!("配置读取通道失败"))?;
        self.stream
            .write_all(path.as_bytes())
            .await
            .map_err(|_| anyhow!("配置读取通道失败"))?;
        let status = self
            .stream
            .read_u32()
            .await
            .map_err(|_| anyhow!("配置读取进程异常退出"))?;
        let size = self
            .stream
            .read_u32()
            .await
            .map_err(|_| anyhow!("配置读取响应不完整"))? as usize;
        match status {
            0 => {
                ensure!(size <= limit, "配置读取响应超过上限");
                let device = self
                    .stream
                    .read_u64()
                    .await
                    .map_err(|_| anyhow!("来源身份响应不完整"))?;
                let inode = self
                    .stream
                    .read_u64()
                    .await
                    .map_err(|_| anyhow!("来源身份响应不完整"))?;
                self.identity = Some((device, inode));
                let mut bytes = Zeroizing::new(vec![0; size]);
                self.stream
                    .read_exact(&mut bytes)
                    .await
                    .map_err(|_| anyhow!("配置读取响应不完整"))?;
                Ok(Some(bytes))
            }
            1 if optional => Ok(None),
            2 if probe => Ok(None),
            2 => Err(anyhow!("配置或凭据来源必须为普通文件")),
            3 => Err(anyhow!("配置或凭据文件超过大小上限")),
            _ => Err(anyhow!("配置或凭据文件无法读取")),
        }
    }

    /// 业务作用：先结束读取权威，再等待读取进程从系统进程表消失。
    /// 参数说明：无。
    /// 返回：确认回收后清除 PID；超时返回错误并禁止继续创建读取者。
    async fn stop(&mut self) -> Result<()> {
        let Some(pid) = self.pid else {
            return Ok(());
        };
        unsafe {
            libc::kill(pid, libc::SIGKILL);
        }
        timeout(REAP_TIMEOUT, async {
            loop {
                let status = unsafe { libc::waitpid(pid, std::ptr::null_mut(), libc::WNOHANG) };
                if status == pid
                    || (status < 0
                        && std::io::Error::last_os_error().raw_os_error() == Some(libc::ECHILD))
                {
                    self.pid = None;
                    return Ok(());
                }
                if status < 0 && std::io::Error::last_os_error().raw_os_error() != Some(libc::EINTR)
                {
                    return Err(anyhow!("配置读取进程回收失败，停止应用"));
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .map_err(|_| anyhow!("配置读取进程无法按时回收，停止应用"))?
    }
}

impl Drop for FileReader {
    /// 业务作用：运行期任务被强制取消时终止读取者，并安排有界非阻塞回收。
    /// 参数说明：无。
    /// 返回：同步撤销进程执行权；运行时仍存活时继续回收，不能阻塞析构。
    fn drop(&mut self) {
        let Some(pid) = self.pid.take() else {
            return;
        };
        unsafe {
            libc::kill(pid, libc::SIGKILL);
        }
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn(async move {
                let deadline = Instant::now() + REAP_TIMEOUT;
                loop {
                    let result = unsafe { libc::waitpid(pid, std::ptr::null_mut(), libc::WNOHANG) };
                    if result == pid
                        || (result < 0
                            && std::io::Error::last_os_error().raw_os_error() != Some(libc::EINTR))
                        || Instant::now() >= deadline
                    {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
            });
        }
    }
}

/// 业务作用：在 napp 初始化预算内读取完整目录，取消时保留独立回收所有者。
/// 参数说明：`cancel` 是初始化取消令牌；`process` 属于 napp 初始化清理栈。
/// 返回：目录与材料完整有效才成功；任何读取拒绝阻止 Ready。
pub(crate) async fn preflight(
    cancel: CancellationToken,
    process: &ReadProcess,
) -> Result<(CatalogSource, Candidate)> {
    let (source, attempt) = read_catalog(None, cancel, process).await?;
    let candidate = attempt.result?;
    Ok((source.ok_or_else(|| anyhow!("配置来源不可用"))?, candidate))
}

/// 业务作用：对固定来源执行一次可终止的完整读取，失败时保留现有目录。
/// 参数说明：`source` 是启动时固定的配置来源；`cancel` 撤销本轮读取权威。
/// 返回：普通读取错误作为候选拒绝；无法回收进程时返回致命错误。
pub(crate) async fn reload(
    source: CatalogSource,
    cancel: CancellationToken,
) -> Result<LoadAttempt> {
    let (_, attempt) = read_catalog(Some(source), cancel, &ReadProcess::default()).await?;
    Ok(attempt)
}

/// 业务作用：为整轮文件读取、合并和校验维护统一期限，并在结束后回收隔离进程。
/// 参数说明：`source` 为空时建立启动来源；`cancel` 为取消权威；`process` 持有取消后的回收权。
/// 返回：来源和候选结果均只来自已完成且已回收的读取轮次。
async fn read_catalog(
    source: Option<CatalogSource>,
    cancel: CancellationToken,
    process: &ReadProcess,
) -> Result<(Option<CatalogSource>, LoadAttempt)> {
    let started = Instant::now();
    let mut slot = process.0.lock().await;
    *slot = Some(FileReader::start()?);
    let reader = slot.as_mut().ok_or_else(|| anyhow!("配置读取者不可用"))?;
    let ceiling = Duration::from_millis(source.as_ref().map_or(3000, |s| s.load_timeout_ms));
    let result = tokio::select! {
        biased;
        _ = cancel.cancelled() => Err(anyhow!("配置读取已取消")),
        result = timeout_at(started + ceiling, async {
            let source = match source { Some(source) => source, None => CatalogSource::standard(reader).await? };
            let deadline = started + Duration::from_millis(source.load_timeout_ms);
            let attempt = timeout_at(deadline, source.load(reader)).await.map_err(|_| anyhow!("配置读取超时，保留当前目录"))?;
            // 解析使用有界内存并在异步阶段间让出执行权；完成时再复验期限，迟到候选不能发布。
            ensure!(Instant::now() < deadline, "配置读取超时，保留当前目录");
            Ok((Some(source), attempt))
        }) => result.unwrap_or_else(|_| Err(anyhow!("配置读取超时，保留当前目录"))),
    };
    // 不论读取成功与否都先杀死并回收；下一轮不能与失去权威的读取者并存。
    reader.stop().await?;
    slot.take();
    Ok(result.unwrap_or_else(|error| {
        (
            None,
            LoadAttempt {
                desired_revision: None,
                result: Err(error),
            },
        )
    }))
}

/// 业务作用：在 fork 后仅执行普通文件读取与匿名通道传输，禁止触碰父进程的运行时和锁。
/// 参数说明：`socket` 是子端通道；`buffer` 是 fork 前分配的单文件缓冲；`fd_limit` 为描述符上界；`parent` 为父进程身份。
/// 返回：协议终止或身份失效时直接 _exit，不运行 Rust 析构。
/// 安全边界：缓冲至少 MAX_FILE + 1 字节；本函数及所调用函数只允许异步信号安全系统调用与无分配的内存运算。
unsafe fn read_loop(socket: RawFd, buffer: *mut u8, fd_limit: i32, parent: libc::pid_t) -> ! {
    // 清除继承的应用信号回调；独立闹钟即使父进程意外消失也会终止读取者。
    let mut action: libc::sigaction = std::mem::zeroed();
    action.sa_sigaction = libc::SIG_DFL;
    libc::sigemptyset(&mut action.sa_mask);
    for signal in 1..=64 {
        libc::sigaction(signal, &action, std::ptr::null_mut());
    }
    libc::sigprocmask(libc::SIG_SETMASK, &action.sa_mask, std::ptr::null_mut());
    libc::alarm(4);
    #[cfg(target_os = "linux")]
    if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) != 0 {
        libc::_exit(1);
    }
    if libc::getppid() != parent || libc::dup2(socket, 3) < 0 {
        libc::_exit(1);
    }
    // 子进程不能延长业务 listener 或出站连接的生命，保留的唯一通道固定为 fd 3。
    libc::close(0);
    libc::close(1);
    libc::close(2);
    #[cfg(target_os = "linux")]
    let closed = libc::syscall(libc::SYS_close_range, 4u32, u32::MAX, 0u32) == 0;
    #[cfg(not(target_os = "linux"))]
    let closed = false;
    if !closed {
        for fd in 4..fd_limit {
            libc::close(fd);
        }
    }
    loop {
        let mut header = [0u8; 8];
        if !transfer(3, header.as_mut_ptr(), 8, false) {
            libc::_exit(0);
        }
        let length = u32::from_be_bytes([header[0], header[1], header[2], header[3]]) as usize;
        let request = u32::from_be_bytes([header[4], header[5], header[6], header[7]]);
        let directory = request & (1 << 31) != 0;
        let limit = (request & !(1 << 31)) as usize;
        if length == 0 || length > MAX_PATH || limit > MAX_FILE {
            libc::_exit(1);
        }
        let mut path = [0u8; MAX_PATH + 1];
        if !transfer(3, path.as_mut_ptr(), length, false) {
            libc::_exit(1);
        }
        let fd = libc::open(path.as_ptr().cast(), libc::O_RDONLY | libc::O_NONBLOCK);
        let mut status = 0u32;
        let mut size = 0usize;
        let mut device = 0u64;
        let mut inode = 0u64;
        if fd < 0 {
            status = if os_error() == libc::ENOENT && absent_path(path.as_mut_ptr(), length) {
                1
            } else {
                4
            };
        } else {
            let mut metadata: libc::stat = std::mem::zeroed();
            if libc::fstat(fd, &mut metadata) != 0 {
                status = 4;
            } else if directory {
                if metadata.st_mode & libc::S_IFMT != libc::S_IFDIR {
                    status = 2;
                } else {
                    let result = directory_bytes(fd, buffer, limit);
                    status = result.0;
                    size = result.1;
                }
            } else if metadata.st_mode & libc::S_IFMT != libc::S_IFREG {
                status = 2;
            } else {
                loop {
                    let read = libc::read(fd, buffer.add(size).cast(), limit + 1 - size);
                    if read < 0 && os_error() == libc::EINTR {
                        continue;
                    }
                    if read < 0 {
                        status = 4;
                        break;
                    }
                    if read == 0 {
                        break;
                    }
                    size += read as usize;
                    if size > limit {
                        break;
                    }
                }
                if status == 0 && size > limit {
                    status = 3;
                }
            }
            if status == 0 {
                let mut after: libc::stat = std::mem::zeroed();
                if libc::fstat(fd, &mut after) != 0
                    || metadata.st_dev != after.st_dev
                    || metadata.st_ino != after.st_ino
                    || metadata.st_size != after.st_size
                    || metadata.st_mtime != after.st_mtime
                    || metadata.st_mtime_nsec != after.st_mtime_nsec
                {
                    status = 4;
                }
                // 系统字段宽度随平台变化；通道统一传输 64 位身份，不能截断去重证据。
                #[allow(clippy::unnecessary_cast)]
                {
                    device = metadata.st_dev as u64;
                    inode = metadata.st_ino as u64;
                }
            }
            libc::close(fd);
        }
        if status != 0 {
            size = 0;
        }
        let mut code = status.to_be_bytes();
        let mut count = (size as u32).to_be_bytes();
        if !transfer(3, code.as_mut_ptr(), 4, true)
            || !transfer(3, count.as_mut_ptr(), 4, true)
            || (status == 0
                && (!transfer(3, device.to_be_bytes().as_mut_ptr(), 8, true)
                    || !transfer(3, inode.to_be_bytes().as_mut_ptr(), 8, true)))
            || !transfer(3, buffer, size, true)
        {
            libc::_exit(1);
        }
    }
}

/// 业务作用：在只读进程内完成固定大小帧的传输，处理短读写和中断。
/// 参数说明：`fd` 为匿名通道；`bytes` 指向有效缓冲；`length` 为字节数；`write` 指定方向。
/// 返回：完整传输为 true，通道关闭或系统错误为 false；不分配内存或调用日志。
unsafe fn transfer(fd: RawFd, bytes: *mut u8, length: usize, write: bool) -> bool {
    let mut offset = 0;
    while offset < length {
        let count = if write {
            libc::write(fd, bytes.add(offset).cast(), length - offset)
        } else {
            libc::read(fd, bytes.add(offset).cast(), length - offset)
        };
        if count < 0 && os_error() == libc::EINTR {
            continue;
        }
        if count <= 0 {
            return false;
        }
        offset += count as usize;
    }
    true
}

/// 业务作用：读取当前只读线程的系统错误码，避免进入 Rust 错误格式化与分配器。
/// 参数说明：无。
/// 返回：最近失败系统调用的 errno 数值。
unsafe fn os_error() -> i32 {
    #[cfg(target_os = "linux")]
    {
        *libc::__errno_location()
    }
    #[cfg(target_os = "macos")]
    {
        *libc::__error()
    }
}

#[cfg(target_os = "macos")]
unsafe extern "C" {
    /// 业务作用：直接读取 macOS 目录记录，避免 fork 后触发用户态分配器或目录锁。
    /// 参数说明：`fd` 为目录句柄；`buffer/size` 指定可写缓冲；`base` 接收目录位置。
    /// 返回：已写字节数、目录结束时的零或失败时的负值，错误由 errno 提供。
    fn __getdirentries64(
        fd: libc::c_int,
        buffer: *mut libc::c_char,
        size: libc::size_t,
        base: *mut libc::off_t,
    ) -> libc::ssize_t;
}

/// 业务作用：通过内核目录读取接口取得文件名，fork 后不调用带分配器或锁的目录库。
/// 参数说明：`fd` 是已确认的目录；`output` 为预分配缓冲；`limit` 是输出字节上限。
/// 返回：协议状态和 NUL 分隔名称字节数；结构异常或超过条目预算立即拒绝。
/// 安全边界：输出缓冲至少 limit 字节；目录记录只在校验范围内使用未对齐读取。
unsafe fn directory_bytes(fd: RawFd, output: *mut u8, limit: usize) -> (u32, usize) {
    let mut scratch = [0u8; 16384];
    let mut size = 0usize;
    let mut entries = 0usize;
    #[cfg(target_os = "macos")]
    let mut base: libc::off_t = 0;
    loop {
        #[cfg(target_os = "macos")]
        let count = __getdirentries64(fd, scratch.as_mut_ptr().cast(), scratch.len(), &mut base);
        #[cfg(target_os = "linux")]
        let count = libc::syscall(
            libc::SYS_getdents64,
            fd,
            scratch.as_mut_ptr(),
            scratch.len(),
        ) as libc::ssize_t;
        if count < 0 && os_error() == libc::EINTR {
            continue;
        }
        if count < 0 {
            return (4, 0);
        }
        if count == 0 {
            return (0, size);
        }
        if count as usize > scratch.len() {
            return (4, 0);
        }
        let mut offset = 0usize;
        while offset < count as usize {
            #[cfg(target_os = "macos")]
            let name_offset = 21usize;
            #[cfg(target_os = "linux")]
            let name_offset = 19usize;
            if (count as usize) - offset <= name_offset {
                return (4, 0);
            }
            let record = scratch.as_ptr().add(offset);
            let length = std::ptr::read_unaligned(record.add(16).cast::<u16>()) as usize;
            if length <= name_offset || length > count as usize - offset {
                return (4, 0);
            }
            let mut name_length = 0usize;
            while name_offset + name_length < length && *record.add(name_offset + name_length) != 0
            {
                name_length += 1;
            }
            if name_length == 0 || name_offset + name_length >= length {
                return (4, 0);
            }
            let name = record.add(name_offset);
            let dot = name_length == 1 && *name == b'.';
            let parent = name_length == 2 && *name == b'.' && *name.add(1) == b'.';
            if !dot && !parent && std::ptr::read_unaligned(record.cast::<u64>()) != 0 {
                entries += 1;
                if entries > 4096 || name_length + 1 > limit.saturating_sub(size) {
                    return (3, 0);
                }
                std::ptr::copy_nonoverlapping(name, output.add(size), name_length);
                *output.add(size + name_length) = 0;
                size += name_length + 1;
            }
            offset += length;
        }
    }
}

/// 业务作用：区分来源不存在与悬空链接，optional 不能吞掉链接损坏。
/// 参数说明：`path` 是可写且以 NUL 结束的路径缓冲；`length` 不含末尾 NUL。
/// 返回：只在缺失且所有已有链接可解析时为真；仅调用无分配的系统接口。
/// 安全边界：缓冲至少 length + 1 字节，临时分隔符在每轮检查后恢复。
unsafe fn absent_path(path: *mut u8, length: usize) -> bool {
    for index in 1..=length {
        if index != length && *path.add(index) != b'/' {
            continue;
        }
        let saved = *path.add(index);
        *path.add(index) = 0;
        let mut metadata: libc::stat = std::mem::zeroed();
        let result = libc::lstat(path.cast(), &mut metadata);
        let error = if result < 0 { os_error() } else { 0 };
        let broken = result == 0
            && metadata.st_mode & libc::S_IFMT == libc::S_IFLNK
            && libc::stat(path.cast(), &mut metadata) != 0;
        *path.add(index) = saved;
        if broken || (result < 0 && error != libc::ENOENT) {
            return false;
        }
    }
    true
}

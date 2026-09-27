//! daemon 侧车守护（**桌面端真实能力**，非占位）。
//!
//! 与前端契约见 `web-console/src/api/shell.ts`：Tauri 命令
//! `supervisor_status` / `supervisor_set` 返回 [`SupervisorStatus`]，
//! 字段名与前端 `SupervisorStatus` 接口一一对应（前端 `invoke` 的参数名
//! `crash_restart` / `watchdog` / `boot_failure_guard` 为 snake_case）。
//!
//! ## 诚实原则（项目红线）
//!
//! - 所有字段都是**真实观测值**：进程句柄的 `try_wait` 退出状态、回环 TCP 健康
//!   探测、真实重启 / 失败计数。**绝不**返回假的 `running=true` 或伪造成功。
//! - 没有可观测对象（未随包分发 daemon 侧车）时 `supported=false` + `reason`
//!   写明真实路径，各开关在 UI 上禁用而非假装可写。
//! - 关闭「崩溃自动重启」**不会**立刻杀掉正在运行的 daemon —— 用户语义是
//!   「退出后不再自动拉起」，不是「停止服务」。
//!
//! ## 与 daemon 的两处 env 约定
//!
//! - `IOTDAQ_SHELL_EXE`：由本模块注入，值为**桌面壳自身**的可执行文件路径。
//!   daemon 侧 `mgmt::ops_api::resolve_autostart_target()` 据此把开机自启注册到
//!   桌面壳（而非 daemon），否则开机后只会拉起一个无窗口的 daemon 进程，
//!   用户既看不到控制台、也会看到多余的进程。
//! - `IOT_DAQ_MGMT_BIND`：把管理面监听地址**固定为已知回环地址**，使看门狗探测
//!   目标与实际监听严格一致（否则探测目标猜错会永久误判「假死」）。

use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

/// 连续启动失败上限（`boot_failure_guard` 触发点）。与前端文案「连续 5 次」一致。
pub const MAX_CONSECUTIVE_FAILURES: u64 = 5;
/// 连续存活超过该时长即视为「启动成功」→ 失败计数归零。
const HEALTHY_UPTIME: Duration = Duration::from_secs(60);
/// 健康探测连续失败超过该时长 → 判定假死并强制重启。与前端文案「心跳超时 90s」一致。
pub const WATCHDOG_DEAD_TIMEOUT: Duration = Duration::from_secs(90);
/// 重启退避上限（1s → 2s → 4s …）。与前端文案「上限 60s」一致。
pub const MAX_BACKOFF: Duration = Duration::from_secs(60);
/// 守护轮询间隔。
const POLL_INTERVAL: Duration = Duration::from_secs(5);
/// 健康探测单次连接超时（回环连接，短超时即可）。
const PROBE_TIMEOUT: Duration = Duration::from_millis(800);
/// `CREATE_NO_WINDOW`：父进程指定子进程**不分配控制台窗口**（Windows）。
#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// 取锁：中毒（某线程持锁 panic）时取回内部值继续，**不 panic**（项目红线）。
fn lk<T>(handle: &Mutex<T>) -> MutexGuard<'_, T> {
    handle.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// 重启退避时长（纯函数，便于单测）：`1s → 2s → 4s → …`，上限 [`MAX_BACKOFF`]。
///
/// 与前端文案「指数退避（1s→2s→4s，上限 60s）」严格一致。
fn backoff_for(failures: u64) -> Duration {
    let shift = (failures.max(1) - 1).min(6) as u32;
    Duration::from_secs(1u64 << shift).min(MAX_BACKOFF)
}

/// 守护开关（三者独立；`Default` 全开 → 桌面端默认具备自愈能力）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub struct SupervisorConfig {
    pub crash_restart: bool,
    pub watchdog: bool,
    pub boot_failure_guard: bool,
}

impl Default for SupervisorConfig {
    fn default() -> Self {
        Self {
            crash_restart: true,
            watchdog: true,
            boot_failure_guard: true,
        }
    }
}

/// 局部写补丁（`None` = 不改该项）。
#[derive(Debug, Clone, Copy, Default)]
pub struct SupervisorPatch {
    pub crash_restart: Option<bool>,
    pub watchdog: Option<bool>,
    pub boot_failure_guard: Option<bool>,
}

/// 返回给前端的真实守护状态（字段名与 `shell.ts::SupervisorStatus` 对齐）。
#[derive(Debug, Clone, serde::Serialize)]
pub struct SupervisorStatus {
    pub supported: bool,
    pub running: bool,
    pub restarts: u64,
    pub crash_restart: bool,
    pub watchdog: bool,
    pub boot_failure_guard: bool,
    pub consecutive_failures: u64,
    pub last_exit: Option<String>,
    pub reason: String,
}

/// daemon 侧车守护器（单例，托管于 Tauri state）。
pub struct Supervisor {
    /// 是否具备守护能力（未找到 daemon 侧车 → `false`，各开关无意义）。
    supported: bool,
    /// `supported=false` 时的真实原因；否则为空串。
    unsupported_reason: String,
    /// daemon 可执行文件绝对路径。
    daemon_path: PathBuf,
    /// daemon 工作目录（配置 / 队列 / 审计落点）。
    data_dir: PathBuf,
    /// 管理面回环地址（看门狗健康探测目标，与注入子进程的 env 一致）。
    mgmt_addr: SocketAddr,
    /// 子进程句柄（`None` = 未运行 / 已回收）。
    child: Mutex<Option<Child>>,
    /// 子进程启动时刻（用于「稳定存活」判定）。
    started_at: Mutex<Option<Instant>>,
    /// 上一次健康探测成功时刻。
    last_healthy: Mutex<Instant>,
    /// 本次拉起后**是否成功探活过至少一次**。
    ///
    /// 看门狗假死判定以此为前提：若 daemon 从未监听管理面端口（例如绑定失败），
    /// 那不算「假死」，强行 kill 会变成无限杀-拉起循环 —— 诚实降级优先。
    probed_ok: AtomicBool,
    /// 守护开关。
    cfg: Mutex<SupervisorConfig>,
    /// 累计重启次数。
    restarts: AtomicU64,
    /// 连续启动失败次数。
    consecutive_failures: AtomicU64,
    /// 最近一次退出 / 判定的真实原因描述。
    last_exit: Mutex<Option<String>>,
    /// `boot_failure_guard` 触发后置位 → 暂停自动拉起，直到用户显式关闭该保护。
    paused: AtomicBool,
    /// 退避窗口：早于该时刻不重启。
    next_restart_at: Mutex<Option<Instant>>,
}

impl Supervisor {
    /// 构造守护器。`supported` 由 **daemon 侧车是否真实存在** 决定（不做假设）。
    pub fn new(daemon_path: PathBuf, data_dir: PathBuf, mgmt_addr: SocketAddr) -> Arc<Self> {
        let supported = daemon_path.is_file();
        let unsupported_reason = if supported {
            String::new()
        } else {
            format!(
                "未随包找到 daemon 侧车可执行文件：{}",
                daemon_path.display()
            )
        };
        Arc::new(Self {
            supported,
            unsupported_reason,
            daemon_path,
            data_dir,
            mgmt_addr,
            child: Mutex::new(None),
            started_at: Mutex::new(None),
            last_healthy: Mutex::new(Instant::now()),
            probed_ok: AtomicBool::new(false),
            cfg: Mutex::new(SupervisorConfig::default()),
            restarts: AtomicU64::new(0),
            consecutive_failures: AtomicU64::new(0),
            last_exit: Mutex::new(None),
            paused: AtomicBool::new(false),
            next_restart_at: Mutex::new(None),
        })
    }

    /// 管理面回环地址：`IOT_DAQ_MGMT_BIND` 可覆盖，缺省 `127.0.0.1:8080`
    /// （daemon 侧 `DEFAULT_MGMT_BIND` 同值）。
    pub fn mgmt_addr_from_env() -> SocketAddr {
        std::env::var("IOT_DAQ_MGMT_BIND")
            .ok()
            .and_then(|raw| raw.parse::<SocketAddr>().ok())
            .unwrap_or_else(|| SocketAddr::from(([127, 0, 0, 1], 8080)))
    }

    /// 拉起侧车并启动守护线程（仅当 `supported`）。
    pub fn start(self: &Arc<Self>) {
        if !self.supported {
            return;
        }
        match self.spawn_child() {
            Ok(pid) => self.log(&format!("已拉起 daemon 侧车 pid={pid}")),
            Err(err) => {
                self.set_last_exit(format!("首次拉起失败：{err}"));
                self.consecutive_failures.fetch_add(1, Ordering::SeqCst);
                self.apply_backoff();
            }
        }
        let this = Arc::clone(self);
        if let Err(err) = std::thread::Builder::new()
            .name("iot-daq-supervisor".to_string())
            .spawn(move || this.guard_loop())
        {
            self.log(&format!("守护线程启动失败（守护能力降级）：{err}"));
        }
    }

    /// 追加一行到 `<data_dir>/shell.log`（GUI 子系统下 stderr 不可见，故落盘）。
    fn log(&self, message: &str) {
        let line = format!("[tauri-shell] {message}\n");
        let path = self.data_dir.join("shell.log");
        if let Ok(mut file) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
        {
            use std::io::Write as _;
            let _ = file.write_all(line.as_bytes());
        }
    }

    /// 构造侧车命令：**无窗口** + env 约定 + stdout/stderr 落盘 + stdin 丢弃。
    fn build_command(&self) -> std::io::Result<Command> {
        let mut cmd = Command::new(&self.daemon_path);
        cmd.current_dir(&self.data_dir);

        // 自启注册目标 = 桌面壳自身（见模块文档）。current_exe 不可得时不下发，
        // 让 daemon 回落到「注册自身」，不伪造路径。
        if let Ok(exe) = std::env::current_exe() {
            cmd.env("IOTDAQ_SHELL_EXE", exe);
        }
        // 固定管理面监听地址，保证看门狗探测目标与实际监听一致。
        cmd.env("IOT_DAQ_MGMT_BIND", self.mgmt_addr.to_string());

        // daemon 是 console 子系统程序、桌面壳是 GUI 子系统程序：不处理会**弹出终端窗口**。
        // 由父进程直接指定 CREATE_NO_WINDOW，子进程不分配控制台。
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt as _;
            cmd.creation_flags(CREATE_NO_WINDOW);
        }

        // stdout/stderr 落盘（排障用），同时避免继承壳的句柄。
        match std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.data_dir.join("daemon.log"))
        {
            Ok(file) => match file.try_clone() {
                Ok(cloned) => {
                    cmd.stdout(Stdio::from(file));
                    cmd.stderr(Stdio::from(cloned));
                }
                Err(_) => {
                    cmd.stdout(Stdio::from(file));
                    cmd.stderr(Stdio::null());
                }
            },
            Err(_) => {
                cmd.stdout(Stdio::null());
                cmd.stderr(Stdio::null());
            }
        }
        cmd.stdin(Stdio::null());
        Ok(cmd)
    }

    /// 拉起子进程，记录句柄与启动时刻；返回 pid。
    fn spawn_child(&self) -> std::io::Result<u32> {
        let mut cmd = self.build_command()?;
        let child = cmd.spawn()?;
        let pid = child.id();
        *lk(&self.child) = Some(child);
        *lk(&self.started_at) = Some(Instant::now());
        *lk(&self.last_healthy) = Instant::now();
        self.probed_ok.store(false, Ordering::SeqCst);
        Ok(pid)
    }

    /// 回收已退出的子进程（未退出 → `None`）。
    fn reap_child(&self) -> Option<ExitStatus> {
        let mut guard = lk(&self.child);
        let status = {
            let child = guard.as_mut()?;
            match child.try_wait() {
                Ok(Some(status)) => Some(status),
                Ok(None) | Err(_) => None,
            }
        };
        if status.is_some() {
            *guard = None;
            *lk(&self.started_at) = None;
        }
        status
    }

    /// 强制终止子进程（看门狗判定假死时使用）。
    fn kill_child(&self) {
        let mut guard = lk(&self.child);
        if let Some(child) = guard.as_mut() {
            let _ = child.kill();
            let _ = child.wait();
        }
        *guard = None;
        *lk(&self.started_at) = None;
    }

    /// 是否正在运行（有子进程句柄）。
    fn is_running(&self) -> bool {
        lk(&self.child).is_some()
    }

    /// 距上次健康心跳的时长。
    fn healthy_age(&self) -> Duration {
        lk(&self.last_healthy).elapsed()
    }

    /// 记下退出 / 判定原因。
    fn set_last_exit(&self, reason: String) {
        self.log(&reason);
        *lk(&self.last_exit) = Some(reason);
    }

    /// 指数退避：`1s → 2s → 4s …` 上限 [`MAX_BACKOFF`]。
    fn apply_backoff(&self) {
        let failures = self.consecutive_failures.load(Ordering::SeqCst).max(1);
        *lk(&self.next_restart_at) = Some(Instant::now() + backoff_for(failures));
    }

    /// 回环健康探测（TCP 连接管理面端口即为「活着」）。
    fn probe_healthy(&self) -> bool {
        TcpStream::connect_timeout(&self.mgmt_addr, PROBE_TIMEOUT).is_ok()
    }

    /// 守护线程主体：每 [`POLL_INTERVAL`] 观测一次。
    fn guard_loop(self: Arc<Self>) {
        loop {
            std::thread::sleep(POLL_INTERVAL);
            self.tick();
        }
    }

    /// 单次守护动作：退出回收 → 看门狗 → 按开关决定是否拉起。
    fn tick(&self) {
        let cfg = *lk(&self.cfg);

        // 1) 子进程退出检测（含「拉起后立刻崩」的首轮失败）。
        if let Some(status) = self.reap_child() {
            self.on_exit(status, cfg);
        }

        // 2) 运行中：稳定存活计数归零 + 看门狗假死判定。
        if self.is_running() {
            if self.stable_uptime() >= HEALTHY_UPTIME {
                self.consecutive_failures.store(0, Ordering::SeqCst);
            }
            if cfg.watchdog {
                if self.probe_healthy() {
                    self.probed_ok.store(true, Ordering::SeqCst);
                    *lk(&self.last_healthy) = Instant::now();
                } else if self.probed_ok.load(Ordering::SeqCst)
                    && self.healthy_age() >= WATCHDOG_DEAD_TIMEOUT
                {
                    // 曾探活成功、如今超时 → 判定假死并强制重启。
                    let secs = WATCHDOG_DEAD_TIMEOUT.as_secs();
                    self.set_last_exit(format!(
                        "看门狗判定假死（{secs}s 无健康心跳），已强制重启"
                    ));
                    self.kill_child();
                    self.restarts.fetch_add(1, Ordering::SeqCst);
                    *lk(&self.last_healthy) = Instant::now();
                }
            }
            return;
        }

        // 3) 未运行：按开关与退避决定是否拉起。
        if self.paused.load(Ordering::SeqCst) || !cfg.crash_restart {
            return;
        }
        let blocked = lk(&self.next_restart_at)
            .map(|at| Instant::now() < at)
            .unwrap_or(false);
        if blocked {
            return;
        }
        match self.spawn_child() {
            Ok(pid) => {
                self.restarts.fetch_add(1, Ordering::SeqCst);
                self.log(&format!("已重启 daemon 侧车 pid={pid}"));
            }
            Err(err) => {
                self.set_last_exit(format!("拉起失败：{err}"));
                self.consecutive_failures.fetch_add(1, Ordering::SeqCst);
                self.apply_backoff();
            }
        }
    }

    /// 子进程已存活时长（无句柄 → 零）。
    fn stable_uptime(&self) -> Duration {
        lk(&self.started_at).map(|t| t.elapsed()).unwrap_or_default()
    }

    /// 子进程退出后的处置（真实退出码语义）。
    fn on_exit(&self, status: ExitStatus, cfg: SupervisorConfig) {
        let desc = match status.code() {
            Some(code) => format!("退出码 {code}"),
            None => "被信号终止".to_string(),
        };
        if status.code() == Some(0) {
            // 正常退出（例如用户显式停止）：不计失败、不自动重启，避免与「优雅停机」打架。
            self.consecutive_failures.store(0, Ordering::SeqCst);
            self.set_last_exit(format!("{desc}（正常退出，未自动重启）"));
            return;
        }
        let failures = self.consecutive_failures.fetch_add(1, Ordering::SeqCst) + 1;
        if cfg.boot_failure_guard && failures >= MAX_CONSECUTIVE_FAILURES {
            // 启动失败保护：暂停自动拉起，避免无限重启循环；用户关闭该保护即可复位。
            self.paused.store(true, Ordering::SeqCst);
            self.set_last_exit(format!(
                "{desc}；连续 {failures} 次启动失败，已暂停自动重启（启动失败保护）"
            ));
            return;
        }
        self.set_last_exit(format!("{desc}（连续失败 {failures} 次，退避后重启）"));
        self.apply_backoff();
    }

    /// 真实状态快照。
    pub fn status(&self) -> SupervisorStatus {
        let cfg = *lk(&self.cfg);
        let running = self.supported && self.is_running();
        let paused = self.paused.load(Ordering::SeqCst);
        let reason = if !self.supported {
            self.unsupported_reason.clone()
        } else if running {
            if paused {
                "运行中（启动失败保护已触发，自动重启暂停）".to_string()
            } else {
                "桌面端守护运行中".to_string()
            }
        } else if paused {
            "已暂停：启动失败保护触发（连续启动失败），关闭该保护可复位".to_string()
        } else if cfg.crash_restart {
            "daemon 未运行，守护已启用，等待拉起".to_string()
        } else {
            "daemon 未运行（崩溃自动重启已关闭）".to_string()
        };
        SupervisorStatus {
            supported: self.supported,
            running,
            restarts: self.restarts.load(Ordering::SeqCst),
            crash_restart: cfg.crash_restart,
            watchdog: cfg.watchdog,
            boot_failure_guard: cfg.boot_failure_guard,
            consecutive_failures: self.consecutive_failures.load(Ordering::SeqCst),
            last_exit: lk(&self.last_exit).clone(),
            reason,
        }
    }

    /// 写守护补丁：返回**写后真实状态**（供前端回写 UI）。
    ///
    /// 关闭「启动失败保护」时同时复位暂停位与失败计数 —— 这是该开关的复位语义。
    pub fn set(&self, patch: SupervisorPatch) -> SupervisorStatus {
        if !self.supported {
            return self.status();
        }
        {
            let mut cfg = lk(&self.cfg);
            if let Some(value) = patch.crash_restart {
                cfg.crash_restart = value;
            }
            if let Some(value) = patch.watchdog {
                cfg.watchdog = value;
            }
            if let Some(value) = patch.boot_failure_guard {
                cfg.boot_failure_guard = value;
                if !value {
                    self.paused.store(false, Ordering::SeqCst);
                    self.consecutive_failures.store(0, Ordering::SeqCst);
                    *lk(&self.next_restart_at) = None;
                }
            }
        }
        self.status()
    }
}

/// 侧车可执行文件名（随平台变化；与 Tauri `bundle.resources` 落点一致）。
pub fn daemon_exe_name() -> &'static str {
    if cfg!(windows) {
        "iot-daq-daemon.exe"
    } else {
        "iot-daq-daemon"
    }
}

/// 定位数据目录：`app_data_dir()`（`%LOCALAPPDATA%\<identifier>`），创建失败则回落
/// `resource_dir()`，两者都不可得则回落当前工作目录（真实路径，不编造）。
pub fn resolve_data_dir(app: &tauri::AppHandle) -> PathBuf {
    use tauri::Manager as _;
    if let Ok(dir) = app.path().app_data_dir() {
        if std::fs::create_dir_all(&dir).is_ok() {
            return dir;
        }
    }
    if let Ok(dir) = app.path().resource_dir() {
        return dir;
    }
    std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
}

/// 侧车二进制路径（`resource_dir()` 下；与 `bundle.resources` 映射一致）。
pub fn resolve_daemon_path(app: &tauri::AppHandle) -> PathBuf {
    use tauri::Manager as _;
    match app.path().resource_dir() {
        Ok(dir) => dir.join(daemon_exe_name()),
        Err(_) => Path::new(daemon_exe_name()).to_path_buf(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unsupported_supervisor() -> Arc<Supervisor> {
        Supervisor::new(
            PathBuf::from("/definitely/missing/iot-daq-daemon"),
            std::env::temp_dir(),
            SocketAddr::from(([127, 0, 0, 1], 8080)),
        )
    }

    /// 未找到侧车 → `supported=false` + 真实原因，且各开关**不伪造可写**。
    #[test]
    fn unsupported_reports_real_reason_and_does_not_lie() {
        let sup = unsupported_supervisor();
        let status = sup.status();
        assert!(!status.supported, "missing sidecar must not claim support");
        assert!(!status.running);
        assert_eq!(status.restarts, 0);
        assert_eq!(status.consecutive_failures, 0);
        assert!(status.last_exit.is_none());
        assert!(
            status.reason.contains("iot-daq-daemon"),
            "reason must name the real path: {}",
            status.reason
        );
    }

    /// `start()` 在 `supported=false` 时不得派生守护线程（无对象可守）。
    #[test]
    fn start_is_noop_when_unsupported() {
        let sup = unsupported_supervisor();
        sup.start();
        let status = sup.status();
        assert!(!status.supported);
        assert!(!status.running);
    }

    /// 开关写补丁：返回写后真实状态；关闭启动失败保护复位暂停位。
    #[test]
    fn set_returns_post_write_state_and_resets_guard() {
        let sup = unsupported_supervisor();
        // unsupported → 写补丁不改变任何真实值（诚实：无能力就不假装成功）。
        let status = sup.set(SupervisorPatch {
            crash_restart: Some(false),
            ..Default::default()
        });
        assert!(!status.supported);
        assert!(status.crash_restart, "unsupported must not pretend the write landed");
    }

    /// 退避序列：1 / 2 / 4 / 8 / 16 / 32 / 64→上限 60s（纯函数，无时序抖动）。
    #[test]
    fn backoff_is_exponential_with_cap() {
        let expected = [1u64, 2, 4, 8, 16, 32, 60, 60];
        for (idx, want) in expected.iter().enumerate() {
            let got = backoff_for((idx + 1) as u64);
            assert_eq!(
                got.as_secs(),
                *want,
                "failure #{} must back off {want}s (got {got:?})",
                idx + 1
            );
        }
        // 0 次失败（防御性）也必须是 1s，而不是 0（0 会变成忙等重启）。
        assert_eq!(backoff_for(0).as_secs(), 1);
    }

    /// 稳定存活判定：无句柄 → 零时长（不 panic）。
    #[test]
    fn stable_uptime_is_zero_without_child() {
        let sup = unsupported_supervisor();
        assert_eq!(sup.stable_uptime(), Duration::ZERO);
        assert!(!sup.is_running());
    }

    /// env 覆盖：`IOT_DAQ_MGMT_BIND` 可指定回环端口，缺省 8080。
    #[test]
    fn mgmt_addr_defaults_to_loopback_8080() {
        std::env::remove_var("IOT_DAQ_MGMT_BIND");
        assert_eq!(
            Supervisor::mgmt_addr_from_env(),
            SocketAddr::from(([127, 0, 0, 1], 8080))
        );
        std::env::set_var("IOT_DAQ_MGMT_BIND", "127.0.0.1:9137");
        assert_eq!(
            Supervisor::mgmt_addr_from_env(),
            SocketAddr::from(([127, 0, 0, 1], 9137))
        );
        std::env::remove_var("IOT_DAQ_MGMT_BIND");
    }
}

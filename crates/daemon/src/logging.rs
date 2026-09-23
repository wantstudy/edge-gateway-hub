//! 日志与可观测性（计划 task 5）：tracing + tracing-subscriber。
//!
//! 能力：
//! - 分级日志：`EnvFilter` 语法（`"warn"`、`"info,daemon=debug"`），级别来自配置；
//! - JSON 结构化可选开关（`json: true` 时输出机器可解析 JSON；默认人读文本）；
//! - 文件轮转：`tracing-appender` 按天轮转（大小轮转与归档清理在 Wave 7 的运维面
//!   补齐；1s 节流日志示例按里程碑口径不做）；
//! - 远程日志推送地址：本阶段仅承载配置字段（`remote_endpoint`），推送通道在
//!   task 36（远程运维）接入。
//!
//! 红线：日志不输出密钥 / 激活码 / 完整硬件标识（指纹模块 key 的 Debug 已脱敏）。

use std::path::PathBuf;

use tracing_subscriber::prelude::*;
use tracing_subscriber::{fmt, EnvFilter};

/// 非阻塞文件写入的 flush 句柄：进程退出前必须持有，drop 时 flush。
pub use tracing_appender::non_blocking::WorkerGuard;

/// 日志初始化配置。
#[derive(Debug, Clone)]
pub struct LoggingConfig {
    /// 最低输出级别（EnvFilter 语法，如 `"info"`、`"warn,daemon=debug"`）。
    pub level: String,
    /// JSON 结构化输出开关（默认 false = 文本格式）。
    pub json: bool,
    /// 日志文件目录（按天轮转）；`None` = 仅 stdout。
    pub log_dir: Option<PathBuf>,
    /// 远程日志推送地址（预留字段，task 36 接入）。
    pub remote_endpoint: Option<String>,
}

impl Default for LoggingConfig {
    fn default() -> Self {
        Self {
            level: "info".to_string(),
            json: false,
            log_dir: None,
            remote_endpoint: None,
        }
    }
}

/// 日志初始化错误（错误路径不 panic）。
#[derive(Debug, thiserror::Error)]
pub enum LogInitError {
    /// 级别过滤器语法非法。
    #[error("invalid log level filter `{0}`")]
    InvalidFilter(String),
    /// 全局 subscriber 已初始化（重复 init）。
    #[error("tracing global subscriber already initialized")]
    AlreadyInit,
    /// 文件系统 IO 失败（创建日志目录 / 打开文件）。
    #[error("logging io failure: {0}")]
    Io(#[from] std::io::Error),
    /// subscriber 安装失败（fmt 层冲突等内部问题）。
    #[error("subscriber install failed: {0}")]
    Install(String),
}

/// 初始化全局 tracing subscriber。
///
/// `log_dir` 提供时同时挂 stdout 与按天轮转的文件输出（非阻塞 writer），
/// 返回的 [`WorkerGuard`] 必须由调用方持有到进程退出（drop 时 flush）。
///
/// # Errors
/// 过滤器非法 / 目录创建失败 / 重复初始化返回 [`LogInitError`]。
pub fn init(config: &LoggingConfig) -> Result<Option<WorkerGuard>, LogInitError> {
    let filter = EnvFilter::try_new(&config.level)
        .map_err(|_| LogInitError::InvalidFilter(config.level.clone()))?;

    // 四条初始化路径（file×json 组合）各自独立构造 subscriber——
    // fmt 层的 json/text 是不同具体类型，禁止跨分支共享层变量。
    match &config.log_dir {
        Some(dir) => {
            std::fs::create_dir_all(dir)?;
            let file_appender = tracing_appender::rolling::daily(dir, "iot-daq.log");
            let (file_writer, guard) = tracing_appender::non_blocking(file_appender);
            match config.json {
                true => {
                    tracing_subscriber::registry()
                        .with(filter)
                        .with(
                            fmt::layer()
                                .json()
                                .with_writer(file_writer)
                                .with_ansi(false),
                        )
                        .with(fmt::layer().json().with_writer(std::io::stdout))
                        .try_init()
                        .map_err(|e| LogInitError::Install(e.to_string()))?;
                }
                false => {
                    tracing_subscriber::registry()
                        .with(filter)
                        .with(fmt::layer().with_writer(file_writer).with_ansi(false))
                        .with(fmt::layer().with_writer(std::io::stdout))
                        .try_init()
                        .map_err(|e| LogInitError::Install(e.to_string()))?;
                }
            }
            Ok(Some(guard))
        }
        None => {
            match config.json {
                true => {
                    tracing_subscriber::registry()
                        .with(filter)
                        .with(fmt::layer().json().with_writer(std::io::stdout))
                        .try_init()
                        .map_err(|e| LogInitError::Install(e.to_string()))?;
                }
                false => {
                    tracing_subscriber::registry()
                        .with(filter)
                        .with(fmt::layer().with_writer(std::io::stdout))
                        .try_init()
                        .map_err(|e| LogInitError::Install(e.to_string()))?;
                }
            }
            Ok(None)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    /// 捕获型 writer：把输出攒进共享缓冲，供断言。
    #[derive(Clone)]
    struct Capture(Arc<Mutex<Vec<u8>>>);

    impl Capture {
        fn new() -> (Self, Arc<Mutex<Vec<u8>>>) {
            let buf = Arc::new(Mutex::new(Vec::new()));
            (Self(buf.clone()), buf)
        }

        fn drain(buf: &Arc<Mutex<Vec<u8>>>) -> String {
            let mut guard = buf.lock().unwrap_or_else(|p| p.into_inner());
            let text = String::from_utf8_lossy(&guard).to_string();
            guard.clear();
            text
        }
    }

    impl Write for Capture {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// QA: 级别 WARN 时只有 warn/error 输出，debug/info 被过滤。
    #[test]
    fn level_filter_warn_suppresses_debug_and_info() {
        let (writer, buf) = Capture::new();
        let filter = EnvFilter::try_new("warn").expect("filter");
        let layer = fmt::layer()
            .with_writer(move || writer.clone())
            .with_ansi(false);
        let subscriber = tracing_subscriber::registry().with(filter).with(layer);

        tracing::subscriber::with_default(subscriber, || {
            tracing::debug!("debug-should-not-appear");
            tracing::info!("info-should-not-appear");
            tracing::warn!("warn-should-appear");
            tracing::error!("error-should-appear");
        });

        let out = Capture::drain(&buf);
        assert!(!out.contains("debug-should-not-appear"));
        assert!(!out.contains("info-should-not-appear"));
        assert!(out.contains("warn-should-appear"), "out: {out}");
        assert!(out.contains("error-should-appear"), "out: {out}");
    }

    /// JSON 结构化开关：输出为 JSON 行（level 字段可检索）。
    #[test]
    fn json_format_outputs_json_lines() {
        let (writer, buf) = Capture::new();
        let filter = EnvFilter::try_new("info").expect("filter");
        let layer = fmt::layer()
            .json()
            .with_writer(move || writer.clone())
            .with_ansi(false);
        let subscriber = tracing_subscriber::registry().with(filter).with(layer);

        tracing::subscriber::with_default(subscriber, || {
            tracing::warn!(outlet = "north-1", "json probe message");
        });

        let out = Capture::drain(&buf);
        assert!(
            out.contains("\"level\":\"WARN\""),
            "json line must carry level field: {out}"
        );
        assert!(
            out.contains("json probe message"),
            "json line must carry message: {out}"
        );
        assert!(
            out.contains("north-1"),
            "structured field must appear: {out}"
        );
    }

    /// EnvFilter 支持模块级覆盖（生产口径：全局 info + daemon debug）。
    #[test]
    fn env_filter_supports_per_module_override() {
        let (writer, buf) = Capture::new();
        let filter = EnvFilter::try_new("info,daemon=debug").expect("filter");
        let layer = fmt::layer()
            .with_writer(move || writer.clone())
            .with_ansi(false);
        let subscriber = tracing_subscriber::registry().with(filter).with(layer);

        tracing::subscriber::with_default(subscriber, || {
            tracing::debug!(target: "daemon::config", "daemon debug visible");
            tracing::debug!(target: "other::crate", "other debug hidden");
        });

        let out = Capture::drain(&buf);
        assert!(out.contains("daemon debug visible"), "out: {out}");
        assert!(!out.contains("other debug hidden"), "out: {out}");
    }

    /// 非法过滤器在构造期报错（错误路径不 panic），合法过滤器可构造。
    #[test]
    fn invalid_filter_is_rejected() {
        assert!(
            EnvFilter::try_new("daemon=nonsense").is_err(),
            "invalid level must be rejected"
        );
        assert!(EnvFilter::try_new("warn").is_ok());
        assert!(EnvFilter::try_new("info,daemon=debug").is_ok());
        assert!(
            EnvFilter::try_new("").is_ok(),
            "empty filter is legal (no directives)"
        );
    }

    /// QA: 文件按天轮转可用——事件经非阻塞 writer 落盘到日期命名文件。
    #[test]
    fn rolling_daily_file_receives_events() {
        let dir = tempfile::tempdir().expect("tempdir");
        let appender = tracing_appender::rolling::daily(dir.path(), "iot-daq.log");
        let (non_blocking, guard) = tracing_appender::non_blocking(appender);

        let filter = EnvFilter::try_new("info").expect("filter");
        let layer = fmt::layer().with_writer(non_blocking).with_ansi(false);
        let subscriber = tracing_subscriber::registry().with(filter).with(layer);

        tracing::subscriber::with_default(subscriber, || {
            tracing::info!("rolling-file probe message");
        });
        drop(guard); // 触发 flush

        // 轮询等非阻塞 worker 落盘（最多 2s）；break 即代表消息已落盘。
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            let mut found = String::new();
            for entry in std::fs::read_dir(dir.path()).expect("read_dir") {
                let entry = entry.expect("entry");
                let text = std::fs::read_to_string(entry.path()).unwrap_or_default();
                found.push_str(&text);
            }
            if found.contains("rolling-file probe message") {
                break;
            }
            assert!(Instant::now() < deadline, "file never flushed: {found:?}");
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    /// LoggingConfig 默认值符合口径。
    #[test]
    fn default_config_matches_convention() {
        let config = LoggingConfig::default();
        assert_eq!(config.level, "info");
        assert!(!config.json);
        assert!(config.log_dir.is_none());
        assert!(config.remote_endpoint.is_none());
        // Debug 输出不 panic（格式化路径自检）。
        let rendered = format!("{config:?}");
        assert!(rendered.contains("LoggingConfig"));
    }

    /// PathBuf 字段类型自检（log_dir 可指向具体目录）。
    #[test]
    fn config_accepts_log_dir() {
        let config = LoggingConfig {
            log_dir: Some(PathBuf::from("/var/log/iot-daq")),
            ..LoggingConfig::default()
        };
        assert_eq!(
            config.log_dir.as_deref(),
            Some(std::path::Path::new("/var/log/iot-daq"))
        );
    }
}

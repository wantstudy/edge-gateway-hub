//! `licensing-server` 可执行入口（task-61 验收 D-05 修复：产品缺口——此前只有
//! lib 无 bin 目标，云端授权服务无法启动）。
//!
//! 职责边界（**薄壳，只做装配，零业务逻辑**）：
//! 1. 从环境变量解析监听地址 / SQLite 数据库路径 / 日志级别；
//! 2. 初始化全局 tracing subscriber（与 daemon 同款 env 约定）；
//! 3. 打开 [`licensing_server::store::Store`]、装配 [`licensing_server::keys::KeyRing`]
//!    （部署契约：签名私钥**只经环境变量注入**，绝不落盘 / 进日志）并构造
//!    [`licensing_server::service::LicensingService`]；
//! 4. 用 lib 的现成路由 [`licensing_server::http::router`] 起 axum 服务。
//!
//! 环境变量：
//! - `IOT_DAQ_LISTEN_ADDR`：监听地址（默认 `0.0.0.0:7080`，licensing-api.md 口径）；
//! - `IOT_DAQ_LICENSE_DB`：SQLite 数据库路径（默认 `./licensing.db`）；
//! - `IOT_DAQ_LOG_LEVEL`：最低日志级别（默认 `info`，EnvFilter 语法）；
//! - `IOT_DAQ_LICENSE_SIGNING_KID` + `IOT_DAQ_LICENSE_SIGNING_KEY`：Lease Token
//!   签名私钥注入（**成对出现才注册**；b64 Ed25519 32/64 字节；缺失时签发端点
//!   fail-closed，验签路径不受影响——不猜测意图、不生成临时密钥）。
//!
//! 退出码：`0` = 正常退出；`1` = 启动失败（数据库打开 / 端口绑定失败）。

use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use licensing_server::http;
use licensing_server::keys::KeyRing;
use licensing_server::service::LicensingService;
use licensing_server::store::Store;

/// 监听地址环境变量。
pub const LISTEN_ADDR_ENV: &str = "IOT_DAQ_LISTEN_ADDR";
/// SQLite 数据库路径环境变量。
pub const LICENSE_DB_ENV: &str = "IOT_DAQ_LICENSE_DB";
/// 最低日志级别环境变量（与 daemon 同款约定）。
pub const LOG_LEVEL_ENV: &str = "IOT_DAQ_LOG_LEVEL";
/// Lease Token 签名私钥（b64）环境变量。
pub const SIGNING_KEY_ENV: &str = "IOT_DAQ_LICENSE_SIGNING_KEY";
/// Lease Token 签名私钥的 kid 环境变量（与 [`SIGNING_KEY_ENV`] 成对）。
pub const SIGNING_KID_ENV: &str = "IOT_DAQ_LICENSE_SIGNING_KID";

/// 默认监听地址（licensing-api.md 口径端口 7080）。
const DEFAULT_LISTEN_ADDR: &str = "0.0.0.0:7080";
/// 默认 SQLite 数据库路径。
const DEFAULT_DB_PATH: &str = "./licensing.db";

/// 环境值解析：trim 后非空才采用，否则回落默认（空白 = 未配置）。
fn env_or(value: Option<String>, default: &str) -> String {
    value
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| default.to_string())
}

/// 初始化全局 tracing subscriber（级别缺省 `info`；重复初始化静默忽略）。
fn init_logging(level: Option<String>) {
    let level = env_or(level, "info");
    let filter = tracing_subscriber::EnvFilter::try_new(level)
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    let _ = tracing_subscriber::fmt().with_env_filter(filter).try_init();
}

/// 当前 Unix 秒（时钟早于纪元按 0 处理，仅作 kid 时间戳前缀）。
fn now_unix_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(0))
}

/// 签名私钥注入（部署契约：只经环境变量；kid 与 key **成对出现**才注册）。
///
/// - 注册失败只记 `warn` 不阻断启动（fail-safe：`/verify` 等验签路径不受影响，
///   签发端点按业务规则 fail-closed）；
/// - 绝不生成临时密钥（跨重启不可复现的 kid 会让已签发 Lease 全部失验）；
/// - key 值**绝不进日志**。
fn register_signing_key_from_env(keyring: &KeyRing) {
    let kid = std::env::var(SIGNING_KID_ENV)
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty());
    let key = std::env::var(SIGNING_KEY_ENV)
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty());
    match (kid, key) {
        (Some(kid), Some(key)) => {
            if let Err(err) = keyring.register_from_b64(&kid, &key, None, now_unix_secs()) {
                tracing::warn!(
                    error = %err,
                    "licensing-server: signing key registration failed; issuance endpoints \
                     will fail closed until a valid key is provided"
                );
            }
        }
        (Some(_), None) | (None, Some(_)) => {
            tracing::warn!(
                "licensing-server: {SIGNING_KID_ENV} and {SIGNING_KEY_ENV} must be provided \
                 together; signing key not registered"
            );
        }
        (None, None) => {
            tracing::warn!(
                "licensing-server: no signing key registered ({SIGNING_KID_ENV} / \
                 {SIGNING_KEY_ENV}); issuance endpoints will fail closed"
            );
        }
    }
}

/// 装配并运行服务（错误收敛为可读消息，由 main 映射退出码）。
///
/// # Errors
/// 数据库打开失败 / 端口绑定失败 / serve 异常时返回 `Err(String)`。
async fn run(listen: String, db_path: PathBuf) -> Result<(), String> {
    let store =
        Store::open(&db_path).map_err(|e| format!("open license db {}: {e}", db_path.display()))?;
    let keyring = KeyRing::empty();
    register_signing_key_from_env(&keyring);
    let service = Arc::new(LicensingService::new(store, keyring));

    let listener = tokio::net::TcpListener::bind(&listen)
        .await
        .map_err(|e| format!("bind {listen}: {e}"))?;
    tracing::info!(
        listen = %listen,
        db = %db_path.display(),
        "licensing-server listening"
    );
    axum::serve(listener, http::router(service))
        .await
        .map_err(|e| format!("serve: {e}"))
}

fn main() -> ExitCode {
    let listen = env_or(std::env::var(LISTEN_ADDR_ENV).ok(), DEFAULT_LISTEN_ADDR);
    let db_path = PathBuf::from(env_or(std::env::var(LICENSE_DB_ENV).ok(), DEFAULT_DB_PATH));
    init_logging(std::env::var(LOG_LEVEL_ENV).ok());

    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(err) => {
            eprintln!("[licensing-server] tokio runtime build failed: {err}");
            return ExitCode::FAILURE;
        }
    };
    match runtime.block_on(run(listen, db_path)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("[licensing-server] {err}");
            ExitCode::FAILURE
        }
    }
}

// ---------------------------------------------------------------------------
// 测试（env 解析纯函数回归；路由 / 业务在 lib 与集成测试覆盖）
// ---------------------------------------------------------------------------
#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    /// env_or：trim 非空才采用；空白 / 缺失回落默认。
    #[test]
    fn env_or_trims_and_falls_back_to_default() {
        assert_eq!(env_or(None, "0.0.0.0:7080"), "0.0.0.0:7080");
        assert_eq!(env_or(Some(String::new()), "0.0.0.0:7080"), "0.0.0.0:7080");
        assert_eq!(
            env_or(Some("   ".to_string()), "0.0.0.0:7080"),
            "0.0.0.0:7080"
        );
        assert_eq!(
            env_or(Some(" 0.0.0.0:9000 \n".to_string()), "0.0.0.0:7080"),
            "0.0.0.0:9000"
        );
    }

    /// 缺省常量与文档口径一致（监听 7080 / 库文件名）。
    #[test]
    fn defaults_match_deployment_contract() {
        assert_eq!(DEFAULT_LISTEN_ADDR, "0.0.0.0:7080");
        assert_eq!(DEFAULT_DB_PATH, "./licensing.db");
        assert_eq!(LISTEN_ADDR_ENV, "IOT_DAQ_LISTEN_ADDR");
        assert_eq!(LICENSE_DB_ENV, "IOT_DAQ_LICENSE_DB");
    }

    /// now_unix_secs 不 panic 且为非负（时钟异常回落 0）。
    #[test]
    fn now_unix_secs_is_non_negative() {
        assert!(now_unix_secs() >= 0);
    }

    /// db 路径经 env_or 后可被 `Store::open` 的 `&Path` 签名消费（装配链类型自检）。
    #[test]
    fn db_path_consumable_by_store_signature() {
        let raw = env_or(None, DEFAULT_DB_PATH);
        let path = PathBuf::from(raw);
        let as_ref: &Path = path.as_path();
        assert_eq!(as_ref, Path::new("./licensing.db"));
    }
}

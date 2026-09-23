//! 统一错误类型、错误码与统一 Result（计划 task 6）。
//!
//! 约定：
//! - `DaemonError` 是 daemon 侧唯一主错误枚举，后续模块（驱动 / 缓存 / MQTT / 授权 /
//!   存储 / 网络 / 安全）的错误全部经 `From` 转换收敛进来；
//! - 错误码为 u16 常量，按域分段（1xxx 协议、2xxx 配置、3xxx 授权……），
//!   用于日志、北向诊断与界面展示（见 `error_code()`）；
//! - Display 以变体名开头（如 `ProtocolError: ...`），保证日志可检索。

use std::io;

use crate::auth::machine_id::FingerprintError;

// ---- 错误码常量（按域分段；新增域时续接分段，勿插队） ----

/// 南向协议驱动域。
pub const ERR_PROTOCOL: u16 = 1000;
/// 配置域（解析 / 热重载 / 监听）。
pub const ERR_CONFIG: u16 = 2000;
/// 授权域（通用）。
pub const ERR_AUTH: u16 = 3000;
/// 机器码指纹域（授权子域）。
pub const ERR_MACHINE_FINGERPRINT: u16 = 3100;
/// 本地存储域（缓存 / SQLite / 文件系统）。
pub const ERR_STORAGE: u16 = 4000;
/// 北向 MQTT 域。
pub const ERR_MQTT: u16 = 5000;
/// 网络域（南向 / 北向连接通用网络错误）。
pub const ERR_NETWORK: u16 = 6000;
/// 安全域（TLS / 鉴权 / 防篡改）。
pub const ERR_SECURITY: u16 = 7000;

/// daemon 主错误枚举（计划 task 6 指定七域 + 指纹子域）。
#[derive(Debug, thiserror::Error)]
pub enum DaemonError {
    /// 南向协议驱动错误（Modbus/OPC UA/S7/MC/HTTP/MQTT 接入）。
    #[error("ProtocolError: {0}")]
    ProtocolError(String),

    /// 配置错误（解析失败 / 热重载失败 / 监听器故障）。
    #[error("ConfigError: {0}")]
    ConfigError(String),

    /// 授权错误（租约 / 心跳 / 激活状态）。
    #[error("AuthError: {0}")]
    AuthError(String),

    /// 本地存储错误（缓存 / SQLite / 本地文件系统）。
    #[error("StorageError: {0}")]
    StorageError(String),

    /// 北向 MQTT 错误（连接 / 发布 / QoS）。
    #[error("MqttError: {0}")]
    MqttError(String),

    /// 网络错误（连接失败 / 超时，协议语义之外的传输层问题）。
    #[error("NetworkError: {0}")]
    NetworkError(String),

    /// 安全错误（TLS / 证书 / 防篡改自检）。
    #[error("SecurityError: {0}")]
    SecurityError(String),

    /// 机器码指纹失败（[`FingerprintError`] 经 `From` 并入，保留原始结构）。
    #[error("AuthError: machine fingerprint failed: {0}")]
    MachineFingerprint(#[from] FingerprintError),
}

impl DaemonError {
    /// 错误码（u16，非零；用于日志 / 诊断 / 界面展示）。
    pub fn error_code(&self) -> u16 {
        match self {
            DaemonError::ProtocolError(_) => ERR_PROTOCOL,
            DaemonError::ConfigError(_) => ERR_CONFIG,
            DaemonError::AuthError(_) => ERR_AUTH,
            DaemonError::StorageError(_) => ERR_STORAGE,
            DaemonError::MqttError(_) => ERR_MQTT,
            DaemonError::NetworkError(_) => ERR_NETWORK,
            DaemonError::SecurityError(_) => ERR_SECURITY,
            DaemonError::MachineFingerprint(_) => ERR_MACHINE_FINGERPRINT,
        }
    }
}

/// daemon 统一 Result 别名。
pub type DaemonResult<T> = Result<T, DaemonError>;

impl From<io::Error> for DaemonError {
    fn from(err: io::Error) -> Self {
        DaemonError::StorageError(format!("io: {err}"))
    }
}

impl From<toml::de::Error> for DaemonError {
    fn from(err: toml::de::Error) -> Self {
        DaemonError::ConfigError(format!("toml parse: {err}"))
    }
}

impl From<notify::Error> for DaemonError {
    fn from(err: notify::Error) -> Self {
        DaemonError::ConfigError(format!("fs watch: {err}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::machine_id::FingerprintError;

    /// QA: 每个变体可构造、Display 以变体名开头、错误码非零且互不冲突。
    #[test]
    fn variants_construct_display_and_codes() {
        let cases: Vec<(DaemonError, &'static str, u16)> = vec![
            (
                DaemonError::ProtocolError("modbus timeout".into()),
                "ProtocolError",
                ERR_PROTOCOL,
            ),
            (
                DaemonError::ConfigError("bad toml".into()),
                "ConfigError",
                ERR_CONFIG,
            ),
            (
                DaemonError::AuthError("lease expired".into()),
                "AuthError",
                ERR_AUTH,
            ),
            (
                DaemonError::StorageError("db locked".into()),
                "StorageError",
                ERR_STORAGE,
            ),
            (
                DaemonError::MqttError("publish refused".into()),
                "MqttError",
                ERR_MQTT,
            ),
            (
                DaemonError::NetworkError("dial timeout".into()),
                "NetworkError",
                ERR_NETWORK,
            ),
            (
                DaemonError::SecurityError("tls handshake".into()),
                "SecurityError",
                ERR_SECURITY,
            ),
        ];
        let mut seen = std::collections::HashSet::new();
        for (err, name, code) in cases {
            let rendered = err.to_string();
            assert!(
                rendered.starts_with(name),
                "display must start with variant name: {rendered}"
            );
            assert_ne!(code, 0, "code must be non-zero for {name}");
            assert!(seen.insert(code), "duplicate error code {code}");
        }
    }

    /// QA: ProtocolError Display 含变体名与原始消息（计划 QA 场景原文）。
    #[test]
    fn protocol_error_display_contains_message() {
        let err = DaemonError::ProtocolError("modbus timeout".to_string());
        let rendered = err.to_string();
        assert!(rendered.contains("ProtocolError"), "display: {rendered}");
        assert!(rendered.contains("modbus timeout"), "display: {rendered}");
        assert_eq!(err.error_code(), ERR_PROTOCOL);
        assert_ne!(err.error_code(), 0);
    }

    /// `FingerprintError` 经 `From` 并入且保留错误码域。
    #[test]
    fn fingerprint_error_converts_into_daemon_error() {
        let fp = FingerprintError::EmptyKey;
        let err: DaemonError = fp.into();
        assert!(matches!(err, DaemonError::MachineFingerprint(_)));
        assert_eq!(err.error_code(), ERR_MACHINE_FINGERPRINT);
        assert!(err.to_string().contains("machine fingerprint"));
    }

    /// `io::Error` / `toml::de::Error` / `notify::Error` 的 `From` 收敛。
    #[test]
    fn std_errors_convert_into_daemon_error() {
        let io_err = io::Error::new(io::ErrorKind::NotFound, "no such file");
        let err: DaemonError = io_err.into();
        assert!(matches!(err, DaemonError::StorageError(_)));
        assert_eq!(err.error_code(), ERR_STORAGE);

        let toml_err = toml::from_str::<()>("not [valid").unwrap_err();
        let err: DaemonError = toml_err.into();
        assert!(matches!(err, DaemonError::ConfigError(_)));
        assert_eq!(err.error_code(), ERR_CONFIG);
        assert!(err.to_string().contains("toml parse"));
    }

    /// `DaemonResult<T>` 别名在签名中可用（含问号传播）。
    #[test]
    fn daemon_result_alias_works() {
        fn parse_level(raw: &str) -> DaemonResult<u8> {
            let n: u8 = raw
                .parse()
                .map_err(|e| DaemonError::ConfigError(format!("bad number: {e}")))?;
            Ok(n)
        }
        assert_eq!(parse_level("7").expect("ok"), 7);
        let err = parse_level("x").expect_err("must fail");
        assert_eq!(err.error_code(), ERR_CONFIG);
    }
}

//! `licensing-server` 错误类型雏形（LicenseError / LicenseResult）。
//!
//! 主体语义在 Wave 6（task 45-46）填充；本阶段定义枚举骨架与错误码分段，
//! 保证后续模块从一开始就收敛到统一错误口径。

/// 授权域错误码：激活。
pub const ERR_LICENSE_ACTIVATION: u16 = 1000;
/// 授权域错误码：Token。
pub const ERR_LICENSE_TOKEN: u16 = 1010;
/// 授权域错误码：心跳。
pub const ERR_LICENSE_HEARTBEAT: u16 = 1020;
/// 授权域错误码：配额。
pub const ERR_LICENSE_QUOTA: u16 = 1030;
/// 授权域错误码：激活码状态机。
pub const ERR_LICENSE_KEYSTATE: u16 = 1040;

/// licensing-server 主错误枚举。
#[derive(Debug, thiserror::Error)]
pub enum LicenseError {
    /// 激活被拒（激活码无效 / 已绑定他机 / 已废弃）。
    #[error("LicenseError: activation rejected: {0}")]
    ActivationRejected(String),

    /// Lease Token 无效或过期（签名不符 / kid 未知 / 超窗）。
    #[error("LicenseError: token invalid: {0}")]
    TokenInvalid(String),

    /// 心跳被拒（设备未激活 / 宽限超期）。
    #[error("LicenseError: heartbeat rejected: {0}")]
    HeartbeatRejected(String),

    /// 配额超限（设备数 / 租约数）。
    #[error("LicenseError: quota exceeded: {0}")]
    QuotaExceeded(String),

    /// 激活码状态非法（状态机不允许的迁移，如废弃码重发被拒）。
    #[error("LicenseError: key state illegal: {0}")]
    KeyStateIllegal(String),
}

impl LicenseError {
    /// 错误码（u16，非零）。
    pub fn error_code(&self) -> u16 {
        match self {
            LicenseError::ActivationRejected(_) => ERR_LICENSE_ACTIVATION,
            LicenseError::TokenInvalid(_) => ERR_LICENSE_TOKEN,
            LicenseError::HeartbeatRejected(_) => ERR_LICENSE_HEARTBEAT,
            LicenseError::QuotaExceeded(_) => ERR_LICENSE_QUOTA,
            LicenseError::KeyStateIllegal(_) => ERR_LICENSE_KEYSTATE,
        }
    }
}

/// licensing-server 统一 Result 别名。
pub type LicenseResult<T> = Result<T, LicenseError>;

#[cfg(test)]
mod tests {
    use super::*;

    /// 每变体可构造、Display 含 `LicenseError` 前缀、错误码非零且互异。
    #[test]
    fn variants_construct_display_and_codes() {
        let cases: Vec<(LicenseError, u16)> = vec![
            (
                LicenseError::ActivationRejected("key revoked".into()),
                ERR_LICENSE_ACTIVATION,
            ),
            (
                LicenseError::TokenInvalid("bad sig".into()),
                ERR_LICENSE_TOKEN,
            ),
            (
                LicenseError::HeartbeatRejected("grace over".into()),
                ERR_LICENSE_HEARTBEAT,
            ),
            (
                LicenseError::QuotaExceeded("max devices".into()),
                ERR_LICENSE_QUOTA,
            ),
            (
                LicenseError::KeyStateIllegal("reissue on active".into()),
                ERR_LICENSE_KEYSTATE,
            ),
        ];
        let mut seen = std::collections::HashSet::new();
        for (err, code) in cases {
            let rendered = err.to_string();
            assert!(rendered.starts_with("LicenseError"), "display: {rendered}");
            assert_ne!(code, 0);
            assert!(seen.insert(code), "duplicate code {code}");
            assert_eq!(err.error_code(), code);
        }
    }
}

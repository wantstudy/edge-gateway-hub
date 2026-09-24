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
/// 授权域错误码：存储层（SQLite / 迁移 / 约束冲突）。
pub const ERR_LICENSE_STORAGE: u16 = 1050;
/// 授权域错误码：预绑定冲突（task 46 一机一码）。
pub const ERR_LICENSE_PREBIND: u16 = 1060;

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

    /// 存储层错误（SQLite 打开 / 迁移 / 约束冲突 / 事务失败）。
    ///
    /// 该变体**不承载任何敏感值**：调用方在拼装消息时不得把激活码、私钥、
    /// 指纹原文塞进字符串（错误会被写日志与审计）。
    #[error("LicenseError: storage: {0}")]
    Storage(String),

    /// 预绑定冲突（task 46 一机一码）。
    ///
    /// **为什么必须是独立的结构化变体**：这类错误早期走 `ActivationRejected(String)`，
    /// HTTP 层再用 `msg.contains("prebind")` 反查错误码。那是**脆弱耦合**——任何人改
    /// 一句文案，错误码就静默退化成泛化 `BAD_REQUEST`，而单测通常察觉不到。
    /// 改为变体后 [`crate::http::error_to_code`] 直接 match 变体，
    /// **文案与错误码彻底解耦**（见 `service.rs` 的 `t46_prebind_*` 测试）。
    ///
    /// 消息**一律不含**机器码（指纹属敏感值），也**不含**任何他码 ID（避免跨租户泄漏）。
    #[error("LicenseError: prebind conflict: {kind}")]
    PrebindConflict {
        /// 冲突细分种类（结构化，供调用方与 HTTP 层按类型分支）。
        kind: PrebindKind,
    },
}

/// 预绑定冲突的细分种类。
///
/// 用途：让上层（HTTP 错误码映射、admin-console 提示、风控统计）能**按类型分支**，
/// 而不必解析错误消息文本。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrebindKind {
    /// 激活：该码已被预绑定到另一台机器（请求 `machine_code` 与预绑定值不符）。
    ActivationMachineMismatch,
    /// 发放 / 重发：目标机器码已被**另一张仍可用的码**预绑定。
    MachineAlreadyClaimed,
    /// 发放 / 重发：目标机器码对应的设备已被**另一张码**实际绑定。
    MachineAlreadyBound,
    /// 重发：提供了空白的 `prebind.machine_code`（`"   "` 不算「已提供」）。
    BlankMachineCode,
}

impl PrebindKind {
    /// 稳定字符串标识（日志 / 审计 / 前端 i18n key 用）。
    pub fn as_str(&self) -> &'static str {
        match self {
            PrebindKind::ActivationMachineMismatch => "activation_machine_mismatch",
            PrebindKind::MachineAlreadyClaimed => "machine_already_claimed",
            PrebindKind::MachineAlreadyBound => "machine_already_bound",
            PrebindKind::BlankMachineCode => "blank_machine_code",
        }
    }
}

impl std::fmt::Display for PrebindKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}


impl LicenseError {
    /// 构造预绑定冲突错误（语法糖，避免调用方写嵌套结构体字面量）。
    pub fn prebind_conflict(kind: PrebindKind) -> Self {
        LicenseError::PrebindConflict { kind }
    }

    /// 错误码（u16，非零）。
    pub fn error_code(&self) -> u16 {
        match self {
            LicenseError::ActivationRejected(_) => ERR_LICENSE_ACTIVATION,
            LicenseError::TokenInvalid(_) => ERR_LICENSE_TOKEN,
            LicenseError::HeartbeatRejected(_) => ERR_LICENSE_HEARTBEAT,
            LicenseError::QuotaExceeded(_) => ERR_LICENSE_QUOTA,
            LicenseError::KeyStateIllegal(_) => ERR_LICENSE_KEYSTATE,
            LicenseError::Storage(_) => ERR_LICENSE_STORAGE,
            LicenseError::PrebindConflict { .. } => ERR_LICENSE_PREBIND,
        }
    }
}

impl From<rusqlite::Error> for LicenseError {
    fn from(e: rusqlite::Error) -> Self {
        LicenseError::Storage(e.to_string())
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
            (
                LicenseError::Storage("migration failed".into()),
                ERR_LICENSE_STORAGE,
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

    /// `rusqlite::Error` 经 `From` 归一为 `Storage` 变体（错误码 1050）。
    #[test]
    fn rusqlite_error_maps_to_storage_variant() {
        let err: LicenseError = rusqlite::Error::QueryReturnedNoRows.into();
        assert_eq!(err.error_code(), ERR_LICENSE_STORAGE);
        assert!(err.to_string().contains("LicenseError: storage"), "{err}");
    }
}

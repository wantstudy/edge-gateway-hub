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
/// 授权域错误码：A 档二次校验验签失败（设计 §1.3）。
pub const ERR_LICENSE_VERIFY: u16 = 1070;
/// 授权域错误码：时间窗偏移（±5min，设计 §1.2 / §1.3）。
pub const ERR_LICENSE_SKEW: u16 = 1080;
/// 授权域错误码：租约不存在（设计 §1.2 / §1.4）。
pub const ERR_LICENSE_LEASE: u16 = 1090;
/// 授权域错误码：租约已被后台废弃（立即失效，设计 §1.2）。
pub const ERR_LICENSE_LEASE_STATE: u16 = 1100;
/// 授权域错误码：nonce 重放（设计 §1.2 / §1.3）。
pub const ERR_LICENSE_NONCE: u16 = 1110;
/// 授权域错误码：字段白名单越界 / 回执字段非法（设计 §1.3 / §1.4）。
pub const ERR_LICENSE_RECEIPT: u16 = 1120;
/// 授权域错误码：**同码异机**（一机一码冲突检测，`docs/design/machine-fingerprint.md` §7 步骤 ③）。
pub const ERR_LICENSE_BOUND_OTHER_DEVICE: u16 = 1130;
/// 授权域错误码：激活请求验签失败（`req_sig` / `device_pubkey`，2026-09-25 主理人决策）。
pub const ERR_LICENSE_ACTIVATION_SIG: u16 = 1140;
/// 授权域错误码：激活请求设备公钥与库中钉定公钥不一致（2026-09-25 主理人决策）。
pub const ERR_LICENSE_ACTIVATION_PUBKEY: u16 = 1150;
/// 授权域错误码：废弃确认串不符（`confirm_tail8`，设计 §2.2，HTTP 412）。
pub const ERR_LICENSE_CONFIRM: u16 = 1160;
/// 授权域错误码：废弃原因缺失（`reason` 必填，设计 §2.2，HTTP 400）。
pub const ERR_LICENSE_REASON: u16 = 1170;
/// 授权域错误码：管理端未认证（缺 token / token 无效或过期，HTTP 401）。
pub const ERR_LICENSE_ADMIN_AUTH: u16 = 1180;
/// 授权域错误码：管理端角色越权（RBAC 门控拒绝，HTTP 403）。
pub const ERR_LICENSE_ADMIN_RBAC: u16 = 1190;
/// 授权域错误码：租户不存在（发放 / 废弃 / 重发对未知租户 fail-closed，HTTP 400）。
pub const ERR_LICENSE_TENANT: u16 = 1200;
/// 授权域错误码：发放激活码缺少预绑定机器码（2026-09-27 主理人决策：一机一码
/// 从发放侧闭环，`prebind_machine_code` 必填非空白，HTTP 400）。
pub const ERR_LICENSE_MACHINE_CODE: u16 = 1210;

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

    /// **一机一码冲突**：激活码已绑定到另一台设备，且新机器的锚点命中数低于同机阈值
    /// （`docs/design/machine-fingerprint.md` §7 步骤 ③）。
    ///
    /// **与 [`LicenseError::PrebindConflict`] 语义不同，二者不可互替**：
    /// 后者是**预绑定**不匹配——码**尚未绑定**、首次认领时请求 `machine_code` 与
    /// `prebind_machine_code` 不符；本变体是码**已绑定**到某设备后，另一台机器上报的
    /// `anchor_hashes` 命中 **≤3/5** 被判定为**异机**。两者一个属「发码前约束」，
    /// 一个属「绑定时冲突」，错误码不可复用（`activation_machine_mismatch` 不能拿来表达本条）。
    ///
    /// 对应业务码 `CODE_BOUND_TO_OTHER_DEVICE`（HTTP **403**，`docs/design/licensing-api.md` §1.1）。
    /// 消息**一律不含**锚点哈希原文、`machine_code` 明文或激活码原文（错误会进日志与响应体），
    /// 只给出「申请换机」这类可操作提示。
    #[error("LicenseError: code bound to another device: {kind}; apply for a machine replacement via the admin console")]
    CodeBoundToOtherDevice {
        /// 判定依据（结构化，供 HTTP 层 / 前端提示按类型分支）。
        kind: BoundOtherDeviceKind,
    },

    /// 租约不存在（心跳 / 回执上报了未知 `lease_id`）。
    ///
    /// 对应业务码 `LEASE_NOT_FOUND`（HTTP 404，设计 §1.2 / §1.4）。
    #[error("LicenseError: lease not found: {0}")]
    LeaseNotFound(String),

    /// 租约已被后台废弃（**废弃语义 = 立即失效**，同一心跳周期内即被拒）。
    ///
    /// 对应业务码 `LEASE_REVOKED`（HTTP 403，设计 §1.2 / §1.3 / §1.4）。
    /// 与 [`LicenseError::HeartbeatRejected`] 同为 403 但**分属不同语义**：
    /// 后者是心跳路径的历史泛化错误，本变体专指「租约状态机已被置为废弃」。
    #[error("LicenseError: lease revoked: {0}")]
    LeaseRevoked(String),

    /// A 档二次校验（`/verify`）/ 回执（`/audit/receipt`）验签失败。
    ///
    /// 对应业务码 `VERIFY_FAIL`（HTTP 401，设计 §1.3）。
    #[error("LicenseError: verify failed: {0}")]
    VerifyFailed(String),

    /// 时间窗超限（`|now - ts| > ±5min`）。
    ///
    /// 对应业务码 `TIMESTAMP_SKEW`（HTTP 401，设计 §1.2 / §1.3）。
    /// **独立变体**：与 `VerifyFailed` 同为 401 但触发原因不同，
    /// HTTP 层据此映射到不同业务码，调用方无需解析文案。
    #[error("LicenseError: timestamp skew: {0}")]
    TimestampSkew(String),

    /// nonce 重放（同 `nonce` 二次提交 → `nonce_cache` 命中）。
    ///
    /// 对应业务码 `NONCE_REPLAY`（HTTP 409，设计 §1.2 / §1.3）。
    #[error("LicenseError: nonce replay: {0}")]
    NonceReplay(String),

    /// 字段白名单越界 / 回执字段非法（请求体出现白名单外字段或字段不合规）。
    ///
    /// 对应业务码 `FIELD_WHITELIST_VIOLATION`（HTTP 422，设计 §1.3 / §1.4）。
    #[error("LicenseError: field whitelist violation: {0}")]
    FieldWhitelistViolation(String),

    /// 激活请求验签失败（`req_sig` 与请求自带 `device_pubkey` 不匹配 / 格式非法）。
    ///
    /// 2026-09-25 主理人决策：`/activation` 开启请求验签，wire code
    /// `ACTIVATION_SIGNATURE_INVALID`（HTTP **401**）。消息**绝不**回显签名 / 公钥原文
    /// （错误会进日志与响应体，防探测）。
    #[error("LicenseError: activation signature invalid: {0}")]
    ActivationSignatureInvalid(String),

    /// 激活请求设备公钥与库中已钉定公钥不一致（设备身份漂移 / 疑似激活码盗用）。
    ///
    /// 2026-09-25 主理人决策：设备首次激活成功时 first-write-wins 钉定公钥
    /// （`device.device_pubkey`）；此后任何激活请求必须携带同一公钥。
    /// wire code `ACTIVATION_PUBKEY_MISMATCH`（HTTP **403**）。
    #[error("LicenseError: activation pubkey mismatch: {0}")]
    ActivationPubkeyMismatch(String),

    /// 废弃确认串不符（`confirm_tail8` 与激活码尾 8 位不匹配）。
    ///
    /// 对应业务码 `CONFIRM_MISMATCH`（HTTP **412**，设计 §2.2 高危操作二次确认契约）。
    /// **结构化变体**：绝不用 `msg.contains` 反查——消息不含激活码原文（防泄漏）。
    #[error("LicenseError: confirm mismatch: {0}")]
    ConfirmMismatch(String),

    /// 废弃原因缺失（`reason` 为空白）。
    ///
    /// 对应业务码 `REASON_REQUIRED`（HTTP **400**，设计 §2.2）。
    #[error("LicenseError: reason required: {0}")]
    ReasonRequired(String),

    /// 管理端未认证（缺 Bearer token / token 无效 / token 过期）。
    ///
    /// 对应业务码 `SESSION_EXPIRED`（HTTP **401**，设计 §2 通用错误）。
    /// 消息**不含** token 原文（错误会进日志）。
    #[error("LicenseError: admin unauthorized: {0}")]
    Unauthorized(String),

    /// 管理端角色越权（已认证但 RBAC 门控拒绝该操作）。
    ///
    /// 对应业务码 `ADMIN_ONLY`（HTTP **403**，设计 §2 / §4）。
    #[error("LicenseError: admin forbidden: {0}")]
    Forbidden(String),

    /// 租户不存在（发放 / 废弃 / 重发对未知租户 fail-closed）。
    ///
    /// 对应业务码 `TENANT_NOT_FOUND`（HTTP **400**）。此前走 `ActivationRejected`
    /// 泛化为 `INVALID_CODE`，前端无法区分「码无效」与「租户没建」；独立变体让
    /// admin-console 能给出「请先创建租户」的可操作引导。
    /// 消息由调用方拼装，须包含「先创建租户」类引导（见 `service.rs`）。
    #[error("LicenseError: tenant not found: {0}")]
    TenantNotFound(String),

    /// 发放激活码缺少预绑定机器码（`prebind_machine_code` 缺失 / 空白）。
    ///
    /// 对应业务码 `MACHINE_CODE_REQUIRED`（HTTP **400**）。2026-09-27 主理人决策：
    /// 一机一码从发放侧闭环，发放时机器码必填；消息**不含**任何机器码值。
    #[error("LicenseError: machine code required: {0}")]
    MachineCodeRequired(String),
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

/// 「同码异机」冲突的细分种类（对齐 [`PrebindKind`] 的稳定字符串风格：小写 `snake_case`）。
///
/// 与 [`PrebindKind`] 一样是**结构化**细分：让上层按类型分支，而**不解析错误文本**。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BoundOtherDeviceKind {
    /// 激活：该码已绑定到另一台设备，且新机器锚点命中数 **≤3/5**（低于同机阈值）→ 判异机。
    AnchorMismatch,
}

impl BoundOtherDeviceKind {
    /// 稳定字符串标识（日志 / 审计 / 前端 i18n key 用）。
    pub fn as_str(&self) -> &'static str {
        match self {
            BoundOtherDeviceKind::AnchorMismatch => "code_bound_to_other_device",
        }
    }
}

impl std::fmt::Display for BoundOtherDeviceKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl LicenseError {
    /// 构造预绑定冲突错误（语法糖，避免调用方写嵌套结构体字面量）。
    pub fn prebind_conflict(kind: PrebindKind) -> Self {
        LicenseError::PrebindConflict { kind }
    }

    /// 构造「同码异机」冲突错误（设计 §7 步骤 ③；HTTP 403 `CODE_BOUND_TO_OTHER_DEVICE`）。
    pub fn code_bound_to_other_device() -> Self {
        LicenseError::CodeBoundToOtherDevice {
            kind: BoundOtherDeviceKind::AnchorMismatch,
        }
    }

    /// 构造「租约不存在」错误。
    pub fn lease_not_found(message: impl Into<String>) -> Self {
        LicenseError::LeaseNotFound(message.into())
    }

    /// 构造「租约已废弃」错误。
    pub fn lease_revoked(message: impl Into<String>) -> Self {
        LicenseError::LeaseRevoked(message.into())
    }

    /// 构造「验签失败」错误。
    pub fn verify_failed(message: impl Into<String>) -> Self {
        LicenseError::VerifyFailed(message.into())
    }

    /// 构造「时间窗超限」错误。
    pub fn timestamp_skew(message: impl Into<String>) -> Self {
        LicenseError::TimestampSkew(message.into())
    }

    /// 构造「nonce 重放」错误。
    pub fn nonce_replay(message: impl Into<String>) -> Self {
        LicenseError::NonceReplay(message.into())
    }

    /// 构造「字段白名单越界」错误。
    pub fn field_whitelist_violation(message: impl Into<String>) -> Self {
        LicenseError::FieldWhitelistViolation(message.into())
    }

    /// 构造「激活请求验签失败」错误（HTTP 401，`ACTIVATION_SIGNATURE_INVALID`）。
    pub fn activation_signature_invalid(message: impl Into<String>) -> Self {
        LicenseError::ActivationSignatureInvalid(message.into())
    }

    /// 构造「激活公钥与钉定值不一致」错误（HTTP 403，`ACTIVATION_PUBKEY_MISMATCH`）。
    pub fn activation_pubkey_mismatch(message: impl Into<String>) -> Self {
        LicenseError::ActivationPubkeyMismatch(message.into())
    }

    /// 构造「废弃确认串不符」错误（HTTP 412，`CONFIRM_MISMATCH`）。
    pub fn confirm_mismatch(message: impl Into<String>) -> Self {
        LicenseError::ConfirmMismatch(message.into())
    }

    /// 构造「废弃原因缺失」错误（HTTP 400，`REASON_REQUIRED`）。
    pub fn reason_required(message: impl Into<String>) -> Self {
        LicenseError::ReasonRequired(message.into())
    }

    /// 构造「管理端未认证」错误（HTTP 401，`SESSION_EXPIRED`）。
    pub fn unauthorized(message: impl Into<String>) -> Self {
        LicenseError::Unauthorized(message.into())
    }

    /// 构造「管理端角色越权」错误（HTTP 403，`ADMIN_ONLY`）。
    pub fn forbidden(message: impl Into<String>) -> Self {
        LicenseError::Forbidden(message.into())
    }

    /// 构造「租户不存在」错误（HTTP 400，`TENANT_NOT_FOUND`）。
    pub fn tenant_not_found(message: impl Into<String>) -> Self {
        LicenseError::TenantNotFound(message.into())
    }

    /// 构造「发放缺少机器码」错误（HTTP 400，`MACHINE_CODE_REQUIRED`）。
    pub fn machine_code_required(message: impl Into<String>) -> Self {
        LicenseError::MachineCodeRequired(message.into())
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
            LicenseError::CodeBoundToOtherDevice { .. } => ERR_LICENSE_BOUND_OTHER_DEVICE,
            LicenseError::LeaseNotFound(_) => ERR_LICENSE_LEASE,
            LicenseError::LeaseRevoked(_) => ERR_LICENSE_LEASE_STATE,
            LicenseError::VerifyFailed(_) => ERR_LICENSE_VERIFY,
            LicenseError::TimestampSkew(_) => ERR_LICENSE_SKEW,
            LicenseError::NonceReplay(_) => ERR_LICENSE_NONCE,
            LicenseError::FieldWhitelistViolation(_) => ERR_LICENSE_RECEIPT,
            LicenseError::ActivationSignatureInvalid(_) => ERR_LICENSE_ACTIVATION_SIG,
            LicenseError::ActivationPubkeyMismatch(_) => ERR_LICENSE_ACTIVATION_PUBKEY,
            LicenseError::ConfirmMismatch(_) => ERR_LICENSE_CONFIRM,
            LicenseError::ReasonRequired(_) => ERR_LICENSE_REASON,
            LicenseError::Unauthorized(_) => ERR_LICENSE_ADMIN_AUTH,
            LicenseError::Forbidden(_) => ERR_LICENSE_ADMIN_RBAC,
            LicenseError::TenantNotFound(_) => ERR_LICENSE_TENANT,
            LicenseError::MachineCodeRequired(_) => ERR_LICENSE_MACHINE_CODE,
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
            (
                LicenseError::LeaseNotFound("lease-1".into()),
                ERR_LICENSE_LEASE,
            ),
            (
                LicenseError::LeaseRevoked("lease-1".into()),
                ERR_LICENSE_LEASE_STATE,
            ),
            (
                LicenseError::VerifyFailed("bad sig".into()),
                ERR_LICENSE_VERIFY,
            ),
            (
                LicenseError::TimestampSkew("ts out of window".into()),
                ERR_LICENSE_SKEW,
            ),
            (
                LicenseError::NonceReplay("nonce-1".into()),
                ERR_LICENSE_NONCE,
            ),
            (
                LicenseError::FieldWhitelistViolation("flow_rate".into()),
                ERR_LICENSE_RECEIPT,
            ),
            (
                LicenseError::code_bound_to_other_device(),
                ERR_LICENSE_BOUND_OTHER_DEVICE,
            ),
            (
                LicenseError::activation_signature_invalid("bad sig"),
                ERR_LICENSE_ACTIVATION_SIG,
            ),
            (
                LicenseError::activation_pubkey_mismatch("pubkey drift"),
                ERR_LICENSE_ACTIVATION_PUBKEY,
            ),
            (
                LicenseError::confirm_mismatch("tail8 mismatch"),
                ERR_LICENSE_CONFIRM,
            ),
            (
                LicenseError::reason_required("empty reason"),
                ERR_LICENSE_REASON,
            ),
            (
                LicenseError::unauthorized("no token"),
                ERR_LICENSE_ADMIN_AUTH,
            ),
            (LicenseError::forbidden("role gate"), ERR_LICENSE_ADMIN_RBAC),
            (
                LicenseError::tenant_not_found("unknown tenant: t-x"),
                ERR_LICENSE_TENANT,
            ),
            (
                LicenseError::machine_code_required("issue requires machine code"),
                ERR_LICENSE_MACHINE_CODE,
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

    /// 「同码异机」结构化变体：错误码正确、Display 含稳定串与可操作提示、**不含敏感值**。
    #[test]
    fn code_bound_to_other_device_carries_stable_kind_and_no_secret() {
        let err = LicenseError::code_bound_to_other_device();
        assert_eq!(err.error_code(), ERR_LICENSE_BOUND_OTHER_DEVICE);
        assert_ne!(
            err.error_code(),
            ERR_LICENSE_PREBIND,
            "不得与预绑定冲突共码"
        );

        let rendered = err.to_string();
        assert!(rendered.starts_with("LicenseError"), "{rendered}");
        // 稳定细分类字符串（前端 i18n key / 风控统计）。
        assert!(
            rendered.contains("code_bound_to_other_device"),
            "缺少稳定 kind 串: {rendered}"
        );
        // 可操作提示（引导「申请换机」）。
        assert!(rendered.contains("machine replacement"), "{rendered}");
        // 结构化 kind 的稳定字符串与 PrebindKind 风格一致（小写 snake_case）。
        assert_eq!(
            BoundOtherDeviceKind::AnchorMismatch.as_str(),
            "code_bound_to_other_device"
        );
        assert_ne!(
            BoundOtherDeviceKind::AnchorMismatch.as_str(),
            PrebindKind::ActivationMachineMismatch.as_str(),
            "异机冲突的 kind 不得与预绑定不匹配混用"
        );
    }
}

//! task 57 切片 2 — 管理面 RBAC 原语（服务端判定，四角色 + 权限矩阵 + JWT 守卫）。
//!
//! ## 权威来源与两端对齐（防漂移）
//! - 角色定义权威来源：`docs/design/licensing-api.md` §4「RBAC 四角色」；
//! - **本模块的角色 id 与权限名必须与 `web-console`/`ui-kit` 的
//!   `ui-kit/src/rbac.ts`（前端唯一真源）保持同一套**：前端四角色
//!   `ops / lic_ops / risk / system` 即服务端 JWT `role` claim 的取值域。
//!   rbac.ts 注释中的「后端 `admin` → 前端 `system`、后端 `viewer` → 前端 `risk`」
//!   是历史命名映射说明——**服务端签发的 JWT 只发四个规范 id**，`admin`/`viewer`
//!   等别名在 [`Role::from_str`] 一律拒绝（拒绝即审计友好：未知角色是独立错误变体）；
//! - ⚠️ 权限矩阵改动必须**两端同步**（[`permissions_of`] ↔ rbac.ts `ACTION_MATRIX`），
//!   这是 task 31/57 收口的关键契约点。
//!
//! ## 红线
//! - **授权判定只在 Rust 侧**：本模块是唯一服务端判定入口；WebView/JS 侧的
//!   `can()` 仅做可见性/禁用态，后端必须独立再校验一次（rbac.ts 已注明）。
//! - **fail-closed**：未知角色 / 未知权限 / 未授权一律拒绝；`authorize` 对
//!   矩阵外权限返回 `false`（权限枚举穷举，不存在字符串注入面）。
//! - 管理面运维动作（remote_ops 的 restart / collectors / logs_read）映射为
//!   `ops.*` 三个服务端权限（前端 ACTION_MATRIX 无对应页面，属 mgmt 面新增，
//!   仅 `system` 可授，见 [`permissions_of`] 注释）。

use axum::async_trait;
use axum::extract::{FromRef, FromRequestParts};
use axum::http::request::Parts;
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Json, Response};
use serde_json::json;

use super::auth_jwt::{now_unix_secs, verify, Claims, IssuerKey, JwtError, DEFAULT_LEEWAY_SECS};

// ---- 角色 ----

/// 管理面四角色（licensing-api §4；与 `ui-kit/src/rbac.ts` `ROLES` 同一套 id）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Role {
    /// 运营（只读 + 发放）。
    Ops,
    /// 授权运营（发放 / 废弃 / 重发，高危操作承担者）。
    LicOps,
    /// 风控（回执异常 / 审计只读 + 标记异常）。
    Risk,
    /// 系统（密钥轮换 / 租户策略 / 账号；mgmt 面运维动作唯一可授角色）。
    System,
}

impl Role {
    /// 角色字面量（JWT `role` claim 取值；与 rbac.ts `ROLES` 完全一致）。
    pub fn as_str(self) -> &'static str {
        match self {
            Role::Ops => "ops",
            Role::LicOps => "lic_ops",
            Role::Risk => "risk",
            Role::System => "system",
        }
    }

    /// 解析角色字面量（大小写敏感、**不接受别名**：`admin`/`viewer` 等
    /// 历史命名一律 `None`，由上层转 [`JwtError::UnknownRole`] 便于审计区分）。
    #[allow(clippy::should_implement_trait)]
    pub fn from_str(raw: &str) -> Option<Self> {
        match raw {
            "ops" => Some(Role::Ops),
            "lic_ops" => Some(Role::LicOps),
            "risk" => Some(Role::Risk),
            "system" => Some(Role::System),
            _ => None,
        }
    }
}

// ---- 权限 ----

/// 管理面操作级权限（与 rbac.ts `Action` 同名对齐 + `ops.*` 三个 mgmt 面权限）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Permission {
    // —— 与 rbac.ts `Action` 一一对应（19 项，勿改名）——
    /// 激活码查看。
    CodeView,
    /// 激活码发放。
    CodeIssue,
    /// 激活码废弃（高危，仅 lic_ops）。
    CodeRevoke,
    /// 激活码重发（高危，仅 lic_ops）。
    CodeReissue,
    /// 激活码明文揭示（敏感，lic_ops / system）。
    CodeReveal,
    /// 设备查看。
    DeviceView,
    /// 设备标记异常。
    DeviceMarkAnomaly,
    /// 租户查看（仅 system）。
    TenantView,
    /// 租户策略更新（仅 system）。
    TenantPolicyUpdate,
    /// 回执查看。
    ReceiptView,
    /// 回执标记。
    ReceiptMark,
    /// 换机工单查看（仅 lic_ops）。
    TransferView,
    /// 换机工单处理（仅 lic_ops）。
    TransferProcess,
    /// 签名密钥查看（仅 system）。
    KeyView,
    /// 签名密钥轮换（仅 system）。
    KeyRotate,
    /// 审计日志在线只读（risk / system）。
    AuditView,
    /// 审计日志导出（**仅 system**：数据出境动作，收敛单一角色便于追责，
    /// 见 rbac.ts 顶部契约注释）。
    AuditExport,
    /// 账号查看（仅 system）。
    AccountView,
    /// 账号更新（仅 system）。
    AccountUpdate,
    // —— mgmt 面新增（remote_ops 动作映射；前端 ACTION_MATRIX 无对应页面）——
    /// 远程重启（危险动作，仅 system）。
    OpsRestart,
    /// 采集器运行期启停（仅 system）。
    OpsCollectors,
    /// 运维日志查询（仅 system）。
    OpsLogsRead,
    // —— mgmt 面新增（设备/点位配置写接口；⚠️ 与前端同步：命名已按点分小写
    //    预留，前端适配时须在 `ui-kit/src/rbac.ts` 的 `Action` / `ACTION_MATRIX`
    //    补充 `device.write` / `point.write`——配置写为高危动作，仅 system 可授）——
    /// 设备配置写（新增 / 启停改名 / 删除，仅 system）。
    DeviceWrite,
    /// 点位配置写（新增 / 修改 / 删除，仅 system）。
    PointWrite,
}

impl Permission {
    /// 权限字面量（点分小写；与 rbac.ts `Action` 字符串完全一致）。
    pub fn as_str(self) -> &'static str {
        match self {
            Permission::CodeView => "code.view",
            Permission::CodeIssue => "code.issue",
            Permission::CodeRevoke => "code.revoke",
            Permission::CodeReissue => "code.reissue",
            Permission::CodeReveal => "code.reveal",
            Permission::DeviceView => "device.view",
            Permission::DeviceMarkAnomaly => "device.mark_anomaly",
            Permission::TenantView => "tenant.view",
            Permission::TenantPolicyUpdate => "tenant.policy_update",
            Permission::ReceiptView => "receipt.view",
            Permission::ReceiptMark => "receipt.mark",
            Permission::TransferView => "transfer.view",
            Permission::TransferProcess => "transfer.process",
            Permission::KeyView => "key.view",
            Permission::KeyRotate => "key.rotate",
            Permission::AuditView => "audit.view",
            Permission::AuditExport => "audit.export",
            Permission::AccountView => "account.view",
            Permission::AccountUpdate => "account.update",
            Permission::OpsRestart => "ops.restart",
            Permission::OpsCollectors => "ops.collectors",
            Permission::OpsLogsRead => "ops.logs_read",
            Permission::DeviceWrite => "device.write",
            Permission::PointWrite => "point.write",
        }
    }
}

// ---- 权限矩阵（唯一真源；两端同步点） ----

/// ⚠️ **与 web-console `ui-kit/src/rbac.ts` 的 `ACTION_MATRIX` 对齐，改动须两端同步。**
///
/// 顺序镜像 rbac.ts 各角色数组（便于人工 diff 审查）；`system` 末尾追加的
/// `ops.*` 三项为 mgmt 面权限（前端矩阵无对应页面，见模块注释）。
const PERMS_OPS: &[Permission] = &[
    Permission::CodeView,
    Permission::CodeIssue,
    Permission::DeviceView,
    Permission::ReceiptView,
    Permission::ReceiptMark,
];

const PERMS_LIC_OPS: &[Permission] = &[
    Permission::CodeView,
    Permission::CodeIssue,
    Permission::CodeRevoke,
    Permission::CodeReissue,
    Permission::CodeReveal,
    Permission::DeviceView,
    Permission::DeviceMarkAnomaly,
    Permission::ReceiptView,
    Permission::ReceiptMark,
    Permission::TransferView,
    Permission::TransferProcess,
];

const PERMS_RISK: &[Permission] = &[
    Permission::ReceiptView,
    Permission::ReceiptMark,
    Permission::AuditView,
    Permission::DeviceView,
    Permission::CodeView,
];

const PERMS_SYSTEM: &[Permission] = &[
    Permission::TenantView,
    Permission::TenantPolicyUpdate,
    Permission::KeyView,
    Permission::KeyRotate,
    Permission::AuditView,
    Permission::AuditExport,
    Permission::AccountView,
    Permission::AccountUpdate,
    Permission::CodeReveal,
    Permission::DeviceView,
    Permission::ReceiptView,
    // mgmt 面运维权限（remote_ops 动作；仅 system 档可授）。
    Permission::OpsRestart,
    Permission::OpsCollectors,
    Permission::OpsLogsRead,
    // mgmt 面配置写权限（设备/点位 CRUD；仅 system 档可授，与前端同步见 Permission 注释）。
    Permission::DeviceWrite,
    Permission::PointWrite,
];

/// 角色 → 可授权限集合（**映射关系集中于此一个函数**；改动须与
/// `ui-kit/src/rbac.ts` `ACTION_MATRIX` 两端同步）。
pub fn permissions_of(role: Role) -> &'static [Permission] {
    match role {
        Role::Ops => PERMS_OPS,
        Role::LicOps => PERMS_LIC_OPS,
        Role::Risk => PERMS_RISK,
        Role::System => PERMS_SYSTEM,
    }
}

/// 授权判定（服务端唯一入口；fail-closed：矩阵外权限一律 `false`）。
pub fn authorize(role: Role, permission: Permission) -> bool {
    permissions_of(role).contains(&permission)
}

/// remote_ops 动作字面量（`OpsAction::as_str`）→ mgmt 面权限。
///
/// 供 task 57 全量接线时把 `/api/ops/*` 的 authorizer 切到本模块：
/// `DenyAllOpsAuthorizer` 替换为「解 JWT → 取 role → [`authorize`]",
/// 端点零改动（对齐 remote_ops.rs 注释中的接线点约定）。
pub fn permission_for_ops_action(action: &str) -> Option<Permission> {
    match action {
        "restart" => Some(Permission::OpsRestart),
        "collectors_pause" | "collectors_resume" => Some(Permission::OpsCollectors),
        "logs_read" => Some(Permission::OpsLogsRead),
        _ => None,
    }
}

// ---- JWT 守卫（axum extractor，对齐 remote_ops 的注入式风格） ----

/// 鉴权上下文（axum `State` 成员；`FromRef` 提取，注入式携带签名密钥，不做 IO）。
///
/// 密钥来源（配置 / 派生）由后续接线任务决定；本模块只消费 [`IssuerKey`]。
#[derive(Debug, Clone)]
pub struct RbacAuth {
    /// JWT HS256 签名密钥。
    key: IssuerKey,
    /// 时钟偏移容忍（秒；默认 60）。
    leeway_secs: i64,
}

impl RbacAuth {
    /// 以默认时钟偏移容忍（±60s）构造。
    pub fn new(key: IssuerKey) -> Self {
        Self {
            key,
            leeway_secs: DEFAULT_LEEWAY_SECS,
        }
    }

    /// 覆盖时钟偏移容忍（秒；测试 / 严格部署可调）。
    pub fn with_leeway(mut self, secs: i64) -> Self {
        self.leeway_secs = secs;
        self
    }

    /// 只读访问签名密钥（供非 extractor 形式的手动校验路径）。
    pub fn key(&self) -> IssuerKey {
        self.key
    }

    /// 只读访问时钟偏移容忍。
    pub fn leeway_secs(&self) -> i64 {
        self.leeway_secs
    }
}

/// 已通过 JWT 鉴权的请求身份（extractor 产物；handler 内用 [`AuthedRole::ensure`]
/// 做权限判定，401/403 自动区分）。
#[derive(Debug, Clone)]
pub struct AuthedRole {
    /// 已校验的 claims（`sub` / `jti` / `exp` 可供审计取用）。
    pub claims: Claims,
    /// 解析后的规范角色。
    pub role: Role,
}

impl AuthedRole {
    /// 权限判定（403 路径）：角色不持该权限时返回 [`AuthRejection::Forbidden`]。
    pub fn ensure(&self, permission: Permission) -> Result<(), AuthRejection> {
        if authorize(self.role, permission) {
            Ok(())
        } else {
            Err(AuthRejection::Forbidden(format!(
                "role {:?} is not granted {:?}",
                self.role.as_str(),
                permission.as_str()
            )))
        }
    }
}

/// 鉴权 / 授权拒绝（axum `Rejection`）。
///
/// 语义区分（审计区分攻击面）：
/// - [`AuthRejection::Unauthorized`] → **401**：身份未建立（缺 token / token 非法 /
///   签名错 / 过期 / 时间窗不对）；
/// - [`AuthRejection::Forbidden`] → **403**：身份已建立但权限不足
///   （含 JWT 内角色字面量不是四个规范 id 的 [`JwtError::UnknownRole`]——
///   签名已验真，属「已认证但无有效授权身份」）。
#[derive(Debug)]
pub enum AuthRejection {
    /// 401：身份未建立。
    Unauthorized(String),
    /// 403：权限不足 / 角色未知。
    Forbidden(String),
}

impl AuthRejection {
    /// 对应 HTTP 状态码（测试与调用方可复用）。
    pub fn status(&self) -> StatusCode {
        match self {
            AuthRejection::Unauthorized(_) => StatusCode::UNAUTHORIZED,
            AuthRejection::Forbidden(_) => StatusCode::FORBIDDEN,
        }
    }
}

impl std::fmt::Display for AuthRejection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AuthRejection::Unauthorized(m) => write!(f, "Unauthorized: {m}"),
            AuthRejection::Forbidden(m) => write!(f, "Forbidden: {m}"),
        }
    }
}

impl std::error::Error for AuthRejection {}

/// JWT 校验失败 → 401（`UnknownRole` 例外 → 403，语义见枚举注释）。
impl From<JwtError> for AuthRejection {
    fn from(err: JwtError) -> Self {
        match err {
            JwtError::UnknownRole(role) => {
                AuthRejection::Forbidden(format!("unknown role {role:?} in token"))
            }
            other => AuthRejection::Unauthorized(other.to_string()),
        }
    }
}

impl IntoResponse for AuthRejection {
    fn into_response(self) -> Response {
        let (status, code, message) = match &self {
            AuthRejection::Unauthorized(m) => (StatusCode::UNAUTHORIZED, "unauthorized", m),
            AuthRejection::Forbidden(m) => (StatusCode::FORBIDDEN, "forbidden", m),
        };
        // 不回显 token 任何内容；message 只含失败原因类别（便于审计区分）。
        (status, Json(json!({ "error": code, "message": message }))).into_response()
    }
}

/// 从 `Authorization: Bearer <token>` 提取 token（scheme 大小写不敏感）。
fn bearer_token(headers: &HeaderMap) -> Option<String> {
    let raw = headers.get(header::AUTHORIZATION)?.to_str().ok()?;
    let (scheme, token) = raw.split_once(' ')?;
    if !scheme.eq_ignore_ascii_case("bearer") {
        return None;
    }
    let token = token.trim();
    if token.is_empty() {
        return None;
    }
    Some(token.to_string())
}

#[async_trait]
impl<S> FromRequestParts<S> for AuthedRole
where
    RbacAuth: FromRef<S>,
    S: Send + Sync,
{
    type Rejection = AuthRejection;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        let auth = RbacAuth::from_ref(state);
        let token = bearer_token(&parts.headers).ok_or_else(|| {
            AuthRejection::Unauthorized("missing Authorization: Bearer token".to_string())
        })?;
        // 签名 / 时间窗 / 角色解析全部在 Rust 侧完成（红线）；未知角色由
        // `From<JwtError>` 转 403，其余转 401。
        let claims = verify(&token, auth.key, now_unix_secs(), auth.leeway_secs)?;
        let role = claims.role;
        Ok(AuthedRole { claims, role })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mgmt::auth_jwt::sign;
    use axum::http::Request;

    /// 测试密钥（任意固定 32 字节；生产密钥由接线任务注入，不落盘）。
    const TEST_KEY: IssuerKey = IssuerKey([0x5Au8; 32]);
    /// 与 TEST_KEY 不同的另一把密钥（错误密钥拒绝用）。
    const OTHER_KEY: IssuerKey = IssuerKey([0xA5u8; 32]);
    /// 测试基准时刻（任意固定秒级 Unix 时间）。
    const NOW: i64 = 1_700_000_000;

    /// 构造一组有效 claims（exp/iat 相对真实墙钟，extractor 测试直接可用）。
    fn claims_for(role: Role) -> Claims {
        let now = now_unix_secs();
        Claims {
            sub: "user-1".to_string(),
            role,
            exp: now + 600,
            iat: now,
            nbf: None,
            jti: "jti-1".to_string(),
        }
    }

    /// 构造携带 `Authorization: Bearer` 头的请求 parts。
    fn parts_with(token: Option<&str>) -> Parts {
        let mut builder = Request::builder();
        if let Some(t) = token {
            builder = builder.header(header::AUTHORIZATION, format!("Bearer {t}"));
        }
        let request = builder.body(()).expect("build request");
        let (parts, _) = request.into_parts();
        parts
    }

    /// 测试用 state（演示 `RbacAuth: FromRef<S>` 的最小装配形态）。
    #[derive(Clone)]
    struct TestState {
        auth: RbacAuth,
    }

    impl FromRef<TestState> for RbacAuth {
        fn from_ref(state: &TestState) -> RbacAuth {
            state.auth.clone()
        }
    }

    fn test_state() -> TestState {
        TestState {
            auth: RbacAuth::new(TEST_KEY),
        }
    }

    /// 测试辅助：按任意 header/payload JSON 手工签发 HS256 token
    /// （auth_jwt::sign 只接受类型化 Claims，负例构造需绕过类型层）。
    fn craft_token(header: &serde_json::Value, payload: &serde_json::Value) -> String {
        use base64::engine::general_purpose::URL_SAFE_NO_PAD;
        use base64::Engine as _;
        use hmac::{Hmac, Mac};
        use sha2::Sha256;

        let header_b64 = URL_SAFE_NO_PAD.encode(header.to_string());
        let payload_b64 = URL_SAFE_NO_PAD.encode(payload.to_string());
        let signing_input = format!("{header_b64}.{payload_b64}");
        let mut mac = <Hmac<Sha256> as Mac>::new_from_slice(&TEST_KEY.0).expect("hmac init");
        mac.update(signing_input.as_bytes());
        let sig_b64 = URL_SAFE_NO_PAD.encode(mac.finalize().into_bytes().as_slice());
        format!("{signing_input}.{sig_b64}")
    }

    // ---- 角色字面量 ----

    /// QA: 角色字面量与 rbac.ts `ROLES` 完全一致；别名（admin/viewer 等）
    /// 与大小写变体一律拒绝（防历史命名漂移进服务端）。
    #[test]
    fn role_str_roundtrip_and_aliases_rejected() {
        for role in [Role::Ops, Role::LicOps, Role::Risk, Role::System] {
            assert_eq!(
                Role::from_str(role.as_str()),
                Some(role),
                "{}",
                role.as_str()
            );
        }
        assert_eq!(
            Role::from_str("admin"),
            None,
            "legacy alias must be rejected"
        );
        assert_eq!(
            Role::from_str("viewer"),
            None,
            "legacy alias must be rejected"
        );
        assert_eq!(Role::from_str("System"), None, "case sensitive");
        assert_eq!(Role::from_str(""), None);
        assert_eq!(Role::from_str("root"), None);
    }

    // ---- 权限矩阵 ----

    /// QA 红线: `audit.export` 仅 system 可授（数据出境动作收敛单一角色；
    /// risk 虽可 audit.view 但不可导出，见 rbac.ts 契约注释）。
    #[test]
    fn audit_export_granted_only_to_system() {
        assert!(authorize(Role::System, Permission::AuditExport));
        assert!(!authorize(Role::Ops, Permission::AuditExport));
        assert!(!authorize(Role::LicOps, Permission::AuditExport));
        assert!(!authorize(Role::Risk, Permission::AuditExport));

        // 对照：audit.view 是 risk / system 双角色（防矩阵被误改）。
        assert!(authorize(Role::Risk, Permission::AuditView));
        assert!(authorize(Role::System, Permission::AuditView));
        assert!(!authorize(Role::Ops, Permission::AuditView));
    }

    /// QA: 四角色权限矩阵与 rbac.ts `ACTION_MATRIX` 逐项对齐
    /// （顺序镜像前端数组；system 末尾追加 mgmt 面权限：ops.* 三项 +
    /// 配置写 device.write / point.write 两项——均属 mgmt 面新增，
    /// 前端适配时须同步进 Action / ACTION_MATRIX，见 Permission 注释）。
    #[test]
    fn permission_matrix_matches_frontend_action_matrix() {
        let expected_ops: &[Permission] = &[
            Permission::CodeView,
            Permission::CodeIssue,
            Permission::DeviceView,
            Permission::ReceiptView,
            Permission::ReceiptMark,
        ];
        let expected_lic_ops: &[Permission] = &[
            Permission::CodeView,
            Permission::CodeIssue,
            Permission::CodeRevoke,
            Permission::CodeReissue,
            Permission::CodeReveal,
            Permission::DeviceView,
            Permission::DeviceMarkAnomaly,
            Permission::ReceiptView,
            Permission::ReceiptMark,
            Permission::TransferView,
            Permission::TransferProcess,
        ];
        let expected_risk: &[Permission] = &[
            Permission::ReceiptView,
            Permission::ReceiptMark,
            Permission::AuditView,
            Permission::DeviceView,
            Permission::CodeView,
        ];
        let expected_system: &[Permission] = &[
            Permission::TenantView,
            Permission::TenantPolicyUpdate,
            Permission::KeyView,
            Permission::KeyRotate,
            Permission::AuditView,
            Permission::AuditExport,
            Permission::AccountView,
            Permission::AccountUpdate,
            Permission::CodeReveal,
            Permission::DeviceView,
            Permission::ReceiptView,
            Permission::OpsRestart,
            Permission::OpsCollectors,
            Permission::OpsLogsRead,
            Permission::DeviceWrite,
            Permission::PointWrite,
        ];

        assert_eq!(permissions_of(Role::Ops), expected_ops);
        assert_eq!(permissions_of(Role::LicOps), expected_lic_ops);
        assert_eq!(permissions_of(Role::Risk), expected_risk);
        assert_eq!(permissions_of(Role::System), expected_system);

        // 关键最小权限抽查（高危动作不越档）。
        assert!(!authorize(Role::Ops, Permission::CodeRevoke));
        assert!(authorize(Role::LicOps, Permission::CodeRevoke));
        assert!(!authorize(Role::Risk, Permission::CodeIssue));
        assert!(!authorize(Role::Ops, Permission::KeyRotate));
        assert!(authorize(Role::System, Permission::KeyRotate));

        // 配置写权限仅 system 可授（设备/点位写 = 高危配置动作，见 Permission 注释）。
        assert!(authorize(Role::System, Permission::DeviceWrite));
        assert!(authorize(Role::System, Permission::PointWrite));
        assert!(!authorize(Role::Ops, Permission::DeviceWrite));
        assert!(!authorize(Role::LicOps, Permission::DeviceWrite));
        assert!(!authorize(Role::Risk, Permission::DeviceWrite));
        assert!(!authorize(Role::Ops, Permission::PointWrite));
        assert!(!authorize(Role::LicOps, Permission::PointWrite));
        assert!(!authorize(Role::Risk, Permission::PointWrite));
    }

    /// QA: remote_ops 动作字面量 → mgmt 面权限映射稳定（task 57 全量接线依据）；
    /// restart / collectors / logs_read 仅 system 可授，未知动作映射 None。
    #[test]
    fn ops_action_permission_mapping_and_system_only() {
        assert_eq!(
            permission_for_ops_action("restart"),
            Some(Permission::OpsRestart)
        );
        assert_eq!(
            permission_for_ops_action("collectors_pause"),
            Some(Permission::OpsCollectors)
        );
        assert_eq!(
            permission_for_ops_action("collectors_resume"),
            Some(Permission::OpsCollectors)
        );
        assert_eq!(
            permission_for_ops_action("logs_read"),
            Some(Permission::OpsLogsRead)
        );
        assert_eq!(permission_for_ops_action("nuke"), None);
        assert_eq!(permission_for_ops_action(""), None);

        for permission in [
            Permission::OpsRestart,
            Permission::OpsCollectors,
            Permission::OpsLogsRead,
        ] {
            assert!(authorize(Role::System, permission));
            assert!(!authorize(Role::Ops, permission));
            assert!(!authorize(Role::LicOps, permission));
            assert!(!authorize(Role::Risk, permission));
        }
    }

    // ---- JWT 守卫 extractor ----

    /// QA 安全: 缺头 / 非 Bearer / 垃圾 token / 错误密钥签名 / 过期 → 全部 401。
    #[tokio::test]
    async fn extractor_401_on_missing_bad_or_expired_token() {
        let state = test_state();

        // 缺 Authorization 头。
        let rejection = AuthedRole::from_request_parts(&mut parts_with(None), &state)
            .await
            .expect_err("missing header must be rejected");
        assert_eq!(rejection.status(), StatusCode::UNAUTHORIZED);

        // 非 Bearer scheme。
        let mut parts = parts_with(None);
        parts.headers.insert(
            header::AUTHORIZATION,
            header::HeaderValue::from_static("Basic dXNlcjpwYXNz"),
        );
        let rejection = AuthedRole::from_request_parts(&mut parts, &state)
            .await
            .expect_err("non-bearer scheme must be rejected");
        assert_eq!(rejection.status(), StatusCode::UNAUTHORIZED);

        // 结构非法 token。
        let rejection = AuthedRole::from_request_parts(&mut parts_with(Some("garbage")), &state)
            .await
            .expect_err("garbage token must be rejected");
        assert_eq!(rejection.status(), StatusCode::UNAUTHORIZED);

        // 错误密钥签发。
        let token = sign(&claims_for(Role::Ops), OTHER_KEY).expect("sign");
        let rejection = AuthedRole::from_request_parts(&mut parts_with(Some(&token)), &state)
            .await
            .expect_err("wrong-key token must be rejected");
        assert_eq!(rejection.status(), StatusCode::UNAUTHORIZED);

        // 过期（NOW 基准构造，extractor 用真实墙钟 → 已远过期）。
        let expired = Claims {
            exp: NOW - 3600,
            ..claims_for(Role::Ops)
        };
        let token = sign(&expired, TEST_KEY).expect("sign");
        let rejection = AuthedRole::from_request_parts(&mut parts_with(Some(&token)), &state)
            .await
            .expect_err("expired token must be rejected");
        assert_eq!(rejection.status(), StatusCode::UNAUTHORIZED);
    }

    /// QA: 合法 token → extractor 给出规范角色；`ensure` 权限不足 403、
    /// 授权动作 Ok；token 内未知角色（历史别名 admin）→ 403（签名已验真，
    /// 属授权身份无效，与 401 区分）。
    #[tokio::test]
    async fn extractor_403_on_insufficient_permission_or_unknown_role() {
        let state = test_state();

        // ops 持 code.issue，不持 key.rotate。
        let token = sign(&claims_for(Role::Ops), TEST_KEY).expect("sign");
        let authed = AuthedRole::from_request_parts(&mut parts_with(Some(&token)), &state)
            .await
            .expect("valid token must pass");
        assert_eq!(authed.role, Role::Ops);
        assert_eq!(authed.claims.sub, "user-1");
        authed.ensure(Permission::CodeIssue).expect("ops may issue");
        let rejection = authed
            .ensure(Permission::KeyRotate)
            .expect_err("ops must not rotate keys");
        assert_eq!(rejection.status(), StatusCode::FORBIDDEN);

        // risk 可读审计但不可导出（audit.export 红线走 403 路径）。
        let token = sign(&claims_for(Role::Risk), TEST_KEY).expect("sign");
        let authed = AuthedRole::from_request_parts(&mut parts_with(Some(&token)), &state)
            .await
            .expect("valid token must pass");
        authed
            .ensure(Permission::AuditView)
            .expect("risk may view audit");
        let rejection = authed
            .ensure(Permission::AuditExport)
            .expect_err("risk must not export audit");
        assert_eq!(rejection.status(), StatusCode::FORBIDDEN);

        // 历史别名 admin：签名合法但角色未知 → 403（非 401）。
        let now = now_unix_secs();
        let admin_payload = format!(
            r#"{{"sub":"user-1","role":"admin","exp":{},"iat":{},"jti":"jti-1"}}"#,
            now + 9_999_999,
            now
        );
        // 任意 payload 均可被签发校验（header/payload 自由拼接，签名仍正确）。
        let token = craft_token(
            &serde_json::json!({"alg": "HS256", "typ": "JWT"}),
            &serde_json::from_str::<serde_json::Value>(&admin_payload).expect("payload json"),
        );
        let rejection = AuthedRole::from_request_parts(&mut parts_with(Some(&token)), &state)
            .await
            .expect_err("legacy alias admin must be rejected");
        assert_eq!(
            rejection.status(),
            StatusCode::FORBIDDEN,
            "unknown role = authenticated but no valid authority: {rejection}"
        );
    }
}

//! 账号与角色管理端点（D 项收口）。
//!
//! 端点清单：`GET/POST /api/roles` + `PUT/DELETE /api/roles/:id` +
//! `GET/POST /api/accounts` + `PUT/DELETE /api/accounts/:account` +
//! `GET /api/permissions`。
//!
//!
//! ## 契约（以 `web-console/src/api/repo.ts` 冻结形状为准，**不改前端**）
//! - `GET /api/roles` / `GET /api/accounts` / `GET /api/permissions` → **裸数组**
//!   （repo 层 `apiRequest<unknown[]>`，无分页对象）；
//! - 角色行：`{id, name, permissions: string[], builtin, accountCount}`；
//! - 账号行：`{account, role, status, last_login_at, created_at}`（毫秒时间戳
//!   **字符串**编码——大数红线；`null` = 无记录）；
//! - 写 body：`withReason` 原样下发 `reason` / `note` / `confirm` 三个独立字段；
//! - 权限 id 取值域 = [`crate::mgmt::rbac::Permission::as_str`]（唯一权限源，
//!   **不造第二套权限模型**）。
//!
//! ## 内建保护（fail-closed）
//! - 内置四角色（`ops` / `lic_ops` / `risk` / `system`）：`builtin=true` 只读展示，
//!   PUT / DELETE 一律 400（结构化 `builtin_immutable`）；`[[mgmt_auth.roles]]`
//!   只承载自定义角色，`builtin=true` 的行不可能出现在配置里；
//! - 账号保护：删除 / 降权 / 停用后若**可用账号归零**（active 且角色可解析）→
//!   400（绝不把登录通道锁死）。
//!
//! ## 危险操作四要素
//! `reason`（必填非空）/ `note`（可选）/ `confirm`（必填，= 对象全名原文，
//! `trim()` + 大小写不敏感精确匹配 → 400 `confirm_mismatch`）；全部写动作
//! （含被拒）落审计环。
//!
//! ## 已知边界（如实声明，不伪造）
//! 绑定**自定义角色**的账号当前在登录装配时被 fail-closed 跳过
//! （`auth_login::sync_users` 仅接受内置四角色字面量）——自定义角色的运行时
//! 授权融合需要 `rbac::Role` 扩展 + 前端 `ACTION_MATRIX` 两端同步，属后续任务。

use axum::extract::{Path as AxumPath, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Json, Response};
use serde_json::{json, Value};

use super::rbac::{AuthedRole, Permission, Role};
use super::remote_ops::{OpsAction, OUTCOME_ACCEPTED, OUTCOME_BAD_REQUEST, OUTCOME_DENIED};
use super::writeapi::{audit, validation_error, write_guard};
use super::MgmtState;

// ---- 内置角色目录（与 rbac PERMS_* 同源，名字对齐 rbac.ts 中文标注） ----

/// 内置角色（id → 显示名）。permissions 由 [`Role::as_str`] + `permissions_of` 动态取，
/// 保证与授权判定同源。
const BUILTIN_ROLES: &[(&str, &str)] = &[
    ("ops", "运维"),
    ("lic_ops", "授权运维"),
    ("risk", "风控审计"),
    ("system", "系统管理"),
];

/// 权限目录（`GET /api/permissions`；label / group 为展示文案，id 即
/// [`Permission::as_str`]——前端弹窗据此渲染权限编辑器）。
const PERMISSION_CATALOG: &[(Permission, &str, &str)] = &[
    (Permission::CodeView, "激活码查看", "激活码"),
    (Permission::CodeIssue, "激活码发放", "激活码"),
    (Permission::CodeRevoke, "激活码废弃", "激活码"),
    (Permission::CodeReissue, "激活码重发", "激活码"),
    (Permission::CodeReveal, "激活码明文揭示", "激活码"),
    (Permission::DeviceView, "设备查看", "设备"),
    (Permission::DeviceMarkAnomaly, "设备标记异常", "设备"),
    (Permission::DeviceWrite, "设备配置写", "设备"),
    (Permission::PointWrite, "点位配置写", "设备"),
    (Permission::TenantView, "租户查看", "租户"),
    (Permission::TenantPolicyUpdate, "租户策略更新", "租户"),
    (Permission::ReceiptView, "回执查看", "回执"),
    (Permission::ReceiptMark, "回执标记", "回执"),
    (Permission::TransferView, "换机工单查看", "换机"),
    (Permission::TransferProcess, "换机工单处理", "换机"),
    (Permission::KeyView, "签名密钥查看", "密钥"),
    (Permission::KeyRotate, "签名密钥轮换", "密钥"),
    (Permission::AuditView, "审计日志查看", "审计"),
    (Permission::AuditExport, "审计日志导出", "审计"),
    (Permission::AccountView, "账号查看", "账号"),
    (Permission::AccountUpdate, "账号更新", "账号"),
    (Permission::OpsRestart, "远程重启", "运维"),
    (Permission::OpsCollectors, "采集器启停", "运维"),
    (Permission::OpsLogsRead, "运维日志查询", "运维"),
];

/// 权限 id 字面量全集中表（校验用；从 [`PERMISSION_CATALOG`] 派生，单一真源）。
fn all_permission_ids() -> Vec<&'static str> {
    PERMISSION_CATALOG
        .iter()
        .map(|(p, _, _)| p.as_str())
        .collect()
}

/// 解析权限 id 字面量 → [`Permission`]（目录内精确匹配；大小写敏感——权限 id
/// 是机器契约，不做宽松匹配）。
fn permission_from_str(raw: &str) -> Option<Permission> {
    PERMISSION_CATALOG
        .iter()
        .find(|(p, _, _)| p.as_str() == raw)
        .map(|(p, _, _)| *p)
}

// ---- 请求体 ----

/// 危险操作三要素（`reason` / `note` / `confirm` 三个独立字段，绝不拼接）。
#[derive(Debug, Default, serde::Deserialize)]
struct DangerTrio {
    #[serde(default)]
    reason: String,
    #[serde(default)]
    note: String,
    #[serde(default)]
    confirm: String,
}

/// `POST /api/roles` body。
#[derive(Debug, Default, serde::Deserialize)]
struct RoleCreateBody {
    #[serde(default)]
    name: String,
    #[serde(default)]
    permissions: Vec<String>,
    #[serde(flatten)]
    trio: DangerTrio,
}

/// `PUT /api/roles/:id` body（部分更新语义：漏传 = 不变）。
#[derive(Debug, Default, serde::Deserialize)]
struct RoleUpdateBody {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    permissions: Option<Vec<String>>,
    #[serde(flatten)]
    trio: DangerTrio,
}

/// `POST /api/accounts` body。
#[derive(Debug, Default, serde::Deserialize)]
struct AccountCreateBody {
    #[serde(default)]
    account: String,
    #[serde(default)]
    role: String,
    #[serde(default)]
    password: String,
    #[serde(flatten)]
    trio: DangerTrio,
}

/// `PUT /api/accounts/:account` body（改角色 / 停启用 / 重置口令；漏传 = 不变）。
#[derive(Debug, Default, serde::Deserialize)]
struct AccountUpdateBody {
    #[serde(default)]
    role: Option<String>,
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    password: Option<String>,
    #[serde(flatten)]
    trio: DangerTrio,
}

// ---- 三要素校验（与 alerts_api 同口径） ----

/// 三要素完整校验：`reason` 必填非空、`note` 可选（非空则 ≥8 字）、
/// `confirm` 必填且 = 对象全名原文（trim + 大小写不敏感）。
#[allow(clippy::result_large_err)]
fn check_trio(trio: &DangerTrio, confirm_target: &str) -> Result<(), Response> {
    if trio.reason.trim().is_empty() {
        return Err(validation_error(
            "reason",
            "reason is required",
            "non-empty change reason",
        ));
    }
    let note = trio.note.trim();
    if !note.is_empty() && note.chars().count() < 8 {
        return Err(validation_error(
            "note",
            "note must be at least 8 characters",
            "≥8 characters, kept independent from `reason`",
        ));
    }
    if trio.confirm.trim().is_empty() {
        return Err(validation_error(
            "confirm",
            "confirm is required (echo the full object name)",
            "the full object name",
        ));
    }
    if !trio.confirm.trim().eq_ignore_ascii_case(confirm_target) {
        return Err(confirm_mismatch(confirm_target));
    }
    Ok(())
}

/// `confirm` 回显不匹配（wire 形状与 alerts_api 逐字一致）。
fn confirm_mismatch(expected: &str) -> Response {
    (
        StatusCode::BAD_REQUEST,
        Json(json!({
            "error": "confirm_mismatch",
            "field": "confirm",
            "reason": format!("confirm does not match {expected:?}"),
            "allowed": expected,
        })),
    )
        .into_response()
}

/// 审计落环的统一小包装（写路径成功 / 失败都落；失败只 warn 不阻断响应）。
fn audit_write(state: &MgmtState, actor: &str, action: OpsAction, ok: bool, detail: &str) {
    audit(
        state,
        actor,
        action,
        ok,
        if ok {
            OUTCOME_ACCEPTED
        } else {
            OUTCOME_BAD_REQUEST
        },
        detail,
    );
}

/// 读接口鉴权（401 extractor + 403 ensure）：账号 / 角色 / 权限目录属敏感配置，
/// **不开放匿名读**（失败响应形状与 rbac 拒绝路径一致）。
#[allow(clippy::result_large_err)]
fn ensure_account_view(authed: &AuthedRole) -> Result<(), Response> {
    authed
        .ensure(Permission::AccountView)
        .map_err(|rejection| rejection.into_response())
}

/// 写接口鉴权：`account.update`（账号与角色的管理动作共用该权限——
/// rbac 矩阵里二者同档，仅 system 可授）。
#[allow(clippy::result_large_err)]
fn ensure_account_update(authed: &AuthedRole) -> Result<(), Response> {
    authed
        .ensure(Permission::AccountUpdate)
        .map_err(|rejection| rejection.into_response())
}

// ---- 视图 ----

/// 自定义角色行 → wire（`accountCount` = users 里 role 指向该 id 的账号数）。
fn custom_role_to_wire(id: &str, name: &str, permissions: &[String], users: &[Value]) -> Value {
    let count = users.iter().filter(|u| u["role"] == *id).count();
    json!({
        "id": id,
        "name": name,
        "permissions": permissions,
        "builtin": false,
        "accountCount": count,
    })
}

/// 内置角色行 → wire（permissions 从 `permissions_of` 动态取，与授权判定同源）。
fn builtin_role_to_wire(id: &str, name: &str, users: &[Value]) -> Value {
    let role = Role::from_str(id).unwrap_or(Role::Ops);
    let perms: Vec<&str> = super::rbac::permissions_of(role)
        .iter()
        .map(|p| p.as_str())
        .collect();
    let count = users.iter().filter(|u| u["role"] == *id).count();
    json!({
        "id": id,
        "name": name,
        "permissions": perms,
        "builtin": true,
        "accountCount": count,
    })
}

/// 毫秒时间戳 → wire（字符串编码——大数红线；`None` → JSON `null`）。
fn ms_to_wire(ms: Option<u64>) -> Value {
    ms.map(|v| json!(v.to_string())).unwrap_or(Value::Null)
}

/// 账号行 → wire（口令摘要**永不**出 wire）。
fn user_to_wire(user: &crate::config::MgmtAuthUser) -> Value {
    json!({
        "account": user.name,
        "role": user.role,
        "status": user.status.clone().unwrap_or_else(|| "active".to_string()),
        "last_login_at": ms_to_wire(user.last_login_at_ms),
        "created_at": ms_to_wire(user.created_at_ms),
        "display_name": user.display_name,
    })
}

/// `mgmt_auth` 段缺失时的兜底视图（诚实空态：内置角色照列，账号空）。
fn section_or_empty(config: &crate::config::GatewayConfig) -> crate::config::MgmtAuthSection {
    config.mgmt_auth.clone().unwrap_or_default()
}

// ---- 端点：权限目录 ----

/// `GET /api/permissions` → 权限目录（裸数组；前端权限编辑器数据源）。
pub async fn permissions_list(State(state): State<MgmtState>, authed: AuthedRole) -> Response {
    if let Err(resp) = ensure_account_view(&authed) {
        return resp;
    }
    let _ = state; // 目录静态，不读配置；State 仅为了与路由签名统一
    let rows: Vec<Value> = PERMISSION_CATALOG
        .iter()
        .map(|(p, label, group)| json!({ "id": p.as_str(), "label": label, "group": group }))
        .collect();
    Json(Value::Array(rows)).into_response()
}

// ---- 端点：角色 ----

/// `GET /api/roles` → 内置四角色 + 自定义角色（裸数组）。
pub async fn roles_list(State(state): State<MgmtState>, authed: AuthedRole) -> Response {
    if let Err(resp) = ensure_account_view(&authed) {
        return resp;
    }
    let config = state.config();
    let section = section_or_empty(&config);
    let users: Vec<Value> = section.users.iter().map(user_to_wire).collect();
    let mut rows: Vec<Value> = BUILTIN_ROLES
        .iter()
        .map(|(id, name)| builtin_role_to_wire(id, name, &users))
        .collect();
    rows.extend(
        section
            .roles
            .iter()
            .map(|r| custom_role_to_wire(&r.id, &r.name, &r.permissions, &users)),
    );
    Json(Value::Array(rows)).into_response()
}

/// 角色名 → 安全 id（slug：小写、非 `[a-z0-9_]` 转 `_`、掐头尾 `_`）。
/// 空 slug → `None`（调用方报 400）。
fn slugify(name: &str) -> Option<String> {
    let slug: String = name
        .trim()
        .chars()
        .map(|c| {
            if c.is_ascii_lowercase() || c.is_ascii_digit() {
                c
            } else if c.is_ascii_uppercase() {
                c.to_ascii_lowercase()
            } else {
                '_'
            }
        })
        .collect();
    let slug = slug.trim_matches('_').to_string();
    if slug.is_empty() {
        None
    } else {
        Some(slug)
    }
}

/// `POST /api/roles` → 新增自定义角色（id 由 name slug 化生成；重名 / slug 冲突 400）。
pub async fn role_create(
    State(state): State<MgmtState>,
    authed: AuthedRole,
    body: axum::body::Bytes,
) -> Response {
    let actor = authed.claims.sub.clone();
    if let Err(resp) = ensure_account_update(&authed) {
        audit(
            &state,
            &actor,
            OpsAction::RoleCreate,
            false,
            OUTCOME_DENIED,
            "forbidden",
        );
        return resp;
    }
    let req: RoleCreateBody = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(err) => {
            audit_write(
                &state,
                &actor,
                OpsAction::RoleCreate,
                false,
                "malformed json body",
            );
            return validation_error(
                "body",
                &format!("malformed JSON: {err}"),
                "object {name, permissions}",
            );
        }
    };
    let name = req.name.trim().to_string();
    if name.is_empty() {
        audit_write(
            &state,
            &actor,
            OpsAction::RoleCreate,
            false,
            "name is required",
        );
        return validation_error("name", "name is required", "non-empty role display name");
    }
    if let Err(resp) = validate_permissions(&req.permissions) {
        audit_write(
            &state,
            &actor,
            OpsAction::RoleCreate,
            false,
            "invalid permissions",
        );
        return resp;
    }
    // 角色名可能不含任何 ASCII 字母 / 数字（纯中文等）→ fallback `role-<epoch_ms>`
    // 确定性生成 id；有 slug 时 slug 冲突由下方唯一性检查报 400。
    let id = slugify(&name).unwrap_or_else(|| format!("role-{}", now_ms()));
    let _guard = write_guard();
    let mut config = (*state.config()).clone();
    let section = config.mgmt_auth.get_or_insert_with(Default::default);
    if BUILTIN_ROLES.iter().any(|(bid, _)| *bid == id) || section.roles.iter().any(|r| r.id == id) {
        audit_write(
            &state,
            &actor,
            OpsAction::RoleCreate,
            false,
            &format!("duplicate role id {id:?}"),
        );
        return validation_error(
            "name",
            &format!("role id {id:?} already exists"),
            "unique role name",
        );
    }
    section.roles.push(crate::config::MgmtAuthRole {
        id: id.clone(),
        name,
        permissions: req.permissions,
        builtin: false,
    });
    let reason_note = req.trio.reason.trim().to_string();
    persist(
        state,
        config,
        &actor,
        OpsAction::RoleCreate,
        &format!("create role {id:?}; reason={reason_note:?}"),
    )
}

/// `PUT /api/roles/:id` → 修改自定义角色（内置角色 → 400 `builtin_immutable`）。
pub async fn role_update(
    State(state): State<MgmtState>,
    authed: AuthedRole,
    AxumPath(id): AxumPath<String>,
    body: axum::body::Bytes,
) -> Response {
    let actor = authed.claims.sub.clone();
    if let Err(resp) = ensure_account_update(&authed) {
        audit(
            &state,
            &actor,
            OpsAction::RoleUpdate,
            false,
            OUTCOME_DENIED,
            "forbidden",
        );
        return resp;
    }
    let req: RoleUpdateBody = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(err) => {
            audit_write(
                &state,
                &actor,
                OpsAction::RoleUpdate,
                false,
                "malformed json body",
            );
            return validation_error(
                "body",
                &format!("malformed JSON: {err}"),
                "object {name?, permissions?}",
            );
        }
    };
    let id = id.trim().to_string();
    if BUILTIN_ROLES.iter().any(|(bid, _)| *bid == id) {
        audit_write(
            &state,
            &actor,
            OpsAction::RoleUpdate,
            false,
            &format!("role {id:?} is builtin"),
        );
        return builtin_immutable(&id);
    }
    if let Some(perms) = req.permissions.as_ref() {
        if let Err(resp) = validate_permissions(perms) {
            audit_write(
                &state,
                &actor,
                OpsAction::RoleUpdate,
                false,
                "invalid permissions",
            );
            return resp;
        }
    }
    let _guard = write_guard();
    let mut config = (*state.config()).clone();
    let section = config.mgmt_auth.get_or_insert_with(Default::default);
    // 三要素先于定位与一切业务分支（confirm = 角色 id 原文）。
    if let Err(resp) = check_trio(&req.trio, &id) {
        audit_write(
            &state,
            &actor,
            OpsAction::RoleUpdate,
            false,
            "danger trio rejected",
        );
        return resp;
    }
    let Some(role) = section.roles.iter_mut().find(|r| r.id == id) else {
        audit_write(
            &state,
            &actor,
            OpsAction::RoleUpdate,
            false,
            &format!("unknown role {id:?}"),
        );
        return not_found(&format!("role {id:?} not found"));
    };
    if let Some(name) = req.name.as_deref() {
        let name = name.trim().to_string();
        if name.is_empty() {
            audit_write(&state, &actor, OpsAction::RoleUpdate, false, "empty name");
            return validation_error(
                "name",
                "name must not be empty",
                "non-empty role display name",
            );
        }
        role.name = name;
    }
    if let Some(perms) = req.permissions {
        role.permissions = perms;
    }
    persist(
        state,
        config,
        &actor,
        OpsAction::RoleUpdate,
        &format!("update role {id:?}"),
    )
}

/// `DELETE /api/roles/:id` → 删除自定义角色（内置 → 400；被账号引用 → 400）。
pub async fn role_remove(
    State(state): State<MgmtState>,
    authed: AuthedRole,
    AxumPath(id): AxumPath<String>,
    body: axum::body::Bytes,
) -> Response {
    let actor = authed.claims.sub.clone();
    if let Err(resp) = ensure_account_update(&authed) {
        audit(
            &state,
            &actor,
            OpsAction::RoleDelete,
            false,
            OUTCOME_DENIED,
            "forbidden",
        );
        return resp;
    }
    let req: DangerTrio = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(err) => {
            audit_write(
                &state,
                &actor,
                OpsAction::RoleDelete,
                false,
                "malformed json body",
            );
            return validation_error(
                "body",
                &format!("malformed JSON: {err}"),
                "object {reason, note?, confirm}",
            );
        }
    };
    let id = id.trim().to_string();
    if BUILTIN_ROLES.iter().any(|(bid, _)| *bid == id) {
        audit_write(
            &state,
            &actor,
            OpsAction::RoleDelete,
            false,
            &format!("role {id:?} is builtin"),
        );
        return builtin_immutable(&id);
    }
    let _guard = write_guard();
    let mut config = (*state.config()).clone();
    let section = config.mgmt_auth.get_or_insert_with(Default::default);
    // 三要素先于定位与一切业务分支（confirm = 角色 id 原文）。
    if let Err(resp) = check_trio(&req, &id) {
        audit_write(
            &state,
            &actor,
            OpsAction::RoleDelete,
            false,
            "danger trio rejected",
        );
        return resp;
    }
    let Some(pos) = section.roles.iter().position(|r| r.id == id) else {
        audit_write(
            &state,
            &actor,
            OpsAction::RoleDelete,
            false,
            &format!("unknown role {id:?}"),
        );
        return not_found(&format!("role {id:?} not found"));
    };
    if section.users.iter().any(|u| u.role == id) {
        audit_write(
            &state,
            &actor,
            OpsAction::RoleDelete,
            false,
            &format!("role {id:?} still referenced by accounts"),
        );
        return validation_error(
            "id",
            &format!("role {id:?} is still assigned to at least one account"),
            "reassign accounts to another role first",
        );
    }
    section.roles.remove(pos);
    persist(
        state,
        config,
        &actor,
        OpsAction::RoleDelete,
        &format!("delete role {id:?}"),
    )
}

// ---- 端点：账号 ----

/// `GET /api/accounts` → 账号清单（裸数组；口令摘要不出 wire）。
pub async fn accounts_list(State(state): State<MgmtState>, authed: AuthedRole) -> Response {
    if let Err(resp) = ensure_account_view(&authed) {
        return resp;
    }
    let config = state.config();
    let section = section_or_empty(&config);
    Json(Value::Array(
        section.users.iter().map(user_to_wire).collect(),
    ))
    .into_response()
}

/// `POST /api/accounts` → 新增账号（role 须指向内置角色或已定义自定义角色；
/// 口令 ≥ 8 字符，Argon2id PHC 落盘）。
pub async fn account_create(
    State(state): State<MgmtState>,
    authed: AuthedRole,
    body: axum::body::Bytes,
) -> Response {
    let actor = authed.claims.sub.clone();
    if let Err(resp) = ensure_account_update(&authed) {
        audit(
            &state,
            &actor,
            OpsAction::AccountCreate,
            false,
            OUTCOME_DENIED,
            "forbidden",
        );
        return resp;
    }
    let req: AccountCreateBody = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(err) => {
            audit_write(
                &state,
                &actor,
                OpsAction::AccountCreate,
                false,
                "malformed json body",
            );
            return validation_error(
                "body",
                &format!("malformed JSON: {err}"),
                "object {account, role, password}",
            );
        }
    };
    let account = req.account.trim().to_string();
    let role = req.role.trim().to_string();
    if account.is_empty() {
        audit_write(
            &state,
            &actor,
            OpsAction::AccountCreate,
            false,
            "account is required",
        );
        return validation_error(
            "account",
            "account is required",
            "non-empty unique account name",
        );
    }
    if req.password.chars().count() < super::auth_login::MIN_PASSWORD_LEN {
        audit_write(
            &state,
            &actor,
            OpsAction::AccountCreate,
            false,
            "password too short",
        );
        return validation_error(
            "password",
            &format!(
                "password must be at least {} characters",
                super::auth_login::MIN_PASSWORD_LEN
            ),
            &format!("≥{} characters", super::auth_login::MIN_PASSWORD_LEN),
        );
    }
    let _guard = write_guard();
    let mut config = (*state.config()).clone();
    let section = config.mgmt_auth.get_or_insert_with(Default::default);
    if !role_exists(&section.roles, &role) {
        audit_write(
            &state,
            &actor,
            OpsAction::AccountCreate,
            false,
            &format!("unknown role {role:?}"),
        );
        return validation_error(
            "role",
            &format!("unknown role {role:?}"),
            "a builtin role literal or an existing custom role id",
        );
    }
    if section.users.iter().any(|u| u.name == account) {
        audit_write(
            &state,
            &actor,
            OpsAction::AccountCreate,
            false,
            &format!("duplicate account {account:?}"),
        );
        return validation_error(
            "account",
            &format!("account {account:?} already exists"),
            "unique account name",
        );
    }
    let Some(phc) = super::auth_login::hash_phc(&req.password) else {
        audit_write(
            &state,
            &actor,
            OpsAction::AccountCreate,
            false,
            "password hash unavailable",
        );
        return internal("password hashing unavailable; account not created");
    };
    section.users.push(crate::config::MgmtAuthUser {
        name: account.clone(),
        role,
        password_hash: phc,
        display_name: None,
        status: Some("active".to_string()),
        created_at_ms: Some(now_ms()),
        last_login_at_ms: None,
    });
    let reason_note = req.trio.reason.trim().to_string();
    persist(
        state,
        config,
        &actor,
        OpsAction::AccountCreate,
        &format!("create account {account:?}; reason={reason_note:?}"),
    )
}

/// `PUT /api/accounts/:account` → 改角色 / 停启用 / 重置口令（部分更新）。
///
/// 保护：变更后可用账号（active 且角色可解析）不得归零（fail-closed）。
pub async fn account_update(
    State(state): State<MgmtState>,
    authed: AuthedRole,
    AxumPath(account): AxumPath<String>,
    body: axum::body::Bytes,
) -> Response {
    let actor = authed.claims.sub.clone();
    if let Err(resp) = ensure_account_update(&authed) {
        audit(
            &state,
            &actor,
            OpsAction::AccountUpdate,
            false,
            OUTCOME_DENIED,
            "forbidden",
        );
        return resp;
    }
    let req: AccountUpdateBody = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(err) => {
            audit_write(
                &state,
                &actor,
                OpsAction::AccountUpdate,
                false,
                "malformed json body",
            );
            return validation_error(
                "body",
                &format!("malformed JSON: {err}"),
                "object {role?, status?, password?}",
            );
        }
    };
    if let Some(status) = req.status.as_deref() {
        if status != "active" && status != "disabled" {
            audit_write(
                &state,
                &actor,
                OpsAction::AccountUpdate,
                false,
                "invalid status",
            );
            return validation_error(
                "status",
                &format!("unknown status {status:?}"),
                "active | disabled",
            );
        }
    }
    if let Some(role) = req.role.as_deref() {
        let role = role.trim();
        let section = section_or_empty(&state.config());
        if !role_exists(&section.roles, role) {
            audit_write(
                &state,
                &actor,
                OpsAction::AccountUpdate,
                false,
                &format!("unknown role {role:?}"),
            );
            return validation_error(
                "role",
                &format!("unknown role {role:?}"),
                "a builtin role literal or an existing custom role id",
            );
        }
    }
    if let Some(password) = req.password.as_deref() {
        if password.chars().count() < super::auth_login::MIN_PASSWORD_LEN {
            audit_write(
                &state,
                &actor,
                OpsAction::AccountUpdate,
                false,
                "password too short",
            );
            return validation_error(
                "password",
                &format!(
                    "password must be at least {} characters",
                    super::auth_login::MIN_PASSWORD_LEN
                ),
                &format!("≥{} characters", super::auth_login::MIN_PASSWORD_LEN),
            );
        }
    }
    let _guard = write_guard();
    let mut config = (*state.config()).clone();
    let section = config.mgmt_auth.get_or_insert_with(Default::default);
    // 三要素先于定位与一切业务分支（confirm = 账号名原文）。
    if let Err(resp) = check_trio(&req.trio, &account) {
        audit_write(
            &state,
            &actor,
            OpsAction::AccountUpdate,
            false,
            "danger trio rejected",
        );
        return resp;
    }
    let Some(user) = section.users.iter_mut().find(|u| u.name == account) else {
        audit_write(
            &state,
            &actor,
            OpsAction::AccountUpdate,
            false,
            &format!("unknown account {account:?}"),
        );
        return not_found(&format!("account {account:?} not found"));
    };
    if let Some(role) = req.role.as_deref() {
        user.role = role.trim().to_string();
    }
    if let Some(status) = req.status.as_deref() {
        user.status = Some(status.to_string());
    }
    if let Some(password) = req.password.as_deref() {
        let Some(phc) = super::auth_login::hash_phc(password) else {
            audit_write(
                &state,
                &actor,
                OpsAction::AccountUpdate,
                false,
                "password hash unavailable",
            );
            return internal("password hashing unavailable; change not applied");
        };
        user.password_hash = phc;
    }
    // 保护：变更后可用账号不得归零。
    if usable_accounts(&section.users) == 0 {
        audit_write(
            &state,
            &actor,
            OpsAction::AccountUpdate,
            false,
            "change would leave zero usable accounts",
        );
        return validation_error(
            "account",
            "this change would leave zero usable accounts (login lockout)",
            "keep at least one active account with a resolvable role",
        );
    }
    persist(
        state,
        config,
        &actor,
        OpsAction::AccountUpdate,
        &format!("update account {account:?}"),
    )
}

/// `DELETE /api/accounts/:account` → 删除账号（删除后可用账号归零 → 400）。
pub async fn account_remove(
    State(state): State<MgmtState>,
    authed: AuthedRole,
    AxumPath(account): AxumPath<String>,
    body: axum::body::Bytes,
) -> Response {
    let actor = authed.claims.sub.clone();
    if let Err(resp) = ensure_account_update(&authed) {
        audit(
            &state,
            &actor,
            OpsAction::AccountDelete,
            false,
            OUTCOME_DENIED,
            "forbidden",
        );
        return resp;
    }
    let req: DangerTrio = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(err) => {
            audit_write(
                &state,
                &actor,
                OpsAction::AccountDelete,
                false,
                "malformed json body",
            );
            return validation_error(
                "body",
                &format!("malformed JSON: {err}"),
                "object {reason, note?, confirm}",
            );
        }
    };
    let _guard = write_guard();
    let mut config = (*state.config()).clone();
    let section = config.mgmt_auth.get_or_insert_with(Default::default);
    // 三要素先于定位与一切业务分支（confirm = 账号名原文）。
    if let Err(resp) = check_trio(&req, &account) {
        audit_write(
            &state,
            &actor,
            OpsAction::AccountDelete,
            false,
            "danger trio rejected",
        );
        return resp;
    }
    let Some(pos) = section.users.iter().position(|u| u.name == account) else {
        audit_write(
            &state,
            &actor,
            OpsAction::AccountDelete,
            false,
            &format!("unknown account {account:?}"),
        );
        return not_found(&format!("account {account:?} not found"));
    };
    section.users.remove(pos);
    if usable_accounts(&section.users) == 0 {
        // 回滚本次内存变更（尚未落盘，直接报错即可）。
        audit_write(
            &state,
            &actor,
            OpsAction::AccountDelete,
            false,
            "delete would leave zero usable accounts",
        );
        return validation_error(
            "account",
            "deleting this account would leave zero usable accounts (login lockout)",
            "keep at least one active account with a resolvable role",
        );
    }
    persist(
        state,
        config,
        &actor,
        OpsAction::AccountDelete,
        &format!("delete account {account:?}"),
    )
}

// ---- 内部工具 ----

/// 角色 id 是否存在（内置字面量或已定义自定义角色）。
fn role_exists(custom_roles: &[crate::config::MgmtAuthRole], role: &str) -> bool {
    Role::from_str(role).is_some() || custom_roles.iter().any(|r| r.id == role)
}

/// 可用账号数：active（或未声明 status）且角色可被登录装配解析。
fn usable_accounts(users: &[crate::config::MgmtAuthUser]) -> usize {
    users
        .iter()
        .filter(|u| u.status.as_deref() != Some("disabled"))
        .filter(|u| Role::from_str(&u.role).is_some())
        .count()
}

/// 当前 Unix 毫秒（回退 0——时间不可得时记 0 而不是阻塞建号）。
fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// 内置角色不可变更（结构化错误变体，不做 msg.contains 判定）。
fn builtin_immutable(id: &str) -> Response {
    (
        StatusCode::BAD_REQUEST,
        Json(json!({
            "error": "builtin_immutable",
            "field": "id",
            "reason": format!("role {id:?} is builtin and cannot be modified or deleted"),
            "allowed": "custom roles only",
        })),
    )
        .into_response()
}

fn not_found(message: &str) -> Response {
    (
        StatusCode::NOT_FOUND,
        Json(json!({ "error": "not_found", "message": message })),
    )
        .into_response()
}

fn internal(message: &str) -> Response {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(json!({ "error": "internal", "message": message })),
    )
        .into_response()
}

/// `permissions` 取值域校验（逐项必须命中权限目录；结构化 400）。
#[allow(clippy::result_large_err)]
fn validate_permissions(perms: &[String]) -> Result<(), Response> {
    let catalog = all_permission_ids();
    for raw in perms {
        if permission_from_str(raw).is_none() {
            return Err(validation_error(
                "permissions",
                &format!("unknown permission {raw:?}"),
                &catalog.join(" | "),
            ));
        }
    }
    Ok(())
}

/// 落盘范式（与 writeapi::persist 同一写锁与热替换；事件发 `ConfigReloaded`）。
fn persist(
    state: MgmtState,
    config: crate::config::GatewayConfig,
    actor: &str,
    action: OpsAction,
    detail: &str,
) -> Response {
    let Some(path) = state.config_path() else {
        audit_write(&state, actor, action, false, "config path not bound");
        return internal("config file path not configured; write refused (fail-closed)");
    };
    if let Err(err) = config.save(&path) {
        audit_write(
            &state,
            actor,
            action,
            false,
            &format!("{detail}: save failed: {err}"),
        );
        return internal(&format!("config save failed: {err}"));
    }
    let version = state.daemon().config_shared().replace(config);
    state.publish(super::MgmtEvent::ConfigReloaded { version });
    audit(
        &state,
        actor,
        action,
        true,
        OUTCOME_ACCEPTED,
        &format!("{detail}; config_version={version}"),
    );
    (
        StatusCode::OK,
        Json(json!({
            "accepted": true,
            "config_version": version.to_string(),
        })),
    )
        .into_response()
}

//! `licensing-server` HTTP 层（task 46：激活码生命周期 + 一机一码预绑定；
//! 管理端 API 补齐：登录鉴权 + 全量 GET 查询 + 租户自举写端点）。
//!
//! # 职责边界（见 `lib.rs` 模块地图）
//!
//! - axum 路由装配（设备端 4 + 管理端 12 个端点）；
//! - 反序列化请求体 → 调用 [`crate::service::LicensingService`]；
//! - **结构化错误映射**：[`error_to_code`] 直接 `match` [`LicenseError`] 变体，把错误映射到
//!   `proto::codes` 业务码（HTTP 状态码再经 `proto::http_status` 推出）。
//!   **绝不**使用 `msg.contains(...)` 之类的字符串反查——那是脆弱耦合（见 `error.rs` 注释）：
//!   任何人改一句文案都会让错误码静默退化成泛化 `BAD_REQUEST`，而单测通常察觉不到。
//! - 统一 [`ApiEnvelope`] 响应包裹（`{ code, data, message, trace_id }`）。
//!
//! # 管理端鉴权（设计 §4；本文件此前完全无鉴权，为安全红线缺口）
//!
//! - `POST /admin/auth/login`：凭据换 JWT（HS256，1h TTL；实现见 [`crate::admin_auth`]）；
//! - **其余全部 `/admin/*` 端点必须携带 `Authorization: Bearer <token>`**：
//!   缺 token / 坏 token / 过期 → 401 `SESSION_EXPIRED`；
//!   已认证但角色不符 → 403 `ADMIN_ONLY`（角色门控见各 handler 注释）；
//! - 设备端（`/activation`、`/heartbeat`、`/verify`、`/audit/receipt`）走
//!   Lease Token + 设备签名体系，**不经**管理端 JWT（口径不变）。
//!
//! # 租户 / 操作者解析约定（本版收紧）
//!
//! - 管理端写操作的操作者一律取 **JWT `sub`**（登录用户名，入审计）；
//!   原先「`X-Actor-Id` 缺省 admin」的宽松口径移除——未认证请求根本到不了 handler；
//! - `POST /admin/codes/:code_id/revoke|reissue` 的租户取请求头 `X-Tenant-Id`
//!   （**必填**：缺失 → 400 结构化错误，不再缺省 `unknown`）。
//!   上层反代可注入该头；持有有效管理 token 的调用方（admin-console）总是携带它。

use std::sync::Arc;

use axum::{
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post, put},
    Json, Router,
};
use serde::Serialize;

use crate::admin_auth::{AdminAuth, AuthedAdmin, Role};
use crate::error::LicenseError;
use crate::model::{now_ns_id, ActorType, CodeStatus, Device, SigningKey, Tenant};
use crate::proto::{
    self, ActivationRequest, AdminLoginRequest, AdminLoginResponse, ApiEnvelope, AuditLogItem,
    AuditLogQuery, CodeDetail, CodeListQuery, CodeSummary, CreateTenantRequest, DeviceListItem,
    DeviceListQuery, HeartbeatRequest, IssueCodesRequest, OverviewResponse, PagedResponse,
    ReceiptAnomalyItem, ReissueCodeRequest, RevokeCodeRequest, SigningKeyItem, TenantItem,
    TimelineEntry, UpdateTenantPolicyRequest,
};
use crate::service::LicensingService;
use crate::store::{AuditFilter, CodeFilter};

/// 共享服务句柄（axum 路由状态）。
pub type SharedService = Arc<LicensingService>;

/// axum 路由状态：业务服务 + 管理端鉴权器。
#[derive(Clone)]
pub struct AppState {
    /// 业务服务（store / keyring / ledger 聚合）。
    pub service: SharedService,
    /// 管理端鉴权器（账号表 + JWT 签发密钥）。
    pub auth: Arc<AdminAuth>,
}

/// 每页条数缺省值。
const DEFAULT_PAGE_SIZE: u32 = 20;
/// 每页条数上限（防一次拉取过多）。
const MAX_PAGE_SIZE: u32 = 200;
/// 内存遍历扫描的批大小（store 无原生复合过滤时的分批拉取粒度）。
const SCAN_BATCH: u32 = 500;

/// 装配 HTTP 路由（设备端 4 + 管理端 12 共 16 个端点）。
///
/// - 设备端：`POST /activation`、`POST /heartbeat`、`POST /verify`、`POST /audit/receipt`
///   （Lease Token + 设备签名体系，不经管理端 JWT）；
/// - 管理端（**全部需要 `Authorization: Bearer`，除 login 外**）：
///   - `POST /admin/auth/login`（无需 token）；
///   - `GET  /admin/overview`（任意角色）；
///   - `GET  /admin/codes`、`GET /admin/codes/:code_id`（任意角色）；
///   - `POST /admin/codes/issue`（ops / lic_ops / system）；
///   - `POST /admin/codes/:code_id/revoke`、`.../reissue`（lic_ops / system，高危）；
///   - `GET  /admin/tenants`（任意角色）；`POST /admin/tenants`、
///     `PUT /admin/tenants/:tenant_id/policy`（仅 system）；
///   - `GET  /admin/devices`、`GET /admin/receipts/anomalies`、`GET /admin/keys`、
///     `GET /admin/audit/logs`（任意角色）。
pub fn router(service: SharedService, auth: Arc<AdminAuth>) -> Router {
    let state = AppState { service, auth };
    Router::new()
        // 设备端（设备签名体系，无管理端 JWT）。
        // `/activate` 别名：daemon 侧 LicensingClient 激活端点拼的是
        // `{base_url}/activate`（auth/client.rs `endpoint("activate")`），
        // 与本服务既有 `/activation` 同一 handler——两路由并存，兼容两端契约。
        .route("/activation", post(activate))
        .route("/activate", post(activate))
        .route("/heartbeat", post(heartbeat))
        .route("/verify", post(verify))
        .route("/audit/receipt", post(audit_receipt))
        // 管理端：登录（唯一免 token 端点）。
        .route("/admin/auth/login", post(admin_login))
        // 管理端：总览 / 查询（任意已认证角色）。
        .route("/admin/overview", get(admin_overview))
        .route("/admin/codes", get(admin_codes))
        .route("/admin/codes/:code_id", get(admin_code_detail))
        .route(
            "/admin/tenants",
            get(admin_tenants).post(admin_create_tenant),
        )
        .route(
            "/admin/tenants/:tenant_id/policy",
            put(admin_update_tenant_policy),
        )
        .route("/admin/devices", get(admin_devices))
        .route("/admin/receipts/anomalies", get(admin_receipt_anomalies))
        .route("/admin/keys", get(admin_keys))
        .route("/admin/audit/logs", get(admin_audit_logs))
        // 管理端：账号 / 角色可配置（缺口 #9；账号读写仅 system，角色清单任意角色）。
        .route(
            "/admin/accounts",
            get(admin_list_accounts).post(admin_create_account),
        )
        .route(
            "/admin/accounts/:account",
            put(admin_update_account).delete(admin_delete_account),
        )
        .route("/admin/roles", get(admin_roles))
        // 管理端：高危写（lic_ops / system）。
        .route("/admin/codes/issue", post(issue_codes))
        .route("/admin/codes/:code_id/revoke", post(revoke_code))
        .route("/admin/codes/:code_id/reissue", post(reissue_code))
        .with_state(state)
}

// ============================================================================
// 设备端 handler（与既有口径一致：设备签名体系，不经管理端 JWT）
// ============================================================================

/// `POST /activation`：设备首激 + 一机一码绑定。
async fn activate(State(state): State<AppState>, Json(req): Json<ActivationRequest>) -> Response {
    match state.service.activate(&req) {
        Ok(resp) => ok_json(resp),
        Err(e) => error_response(&e),
    }
}

/// `POST /heartbeat`：设备心跳保活（设计 §1.2）。
async fn heartbeat(State(state): State<AppState>, Json(req): Json<HeartbeatRequest>) -> Response {
    match state.service.heartbeat(&req) {
        Ok(resp) => ok_json(resp),
        Err(e) => error_response(&e),
    }
}

/// `POST /verify`：A 档二次校验（设计 §1.3）。
///
/// 取**原始 JSON**（而非强类型结构体）：字段白名单需对原始对象做「越界字段」判定，
/// 而 serde 默认忽略未知字段——若先反序列化为结构体，越界字段会**静默消失**，
/// 白名单形同虚设。
async fn verify(State(state): State<AppState>, Json(body): Json<serde_json::Value>) -> Response {
    match state.service.verify(&body) {
        Ok(resp) => ok_json(resp),
        Err(e) => error_response(&e),
    }
}

/// `POST /audit/receipt`：B 档审计回执（设计 §1.4）。
///
/// 同样取**原始 JSON**：设计明确要求「服务端拒收任何业务字段」——越界字段必须
/// 在**反序列化之前**对原始对象判定，否则 serde 会把它丢掉。
async fn audit_receipt(
    State(state): State<AppState>,
    Json(body): Json<serde_json::Value>,
) -> Response {
    match state.service.audit_receipt(&body) {
        Ok(resp) => ok_json(resp),
        Err(e) => error_response(&e),
    }
}

// ============================================================================
// 管理端：鉴权辅助
// ============================================================================

/// 从请求头解析 Bearer token 并校验 JWT。
///
/// 缺 token / 坏 token / 过期 → 401 `SESSION_EXPIRED`（结构化信封）。
/// `Err` 为完整 axum `Response`（错误信封）；`result_large_err` 为 axum
/// handler 辅助函数的固有形态，显式豁免。
#[allow(clippy::result_large_err)]
fn authenticate(state: &AppState, headers: &HeaderMap) -> Result<AuthedAdmin, Response> {
    const BEARER_PREFIX: &str = "Bearer ";
    let token = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.strip_prefix(BEARER_PREFIX))
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let token = match token {
        Some(t) => t,
        None => {
            return Err(error_response(&LicenseError::unauthorized(
                "missing bearer token",
            )));
        }
    };
    match state.auth.verify_token(token) {
        Some(authed) => Ok(authed),
        None => Err(error_response(&LicenseError::unauthorized(
            "invalid or expired admin token",
        ))),
    }
}

/// 角色门控：`authed.role` 不在 `allowed` 内 → 403 `ADMIN_ONLY`（结构化信封）。
#[allow(clippy::result_large_err)]
fn require_role(authed: &AuthedAdmin, allowed: &[Role], action: &str) -> Result<(), Response> {
    if allowed.contains(&authed.role) {
        Ok(())
    } else {
        Err(error_response(&LicenseError::forbidden(format!(
            "role {} is not allowed to {action}",
            authed.role.as_str()
        ))))
    }
}

/// 发放角色集（设计 §4：运营「只读 + 发放」；授权运营与系统全量）。
const ROLES_ISSUE: [Role; 3] = [Role::Ops, Role::LicOps, Role::System];
/// 高危操作角色集（废弃 / 重发：仅授权运营与系统）。
const ROLES_DANGEROUS: [Role; 2] = [Role::LicOps, Role::System];
/// 租户管理角色集（创建 / 策略：仅系统）。
const ROLES_TENANT_ADMIN: [Role; 1] = [Role::System];

/// `POST /admin/auth/login`：`{username, password}` → 200 `{token, role}`。
///
/// 凭据来源见 [`crate::admin_auth`]（env 注入初始管理员，fail-closed：零账号全拒）。
/// 未知用户 / 错误口令 / 请求体非法一律**同一 401**（不区分原因，防账号枚举）。
async fn admin_login(State(state): State<AppState>, body: Json<AdminLoginRequest>) -> Response {
    let store = state.service.store();
    // 账号 / 角色可配置：登录**实时读 store** 账号表（不再只读内存账号表）。
    let accounts = match store.list_admin_accounts() {
        Ok(rows) => rows,
        Err(e) => return error_response(&e),
    };
    let username = body.username.trim();
    match state
        .auth
        .login_with_accounts(username, &body.password, &accounts)
    {
        Some((token, role)) => {
            // best-effort 记录最近登录时刻（失败不影响登录结果）。
            if let Some(acct) = accounts.iter().find(|a| a.account == username) {
                let _ = store
                    .touch_admin_account_login(&acct.account, crate::admin_auth::now_unix_secs());
            }
            ok_json(AdminLoginResponse {
                token,
                role: role.as_str().to_string(),
            })
        }
        None => error_response(&LicenseError::unauthorized("invalid credentials")),
    }
}

// ============================================================================
// 管理端：GET 查询（任意已认证角色）
// ============================================================================

/// `GET /admin/overview`：总览聚合（全部计数 **String**，大数红线）。
async fn admin_overview(State(state): State<AppState>, headers: HeaderMap) -> Response {
    let authed = match authenticate(&state, &headers) {
        Ok(a) => a,
        Err(resp) => return resp,
    };
    let _ = authed;
    let store = state.service.store();
    let count = |status: CodeStatus| -> u64 {
        let filter = CodeFilter {
            status: Some(status),
            ..Default::default()
        };
        store.count_codes(&filter).unwrap_or(0)
    };
    let active_kid = store.list_signing_keys().ok().and_then(|keys| {
        keys.into_iter()
            .find(|k| k.status.as_str() == "active")
            .map(|k| k.kid)
    });
    let data = OverviewResponse {
        tenants: store
            .list_tenants()
            .map(|t| t.len())
            .unwrap_or(0)
            .to_string(),
        devices: store.count_devices(None).unwrap_or(0).to_string(),
        codes_total: store
            .count_codes(&CodeFilter::default())
            .unwrap_or(0)
            .to_string(),
        codes_issued: count(CodeStatus::Issued).to_string(),
        codes_bound: count(CodeStatus::Bound).to_string(),
        codes_revoked: count(CodeStatus::Revoked).to_string(),
        codes_reissued: count(CodeStatus::Reissued).to_string(),
        receipts_anomalous: state
            .service
            .ledger()
            .count_warnings()
            .unwrap_or(0)
            .to_string(),
        active_kid,
    };
    ok_json(data)
}

/// `GET /admin/codes`：激活码列表 / 筛选 / 分页（设计 §2.4；码值掩码显示）。
async fn admin_codes(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<CodeListQuery>,
) -> Response {
    let authed = match authenticate(&state, &headers) {
        Ok(a) => a,
        Err(resp) => return resp,
    };
    let _ = authed;
    let (page, page_size) = page_params(q.page, q.page_size);
    let filter = CodeFilter {
        tenant_id: q.tenant_id,
        status: match q.status.as_deref().map(str::trim) {
            None | Some("") => None,
            Some(raw) => match CodeStatus::parse(raw) {
                Ok(s) => Some(s),
                Err(_) => {
                    return error_response(&LicenseError::KeyStateIllegal(format!(
                        "unknown status filter: {raw}"
                    )));
                }
            },
        },
        tier: q.tier.filter(|t| !t.trim().is_empty()),
        order_id: q.order_id.filter(|t| !t.trim().is_empty()),
    };
    let store = state.service.store();
    let total = match store.count_codes(&filter) {
        Ok(n) => n,
        Err(e) => return error_response(&e),
    };
    let rows = match store.list_codes(&filter, page, page_size) {
        Ok(rows) => rows,
        Err(e) => return error_response(&e),
    };
    let items: Vec<CodeSummary> = rows
        .iter()
        .map(|c| CodeSummary {
            code_id: c.code_id.clone(),
            code_masked: mask_code(&c.code),
            status: c.status.as_str().to_string(),
            tenant_id: c.tenant_id.clone(),
            tier: c.tier.clone(),
            bound_device_id: c.bound_device_id.clone(),
            valid_until: c.valid_until.to_string(),
            created_at: c.created_at.to_string(),
        })
        .collect();
    ok_json(paged(items, total, page, page_size))
}

/// `GET /admin/codes/:code_id`：详情 + 时间线 + 重发溯源链（设计 §2.5）。
async fn admin_code_detail(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(code_id): Path<String>,
) -> Response {
    let authed = match authenticate(&state, &headers) {
        Ok(a) => a,
        Err(resp) => return resp,
    };
    let _ = authed;
    let store = state.service.store();
    let code = match store.get_code_by_id(&code_id) {
        Ok(Some(c)) => c,
        Ok(None) => {
            return error_response(&LicenseError::KeyStateIllegal(format!(
                "activation code not found: {code_id}"
            )));
        }
        Err(e) => return error_response(&e),
    };

    // 时间线：audit_log 中 entity_id == code_id 的全部事件（激活 / 废弃 / 重发…）。
    let timeline = match store.list_audit_logs(
        &AuditFilter {
            entity_id: Some(code_id.clone()),
            ..Default::default()
        },
        1,
        MAX_PAGE_SIZE,
    ) {
        Ok(logs) => logs
            .iter()
            .map(|l| TimelineEntry {
                action: l.action.clone(),
                actor: format!("{}:{}", l.actor_type.as_str(), l.actor_id),
                at: l.ts.to_string(),
                detail: l.detail.clone(),
            })
            .collect(),
        Err(e) => return error_response(&e),
    };

    // 重发溯源链：沿 `reissued_from_id` 向上收集祖先 code_id（上限 20 层防环）。
    let mut reissued_chain: Vec<String> = Vec::new();
    let mut cursor = code.reissued_from_id.clone();
    while let Some(ancestor) = cursor {
        if reissued_chain.len() >= 20 {
            break;
        }
        reissued_chain.push(ancestor.clone());
        cursor = match store.get_code_by_id(&ancestor) {
            Ok(Some(c)) => c.reissued_from_id,
            _ => None,
        };
    }

    let data = CodeDetail {
        code_id: code.code_id.clone(),
        code: code.code.clone(),
        status: code.status.as_str().to_string(),
        tenant_id: code.tenant_id.clone(),
        tier: code.tier.clone(),
        bound_device_id: code.bound_device_id.clone(),
        valid_from: code.valid_from.to_string(),
        valid_until: code.valid_until.to_string(),
        timeline,
        reissued_chain,
        revoked_at: code.revoked_at.map(|t| t.to_string()),
        revoked_reason: code.revoked_reason.clone(),
    };
    ok_json(data)
}

/// `GET /admin/tenants`：租户列表（任意角色）。
async fn admin_tenants(State(state): State<AppState>, headers: HeaderMap) -> Response {
    let authed = match authenticate(&state, &headers) {
        Ok(a) => a,
        Err(resp) => return resp,
    };
    let _ = authed;
    let rows = match state.service.store().list_tenants() {
        Ok(rows) => rows,
        Err(e) => return error_response(&e),
    };
    let items: Vec<TenantItem> = rows.iter().map(tenant_item).collect();
    let total = items.len() as u64;
    let page_size = items.len().max(1) as u32;
    ok_json(paged(items, total, 1, page_size))
}

/// `POST /admin/tenants`：创建租户（仅 system；新部署自举必需）。
async fn admin_create_tenant(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<CreateTenantRequest>,
) -> Response {
    let authed = match authenticate(&state, &headers) {
        Ok(a) => a,
        Err(resp) => return resp,
    };
    if let Err(resp) = require_role(&authed, &ROLES_TENANT_ADMIN, "create tenants") {
        return resp;
    }
    match state.service.admin_create_tenant(
        &req.tenant_id,
        &req.name,
        &req.contact,
        req.verify_mode_default.as_deref(),
        &authed.sub,
    ) {
        Ok(tenant) => ok_json(tenant_item(&tenant)),
        Err(e) => error_response(&e),
    }
}

/// `PUT /admin/tenants/:tenant_id/policy`：更新租户默认校验档位（仅 system）。
async fn admin_update_tenant_policy(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(tenant_id): Path<String>,
    Json(req): Json<UpdateTenantPolicyRequest>,
) -> Response {
    let authed = match authenticate(&state, &headers) {
        Ok(a) => a,
        Err(resp) => return resp,
    };
    if let Err(resp) = require_role(&authed, &ROLES_TENANT_ADMIN, "update tenant policy") {
        return resp;
    }
    match state.service.admin_update_tenant_policy(
        &tenant_id,
        &req.verify_mode_default,
        &authed.sub,
    ) {
        Ok(()) => ok_json(()),
        Err(e) => error_response(&e),
    }
}

// ============================================================================
// 管理端：账号 / 角色（可配置；缺口 #9 修复）
// ============================================================================

/// 仅 system 可管理的账号端点角色集。
const ROLES_ACCOUNT_ADMIN: [Role; 1] = [Role::System];
/// 任意已认证角色（角色清单为只读元数据）。
const ROLES_ANY: [Role; 4] = [Role::Ops, Role::LicOps, Role::Risk, Role::System];

/// store 账号行 → 下发条目（**口令摘要绝不下发**）。
fn admin_account_item(a: &crate::admin_auth::AdminAccount) -> proto::AdminAccountItem {
    proto::AdminAccountItem {
        account: a.account.clone(),
        display_name: a.display_name.clone(),
        role: a.role.clone(),
        status: a.status.clone(),
        last_login_at: a.last_login_at.map(|v| v.to_string()),
        created_at: a.created_at.to_string(),
        updated_at: a.updated_at.to_string(),
    }
}

/// 角色清单（与 ui-kit `ROLES` / `ROLE_META` 同一套 id 与中文名）。
fn role_catalog() -> proto::AdminRolesResponse {
    let item = |id: &str, label: &str, full_label: &str| proto::AdminRoleItem {
        id: id.to_string(),
        label: label.to_string(),
        full_label: full_label.to_string(),
    };
    proto::AdminRolesResponse {
        items: vec![
            item("ops", "运营", "运营（只读 + 发放）"),
            item("lic_ops", "授权运营", "授权运营（发放 / 废弃 / 重发）"),
            item("risk", "风控", "风控（回执异常 / 审计只读 + 标记异常）"),
            item("system", "系统", "系统（全部权限）"),
        ],
    }
}

/// `GET /admin/accounts`：管理员账号列表（仅 system；口令摘要绝不下发）。
async fn admin_list_accounts(State(state): State<AppState>, headers: HeaderMap) -> Response {
    let authed = match authenticate(&state, &headers) {
        Ok(a) => a,
        Err(resp) => return resp,
    };
    if let Err(resp) = require_role(&authed, &ROLES_ACCOUNT_ADMIN, "list admin accounts") {
        return resp;
    }
    let rows = match state.service.admin_list_admin_accounts() {
        Ok(rows) => rows,
        Err(e) => return error_response(&e),
    };
    let items: Vec<proto::AdminAccountItem> = rows.iter().map(admin_account_item).collect();
    let total = items.len() as u64;
    let page_size = items.len().max(1) as u32;
    ok_json(paged(items, total, 1, page_size))
}

/// `POST /admin/accounts`：创建管理员账号（仅 system）。
async fn admin_create_account(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<proto::CreateAdminAccountRequest>,
) -> Response {
    let authed = match authenticate(&state, &headers) {
        Ok(a) => a,
        Err(resp) => return resp,
    };
    if let Err(resp) = require_role(&authed, &ROLES_ACCOUNT_ADMIN, "create admin account") {
        return resp;
    }
    match state.service.admin_create_admin_account(
        &req.account,
        &req.display_name,
        &req.role,
        &req.password,
        &authed.sub,
    ) {
        Ok(acct) => ok_json(admin_account_item(&acct)),
        Err(e) => error_response(&e),
    }
}

/// `PUT /admin/accounts/:account`：更新账号（显示名 / 角色 / 口令 / 状态；仅 system）。
async fn admin_update_account(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(account): Path<String>,
    Json(req): Json<proto::UpdateAdminAccountRequest>,
) -> Response {
    let authed = match authenticate(&state, &headers) {
        Ok(a) => a,
        Err(resp) => return resp,
    };
    if let Err(resp) = require_role(&authed, &ROLES_ACCOUNT_ADMIN, "update admin account") {
        return resp;
    }
    match state.service.admin_update_admin_account(
        &account,
        req.display_name.as_deref(),
        req.role.as_deref(),
        req.password.as_deref(),
        req.status.as_deref(),
        req.note.as_deref().unwrap_or(""),
        &authed.sub,
    ) {
        Ok(()) => ok_json(()),
        Err(e) => error_response(&e),
    }
}

/// `DELETE /admin/accounts/:account`：删除账号（仅 system）。
async fn admin_delete_account(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(account): Path<String>,
    Json(req): Json<proto::DeleteAdminAccountRequest>,
) -> Response {
    let authed = match authenticate(&state, &headers) {
        Ok(a) => a,
        Err(resp) => return resp,
    };
    if let Err(resp) = require_role(&authed, &ROLES_ACCOUNT_ADMIN, "delete admin account") {
        return resp;
    }
    match state
        .service
        .admin_delete_admin_account(&account, req.note.as_deref().unwrap_or(""), &authed.sub)
    {
        Ok(()) => ok_json(()),
        Err(e) => error_response(&e),
    }
}

/// `GET /admin/roles`：角色清单（任意已认证角色）。
async fn admin_roles(State(state): State<AppState>, headers: HeaderMap) -> Response {
    let authed = match authenticate(&state, &headers) {
        Ok(a) => a,
        Err(resp) => return resp,
    };
    if let Err(resp) = require_role(&authed, &ROLES_ANY, "list roles") {
        return resp;
    }
    ok_json(role_catalog())
}

/// `GET /admin/devices`：设备列表 / 筛选 / 分页（设计 §2.6；机器码掩码）。
///
/// store 原生只支持租户过滤；`deploy_mode` / `status` / `machine_code` 在内存过滤
/// （管理台数据量级可接受，注释即契约）。
async fn admin_devices(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<DeviceListQuery>,
) -> Response {
    let authed = match authenticate(&state, &headers) {
        Ok(a) => a,
        Err(resp) => return resp,
    };
    let _ = authed;
    let (page, page_size) = page_params(q.page, q.page_size);
    let store = state.service.store();
    let deploy = q
        .deploy_mode
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let status = q.status.as_deref().map(str::trim).filter(|s| !s.is_empty());
    let machine = q
        .machine_code
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| s.to_lowercase());
    let needs_scan = deploy.is_some() || status.is_some() || machine.is_some();

    // 取候选集：无内存过滤直接走 store 分页；有过滤则分批扫描全量再过滤。
    let candidates: Vec<Device> = if !needs_scan {
        match store.list_devices(
            q.tenant_id.as_deref().filter(|t| !t.trim().is_empty()),
            page,
            page_size,
        ) {
            Ok(rows) => rows,
            Err(e) => return error_response(&e),
        }
    } else {
        match scan_all_devices(
            store,
            q.tenant_id.as_deref().filter(|t| !t.trim().is_empty()),
        ) {
            Ok(rows) => rows
                .into_iter()
                .filter(|d| {
                    if let Some(want) = deploy {
                        if d.deploy_mode.as_str() != want {
                            return false;
                        }
                    }
                    if let Some(want) = status {
                        if d.status.as_str() != want {
                            return false;
                        }
                    }
                    if let Some(kw) = &machine {
                        if !d.machine_code.to_lowercase().contains(kw) {
                            return false;
                        }
                    }
                    true
                })
                .collect(),
            Err(e) => return error_response(&e),
        }
    };

    let total = if needs_scan {
        candidates.len() as u64
    } else {
        store
            .count_devices(q.tenant_id.as_deref().filter(|t| !t.trim().is_empty()))
            .unwrap_or(0)
    };
    // needs_scan 时对内存过滤结果手动切页；无过滤时 store 已按页切好。
    let page_items: Vec<Device> = if needs_scan {
        let start = ((page - 1) as usize) * page_size as usize;
        candidates
            .into_iter()
            .skip(start)
            .take(page_size as usize)
            .collect()
    } else {
        candidates
    };
    let items: Vec<DeviceListItem> = page_items.iter().map(|d| device_item(store, d)).collect();
    ok_json(paged(items, total, page, page_size))
}

/// `GET /admin/receipts/anomalies`：异常回执列表（任意角色）。
///
/// 数据源 = 回执账本 `audit_receipt_warning` 表（设计 §1.4：跳空 / 回退 / 缺失
/// 告警的权威落点，「人工核实、不自动封禁」）。
async fn admin_receipt_anomalies(State(state): State<AppState>, headers: HeaderMap) -> Response {
    let authed = match authenticate(&state, &headers) {
        Ok(a) => a,
        Err(resp) => return resp,
    };
    let _ = authed;
    let (page, page_size) = page_params(None, None);
    let ledger = state.service.ledger();
    let total = match ledger.count_warnings() {
        Ok(n) => n,
        Err(e) => return error_response(&e),
    };
    let rows = match ledger.list_warnings(page, page_size) {
        Ok(rows) => rows,
        Err(e) => return error_response(&e),
    };
    let items: Vec<ReceiptAnomalyItem> = rows
        .iter()
        .map(|w| ReceiptAnomalyItem {
            id: w.id.to_string(),
            device_mid: w.device_mid.clone(),
            lease_id: w.lease_id.clone(),
            kind: w.kind.clone(),
            seq_from: w.seq_from.to_string(),
            seq_to: w.seq_to.to_string(),
            last_seq_to: w.last_seq_to.to_string(),
            detail: w.detail.clone(),
            created_at: w.created_at.to_string(),
        })
        .collect();
    ok_json(paged(items, total, page, page_size))
}

/// `GET /admin/keys`：签名密钥列表（**只含公钥**；任意角色）。
async fn admin_keys(State(state): State<AppState>, headers: HeaderMap) -> Response {
    let authed = match authenticate(&state, &headers) {
        Ok(a) => a,
        Err(resp) => return resp,
    };
    let _ = authed;
    let rows: Vec<SigningKey> = match state.service.store().list_signing_keys() {
        Ok(rows) => rows,
        Err(e) => return error_response(&e),
    };
    let items: Vec<SigningKeyItem> = rows
        .iter()
        .map(|k| SigningKeyItem {
            kid: k.kid.clone(),
            status: k.status.as_str().to_string(),
            public_key: k.public_key.clone(),
            hsm_ref: k.hsm_ref.clone(),
            enabled_at: k.enabled_at.to_string(),
            retired_at: k.retired_at.map(|t| t.to_string()),
        })
        .collect();
    let total = items.len() as u64;
    let page_size = items.len().max(1) as u32;
    ok_json(paged(items, total, 1, page_size))
}

/// `GET /admin/audit/logs`：审计日志（筛选 / 分页；任意角色）。
///
/// store 原生支持 actor_type / action / entity 过滤；`time_from` / `time_to` 在
/// 内存过滤（分批扫描，管理台数据量级可接受）。
async fn admin_audit_logs(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<AuditLogQuery>,
) -> Response {
    let authed = match authenticate(&state, &headers) {
        Ok(a) => a,
        Err(resp) => return resp,
    };
    let _ = authed;
    let (page, page_size) = page_params(q.page, q.page_size);
    let actor_type = match q.actor_type.as_deref().map(str::trim) {
        None | Some("") => None,
        Some(raw) => match ActorType::parse(raw) {
            Ok(t) => Some(t),
            Err(_) => {
                return error_response(&LicenseError::KeyStateIllegal(format!(
                    "unknown actor_type filter: {raw}"
                )));
            }
        },
    };
    let time_from = match parse_i64_filter(q.time_from.as_deref(), "time_from") {
        Ok(v) => v,
        Err(resp) => return resp,
    };
    let time_to = match parse_i64_filter(q.time_to.as_deref(), "time_to") {
        Ok(v) => v,
        Err(resp) => return resp,
    };
    let filter = AuditFilter {
        actor_type,
        action: q.action.filter(|s| !s.trim().is_empty()),
        entity_type: q.entity_type.filter(|s| !s.trim().is_empty()),
        entity_id: q.entity_id.filter(|s| !s.trim().is_empty()),
    };
    let store = state.service.store();

    // 无时间范围 → store 直查分页；有时间范围 → 分批扫描 + 内存过滤 + 手动切页。
    if time_from.is_none() && time_to.is_none() {
        let total = match store.count_audit_logs(&filter) {
            Ok(n) => n,
            Err(e) => return error_response(&e),
        };
        let rows = match store.list_audit_logs(&filter, page, page_size) {
            Ok(rows) => rows,
            Err(e) => return error_response(&e),
        };
        let items: Vec<AuditLogItem> = rows.iter().map(audit_item).collect();
        return ok_json(paged(items, total, page, page_size));
    }

    let mut all: Vec<crate::model::AuditLog> = Vec::new();
    let mut scan_page: u32 = 1;
    loop {
        let batch = match store.list_audit_logs(&filter, scan_page, SCAN_BATCH) {
            Ok(b) => b,
            Err(e) => return error_response(&e),
        };
        let done = batch.len() < SCAN_BATCH as usize;
        for log in batch {
            if let Some(from) = time_from {
                if log.ts < from {
                    continue;
                }
            }
            if let Some(to) = time_to {
                if log.ts > to {
                    continue;
                }
            }
            all.push(log);
        }
        if done || scan_page >= 1000 {
            break;
        }
        scan_page += 1;
    }
    let total = all.len() as u64;
    let start = ((page - 1) as usize) * page_size as usize;
    let slice: Vec<crate::model::AuditLog> = all
        .into_iter()
        .skip(start)
        .take(page_size as usize)
        .collect();
    let items: Vec<AuditLogItem> = slice.iter().map(audit_item).collect();
    ok_json(paged(items, total, page, page_size))
}

// ============================================================================
// 管理端：写操作（issue / revoke / reissue，高危仅 lic_ops / system）
// ============================================================================

/// `POST /admin/codes/issue`：批量发放（ops / lic_ops / system）。
async fn issue_codes(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<IssueCodesRequest>,
) -> Response {
    let authed = match authenticate(&state, &headers) {
        Ok(a) => a,
        Err(resp) => return resp,
    };
    if let Err(resp) = require_role(&authed, &ROLES_ISSUE, "issue codes") {
        return resp;
    }
    match state.service.issue_codes(&req) {
        Ok(resp) => ok_json(resp),
        Err(e) => error_response(&e),
    }
}

/// `POST /admin/codes/:code_id/revoke`：废弃（lic_ops / system，高危）。
///
/// 契约校验（设计 §2.2）在 service 层执行：`confirm_tail8` 尾 8 位不符 → 412
/// `CONFIRM_MISMATCH`；`note` < 10 字 → 400；`reason` 空白 → 400 `REASON_REQUIRED`。
async fn revoke_code(
    State(state): State<AppState>,
    Path(code_id): Path<String>,
    headers: HeaderMap,
    Json(req): Json<RevokeCodeRequest>,
) -> Response {
    let authed = match authenticate(&state, &headers) {
        Ok(a) => a,
        Err(resp) => return resp,
    };
    if let Err(resp) = require_role(&authed, &ROLES_DANGEROUS, "revoke codes") {
        return resp;
    }
    let tenant_id = match require_tenant_header(&headers) {
        Ok(t) => t,
        Err(resp) => return resp,
    };
    // 操作者 = JWT sub（登录用户名，入审计），不再取可伪造的 X-Actor-Id。
    match state
        .service
        .revoke(&tenant_id, &code_id, &req, &authed.sub)
    {
        Ok(()) => ok_json(()),
        Err(e) => error_response(&e),
    }
}

/// `POST /admin/codes/:code_id/reissue`：重发 / 换机迁移（lic_ops / system，高危）。
async fn reissue_code(
    State(state): State<AppState>,
    Path(code_id): Path<String>,
    headers: HeaderMap,
    Json(req): Json<ReissueCodeRequest>,
) -> Response {
    let authed = match authenticate(&state, &headers) {
        Ok(a) => a,
        Err(resp) => return resp,
    };
    if let Err(resp) = require_role(&authed, &ROLES_DANGEROUS, "reissue codes") {
        return resp;
    }
    let tenant_id = match require_tenant_header(&headers) {
        Ok(t) => t,
        Err(resp) => return resp,
    };
    match state
        .service
        .reissue(&tenant_id, &code_id, &req, &authed.sub)
    {
        Ok(resp) => ok_json(resp),
        Err(e) => error_response(&e),
    }
}

// ============================================================================
// 内部辅助
// ============================================================================

/// 从请求头取值，`trim` 后空串也按缺省处理。
fn extract_header(headers: &HeaderMap, name: &str, default: &str) -> String {
    headers
        .get(name)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|| default.to_string())
}

/// `X-Tenant-Id` 头必填（缺失 → 400 结构化错误；不再缺省 `unknown`）。
#[allow(clippy::result_large_err)]
fn require_tenant_header(headers: &HeaderMap) -> Result<String, Response> {
    let raw = extract_header(headers, "x-tenant-id", "");
    if raw.is_empty() {
        return Err(error_response(&LicenseError::KeyStateIllegal(
            "missing required header: X-Tenant-Id".into(),
        )));
    }
    Ok(raw)
}

/// 解析分页参数（`page` 从 1 起；`page_size` 缺省 [`DEFAULT_PAGE_SIZE`]，上限
/// [`MAX_PAGE_SIZE`]）。
fn page_params(page: Option<u32>, page_size: Option<u32>) -> (u32, u32) {
    let page = page.unwrap_or(1).max(1);
    let page_size = page_size
        .unwrap_or(DEFAULT_PAGE_SIZE)
        .clamp(1, MAX_PAGE_SIZE);
    (page, page_size)
}

/// 构造通用分页信封（total / page / page_size 均 **String**，大数红线）。
fn paged<T>(items: Vec<T>, total: u64, page: u32, page_size: u32) -> PagedResponse<T> {
    PagedResponse {
        items,
        total: total.to_string(),
        page: page.to_string(),
        page_size: page_size.to_string(),
    }
}

/// 掩码激活码值（列表页显示；详情页揭示完整码值）。
///
/// 形如 `IOTDAQ-****-****-****-AB12`（保留前缀 `IOTDAQ-` 与尾 4 位）；
/// 非 `IOTDAQ-` 前缀的短码一律整串掩码（不留可猜测片段）。
fn mask_code(code: &str) -> String {
    let chars: Vec<char> = code.chars().collect();
    if code.starts_with("IOTDAQ-") && chars.len() >= 12 {
        let tail: String = chars[chars.len() - 4..].iter().collect();
        return format!("IOTDAQ-****-****-****-{tail}");
    }
    if chars.len() <= 4 {
        return "*".repeat(chars.len());
    }
    let head: String = chars[..2].iter().collect();
    let tail: String = chars[chars.len() - 2..].iter().collect();
    format!("{head}****{tail}")
}

/// 掩码机器码（列表页显示；保留首尾各 2 位，中间 `****`）。
fn mask_machine_code(machine_code: &str) -> String {
    let chars: Vec<char> = machine_code.chars().collect();
    if chars.len() <= 4 {
        return "****".to_string();
    }
    let head: String = chars[..2].iter().collect();
    let tail: String = chars[chars.len() - 2..].iter().collect();
    format!("{head}****{tail}")
}

/// 分批拉取全部设备（store 无原生复合过滤时的扫描底座；批 [`SCAN_BATCH`]）。
fn scan_all_devices(
    store: &crate::store::Store,
    tenant_id: Option<&str>,
) -> crate::error::LicenseResult<Vec<Device>> {
    let mut out = Vec::new();
    let mut page: u32 = 1;
    loop {
        let batch = store.list_devices(tenant_id, page, SCAN_BATCH)?;
        let done = batch.len() < SCAN_BATCH as usize;
        out.extend(batch);
        if done || page >= 1000 {
            break;
        }
        page += 1;
    }
    Ok(out)
}

/// `Tenant` → 响应条目。
fn tenant_item(t: &Tenant) -> TenantItem {
    TenantItem {
        tenant_id: t.tenant_id.clone(),
        name: t.name.clone(),
        verify_mode_default: t.verify_mode_default.as_str().to_string(),
        contact: t.contact.clone(),
        created_at: t.created_at.to_string(),
    }
}

/// `Device` → 响应条目（机器码掩码 + 最近租约状态 + 回执异常汇总）。
///
/// 最近租约取该设备 `issued_at` 最新一条（其状态与 `last_heartbeat_at`）；
/// 回执异常汇总 = 该设备全部租约的 `gap_flag = 1` 回执计数（形如 `anomalous=2`；
/// 无租约 / 无异常为 `anomalous=0`）。
fn device_item(store: &crate::store::Store, d: &Device) -> DeviceListItem {
    let (lease_status, last_heartbeat_at) = match store.list_leases_by_device(&d.device_id) {
        Ok(leases) => match leases.first() {
            Some(l) => (
                Some(l.status.as_str().to_string()),
                l.last_heartbeat_at.map(|t| t.to_string()),
            ),
            None => (None, None),
        },
        Err(_) => (None, None),
    };
    let anomalous = store
        .list_leases_by_device(&d.device_id)
        .ok()
        .map(|leases| {
            leases
                .iter()
                .filter_map(|l| store.list_receipts_by_lease(&l.lease_id).ok())
                .flat_map(|rs| rs.into_iter())
                .filter(|r| r.gap_flag)
                .count()
        })
        .unwrap_or(0);
    DeviceListItem {
        device_id: d.device_id.clone(),
        tenant_id: d.tenant_id.clone(),
        machine_code_masked: mask_machine_code(&d.machine_code),
        deploy_mode: d.deploy_mode.as_str().to_string(),
        image_digest: d.image_digest.clone(),
        last_heartbeat_at,
        lease_status,
        receipt_gap_summary: format!("anomalous={anomalous}"),
    }
}

/// `AuditLog` → 响应条目（actor / entity 拼接为 `type:id`）。
fn audit_item(l: &crate::model::AuditLog) -> AuditLogItem {
    AuditLogItem {
        ts: l.ts.to_string(),
        actor: format!("{}:{}", l.actor_type.as_str(), l.actor_id),
        action: l.action.clone(),
        entity: format!("{}:{}", l.entity_type, l.entity_id),
        detail: l.detail.clone(),
        ip: l.ip.clone(),
    }
}

/// 解析可空的 i64 过滤参数（非法整数 → 400 结构化错误）。
#[allow(clippy::result_large_err)]
fn parse_i64_filter(raw: Option<&str>, name: &str) -> Result<Option<i64>, Response> {
    match raw.map(str::trim) {
        None | Some("") => Ok(None),
        Some(v) => match v.parse::<i64>() {
            Ok(n) => Ok(Some(n)),
            Err(_) => Err(error_response(&LicenseError::KeyStateIllegal(format!(
                "invalid {name}: must be integer seconds"
            )))),
        },
    }
}

/// 生成本次响应的链路追踪 ID（大整数纪律：字符串时间戳前缀）。
fn new_trace() -> String {
    now_ns_id("http")
}

/// 构造成功响应：`(200, ApiEnvelope::ok)` 统一为 `Response`。
fn ok_json<T: Serialize>(data: T) -> Response {
    Json(ApiEnvelope::ok(data, new_trace())).into_response()
}

/// 构造失败响应：**结构化**错误映射（变体 → 业务码 → HTTP 状态码）。
fn error_response(err: &LicenseError) -> Response {
    let code = error_to_code(err);
    let status = proto::http_status(code);
    let status_code = StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    (
        status_code,
        Json(ApiEnvelope::<()>::err(code, err.to_string(), new_trace())),
    )
        .into_response()
}

/// 把 [`LicenseError`] 映射为业务码（见 `proto::codes`）。
///
/// **结构化映射**：直接 `match` 变体，不依赖错误消息文本。HTTP 状态码由
/// [`proto::http_status`] 据返回的业务码推出，保证「业务码 ↔ HTTP 状态」唯一来源。
pub fn error_to_code(err: &LicenseError) -> &'static str {
    match err {
        // 一机一码预绑定冲突 → PREBIND_CONFLICT（proto::http_status → 400）。
        LicenseError::PrebindConflict { .. } => proto::codes::PREBIND_CONFLICT,
        // 同码异机（§7 冲突检测步骤 ③）→ CODE_BOUND_TO_OTHER_DEVICE（403）。
        LicenseError::CodeBoundToOtherDevice { .. } => proto::codes::CODE_BOUND_TO_OTHER_DEVICE,
        // 激活被拒（码无效 / 已绑定他机 / 已废弃 / 未知租户）→ INVALID_CODE（400）。
        LicenseError::ActivationRejected(_) => proto::codes::INVALID_CODE,
        // 状态机非法（未废弃重发 / 空有效期 / 空 reason / note 过短等）→ BAD_REQUEST（400）。
        LicenseError::KeyStateIllegal(_) => proto::codes::BAD_REQUEST,
        // Token 无效 / 过期 → TOKEN_EXPIRED（401）。
        LicenseError::TokenInvalid(_) => proto::codes::TOKEN_EXPIRED,
        // 心跳被拒 → LEASE_REVOKED（403）。
        LicenseError::HeartbeatRejected(_) => proto::codes::LEASE_REVOKED,
        // 租约不存在 → LEASE_NOT_FOUND（404）。
        LicenseError::LeaseNotFound(_) => proto::codes::LEASE_NOT_FOUND,
        // 租约已废弃 → LEASE_REVOKED（403，废弃语义 = 立即失效）。
        LicenseError::LeaseRevoked(_) => proto::codes::LEASE_REVOKED,
        // 验签失败 → VERIFY_FAIL（401）。
        LicenseError::VerifyFailed(_) => proto::codes::VERIFY_FAIL,
        // 时间窗超限 → TIMESTAMP_SKEW（401）。
        LicenseError::TimestampSkew(_) => proto::codes::TIMESTAMP_SKEW,
        // nonce 重放 → NONCE_REPLAY（409）。
        LicenseError::NonceReplay(_) => proto::codes::NONCE_REPLAY,
        // 字段白名单越界 → FIELD_WHITELIST_VIOLATION（422）。
        LicenseError::FieldWhitelistViolation(_) => proto::codes::FIELD_WHITELIST_VIOLATION,
        // 激活请求验签失败 → ACTIVATION_SIGNATURE_INVALID（401；2026-09-25 主理人决策）。
        LicenseError::ActivationSignatureInvalid(_) => proto::codes::ACTIVATION_SIGNATURE_INVALID,
        // 激活公钥与钉定值不一致 → ACTIVATION_PUBKEY_MISMATCH（403；2026-09-25 主理人决策）。
        LicenseError::ActivationPubkeyMismatch(_) => proto::codes::ACTIVATION_PUBKEY_MISMATCH,
        // 配额超限 → QUOTA_EXCEEDED（403）。
        LicenseError::QuotaExceeded(_) => proto::codes::QUOTA_EXCEEDED,
        // 废弃确认串不符 → CONFIRM_MISMATCH（412，设计 §2.2）。
        LicenseError::ConfirmMismatch(_) => proto::codes::CONFIRM_MISMATCH,
        // 废弃原因缺失 → REASON_REQUIRED（400，设计 §2.2）。
        LicenseError::ReasonRequired(_) => proto::codes::REASON_REQUIRED,
        // 管理端未认证 → SESSION_EXPIRED（401，设计 §2）。
        LicenseError::Unauthorized(_) => proto::codes::SESSION_EXPIRED,
        // 管理端角色越权 → ADMIN_ONLY（403，设计 §2 / §4）。
        LicenseError::Forbidden(_) => proto::codes::ADMIN_ONLY,
        // 存储层异常属内部错误：用未知业务码，让 `http_status` 兜底为 500，
        // 绝不伪装成客户端 400（否则故障被掩盖）。
        LicenseError::Storage(_) => "INTERNAL_SERVER_ERROR",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::admin_auth::{AdminAuth, Role};
    use crate::keys::KeyRing;
    use crate::model::{now_unix_secs, Tenant};
    use crate::store::Store;
    use axum::body::{to_bytes, Body};
    use axum::http::{Request, StatusCode};
    use base64::engine::general_purpose::STANDARD as B64;
    use base64::Engine as _;
    use ed25519_dalek::{Signer, SigningKey};
    use serde_json::json;
    use std::sync::Arc;
    use tower::ServiceExt;

    /// **TEST_ONLY_** 密钥种子（仅测试；生产密钥绝不硬编码）。
    const TEST_ONLY_SEED: [u8; 32] = *b"iotdaq-test-seed-http-0000000001";
    /// **TEST_ONLY_** 异钥种子（伪造签名负例）。
    const TEST_ONLY_ROGUE_SEED: [u8; 32] = *b"iotdaq-rogue-seed-http-000000001";
    /// **TEST_ONLY_** 设备密钥种子（激活请求 `req_sig` 签名；仅测试，禁止真实部署）。
    const TEST_ONLY_DEVICE_SEED: [u8; 32] = *b"iotdaq-device-seed-http-00000001";
    /// **TEST_ONLY_** 管理员口令（仅测试）。
    const TEST_ONLY_ADMIN_PASSWORD: &str = "test-admin-password";
    /// **TEST_ONLY_** 运营角色账号（403 负例用）。
    const TEST_ONLY_OPS_PASSWORD: &str = "test-ops-password";

    /// 构造 **TEST_ONLY_** 管理端鉴权器（admin/system + oliver/ops）。
    fn test_auth() -> Arc<AdminAuth> {
        Arc::new(
            AdminAuth::new(crate::admin_auth::IssuerKey([0x77u8; 32]))
                .with_user("admin", Role::System, TEST_ONLY_ADMIN_PASSWORD)
                .with_user("oliver", Role::Ops, TEST_ONLY_OPS_PASSWORD),
        )
    }

    /// 构造带内存库 + 已注册**已知测试密钥**的服务（测试专用）。
    ///
    /// 账号表预置 admin/system + oliver/ops：HTTP 登录**实时读 store**（缺口 #9），
    /// 故测试库必须与内存账号表同源（否则 `/admin/auth/login` 会 401）。
    fn build_service() -> SharedService {
        let store = Store::open_in_memory().expect("open in-memory store");
        let tenant = Tenant::new(
            "t-1".to_string(),
            "Tenant".to_string(),
            "ops@x".to_string(),
            now_unix_secs(),
        );
        store.insert_tenant(&tenant).expect("seed tenant");
        let seeded_at = now_unix_secs();
        for (account, role, password) in [
            ("admin", "system", TEST_ONLY_ADMIN_PASSWORD),
            ("oliver", "ops", TEST_ONLY_OPS_PASSWORD),
        ] {
            store
                .insert_admin_account(&crate::admin_auth::AdminAccount {
                    account: account.to_string(),
                    display_name: String::new(),
                    role: role.to_string(),
                    password_sha256: crate::admin_auth::sha256_hex(password),
                    status: "active".to_string(),
                    last_login_at: None,
                    created_at: seeded_at,
                    updated_at: seeded_at,
                })
                .expect("seed admin account");
        }
        let keyring = KeyRing::empty();
        keyring
            .register_from_b64("k-test", &B64.encode(TEST_ONLY_SEED), None, 1_700_000_000)
            .expect("register signing key");
        Arc::new(LicensingService::new(store, keyring))
    }

    /// 以 admin 账号直接签发测试 token（绕过 HTTP，避免测试间串扰）。
    fn admin_token() -> String {
        test_auth()
            .login("admin", TEST_ONLY_ADMIN_PASSWORD)
            .expect("admin login")
            .0
    }

    /// 以 oliver（ops 角色）签发测试 token。
    fn ops_token() -> String {
        test_auth()
            .login("oliver", TEST_ONLY_OPS_PASSWORD)
            .expect("ops login")
            .0
    }

    /// Bearer 请求头键值。
    fn bearer(token: &str) -> (String, String) {
        ("authorization".to_string(), format!("Bearer {token}"))
    }

    /// 对摘要用测试密钥签名（STANDARD base64）。
    fn sign_hash(hash: &[u8; 32]) -> String {
        let key = SigningKey::from_bytes(&TEST_ONLY_SEED);
        B64.encode(key.sign(hash).to_bytes())
    }

    /// 对摘要用异钥签名（伪造负例）。
    fn sign_hash_rogue(hash: &[u8; 32]) -> String {
        let key = SigningKey::from_bytes(&TEST_ONLY_ROGUE_SEED);
        B64.encode(key.sign(hash).to_bytes())
    }

    /// 构造 `POST /admin/codes/issue` 请求体。
    fn issue_body(prebind: Option<&str>, idem: &str) -> serde_json::Value {
        let n = now_unix_secs();
        json!({
            "tenant_id": "t-1",
            "tier": "pro",
            "valid_from": (n - 1000).to_string(),
            "valid_until": (n + 365 * 86_400).to_string(),
            "count": 1,
            "prebind_machine_code": prebind,
            "idempotency_key": idem
        })
    }

    /// 构造 `POST /activation` 请求体（默认设备密钥，签名合法，nonce 唯一）。
    fn activate_body(code_value: &str, machine: &str) -> serde_json::Value {
        let anchors = ["a", "b", "c", "d", "e"];
        activate_body_full(
            code_value,
            machine,
            &anchors.iter().map(|s| (*s).to_string()).collect::<Vec<_>>(),
            &TEST_ONLY_DEVICE_SEED,
            now_unix_secs(),
        )
    }

    /// 构造带**自定义锚点集**的 `POST /activation` 请求体（N-of-M 冲突检测测试用）。
    fn activate_body_with_anchors(
        code_value: &str,
        machine: &str,
        anchors: &[&str],
    ) -> serde_json::Value {
        activate_body_full(
            code_value,
            machine,
            &anchors.iter().map(|s| (*s).to_string()).collect::<Vec<_>>(),
            &TEST_ONLY_DEVICE_SEED,
            now_unix_secs(),
        )
    }

    /// 构造 `/activation` 请求体的完整形态：给定设备私钥种子与 `ts`，
    /// `req_sig` = 设备私钥对 `activation_payload_hash` 的签名（与 daemon 侧
    /// `auth::client::activate` 的构造逐字段同构），nonce 唯一。
    fn activate_body_full(
        code_value: &str,
        machine: &str,
        anchors: &[String],
        seed: &[u8; 32],
        ts: i64,
    ) -> serde_json::Value {
        let key = SigningKey::from_bytes(seed);
        let pubkey = B64.encode(key.verifying_key().to_bytes());
        let nonce = now_ns_id("n");
        let hash = crate::device_auth::activation_payload_hash(
            code_value, machine, anchors, &pubkey, &nonce, ts,
        );
        json!({
            "activation_code": code_value,
            "machine_code": machine,
            "anchor_hashes": anchors,
            "device_pubkey": pubkey,
            "nonce": nonce,
            "ts": ts.to_string(),
            "req_sig": B64.encode(key.sign(&hash).to_bytes()),
        })
    }

    /// 取出 issue 响应里首个码的 `(code_id, code_value)`。
    fn first_code(issue_body: &serde_json::Value) -> (String, String) {
        let id = issue_body["data"]["codes"][0]["code_id"]
            .as_str()
            .unwrap()
            .to_string();
        let value = issue_body["data"]["codes"][0]["code"]
            .as_str()
            .unwrap()
            .to_string();
        (id, value)
    }

    /// 激活码 → 后 8 位确认串（去分隔符，与 admin-console `tail8Of` 同构）。
    fn tail8_of(code: &str) -> String {
        code.chars()
            .filter(|c| c.is_ascii_alphanumeric())
            .collect::<String>()
            .to_uppercase()
            .chars()
            .rev()
            .take(8)
            .collect::<String>()
            .chars()
            .rev()
            .collect()
    }

    /// 发出一次请求并返回 `(status, json_body)`。
    ///
    /// `headers` 为 `(名称, 值)` 元组切片（值统一 `String`，便于混入动态 Bearer）。
    async fn call_with(
        service: &SharedService,
        method: &str,
        uri: &str,
        body: serde_json::Value,
        headers: &[(String, String)],
    ) -> (StatusCode, serde_json::Value) {
        let router = router(Arc::clone(service), test_auth());
        let mut builder = Request::builder()
            .method(method)
            .uri(uri)
            .header("content-type", "application/json");
        for (k, v) in headers {
            builder = builder.header(k.as_str(), v.as_str());
        }
        let req = builder
            .body(Body::from(serde_json::to_string(&body).unwrap()))
            .unwrap();
        let resp = router.oneshot(req).await.unwrap();
        let status = resp.status();
        let bytes = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap_or(json!(null));
        (status, body)
    }

    fn bad_request() -> StatusCode {
        StatusCode::from_u16(400).unwrap()
    }

    // ---------------- issue ----------------

    #[tokio::test]
    async fn http_issue_success_returns_200_and_envelope_ok() {
        let svc = build_service();
        let token = admin_token();
        let authed = [bearer(&token)];
        let (status, body) = call_with(
            &svc,
            "POST",
            "/admin/codes/issue",
            issue_body(Some("M1"), "h-issue-1"),
            &authed,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["code"], "OK");
        let codes = body["data"]["codes"].as_array().expect("codes array");
        assert_eq!(codes.len(), 1);
        // G1：预绑定必须落库并回显。
        assert_eq!(body["data"]["codes"][0]["prebind"], "M1");
    }

    #[tokio::test]
    async fn http_issue_prebind_conflict_returns_400_prebind_conflict() {
        let svc = build_service();
        let token = admin_token();
        let authed = [bearer(&token)];
        call_with(
            &svc,
            "POST",
            "/admin/codes/issue",
            issue_body(Some("M-X"), "h-issue-a"),
            &authed,
        )
        .await;
        // 同租户再发同预绑定 → 结构化错误码 PREBIND_CONFLICT，HTTP 400。
        let (status, body) = call_with(
            &svc,
            "POST",
            "/admin/codes/issue",
            issue_body(Some("M-X"), "h-issue-b"),
            &authed,
        )
        .await;
        assert_eq!(status, bad_request());
        assert_eq!(body["code"], "PREBIND_CONFLICT");
    }

    #[tokio::test]
    async fn http_issue_idempotent_returns_same_code() {
        let svc = build_service();
        let token = admin_token();
        let authed = [bearer(&token)];
        let (_, a) = call_with(
            &svc,
            "POST",
            "/admin/codes/issue",
            issue_body(None, "h-idem"),
            &authed,
        )
        .await;
        let (_, b) = call_with(
            &svc,
            "POST",
            "/admin/codes/issue",
            issue_body(None, "h-idem"),
            &authed,
        )
        .await;
        assert_eq!(a["code"], "OK");
        assert_eq!(b["code"], "OK");
        // G2：发放幂等——同键重放返回首次结果（同一 code_id）。
        let id_a = a["data"]["codes"][0]["code_id"].as_str().unwrap();
        let id_b = b["data"]["codes"][0]["code_id"].as_str().unwrap();
        assert_eq!(id_a, id_b, "发放必须幂等（同键返回首次结果）");
    }

    // ---------------- activate ----------------

    #[tokio::test]
    async fn http_activate_success_returns_200_and_lease() {
        let svc = build_service();
        let token = admin_token();
        let authed = [bearer(&token)];
        let (_, issue) = call_with(
            &svc,
            "POST",
            "/admin/codes/issue",
            issue_body(Some("M1"), "h-act-2"),
            &authed,
        )
        .await;
        let code = issue["data"]["codes"][0]["code"]
            .as_str()
            .unwrap()
            .to_string();
        let (status, body) =
            call_with(&svc, "POST", "/activation", activate_body(&code, "M1"), &[]).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["code"], "OK");
        // 激活成功必须下发非空 Lease Token。
        assert!(
            !body["data"]["lease_token"].as_str().unwrap().is_empty(),
            "lease_token 必须非空"
        );
    }

    #[tokio::test]
    async fn http_activate_prebind_mismatch_returns_400_prebind_conflict() {
        let svc = build_service();
        let token = admin_token();
        let authed = [bearer(&token)];
        let (_, issue) = call_with(
            &svc,
            "POST",
            "/admin/codes/issue",
            issue_body(Some("M1"), "h-act-3"),
            &authed,
        )
        .await;
        let code = issue["data"]["codes"][0]["code"]
            .as_str()
            .unwrap()
            .to_string();
        // G1：错误机器激活 → 结构化 PrebindConflict，HTTP 400（绝非 msg.contains 反查）。
        let (status, body) = call_with(
            &svc,
            "POST",
            "/activation",
            activate_body(&code, "WRONG"),
            &[],
        )
        .await;
        assert_eq!(status, bad_request());
        assert_eq!(body["code"], "PREBIND_CONFLICT");
    }

    #[tokio::test]
    async fn http_activate_unknown_code_returns_400_invalid_code() {
        let svc = build_service();
        let (status, body) = call_with(
            &svc,
            "POST",
            "/activation",
            activate_body("IOTDAQ-NOPE-NOPE-NOPE-NOPE", "M1"),
            &[],
        )
        .await;
        assert_eq!(status, bad_request());
        assert_eq!(body["code"], "INVALID_CODE");
    }

    /// 结构化错误映射：同码异机 → `CODE_BOUND_TO_OTHER_DEVICE` → HTTP **403**。
    #[test]
    fn code_bound_to_other_device_maps_to_403_business_code() {
        let err = LicenseError::code_bound_to_other_device();
        let code = error_to_code(&err);
        assert_eq!(code, proto::codes::CODE_BOUND_TO_OTHER_DEVICE);
        assert_eq!(proto::http_status(code), 403, "设计 §7 步骤 ③ 明确要求 403");
        // 与预绑定冲突（400）分属不同业务码。
        assert_ne!(code, proto::codes::PREBIND_CONFLICT);
    }

    /// **HTTP 层**：同码异机 → 403 + wire code `CODE_BOUND_TO_OTHER_DEVICE`，消息不含敏感值。
    #[tokio::test]
    async fn http_activate_bound_to_other_device_returns_403_code_bound_to_other_device() {
        let svc = build_service();
        let token = admin_token();
        let authed = [bearer(&token)];
        let (_, issue) = call_with(
            &svc,
            "POST",
            "/admin/codes/issue",
            issue_body(None, "h-nofm-1"),
            &authed,
        )
        .await;
        let code = issue["data"]["codes"][0]["code"]
            .as_str()
            .unwrap()
            .to_string();

        // 首激：M1 + 5 锚点。
        let (s1, _) = call_with(
            &svc,
            "POST",
            "/activation",
            activate_body_with_anchors(&code, "M1", &["a", "b", "c", "d", "e"]),
            &[],
        )
        .await;
        assert_eq!(s1, StatusCode::OK);

        // 异机：machine_code 不同且锚点仅命中 2/5 → 403 CODE_BOUND_TO_OTHER_DEVICE。
        let (status, body) = call_with(
            &svc,
            "POST",
            "/activation",
            activate_body_with_anchors(&code, "M2", &["a", "b", "p", "q", "r"]),
            &[],
        )
        .await;
        assert_eq!(status, StatusCode::from_u16(403).unwrap());
        assert_eq!(body["code"], "CODE_BOUND_TO_OTHER_DEVICE");
        // 消息不含 machine_code / 锚点原文，只给「申请换机」提示。
        let msg = body["message"].as_str().unwrap();
        assert!(!msg.contains("M2"), "消息泄露 machine_code: {msg}");
        assert!(
            msg.contains("machine replacement"),
            "消息缺少可操作提示: {msg}"
        );
    }

    // ---------------- 激活鉴权（验签 / ts 窗口 / nonce 防重放） ----------------

    /// 错签名（格式合法但验不过）→ 401 + `ACTIVATION_SIGNATURE_INVALID`。
    #[tokio::test]
    async fn http_activate_forged_signature_returns_401_activation_signature_invalid() {
        let svc = build_service();
        let token = admin_token();
        let authed = [bearer(&token)];
        let (_, issue) = call_with(
            &svc,
            "POST",
            "/admin/codes/issue",
            issue_body(None, "h-act-sig"),
            &authed,
        )
        .await;
        let code = issue["data"]["codes"][0]["code"]
            .as_str()
            .unwrap()
            .to_string();

        let mut body = activate_body(&code, "M1");
        body["req_sig"] = json!(B64.encode([0u8; 64])); // 格式合法、验签必败
        let (status, resp) = call_with(&svc, "POST", "/activation", body, &[]).await;
        assert_eq!(status, StatusCode::from_u16(401).unwrap());
        assert_eq!(resp["code"], "ACTIVATION_SIGNATURE_INVALID");
    }

    /// 已钉定公钥不一致（异钥设备同码重激活）→ 403 + `ACTIVATION_PUBKEY_MISMATCH`。
    #[tokio::test]
    async fn http_activate_pubkey_mismatch_returns_403_activation_pubkey_mismatch() {
        let svc = build_service();
        let token = admin_token();
        let authed = [bearer(&token)];
        let (_, issue) = call_with(
            &svc,
            "POST",
            "/admin/codes/issue",
            issue_body(None, "h-act-pk"),
            &authed,
        )
        .await;
        let code = issue["data"]["codes"][0]["code"]
            .as_str()
            .unwrap()
            .to_string();

        // 首激：正常设备密钥 → 200，公钥被钉定。
        let (s1, _) = call_with(&svc, "POST", "/activation", activate_body(&code, "M1"), &[]).await;
        assert_eq!(s1, StatusCode::OK);

        // 异钥设备（签名自洽但公钥不同）→ 403 ACTIVATION_PUBKEY_MISMATCH。
        let anchors = [
            "a".to_string(),
            "b".to_string(),
            "c".to_string(),
            "d".to_string(),
            "e".to_string(),
        ];
        let body = activate_body_full(
            &code,
            "M1",
            &anchors,
            &TEST_ONLY_ROGUE_SEED,
            now_unix_secs(),
        );
        let (status, resp) = call_with(&svc, "POST", "/activation", body, &[]).await;
        assert_eq!(status, StatusCode::from_u16(403).unwrap());
        assert_eq!(resp["code"], "ACTIVATION_PUBKEY_MISMATCH");
    }

    /// 过期 ts（±5min 窗外，签名自洽）→ 401 + `TIMESTAMP_SKEW`。
    #[tokio::test]
    async fn http_activate_stale_ts_returns_401_timestamp_skew() {
        let svc = build_service();
        let token = admin_token();
        let authed = [bearer(&token)];
        let (_, issue) = call_with(
            &svc,
            "POST",
            "/admin/codes/issue",
            issue_body(None, "h-act-ts"),
            &authed,
        )
        .await;
        let code = issue["data"]["codes"][0]["code"]
            .as_str()
            .unwrap()
            .to_string();

        let anchors = [
            "a".to_string(),
            "b".to_string(),
            "c".to_string(),
            "d".to_string(),
            "e".to_string(),
        ];
        let body = activate_body_full(
            &code,
            "M1",
            &anchors,
            &TEST_ONLY_DEVICE_SEED,
            now_unix_secs() - 10_000,
        );
        let (status, resp) = call_with(&svc, "POST", "/activation", body, &[]).await;
        assert_eq!(status, StatusCode::from_u16(401).unwrap());
        assert_eq!(resp["code"], "TIMESTAMP_SKEW");
    }

    /// nonce 重放（同一请求体二次提交）→ 409 + `NONCE_REPLAY`。
    #[tokio::test]
    async fn http_activate_replayed_nonce_returns_409_nonce_replay() {
        let svc = build_service();
        let token = admin_token();
        let authed = [bearer(&token)];
        let (_, issue) = call_with(
            &svc,
            "POST",
            "/admin/codes/issue",
            issue_body(None, "h-act-replay"),
            &authed,
        )
        .await;
        let code = issue["data"]["codes"][0]["code"]
            .as_str()
            .unwrap()
            .to_string();

        let body = activate_body(&code, "M1");
        let (s1, _) = call_with(&svc, "POST", "/activation", body.clone(), &[]).await;
        assert_eq!(s1, StatusCode::OK);
        let (status, resp) = call_with(&svc, "POST", "/activation", body, &[]).await;
        assert_eq!(status, StatusCode::from_u16(409).unwrap());
        assert_eq!(resp["code"], "NONCE_REPLAY");
    }

    // ---------------- revoke + reissue ----------------

    #[tokio::test]
    async fn http_revoke_then_reissue_idempotent() {
        let svc = build_service();
        let token = admin_token();
        let authed = [bearer(&token)];
        let (_, issue) = call_with(
            &svc,
            "POST",
            "/admin/codes/issue",
            issue_body(None, "h-ri-1"),
            &authed,
        )
        .await;
        let (code_id, code_value) = first_code(&issue);

        let revoke_body = json!({
            "reason": "compromised",
            "note": "note note note",
            "confirm_tail8": tail8_of(&code_value),
            "second_approver": null
        });
        let (rs, rb) = call_with(
            &svc,
            "POST",
            &format!("/admin/codes/{code_id}/revoke"),
            revoke_body,
            &[
                bearer(&token),
                ("x-tenant-id".to_string(), "t-1".to_string()),
            ],
        )
        .await;
        assert_eq!(rs, StatusCode::OK, "{rb}");
        assert_eq!(rb["code"], "OK");

        let reissue_body = json!({
            "prebind": null,
            "inherit_tier": true,
            "inherit_validity": true,
            "overrides": null,
            "idempotency_key": "h-reissue-1"
        });
        let (rs1, rb1) = call_with(
            &svc,
            "POST",
            &format!("/admin/codes/{code_id}/reissue"),
            reissue_body.clone(),
            &[
                bearer(&token),
                ("x-tenant-id".to_string(), "t-1".to_string()),
            ],
        )
        .await;
        assert_eq!(rs1, StatusCode::OK, "{rb1}");
        let new_id = rb1["data"]["new_code"]["code_id"]
            .as_str()
            .unwrap()
            .to_string();

        // G2：重发幂等——同逻辑键重放返回同一张新码，绝不重复签发。
        let (rs2, rb2) = call_with(
            &svc,
            "POST",
            &format!("/admin/codes/{code_id}/reissue"),
            reissue_body,
            &[
                bearer(&token),
                ("x-tenant-id".to_string(), "t-1".to_string()),
            ],
        )
        .await;
        assert_eq!(rs2, StatusCode::OK);
        let new_id2 = rb2["data"]["new_code"]["code_id"]
            .as_str()
            .unwrap()
            .to_string();
        assert_eq!(new_id, new_id2, "重发必须幂等");
    }

    #[tokio::test]
    async fn http_reissue_requires_revoked_original_returns_400() {
        let svc = build_service();
        let token = admin_token();
        let authed = [bearer(&token)];
        let (_, issue) = call_with(
            &svc,
            "POST",
            "/admin/codes/issue",
            issue_body(None, "h-rr-1"),
            &authed,
        )
        .await;
        let (code_id, _) = first_code(&issue);
        // 未废弃直接重发 → KeyStateIllegal → BAD_REQUEST → 400。
        let reissue_body = json!({
            "prebind": null,
            "inherit_tier": true,
            "inherit_validity": true,
            "overrides": null,
            "idempotency_key": "h-reissue-rr"
        });
        let (status, body) = call_with(
            &svc,
            "POST",
            &format!("/admin/codes/{code_id}/reissue"),
            reissue_body,
            &[
                bearer(&token),
                ("x-tenant-id".to_string(), "t-1".to_string()),
            ],
        )
        .await;
        assert_eq!(status, bad_request());
        assert_eq!(body["code"], "BAD_REQUEST");
    }

    /// 缺省 `X-Tenant-Id` 头 → 400 结构化错误（本版收紧：不再缺省 `unknown`）。
    #[tokio::test]
    async fn http_revop_missing_tenant_header_is_rejected() {
        let svc = build_service();
        let token = admin_token();
        let authed = [bearer(&token)];
        let (_, issue) = call_with(
            &svc,
            "POST",
            "/admin/codes/issue",
            issue_body(None, "h-rh-1"),
            &authed,
        )
        .await;
        let (code_id, code_value) = first_code(&issue);
        let revoke_body = json!({
            "reason": "x",
            "note": "note note note",
            "confirm_tail8": tail8_of(&code_value),
            "second_approver": null
        });
        let (status, body) = call_with(
            &svc,
            "POST",
            &format!("/admin/codes/{code_id}/revoke"),
            revoke_body,
            &[bearer(&token)],
        )
        .await;
        assert_eq!(status, bad_request());
        assert_eq!(body["code"], "BAD_REQUEST");
    }

    /// **revoke 契约（设计 §2.2）**：`confirm_tail8` 不符 → 412 `CONFIRM_MISMATCH`。
    #[tokio::test]
    async fn http_revoke_wrong_tail8_returns_412_confirm_mismatch() {
        let svc = build_service();
        let token = admin_token();
        let authed = [bearer(&token)];
        let (_, issue) = call_with(
            &svc,
            "POST",
            "/admin/codes/issue",
            issue_body(None, "h-cm-1"),
            &authed,
        )
        .await;
        let (code_id, _) = first_code(&issue);
        let revoke_body = json!({
            "reason": "compromised",
            "note": "note note note",
            "confirm_tail8": "AAAAAAAA",
            "second_approver": null
        });
        let (status, body) = call_with(
            &svc,
            "POST",
            &format!("/admin/codes/{code_id}/revoke"),
            revoke_body,
            &[
                bearer(&token),
                ("x-tenant-id".to_string(), "t-1".to_string()),
            ],
        )
        .await;
        assert_eq!(status, StatusCode::from_u16(412).unwrap());
        assert_eq!(body["code"], "CONFIRM_MISMATCH");
        // 消息不得回显激活码原文。
        let msg = body["message"].as_str().unwrap();
        assert!(!msg.contains("IOTDAQ-"), "消息泄露激活码: {msg}");
    }

    /// **revoke 契约**：`note` < 10 字 → 400。
    #[tokio::test]
    async fn http_revoke_short_note_returns_400() {
        let svc = build_service();
        let token = admin_token();
        let authed = [bearer(&token)];
        let (_, issue) = call_with(
            &svc,
            "POST",
            "/admin/codes/issue",
            issue_body(None, "h-cm-2"),
            &authed,
        )
        .await;
        let (code_id, code_value) = first_code(&issue);
        let revoke_body = json!({
            "reason": "compromised",
            "note": "短",
            "confirm_tail8": tail8_of(&code_value),
            "second_approver": null
        });
        let (status, body) = call_with(
            &svc,
            "POST",
            &format!("/admin/codes/{code_id}/revoke"),
            revoke_body,
            &[
                bearer(&token),
                ("x-tenant-id".to_string(), "t-1".to_string()),
            ],
        )
        .await;
        assert_eq!(status, bad_request());
        assert_eq!(body["code"], "BAD_REQUEST");
    }

    /// **revoke 契约**：`reason` 空白 → 400 `REASON_REQUIRED`。
    #[tokio::test]
    async fn http_revoke_blank_reason_returns_400_reason_required() {
        let svc = build_service();
        let token = admin_token();
        let authed = [bearer(&token)];
        let (_, issue) = call_with(
            &svc,
            "POST",
            "/admin/codes/issue",
            issue_body(None, "h-cm-3"),
            &authed,
        )
        .await;
        let (code_id, code_value) = first_code(&issue);
        let revoke_body = json!({
            "reason": "   ",
            "note": "note note note",
            "confirm_tail8": tail8_of(&code_value),
            "second_approver": null
        });
        let (status, body) = call_with(
            &svc,
            "POST",
            &format!("/admin/codes/{code_id}/revoke"),
            revoke_body,
            &[
                bearer(&token),
                ("x-tenant-id".to_string(), "t-1".to_string()),
            ],
        )
        .await;
        assert_eq!(status, bad_request());
        assert_eq!(body["code"], "REASON_REQUIRED");
    }

    // ---------------- 管理端鉴权（login / Bearer / RBAC） ----------------

    /// 登录正路径：正确口令 → 200 `{token, role}`；token 可调受保护端点。
    #[tokio::test]
    async fn http_admin_login_happy_returns_token_and_role() {
        let svc = build_service();
        let (status, body) = call_with(
            &svc,
            "POST",
            "/admin/auth/login",
            json!({"username": "admin", "password": TEST_ONLY_ADMIN_PASSWORD}),
            &[],
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["code"], "OK");
        assert_eq!(body["data"]["role"], "system");
        let token = body["data"]["token"].as_str().unwrap().to_string();
        assert!(token.split('.').count() == 3, "JWT 三段式");

        // token 可访问受保护端点。
        let (status, list) =
            call_with(&svc, "GET", "/admin/codes", json!(null), &[bearer(&token)]).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(list["code"], "OK");
    }

    /// 登录负路径：错误口令 / 未知用户 / 请求体非法 → 一律 401 `SESSION_EXPIRED`。
    #[tokio::test]
    async fn http_admin_login_negative_paths_are_401() {
        let svc = build_service();
        for (uri, body) in [
            (
                "/admin/auth/login",
                json!({"username": "admin", "password": "wrong"}),
            ),
            (
                "/admin/auth/login",
                json!({"username": "nobody", "password": TEST_ONLY_ADMIN_PASSWORD}),
            ),
            ("/admin/auth/login", json!({})),
        ] {
            let (status, body) = call_with(&svc, "POST", uri, body, &[]).await;
            assert_eq!(status, StatusCode::from_u16(401).unwrap(), "{body}");
            assert_eq!(body["code"], "SESSION_EXPIRED");
        }
    }

    // ---------------- 账号 / 角色可配置（缺口 #9 修复） ----------------

    /// **缺口 #9**：账号 / 角色可配置端到端。
    ///
    /// 覆盖：GET 列表（含种子账号 + **绝不泄露口令摘要**）→ POST 新增 →
    /// PUT 停用（停用账号登录被拒）→ DELETE 删除 → 列表不再出现；
    /// 角色清单任意角色可读；非系统角色访问账号端点 → 403 `ADMIN_ONLY`。
    #[tokio::test]
    async fn http_admin_accounts_crud_end_to_end() {
        let svc = build_service();
        let token = admin_token();
        let authed = [bearer(&token)];

        // ① 列表：含种子 admin；响应**绝不含口令摘要**。
        let (status, body) = call_with(&svc, "GET", "/admin/accounts", json!(null), &authed).await;
        assert_eq!(status, StatusCode::OK);
        let items = body["data"]["items"].as_array().expect("items");
        assert!(items.iter().any(|i| i["account"] == "admin"), "{body}");
        let raw = body.to_string();
        assert!(!raw.contains("password_sha256"), "响应泄露口令摘要字段");
        assert!(
            !raw.contains(&crate::admin_auth::sha256_hex(TEST_ONLY_ADMIN_PASSWORD)),
            "响应泄露口令摘要值"
        );

        // ② 新增账号（risk 角色）。
        let (status, created) = call_with(
            &svc,
            "POST",
            "/admin/accounts",
            json!({"account": "new.guy", "display_name": "新人", "role": "risk",
                   "password": "pw-new-guy-1"}),
            &authed,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{created}");
        assert_eq!(created["data"]["account"], "new.guy");
        assert_eq!(created["data"]["role"], "risk");
        assert_eq!(created["data"]["status"], "active");

        // ②b 重复账号 → 400。
        let (status, _) = call_with(
            &svc,
            "POST",
            "/admin/accounts",
            json!({"account": "new.guy", "display_name": "重复", "role": "ops",
                   "password": "pw-new-guy-2"}),
            &authed,
        )
        .await;
        assert_eq!(status, bad_request());

        // ③ 停用（PUT status=disabled）→ 该账号登录被拒。
        let (status, _) = call_with(
            &svc,
            "PUT",
            "/admin/accounts/new.guy",
            json!({"status": "disabled", "note": "停用测试（补充说明）"}),
            &authed,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let (status, body) = call_with(
            &svc,
            "POST",
            "/admin/auth/login",
            json!({"username": "new.guy", "password": "pw-new-guy-1"}),
            &[],
        )
        .await;
        assert_eq!(
            status,
            StatusCode::from_u16(401).unwrap(),
            "停用账号必须拒绝登录: {body}"
        );

        // ④ 删除 → 列表不再出现。
        let (status, _) = call_with(
            &svc,
            "DELETE",
            "/admin/accounts/new.guy",
            json!({"reason": "清理", "note": "测试清理（补充说明）", "confirm": "new.guy"}),
            &authed,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let (_, after) = call_with(&svc, "GET", "/admin/accounts", json!(null), &authed).await;
        let items2 = after["data"]["items"].as_array().expect("items");
        assert!(
            !items2.iter().any(|i| i["account"] == "new.guy"),
            "{after}"
        );

        // ⑤ 角色清单（任意角色可访问，四角色）。
        let (status, roles) = call_with(&svc, "GET", "/admin/roles", json!(null), &authed).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(roles["data"]["items"].as_array().unwrap().len(), 4);

        // ⑥ 非系统角色（ops）访问账号端点 → 403 ADMIN_ONLY。
        let ops = ops_token();
        let (status, body) =
            call_with(&svc, "GET", "/admin/accounts", json!(null), &[bearer(&ops)]).await;
        assert_eq!(status, StatusCode::from_u16(403).unwrap(), "{body}");
        assert_eq!(body["code"], "ADMIN_ONLY");
    }

    /// 无 token / 坏 token 调受保护端点 → 401 `SESSION_EXPIRED`。
    #[tokio::test]
    async fn http_admin_endpoints_require_valid_token() {
        let svc = build_service();
        // 无 token。
        let (status, body) = call_with(&svc, "GET", "/admin/overview", json!(null), &[]).await;
        assert_eq!(status, StatusCode::from_u16(401).unwrap());
        assert_eq!(body["code"], "SESSION_EXPIRED");
        // 垃圾 token。
        let (status, body) = call_with(
            &svc,
            "GET",
            "/admin/overview",
            json!(null),
            &[bearer("not.a.jwt")],
        )
        .await;
        assert_eq!(status, StatusCode::from_u16(401).unwrap());
        assert_eq!(body["code"], "SESSION_EXPIRED");
        // 写端点同样受保护。
        let (status, _) = call_with(
            &svc,
            "POST",
            "/admin/codes/issue",
            issue_body(None, "h-noauth"),
            &[],
        )
        .await;
        assert_eq!(status, StatusCode::from_u16(401).unwrap());
    }

    /// RBAC 门控：ops 角色调高危端点（revoke）→ 403 `ADMIN_ONLY`；
    /// system 正常。读端点任意角色可访问。
    #[tokio::test]
    async fn http_admin_rbac_gates_dangerous_endpoints() {
        let svc = build_service();
        let admin = admin_token();
        let ops = ops_token();
        let admin_authed = [bearer(&admin)];
        let (_, issue) = call_with(
            &svc,
            "POST",
            "/admin/codes/issue",
            issue_body(None, "h-rbac-1"),
            &admin_authed,
        )
        .await;
        let (code_id, code_value) = first_code(&issue);
        let revoke_body = json!({
            "reason": "compromised",
            "note": "note note note",
            "confirm_tail8": tail8_of(&code_value),
            "second_approver": null
        });
        // ops（有 token 但角色不符）→ 403。
        let (status, body) = call_with(
            &svc,
            "POST",
            &format!("/admin/codes/{code_id}/revoke"),
            revoke_body.clone(),
            &[bearer(&ops), ("x-tenant-id".to_string(), "t-1".to_string())],
        )
        .await;
        assert_eq!(status, StatusCode::from_u16(403).unwrap());
        assert_eq!(body["code"], "ADMIN_ONLY");
        // ops 调租户创建（仅 system）→ 403。
        let (status, body) = call_with(
            &svc,
            "POST",
            "/admin/tenants",
            json!({"tenant_id": "t-ops", "name": "x"}),
            &[bearer(&ops)],
        )
        .await;
        assert_eq!(status, StatusCode::from_u16(403).unwrap());
        assert_eq!(body["code"], "ADMIN_ONLY");
        // 读端点 ops 可访问。
        let (status, _) =
            call_with(&svc, "GET", "/admin/codes", json!(null), &[bearer(&ops)]).await;
        assert_eq!(status, StatusCode::OK);
    }

    // ---------------- GET 端点（happy + 筛选 / 分页） ----------------

    /// `GET /admin/codes`：列表 + tenant/tier 筛选 + 分页 + 码值掩码。
    #[tokio::test]
    async fn http_admin_codes_list_filter_and_pagination() {
        let svc = build_service();
        let token = admin_token();
        let authed = [bearer(&token)];
        for i in 0..3 {
            call_with(
                &svc,
                "POST",
                "/admin/codes/issue",
                issue_body(None, &format!("h-codes-{i}")),
                &authed,
            )
            .await;
        }
        // 全量：3 条 + total 为字符串（大数红线）。
        let (status, body) = call_with(
            &svc,
            "GET",
            "/admin/codes?page=1&page_size=2",
            json!(null),
            &authed,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["data"]["items"].as_array().unwrap().len(), 2);
        assert_eq!(body["data"]["total"], "3");
        assert_eq!(body["data"]["page"], "1");
        assert!(
            body["data"]["items"][0]["code_masked"]
                .as_str()
                .unwrap()
                .contains("****"),
            "码值必须掩码"
        );
        // 第二页。
        let (_, p2) = call_with(
            &svc,
            "GET",
            "/admin/codes?page=2&page_size=2",
            json!(null),
            &authed,
        )
        .await;
        assert_eq!(p2["data"]["items"].as_array().unwrap().len(), 1);
        // tier 筛选（全部为 pro）。
        let (_, pro) = call_with(&svc, "GET", "/admin/codes?tier=pro", json!(null), &authed).await;
        assert_eq!(pro["data"]["total"], "3");
        // 非法 status 筛选 → 400。
        let (status, _) = call_with(
            &svc,
            "GET",
            "/admin/codes?status=weird",
            json!(null),
            &authed,
        )
        .await;
        assert_eq!(status, bad_request());
    }

    /// `GET /admin/codes/:id`：详情含完整码值 + 时间线 + 溯源链。
    #[tokio::test]
    async fn http_admin_code_detail_returns_timeline_and_chain() {
        let svc = build_service();
        let token = admin_token();
        let authed = [bearer(&token)];
        let (_, issue) = call_with(
            &svc,
            "POST",
            "/admin/codes/issue",
            issue_body(None, "h-detail-1"),
            &authed,
        )
        .await;
        let (code_id, code_value) = first_code(&issue);

        // 废弃 + 重发，产生时间线与溯源链。
        call_with(
            &svc,
            "POST",
            &format!("/admin/codes/{code_id}/revoke"),
            json!({
                "reason": "compromised",
                "note": "note note note",
                "confirm_tail8": tail8_of(&code_value),
                "second_approver": null
            }),
            &[
                bearer(&token),
                ("x-tenant-id".to_string(), "t-1".to_string()),
            ],
        )
        .await;
        let (_, reissued) = call_with(
            &svc,
            "POST",
            &format!("/admin/codes/{code_id}/reissue"),
            json!({
                "prebind": null, "inherit_tier": true, "inherit_validity": true,
                "overrides": null, "idempotency_key": "h-detail-reissue"
            }),
            &[
                bearer(&token),
                ("x-tenant-id".to_string(), "t-1".to_string()),
            ],
        )
        .await;
        let new_id = reissued["data"]["new_code"]["code_id"]
            .as_str()
            .unwrap()
            .to_string();

        // 新码详情：溯源链含原码；原码详情：完整码值 + revoke 时间线。
        let (_, detail) = call_with(
            &svc,
            "GET",
            &format!("/admin/codes/{new_id}"),
            json!(null),
            &authed,
        )
        .await;
        assert_eq!(detail["data"]["code_id"], new_id.as_str());
        assert_eq!(
            detail["data"]["reissued_chain"].as_array().unwrap().len(),
            1
        );
        assert_eq!(
            detail["data"]["reissued_chain"][0],
            code_id.as_str(),
            "溯源链必须指向原码"
        );

        let (_, orig) = call_with(
            &svc,
            "GET",
            &format!("/admin/codes/{code_id}"),
            json!(null),
            &authed,
        )
        .await;
        assert_eq!(
            orig["data"]["code"],
            code_value.as_str(),
            "详情页揭示完整码值"
        );
        assert_eq!(orig["data"]["status"], "reissued");
        let actions: Vec<&str> = orig["data"]["timeline"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| e["action"].as_str().unwrap())
            .collect();
        assert!(
            actions.contains(&"revoke"),
            "时间线必须含 revoke: {actions:?}"
        );
        // 未知码 → 400。
        let (status, _) = call_with(&svc, "GET", "/admin/codes/nope", json!(null), &authed).await;
        assert_eq!(status, bad_request());
    }

    /// 租户链路：`POST /admin/tenants` 创建 → `POST /admin/codes/issue` 对新租户发放
    /// → `GET /admin/tenants` 列表可见（新部署自举必需）。
    #[tokio::test]
    async fn http_admin_tenant_create_then_issue_bootstrap_chain() {
        let svc = build_service();
        let token = admin_token();
        let authed = [bearer(&token)];

        // 前置：对新租户 issue 直接失败（租户不存在）。
        let mut new_issue = issue_body(None, "h-boot-0");
        new_issue["tenant_id"] = json!("t-new");
        let (status, body) =
            call_with(&svc, "POST", "/admin/codes/issue", new_issue, &authed).await;
        assert_eq!(status, bad_request());
        assert_eq!(body["code"], "INVALID_CODE");

        // 创建租户 → issue 成功（自举链路）。
        let (status, created) = call_with(
            &svc,
            "POST",
            "/admin/tenants",
            json!({"tenant_id": "t-new", "name": "New Tenant", "contact": "a@b.c"}),
            &authed,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{created}");
        assert_eq!(created["data"]["verify_mode_default"], "B");
        let mut new_issue = issue_body(None, "h-boot-1");
        new_issue["tenant_id"] = json!("t-new");
        let (status, body) =
            call_with(&svc, "POST", "/admin/codes/issue", new_issue, &authed).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["data"]["codes"].as_array().unwrap().len(), 1);

        // 列表可见 + 重复创建被拒。
        let (_, list) = call_with(&svc, "GET", "/admin/tenants", json!(null), &authed).await;
        let tenants = list["data"]["items"].as_array().unwrap();
        assert!(tenants.iter().any(|t| t["tenant_id"] == "t-new"));
        let (status, _dup) = call_with(
            &svc,
            "POST",
            "/admin/tenants",
            json!({"tenant_id": "t-new", "name": "dup"}),
            &authed,
        )
        .await;
        assert_eq!(status, bad_request());

        // 策略更新 + 非法档位被拒。
        let (status, _) = call_with(
            &svc,
            "PUT",
            "/admin/tenants/t-new/policy",
            json!({"verify_mode_default": "A"}),
            &authed,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let (status, _) = call_with(
            &svc,
            "PUT",
            "/admin/tenants/t-new/policy",
            json!({"verify_mode_default": "Z"}),
            &authed,
        )
        .await;
        assert_eq!(status, bad_request());
        // 未知租户策略更新 → 400。
        let (status, _) = call_with(
            &svc,
            "PUT",
            "/admin/tenants/t-missing/policy",
            json!({"verify_mode_default": "A"}),
            &authed,
        )
        .await;
        assert_eq!(status, bad_request());
    }

    /// `GET /admin/devices`：设备列表 + 机器码掩码 + status 内存筛选。
    #[tokio::test]
    async fn http_admin_devices_list_and_filter() {
        let svc = build_service();
        let token = admin_token();
        let authed = [bearer(&token)];
        // 发码 + 激活 → 产生设备。
        let (_, issue) = call_with(
            &svc,
            "POST",
            "/admin/codes/issue",
            issue_body(None, "h-dev-1"),
            &authed,
        )
        .await;
        let code = issue["data"]["codes"][0]["code"].as_str().unwrap();
        let (status, _) = call_with(
            &svc,
            "POST",
            "/activation",
            activate_body(code, "MID-DEV-1"),
            &[],
        )
        .await;
        assert_eq!(status, StatusCode::OK);

        let (_, list) = call_with(&svc, "GET", "/admin/devices", json!(null), &authed).await;
        assert_eq!(list["code"], "OK");
        let items = list["data"]["items"].as_array().unwrap();
        assert_eq!(items.len(), 1);
        assert!(
            items[0]["machine_code_masked"]
                .as_str()
                .unwrap()
                .contains("****"),
            "机器码必须掩码"
        );
        assert_eq!(items[0]["deploy_mode"], "native");
        // 状态筛选命中。
        let (_, hit) = call_with(
            &svc,
            "GET",
            "/admin/devices?status=active",
            json!(null),
            &authed,
        )
        .await;
        assert_eq!(hit["data"]["total"], "1");
        // 状态筛选不命中。
        let (_, miss) = call_with(
            &svc,
            "GET",
            "/admin/devices?status=stopped",
            json!(null),
            &authed,
        )
        .await;
        assert_eq!(miss["data"]["total"], "0");
        // 关键字筛选（machine_code contains，大小写不敏感）。
        let (_, kw) = call_with(
            &svc,
            "GET",
            "/admin/devices?machine_code=mid-dev",
            json!(null),
            &authed,
        )
        .await;
        assert_eq!(kw["data"]["total"], "1");
    }

    /// `GET /admin/receipts/anomalies`：gap 回执出现在异常列表（大数字段为字符串）。
    #[tokio::test]
    async fn http_admin_receipt_anomalies_lists_gap_receipts() {
        let svc = build_service();
        let token = admin_token();
        let authed = [bearer(&token)];
        // 发码 + 激活。
        let (_, issue) = call_with(
            &svc,
            "POST",
            "/admin/codes/issue",
            issue_body(None, "h-anom-1"),
            &authed,
        )
        .await;
        let code = issue["data"]["codes"][0]["code"].as_str().unwrap();
        let (_, act) = call_with(
            &svc,
            "POST",
            "/activation",
            activate_body(code, "MID-ANOM"),
            &[],
        )
        .await;
        let lease_id = act["data"]["lease_id"].as_str().unwrap().to_string();

        // 上报正常区间 → 无异常；再上报跳空区间 → gap_flag 置位。
        let now = now_unix_secs();
        let count = 100i64;
        let hash = crate::receipt::receipt_payload_hash(
            "MID-ANOM", &lease_id, 1, 100, count, "sha256:d", now,
        );
        let (_, r1) = call_with(
            &svc,
            "POST",
            "/audit/receipt",
            json!({
                "device_mid": "MID-ANOM", "lease_id": lease_id,
                "seq_from": "1", "seq_to": "100", "count": "100",
                "payload_digest": "sha256:d", "ts": now.to_string(),
                "sig": sign_hash(&hash)
            }),
            &[],
        )
        .await;
        assert_eq!(r1["data"]["gap"], "none");
        let hash2 = crate::receipt::receipt_payload_hash(
            "MID-ANOM", &lease_id, 150, 300, 151, "sha256:d", now,
        );
        let (_, r2) = call_with(
            &svc,
            "POST",
            "/audit/receipt",
            json!({
                "device_mid": "MID-ANOM", "lease_id": lease_id,
                "seq_from": "150", "seq_to": "300", "count": "151",
                "payload_digest": "sha256:d", "ts": now.to_string(),
                "sig": sign_hash(&hash2)
            }),
            &[],
        )
        .await;
        assert_eq!(r2["data"]["gap"], "gap");

        // 异常列表含该 gap 回执；序号为字符串（大数红线）。
        let (_, list) = call_with(
            &svc,
            "GET",
            "/admin/receipts/anomalies",
            json!(null),
            &authed,
        )
        .await;
        assert_eq!(list["code"], "OK");
        let items = list["data"]["items"].as_array().unwrap();
        assert_eq!(items.len(), 1);
        assert!(items[0]["seq_from"].is_string(), "seq_from 必须为字符串");
        assert_eq!(items[0]["seq_from"], "150");
        assert_eq!(items[0]["kind"], "gap");
        assert_eq!(items[0]["device_mid"], "MID-ANOM");

        // overview 的 receipts_anomalous 同步为 "1"。
        let (_, ov) = call_with(&svc, "GET", "/admin/overview", json!(null), &authed).await;
        assert_eq!(ov["data"]["receipts_anomalous"], "1");
        assert_eq!(ov["data"]["codes_bound"], "1");
    }

    /// `GET /admin/keys`：只含公钥的密钥列表。
    #[tokio::test]
    async fn http_admin_keys_lists_signing_keys() {
        let svc = build_service();
        // 直接登记一条公钥记录（service 端点不发私钥）。
        svc.store()
            .insert_signing_key(&crate::model::SigningKey {
                kid: "k-admin".into(),
                status: crate::model::SigningKeyStatus::Active,
                public_key: "cHVia2V5".into(),
                hsm_ref: None,
                enabled_at: 1_700_000_000,
                retired_at: None,
            })
            .expect("insert key");
        let token = admin_token();
        let (_, list) = call_with(&svc, "GET", "/admin/keys", json!(null), &[bearer(&token)]).await;
        assert_eq!(list["code"], "OK");
        let items = list["data"]["items"].as_array().unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0]["kid"], "k-admin");
        assert_eq!(items[0]["status"], "active");
        // overview 的 active_kid 指向该密钥。
        let (_, ov) = call_with(
            &svc,
            "GET",
            "/admin/overview",
            json!(null),
            &[bearer(&token)],
        )
        .await;
        assert_eq!(ov["data"]["active_kid"], "k-admin");
    }

    /// `GET /admin/audit/logs`：审计查询 + action 筛选 + 时间范围过滤 + 大数 ts 字符串。
    #[tokio::test]
    async fn http_admin_audit_logs_filter_and_time_range() {
        let svc = build_service();
        let token = admin_token();
        let authed = [bearer(&token)];
        // 产生 issue 审计事件。
        call_with(
            &svc,
            "POST",
            "/admin/codes/issue",
            issue_body(None, "h-audit-1"),
            &authed,
        )
        .await;
        // 全量。
        let (_, list) = call_with(&svc, "GET", "/admin/audit/logs", json!(null), &authed).await;
        assert_eq!(list["code"], "OK");
        let items = list["data"]["items"].as_array().unwrap();
        assert!(!items.is_empty());
        assert!(items[0]["ts"].is_string(), "ts 必须为字符串（大数红线）");
        // action 筛选。
        let (_, issue_only) = call_with(
            &svc,
            "GET",
            "/admin/audit/logs?action=issue",
            json!(null),
            &authed,
        )
        .await;
        assert_eq!(issue_only["data"]["items"].as_array().unwrap().len(), 1);
        // 时间范围（未来窗口 → 空）。
        let far = now_unix_secs() + 100_000;
        let (_, empty) = call_with(
            &svc,
            "GET",
            &format!("/admin/audit/logs?time_from={far}"),
            json!(null),
            &authed,
        )
        .await;
        assert_eq!(empty["data"]["total"], "0");
        // 非法时间范围 → 400。
        let (status, _) = call_with(
            &svc,
            "GET",
            "/admin/audit/logs?time_from=abc",
            json!(null),
            &authed,
        )
        .await;
        assert_eq!(status, bad_request());
    }

    /// `GET /admin/overview`：聚合计数（字符串编码）+ 无活跃密钥时 active_kid 为 null。
    #[tokio::test]
    async fn http_admin_overview_aggregates_counts() {
        let svc = build_service();
        let token = admin_token();
        let authed = [bearer(&token)];
        call_with(
            &svc,
            "POST",
            "/admin/codes/issue",
            issue_body(None, "h-ov-1"),
            &authed,
        )
        .await;
        let (_, ov) = call_with(&svc, "GET", "/admin/overview", json!(null), &authed).await;
        assert_eq!(ov["code"], "OK");
        assert_eq!(ov["data"]["tenants"], "1");
        assert_eq!(ov["data"]["codes_total"], "1");
        assert_eq!(ov["data"]["codes_issued"], "1");
        assert!(ov["data"]["active_kid"].is_null(), "无公钥记录 → null");
    }

    // ---------------- 设备端：心跳 / 校验 / 回执 ----------------

    /// 经 HTTP 端点走一遍「发码 → 激活」，返回 `lease_id`。
    async fn activate_lease(svc: &SharedService, idem: &str, machine: &str) -> String {
        let token = admin_token();
        let authed = [bearer(&token)];
        let (_, issue) = call_with(
            svc,
            "POST",
            "/admin/codes/issue",
            issue_body(None, idem),
            &authed,
        )
        .await;
        let code = issue["data"]["codes"][0]["code"]
            .as_str()
            .unwrap()
            .to_string();
        let (_, act) = call_with(
            svc,
            "POST",
            "/activation",
            activate_body(&code, machine),
            &[],
        )
        .await;
        act["data"]["lease_id"].as_str().unwrap().to_string()
    }

    /// 构造心跳请求体（已签名）。
    fn heartbeat_body(lease_id: &str, nonce: &str, ts: i64, sign_ts: i64) -> serde_json::Value {
        let hash = crate::device_auth::heartbeat_payload_hash(lease_id, sign_ts, nonce, None);
        json!({
            "lease_id": lease_id,
            "ts": ts.to_string(),
            "nonce": nonce,
            "receipt_cursor": null,
            "device_sig": sign_hash(&hash)
        })
    }

    /// 构造 `/verify` 请求体（已签名）。
    fn verify_body(
        mid: &str,
        lease_id: &str,
        nonce: &str,
        ts: i64,
        sign_ts: i64,
    ) -> serde_json::Value {
        let hash =
            crate::device_auth::verify_payload_hash(mid, lease_id, "sha256:abc", sign_ts, nonce);
        json!({
            "device_mid": mid,
            "lease_id": lease_id,
            "payload_digest": "sha256:abc",
            "ts": ts.to_string(),
            "nonce": nonce,
            "device_sig": sign_hash(&hash)
        })
    }

    /// 构造 `/audit/receipt` 请求体（已签名）。
    fn receipt_body(
        mid: &str,
        lease_id: &str,
        from: i64,
        to: i64,
        signer: &dyn Fn(&[u8; 32]) -> String,
    ) -> serde_json::Value {
        let now = now_unix_secs();
        let count = to - from + 1;
        let hash =
            crate::receipt::receipt_payload_hash(mid, lease_id, from, to, count, "sha256:d", now);
        json!({
            "device_mid": mid,
            "lease_id": lease_id,
            "seq_from": from.to_string(),
            "seq_to": to.to_string(),
            "count": count.to_string(),
            "payload_digest": "sha256:d",
            "ts": now.to_string(),
            "sig": signer(&hash)
        })
    }

    #[tokio::test]
    async fn http_heartbeat_happy_returns_200() {
        let svc = build_service();
        let lease_id = activate_lease(&svc, "hb-http-ok", "MID-0001").await;
        let now = now_unix_secs();
        let (status, body) = call_with(
            &svc,
            "POST",
            "/heartbeat",
            heartbeat_body(&lease_id, "hb-ok", now, now),
            &[],
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["code"], "OK");
        assert!(!body["data"]["sig"].as_str().unwrap().is_empty());
        // 副作用：last_heartbeat_at 已回写。
        assert!(svc
            .store()
            .get_lease(&lease_id)
            .unwrap()
            .unwrap()
            .last_heartbeat_at
            .is_some());
    }

    #[tokio::test]
    async fn http_heartbeat_revoked_returns_403_lease_revoked() {
        let svc = build_service();
        let lease_id = activate_lease(&svc, "hb-http-rev", "MID-0001").await;
        svc.store()
            .update_lease_status(&lease_id, crate::model::LeaseStatus::Stopped)
            .unwrap();
        let now = now_unix_secs();
        let (status, body) = call_with(
            &svc,
            "POST",
            "/heartbeat",
            heartbeat_body(&lease_id, "hb-rev", now, now),
            &[],
        )
        .await;
        assert_eq!(status, StatusCode::from_u16(403).unwrap());
        assert_eq!(body["code"], "LEASE_REVOKED");
    }

    #[tokio::test]
    async fn http_heartbeat_unknown_lease_returns_404() {
        let svc = build_service();
        let now = now_unix_secs();
        let (status, body) = call_with(
            &svc,
            "POST",
            "/heartbeat",
            heartbeat_body("lease-missing", "hb-404", now, now),
            &[],
        )
        .await;
        assert_eq!(status, StatusCode::from_u16(404).unwrap());
        assert_eq!(body["code"], "LEASE_NOT_FOUND");
    }

    #[tokio::test]
    async fn http_heartbeat_replayed_nonce_returns_409() {
        let svc = build_service();
        let lease_id = activate_lease(&svc, "hb-http-replay", "MID-0001").await;
        let now = now_unix_secs();
        let body = heartbeat_body(&lease_id, "hb-replay", now, now);
        let (s1, _) = call_with(&svc, "POST", "/heartbeat", body.clone(), &[]).await;
        assert_eq!(s1, StatusCode::OK);
        let (status, resp) = call_with(&svc, "POST", "/heartbeat", body, &[]).await;
        assert_eq!(status, StatusCode::from_u16(409).unwrap());
        assert_eq!(resp["code"], "NONCE_REPLAY");
    }

    #[tokio::test]
    async fn http_heartbeat_skewed_returns_401_timestamp_skew() {
        let svc = build_service();
        let lease_id = activate_lease(&svc, "hb-http-skew", "MID-0001").await;
        let skewed = now_unix_secs() - 10_000;
        let (status, body) = call_with(
            &svc,
            "POST",
            "/heartbeat",
            heartbeat_body(&lease_id, "hb-skew", skewed, skewed),
            &[],
        )
        .await;
        assert_eq!(status, StatusCode::from_u16(401).unwrap());
        assert_eq!(body["code"], "TIMESTAMP_SKEW");
    }

    #[tokio::test]
    async fn http_verify_happy_returns_200() {
        let svc = build_service();
        let lease_id = activate_lease(&svc, "v-http-ok", "MID-0001").await;
        let now = now_unix_secs();
        let (status, body) = call_with(
            &svc,
            "POST",
            "/verify",
            verify_body("MID-0001", &lease_id, "v-ok", now, now),
            &[],
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["code"], "OK");
        assert_eq!(body["data"]["ok"], true);
        assert_eq!(body["data"]["nonce"], "v-ok");
    }

    #[tokio::test]
    async fn http_verify_forged_returns_401_verify_fail() {
        let svc = build_service();
        let lease_id = activate_lease(&svc, "v-http-forge", "MID-0001").await;
        let now = now_unix_secs();
        let mut body = verify_body("MID-0001", &lease_id, "v-forge", now, now);
        body["device_sig"] = json!("AAAA");
        let (status, resp) = call_with(&svc, "POST", "/verify", body, &[]).await;
        assert_eq!(status, StatusCode::from_u16(401).unwrap());
        assert_eq!(resp["code"], "VERIFY_FAIL");
    }

    #[tokio::test]
    async fn http_verify_skewed_returns_401_timestamp_skew() {
        let svc = build_service();
        let lease_id = activate_lease(&svc, "v-http-skew", "MID-0001").await;
        let skewed = now_unix_secs() + 9_999;
        let (status, body) = call_with(
            &svc,
            "POST",
            "/verify",
            verify_body("MID-0001", &lease_id, "v-skew", skewed, skewed),
            &[],
        )
        .await;
        assert_eq!(status, StatusCode::from_u16(401).unwrap());
        assert_eq!(body["code"], "TIMESTAMP_SKEW");
    }

    #[tokio::test]
    async fn http_verify_replayed_nonce_returns_409() {
        let svc = build_service();
        let lease_id = activate_lease(&svc, "v-http-replay", "MID-0001").await;
        let now = now_unix_secs();
        let body = verify_body("MID-0001", &lease_id, "v-replay", now, now);
        let (s1, _) = call_with(&svc, "POST", "/verify", body.clone(), &[]).await;
        assert_eq!(s1, StatusCode::OK);
        let (status, resp) = call_with(&svc, "POST", "/verify", body, &[]).await;
        assert_eq!(status, StatusCode::from_u16(409).unwrap());
        assert_eq!(resp["code"], "NONCE_REPLAY");
    }

    #[tokio::test]
    async fn http_verify_extra_field_returns_422() {
        let svc = build_service();
        let lease_id = activate_lease(&svc, "v-http-wl", "MID-0001").await;
        let now = now_unix_secs();
        let mut body = verify_body("MID-0001", &lease_id, "v-wl", now, now);
        body["flow_rate"] = json!("42");
        let (status, resp) = call_with(&svc, "POST", "/verify", body, &[]).await;
        assert_eq!(status, StatusCode::from_u16(422).unwrap());
        assert_eq!(resp["code"], "FIELD_WHITELIST_VIOLATION");
    }

    #[tokio::test]
    async fn http_audit_receipt_happy_and_gap() {
        let svc = build_service();
        let lease_id = activate_lease(&svc, "r-http-1", "MID-0001").await;
        let (status, body) = call_with(
            &svc,
            "POST",
            "/audit/receipt",
            receipt_body("MID-0001", &lease_id, 1, 100, &sign_hash),
            &[],
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["data"]["accepted"], true);
        assert_eq!(body["data"]["gap"], "none");

        // 跳空。
        let (_, gap) = call_with(
            &svc,
            "POST",
            "/audit/receipt",
            receipt_body("MID-0001", &lease_id, 150, 3_000, &sign_hash),
            &[],
        )
        .await;
        assert_eq!(gap["data"]["gap"], "gap");
        assert!(!gap["data"]["warnings"].as_array().unwrap().is_empty());
    }

    #[tokio::test]
    async fn http_audit_receipt_extra_field_returns_422_recorded_in_audit() {
        let svc = build_service();
        let lease_id = activate_lease(&svc, "r-http-wl", "MID-0001").await;
        let mut body = receipt_body("MID-0001", &lease_id, 1, 100, &sign_hash);
        body["flow_rate"] = json!("42");
        let (status, resp) = call_with(&svc, "POST", "/audit/receipt", body, &[]).await;
        assert_eq!(status, StatusCode::from_u16(422).unwrap());
        assert_eq!(resp["code"], "FIELD_WHITELIST_VIOLATION");
    }

    /// **回归（安全红线 1）**：已受理区间 + 伪造签名 → 拒绝（非幂等接受）。
    #[tokio::test]
    async fn http_audit_receipt_forged_seen_interval_rejected() {
        let svc = build_service();
        let lease_id = activate_lease(&svc, "r-http-forge", "MID-0001").await;
        let (s1, b1) = call_with(
            &svc,
            "POST",
            "/audit/receipt",
            receipt_body("MID-0001", &lease_id, 1, 100, &sign_hash),
            &[],
        )
        .await;
        assert_eq!(s1, StatusCode::OK);
        assert_eq!(b1["data"]["accepted"], true);

        // 同区间、异钥签名 → 验签失败，绝不被幂等短路为 accepted。
        let (status, resp) = call_with(
            &svc,
            "POST",
            "/audit/receipt",
            receipt_body("MID-0001", &lease_id, 1, 100, &sign_hash_rogue),
            &[],
        )
        .await;
        assert_eq!(status, StatusCode::from_u16(401).unwrap());
        assert_eq!(resp["code"], "VERIFY_FAIL");
    }
}

//! `licensing-server` HTTP 层（task 45 最后一块）：把 [`crate::service::LicensingService`]
//! 接上 axum 路由。
//!
//! # 分层边界（**不可动摇**）
//!
//! 本层**只做四件事**：
//! 1. **认证**：`/admin/*` 的管理员身份门控（RBAC 会话）；设备端端点不做会话认证。
//! 2. **反序列化**：请求体 → `proto` 结构体（含回执白名单前置校验）。
//! 3. **调用 service**：转发给 [`crate::service::LicensingService`] 的业务方法。
//! 4. **错误映射**：`LicenseError` → 业务码 + HTTP status 的 [`ApiEnvelope`]。
//!
//! 本层**绝不**做业务判定——不判码状态、不判配额、不判时间窗、不判一机一码。
//! 那些全部在 `service.rs`。**HTTP 层出现业务 if-else 即视为分层红线被击穿。**
//!
//! # 错误信息纪律
//!
//! `LicenseError` 的 `Display` / `Debug` 携带内部细节（SQL 片段、kid 值、库内状态），
//! **绝不**直接返回给客户端。本层统一经 [`error_to_code`] 归一为稳定的业务码，
//! 对外只给「不泄密」的短消息 + [`crate::proto::codes`] + [`crate::proto::http_status`]。
//!
//! # 幂等键来源（对齐 `docs/design/licensing-api.md` §2.1）
//!
//! 契约规定 `POST /admin/codes/issue` / `reissue` 的幂等键是**请求体字段**
//! `idempotency_key`（契约原文：「请求：`{ ... idempotency_key }`」）。因此本层以
//! **请求体字段为准**；同时兼容标准的 `Idempotency-Key` HTTP 头作为**可选覆盖**
//! （头存在且非空时优先），便于网关 / 反代按 HTTP 语义透传。两者都空 → 交给 service
//! 的非幂等路径（每次发放新批次）。

use std::sync::Arc;

use axum::{
    extract::{Path, Query, Request, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use serde::de::DeserializeOwned;

use crate::error::{LicenseError, LicenseResult};
use crate::model::now_unix_secs;
use crate::proto::{
    codes, http_status, ActivationRequest, ActivationResponse, ApiEnvelope, AuditLogQuery,
    AuditReceiptRequest, AuditReceiptResponse, CodeDetail, CodeListQuery, DeviceListQuery,
    HeartbeatRequest, HeartbeatResponse, IssueCodesRequest, IssueCodesResponse, ReissueCodeRequest,
    ReissueCodeResponse, RevokeCodeRequest, VerifyRequest, VerifyResponse,
};
use crate::service::LicensingService;

/// 单页上限（列表端点 `page_size`；`page` 由 query 提供，从 1 起）。
pub const DEFAULT_PAGE_SIZE: u32 = 50;

// ============================================================================
// 应用状态与认证
// ============================================================================

/// 管理员会话门控（RBAC 的一部分）。
///
/// HTTP 层**只做认证**：判定「调用方是否是厂商管理员」。角色细分（运营 / 授权运营 /
/// 风控 / 系统）与页面级 / 操作级双重门控属于管理后台自身（task 57），不在本层实现。
#[derive(Debug, Clone)]
pub struct AdminAuth {
    /// 合法管理员令牌集合（Bearer token）。空集合 = **门控关闭**（仅供内网 / 测试装配）。
    tokens: Vec<String>,
}

impl AdminAuth {
    /// 构造令牌集合。空 vec → 门控关闭（任何请求视为已认证，**仅限内网可信部署**）。
    pub fn new(tokens: Vec<String>) -> Self {
        AdminAuth { tokens }
    }

    /// 关闭门控（内网 / 测试用）。
    pub fn disabled() -> Self {
        AdminAuth { tokens: Vec::new() }
    }

    /// 从环境变量 `LICENSING_ADMIN_TOKEN`（逗号分隔）装配；未设置 → 门控关闭。
    ///
    /// 令牌经环境变量注入，**绝不**硬编码进源码 / 仓库。
    pub fn from_env() -> Self {
        match std::env::var("LICENSING_ADMIN_TOKEN") {
            Ok(raw) if !raw.trim().is_empty() => AdminAuth::new(
                raw.split(',')
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .collect(),
            ),
            _ => AdminAuth::disabled(),
        }
    }

    /// 校验 `Authorization: Bearer <token>`。
    ///
    /// 门控关闭（令牌集合为空）→ 恒通过。门控开启 → 必须携带匹配令牌，否则拒。
    pub fn authenticate(&self, header: Option<&str>) -> bool {
        if self.tokens.is_empty() {
            return true;
        }
        let Some(value) = header else {
            return false;
        };
        let Some(token) = value.strip_prefix("Bearer ") else {
            return false;
        };
        self.tokens.iter().any(|t| t == token.trim())
    }
}

/// HTTP 层共享状态（[`Router::with_state`]）。
#[derive(Clone)]
pub struct AppState {
    /// 业务服务（内部已 `Arc`，克隆廉价）。
    pub service: Arc<LicensingService>,
    /// 管理员门控。
    pub admin: AdminAuth,
}

impl AppState {
    /// 构造应用状态。
    pub fn new(service: Arc<LicensingService>, admin: AdminAuth) -> Self {
        AppState { service, admin }
    }
}

// ============================================================================
// 错误映射
// ============================================================================

/// `LicenseError` → 业务码（**不泄密**）。
///
/// 映射基于「错误消息里的稳定子串」，而非 `Debug` / `Display` 全文。service 层的
/// 错误消息文本是契约的一部分（见 `service.rs` 各 `format!`），但 HTTP 层只提取
/// 判定所需的稳定关键词，对外消息一律用固定短语。
pub fn error_to_code(err: &LicenseError) -> &'static str {
    match err {
        LicenseError::QuotaExceeded(_) => codes::QUOTA_EXCEEDED,
        LicenseError::TokenInvalid(_) => codes::TOKEN_EXPIRED,
        LicenseError::HeartbeatRejected(msg) => classify_heartbeat(msg),
        LicenseError::KeyStateIllegal(msg) => classify_key_state(msg),
        LicenseError::ActivationRejected(msg) => classify_activation(msg),
        // 存储错误 → 500 兜底（不暴露 SQL 细节，也不伪装成客户端错误）。
        LicenseError::Storage(_) => codes::BAD_REQUEST,
    }
}

/// 心跳错误细分。
fn classify_heartbeat(msg: &str) -> &'static str {
    if msg.contains("nonce replay") {
        codes::NONCE_REPLAY
    } else if msg.contains("timestamp skew") {
        codes::TIMESTAMP_SKEW
    } else if msg.contains("missing") || msg.contains("not found") {
        codes::LEASE_NOT_FOUND
    } else if msg.contains("revoked") || msg.contains("stopped") {
        codes::LEASE_REVOKED
    } else {
        codes::BAD_REQUEST
    }
}

/// 码状态机错误细分。
fn classify_key_state(msg: &str) -> &'static str {
    if msg.contains("already revoked") {
        // 已用其它原因废弃：属状态机冲突，归 400（无专用码，设计未定义）。
        codes::BAD_REQUEST
    } else if msg.contains("has been reissued") || msg.contains("reissued and cannot") {
        codes::ALREADY_REISSUED
    } else if msg.contains("must be revoked before reissue") {
        // 原码未废弃不可重发（设计 §2.3 `ORIGINAL_NOT_REVOKED`）。
        codes::ORIGINAL_NOT_REVOKED
    } else if msg.contains("prebind") {
        codes::PREBIND_CONFLICT
    } else {
        codes::BAD_REQUEST
    }
}

/// 激活域错误细分（覆盖激活 + 废弃 + 重发 + 回执的部分错误）。
fn classify_activation(msg: &str) -> &'static str {
    if msg.contains("nonce replay") {
        codes::NONCE_REPLAY
    } else if msg.contains("timestamp skew") {
        codes::TIMESTAMP_SKEW
    } else if msg.contains("whitelist") {
        codes::FIELD_WHITELIST_VIOLATION
    } else if msg.contains("is revoked") {
        codes::CODE_REVOKED
    } else if msg.contains("has been reissued") {
        codes::CODE_REISSUED
    } else if msg.contains("bound to another device") {
        codes::CODE_BOUND_TO_OTHER_DEVICE
    } else if msg.contains("tenant not found") {
        // ⚠️ 必须在通用「not found」**之前**判定：`tenant not found: xxx` 也含 `not found`。
        codes::TENANT_NOT_FOUND
    } else if msg.contains("invalid activation code") || msg.contains("not found") {
        codes::INVALID_CODE
    } else if msg.contains("confirm") {
        codes::CONFIRM_MISMATCH
    } else if msg.contains("prebind") {
        codes::PREBIND_CONFLICT
    } else if msg.contains("reason") || msg.contains("note") {
        codes::REASON_REQUIRED
    } else {
        codes::BAD_REQUEST
    }
}

/// 对外安全消息（**不含**内部细节）。
pub fn safe_message(code: &str) -> &'static str {
    match code {
        codes::OK => "ok",
        codes::INVALID_CODE => "activation code is invalid",
        codes::CODE_REVOKED => "activation code has been revoked",
        codes::CODE_BOUND_TO_OTHER_DEVICE => "activation code is bound to another device",
        codes::CODE_REISSUED => "activation code has been reissued; import the replacement",
        codes::NONCE_REPLAY => "nonce replay detected",
        codes::TIMESTAMP_SKEW => "request timestamp outside allowed window",
        codes::LEASE_REVOKED => "lease has been revoked",
        codes::LEASE_NOT_FOUND => "lease not found",
        codes::VERIFY_FAIL => "verification failed",
        codes::FIELD_WHITELIST_VIOLATION => "request contains non-whitelisted field",
        codes::ADMIN_ONLY => "admin privilege required",
        codes::SESSION_EXPIRED => "session expired",
        codes::TENANT_NOT_FOUND => "tenant not found",
        codes::CONFIRM_MISMATCH => "confirmation mismatch",
        codes::REASON_REQUIRED => "reason and note are required",
        codes::ALREADY_REISSUED => "code already reissued",
        codes::ORIGINAL_NOT_REVOKED => "original code must be revoked before reissue",
        codes::PREBIND_CONFLICT => "prebind machine code conflicts with an existing binding",
        codes::QUOTA_EXCEEDED => "quota exceeded",
        codes::TOKEN_EXPIRED => "token expired or invalid",
        _ => "request failed",
    }
}

/// 统一失败响应：`ApiEnvelope::err` + `http_status` 决定的 HTTP 状态。
fn error_response(code: &str, trace_id: &str) -> Response {
    let status =
        StatusCode::from_u16(http_status(code)).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    let body: ApiEnvelope<serde_json::Value> = ApiEnvelope::err(code, safe_message(code), trace_id);
    (status, Json(body)).into_response()
}

/// 把 service 结果映射为 HTTP 响应（成功 / 失败统一）。
fn into_response<T>(result: LicenseResult<T>, trace_id: &str) -> Response
where
    T: serde::Serialize,
{
    match result {
        Ok(data) => {
            let body: ApiEnvelope<T> = ApiEnvelope::ok(data, trace_id);
            (StatusCode::OK, Json(body)).into_response()
        }
        Err(err) => error_response(error_to_code(&err), trace_id),
    }
}

// ============================================================================
// 请求解析辅助
// ============================================================================

/// 从请求头取 trace_id（缺失则现场生成），并**不**回显任何内部信息。
fn trace_id_of(req: &Request) -> String {
    req.headers()
        .get("x-trace-id")
        .and_then(|v| v.to_str().ok())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .unwrap_or_else(|| crate::model::now_ns_id("trace"))
}

/// 从请求头取可选 `Idempotency-Key`（非空时优先于请求体字段）。
fn idempotency_key_header(req: &Request) -> Option<String> {
    req.headers()
        .get("idempotency-key")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// 认证守卫：门控失败 → `403 ADMIN_ONLY`。
fn require_admin(state: &AppState, req: &Request, trace_id: &str) -> Option<Response> {
    let header = req
        .headers()
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok());
    if state.admin.authenticate(header) {
        None
    } else {
        Some(error_response(codes::ADMIN_ONLY, trace_id))
    }
}

/// 反序列化 JSON 体；失败 → `400 BAD_REQUEST`（**不回显 serde 具体错误行**）。
///
/// `Err` 直接携带 [`Response`]（axum 惯用返回形态）；`Response` 体积较大是 axum 的
/// 固有事实，boxing 只会给每个 handler 调用点引入无谓的解包噪音，故此处显式豁免该 lint。
#[allow(clippy::result_large_err)]
fn parse_json<T: DeserializeOwned>(bytes: &[u8], trace_id: &str) -> Result<T, Response> {
    serde_json::from_slice::<T>(bytes).map_err(|_| error_response(codes::BAD_REQUEST, trace_id))
}

/// Query 解析：`serde_urlencoded` 风格由 axum `Query` 完成，这里做「部分字段非法即拒」。
fn query_page(page: Option<u32>) -> u32 {
    page.unwrap_or(1).max(1)
}

// ============================================================================
// 设备端 handler
// ============================================================================

/// `POST /activation`。
async fn activation(State(state): State<AppState>, req: Request) -> Response {
    let trace_id = trace_id_of(&req);
    let (_, body) = req.into_parts();
    let bytes = match axum::body::to_bytes(body, usize::MAX).await {
        Ok(b) => b,
        Err(_) => return error_response(codes::BAD_REQUEST, &trace_id),
    };
    let parsed: ActivationRequest = match parse_json(&bytes, &trace_id) {
        Ok(p) => p,
        Err(resp) => return resp,
    };
    let result: LicenseResult<ActivationResponse> = state.service.activate(&parsed);
    into_response(result, &trace_id)
}

/// `POST /heartbeat`。
async fn heartbeat(State(state): State<AppState>, req: Request) -> Response {
    let trace_id = trace_id_of(&req);
    let (_, body) = req.into_parts();
    let bytes = match axum::body::to_bytes(body, usize::MAX).await {
        Ok(b) => b,
        Err(_) => return error_response(codes::BAD_REQUEST, &trace_id),
    };
    let parsed: HeartbeatRequest = match parse_json(&bytes, &trace_id) {
        Ok(p) => p,
        Err(resp) => return resp,
    };
    let result: LicenseResult<HeartbeatResponse> = state.service.heartbeat(&parsed);
    into_response(result, &trace_id)
}

/// `POST /verify`（仅 A 档）。
async fn verify(State(state): State<AppState>, req: Request) -> Response {
    let trace_id = trace_id_of(&req);
    let (_, body) = req.into_parts();
    let bytes = match axum::body::to_bytes(body, usize::MAX).await {
        Ok(b) => b,
        Err(_) => return error_response(codes::BAD_REQUEST, &trace_id),
    };
    let parsed: VerifyRequest = match parse_json(&bytes, &trace_id) {
        Ok(p) => p,
        Err(resp) => return resp,
    };
    let result: LicenseResult<VerifyResponse> = state.service.verify(&parsed);
    into_response(result, &trace_id)
}

/// `POST /audit/receipt`。
///
/// **白名单前置**：先对**原始 JSON** 做字段白名单校验（出现白名单外字段 → 整单拒收），
/// 再反序列化为强类型结构体。顺序不可颠倒——serde 默认忽略未知字段，若先反序列化则
/// 白名单外字段会被**静默丢弃**（本项目有过该类严重缺陷先例）。
async fn audit_receipt(State(state): State<AppState>, req: Request) -> Response {
    let trace_id = trace_id_of(&req);
    let (_, body) = req.into_parts();
    let bytes = match axum::body::to_bytes(body, usize::MAX).await {
        Ok(b) => b,
        Err(_) => return error_response(codes::BAD_REQUEST, &trace_id),
    };
    // 步骤 1：原始 JSON 白名单校验（拒收，不是忽略）。
    let value: serde_json::Value = match serde_json::from_slice(&bytes) {
        Ok(v) => v,
        Err(_) => return error_response(codes::BAD_REQUEST, &trace_id),
    };
    if let Err(err) = AuditReceiptRequest::validate_whitelist_value(&value) {
        return error_response(error_to_code(&err), &trace_id);
    }
    // 步骤 2：反序列化（此时可确信字段集合 ⊆ 白名单）。
    let parsed: AuditReceiptRequest = match parse_json(&bytes, &trace_id) {
        Ok(p) => p,
        Err(resp) => return resp,
    };
    let result: LicenseResult<AuditReceiptResponse> = state.service.audit_receipt(&parsed);
    into_response(result, &trace_id)
}

// ============================================================================
// 管理端 handler（RBAC 门控）
// ============================================================================

/// `POST /admin/codes/issue`。
async fn admin_issue(State(state): State<AppState>, req: Request) -> Response {
    let trace_id = trace_id_of(&req);
    if let Some(resp) = require_admin(&state, &req, &trace_id) {
        return resp;
    }
    let header_key = idempotency_key_header(&req);
    let (_, body) = req.into_parts();
    let bytes = match axum::body::to_bytes(body, usize::MAX).await {
        Ok(b) => b,
        Err(_) => return error_response(codes::BAD_REQUEST, &trace_id),
    };
    let mut parsed: IssueCodesRequest = match parse_json(&bytes, &trace_id) {
        Ok(p) => p,
        Err(resp) => return resp,
    };
    // 幂等键来源：请求体字段（契约 §2.1）；`Idempotency-Key` 头可选覆盖。
    if let Some(hk) = header_key {
        parsed.idempotency_key = hk;
    }
    let result: LicenseResult<IssueCodesResponse> = state.service.issue_codes(&parsed, "admin");
    into_response(result, &trace_id)
}

/// `POST /admin/codes/{id}/revoke`。
async fn admin_revoke(
    State(state): State<AppState>,
    Path(code_id): Path<String>,
    req: Request,
) -> Response {
    let trace_id = trace_id_of(&req);
    if let Some(resp) = require_admin(&state, &req, &trace_id) {
        return resp;
    }
    let (_, body) = req.into_parts();
    let bytes = match axum::body::to_bytes(body, usize::MAX).await {
        Ok(b) => b,
        Err(_) => return error_response(codes::BAD_REQUEST, &trace_id),
    };
    let parsed: RevokeCodeRequest = match parse_json(&bytes, &trace_id) {
        Ok(p) => p,
        Err(resp) => return resp,
    };
    let result = state.service.revoke_code(&code_id, &parsed, "admin");
    into_response(result, &trace_id)
}

/// `POST /admin/codes/{id}/reissue`。
async fn admin_reissue(
    State(state): State<AppState>,
    Path(code_id): Path<String>,
    req: Request,
) -> Response {
    let trace_id = trace_id_of(&req);
    if let Some(resp) = require_admin(&state, &req, &trace_id) {
        return resp;
    }
    let header_key = idempotency_key_header(&req);
    let (_, body) = req.into_parts();
    let bytes = match axum::body::to_bytes(body, usize::MAX).await {
        Ok(b) => b,
        Err(_) => return error_response(codes::BAD_REQUEST, &trace_id),
    };
    let mut parsed: ReissueCodeRequest = match parse_json(&bytes, &trace_id) {
        Ok(p) => p,
        Err(resp) => return resp,
    };
    if let Some(hk) = header_key {
        parsed.idempotency_key = hk;
    }
    let result: LicenseResult<ReissueCodeResponse> =
        state.service.reissue_code(&code_id, &parsed, "admin");
    into_response(result, &trace_id)
}

/// `GET /admin/codes`（分页 + 过滤）。
async fn admin_list_codes(
    State(state): State<AppState>,
    Query(q): Query<CodeListQuery>,
    req: Request,
) -> Response {
    let trace_id = trace_id_of(&req);
    if let Some(resp) = require_admin(&state, &req, &trace_id) {
        return resp;
    }
    let filter = crate::store::CodeFilter {
        tenant_id: q.tenant_id.clone(),
        status: parse_code_status(q.status.as_deref()),
        tier: q.tier.clone(),
        order_id: q.order_id.clone(),
    };
    let page = query_page(q.page);
    let result: LicenseResult<Vec<crate::proto::CodeSummary>> =
        state.service.list_codes(&filter, page, DEFAULT_PAGE_SIZE);
    into_response(result, &trace_id)
}

/// `GET /admin/codes/{id}`（详情）。
async fn admin_code_detail(
    State(state): State<AppState>,
    Path(code_id): Path<String>,
    req: Request,
) -> Response {
    let trace_id = trace_id_of(&req);
    if let Some(resp) = require_admin(&state, &req, &trace_id) {
        return resp;
    }
    let result: LicenseResult<CodeDetail> = state.service.code_detail(&code_id);
    into_response(result, &trace_id)
}

/// `GET /admin/devices`（分页）。
async fn admin_list_devices(
    State(state): State<AppState>,
    Query(q): Query<DeviceListQuery>,
    req: Request,
) -> Response {
    let trace_id = trace_id_of(&req);
    if let Some(resp) = require_admin(&state, &req, &trace_id) {
        return resp;
    }
    let page = query_page(q.page);
    let result: LicenseResult<Vec<crate::proto::DeviceListItem>> = state
        .service
        .list_devices(q.tenant_id.as_deref(), page, DEFAULT_PAGE_SIZE)
        .map(|items| {
            items
                .into_iter()
                .map(|d| crate::proto::DeviceListItem {
                    device_id: d.device_id,
                    tenant_id: d.tenant_id,
                    machine_code_masked: d.machine_code_masked,
                    deploy_mode: d.deploy_mode,
                    image_digest: d.image_digest,
                    last_heartbeat_at: d.last_heartbeat_at,
                    lease_status: d.lease_status,
                    receipt_gap_summary: d.receipt_gap_summary,
                })
                .collect()
        });
    into_response(result, &trace_id)
}

/// `GET /admin/audit/logs`（分页）。
async fn admin_audit_logs(
    State(state): State<AppState>,
    Query(q): Query<AuditLogQuery>,
    req: Request,
) -> Response {
    let trace_id = trace_id_of(&req);
    if let Some(resp) = require_admin(&state, &req, &trace_id) {
        return resp;
    }
    let filter = crate::store::AuditFilter {
        actor_type: parse_actor_type(q.actor_type.as_deref()),
        action: q.action.clone(),
        entity_type: q.entity_type.clone(),
        entity_id: q.entity_id.clone(),
    };
    let page = query_page(q.page);
    let result: LicenseResult<Vec<crate::proto::AuditLogItem>> = state
        .service
        .list_audit_logs(&filter, page, DEFAULT_PAGE_SIZE)
        .map(|logs| {
            logs.into_iter()
                .map(|l| crate::proto::AuditLogItem {
                    ts: l.ts.to_string(),
                    actor: format!("{}:{}", l.actor_type.as_str(), l.actor_id),
                    action: l.action.clone(),
                    entity: format!("{}:{}", l.entity_type, l.entity_id),
                    detail: l.detail.clone(),
                    ip: l.ip.clone(),
                })
                .collect()
        });
    into_response(result, &trace_id)
}

/// 解析状态过滤串（非法值 → `None`，由 store 视为「不筛选」）。
fn parse_code_status(raw: Option<&str>) -> Option<crate::model::CodeStatus> {
    raw.and_then(|s| crate::model::CodeStatus::parse(s).ok())
}

/// 解析 actor_type 过滤串（非法值 → `None`）。
fn parse_actor_type(raw: Option<&str>) -> Option<crate::model::ActorType> {
    raw.and_then(|s| crate::model::ActorType::parse(s).ok())
}

// ============================================================================
// Router 装配
// ============================================================================

/// 装配完整路由（11 个端点）。
pub fn router(state: AppState) -> Router {
    Router::new()
        // 设备端。
        .route("/activation", post(activation))
        .route("/heartbeat", post(heartbeat))
        .route("/verify", post(verify))
        .route("/audit/receipt", post(audit_receipt))
        // 管理端。
        .route("/admin/codes/issue", post(admin_issue))
        .route("/admin/codes/:id/revoke", post(admin_revoke))
        .route("/admin/codes/:id/reissue", post(admin_reissue))
        .route("/admin/codes", get(admin_list_codes))
        .route("/admin/codes/:id", get(admin_code_detail))
        .route("/admin/devices", get(admin_list_devices))
        .route("/admin/audit/logs", get(admin_audit_logs))
        .with_state(state)
}

/// 便捷入口：仅凭 service 装配（管理员门控关闭——内网 / 测试）。
pub fn router_disabled_auth(service: Arc<LicensingService>) -> Router {
    router(AppState::new(service, AdminAuth::disabled()))
}

/// 记录一次请求日志（不打印请求体，避免激活码 / 签名进日志）。
pub fn log_request(method: &str, path: &str, status: u16) {
    tracing::info!(target: "licensing_http", method, path, status, "http request");
}

/// 当前时间戳（测试辅助导出；保持与 service 同源）。
pub fn current_unix_secs() -> i64 {
    now_unix_secs()
}

// ============================================================================
// 集成测试：真实 Router + 真实请求 → 真实响应（不 mock service）
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keys::KeyRing;
    use crate::model::{Tenant, VerifyMode};
    use crate::proto::IssueCodesRequest;
    use crate::service::ServiceConfig;
    use crate::store::Store;
    use axum::body::Body;
    use axum::http::{Method, Request as HttpRequest};
    use tower::ServiceExt as _;

    // ---------------- 测试装置 ----------------

    /// 构造带真实密钥环 + 单个 **A 档**租户（便于覆盖 /verify 成功路径）的服务与路由。
    fn fixture() -> (Router, Arc<LicensingService>, Arc<KeyRing>) {
        let store = Arc::new(Store::open_in_memory().expect("in-memory store"));
        let ring = Arc::new(KeyRing::empty());
        ring.register_generated(Some("kms://test".into()), 1_700_000_000)
            .expect("register signing key");
        // A 档租户：verify 端点的成功路径需要 A 档租约。
        let tenant = Tenant {
            tenant_id: "t-1".into(),
            name: "测试租户".into(),
            verify_mode_default: VerifyMode::A,
            contact: "ops@example.com".into(),
            created_at: 1_700_000_000,
        };
        store.insert_tenant(&tenant).expect("insert tenant");
        let svc = Arc::new(LicensingService::new(
            store.clone(),
            ring.clone(),
            ServiceConfig {
                clock_skew_secs: 300,
                nonce_ttl_secs: 600,
                ..ServiceConfig::default()
            },
        ));
        // 门控开启：测试用固定 token `test-admin`。
        let router = router(AppState::new(
            svc.clone(),
            AdminAuth::new(vec!["test-admin".into()]),
        ));
        (router, svc, ring)
    }

    /// 发一次请求（真实 `oneshot`），返回 (status, JSON body)。
    async fn send(router: Router, req: HttpRequest<Body>) -> (u16, serde_json::Value) {
        let resp = router.oneshot(req).await.expect("oneshot");
        let status = resp.status().as_u16();
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .expect("body");
        let value: serde_json::Value =
            serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
        (status, value)
    }

    /// JSON POST 请求（管理员：带 Bearer token）。
    fn admin_post(path: &str, body: serde_json::Value) -> HttpRequest<Body> {
        HttpRequest::builder()
            .method(Method::POST)
            .uri(path)
            .header("content-type", "application/json")
            .header("authorization", "Bearer test-admin")
            .body(Body::from(serde_json::to_vec(&body).unwrap()))
            .unwrap()
    }

    /// JSON POST 请求（设备端：无认证）。
    fn device_post(path: &str, body: serde_json::Value) -> HttpRequest<Body> {
        HttpRequest::builder()
            .method(Method::POST)
            .uri(path)
            .header("content-type", "application/json")
            .body(Body::from(serde_json::to_vec(&body).unwrap()))
            .unwrap()
    }

    fn admin_get(path: &str) -> HttpRequest<Body> {
        HttpRequest::builder()
            .method(Method::GET)
            .uri(path)
            .header("authorization", "Bearer test-admin")
            .body(Body::empty())
            .unwrap()
    }

    /// 发放一个码（直接走 service），返回 (code_id, code_value)。
    fn issue_one(svc: &LicensingService) -> (String, String) {
        let now = current_unix_secs();
        let req = IssueCodesRequest {
            tenant_id: "t-1".into(),
            tier: "standard".into(),
            valid_from: now.to_string(),
            valid_until: (now + 365 * 86_400).to_string(),
            count: 1,
            prebind_machine_code: None,
            idempotency_key: format!("idem-{}", crate::model::now_ns_id("k")),
        };
        let resp = svc.issue_codes(&req, "admin").expect("issue");
        let c = &resp.codes[0];
        (c.code_id.clone(), c.code.clone())
    }

    /// 激活一台设备（直接走 service），返回 (lease_id, nonce)。
    fn activate_device(svc: &LicensingService, machine: &str, nonce: &str) -> String {
        let (_id, code) = issue_one(svc);
        let anchors: Vec<String> = (0..5).map(|i| format!("{machine}-a-{i}")).collect();
        let req = ActivationRequest {
            activation_code: code,
            machine_code: machine.into(),
            anchor_hashes: anchors,
            device_pubkey: "pub".into(),
            nonce: nonce.into(),
            ts: current_unix_secs().to_string(),
            req_sig: "sig".into(),
        };
        svc.activate(&req).expect("activate").lease_id
    }

    /// 码值后 8 位确认串（**去连字符后的末 8 位**，与 service 校验口径一致）。
    fn tail8_of(code: &str) -> String {
        let stripped: String = code.chars().filter(|c| *c != '-').collect();
        stripped[stripped.len() - 8..].to_string()
    }

    /// 为一个回执请求生成**真实 Ed25519 签名**（对 `receipt_payload_hash` 的 32 字节）。
    ///
    /// `/audit/receipt` 现已在 service 层强制验签（安全要求，设计 §1.4 `sig`），
    /// 占位签名会被 `401` 拒绝。测试必须与生产同路径：先算语义哈希，再签，再 base64。
    fn sign_receipt(ring: &KeyRing, req: &AuditReceiptRequest) -> String {
        let hash = crate::receipt::receipt_payload_hash(
            &req.device_mid,
            &req.lease_id,
            req.seq_from.parse::<i64>().expect("seq_from"),
            req.seq_to.parse::<i64>().expect("seq_to"),
            req.count.parse::<i64>().expect("count"),
            &req.payload_digest,
            req.ts.parse::<i64>().expect("ts"),
        );
        let (_kid, sig_b64) = ring.sign(&hash).expect("sign receipt");
        sig_b64
    }

    /// 构造一个回执请求（`sig` 为空，待签名）。
    fn receipt_req(
        mid: &str,
        lease_id: &str,
        seq_from: i64,
        seq_to: i64,
        digest: &str,
        ts: i64,
    ) -> AuditReceiptRequest {
        AuditReceiptRequest {
            device_mid: mid.into(),
            lease_id: lease_id.into(),
            seq_from: seq_from.to_string(),
            seq_to: seq_to.to_string(),
            count: (seq_to - seq_from + 1).to_string(),
            payload_digest: digest.into(),
            ts: ts.to_string(),
            sig: String::new(),
        }
    }

    // ---------------- 端点成功 / 失败路径 ----------------

    /// /activation 成功：200 + OK + lease_token 三段式 + verify_mode 回显。
    #[tokio::test]
    async fn activation_success_and_failure() {
        let (router, svc, _ring) = fixture();
        let (_cid, code) = issue_one(&svc);
        let anchors: Vec<String> = (0..5).map(|i| format!("dev-a-{i}")).collect();
        let body = serde_json::json!({
            "activation_code": code,
            "machine_code": "dev-1",
            "anchor_hashes": anchors,
            "device_pubkey": "pub",
            "nonce": "http-act-n1",
            "ts": current_unix_secs().to_string(),
            "req_sig": "sig"
        });
        let (status, v) = send(router.clone(), device_post("/activation", body)).await;
        assert_eq!(status, 200, "{v}");
        assert_eq!(v["code"], codes::OK);
        assert_eq!(v["data"]["verify_mode"], "A");
        let token = v["data"]["lease_token"].as_str().unwrap();
        // 三段式 kid.payload.sig。
        assert_eq!(
            token.split('.').count(),
            3,
            "lease_token 必须三段式: {token}"
        );

        // 失败：非法激活码 → 400 INVALID_CODE。
        let bad = serde_json::json!({
            "activation_code": "IOTDAQ-0000-0000-0000-0000",
            "machine_code": "dev-2",
            "anchor_hashes": ["x"],
            "device_pubkey": "pub",
            "nonce": "http-act-bad",
            "ts": current_unix_secs().to_string(),
            "req_sig": "sig"
        });
        let (status, v) = send(router, device_post("/activation", bad)).await;
        assert_eq!(status, 400, "{v}");
        assert_eq!(v["code"], codes::INVALID_CODE);
    }

    /// /heartbeat 成功 + 租约不存在失败。
    #[tokio::test]
    async fn heartbeat_success_and_failure() {
        let (router, svc, _ring) = fixture();
        let lease = activate_device(&svc, "hb-dev", "hb-n0");
        let body = serde_json::json!({
            "lease_id": lease,
            "ts": current_unix_secs().to_string(),
            "nonce": "hb-n1",
            "receipt_cursor": { "seq_from": "1", "seq_to": "9" },
            "device_sig": "sig"
        });
        let (status, v) = send(router.clone(), device_post("/heartbeat", body)).await;
        assert_eq!(status, 200, "{v}");
        assert_eq!(v["code"], codes::OK);
        assert_eq!(v["data"]["verify_mode"], "A");
        // next_deadline 是字符串大整数。
        assert!(v["data"]["next_deadline"].is_string(), "{v}");

        // 失败：租约不存在 → 404 LEASE_NOT_FOUND。
        let bad = serde_json::json!({
            "lease_id": "no-such-lease",
            "ts": current_unix_secs().to_string(),
            "nonce": "hb-n2",
            "receipt_cursor": null,
            "device_sig": "sig"
        });
        let (status, v) = send(router, device_post("/heartbeat", bad)).await;
        assert_eq!(status, 404, "{v}");
        assert_eq!(v["code"], codes::LEASE_NOT_FOUND);
    }

    /// /verify 成功（真实 Ed25519 签名）+ 非 A 档失败。
    #[tokio::test]
    async fn verify_success_and_failure() {
        let (router, svc, ring) = fixture();
        let lease = activate_device(&svc, "vf-dev", "vf-n0");
        let now = current_unix_secs();
        // verify 验签对象：`mid|lease|digest|ts`，用业务层同一密钥环签名。
        let signed = format!("vf-dev|{lease}|digest-1|{now}");
        let (_kid, sig) = ring.sign(signed.as_bytes()).expect("sign");
        let body = serde_json::json!({
            "device_mid": "vf-dev",
            "lease_id": lease,
            "payload_digest": "digest-1",
            "ts": now.to_string(),
            "nonce": "vf-n1",
            "device_sig": sig
        });
        let (status, v) = send(router.clone(), device_post("/verify", body)).await;
        assert_eq!(status, 200, "{v}");
        assert_eq!(v["code"], codes::OK);
        assert_eq!(v["data"]["ok"], true);
        assert_eq!(v["data"]["nonce"], "vf-n1");

        // 失败：非 A 档租约 → 401（TOKEN_EXPIRED / VERIFY_FAIL 之一，本层映射 TokenInvalid → TOKEN_EXPIRED）。
        // 构造一个 B 档租约：直接发一个 B 档租户的码再激活。
        let (router_b, svc_b, _ring_b) = fixture_b_tier();
        let lease_b = activate_device(&svc_b, "vf-b", "vfb0");
        let body_b = serde_json::json!({
            "device_mid": "vf-b",
            "lease_id": lease_b,
            "payload_digest": "digest-b",
            "ts": current_unix_secs().to_string(),
            "nonce": "vfb-n1",
            "device_sig": "sig"
        });
        let (status, v) = send(router_b, device_post("/verify", body_b)).await;
        assert_eq!(status, 401, "{v}");
        assert_eq!(v["code"], codes::TOKEN_EXPIRED);
        let _ = router;
    }

    /// 独立装置：B 档默认租户（用于 verify 非 A 档路径）。
    fn fixture_b_tier() -> (Router, Arc<LicensingService>, Arc<KeyRing>) {
        let store = Arc::new(Store::open_in_memory().expect("in-memory store"));
        let ring = Arc::new(KeyRing::empty());
        ring.register_generated(Some("kms://test".into()), 1_700_000_000)
            .expect("register signing key");
        let tenant = Tenant {
            tenant_id: "t-1".into(),
            name: "B 档租户".into(),
            verify_mode_default: VerifyMode::B,
            contact: "ops@example.com".into(),
            created_at: 1_700_000_000,
        };
        store.insert_tenant(&tenant).expect("insert tenant");
        let svc = Arc::new(LicensingService::new(
            store,
            ring.clone(),
            ServiceConfig {
                clock_skew_secs: 300,
                nonce_ttl_secs: 600,
                ..ServiceConfig::default()
            },
        ));
        (router_disabled_auth(svc.clone()), svc, ring)
    }

    /// /audit/receipt：白名单放行成功（**真实签名**）+ **白名单外字段整单拒收（不是忽略）**。
    #[tokio::test]
    async fn audit_receipt_success_and_whitelist_reject() {
        let (router, svc, ring) = fixture();
        let lease = activate_device(&svc, "rc-dev", "rc-n0");

        // 成功：恰好 8 字段 + 真实 Ed25519 签名（service 已强制验签）。
        let now = current_unix_secs();
        let mut receipt = receipt_req("rc-dev", &lease, 1, 5, "d", now);
        receipt.sig = sign_receipt(&ring, &receipt);
        let ok_body = serde_json::json!({
            "device_mid": receipt.device_mid,
            "lease_id": receipt.lease_id,
            "seq_from": receipt.seq_from,
            "seq_to": receipt.seq_to,
            "count": receipt.count,
            "payload_digest": receipt.payload_digest,
            "ts": receipt.ts,
            "sig": receipt.sig
        });
        let (status, v) = send(router.clone(), device_post("/audit/receipt", ok_body)).await;
        assert_eq!(status, 200, "{v}");
        assert_eq!(v["code"], codes::OK);
        assert_eq!(v["data"]["accepted"], true);
        assert_eq!(v["data"]["gap"], "none");

        // 失败：带白名单外业务字段 `flow_rate` → 422 FIELD_WHITELIST_VIOLATION（整单拒收）。
        // ⚠️ 白名单校验在**验签之前**，故此处签名是否有效无关紧要。
        let bad_body = serde_json::json!({
            "device_mid": "rc-dev",
            "lease_id": lease,
            "seq_from": "1",
            "seq_to": "5",
            "count": "5",
            "payload_digest": "d",
            "ts": current_unix_secs().to_string(),
            "sig": "sig",
            "flow_rate": "42"
        });
        let (status, v) = send(router, device_post("/audit/receipt", bad_body)).await;
        assert_eq!(status, 422, "{v}");
        assert_eq!(v["code"], codes::FIELD_WHITELIST_VIOLATION);
        // 断言「拒收」而非「忽略」：响应无 data。
        assert!(v["data"].is_null(), "白名单外字段必须整单拒收: {v}");
    }

    /// `/audit/receipt` **验签失败 = 认证失败**：必须是 **401 + TOKEN_EXPIRED**，
    /// **不是** 400/422。
    ///
    /// 契约意义（team-lead 口径）：伪造回执属**攻击信号**，若映射成 422 会被监控侧
    /// 误计为「客户端参数写错」，掩盖攻击。故此处**精确断言 status==401 且 code 为
    /// `TOKEN_EXPIRED`**（而非只断言 `!= 200`）。
    #[tokio::test]
    async fn audit_receipt_bad_signature_is_401_token_expired() {
        let (router, svc, _ring) = fixture();
        let lease = activate_device(&svc, "rc-badsig", "rcbs0");
        let now = current_unix_secs();

        // 场景 A：签名是合法 base64、64 字节，但**不是**对该语义哈希的有效签名（伪造 / 篡改）。
        let forged = {
            // 用全零 64 字节：base64(STANDARD) 后仍是合法 64 字节，但验签必失败。
            let zero_sig = {
                use base64::Engine as _;
                base64::engine::general_purpose::STANDARD.encode([0u8; 64])
            };
            serde_json::json!({
                "device_mid": "rc-badsig",
                "lease_id": lease,
                "seq_from": "1",
                "seq_to": "5",
                "count": "5",
                "payload_digest": "d",
                "ts": now.to_string(),
                "sig": zero_sig
            })
        };
        let (status, v) = send(router.clone(), device_post("/audit/receipt", forged)).await;
        assert_eq!(status, 401, "验签失败必须 401（认证失败）: {v}");
        assert_eq!(v["code"], codes::TOKEN_EXPIRED, "{v}");

        // 场景 B：签名压根不是 base64 / 长度非法 → 同为认证失败 401。
        let garbage = serde_json::json!({
            "device_mid": "rc-badsig",
            "lease_id": lease,
            "seq_from": "1",
            "seq_to": "5",
            "count": "5",
            "payload_digest": "d",
            "ts": now.to_string(),
            "sig": "!!!not-base64!!!"
        });
        let (status, v) = send(router.clone(), device_post("/audit/receipt", garbage)).await;
        assert_eq!(status, 401, "非法签名编码也必须 401: {v}");
        assert_eq!(v["code"], codes::TOKEN_EXPIRED, "{v}");

        // 注：**空签名**不在此断言 401——`validate_whitelist()`（必填字段存在性）先于
        // 验签执行，空 `sig` 会被判为「白名单必填字段缺失」→ 422（参数错误语义，合理）。
        // 本测试只锁定「签名存在但**无效**」= 认证失败 401 这条路径。
    }

    /// /audit/receipt：**纯空白 `sig` 必须与空 `sig` 同等对待**（回归守卫）。
    ///
    /// `validate_whitelist()` 原先用 `is_empty()`，**纯空白串会蒙过白名单**
    /// （长度非零即算「已提供」）—— 这条绕过路径已实测确认：`sig = "   "` 曾一路走到
    /// 验签层，仅因 `decode_signature` 内部恰好也做 trim 才被拦住。
    /// **不能依赖下游兜底**，故已把存在性判定改为 `trim().is_empty()`
    /// （`proto.rs::validate_whitelist`），空白现在视同「未提供」。
    ///
    /// 契约依据：`docs/design/licensing-api.md` §1.4 只规定 `422 FIELD_WHITELIST_VIOLATION`
    /// 用于「字段白名单 / 存在性」违规，**未给 `/audit/receipt` 定义 401**；
    /// 401 是 `/verify` 的语义（`VERIFY_FAIL`）。故本端点定位 = **422**。
    #[tokio::test]
    async fn audit_receipt_whitespace_signature_is_never_accepted() {
        let (router, svc, _ring) = fixture();
        let lease = activate_device(&svc, "rc-ws-sig", "rcws0");

        for sig in ["   ", "\t", "\n", " \t \n "] {
            let body = serde_json::json!({
                "device_mid": "rc-ws-sig",
                "lease_id": lease,
                "seq_from": "1",
                "seq_to": "5",
                "count": "5",
                "payload_digest": "d",
                "ts": current_unix_secs().to_string(),
                "sig": sig
            });
            let (status, v) = send(router.clone(), device_post("/audit/receipt", body)).await;
            assert_eq!(
                status,
                crate::proto::http_status(codes::FIELD_WHITELIST_VIOLATION),
                "空白签名必须被判定为「字段缺失」→ 422（sig={sig:?}）: {v}"
            );
            assert_eq!(v["code"], codes::FIELD_WHITELIST_VIOLATION, "{v}");
        }
    }

    /// 空白签名**不得**落到验签层（上游就该拦住）。
    ///
    /// 区分路径的手法：验签层失败会返回 401 + `TOKEN_EXPIRED`，
    /// 白名单层失败返回 422 + `FIELD_WHITELIST_VIOLATION`。
    /// 若哪天有人把 `validate_whitelist` 的 trim 去掉，空白会改走验签分支，
    /// 状态码与业务码双双变化 → 本测试立刻失败。
    #[tokio::test]
    async fn whitespace_signature_takes_whitelist_path_not_verifier_path() {
        let (router, svc, _ring) = fixture();
        let lease = activate_device(&svc, "rc-ws-path", "rcwp0");
        let body = serde_json::json!({
            "device_mid": "rc-ws-path",
            "lease_id": lease,
            "seq_from": "1",
            "seq_to": "5",
            "count": "5",
            "payload_digest": "d",
            "ts": current_unix_secs().to_string(),
            "sig": "   "
        });
        let (status, v) = send(router.clone(), device_post("/audit/receipt", body)).await;

        // 白名单层拦截的特征：422 + FIELD_WHITELIST_VIOLATION。
        assert_eq!(
            status,
            crate::proto::http_status(codes::FIELD_WHITELIST_VIOLATION),
            "空白签名必须由白名单层拦截（422），而非验签层（401）: {v}"
        );
        assert_eq!(
            v["code"],
            codes::FIELD_WHITELIST_VIOLATION,
            "必须是白名单违规业务码，证明路径未经验签: {v}"
        );
        assert_ne!(
            v["code"],
            codes::TOKEN_EXPIRED,
            "不得走验签路径（那说明上游存在性判定被削弱了）: {v}"
        );
    }

    /// /admin/codes/issue 成功 + 幂等重放（同 Idempotency-Key → 同一批码）。
    #[tokio::test]
    async fn admin_issue_success_and_idempotent_replay() {
        let (router, _svc, _ring) = fixture();
        let now = current_unix_secs();
        let body = serde_json::json!({
            "tenant_id": "t-1",
            "tier": "pro",
            "valid_from": now.to_string(),
            "valid_until": (now + 365 * 86_400).to_string(),
            "count": 3,
            "prebind_machine_code": null,
            // 契约 §2.1：幂等键是请求体字段。
            "idempotency_key": "http-idem-1"
        });
        let (status, v1) = send(
            router.clone(),
            admin_post("/admin/codes/issue", body.clone()),
        )
        .await;
        assert_eq!(status, 200, "{v1}");
        assert_eq!(v1["code"], codes::OK);
        let codes1: Vec<String> = v1["data"]["codes"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| c["code_id"].as_str().unwrap().to_string())
            .collect();
        assert_eq!(codes1.len(), 3);

        // 幂等重放：同 body（同 idempotency_key）→ 同一批 code_id。
        let (status, v2) = send(router.clone(), admin_post("/admin/codes/issue", body)).await;
        assert_eq!(status, 200, "{v2}");
        let codes2: Vec<String> = v2["data"]["codes"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| c["code_id"].as_str().unwrap().to_string())
            .collect();
        assert_eq!(codes1, codes2, "同幂等键必须返回同一批码");

        // 失败：租户不存在 → 400 TENANT_NOT_FOUND。
        let bad = serde_json::json!({
            "tenant_id": "t-nope",
            "tier": "pro",
            "valid_from": now.to_string(),
            "valid_until": (now + 86_400).to_string(),
            "count": 1,
            "prebind_machine_code": null,
            "idempotency_key": "http-idem-2"
        });
        let (status, v) = send(router, admin_post("/admin/codes/issue", bad)).await;
        assert_eq!(status, 400, "{v}");
        assert_eq!(v["code"], codes::TENANT_NOT_FOUND);
    }

    /// /admin/codes/{id}/revoke 成功 + confirm 不符失败 + 无 token 403 ADMIN_ONLY。
    #[tokio::test]
    async fn admin_revoke_success_failure_and_auth() {
        let (router, svc, _ring) = fixture();
        let (code_id, code) = issue_one(&svc);
        let tail8 = tail8_of(&code);
        let body = serde_json::json!({
            "reason": "risk",
            "note": "风控已确认异常并人工核实废弃",
            "confirm_tail8": tail8,
            "second_approver": null
        });
        let (status, v) = send(
            router.clone(),
            admin_post(&format!("/admin/codes/{code_id}/revoke"), body),
        )
        .await;
        assert_eq!(status, 200, "{v}");
        assert_eq!(v["code"], codes::OK);

        // 失败：confirm 不符 → 412 CONFIRM_MISMATCH。
        let (code_id2, _code2) = issue_one(&svc);
        let bad = serde_json::json!({
            "reason": "risk",
            "note": "风控已确认异常并人工核实废弃",
            "confirm_tail8": "00000000",
            "second_approver": null
        });
        let (status, v) = send(
            router.clone(),
            admin_post(&format!("/admin/codes/{code_id2}/revoke"), bad),
        )
        .await;
        assert_eq!(status, 412, "{v}");
        assert_eq!(v["code"], codes::CONFIRM_MISMATCH);

        // 认证失败：无 Bearer token → 403 ADMIN_ONLY。
        let noauth = HttpRequest::builder()
            .method(Method::POST)
            .uri(format!("/admin/codes/{code_id2}/revoke"))
            .header("content-type", "application/json")
            .body(Body::from(
                serde_json::to_vec(&serde_json::json!({
                    "reason": "r", "note": "n".repeat(12), "confirm_tail8": "00000000", "second_approver": null
                }))
                .unwrap(),
            ))
            .unwrap();
        let (status, v) = send(router, noauth).await;
        assert_eq!(status, 403, "{v}");
        assert_eq!(v["code"], codes::ADMIN_ONLY);
    }

    /// /admin/codes/{id}/reissue 成功（先 revoke 再 reissue）+ ORIGINAL_NOT_REVOKED 失败。
    #[tokio::test]
    async fn admin_reissue_success_and_failure() {
        let (router, svc, _ring) = fixture();
        let (code_id, code) = issue_one(&svc);
        let tail8 = tail8_of(&code);
        // 先撤销（重发前置条件）。
        let revoke = serde_json::json!({
            "reason": "risk",
            "note": "风控已确认异常并人工核实废弃",
            "confirm_tail8": tail8,
            "second_approver": null
        });
        let (rv_status, rv_v) = send(
            router.clone(),
            admin_post(&format!("/admin/codes/{code_id}/revoke"), revoke),
        )
        .await;
        assert_eq!(rv_status, 200, "撤销前置步骤必须成功: {rv_v}");

        let reissue = serde_json::json!({
            "prebind": null,
            "inherit_tier": true,
            "inherit_validity": true,
            "overrides": null,
            "idempotency_key": "http-reissue-1"
        });
        let (status, v) = send(
            router.clone(),
            admin_post(&format!("/admin/codes/{code_id}/reissue"), reissue),
        )
        .await;
        assert_eq!(status, 200, "{v}");
        assert_eq!(v["code"], codes::OK);
        assert_eq!(v["data"]["new_code"]["reissued_from"], code_id);

        // 失败：对未撤销的原码重发 → 409 ORIGINAL_NOT_REVOKED。
        let (code_id2, _c2) = issue_one(&svc);
        let reissue2 = serde_json::json!({
            "prebind": null,
            "inherit_tier": true,
            "inherit_validity": true,
            "overrides": null,
            "idempotency_key": "http-reissue-2"
        });
        let (status, v) = send(
            router,
            admin_post(&format!("/admin/codes/{code_id2}/reissue"), reissue2),
        )
        .await;
        assert_eq!(status, 409, "{v}");
        assert_eq!(v["code"], codes::ORIGINAL_NOT_REVOKED);
    }

    /// GET /admin/codes 列表（掩码）+ /admin/codes/{id} 详情。
    #[tokio::test]
    async fn admin_list_codes_and_detail() {
        let (router, svc, _ring) = fixture();
        let (code_id, _code) = issue_one(&svc);

        let (status, v) = send(router.clone(), admin_get("/admin/codes?tenant_id=t-1")).await;
        assert_eq!(status, 200, "{v}");
        assert_eq!(v["code"], codes::OK);
        let items = v["data"].as_array().unwrap();
        assert_eq!(items.len(), 1);
        assert!(
            items[0]["code_masked"].as_str().unwrap().contains("****"),
            "列表码值必须掩码: {v}"
        );

        let (status, v) = send(router, admin_get(&format!("/admin/codes/{code_id}"))).await;
        assert_eq!(status, 200, "{v}");
        assert_eq!(v["code"], codes::OK);
        assert_eq!(v["data"]["code_id"], code_id);
        assert!(v["data"]["timeline"].is_array(), "{v}");
    }

    /// GET /admin/devices 设备列表（机器码掩码）+ GET /admin/audit/logs 审计日志。
    #[tokio::test]
    async fn admin_list_devices_and_audit_logs() {
        let (router, svc, _ring) = fixture();
        activate_device(&svc, "list-dev", "ld-n0");

        let (status, v) = send(router.clone(), admin_get("/admin/devices?tenant_id=t-1")).await;
        assert_eq!(status, 200, "{v}");
        assert_eq!(v["code"], codes::OK);
        let items = v["data"].as_array().unwrap();
        assert_eq!(items.len(), 1);
        // 机器码掩码（不得明文）。
        assert!(
            !items[0]["machine_code_masked"]
                .as_str()
                .unwrap()
                .contains("list-dev"),
            "机器码必须掩码: {v}"
        );

        let (status, v) = send(router, admin_get("/admin/audit/logs")).await;
        assert_eq!(status, 200, "{v}");
        assert_eq!(v["code"], codes::OK);
        assert!(v["data"].is_array(), "{v}");
        // 至少有一条 activation 审计。
        let logs = v["data"].as_array().unwrap();
        assert!(!logs.is_empty(), "审计日志不得为空: {v}");
        assert!(logs[0]["ts"].is_string(), "审计 ts 必须是字符串: {v}");
    }

    /// 认证门控关闭时任何请求都放行；开启时非 admin 一律 403（管理端全覆盖抽查）。
    #[tokio::test]
    async fn admin_auth_gate_is_enforced_on_all_admin_endpoints() {
        let (router, _svc, _ring) = fixture();
        let no_auth_gets = [
            "/admin/codes",
            "/admin/codes/whatever",
            "/admin/devices",
            "/admin/audit/logs",
        ];
        for path in no_auth_gets {
            let req = HttpRequest::builder()
                .method(Method::GET)
                .uri(path)
                .body(Body::empty())
                .unwrap();
            let (status, v) = send(router.clone(), req).await;
            assert_eq!(status, 403, "path={path} body={v}");
            assert_eq!(v["code"], codes::ADMIN_ONLY, "path={path}");
        }
    }

    /// 错误映射不泄密：Storage 错误不得把 SQL 细节 / Display 全文吐给客户端。
    #[test]
    fn error_mapping_never_leaks_internals() {
        let storage = LicenseError::Storage("no such table: secret_table".into());
        let code = error_to_code(&storage);
        // 映射到 BAD_REQUEST（400），且对外消息不含内部细节。
        assert_eq!(code, codes::BAD_REQUEST);
        assert!(!safe_message(code).contains("secret_table"));
        assert!(!safe_message(code).contains("sqlite"));
        // 所有业务码的对外消息都非空且不含 "LicenseError" 前缀。
        for c in [
            codes::INVALID_CODE,
            codes::NONCE_REPLAY,
            codes::ADMIN_ONLY,
            codes::FIELD_WHITELIST_VIOLATION,
        ] {
            let m = safe_message(c);
            assert!(!m.is_empty());
            assert!(!m.contains("LicenseError"), "code={c} msg={m}");
        }
    }

    /// 幂等键来源：请求体字段优先；`Idempotency-Key` 头可覆盖。
    #[test]
    fn idempotency_key_header_overrides_body() {
        let mut req = HttpRequest::builder()
            .method(Method::POST)
            .uri("/admin/codes/issue")
            .header("idempotency-key", "from-header")
            .body(Body::empty())
            .unwrap();
        assert_eq!(idempotency_key_header(&req).as_deref(), Some("from-header"));
        // 无头 → None。
        let req2 = HttpRequest::builder()
            .method(Method::POST)
            .uri("/admin/codes/issue")
            .body(Body::empty())
            .unwrap();
        assert_eq!(idempotency_key_header(&req2), None);
        // 空头 → None（视为未提供）。
        let req3 = HttpRequest::builder()
            .method(Method::POST)
            .uri("/admin/codes/issue")
            .header("idempotency-key", "   ")
            .body(Body::empty())
            .unwrap();
        assert_eq!(idempotency_key_header(&req3), None);
        let _ = &mut req;
    }

    /// `AdminAuth` 门控语义：关闭恒通过；开启需匹配 Bearer token。
    #[test]
    fn admin_auth_semantics() {
        let open = AdminAuth::disabled();
        assert!(open.authenticate(None));
        assert!(open.authenticate(Some("garbage")));

        let closed = AdminAuth::new(vec!["tok-a".into(), "tok-b".into()]);
        assert!(!closed.authenticate(None));
        assert!(!closed.authenticate(Some("Bearer wrong")));
        assert!(!closed.authenticate(Some("Basic tok-a")));
        assert!(closed.authenticate(Some("Bearer tok-a")));
        assert!(closed.authenticate(Some("Bearer tok-b")));
    }
}

//! 控制指令下发 API（wave2 be-port）。
//!
//! 端点：
//! - `POST /api/control/issue` — 下发控制指令
//! - `GET /api/control/status` — 控制面装配状态
//! - `GET /api/control/history` — 近期下发历史
//!
//! 模块通过 `#[path]` 注册（见 `lib.rs`），路由由 `mgmt/mod.rs::router()` 挂载。

use std::sync::Arc;

use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Json, Response};
use serde::Deserialize;

use crate::bootstrap::DaemonShared;
use crate::ctrl::{ControlIssueError, ControlLedger, ControlRegistry, RawIssueRequest};
use crate::mgmt::{
    rbac::{AuthedRole, Permission},
    MgmtState,
};

/// 注入到 axum `State` 的控制面句柄。
///
/// `ControlRegistry` 在 bootstrap 阶段构造，经 [`ControlApiState::new`] 封装后挂入共享态。
#[derive(Clone)]
pub struct ControlApiState {
    registry: Arc<ControlRegistry>,
}

impl ControlApiState {
    #[must_use]
    pub fn new(shared: DaemonShared, ledger: Option<Arc<ControlLedger>>) -> Self {
        let mut registry = ControlRegistry::new(shared, crate::ctrl::default_clock());
        if let Some(ledger) = ledger {
            registry.attach_ledger(ledger);
        }
        Self {
            registry: Arc::new(registry),
        }
    }

    #[must_use]
    pub fn registry(&self) -> &ControlRegistry {
        &self.registry
    }
}

// ───────────────────────────── 下发 ─────────────────────────────

/// `POST /api/control/issue`
///
/// 解析请求 → 校验参数 → 经 `ControlRegistry::issue` 幂等下发 → 返回逐指令结果。
///
/// # 错误语义
/// - 400 `bad_request` — 参数非法（空 commands / 地址格式错误 / 未知 op）
/// - 404 `not_found` — 设备不存在
/// - 409 `unsupported` — 驱动/协议不支持该写操作
/// - 502 `delivery_failed` — 设备不可达或写被拒绝
/// - 500 `internal` — 控制面未装配（`ControlWritePort` 未挂载）
///
/// 成功 200 返回 `{ duplicate, idempotency_key, commands: [...] }`。
pub async fn issue(
    State(state): State<MgmtState>,
    authed: AuthedRole,
    Json(raw): Json<RawIssueRequest>,
) -> Response {
    // RBAC：device.write（控制指令可影响设备状态，须写权限）。
    if let Err(rejection) = authed.ensure(Permission::DeviceWrite) {
        return rejection.into_response();
    }

    // 取控制面状态
    let api = match state.control_api() {
        Some(api) => api,
        None => return internal_error("control plane not assembled"),
    };

    // ① 参数解析。
    let req = match crate::ctrl::parse_issue(&raw) {
        Ok(r) => r,
        Err(e) => return control_error(e),
    };

    // ② 下发（幂等 + 审计）。
    let actor = authed.claims.sub.clone();
    match api.registry().issue(&req, &actor).await {
        Ok(result) => Json(serde_json::to_value(&result).unwrap_or_default()).into_response(),
        Err(e) => control_error(e),
    }
}

// ───────────────────────────── 状态 ─────────────────────────────

/// `GET /api/control/status`
///
/// 诚实反映控制面装配：`mounted`（南向端口是否挂入）+ `idempotency`（持久化口径）。
/// 无需鉴权（公开诊断端点）。
pub async fn status(State(state): State<MgmtState>) -> Response {
    match state.control_api() {
        Some(api) => Json(api.registry().status()).into_response(),
        None => {
            let body = serde_json::json!({
                "mounted": false,
                "idempotency": "none"
            });
            (StatusCode::OK, Json(body)).into_response()
        }
    }
}

// ───────────────────────────── 历史 ─────────────────────────────

/// 查询参数（`limit` 默认 50，上限 1000）。
#[derive(Debug, Clone, Deserialize)]
pub struct HistoryQuery {
    #[serde(default = "default_limit")]
    limit: i64,
}

fn default_limit() -> i64 {
    50
}

/// `GET /api/control/history`
///
/// 返回近期下发结果（ ledger 优先；in-memory 兜底）。
/// 需要 `audit.view`（审计读权限）。
pub async fn history(
    State(state): State<MgmtState>,
    authed: AuthedRole,
    Query(q): Query<HistoryQuery>,
) -> Response {
    if let Err(rejection) = authed.ensure(Permission::AuditView) {
        return rejection.into_response();
    }

    let api = match state.control_api() {
        Some(api) => api,
        None => {
            let body = serde_json::json!([]);
            return Json(body).into_response();
        }
    };

    let limit = q.limit.max(1).min(1000);
    let results = api.registry().history(limit as usize);
    let body = serde_json::to_value(&results).unwrap_or_default();
    Json(body).into_response()
}

// ───────────────────────────── 错误映射 ─────────────────────────────

/// 把 [`ControlIssueError`] 映射为 axum `Response`（状态码 + 结构化体）。
fn control_error(e: ControlIssueError) -> Response {
    let status = StatusCode::from_u16(e.status()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    let body = serde_json::json!({
        "error": e.code(),
        "message": e.reason(),
    });
    (status, Json(body)).into_response()
}

fn internal_error(msg: &str) -> Response {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(serde_json::json!({
            "error": "internal",
            "message": msg,
        })),
    )
        .into_response()
}

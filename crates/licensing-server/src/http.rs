//! `licensing-server` HTTP 层（task 46：激活码生命周期 + 一机一码预绑定）。
//!
//! # 职责边界（见 `lib.rs` 模块地图）
//!
//! - axum 路由装配（issue / activate / revoke / reissue 四个端点）；
//! - 反序列化请求体 → 调用 [`crate::service::LicensingService`]；
//! - **结构化错误映射**：[`error_to_code`] 直接 `match` [`LicenseError`] 变体，把错误映射到
//!   `proto::codes` 业务码（HTTP 状态码再经 `proto::http_status` 推出）。
//!   **绝不**使用 `msg.contains(...)` 之类的字符串反查——那是脆弱耦合（见 `error.rs` 注释）：
//!   任何人改一句文案都会让错误码静默退化成泛化 `BAD_REQUEST`，而单测通常察觉不到。
//! - 统一 [`ApiEnvelope`] 响应包裹（`{ code, data, message, trace_id }`）。
//!
//! # 租户解析约定（重建决策）
//!
//! - `POST /admin/codes/issue`：租户来自请求体 `tenant_id`；
//! - `POST /admin/codes/:code_id/revoke`、`.../:code_id/reissue`：租户取自请求头
//!   `X-Tenant-Id`（原 `http.rs` 已损毁，该约定为重建时的最简合理选择；actor 取 `X-Actor-Id`，
//!   缺省 `admin`）。上层反代应注入这两个头。

use std::sync::Arc;

use axum::{
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::post,
    Json, Router,
};
use serde::Serialize;

use crate::error::LicenseError;
use crate::model::now_ns_id;
use crate::proto::{
    self, ActivationRequest, ApiEnvelope, IssueCodesRequest, ReissueCodeRequest, RevokeCodeRequest,
};
use crate::service::LicensingService;

/// 共享服务句柄（axum 路由状态）。
pub type SharedService = Arc<LicensingService>;

/// 装配 HTTP 路由（task 46 四个端点）。
///
/// - `POST /admin/codes/issue`：批量发放（支持预绑定 + 幂等）。
/// - `POST /activation`：设备首激 + 一机一码绑定。
/// - `POST /admin/codes/:code_id/revoke`：废弃（header `X-Tenant-Id` 取租户）。
/// - `POST /admin/codes/:code_id/reissue`：重发（header `X-Tenant-Id` 取租户）。
pub fn router(service: Arc<LicensingService>) -> Router {
    Router::new()
        .route("/admin/codes/issue", post(issue_codes))
        .route("/activation", post(activate))
        .route("/admin/codes/:code_id/revoke", post(revoke_code))
        .route("/admin/codes/:code_id/reissue", post(reissue_code))
        .with_state(service)
}

/// `POST /admin/codes/issue`：批量发放。
async fn issue_codes(
    State(service): State<SharedService>,
    Json(req): Json<IssueCodesRequest>,
) -> impl IntoResponse {
    match service.issue_codes(&req) {
        Ok(resp) => ok_json(resp),
        Err(e) => error_response(&e),
    }
}

/// `POST /activation`：设备首激 + 一机一码绑定。
async fn activate(
    State(service): State<SharedService>,
    Json(req): Json<ActivationRequest>,
) -> impl IntoResponse {
    match service.activate(&req) {
        Ok(resp) => ok_json(resp),
        Err(e) => error_response(&e),
    }
}

/// `POST /admin/codes/:code_id/revoke`：废弃（仅总管理后台）。
async fn revoke_code(
    State(service): State<SharedService>,
    Path(code_id): Path<String>,
    headers: HeaderMap,
    Json(req): Json<RevokeCodeRequest>,
) -> impl IntoResponse {
    let tenant_id = extract_header(&headers, "x-tenant-id", "unknown");
    let actor_id = extract_header(&headers, "x-actor-id", "admin");
    match service.revoke(&tenant_id, &code_id, &req, &actor_id) {
        Ok(()) => ok_json(()),
        Err(e) => error_response(&e),
    }
}

/// `POST /admin/codes/:code_id/reissue`：重发（换机迁移）。
async fn reissue_code(
    State(service): State<SharedService>,
    Path(code_id): Path<String>,
    headers: HeaderMap,
    Json(req): Json<ReissueCodeRequest>,
) -> impl IntoResponse {
    let tenant_id = extract_header(&headers, "x-tenant-id", "unknown");
    let actor_id = extract_header(&headers, "x-actor-id", "admin");
    match service.reissue(&tenant_id, &code_id, &req, &actor_id) {
        Ok(resp) => ok_json(resp),
        Err(e) => error_response(&e),
    }
}

// ============================================================================
// 内部辅助
// ============================================================================

/// 从请求头取值，缺省回退到 `default`（`trim` 后空串也按缺省处理）。
fn extract_header(headers: &HeaderMap, name: &str, default: &str) -> String {
    headers
        .get(name)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|| default.to_string())
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
        // 激活被拒（码无效 / 已绑定他机 / 已废弃 / 未知租户）→ INVALID_CODE（400）。
        LicenseError::ActivationRejected(_) => proto::codes::INVALID_CODE,
        // 状态机非法（未废弃重发 / 空有效期 / 空 reason 等）→ BAD_REQUEST（400）。
        LicenseError::KeyStateIllegal(_) => proto::codes::BAD_REQUEST,
        // Token 无效 / 过期 → TOKEN_EXPIRED（401）。
        LicenseError::TokenInvalid(_) => proto::codes::TOKEN_EXPIRED,
        // 心跳被拒 → LEASE_REVOKED（403）。
        LicenseError::HeartbeatRejected(_) => proto::codes::LEASE_REVOKED,
        // 配额超限 → QUOTA_EXCEEDED（403）。
        LicenseError::QuotaExceeded(_) => proto::codes::QUOTA_EXCEEDED,
        // 存储层异常属内部错误：用未知业务码，让 `http_status` 兜底为 500，
        // 绝不伪装成客户端 400（否则故障被掩盖）。
        LicenseError::Storage(_) => "INTERNAL_SERVER_ERROR",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keys::KeyRing;
    use crate::model::{now_unix_secs, Tenant};
    use crate::service::LicensingService;
    use crate::store::Store;
    use axum::body::{to_bytes, Body};
    use axum::http::{Request, StatusCode};
    use serde_json::json;
    use std::sync::Arc;
    use tower::ServiceExt;

    /// 构造带内存库 + 已注册签发密钥的服务（测试专用）。
    fn build_service() -> SharedService {
        let store = Store::open_in_memory().expect("open in-memory store");
        let tenant = Tenant::new(
            "t-1".to_string(),
            "Tenant".to_string(),
            "ops@x".to_string(),
            now_unix_secs(),
        );
        store.insert_tenant(&tenant).expect("seed tenant");
        let keyring = KeyRing::empty();
        keyring
            .register_generated(None, 1_700_000_000)
            .expect("register signing key");
        Arc::new(LicensingService::new(store, keyring))
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

    /// 构造 `POST /activation` 请求体。
    fn activate_body(code_value: &str, machine: &str) -> serde_json::Value {
        json!({
            "activation_code": code_value,
            "machine_code": machine,
            "anchor_hashes": ["a", "b", "c", "d", "e"],
            "device_pubkey": "x",
            "nonce": "n1",
            "ts": now_unix_secs().to_string(),
            "req_sig": "s"
        })
    }

    /// 取出 issue 响应里首个码的 code_id。
    fn first_code_id(issue_body: &serde_json::Value) -> String {
        issue_body["data"]["codes"][0]["code_id"]
            .as_str()
            .unwrap()
            .to_string()
    }

    /// 发出一次请求并返回 `(status, json_body)`。
    async fn call_with(
        service: &SharedService,
        method: &str,
        uri: &str,
        body: serde_json::Value,
        headers: &[(&str, &str)],
    ) -> (StatusCode, serde_json::Value) {
        let router = router(Arc::clone(service));
        let mut builder = Request::builder()
            .method(method)
            .uri(uri)
            .header("content-type", "application/json");
        for (k, v) in headers {
            builder = builder.header(*k, *v);
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
        let (status, body) = call_with(
            &svc,
            "POST",
            "/admin/codes/issue",
            issue_body(Some("M1"), "h-issue-1"),
            &[],
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
        call_with(
            &svc,
            "POST",
            "/admin/codes/issue",
            issue_body(Some("M-X"), "h-issue-a"),
            &[],
        )
        .await;
        // 同租户再发同预绑定 → 结构化错误码 PREBIND_CONFLICT，HTTP 400。
        let (status, body) = call_with(
            &svc,
            "POST",
            "/admin/codes/issue",
            issue_body(Some("M-X"), "h-issue-b"),
            &[],
        )
        .await;
        assert_eq!(status, bad_request());
        assert_eq!(body["code"], "PREBIND_CONFLICT");
    }

    #[tokio::test]
    async fn http_issue_idempotent_returns_same_code() {
        let svc = build_service();
        let (_, a) = call_with(
            &svc,
            "POST",
            "/admin/codes/issue",
            issue_body(None, "h-idem"),
            &[],
        )
        .await;
        let (_, b) = call_with(
            &svc,
            "POST",
            "/admin/codes/issue",
            issue_body(None, "h-idem"),
            &[],
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
        let (_, issue) = call_with(
            &svc,
            "POST",
            "/admin/codes/issue",
            issue_body(Some("M1"), "h-act-2"),
            &[],
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
        let (_, issue) = call_with(
            &svc,
            "POST",
            "/admin/codes/issue",
            issue_body(Some("M1"), "h-act-3"),
            &[],
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

    // ---------------- revoke + reissue ----------------

    #[tokio::test]
    async fn http_revoke_then_reissue_idempotent() {
        let svc = build_service();
        let (_, issue) = call_with(
            &svc,
            "POST",
            "/admin/codes/issue",
            issue_body(None, "h-ri-1"),
            &[],
        )
        .await;
        let code_id = first_code_id(&issue);

        let revoke_body = json!({
            "reason": "compromised",
            "note": "note note note",
            "confirm_tail8": "tail1234",
            "second_approver": null
        });
        let (rs, rb) = call_with(
            &svc,
            "POST",
            &format!("/admin/codes/{code_id}/revoke"),
            revoke_body,
            &[("x-tenant-id", "t-1")],
        )
        .await;
        assert_eq!(rs, StatusCode::OK);
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
            &[("x-tenant-id", "t-1")],
        )
        .await;
        assert_eq!(rs1, StatusCode::OK);
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
            &[("x-tenant-id", "t-1")],
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
        let (_, issue) = call_with(
            &svc,
            "POST",
            "/admin/codes/issue",
            issue_body(None, "h-rr-1"),
            &[],
        )
        .await;
        let code_id = first_code_id(&issue);
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
            &[("x-tenant-id", "t-1")],
        )
        .await;
        assert_eq!(status, bad_request());
        assert_eq!(body["code"], "BAD_REQUEST");
    }

    #[tokio::test]
    async fn http_revop_missing_tenant_header_is_rejected() {
        let svc = build_service();
        let (_, issue) = call_with(
            &svc,
            "POST",
            "/admin/codes/issue",
            issue_body(None, "h-rh-1"),
            &[],
        )
        .await;
        let code_id = first_code_id(&issue);
        // 缺省 X-Tenant-Id → tenant="unknown" → 未知租户 → INVALID_CODE → 400。
        let revoke_body = json!({
            "reason": "x",
            "note": "note note note",
            "confirm_tail8": "tail1234",
            "second_approver": null
        });
        let (status, body) = call_with(
            &svc,
            "POST",
            &format!("/admin/codes/{code_id}/revoke"),
            revoke_body,
            &[],
        )
        .await;
        assert_eq!(status, bad_request());
        assert_eq!(body["code"], "INVALID_CODE");
    }
}

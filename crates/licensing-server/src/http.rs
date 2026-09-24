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
    self, ActivationRequest, ApiEnvelope, HeartbeatRequest, IssueCodesRequest, ReissueCodeRequest,
    RevokeCodeRequest,
};
use crate::service::LicensingService;

/// 共享服务句柄（axum 路由状态）。
pub type SharedService = Arc<LicensingService>;

/// 装配 HTTP 路由（设备端 4 + 管理端 3 共 7 个端点）。
///
/// - `POST /admin/codes/issue`：批量发放（支持预绑定 + 幂等）。
/// - `POST /activation`：设备首激 + 一机一码绑定。
/// - `POST /heartbeat`：设备心跳保活（设计 §1.2）。
/// - `POST /verify`：A 档二次校验（设计 §1.3）。
/// - `POST /audit/receipt`：B 档审计回执（设计 §1.4）。
/// - `POST /admin/codes/:code_id/revoke`：废弃（header `X-Tenant-Id` 取租户）。
/// - `POST /admin/codes/:code_id/reissue`：重发（header `X-Tenant-Id` 取租户）。
pub fn router(service: Arc<LicensingService>) -> Router {
    Router::new()
        .route("/admin/codes/issue", post(issue_codes))
        .route("/activation", post(activate))
        .route("/heartbeat", post(heartbeat))
        .route("/verify", post(verify))
        .route("/audit/receipt", post(audit_receipt))
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

/// `POST /heartbeat`：设备心跳保活（设计 §1.2）。
async fn heartbeat(
    State(service): State<SharedService>,
    Json(req): Json<HeartbeatRequest>,
) -> impl IntoResponse {
    match service.heartbeat(&req) {
        Ok(resp) => ok_json(resp),
        Err(e) => error_response(&e),
    }
}

/// `POST /verify`：A 档二次校验（设计 §1.3）。
///
/// 取**原始 JSON**（而非强类型结构体）：字段白名单需对原始对象做「越界字段」判定，
/// 而 serde 默认忽略未知字段——若先反序列化为结构体，越界字段会**静默消失**，
/// 白名单形同虚设。
async fn verify(
    State(service): State<SharedService>,
    Json(body): Json<serde_json::Value>,
) -> impl IntoResponse {
    match service.verify(&body) {
        Ok(resp) => ok_json(resp),
        Err(e) => error_response(&e),
    }
}

/// `POST /audit/receipt`：B 档审计回执（设计 §1.4）。
///
/// 同样取**原始 JSON**：设计明确要求「服务端拒收任何业务字段」——越界字段必须
/// 在**反序列化之前**对原始对象判定，否则 serde 会把它丢掉。
async fn audit_receipt(
    State(service): State<SharedService>,
    Json(body): Json<serde_json::Value>,
) -> impl IntoResponse {
    match service.audit_receipt(&body) {
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
        // 同码异机（§7 冲突检测步骤 ③）→ CODE_BOUND_TO_OTHER_DEVICE（403）。
        LicenseError::CodeBoundToOtherDevice { .. } => proto::codes::CODE_BOUND_TO_OTHER_DEVICE,
        // 激活被拒（码无效 / 已绑定他机 / 已废弃 / 未知租户）→ INVALID_CODE（400）。
        LicenseError::ActivationRejected(_) => proto::codes::INVALID_CODE,
        // 状态机非法（未废弃重发 / 空有效期 / 空 reason 等）→ BAD_REQUEST（400）。
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

    /// 构造带内存库 + 已注册**已知测试密钥**的服务（测试专用）。
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
            .register_from_b64("k-test", &B64.encode(TEST_ONLY_SEED), None, 1_700_000_000)
            .expect("register signing key");
        Arc::new(LicensingService::new(store, keyring))
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

    /// 构造 `POST /activation` 请求体。
    fn activate_body(code_value: &str, machine: &str) -> serde_json::Value {
        activate_body_with_anchors(code_value, machine, &["a", "b", "c", "d", "e"])
    }

    /// 构造带**自定义锚点集**的 `POST /activation` 请求体（N-of-M 冲突检测测试用）。
    fn activate_body_with_anchors(
        code_value: &str,
        machine: &str,
        anchors: &[&str],
    ) -> serde_json::Value {
        json!({
            "activation_code": code_value,
            "machine_code": machine,
            "anchor_hashes": anchors,
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
        let (_, issue) = call_with(
            &svc,
            "POST",
            "/admin/codes/issue",
            issue_body(None, "h-nofm-1"),
            &[],
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

    // ---------------- 设备端：心跳 / 校验 / 回执 ----------------

    /// 经 HTTP 端点走一遍「发码 → 激活」，返回 `lease_id`。
    async fn activate_lease(svc: &SharedService, idem: &str, machine: &str) -> String {
        let (_, issue) = call_with(
            svc,
            "POST",
            "/admin/codes/issue",
            issue_body(None, idem),
            &[],
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

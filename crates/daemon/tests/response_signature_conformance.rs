//! **服务端响应签名契约一致性测试**（计划 task 19 尾巴 / task 22 / task 48 关联项）。
//!
//! 把 daemon 侧（`daemon::auth::client` 的响应验签）与 licensing-server 侧
//! （`licensing_server::service` 的响应签名 + `keys::KeyRing`）**同时**引进同一个
//! 测试 crate，锁定三组证据：
//!
//! 1. **域常量 / 域串逐字节相等**：daemon 镜像常量与服务端 `RESPONSE_SIG_DOMAIN_*`
//!    一致；`render_response_signing_message` 双端对同一输入产出完全相同字节；
//! 2. **端到端信任链（真实 `LicensingService`）**：发放 → 激活（响应带
//!    `server_pubkey` + `sig`）→ daemon 用携带公钥验签通过 → **TOFU 钉定** →
//!    心跳响应用钉定公钥验签通过；篡改 / 异钥一律拒绝；
//! 3. **fail-closed 语义**：域串任一字段漂移（lease_id / nonce / server_time / domain）
//!    或公钥被替换 → 验签失败（daemon 侧状态机不推进由 `src/auth/client.rs`
//!    单元测试覆盖，本文件聚焦密码学契约本身）。
//!
//! 签名对象是**业务语义确定性域串** `{domain}|{lease_id}|{nonce}|{server_time}`
//! （非序列化字节，红线合规；`server_time` 为十进制秒字符串——大整数红线）。

use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine as _;
use ed25519_dalek::{Signer, SigningKey};

use daemon::auth::client::{
    render_response_signing_message, verify_server_response_signature,
    RESPONSE_SIG_DOMAIN_ACTIVATION, RESPONSE_SIG_DOMAIN_HEARTBEAT,
};
use licensing_server::audit::ReceiptLedger;
use licensing_server::keys::{KeyRing, KeyStatus};
use licensing_server::model::{now_unix_secs, Tenant};
use licensing_server::proto::{ActivationRequest, HeartbeatRequest, IssueCodesRequest};
use licensing_server::service::{
    render_response_signing_message as server_render_response_signing_message, LicensingService,
    RESPONSE_SIG_DOMAIN_ACTIVATION as SERVER_DOMAIN_ACTIVATION,
    RESPONSE_SIG_DOMAIN_HEARTBEAT as SERVER_DOMAIN_HEARTBEAT,
};
use licensing_server::store::Store;

// ============================================================================
// 测试专用密钥（**仅测试，禁止真实部署**）
// ============================================================================

/// **TEST_ONLY_** 服务端签发私钥种子（响应签名；禁止真实部署）。
const TEST_ONLY_SERVER_SEED: [u8; 32] = [0x41u8; 32];
/// **TEST_ONLY_** 设备私钥种子（激活请求 `req_sig` / 心跳 `device_sig`；禁止真实部署）。
const TEST_ONLY_DEVICE_SEED: [u8; 32] = [0x42u8; 32];
/// **TEST_ONLY_** 异钥种子（伪造负例；禁止真实部署）。
const TEST_ONLY_ROGUE_SEED: [u8; 32] = [0x43u8; 32];

/// 构造真实服务（内存库 + TEST_ONLY 服务端签发密钥 + **钉定设备公钥**）。
///
/// 设备公钥经 `register_public_only` 进密钥环（模拟生产「激活时钉定设备公钥 →
/// 心跳 / verify 验签可用」的装配；服务端 `heartbeat` 经 `verify_any` 查验）。
fn build_service() -> LicensingService {
    let store = Store::open_in_memory().expect("open in-memory store");
    let tenant = Tenant::new(
        "t-cfg-resp".to_string(),
        "Tenant".to_string(),
        "ops@x".to_string(),
        now_unix_secs(),
    );
    store.insert_tenant(&tenant).expect("seed tenant");

    let keyring = KeyRing::empty();
    keyring
        .register_from_b64(
            "k-cfg-resp-server",
            &B64.encode(TEST_ONLY_SERVER_SEED),
            Some("TEST_ONLY_kms".into()),
            1_700_000_000,
        )
        .expect("register TEST_ONLY server signing key");
    keyring
        .register_public_only(
            "k-cfg-resp-device",
            &device_pubkey_b64(TEST_ONLY_DEVICE_SEED),
            KeyStatus::Active,
            Some("TEST_ONLY_device".into()),
            1_700_000_000,
        )
        .expect("register TEST_ONLY device public key");

    LicensingService::with_ledger(store, keyring, ReceiptLedger::open_in_memory())
}

/// TEST_ONLY 设备私钥种子 → 公钥 STANDARD base64。
fn device_pubkey_b64(seed: [u8; 32]) -> String {
    B64.encode(SigningKey::from_bytes(&seed).verifying_key().to_bytes())
}

/// TEST_ONLY 服务端私钥种子 → 公钥 STANDARD base64（激活响应 `server_pubkey` 应等于它）。
fn server_pubkey_b64(seed: [u8; 32]) -> String {
    B64.encode(SigningKey::from_bytes(&seed).verifying_key().to_bytes())
}

/// 发放一张码并完成真实激活，返回激活响应（含 `server_pubkey` + `sig`）。
fn issue_and_activate(svc: &LicensingService) -> licensing_server::proto::ActivationResponse {
    let now = now_unix_secs();
    let issue = IssueCodesRequest {
        tenant_id: "t-cfg-resp".to_string(),
        tier: "standard".to_string(),
        valid_from: (now - 1000).to_string(),
        valid_until: (now + 365 * 86_400).to_string(),
        count: 1,
        prebind_machine_code: None,
        idempotency_key: "cfg-resp-batch-1".to_string(),
    };
    let issued = svc.issue_codes(&issue).expect("issue codes");
    let code_value = issued.codes[0].code.clone();

    let machine = "MID-CFG-RESP-0001";
    let device_key = SigningKey::from_bytes(&TEST_ONLY_DEVICE_SEED);
    let device_pubkey = device_pubkey_b64(TEST_ONLY_DEVICE_SEED);
    let anchors: Vec<String> = vec!["a".to_string(); 5];
    let nonce = "cfgresp-activation-nonce-0001".to_string();

    // daemon 侧语义哈希 + 签名（与服务端 `activation_payload_hash` 逐字节一致，
    // 由 licensing_contract_conformance.rs 锁定）。
    let payload_hash = daemon::auth::client::activation_payload_hash(
        &code_value,
        machine,
        &anchors,
        &device_pubkey,
        &nonce,
        now,
    );
    let req = ActivationRequest {
        activation_code: code_value,
        machine_code: machine.to_string(),
        anchor_hashes: anchors,
        device_pubkey,
        nonce,
        ts: now.to_string(),
        req_sig: B64.encode(device_key.sign(&payload_hash).to_bytes()),
    };
    svc.activate(&req).expect("activation must succeed")
}

// ============================================================================
// 1. 域常量 / 域串逐字节相等（daemon ↔ 服务端）
// ============================================================================

/// 域常量双端逐字节一致（两个端点域）。
#[test]
fn response_sig_domain_constants_are_byte_identical() {
    assert_eq!(
        RESPONSE_SIG_DOMAIN_ACTIVATION.as_bytes(),
        SERVER_DOMAIN_ACTIVATION.as_bytes(),
        "activation response-sig domain drifted"
    );
    assert_eq!(
        RESPONSE_SIG_DOMAIN_HEARTBEAT.as_bytes(),
        SERVER_DOMAIN_HEARTBEAT.as_bytes(),
        "heartbeat response-sig domain drifted"
    );
}

/// 域串渲染：daemon 与服务端对同一输入产生**完全相同**的字节。
#[test]
fn response_signing_message_is_byte_identical_to_server() {
    for domain in [
        RESPONSE_SIG_DOMAIN_ACTIVATION,
        RESPONSE_SIG_DOMAIN_HEARTBEAT,
    ] {
        let d = render_response_signing_message(domain, "lease-0001", "nonce-x", 1_700_000_000);
        let s =
            server_render_response_signing_message(domain, "lease-0001", "nonce-x", 1_700_000_000);
        assert_eq!(
            d.as_bytes(),
            s.as_bytes(),
            "response signing message drifted (domain={domain})"
        );
    }
    // server_time 为大整数也必须是同一十进制字符串渲染（大整数红线）。
    let big = 9_007_199_254_740_991_i64;
    assert_eq!(
        render_response_signing_message("activation", "l", "n", big).as_bytes(),
        server_render_response_signing_message("activation", "l", "n", big).as_bytes(),
    );
}

// ============================================================================
// 2. 端到端信任链（真实 LicensingService → daemon 验签）
// ============================================================================

/// 真实激活响应：`server_pubkey` == 服务端签发公钥，`sig` 用 daemon 验签函数通过。
#[test]
fn real_activation_response_verifies_on_daemon() {
    let svc = build_service();
    let resp = issue_and_activate(&svc);

    assert!(
        !resp.sig.is_empty(),
        "activation response must carry a response-level sig"
    );
    assert_eq!(
        resp.server_pubkey,
        server_pubkey_b64(TEST_ONLY_SERVER_SEED),
        "activation response must carry the server signing public key (TOFU)"
    );
    let server_time = resp.server_time.parse::<i64>().expect("server_time secs");
    verify_server_response_signature(
        &resp.server_pubkey,
        RESPONSE_SIG_DOMAIN_ACTIVATION,
        &resp.lease_id,
        &resp.nonce,
        server_time,
        &resp.sig,
    )
    .expect("real activation response sig must verify on the daemon side");
}

/// 真实心跳响应：用激活响应钉定的公钥（TOFU）验签通过。
#[test]
fn real_heartbeat_response_verifies_with_tofu_pinned_key() {
    let svc = build_service();
    let act = issue_and_activate(&svc);
    // TOFU：钉定激活响应携带的服务端公钥。
    let pinned = act.server_pubkey.clone();

    let now = now_unix_secs();
    let device_key = SigningKey::from_bytes(&TEST_ONLY_DEVICE_SEED);
    let nonce = "cfgresp-heartbeat-nonce-0001".to_string();
    let payload_hash =
        daemon::auth::client::heartbeat_payload_hash(&act.lease_id, now, &nonce, None);
    let hb = HeartbeatRequest {
        lease_id: act.lease_id.clone(),
        ts: now.to_string(),
        nonce: nonce.clone(),
        receipt_cursor: None,
        device_sig: B64.encode(device_key.sign(&payload_hash).to_bytes()),
    };
    let resp = svc.heartbeat(&hb).expect("heartbeat must succeed");

    let server_time = resp.server_time.parse::<i64>().expect("server_time secs");
    verify_server_response_signature(
        &pinned,
        RESPONSE_SIG_DOMAIN_HEARTBEAT,
        &act.lease_id,
        &nonce,
        server_time,
        &resp.sig,
    )
    .expect("real heartbeat response sig must verify with the TOFU-pinned key");
}

// ============================================================================
// 3. fail-closed 负例（篡改 / 异钥 / 域漂移）
// ============================================================================

/// 篡改心跳响应签名（首字节翻转）→ daemon 验签必须失败。
#[test]
fn tampered_heartbeat_response_sig_is_rejected() {
    let svc = build_service();
    let act = issue_and_activate(&svc);
    let pinned = act.server_pubkey.clone();

    let now = now_unix_secs();
    let device_key = SigningKey::from_bytes(&TEST_ONLY_DEVICE_SEED);
    let nonce = "cfgresp-heartbeat-nonce-0002".to_string();
    let payload_hash =
        daemon::auth::client::heartbeat_payload_hash(&act.lease_id, now, &nonce, None);
    let hb = HeartbeatRequest {
        lease_id: act.lease_id.clone(),
        ts: now.to_string(),
        nonce: nonce.clone(),
        receipt_cursor: None,
        device_sig: B64.encode(device_key.sign(&payload_hash).to_bytes()),
    };
    let mut resp = svc.heartbeat(&hb).expect("heartbeat must succeed");
    // 篡改签名首字符（base64 安全）。
    resp.sig
        .replace_range(0..1, if resp.sig.starts_with('A') { "B" } else { "A" });

    let server_time = resp.server_time.parse::<i64>().expect("server_time secs");
    let err = verify_server_response_signature(
        &pinned,
        RESPONSE_SIG_DOMAIN_HEARTBEAT,
        &act.lease_id,
        &nonce,
        server_time,
        &resp.sig,
    )
    .expect_err("tampered response sig must be rejected");
    assert!(err.to_string().contains("server response"), "{err}");
}

/// 异钥负例：用另一把服务端私钥签响应（密钥被替换场景）→ TOFU 钉定公钥验签失败。
#[test]
fn rogue_key_response_sig_is_rejected() {
    let svc = build_service();
    let act = issue_and_activate(&svc);
    let pinned = act.server_pubkey.clone();
    let rogue_pubkey = server_pubkey_b64(TEST_ONLY_ROGUE_SEED);
    assert_ne!(
        pinned, rogue_pubkey,
        "rogue key must differ from pinned key"
    );

    let server_time = act.server_time.parse::<i64>().expect("server_time secs");
    // 用 rogue 私钥对同一域串签名（模拟密钥替换 / 中间人重签）。
    let message = render_response_signing_message(
        RESPONSE_SIG_DOMAIN_ACTIVATION,
        &act.lease_id,
        &act.nonce,
        server_time,
    );
    let rogue_sig = B64.encode(
        SigningKey::from_bytes(&TEST_ONLY_ROGUE_SEED)
            .sign(message.as_bytes())
            .to_bytes(),
    );
    let err = verify_server_response_signature(
        &pinned,
        RESPONSE_SIG_DOMAIN_ACTIVATION,
        &act.lease_id,
        &act.nonce,
        server_time,
        &rogue_sig,
    )
    .expect_err("rogue-key response sig must be rejected");
    assert!(err.to_string().contains("server response"), "{err}");

    // 反向自证：rogue 签名对 rogue 公钥可验，证明失败确因「钉定公钥 ≠ 签名密钥」。
    verify_server_response_signature(
        &rogue_pubkey,
        RESPONSE_SIG_DOMAIN_ACTIVATION,
        &act.lease_id,
        &act.nonce,
        server_time,
        &rogue_sig,
    )
    .expect("rogue sig must verify against the rogue key itself");
}

/// 域串任一字段漂移（lease_id / nonce / server_time / domain）→ 验签失败。
#[test]
fn any_domain_field_drift_is_rejected() {
    let svc = build_service();
    let act = issue_and_activate(&svc);
    let pinned = act.server_pubkey.clone();
    let server_time = act.server_time.parse::<i64>().expect("server_time secs");

    // lease_id 漂移。
    assert!(
        verify_server_response_signature(
            &pinned,
            RESPONSE_SIG_DOMAIN_ACTIVATION,
            "lease-OTHER",
            &act.nonce,
            server_time,
            &act.sig,
        )
        .is_err(),
        "lease_id drift must be rejected"
    );
    // nonce 漂移。
    assert!(
        verify_server_response_signature(
            &pinned,
            RESPONSE_SIG_DOMAIN_ACTIVATION,
            &act.lease_id,
            "nonce-OTHER",
            server_time,
            &act.sig,
        )
        .is_err(),
        "nonce drift must be rejected"
    );
    // server_time 漂移。
    assert!(
        verify_server_response_signature(
            &pinned,
            RESPONSE_SIG_DOMAIN_ACTIVATION,
            &act.lease_id,
            &act.nonce,
            server_time + 1,
            &act.sig,
        )
        .is_err(),
        "server_time drift must be rejected"
    );
    // 跨端点域混用（activation 的 sig 拿去当 heartbeat 的 sig）。
    assert!(
        verify_server_response_signature(
            &pinned,
            RESPONSE_SIG_DOMAIN_HEARTBEAT,
            &act.lease_id,
            &act.nonce,
            server_time,
            &act.sig,
        )
        .is_err(),
        "cross-endpoint domain reuse must be rejected"
    );
}

/// 空公钥 / 空白公钥 → 明确拒绝（fail-closed，绝不静默放行）。
#[test]
fn empty_server_pubkey_is_rejected() {
    let err = verify_server_response_signature(
        "   ",
        RESPONSE_SIG_DOMAIN_ACTIVATION,
        "lease-0001",
        "nonce-x",
        1_700_000_000,
        "c2ln",
    )
    .expect_err("empty server pubkey must be rejected");
    assert!(err.to_string().contains("non-empty"), "{err}");
}

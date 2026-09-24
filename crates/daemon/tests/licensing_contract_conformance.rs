//! **跨 crate 协议契约一致性测试**（daemon 授权客户端 ↔ licensing-server 设备端点）。
//!
//! 本文件是本任务的核心验收证据：它把 daemon 侧（`daemon::auth::client`）与服务端侧
//! （`licensing_server::device_auth` / `proto` / `keys`）**同时**引进同一个测试 crate，
//! 从而把「两个 crate 各自声称形状一致」升级为「同一进程内逐字节比对 + 端到端验签」。
//!
//! 覆盖四组证据：
//! 1. **逐字节相等**：心跳 / 校验 / 激活的签名域串与语义哈希，daemon 与服务端对同一组输入
//!    产生**完全相同**的字节（cursor 分 `Some` / `None` 两种）；
//! 2. **端到端信任链**：固定测试种子构造设备密钥 → daemon 侧算 hash + 签名 → 服务端
//!    `KeyRing` 验签通过；篡改 hash / 异钥伪造必须被拒；
//! 3. **请求体形状**：daemon 构造的请求 JSON 的 key 集合**精确等于**服务端白名单，且
//!    `ts` / `seq_from` / `seq_to` 是 JSON **字符串**（大整数红线），并能反序列化为服务端结构体；
//! 4. **域常量一致**：daemon 的域常量逐字节等于服务端常量（反向守护跨端漂移）。

use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine as _;
use ed25519_dalek::{Signer, SigningKey};

use daemon::auth::client::{
    activation_payload_hash, activation_request_body, heartbeat_payload_hash,
    heartbeat_request_body, render_activation_signing_message, render_heartbeat_signing_message,
    render_verify_signing_message, verify_payload_hash, verify_request_body,
    ACTIVATION_FIELD_WHITELIST, ACTIVATION_SEMANTIC_DOMAIN, ACTIVATION_SIGNING_DOMAIN,
    HEARTBEAT_FIELD_WHITELIST, HEARTBEAT_SEMANTIC_DOMAIN, HEARTBEAT_SIGNING_DOMAIN,
    VERIFY_FIELD_WHITELIST, VERIFY_SEMANTIC_DOMAIN, VERIFY_SIGNING_DOMAIN,
};
use licensing_server::device_auth;
use licensing_server::keys::KeyRing;
use licensing_server::model::Device;
use licensing_server::proto::{
    ActivationRequest, HeartbeatRequest, VerifyRequest, VERIFY_WHITELIST,
};

use daemon::auth::machine_id::{AnchorProvider, FingerprintKey, MachineIdentity, StaticAnchor};

// ============================================================================
// 测试专用密钥（**仅测试，禁止真实部署**）
// ============================================================================

/// **TEST_ONLY_** 设备私钥种子（跨端信任链正向用例；禁止真实部署）。
const TEST_ONLY_DEVICE_SEED: [u8; 32] = *b"iotdaq-contract-conformance-seed";
/// **TEST_ONLY_** 异钥种子（伪造负例；禁止真实部署）。
const TEST_ONLY_ROGUE_SEED: [u8; 32] = *b"iotdaq-contract-rogue-seed-00000";

/// 固定测试时间（UTC 秒），保证签名域输入可复现。
const TS: i64 = 1_700_000_000;

/// 构造仅含测试设备公钥的密钥环（服务端侧验签用）。
fn ring_with_device_key() -> KeyRing {
    let ring = KeyRing::empty();
    ring.register_from_b64(
        "k-device-conformance",
        &B64.encode(TEST_ONLY_DEVICE_SEED),
        Some("TEST_ONLY_device_key".into()),
        TS,
    )
    .expect("register TEST_ONLY device key");
    ring
}

/// 用给定私钥对 32 字节待验数据签名（STANDARD base64）。
fn sign(key: &SigningKey, hash: &[u8; 32]) -> String {
    B64.encode(key.sign(hash).to_bytes())
}

// ============================================================================
// 1. 逐字节相等（daemon ↔ 服务端）
// ============================================================================

/// 心跳签名域串：daemon 与服务端对同一输入产生**完全相同**的字节（cursor 两种情形）。
#[test]
fn heartbeat_signing_message_is_byte_identical_to_server() {
    for cursor in [Some((1i64, 100i64)), None] {
        let d = render_heartbeat_signing_message("lease-0001", TS, "n-1", cursor);
        let s = device_auth::render_heartbeat_signing_message("lease-0001", TS, "n-1", cursor);
        assert_eq!(
            d.as_bytes(),
            s.as_slice(),
            "heartbeat signing message drifted (cursor={cursor:?})"
        );
    }
}

/// 心跳语义哈希：daemon 与服务端逐字节相等（cursor 两种情形）。
#[test]
fn heartbeat_payload_hash_is_byte_identical_to_server() {
    for cursor in [Some((1i64, 100i64)), None] {
        let d = heartbeat_payload_hash("lease-0001", TS, "n-1", cursor);
        let s = device_auth::heartbeat_payload_hash("lease-0001", TS, "n-1", cursor);
        assert_eq!(d, s, "heartbeat payload hash drifted (cursor={cursor:?})");
    }
}

/// 校验签名域串：daemon 与服务端逐字节相等。
#[test]
fn verify_signing_message_is_byte_identical_to_server() {
    let d = render_verify_signing_message("MID-0001", "lease-0001", "sha256:abcdef", TS, "n-1");
    let s = device_auth::render_verify_signing_message(
        "MID-0001",
        "lease-0001",
        "sha256:abcdef",
        TS,
        "n-1",
    );
    assert_eq!(d.as_bytes(), s.as_slice(), "verify signing message drifted");
}

/// 校验语义哈希：daemon 与服务端逐字节相等。
#[test]
fn verify_payload_hash_is_byte_identical_to_server() {
    let d = verify_payload_hash("MID-0001", "lease-0001", "sha256:abcdef", TS, "n-1");
    let s = device_auth::verify_payload_hash("MID-0001", "lease-0001", "sha256:abcdef", TS, "n-1");
    assert_eq!(d, s, "verify payload hash drifted");
}

/// 激活签名域串：daemon 与服务端逐字节相等（含多锚点）。
#[test]
fn activation_signing_message_is_byte_identical_to_server() {
    let anchors = vec!["h0".to_string(), "h1".to_string(), "h2".to_string()];
    let d = render_activation_signing_message("ACT-CODE", "mid-1", &anchors, "PUBKEY", "n-1", TS);
    let s = device_auth::render_activation_signing_message(
        "ACT-CODE", "mid-1", &anchors, "PUBKEY", "n-1", TS,
    );
    assert_eq!(
        d.as_bytes(),
        s.as_slice(),
        "activation signing message drifted"
    );
}

/// 激活语义哈希：daemon 与服务端逐字节相等。
#[test]
fn activation_payload_hash_is_byte_identical_to_server() {
    let anchors = vec!["h0".to_string(), "h1".to_string()];
    let d = activation_payload_hash("ACT-CODE", "mid-1", &anchors, "PUBKEY", "n-1", TS);
    let s =
        device_auth::activation_payload_hash("ACT-CODE", "mid-1", &anchors, "PUBKEY", "n-1", TS);
    assert_eq!(d, s, "activation payload hash drifted");
}

/// 域常量逐字节一致（反向守护：任一端改一字节即红）。
#[test]
fn domain_constants_match_server() {
    assert_eq!(
        HEARTBEAT_SIGNING_DOMAIN,
        device_auth::HEARTBEAT_SIGNING_DOMAIN
    );
    assert_eq!(
        HEARTBEAT_SEMANTIC_DOMAIN,
        device_auth::HEARTBEAT_SEMANTIC_DOMAIN
    );
    assert_eq!(VERIFY_SIGNING_DOMAIN, device_auth::VERIFY_SIGNING_DOMAIN);
    assert_eq!(VERIFY_SEMANTIC_DOMAIN, device_auth::VERIFY_SEMANTIC_DOMAIN);
    assert_eq!(
        ACTIVATION_SIGNING_DOMAIN,
        device_auth::ACTIVATION_SIGNING_DOMAIN
    );
    assert_eq!(
        ACTIVATION_SEMANTIC_DOMAIN,
        device_auth::ACTIVATION_SEMANTIC_DOMAIN
    );
}

// ============================================================================
// 2. 端到端信任链（daemon 签名 → 服务端验签）
// ============================================================================

/// 心跳：daemon 侧算 hash + 签名 → 服务端 `verify_device_signature` 返回 kid。
#[test]
fn heartbeat_signature_is_accepted_by_server_keyring() {
    let ring = ring_with_device_key();
    let key = SigningKey::from_bytes(&TEST_ONLY_DEVICE_SEED);
    let hash = heartbeat_payload_hash("lease-0001", TS, "n-1", Some((1, 100)));
    let sig = sign(&key, &hash);
    let kid = device_auth::verify_device_signature(&ring, &hash, &sig).expect("must verify");
    assert_eq!(kid, "k-device-conformance");
}

/// 校验：daemon 侧算 hash + 签名 → 服务端验签返回 kid。
#[test]
fn verify_signature_is_accepted_by_server_keyring() {
    let ring = ring_with_device_key();
    let key = SigningKey::from_bytes(&TEST_ONLY_DEVICE_SEED);
    let hash = verify_payload_hash("MID-0001", "lease-0001", "sha256:abcdef", TS, "n-1");
    let sig = sign(&key, &hash);
    let kid = device_auth::verify_device_signature(&ring, &hash, &sig).expect("must verify");
    assert_eq!(kid, "k-device-conformance");
}

/// 篡改 hash → 服务端必须拒签。
#[test]
fn tampered_hash_is_rejected_by_server() {
    let ring = ring_with_device_key();
    let key = SigningKey::from_bytes(&TEST_ONLY_DEVICE_SEED);
    let hash = heartbeat_payload_hash("lease-0001", TS, "n-1", None);
    let sig = sign(&key, &hash);

    let mut tampered = hash;
    tampered[0] ^= 0xFF;
    assert!(
        device_auth::verify_device_signature(&ring, &tampered, &sig).is_err(),
        "tampered hash must be rejected"
    );
}

/// 异钥伪造签名 → 服务端必须拒签。
#[test]
fn rogue_key_signature_is_rejected_by_server() {
    let ring = ring_with_device_key();
    let rogue = SigningKey::from_bytes(&TEST_ONLY_ROGUE_SEED);
    let hash = verify_payload_hash("MID-0001", "lease-0001", "sha256:abcdef", TS, "n-1");
    let sig = sign(&rogue, &hash);
    assert!(
        device_auth::verify_device_signature(&ring, &hash, &sig).is_err(),
        "rogue-key signature must be rejected"
    );
}

/// 全链：daemon 构造的**心跳请求体** → 服务端从请求体字段独立重建 hash → 验签通过。
///
/// 这条最接近 `service::heartbeat` 的真实路径（服务端从 `req` 字段重建并验签）。
#[test]
fn daemon_heartbeat_request_body_signature_verifies_on_server() {
    let ring = ring_with_device_key();
    let key = SigningKey::from_bytes(&TEST_ONLY_DEVICE_SEED);
    let cursor = Some((10i64, 20i64));

    let cli_hash = heartbeat_payload_hash("lease-0001", TS, "nonce-abc", cursor);
    let sig = sign(&key, &cli_hash);
    let body = heartbeat_request_body("lease-0001", TS, "nonce-abc", cursor, &sig);

    // 服务端从**请求体字段**重建（与 service::heartbeat 同口径）。
    let server_hash = device_auth::heartbeat_payload_hash(
        body["lease_id"].as_str().expect("lease_id"),
        body["ts"]
            .as_str()
            .expect("ts string")
            .parse::<i64>()
            .expect("ts i64"),
        body["nonce"].as_str().expect("nonce"),
        Some((
            body["receipt_cursor"]["seq_from"]
                .as_str()
                .expect("seq_from")
                .parse::<i64>()
                .expect("seq_from i64"),
            body["receipt_cursor"]["seq_to"]
                .as_str()
                .expect("seq_to")
                .parse::<i64>()
                .expect("seq_to i64"),
        )),
    );
    assert_eq!(
        server_hash, cli_hash,
        "server must reconstruct the same hash"
    );
    let kid = device_auth::verify_device_signature(&ring, &server_hash, &sig).expect("must verify");
    assert_eq!(kid, "k-device-conformance");
}

// ============================================================================
// 3. 请求体形状（key 集合精确 + 字符串大整数）
// ============================================================================

/// 心跳请求体：key 集合精确等于白名单、`ts`/`seq_from`/`seq_to` 是字符串、可反序列化为服务端结构。
#[test]
fn heartbeat_request_body_shape_matches_server() {
    let body = heartbeat_request_body("lease-0001", TS, "n-1", Some((1, 100)), "sig");
    let obj = body.as_object().expect("object");
    let mut keys: Vec<&str> = obj.keys().map(String::as_str).collect();
    keys.sort_unstable();
    let mut expected = HEARTBEAT_FIELD_WHITELIST.to_vec();
    expected.sort_unstable();
    assert_eq!(
        keys, expected,
        "heartbeat body keys must equal the whitelist"
    );
    assert_eq!(obj.len(), 5);

    // 反序列化为服务端 `HeartbeatRequest`（证明形状可被服务端解析）。
    let req: HeartbeatRequest =
        serde_json::from_value(body.clone()).expect("deserialize into HeartbeatRequest");
    assert_eq!(req.lease_id, "lease-0001");
    assert_eq!(req.ts, TS.to_string());

    // 大整数红线：ts / seq_from / seq_to 是 JSON **字符串**。
    assert!(body["ts"].is_string(), "ts must be a JSON string");
    assert!(
        body["receipt_cursor"]["seq_from"].is_string(),
        "seq_from must be a JSON string"
    );
    assert!(
        body["receipt_cursor"]["seq_to"].is_string(),
        "seq_to must be a JSON string"
    );

    // 无游标 → `receipt_cursor` 为 null（仍可反序列化）。
    let none = heartbeat_request_body("l", TS, "n", None, "s");
    assert!(none["receipt_cursor"].is_null());
    let _: HeartbeatRequest =
        serde_json::from_value(none).expect("null cursor body must deserialize");
}

/// 校验请求体：key 集合精确等于**服务端** `VERIFY_WHITELIST`，通过服务端白名单校验。
#[test]
fn verify_request_body_shape_matches_server_whitelist() {
    let body = verify_request_body("MID-0001", "lease-0001", "sha256:abc", TS, "n-1", "sig");
    let obj = body.as_object().expect("object");
    let mut keys: Vec<&str> = obj.keys().map(String::as_str).collect();
    keys.sort_unstable();
    assert_eq!(obj.len(), 6, "verify body must have exactly 6 fields");

    // daemon 白名单 == 服务端白名单（集合相等）。
    let mut server_wl = VERIFY_WHITELIST.to_vec();
    server_wl.sort_unstable();
    assert_eq!(
        keys, server_wl,
        "daemon verify keys must equal VERIFY_WHITELIST"
    );
    let mut daemon_wl = VERIFY_FIELD_WHITELIST.to_vec();
    daemon_wl.sort_unstable();
    assert_eq!(daemon_wl, server_wl);

    // **服务端白名单校验必须直接通过**（出现白名单外字段才 422）。
    VerifyRequest::validate_whitelist_value(&body).expect("must pass server whitelist");

    // 反序列化 + 结构体自检。
    let req: VerifyRequest =
        serde_json::from_value(body.clone()).expect("deserialize into VerifyRequest");
    req.validate_whitelist().expect("struct self-check");

    // 大整数红线。
    assert!(body["ts"].is_string(), "ts must be a JSON string");
}

/// 激活请求体：key 集合精确等于白名单、可反序列化为服务端 `ActivationRequest`。
#[test]
fn activation_request_body_shape_matches_server() {
    let anchors = vec!["h0".to_string(), "h1".to_string()];
    let body =
        activation_request_body("ACT-CODE", "mid-1", &anchors, "pubkey", "n-1", TS, "reqsig");
    let obj = body.as_object().expect("object");
    let mut keys: Vec<&str> = obj.keys().map(String::as_str).collect();
    keys.sort_unstable();
    let mut expected = ACTIVATION_FIELD_WHITELIST.to_vec();
    expected.sort_unstable();
    assert_eq!(
        keys, expected,
        "activation body keys must equal the whitelist"
    );
    assert_eq!(obj.len(), 7);

    let req: ActivationRequest =
        serde_json::from_value(body.clone()).expect("deserialize into ActivationRequest");
    assert_eq!(req.activation_code, "ACT-CODE");
    assert_eq!(req.anchor_hashes, anchors);
    assert_eq!(req.ts, TS.to_string());
    assert!(body["ts"].is_string(), "ts must be a JSON string");
}

// ============================================================================
// 4. N-of-M 端到端：daemon 逐锚点哈希 → 服务端同机判定
// ============================================================================

/// **TEST_ONLY_** 指纹盐（仅测试，禁止真实部署）。
const TEST_ONLY_ANCHOR_KEY: &[u8] = b"iotdaq-contract-anchor-key-00000001";

/// 设计锚点总数 **M**（`docs/design/machine-fingerprint.md` §3：M=5）。
const DESIGNED_ANCHOR_COUNT: usize = 5;

/// **本地可采集 quorum**（N-of-M 里的本地侧门槛）：凑够这么多锚点才能算出指纹 / 哈希集，
/// 不足 → `FingerprintError::QuorumFailed`。
///
/// ⚠️ 这**不是**服务端同机判定阈值——两个概念不同，见 [`SAME_MACHINE_MIN_HITS`]。
const LOCAL_QUORUM_MIN_ANCHORS: usize = 3;

/// **服务端同机判定阈值**：`docs/design/machine-fingerprint.md` §3 —— **N=4, M=5，允许 1 项漂移**。
///
/// 判定规则：命中 **≥4/5 → 同机**；**≤3/5 → 异机**。
/// 理由：单锚点漂移（换网卡 / 加硬盘 / 虚拟化迁移 / OS 重装）均为合法运维，必须容忍；
/// 2 项及以上同时变化在合法运维中罕见，大概率是异机 / 伪装 → 不容忍。
///
/// ⚠️ **切勿**把本常量与 `MachineIdentity::new(providers, min_anchors, key)` 的 `min_anchors`
/// 混为一谈：后者是 [`LOCAL_QUORUM_MIN_ANCHORS`]（本地「能否算出指纹」），本常量是服务端
/// 「是否同机」。二者语义完全独立。
const SAME_MACHINE_MIN_HITS: usize = 4;

/// 构造 5 个测试锚点（**仅测试**；对应 Windows 计划锚点名）。
fn test_anchor_providers() -> Vec<Box<dyn AnchorProvider>> {
    vec![
        Box::new(StaticAnchor::new("machine-guid", Some("guid-AAAA"))),
        Box::new(StaticAnchor::new("csproduct-uuid", Some("uuid-BBBB"))),
        Box::new(StaticAnchor::new("bios-serial", Some("bios-CCCC"))),
        Box::new(StaticAnchor::new("baseboard-serial", Some("board-DDDD"))),
        Box::new(StaticAnchor::new("volume-serial", Some("vol-EEEE"))),
    ]
}

fn test_identity(min_anchors: usize) -> MachineIdentity {
    let key = FingerprintKey::from_bytes(TEST_ONLY_ANCHOR_KEY.to_vec())
        .expect("TEST_ONLY anchor key is non-empty");
    MachineIdentity::new(test_anchor_providers(), min_anchors, key)
}

/// daemon 逐锚点哈希集（真实产出）满足 M=5 契约：5 个互不相同的 32-hex 哈希。
#[test]
fn daemon_anchor_hashes_satisfy_designed_cardinality() {
    let identity = test_identity(LOCAL_QUORUM_MIN_ANCHORS);
    let hashes = identity.get_anchor_hashes().expect("quorum ok (5 >= 3)");
    assert_eq!(
        hashes.len(),
        DESIGNED_ANCHOR_COUNT,
        "M=5 anchors → 5 hashes"
    );
    let unique: std::collections::BTreeSet<&String> = hashes.iter().collect();
    assert_eq!(
        unique.len(),
        DESIGNED_ANCHOR_COUNT,
        "anchor hashes must be distinct"
    );
    for h in &hashes {
        assert_eq!(h.len(), 32, "must be 32 hex chars: {h}");
        assert!(h.bytes().all(|b| b.is_ascii_hexdigit()), "must be hex: {h}");
    }

    // 域分隔：任一锚点哈希 ≠ 机器码；且不含锚点原值。
    let machine_code = identity.get_machine_fingerprint().expect("ok");
    for h in &hashes {
        assert_ne!(
            h, &machine_code,
            "anchor hash must never equal machine_code"
        );
    }
    for raw in [
        "guid-AAAA",
        "uuid-BBBB",
        "bios-CCCC",
        "board-DDDD",
        "vol-EEEE",
    ] {
        assert!(
            !hashes.iter().any(|h| h.contains(raw)),
            "raw anchor value leaked: {raw}"
        );
    }
}

/// 端到端：daemon 真实哈希集 + 服务端真实 `Device::anchor_match_count` → 设计 §3 判定表成立。
///
/// 判定表（`docs/design/machine-fingerprint.md` §3，阈值 N=4/M=5）：
/// - ≥4/5 → 同机；≤3/5 → 异机。
#[test]
fn n_of_m_same_machine_decision_is_end_to_end_satisfiable() {
    // 本地 quorum 只需能算出指纹（与下面的服务端同机阈值无关）。
    let identity = test_identity(LOCAL_QUORUM_MIN_ANCHORS);
    let hashes = identity.get_anchor_hashes().expect("quorum ok");
    assert_eq!(hashes.len(), DESIGNED_ANCHOR_COUNT);
    let machine_code = identity.get_machine_fingerprint().expect("ok");

    // 服务端绑定记录（真实构造器），锚点集即 daemon 产出（M=5）。
    let device = Device::new(
        "dev-1".to_string(),
        "t-1".to_string(),
        machine_code,
        hashes.clone(),
        TS,
    );

    // (1) 命中 5/5 → 判【同机】。
    let hits_5 = device.anchor_match_count(&hashes);
    assert_eq!(hits_5, 5);
    assert!(
        hits_5 >= SAME_MACHINE_MIN_HITS,
        "5/5 must be judged same machine"
    );

    // (2) 命中**恰好 4/5**（1 项漂移，如「换一块网卡 / 加一块硬盘」）→ **仍判【同机】**。
    //     ⚠️ 这是设计 §3 的核心合法运维场景。这里构造**真实漂移集**：4 个原哈希 + 1 个**新**
    //     哈希（模拟「一个锚点变了」）；**不是**就地删一个子集（那只是子集，不构成漂移语义）。
    let mut drifted = hashes[..4].to_vec();
    drifted.push("000000000000000000000000000000ff".to_string()); // 漂移项：新锚点哈希
    let hits_4 = device.anchor_match_count(&drifted);
    assert_eq!(hits_4, 4, "one drifted anchor → exactly 4/5 hits");
    assert!(
        hits_4 >= SAME_MACHINE_MIN_HITS,
        "4/5 (exactly one legal drift) MUST still be judged same machine"
    );

    // (3) 命中**恰好 3/5** → 判【异机】（< 4）。
    let hits_3 = device.anchor_match_count(&hashes[..3]);
    assert_eq!(hits_3, 3);
    assert!(
        hits_3 < SAME_MACHINE_MIN_HITS,
        "3/5 is below the §3 threshold → different machine"
    );

    // (4) 命中 2/5、0/5、空集 → 判【异机】。
    assert!(device.anchor_match_count(&hashes[..2]) < SAME_MACHINE_MIN_HITS);
    let foreign = vec!["deadbeef".to_string(); DESIGNED_ANCHOR_COUNT];
    let hits_0 = device.anchor_match_count(&foreign);
    assert_eq!(hits_0, 0);
    assert!(hits_0 < SAME_MACHINE_MIN_HITS);
    assert!(device.anchor_match_count(&[]) < SAME_MACHINE_MIN_HITS);

    // 大小写变异仍命中（服务端 `normalize` 忽略大小写去重）→ 5/5 同机。
    let upper: Vec<String> = hashes.iter().map(|h| h.to_ascii_uppercase()).collect();
    assert_eq!(device.anchor_match_count(&upper), 5);

    // 空白串不计入命中（服务端语义已固化）：3 个真实 + 2 个空白 → 仍 3（异机）。
    let mut with_blank = vec!["   ".to_string(), "\t".to_string()];
    with_blank.extend(hashes[..3].iter().cloned());
    assert_eq!(
        device.anchor_match_count(&with_blank),
        3,
        "blank entries must not count as matches"
    );
    assert!(device.anchor_match_count(&with_blank) < SAME_MACHINE_MIN_HITS);
}

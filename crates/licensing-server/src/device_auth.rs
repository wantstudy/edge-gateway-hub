//! 设备请求（心跳 / A 档二次校验）的**规范化签名域**与验签入口。
//!
//! # 为什么需要本模块
//!
//! `/audit/receipt` 的签名域已在 [`crate::receipt`] 中与服务端 / 网关逐字节对齐；
//! 但设计 §1.2（`/heartbeat`，`device_sig`）与 §1.3（`/verify`，`device_sig`）
//! 同样要求设备私钥对请求**规范化摘要**签名——此签名域此前**无实现**。
//!
//! 本模块补齐这两个签名域，遵循与 [`crate::receipt`] / [`crate::token`] **完全相同**的
//! 构造纪律（改任一字节即视为跨端漂移）：
//!
//! 1. 域前缀 + **逐字段长度前缀**（`|name=<字节长度>:<value>`），杜绝分隔符歧义；
//! 2. **先哈希、对哈希签**：`SHA-256(二级域前缀 ++ 域串)` 的 32 字节再做 Ed25519；
//! 3. `ts` / 序号一律以**十进制字符串**参与（大整数不走 JSON number）。
//!
//! # 签名域形状
//!
//! `/heartbeat`（`device_sig`，签名域 5 字段，顺序固定）：
//!
//! ```text
//! iotdaq.heartbeat.v1|lease_id=<len>:<id>|ts=<len>:<ts>|nonce=<len>:<n>|cursor_from=<len>:<f>|cursor_to=<len>:<t>
//! ```
//!
//! `receipt_cursor` 可空（A/C 档）：为空时 `cursor_from` / `cursor_to` 均取字面量 `"none"`。
//!
//! `/verify`（`device_sig`，签名域 5 字段，顺序固定）：
//!
//! ```text
//! iotdaq.verify.v1|mid=<len>:<mid>|lease_id=<len>:<id>|payload_digest=<len>:<d>|ts=<len>:<ts>|nonce=<len>:<n>
//! ```
//!
//! # 验签与 kid
//!
//! 心跳 / 校验请求**不携带 kid**（与回执一致），故服务端用 [`KeyRing::verify_any`]
//! 遍历全部可验签 kid——这与 [crate::receipt::verify_receipt_signature] 的取值口径一致。
//!
//! # 设计决策（显式记录）
//!
//! - **签名域字符串由本模块单源定义**：daemon 侧当前尚未实现这两个签名（其心率请求
//!   甚至未携带 `device_sig`），故本模块即为契约基线；一旦客户端对齐，**双方都引用
//!   同一形状**，不得分别"各自发明"。
//! - `cursor` 为空用 `"none"`（而非省略字段）：保持签名字段数量恒定，避免"有无字段"
//!   导致的域串长度差异掩盖空档攻击。

use sha2::{Digest, Sha256};

use crate::error::{LicenseError, LicenseResult};
use crate::keys::KeyRing;

/// `/heartbeat` 签名域前缀（版本化；变更必须升版本号）。
pub const HEARTBEAT_SIGNING_DOMAIN: &str = "iotdaq.heartbeat.v1";

/// `/heartbeat` 语义哈希的二级域前缀（**以 `|` 结尾**，与回执 / Token 同风格）。
pub const HEARTBEAT_SEMANTIC_DOMAIN: &[u8] = b"iotdaq.heartbeat.semantic.v1|";

/// `/verify` 签名域前缀（版本化）。
pub const VERIFY_SIGNING_DOMAIN: &str = "iotdaq.verify.v1";

/// `/verify` 语义哈希的二级域前缀（**以 `|` 结尾**）。
pub const VERIFY_SEMANTIC_DOMAIN: &[u8] = b"iotdaq.verify.semantic.v1|";

/// `/activation` 签名域前缀（版本化）。
///
/// 服务端 `activate` **不验** `req_sig`（见 `service.rs`），但为保持「签名域单源且双端可重建」
/// 的既有纪律，此处与 daemon 侧 `client.rs::ACTIVATION_SIGNING_DOMAIN` **共定义同一形状**，
/// 一旦将来服务端开启激活验签，双端无需再对齐。
pub const ACTIVATION_SIGNING_DOMAIN: &str = "iotdaq.activation.v1";

/// `/activation` 语义哈希的二级域前缀（**以 `|` 结尾**）。
pub const ACTIVATION_SEMANTIC_DOMAIN: &[u8] = b"iotdaq.activation.semantic.v1|";

/// `receipt_cursor` 为空时在签名域中占位的字面量。
pub const CURSOR_NONE: &str = "none";

/// 追加一个长度前缀字段：`|name=<值字节长度>:<value>`（长度取字节数，非字符数）。
fn push_len_field(out: &mut String, name: &str, value: &str) {
    out.push('|');
    out.push_str(name);
    out.push('=');
    out.push_str(&value.len().to_string());
    out.push(':');
    out.push_str(value);
}

/// 渲染 `/heartbeat` 签名域串（**未哈希**）。
///
/// `cursor` 为 `Some((from, to))` 时以十进制落域；`None` 时两端均为 [`CURSOR_NONE`]。
#[must_use]
pub fn render_heartbeat_signing_message(
    lease_id: &str,
    ts: i64,
    nonce: &str,
    cursor: Option<(i64, i64)>,
) -> Vec<u8> {
    let (from, to) = match cursor {
        Some((f, t)) => (f.to_string(), t.to_string()),
        None => (CURSOR_NONE.to_string(), CURSOR_NONE.to_string()),
    };
    let mut out = String::with_capacity(160);
    out.push_str(HEARTBEAT_SIGNING_DOMAIN);
    push_len_field(&mut out, "lease_id", lease_id);
    push_len_field(&mut out, "ts", &ts.to_string());
    push_len_field(&mut out, "nonce", nonce);
    push_len_field(&mut out, "cursor_from", &from);
    push_len_field(&mut out, "cursor_to", &to);
    out.into_bytes()
}

/// `SHA-256(HEARTBEAT_SEMANTIC_DOMAIN ++ 域串)`（32 字节，待验数据）。
#[must_use]
pub fn heartbeat_payload_hash(
    lease_id: &str,
    ts: i64,
    nonce: &str,
    cursor: Option<(i64, i64)>,
) -> [u8; 32] {
    let message = render_heartbeat_signing_message(lease_id, ts, nonce, cursor);
    let mut hasher = Sha256::new();
    hasher.update(HEARTBEAT_SEMANTIC_DOMAIN);
    hasher.update(&message);
    let digest = hasher.finalize();
    let mut out = [0u8; 32];
    out.copy_from_slice(&digest);
    out
}

/// 渲染 `/verify` 签名域串（**未哈希**）。
///
/// `payload_digest` 即业务消息的**规范化摘要**（非序列化原始字节，见设计 §1.3）。
#[must_use]
pub fn render_verify_signing_message(
    device_mid: &str,
    lease_id: &str,
    payload_digest: &str,
    ts: i64,
    nonce: &str,
) -> Vec<u8> {
    let mut out = String::with_capacity(160);
    out.push_str(VERIFY_SIGNING_DOMAIN);
    push_len_field(&mut out, "mid", device_mid);
    push_len_field(&mut out, "lease_id", lease_id);
    push_len_field(&mut out, "payload_digest", payload_digest);
    push_len_field(&mut out, "ts", &ts.to_string());
    push_len_field(&mut out, "nonce", nonce);
    out.into_bytes()
}

/// `SHA-256(VERIFY_SEMANTIC_DOMAIN ++ 域串)`（32 字节，待验数据）。
#[must_use]
pub fn verify_payload_hash(
    device_mid: &str,
    lease_id: &str,
    payload_digest: &str,
    ts: i64,
    nonce: &str,
) -> [u8; 32] {
    let message = render_verify_signing_message(device_mid, lease_id, payload_digest, ts, nonce);
    let mut hasher = Sha256::new();
    hasher.update(VERIFY_SEMANTIC_DOMAIN);
    hasher.update(&message);
    let digest = hasher.finalize();
    let mut out = [0u8; 32];
    out.copy_from_slice(&digest);
    out
}

/// 渲染 `/activation` 签名域串（**未哈希**），与 daemon 侧 `render_activation_signing_message`
/// **逐字节一致**。
///
/// 形状（字段顺序固定；`anchor_hashes` 用 `anchors=<计数>` + 逐个 `anchor_<i>` 定界编码，
/// 空集与含 `|` 的值都不产生歧义）：
///
/// ```text
/// iotdaq.activation.v1|activation_code=<len>:<c>|machine_code=<len>:<m>|anchors=<len>:<n>|
///   anchor_0=<len>:<a0>|...|device_pubkey=<len>:<pk>|nonce=<len>:<n>|ts=<len>:<ts>
/// ```
#[must_use]
pub fn render_activation_signing_message(
    activation_code: &str,
    machine_code: &str,
    anchor_hashes: &[String],
    device_pubkey: &str,
    nonce: &str,
    ts: i64,
) -> Vec<u8> {
    let mut out = String::with_capacity(192);
    out.push_str(ACTIVATION_SIGNING_DOMAIN);
    push_len_field(&mut out, "activation_code", activation_code);
    push_len_field(&mut out, "machine_code", machine_code);
    push_len_field(&mut out, "anchors", &anchor_hashes.len().to_string());
    for (i, anchor) in anchor_hashes.iter().enumerate() {
        push_len_field(&mut out, &format!("anchor_{i}"), anchor);
    }
    push_len_field(&mut out, "device_pubkey", device_pubkey);
    push_len_field(&mut out, "nonce", nonce);
    push_len_field(&mut out, "ts", &ts.to_string());
    out.into_bytes()
}

/// `SHA-256(ACTIVATION_SEMANTIC_DOMAIN ++ 域串)`（32 字节，待验数据）。
///
/// 与服务端 `activate` 当前**不验** `req_sig` 无关：此处单源定义形状，供双端一致性测试比对。
#[must_use]
pub fn activation_payload_hash(
    activation_code: &str,
    machine_code: &str,
    anchor_hashes: &[String],
    device_pubkey: &str,
    nonce: &str,
    ts: i64,
) -> [u8; 32] {
    let message = render_activation_signing_message(
        activation_code,
        machine_code,
        anchor_hashes,
        device_pubkey,
        nonce,
        ts,
    );
    let mut hasher = Sha256::new();
    hasher.update(ACTIVATION_SEMANTIC_DOMAIN);
    hasher.update(&message);
    let digest = hasher.finalize();
    let mut out = [0u8; 32];
    out.copy_from_slice(&digest);
    out
}

/// 用密钥环任一可验签 kid 校验设备对 `payload_hash` 的 Ed25519 签名。
///
/// # Errors
/// 空签名 / 非 STANDARD base64 / 长度非 64 / 环内无 kid 可验通 →
/// [`LicenseError::VerifyFailed`]（**绝不**回显输入，防探测）。
pub fn verify_device_signature(
    ring: &KeyRing,
    payload_hash: &[u8; 32],
    signature_b64: &str,
) -> LicenseResult<String> {
    if signature_b64.trim().is_empty() {
        return Err(LicenseError::verify_failed(
            "device signature is empty".to_string(),
        ));
    }
    ring.verify_any(payload_hash, signature_b64)
        .map_err(|e| LicenseError::verify_failed(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::engine::general_purpose::STANDARD as B64;
    use base64::Engine as _;
    use ed25519_dalek::{Signer, SigningKey};

    /// 测试种子（**仅测试，禁止真实部署**）。
    const TEST_SEED: [u8; 32] = *b"iotdaq-device-auth-test-seed--00";

    fn ring_with_test_key() -> (KeyRing, SigningKey) {
        let key = SigningKey::from_bytes(&TEST_SEED);
        let ring = KeyRing::empty();
        ring.register_from_b64(
            "k-device-test",
            &B64.encode(TEST_SEED),
            Some("TEST_ONLY_device_key".into()),
            1_700_000_000,
        )
        .expect("register device key");
        (ring, key)
    }

    /// 心跳签名域：硬编码期望串（手工推导），守住跨端形状。
    #[test]
    fn heartbeat_domain_layout_is_pinned() {
        let got =
            render_heartbeat_signing_message("lease-0001", 1_700_000_000, "n-1", Some((1, 100)));
        let got = String::from_utf8(got).expect("utf-8");
        let expected = concat!(
            "iotdaq.heartbeat.v1",
            "|lease_id=10:lease-0001",
            "|ts=10:1700000000",
            "|nonce=3:n-1",
            "|cursor_from=1:1",
            "|cursor_to=3:100",
        );
        assert_eq!(got, expected);

        // cursor 为空 → 双端 "none"。
        let none = String::from_utf8(render_heartbeat_signing_message(
            "lease-0001",
            1_700_000_000,
            "n-1",
            None,
        ))
        .unwrap();
        assert!(
            none.contains("|cursor_from=4:none|cursor_to=4:none"),
            "{none}"
        );
    }

    /// `/verify` 签名域：硬编码期望串。
    #[test]
    fn verify_domain_layout_is_pinned() {
        let got = render_verify_signing_message(
            "MID-0001",
            "lease-0001",
            "sha256:abcdef",
            1_700_000_000,
            "n-1",
        );
        let got = String::from_utf8(got).expect("utf-8");
        let expected = concat!(
            "iotdaq.verify.v1",
            "|mid=8:MID-0001",
            "|lease_id=10:lease-0001",
            "|payload_digest=13:sha256:abcdef",
            "|ts=10:1700000000",
            "|nonce=3:n-1",
        );
        assert_eq!(got, expected);
    }

    /// `/activation` 签名域：硬编码期望串（与 daemon 侧 `render_activation_signing_message`
    /// **逐字节一致**）。含 anchor 计数 + 逐个定界，钉死形状防跨端漂移。
    #[test]
    fn activation_domain_layout_is_pinned() {
        let hashes = vec!["a".to_string(), "b".to_string()];
        let got = render_activation_signing_message(
            "ACT-CODE",
            "mid-1",
            &hashes,
            "PUBKEY",
            "n-1",
            1_700_000_000,
        );
        let got = String::from_utf8(got).expect("utf-8");
        let expected = concat!(
            "iotdaq.activation.v1",
            "|activation_code=8:ACT-CODE",
            "|machine_code=5:mid-1",
            "|anchors=1:2",
            "|anchor_0=1:a",
            "|anchor_1=1:b",
            "|device_pubkey=6:PUBKEY",
            "|nonce=3:n-1",
            "|ts=10:1700000000",
        );
        assert_eq!(got, expected);
    }

    /// 长度前缀消除歧义 + 二级域前缀确实参与哈希。
    #[test]
    fn length_prefix_and_second_level_domain_are_effective() {
        let a = render_verify_signing_message("ab", "c", "d", 1, "n");
        let b = render_verify_signing_message("a", "bc", "d", 1, "n");
        assert_ne!(a, b);

        // 裸哈希 ≠ 带二级域前缀哈希。
        let message = render_verify_signing_message("m", "l", "d", 1, "n");
        let prefixed = verify_payload_hash("m", "l", "d", 1, "n");
        let bare: [u8; 32] = Sha256::digest(&message).into();
        assert_ne!(prefixed, bare);

        // 心跳同理。
        let hb_msg = render_heartbeat_signing_message("l", 1, "n", None);
        let hb_prefixed = heartbeat_payload_hash("l", 1, "n", None);
        let hb_bare: [u8; 32] = Sha256::digest(&hb_msg).into();
        assert_ne!(hb_prefixed, hb_bare);
    }

    /// 合法签名验通；篡改哈希 / 异钥 / 空签名 / 畸形 base64 一律拒绝且不回显输入。
    #[test]
    fn verify_device_signature_covers_pass_and_fail_paths() {
        let (ring, key) = ring_with_test_key();
        let hash = verify_payload_hash("MID-0001", "lease-0001", "d", 1_700_000_000, "n-1");
        let sig = B64.encode(key.sign(&hash).to_bytes());
        assert_eq!(
            verify_device_signature(&ring, &hash, &sig).expect("must verify"),
            "k-device-test"
        );

        // 篡改哈希 → 拒绝。
        let mut tampered = hash;
        tampered[0] ^= 0xFF;
        assert!(verify_device_signature(&ring, &tampered, &sig).is_err());

        // 异钥伪造 → 拒绝。
        let rogue = SigningKey::from_bytes(&[0x42u8; 32]);
        let rogue_sig = B64.encode(rogue.sign(&hash).to_bytes());
        assert!(verify_device_signature(&ring, &hash, &rogue_sig).is_err());

        // 空签名 / 畸形 base64 → 拒绝（且不 panic）。
        assert!(verify_device_signature(&ring, &hash, "").is_err());
        assert!(verify_device_signature(&ring, &hash, "   ").is_err());
        let secret_like = "!!!not base64!!!";
        let err = verify_device_signature(&ring, &hash, secret_like).unwrap_err();
        assert!(!err.to_string().contains(secret_like));
    }
}

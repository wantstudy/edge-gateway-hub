//! 审计回执（B 档）的**服务端**验签实现。
//!
//! # 背景
//!
//! B 档二次校验的信任机制是「网关侧签名 → 云端验签 → 序号区间跳空/回退/缺失检测」。
//! 其中**验签必须发生在服务端**：否则任何人都能伪造一条回执，让
//! `accepted = true` 落库，进而污染跳空告警、并让回执失去审计证据效力。
//!
//! 本模块提供与 `crates/daemon/src/auth/client.rs` **逐字节一致**的消息重建，
//! 以及一个可被 [`crate::service::LicensingService::audit_receipt`] 调用的验签入口。
//!
//! # ⚠️ 签名管线：回执 **不是** Lease Token（本项目最易踩的坑）
//!
//! 同一个仓库里并存**两条形状相似但语义不同**的签名管线，**不可互相照抄**：
//!
//! | 对象 | 待签/待验数据 | 参考实现 |
//! |---|---|---|
//! | Lease Token | **域串本身**（无预哈希） | `crate::token::render_signing_message` → `ring.sign(&message)` |
//! | 审计回执   | **`SHA-256("iotdaq.receipt.semantic.v1\|" ++ 域串)`** 的 32 字节 | 本模块 [`receipt_payload_hash`] |
//!
//! 即：回执是**先哈希、对哈希签名**，且该哈希带一层**自己的域分隔前缀**
//! （`iotdaq.receipt.semantic.v1|`，注意以 `|` 结尾）。
//! 若照 `token.rs` 的形状直接对域串验签，**全部回执都会验签失败**。
//! 客户端侧的对应实现见 `client.rs` 的 `sign_receipt` / `semantic_digest`。
//!
//! # 校验顺序（安全语义，顺序不可乱）
//!
//! [`verify_receipt_signature`] 的调用方应保证以下顺序：
//!
//! 1. 字段白名单（`AuditReceiptRequest::validate_whitelist`）
//! 2. 字段格式（`seq_from` / `seq_to` / `count` / `ts` 从**字符串**解析为整数）
//! 3. 时序窗（`|now - ts| <= clock_skew_secs`）—— 廉价且不依赖密钥状态
//! 4. **验签**（本模块）—— 昂贵且依赖密钥环状态
//! 5. 业务语义（序号区间跳空 / 幂等）
//!
//! 理由：把验签放在格式与时间窗之后，可避免攻击者用**畸形输入**消耗验签 CPU；
//! 放在业务语义之前，则是「先确认数据可信、再用数据做判断」的前提。

use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine as _;
use sha2::{Digest, Sha256};

use crate::error::{LicenseError, LicenseResult};
use crate::keys::KeyRing;
use crate::proto::AuditReceiptRequest;

/// 回执签名域（与 daemon 侧 `RECEIPT_SIGNING_DOMAIN` **必须一致**）。
///
/// **不带**尾随 `|`：分隔符由 [`push_len_field`] 前置，与 Lease Token 签名域同风格。
pub const RECEIPT_SIGNING_DOMAIN: &str = "iotdaq.receipt.v1";

/// 回执语义哈希的二级域前缀（与 daemon 侧 `semantic_digest` 内联常量**必须一致**）。
///
/// ⚠️ **以 `|` 结尾**（与域常量不同），这是客户端实现的既有事实，改动即破坏跨端兼容。
pub const RECEIPT_SEMANTIC_DOMAIN: &[u8] = b"iotdaq.receipt.semantic.v1|";

/// 追加一个长度前缀字段：`|name=<value 的字节长度>:<value>`。
///
/// 与 daemon 侧 `push_len_field` 逐字节一致。长度取**字节长度**（`str::len()`），
/// 不是字符数——含非 ASCII 的字段（如 `mid` 里的中文机位名）会因此不同。
fn push_len_field(out: &mut String, name: &str, value: &str) {
    out.push('|');
    out.push_str(name);
    out.push('=');
    out.push_str(&value.len().to_string());
    out.push(':');
    out.push_str(value);
}

/// 渲染回执签名域串（**未哈希**），与 daemon 侧 `receipt_signing_message` 逐字节一致。
///
/// 格式（7 个字段，顺序固定）：
///
/// ```text
/// iotdaq.receipt.v1|mid=<len>:<mid>|lease_id=<len>:<id>|seq_from=<len>:<n>|seq_to=<len>:<n>|count=<len>:<n>|payload_digest=<len>:<s>|ts=<len>:<ts>
/// ```
///
/// `seq_from` / `seq_to` / `count` / `ts` 以**十进制字符串**参与（大整数不走 JSON number 红线）。
#[must_use]
pub fn render_receipt_signing_message(
    mid: &str,
    lease_id: &str,
    seq_from: i64,
    seq_to: i64,
    count: i64,
    payload_digest: &str,
    ts: i64,
) -> Vec<u8> {
    let mut out = String::with_capacity(192);
    out.push_str(RECEIPT_SIGNING_DOMAIN);
    push_len_field(&mut out, "mid", mid);
    push_len_field(&mut out, "lease_id", lease_id);
    push_len_field(&mut out, "seq_from", &seq_from.to_string());
    push_len_field(&mut out, "seq_to", &seq_to.to_string());
    push_len_field(&mut out, "count", &count.to_string());
    push_len_field(&mut out, "payload_digest", payload_digest);
    push_len_field(&mut out, "ts", &ts.to_string());
    out.into_bytes()
}

/// 计算回执**待验数据**：`SHA-256(RECEIPT_SEMANTIC_DOMAIN ++ 域串)`，输出 32 字节。
///
/// 与 daemon 侧 `semantic_digest(receipt_signing_message(..))` 等价。
///
/// **显式暴露此步**（而不是藏在 [`verify_receipt_signature`] 内）是为了让跨端一致性
/// 测试能**逐层比对**：先比域串，再比哈希，便于定位是"字段顺序错了"还是"域前缀错了"。
#[must_use]
pub fn receipt_payload_hash(
    mid: &str,
    lease_id: &str,
    seq_from: i64,
    seq_to: i64,
    count: i64,
    payload_digest: &str,
    ts: i64,
) -> [u8; 32] {
    let message =
        render_receipt_signing_message(mid, lease_id, seq_from, seq_to, count, payload_digest, ts);
    let mut hasher = Sha256::new();
    hasher.update(RECEIPT_SEMANTIC_DOMAIN);
    hasher.update(&message);
    let digest = hasher.finalize();
    let mut out = [0u8; 32];
    out.copy_from_slice(&digest);
    out
}

/// 验签通过后的回执事实（仅供调用方记录 / 审计，不含原始 `sig`）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedReceipt {
    /// 实际验签通过的 `kid`（用于追溯是哪把密钥签的；回执本身不携带 kid）。
    pub kid: String,
    /// 参与签名的设备机位码。
    pub device_mid: String,
    /// 租约 ID。
    pub lease_id: String,
    /// 序号区间起点（含）。
    pub seq_from: i64,
    /// 序号区间终点（含）。
    pub seq_to: i64,
    /// 区间内消息条数。
    pub count: i64,
    /// 业务负载摘要（**不透明字符串**，服务端不解其内容）。
    pub payload_digest: String,
    /// 客户端声明的签名时间（unix 秒）。
    pub ts: i64,
}

/// 从请求体解析出各数值字段的十进制整数形式（**必须是字符串**）。
///
/// 该请求结构体的时序 / 序号 / 计数均为 `String`，对应项目红线
/// 「JSON number 是 IEEE754 double，安全整数上限 2^53−1」——
/// 纳秒时间戳与 `uint64` 计数器**必须编码为字符串**，否则静默丢精度。
fn parse_i64(value: &str, field: &str) -> LicenseResult<i64> {
    value.trim().parse::<i64>().map_err(|_| {
        // 不回显原值：畸形输入可能是探测载荷，且原值可能很长。
        LicenseError::ActivationRejected(format!("receipt field {field} is not a valid integer"))
    })
}

/// 解析回执的整数字段（供 service 层与测试复用）。
///
/// 返回 `(seq_from, seq_to, count, ts)`。
///
/// # Errors
/// 任一字段不是合法 `i64` 十进制字符串 → [`LicenseError::ActivationRejected`]。
pub fn parse_receipt_ints(
    seq_from: &str,
    seq_to: &str,
    count: &str,
    ts: &str,
) -> LicenseResult<(i64, i64, i64, i64)> {
    Ok((
        parse_i64(seq_from, "seq_from")?,
        parse_i64(seq_to, "seq_to")?,
        parse_i64(count, "count")?,
        parse_i64(ts, "ts")?,
    ))
}

/// 校验回执签名。**通过返回 [`VerifiedReceipt`]，失败返回错误**。
///
/// # 参数
/// - `ring`：服务端密钥环。用 [`KeyRing::verify_any`] 遍历所有可验签的 kid——
///   回执签名**不携带 kid**（与 Lease Token 的三段式不同），所以不能用 `verify(kid, ..)`。
/// - `req`：已通过白名单与格式校验的回执请求。
/// - `now`：服务端当前 unix 秒，用于时序窗判定（由调用方注入，便于测试）。
/// - `clock_skew_secs`：允许的时钟偏移窗口（`ServiceConfig::clock_skew_secs`）。
///
/// # 校验顺序
/// 1. 数值字段解析（字符串 → `i64`）
/// 2. `seq_from <= seq_to`（区间方向）
/// 3. 时序窗 `|now - ts| <= clock_skew_secs`
/// 4. Ed25519 验签（对 [`receipt_payload_hash`] 的 32 字节）
///
/// ⚠️ **时序窗先于验签**：时间检查廉价且不依赖密钥环，先做可避免畸形 / 过期输入
/// 消耗验签 CPU。**验签先于任何业务语义**：不可信的数据不得参与业务判定。
///
/// # Errors
/// - 字段解析失败 / 区间方向错误 → [`LicenseError::ActivationRejected`]
/// - 超出时序窗 → [`LicenseError::ActivationRejected`]
/// - 验签失败（含未知 kid、非 STANDARD base64、非 64 字节）→ [`LicenseError::TokenInvalid`]
pub fn verify_receipt_signature(
    ring: &KeyRing,
    req: &AuditReceiptRequest,
    now: i64,
    clock_skew_secs: i64,
) -> LicenseResult<VerifiedReceipt> {
    // ---- 1. 数值字段解析（廉价，无密钥依赖）----
    let (seq_from, seq_to, count, ts) =
        parse_receipt_ints(&req.seq_from, &req.seq_to, &req.count, &req.ts)?;

    // ---- 2. 区间方向 ----
    if seq_from > seq_to {
        return Err(LicenseError::ActivationRejected(format!(
            "receipt seq_from {seq_from} is greater than seq_to {seq_to}"
        )));
    }

    // ---- 3. 时序窗（同样廉价；先于验签以省 CPU）----
    if (now - ts).abs() > clock_skew_secs {
        return Err(LicenseError::ActivationRejected(format!(
            "receipt timestamp skew: ts={ts} server_now={now} window=±{clock_skew_secs}s"
        )));
    }

    // ---- 4. 空签名快速拒绝（避免把空串送进 base64 解码）----
    if req.sig.trim().is_empty() {
        return Err(LicenseError::TokenInvalid(
            "receipt signature is empty".into(),
        ));
    }

    // ---- 5. Ed25519 验签：对「域串的语义哈希」验签，**不是**对域串本身 ----
    let payload_hash = receipt_payload_hash(
        &req.device_mid,
        &req.lease_id,
        seq_from,
        seq_to,
        count,
        &req.payload_digest,
        ts,
    );
    let kid = ring.verify_any(&payload_hash, &req.sig)?;

    Ok(VerifiedReceipt {
        kid,
        device_mid: req.device_mid.clone(),
        lease_id: req.lease_id.clone(),
        seq_from,
        seq_to,
        count,
        payload_digest: req.payload_digest.clone(),
        ts,
    })
}

/// 解码 STANDARD base64 签名（供测试断言"只接受 STANDARD 变体"）。
///
/// URL-safe（`-` / `_`）与 no-pad 变体在此**解析失败**——这是有意的：
/// 客户端用 `BASE64_STANDARD` 编码，服务端只接受同一变体，避免"两种变体都能过"
/// 的宽松接受面。
///
/// # Errors
/// 非 STANDARD base64 / 解码后不是 64 字节 → [`LicenseError::TokenInvalid`]。
pub fn decode_signature(sig_b64: &str) -> LicenseResult<[u8; 64]> {
    let raw = B64
        .decode(sig_b64.trim())
        .map_err(|e| LicenseError::TokenInvalid(format!("signature is not valid base64: {e}")))?;
    raw.as_slice().try_into().map_err(|_| {
        LicenseError::TokenInvalid(format!("signature must be 64 bytes, got {}", raw.len()))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 构造一个最小可用的回执请求（字段值固定，便于跨端比对）。
    fn req() -> AuditReceiptRequest {
        AuditReceiptRequest {
            device_mid: "MID-0001".into(),
            lease_id: "lease-0001".into(),
            seq_from: "1".into(),
            seq_to: "100".into(),
            count: "100".into(),
            payload_digest: "sha256:abcdef".into(),
            ts: "1700000000".into(),
            sig: String::new(),
        }
    }

    fn now() -> i64 {
        1_700_000_000
    }

    // ------------------------------------------------------------------
    // 跨端一致性：域串逐字节比对（**硬编码期望串，独立于实现**）
    // ------------------------------------------------------------------

    /// 域串必须与 daemon 侧 `receipt_signing_message` **逐字节一致**。
    ///
    /// 期望串**手工推导**（照 daemon 实现：域 + 7 个 `|name=len:value`），
    /// 不是从本实现的输出复制——否则测试会与实现一起漂移，失去意义。
    #[test]
    fn signing_message_layout_is_byte_identical_to_client() {
        let got = render_receipt_signing_message(
            "MID-0001",
            "lease-0001",
            1,
            100,
            100,
            "sha256:abcdef",
            1_700_000_000,
        );
        let got = String::from_utf8(got).expect("utf-8");

        let expected = concat!(
            "iotdaq.receipt.v1",
            "|mid=8:MID-0001",
            "|lease_id=10:lease-0001",
            "|seq_from=1:1",
            "|seq_to=3:100",
            "|count=3:100",
            // `sha256:abcdef` 共 13 字节（8 + 1 + 6）——长度前缀是**字节长度**。
            "|payload_digest=13:sha256:abcdef",
            "|ts=10:1700000000",
        );

        assert_eq!(got, expected, "回执签名域与服务端期望串不一致（跨端漂移）");
    }

    /// `|` 出现在字段值里**不产生歧义**：长度前缀保证边界唯一。
    #[test]
    fn length_prefix_makes_delimiter_in_value_unambiguous() {
        // 相同字段名序列、不同切分方式 → 域串必须不同。
        let a = render_receipt_signing_message("ab", "c", 1, 2, 3, "d", 4);
        let b = render_receipt_signing_message("a", "bc", 1, 2, 3, "d", 4);
        assert_ne!(a, b, "长度前缀必须让不同的字段切分产生不同域串");

        // 值内含 `|mid=` 这类"看起来像分隔符"的内容，也不会歧义。
        let c = render_receipt_signing_message("x|mid=1:y", "c", 1, 2, 3, "d", 4);
        let d = render_receipt_signing_message("x", "c", 1, 2, 3, "d", 4);
        assert_ne!(c, d);
    }

    /// 域常量与二级域前缀必须与客户端约定一致（**改这个会破坏跨端兼容**）。
    #[test]
    fn domain_constants_match_client_contract() {
        assert_eq!(RECEIPT_SIGNING_DOMAIN, "iotdaq.receipt.v1");
        // 二级域**以 `|` 结尾**，且与域常量不同 —— 这是客户端既有事实。
        assert_eq!(RECEIPT_SEMANTIC_DOMAIN, b"iotdaq.receipt.semantic.v1|");
        assert_ne!(RECEIPT_SEMANTIC_DOMAIN.last(), Some(&b'.'));
    }

    /// 预哈希步骤必须真的存在：`receipt_payload_hash` ≠ `SHA256(域串)`。
    ///
    /// 这条专门守住「回执是哈希后签、不是直签」——若有人把二级域前缀删掉，
    /// 本测试会失败（因为那样它就退化成对域串的裸哈希）。
    #[test]
    fn payload_hash_includes_second_level_domain_prefix() {
        let message = render_receipt_signing_message("m", "l", 1, 2, 3, "d", 4);
        let with_prefix = receipt_payload_hash("m", "l", 1, 2, 3, "d", 4);

        // 裸哈希（无二级域前缀）——必须**不相等**。
        let mut bare = Sha256::new();
        bare.update(&message);
        let bare: [u8; 32] = bare.finalize().into();

        assert_ne!(
            with_prefix, bare,
            "回执哈希必须带 iotdaq.receipt.semantic.v1| 二级域前缀"
        );
    }

    // ------------------------------------------------------------------
    // 验签：通过 / 失败 / 字段参与
    // ------------------------------------------------------------------

    /// 测试用固定种子（可复现；**生产密钥绝不这样生成**）。
    const TEST_SEED: [u8; 32] = *b"iotdaq-receipt-test-seed-0000000";

    /// 用**客户端同款管线**（先哈希、再对哈希签）为 `r` 填充 `sig`。
    ///
    /// 刻意不依赖从 `KeyRing` 取私钥（`KeyEntry.signing` 是私有实现细节）：
    /// 测试自持 `SigningKey`，并用 `register_from_b64` 把同一把注册进环，
    /// 从而完整地走「签名 → 进环公钥验证」的真实路径。
    fn sign_with(key: &ed25519_dalek::SigningKey, r: &mut AuditReceiptRequest) {
        use ed25519_dalek::Signer;

        let (seq_from, seq_to, count, ts) =
            parse_receipt_ints(&r.seq_from, &r.seq_to, &r.count, &r.ts).expect("parse ints");

        // 注意：必须走「先哈希、对哈希签」——与客户端 sign_receipt 一致。
        let payload_hash = receipt_payload_hash(
            &r.device_mid,
            &r.lease_id,
            seq_from,
            seq_to,
            count,
            &r.payload_digest,
            ts,
        );
        r.sig = B64.encode(key.sign(&payload_hash).to_bytes());
    }

    /// 生成「密钥环 + 对应签名私钥 + 已签名的回执」。
    fn ring_and_signed_req(
        mutate: impl FnOnce(&mut AuditReceiptRequest),
    ) -> (KeyRing, ed25519_dalek::SigningKey, AuditReceiptRequest) {
        use ed25519_dalek::SigningKey;

        let key = SigningKey::from_bytes(&TEST_SEED);
        let ring = KeyRing::empty();
        ring.register_from_b64(
            "kid-receipt-test",
            &B64.encode(TEST_SEED),
            Some("kms://test".into()),
            now(),
        )
        .expect("register key from seed");

        let mut r = req();
        mutate(&mut r);
        sign_with(&key, &mut r);

        (ring, key, r)
    }

    /// 合法回执 → 验签通过，且 `kid` 被正确回报。
    #[test]
    fn valid_receipt_verifies_and_reports_kid() {
        let (ring, _key, r) = ring_and_signed_req(|_| {});
        let v = verify_receipt_signature(&ring, &r, now(), 300).expect("must verify");

        assert!(!v.kid.is_empty(), "必须回报验签通过的 kid");
        assert_eq!(v.device_mid, "MID-0001");
        assert_eq!(v.lease_id, "lease-0001");
        assert_eq!(v.seq_from, 1);
        assert_eq!(v.seq_to, 100);
        assert_eq!(v.count, 100);
        assert_eq!(v.ts, 1_700_000_000);
    }

    /// **每个字段都必须真的参与签名**：逐个变异 → 验签全部失败。
    ///
    /// 这能挡住「某字段漏进签名域」（漏掉的字段可被中间人篡改而不破坏签名）。
    #[test]
    fn every_whitelisted_field_participates_in_the_signature() {
        let (ring, _key, base) = ring_and_signed_req(|_| {});

        // 先确认基线可验签（否则下面的失败可能"本来就是坏的"）。
        verify_receipt_signature(&ring, &base, now(), 300).expect("baseline must verify");

        // 变异算子类型别名（避免 clippy::type_complexity）。
        type Mutation = (&'static str, Box<dyn Fn(&mut AuditReceiptRequest)>);
        let mutations: Vec<Mutation> = vec![
            (
                "device_mid",
                Box::new(|r: &mut AuditReceiptRequest| r.device_mid = "MID-0002".into()),
            ),
            (
                "lease_id",
                Box::new(|r: &mut AuditReceiptRequest| r.lease_id = "lease-0002".into()),
            ),
            (
                "seq_from",
                Box::new(|r: &mut AuditReceiptRequest| r.seq_from = "2".into()),
            ),
            (
                "seq_to",
                Box::new(|r: &mut AuditReceiptRequest| r.seq_to = "101".into()),
            ),
            (
                "count",
                Box::new(|r: &mut AuditReceiptRequest| r.count = "101".into()),
            ),
            (
                "payload_digest",
                Box::new(|r: &mut AuditReceiptRequest| r.payload_digest = "sha256:ffff".into()),
            ),
            (
                "ts",
                Box::new(|r: &mut AuditReceiptRequest| r.ts = "1700000001".into()),
            ),
        ];

        for (field, mutate) in mutations {
            let mut tampered = base.clone();
            mutate(&mut tampered);
            let result = verify_receipt_signature(&ring, &tampered, now(), 300);
            assert!(
                result.is_err(),
                "篡改字段 `{field}` 后仍验签通过 —— 该字段未参与签名域"
            );
        }
    }

    /// 用另一把密钥签名 → 验签失败（证明不是"任何签名都收"）。
    #[test]
    fn signature_from_another_key_is_rejected() {
        use ed25519_dalek::{Signer, SigningKey};

        let ring = KeyRing::empty();
        ring.register_generated(Some("kms://test".into()), now())
            .expect("register");

        // 环外的独立密钥签名。
        let rogue = SigningKey::from_bytes(&[7u8; 32]);
        let mut r = req();
        let h = receipt_payload_hash(
            &r.device_mid,
            &r.lease_id,
            1,
            100,
            100,
            &r.payload_digest,
            1_700_000_000,
        );
        r.sig = B64.encode(rogue.sign(&h).to_bytes());

        assert!(
            verify_receipt_signature(&ring, &r, now(), 300).is_err(),
            "环外密钥的签名必须被拒"
        );
    }

    /// 空签名 → 拒绝（不得把空串当合法签名）。
    #[test]
    fn empty_signature_is_rejected() {
        let (ring, _key, mut r) = ring_and_signed_req(|_| {});
        r.sig = String::new();
        assert!(verify_receipt_signature(&ring, &r, now(), 300).is_err());
        r.sig = "   ".into();
        assert!(verify_receipt_signature(&ring, &r, now(), 300).is_err());
    }

    /// **时序窗先于验签**：超窗 + 完全无效签名 → 报的是"时间偏移"而非"验签失败"。
    ///
    /// 这条守住校验顺序：若实现把验签放在时间窗之前，本测试会因错误类型不符而失败。
    #[test]
    fn timestamp_window_is_checked_before_signature() {
        let (ring, _key, mut r) = ring_and_signed_req(|_| {});
        r.sig = "not-a-signature-at-all".into();

        // ts 距 now 超过窗口。
        let err =
            verify_receipt_signature(&ring, &r, now() + 10_000, 300).expect_err("超窗必须被拒");
        let msg = err.to_string();
        assert!(
            msg.contains("timestamp skew"),
            "应先报时间偏移（说明时序窗在验签之前），实际: {msg}"
        );
    }

    /// 只接受 STANDARD base64：URL-safe / no-pad 变体必须被拒。
    #[test]
    fn only_standard_base64_is_accepted() {
        let (ring, _key, mut r) = ring_and_signed_req(|_| {});
        let std_sig = r.sig.clone();

        // 基线（STANDARD）可验签。
        verify_receipt_signature(&ring, &r, now(), 300).expect("standard base64 must work");

        // 转成 URL-safe（`+`→`-`, `/`→`_`）——只有含这些字符时才有区别，
        // 故先确认原串确实含 `+` 或 `/`，否则换一个必然含的输入不可行，
        // 此时退化为断言"no-pad 被拒"。
        let urlsafe = std_sig.replace('+', "-").replace('/', "_");
        if urlsafe != std_sig {
            r.sig = urlsafe;
            assert!(
                verify_receipt_signature(&ring, &r, now(), 300).is_err(),
                "URL-safe base64 必须被拒（只接受 STANDARD）"
            );
        }

        // no-pad：去掉 `=` 填充。
        let unpadded = std_sig.trim_end_matches('=').to_string();
        if unpadded.len() != std_sig.len() {
            r.sig = unpadded;
            assert!(
                verify_receipt_signature(&ring, &r, now(), 300).is_err(),
                "no-pad base64 必须被拒（只接受带填充的 STANDARD）"
            );
        }

        // 非 base64 字符。
        r.sig = "!!!not base64!!!".into();
        assert!(verify_receipt_signature(&ring, &r, now(), 300).is_err());
    }

    /// 64 字节以外的 base64（长度不足）必须被拒。
    #[test]
    fn wrong_length_signature_is_rejected() {
        let (ring, _key, mut r) = ring_and_signed_req(|_| {});
        r.sig = B64.encode([0u8; 32]);
        assert!(verify_receipt_signature(&ring, &r, now(), 300).is_err());
    }

    /// `seq_from > seq_to` 必须被拒（区间方向非法）。
    #[test]
    fn inverted_seq_range_is_rejected() {
        let (ring, _key, mut r) = ring_and_signed_req(|_| {});
        r.seq_from = "100".into();
        r.seq_to = "1".into();
        assert!(verify_receipt_signature(&ring, &r, now(), 300).is_err());
    }

    /// 数值字段不是合法整数（含 JSON number 风格的小数 / 科学计数）→ 拒绝。
    #[test]
    fn non_integer_numeric_fields_are_rejected() {
        for bad in ["1.0", "1e3", "abc", "", " 999999999999999999999999999999 "] {
            let (ring, _key, mut r) = ring_and_signed_req(|_| {});
            r.seq_from = bad.to_string();
            assert!(
                verify_receipt_signature(&ring, &r, now(), 300).is_err(),
                "非整数字段 `{bad}` 必须被拒"
            );
        }
    }

    /// `parse_receipt_ints` 对**超大**整数字符串（> i64）应报错而非静默截断。
    #[test]
    fn parse_receipt_ints_rejects_overflow_instead_of_truncating() {
        let over = "99999999999999999999999999";
        assert!(parse_receipt_ints(over, "1", "1", "1").is_err());

        // 边界内的大值（2^53 以上）必须正确解析 —— 这正是"走字符串"的意义。
        let big = "9007199254740993"; // 2^53 + 1
        let (a, _, _, _) = parse_receipt_ints(big, "1", "1", "1").expect("must parse");
        assert_eq!(a, 9_007_199_254_740_993);
    }

    /// `decode_signature` 对非法输入返回错误且**不回显输入内容**（防探测）。
    #[test]
    fn decode_signature_error_does_not_echo_input() {
        let secret_like = "QUJDREVGR0hJSktMTU5PUFFSU1RVVldYWVo";
        let err = decode_signature(secret_like).expect_err("must fail");
        let msg = err.to_string();
        assert!(
            !msg.contains(secret_like),
            "错误信息不得回显输入内容：{msg}"
        );
    }
}

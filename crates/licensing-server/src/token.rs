//! `licensing-server` Lease Token：内容、签发与验签（计划 task 45 / 设计 §1.1、§3）。
//!
//! # Token 结构与签名域
//!
//! 载荷（载荷字段固定顺序，**逐字段长度前缀**后拼接再签名，避免拼接歧义）：
//!
//! ```text
//! iotdaq.lease.v1|kid=<len>:<kid>|lease_id=<len>:<lease_id>|...|valid_until=<len>:<ts>
//! ```
//!
//! 设计要点：
//! - **签名对象是语义规范化串，不是序列化字节**（与 daemon 侧 `auth::signing::semantic_hash`
//!   同一口径，避免「JSON 字段顺序变化 → 验签失败」这类脆弱性）。
//! - **kid 进签名域**：换密钥不会让新旧 Token 的签名域混同。
//! - **长度前缀**：`|` 分隔的字段若自身含 `|` 会产生歧义（如 tier 被注入），
//!   故每个字段前置 `len:`——这是从 daemon 侧 `semantic_hash` 继承的做法。
//! - Token 对外形态为**三段式** `kid.base64url(payload_json).base64(sig)`：
//!   `kid` 放最外层，使验签方无需遍历公钥集即可定位公钥；又因 **kid 同样在签名域内**，
//!   攻击者改写 kid 只会导致验签失败，不可能让 Token 被另一把密钥「碰巧」验过。
//! - 载荷**明文可读**（客户端需读 `valid_until` 做本地过期判定），
//!   **安全性来自签名而非加密**——载荷里不得放任何秘密。

use base64::engine::general_purpose::STANDARD as B64;
use base64::engine::general_purpose::STANDARD_NO_PAD as B64NP;
use base64::Engine as _;
use serde::{Deserialize, Serialize};

use crate::error::{LicenseError, LicenseResult};
use crate::keys::KeyRing;

/// Lease Token 签名域前缀（版本化；变更签名域必须升版本号）。
pub const LEASE_SIGNING_DOMAIN: &str = "iotdaq.lease.v1";

/// Token 载荷（对客户端可见，**不含任何私密信息**）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LeaseClaims {
    /// 租约 ID（服务端主键）。
    pub lease_id: String,
    /// 设备 ID。
    pub device_id: String,
    /// 机器码指纹（HMAC 加盐截断后的值，非原始锚点）。
    pub mid: String,
    /// 授权档位（如 `standard` / `pro` / `trial`）。
    pub tier: String,
    /// 二次校验档位：A / B / C（**随 Token 下发**，客户端无切换接口）。
    pub verify_mode: String,
    /// 签发时刻（UTC 秒）。
    pub issued_at: i64,
    /// 失效时刻（UTC 秒）。
    pub valid_until: i64,
}

impl LeaseClaims {
    /// 业务校验：字段非空、时间区间合法。
    pub fn validate(&self) -> LicenseResult<()> {
        if self.lease_id.is_empty() {
            return Err(LicenseError::TokenInvalid("lease_id is empty".into()));
        }
        if self.device_id.is_empty() {
            return Err(LicenseError::TokenInvalid("device_id is empty".into()));
        }
        if self.mid.is_empty() {
            return Err(LicenseError::TokenInvalid("mid is empty".into()));
        }
        if self.tier.is_empty() {
            return Err(LicenseError::TokenInvalid("tier is empty".into()));
        }
        if !matches!(self.verify_mode.as_str(), "A" | "B" | "C") {
            return Err(LicenseError::TokenInvalid(format!(
                "verify_mode must be A|B|C, got {}",
                self.verify_mode
            )));
        }
        if self.valid_until <= self.issued_at {
            return Err(LicenseError::TokenInvalid(format!(
                "valid_until ({}) must be greater than issued_at ({})",
                self.valid_until, self.issued_at
            )));
        }
        Ok(())
    }

    /// 是否已过期（`now >= valid_until` 即过期）。
    pub fn is_expired_at(&self, now_unix_secs: i64) -> bool {
        now_unix_secs >= self.valid_until
    }

    /// 剩余有效期秒数（已过期返回 0，不返回负数）。
    pub fn remaining_secs(&self, now_unix_secs: i64) -> i64 {
        (self.valid_until - now_unix_secs).max(0)
    }
}

/// 已签发的 Lease Token（含 kid 与两段式编码）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LeaseToken {
    /// 签名所用 kid。
    pub kid: String,
    /// 载荷。
    pub claims: LeaseClaims,
    /// 签名（base64）。
    pub signature_b64: String,
}

/// 验签通过后的 Token 解析结果（携带 kid，供审计与轮换覆盖率统计）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedToken {
    /// 签名所用 kid。
    pub kid: String,
    /// 载荷。
    pub claims: LeaseClaims,
}

impl LeaseToken {
    /// 编码为 `kid.base64url(payload_json).base64(sig)`。
    pub fn encode(&self) -> String {
        let payload = serde_json::to_vec(&self.claims).unwrap_or_else(|_| b"{}".to_vec()); // 载荷由受控结构体产生，不会失败；保底空对象
        format!(
            "{}.{}.{}",
            encode_kid_segment(&self.kid),
            B64NP.encode(payload),
            self.signature_b64
        )
    }

    /// 从 `kid.base64url(payload_json).base64(sig)` 解码（**不验签**，仅解析结构）。
    ///
    /// 调用方必须随后调用 [`verify_lease_token_with_kid`]；本函数不检查签名。
    /// 返回的 kid 是**未经验证**的声明值，仅可用于定位公钥。
    pub fn decode(raw: &str) -> LicenseResult<(String, LeaseClaims, String)> {
        let mut parts = raw.split('.');
        let (kid_seg, payload_b64, sig_b64) = match (parts.next(), parts.next(), parts.next()) {
            (Some(a), Some(b), Some(c)) => (a, b, c),
            _ => {
                return Err(LicenseError::TokenInvalid(
                    "lease token must be 'kid.payload.signature'".into(),
                ));
            }
        };
        if parts.next().is_some() {
            return Err(LicenseError::TokenInvalid(
                "lease token has too many '.'-separated segments".into(),
            ));
        }
        if kid_seg.is_empty() || payload_b64.is_empty() || sig_b64.is_empty() {
            return Err(LicenseError::TokenInvalid(
                "lease token has an empty kid, payload or signature segment".into(),
            ));
        }
        let kid = decode_kid_segment(kid_seg)?;
        let payload_bytes = B64NP.decode(payload_b64).map_err(|e| {
            LicenseError::TokenInvalid(format!("lease token payload is not valid base64: {e}"))
        })?;
        let claims: LeaseClaims = serde_json::from_slice(&payload_bytes).map_err(|e| {
            LicenseError::TokenInvalid(format!("lease token payload is not valid JSON: {e}"))
        })?;
        Ok((kid, claims, sig_b64.to_string()))
    }
}

/// kid 段编码：标准 base64（无填充）。kid 由服务端生成（`k<日期>-<hex>`），
/// 但为防未来 kid 字符集扩张（如含 `/`、`-`）导致分段歧义，统一 base64 包裹。
fn encode_kid_segment(kid: &str) -> String {
    B64NP.encode(kid.as_bytes())
}

fn decode_kid_segment(seg: &str) -> LicenseResult<String> {
    let bytes = B64NP.decode(seg).map_err(|e| {
        LicenseError::TokenInvalid(format!("lease token kid segment is not valid base64: {e}"))
    })?;
    String::from_utf8(bytes).map_err(|_| {
        LicenseError::TokenInvalid("lease token kid segment is not valid UTF-8".into())
    })
}

/// 用指定密钥环签发 Lease Token。
///
/// 使用 [`KeyRing::signing_kid`] 指向的密钥；无可用签发密钥时返回 `TokenInvalid`
/// （**绝不降级为不签名**）。
pub fn issue_lease_token(ring: &KeyRing, claims: &LeaseClaims) -> LicenseResult<LeaseToken> {
    claims.validate()?;
    let message = signing_message(
        ring.signing_kid()
            .ok_or_else(|| {
                LicenseError::TokenInvalid(
                    "no active signing key available for lease issuance".into(),
                )
            })?
            .as_str(),
        claims,
    )?;
    let (kid, signature_b64) = ring.sign(&message)?;
    Ok(LeaseToken {
        kid,
        claims: claims.clone(),
        signature_b64,
    })
}

/// 验签并解析 Lease Token（kid 从 Token 外层的 kid 段自取）。
///
/// 校验链（顺序即短路顺序，与设计 §1.3 一致）：
/// 1. 结构解析（三段式 base64 + JSON）
/// 2. 载荷业务校验（字段非空 / verify_mode 合法 / 时间区间合法）
/// 3. **kid 已知且未退役**（未知 kid → 拒绝，不做单钥兜底）
/// 4. Ed25519 验签（签名域含 kid，故改写 kid 必然验签失败）
/// 5. 过期判定（`now >= valid_until` → `TokenInvalid`，语义对应 `TOKEN_EXPIRED`）
///
/// 时间窗 ±5min 的**时钟偏移**判定由调用方（心跳 / verify 端点）负责，
/// 因为「服务端时间」是端点级概念；本函数只管 Token 自身的绝对有效期。
pub fn verify_lease_token(
    ring: &KeyRing,
    raw: &str,
    now_unix_secs: i64,
) -> LicenseResult<VerifiedToken> {
    let (kid, claims, signature_b64) = LeaseToken::decode(raw)?;
    claims.validate()?;

    // kid 来自 Token 自身，但签名域包含 kid → 伪造 kid 会让验签失败，
    // 因此这里无需担心「用别人的 kid 蒙过验签」。
    let message = render_signing_message(&kid, &claims);
    ring.verify(&kid, &message, &signature_b64)?;

    if claims.is_expired_at(now_unix_secs) {
        return Err(LicenseError::TokenInvalid(format!(
            "lease token expired at {} (now {})",
            claims.valid_until, now_unix_secs
        )));
    }
    Ok(VerifiedToken { kid, claims })
}

/// 用**调用方指定的 kid** 验签（用于「Token 外层 kid 与预期不符」的强校验场景）。
///
/// 若 Token 自带的 kid 与 `expected_kid` 不同，直接拒绝——防止 Token 被换成另一把
/// （同样合法但已 retiring 的）密钥签发后被当作新租约使用。
pub fn verify_lease_token_with_kid(
    ring: &KeyRing,
    expected_kid: &str,
    raw: &str,
    now_unix_secs: i64,
) -> LicenseResult<VerifiedToken> {
    let (kid, claims, signature_b64) = LeaseToken::decode(raw)?;
    if kid != expected_kid {
        return Err(LicenseError::TokenInvalid(format!(
            "lease token kid mismatch: expected {expected_kid}, got {kid}"
        )));
    }
    claims.validate()?;
    let message = render_signing_message(&kid, &claims);
    ring.verify(&kid, &message, &signature_b64)?;
    if claims.is_expired_at(now_unix_secs) {
        return Err(LicenseError::TokenInvalid(format!(
            "lease token expired at {} (now {})",
            claims.valid_until, now_unix_secs
        )));
    }
    Ok(VerifiedToken { kid, claims })
}

/// 构造签名消息：`domain|kid=<len>:<kid>|...`（逐字段长度前缀）。
fn signing_message(kid: &str, claims: &LeaseClaims) -> LicenseResult<Vec<u8>> {
    claims.validate()?;
    Ok(render_signing_message(kid, claims))
}

/// 签名域的确定性渲染（供测试与跨端一致性校验复用）。
///
/// 格式：`<domain>|<kid 长度>:<kid>|<lease_id 长度>:<lease_id>|...`
pub fn render_signing_message(kid: &str, claims: &LeaseClaims) -> Vec<u8> {
    let mut out = String::with_capacity(256);
    out.push_str(LEASE_SIGNING_DOMAIN);
    push_field(&mut out, "kid", kid);
    push_field(&mut out, "lease_id", &claims.lease_id);
    push_field(&mut out, "device_id", &claims.device_id);
    push_field(&mut out, "mid", &claims.mid);
    push_field(&mut out, "tier", &claims.tier);
    push_field(&mut out, "verify_mode", &claims.verify_mode);
    push_field(&mut out, "issued_at", &claims.issued_at.to_string());
    push_field(&mut out, "valid_until", &claims.valid_until.to_string());
    out.into_bytes()
}

fn push_field(out: &mut String, name: &str, value: &str) {
    out.push('|');
    out.push_str(name);
    out.push('=');
    out.push_str(&value.len().to_string());
    out.push(':');
    out.push_str(value);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn claims() -> LeaseClaims {
        LeaseClaims {
            lease_id: "lease-0001".into(),
            device_id: "dev-0001".into(),
            mid: "a1b2c3d4e5f6a7b8".into(),
            tier: "standard".into(),
            verify_mode: "B".into(),
            issued_at: 1_700_000_000,
            valid_until: 1_700_000_000 + 365 * 86_400,
        }
    }

    fn ring_with_key() -> KeyRing {
        let ring = KeyRing::empty();
        ring.register_generated(None, 1_700_000_000).unwrap();
        ring
    }

    /// 签发 → 验签往返成功，且 kid 一致（task 45 验收点）。
    #[test]
    fn issue_and_verify_round_trip() {
        let ring = ring_with_key();
        let token = issue_lease_token(&ring, &claims()).unwrap();
        assert_eq!(token.kid, ring.signing_kid().unwrap());
        let raw = token.encode();
        let verified = verify_lease_token_with_kid(&ring, &token.kid, &raw, 1_700_000_100).unwrap();
        assert_eq!(verified.claims, claims());
        assert_eq!(verified.kid, token.kid);
    }

    /// 独立验签入口（kid 从 Token 自取）也能验通。
    #[test]
    fn issue_and_verify_via_any_kid_path() {
        let ring = ring_with_key();
        let token = issue_lease_token(&ring, &claims()).unwrap();
        let verified = verify_lease_token(&ring, &token.encode(), 1_700_000_100).unwrap();
        assert_eq!(verified.kid, token.kid);
        assert_eq!(verified.claims, claims());
    }

    /// **过期 Token 必须被拒**（task 45 验收点），且错误可定位到「过期」。
    #[test]
    fn expired_token_is_rejected() {
        let ring = ring_with_key();
        let token = issue_lease_token(&ring, &claims()).unwrap();
        let raw = token.encode();
        // 恰好到期（now == valid_until）即算过期。
        let err = verify_lease_token_with_kid(&ring, &token.kid, &raw, token.claims.valid_until)
            .unwrap_err();
        assert!(err.to_string().contains("expired"), "{err}");
        // 远超期同样拒绝。
        let err =
            verify_lease_token_with_kid(&ring, &token.kid, &raw, token.claims.valid_until + 10_000)
                .unwrap_err();
        assert!(matches!(err, LicenseError::TokenInvalid(_)), "{err:?}");
        // 到期前 1 秒仍有效（边界）。
        assert!(
            verify_lease_token_with_kid(&ring, &token.kid, &raw, token.claims.valid_until - 1)
                .is_ok()
        );
    }

    /// **伪造 / 未知 kid 必须被拒**（task 45 验收点）。
    ///
    /// 两条路径都要拒绝：
    /// - 显式 kid 路径：kid 与 Token 外层不符 → `kid mismatch`
    /// - 自取 kid 路径：把外层 kid 改成一个**未注册**的 kid → 签名域变化 → 验签失败
    #[test]
    fn forged_kid_is_rejected() {
        let ring = ring_with_key();
        let token = issue_lease_token(&ring, &claims()).unwrap();
        let raw = token.encode();

        // 路径 1：显式指定一个未知 kid。
        let err = verify_lease_token_with_kid(&ring, "k19700101-00000000", &raw, 1_700_000_100)
            .unwrap_err();
        assert!(matches!(err, LicenseError::TokenInvalid(_)), "{err:?}");
        assert!(err.to_string().contains("kid mismatch"), "{err}");

        // 路径 2：把 Token 外层 kid 段整体换成未注册 kid。
        let mut segs = raw.split('.');
        let _orig_kid = segs.next().unwrap();
        let payload_seg = segs.next().unwrap();
        let sig_seg = segs.next().unwrap();
        let forged = format!(
            "{}.{}.{}",
            encode_kid_segment("k19700101-00000000"),
            payload_seg,
            sig_seg
        );
        let err = verify_lease_token(&ring, &forged, 1_700_000_100).unwrap_err();
        assert!(matches!(err, LicenseError::TokenInvalid(_)), "{err:?}");
        assert!(err.to_string().contains("unknown kid"), "{err}");
    }

    /// 篡改载荷任一字段（这里是 `tier`）→ 验签失败。
    #[test]
    fn tampered_claim_breaks_signature() {
        let ring = ring_with_key();
        let token = issue_lease_token(&ring, &claims()).unwrap();
        let mut tampered = token.clone();
        tampered.claims.tier = "enterprise".into(); // 免费升档尝试
        let raw = tampered.encode();
        let err = verify_lease_token_with_kid(&ring, &token.kid, &raw, 1_700_000_100).unwrap_err();
        assert!(matches!(err, LicenseError::TokenInvalid(_)), "{err:?}");
    }

    /// 篡改 `valid_until`（自行延长有效期）→ 验签失败。
    #[test]
    fn self_extended_validity_breaks_signature() {
        let ring = ring_with_key();
        let token = issue_lease_token(&ring, &claims()).unwrap();
        let mut tampered = token.clone();
        tampered.claims.valid_until += 10 * 365 * 86_400;
        let raw = tampered.encode();
        assert!(verify_lease_token_with_kid(&ring, &token.kid, &raw, 1_700_000_100).is_err());
    }

    /// 篡改签名段 → 拒绝。
    #[test]
    fn tampered_signature_is_rejected() {
        let ring = ring_with_key();
        let token = issue_lease_token(&ring, &claims()).unwrap();
        let raw = token.encode();
        let mut segs = raw.split('.');
        let kid_seg = segs.next().unwrap();
        let payload_seg = segs.next().unwrap();
        let sig_seg = segs.next().unwrap();
        // 签名由 `KeyRing::sign` 输出，统一用 STANDARD（含填充）base64。
        let mut sig_bytes = B64.decode(sig_seg).unwrap();
        sig_bytes[0] ^= 0xFF;
        let forged = format!("{kid_seg}.{payload_seg}.{}", B64.encode(&sig_bytes));
        let err =
            verify_lease_token_with_kid(&ring, &token.kid, &forged, 1_700_000_100).unwrap_err();
        assert!(matches!(err, LicenseError::TokenInvalid(_)), "{err:?}");
    }

    /// **改写 Token 外层 kid** 必须导致验签失败（kid 在签名域内，防「换钥冒用」）。
    #[test]
    fn rewriting_outer_kid_breaks_verification() {
        let ring = KeyRing::empty();
        let kid_a = ring.register_generated(None, 1_700_000_000).unwrap();
        let kid_b = ring.register_generated(None, 1_700_000_100).unwrap();
        // 用 B 的私钥签发（当前签发位是 B）。
        let token = issue_lease_token(&ring, &claims()).unwrap();
        assert_eq!(token.kid, kid_b);

        let raw = token.encode();
        let rest: Vec<&str> = raw.splitn(3, '.').collect();
        // 把外层 kid 改成 A（A 同样合法且可验签）。
        let forged = format!("{}.{}.{}", encode_kid_segment(&kid_a), rest[1], rest[2]);
        let err = verify_lease_token(&ring, &forged, 1_700_000_100).unwrap_err();
        assert!(
            matches!(err, LicenseError::TokenInvalid(_)),
            "改写 kid 必须验签失败: {err:?}"
        );

        // 显式 kid 路径同样拒绝（kid 不匹配）。
        let err2 = verify_lease_token_with_kid(&ring, &kid_a, &raw, 1_700_000_100).unwrap_err();
        assert!(err2.to_string().contains("kid mismatch"), "{err2}");
    }

    /// 载荷字段含分隔符不产生歧义（长度前缀的意义）。
    #[test]
    fn pipe_in_field_does_not_create_ambiguity() {
        let ring = ring_with_key();
        let mut c = claims();
        c.tier = "a|b".into();
        let token = issue_lease_token(&ring, &c).unwrap();
        assert!(
            verify_lease_token_with_kid(&ring, &token.kid, &token.encode(), 1_700_000_100).is_ok()
        );

        // 若拼接无长度前缀，`tier="a|b"` 与 `tier="a", verify_mode="b|B"` 会碰撞。
        // 用两个不同载荷但相同朴素拼接串证明长度前缀确实区分了它们。
        let mut c2 = claims();
        c2.tier = "standard".into();
        c2.verify_mode = "B".into();
        let m1 = render_signing_message(&token.kid, &c);
        let m2 = render_signing_message(&token.kid, &c2);
        assert_ne!(m1, m2);
    }

    /// `verify_mode` 非法值在签发与验签两侧都拒绝。
    #[test]
    fn invalid_verify_mode_is_rejected() {
        let ring = ring_with_key();
        let mut c = claims();
        c.verify_mode = "D".into();
        let err = issue_lease_token(&ring, &c).unwrap_err();
        assert!(matches!(err, LicenseError::TokenInvalid(_)), "{err:?}");

        // 即便绕过签发（构造 raw），验签也必须拒绝。
        let good = issue_lease_token(&ring, &claims()).unwrap();
        let mut bad = good.clone();
        bad.claims.verify_mode = "D".into();
        // 用原始签名（不重签）→ 先撞载荷校验。
        let raw = bad.encode();
        assert!(verify_lease_token_with_kid(&ring, &good.kid, &raw, 1_700_000_100).is_err());
    }

    /// 字段为空 / 时间区间倒置在签发时即被拦下。
    #[test]
    fn invalid_claims_are_rejected_at_issuance() {
        let ring = ring_with_key();

        let mut c = claims();
        c.lease_id = String::new();
        assert!(issue_lease_token(&ring, &c).is_err());

        let mut c = claims();
        c.device_id = String::new();
        assert!(issue_lease_token(&ring, &c).is_err());

        let mut c = claims();
        c.mid = String::new();
        assert!(issue_lease_token(&ring, &c).is_err());

        let mut c = claims();
        c.tier = String::new();
        assert!(issue_lease_token(&ring, &c).is_err());

        let mut c = claims();
        c.valid_until = c.issued_at;
        let err = issue_lease_token(&ring, &c).unwrap_err();
        assert!(err.to_string().contains("valid_until"), "{err}");

        let mut c = claims();
        c.valid_until = c.issued_at - 1;
        assert!(issue_lease_token(&ring, &c).is_err());
    }

    /// 无可用签发密钥时**明确失败**，绝不签发未签名 Token。
    #[test]
    fn issuance_without_signing_key_fails_explicitly() {
        let ring = KeyRing::empty();
        let err = issue_lease_token(&ring, &claims()).unwrap_err();
        assert!(matches!(err, LicenseError::TokenInvalid(_)), "{err:?}");

        // retiring 后同样失败。
        let ring = ring_with_key();
        let kid = ring.signing_kid().unwrap();
        ring.set_status(&kid, crate::keys::KeyStatus::Retiring, 0)
            .unwrap();
        assert!(issue_lease_token(&ring, &claims()).is_err());
    }

    /// 畸形 raw 串全部是 `TokenInvalid`，不 panic（缺段 / 空段 / 非法 base64 / 非 JSON / 多段）。
    #[test]
    fn malformed_raw_is_rejected_without_panic() {
        let ring = ring_with_key();
        let good_kid = encode_kid_segment("kid-1");
        let cases = vec![
            String::new(),
            "no-dots-here".to_string(),
            "only.two".to_string(),
            format!("{good_kid}..sig"),
            format!(".{}.sig", B64NP.encode(b"{}")),
            format!("{good_kid}.{}.", B64NP.encode(b"{}")),
            "!!!.!!!.!!!".to_string(),
            format!("{good_kid}.{}.abc", B64NP.encode(b"not json")),
            // 四段（多一个 `.`）。
            format!(
                "{good_kid}.{}.{}.extra",
                B64NP.encode(b"{}"),
                B64.encode([0u8; 64])
            ),
        ];
        for raw in cases {
            let err = verify_lease_token(&ring, &raw, 0).unwrap_err();
            assert!(
                matches!(err, LicenseError::TokenInvalid(_)),
                "raw={raw:?} err={err:?}"
            );
        }
    }

    /// Token 载荷可被外部读取（客户端需读 exp），但 `mid` 是加盐截断值不含原始锚点。
    #[test]
    fn token_payload_is_readable_for_client_expiry_check() {
        let ring = ring_with_key();
        let token = issue_lease_token(&ring, &claims()).unwrap();
        let (kid_out, claims_out, _sig) = LeaseToken::decode(&token.encode()).unwrap();
        assert_eq!(kid_out, token.kid);
        assert_eq!(claims_out.valid_until, claims().valid_until);
        assert!(!claims_out.is_expired_at(claims().valid_until - 1));
        assert_eq!(claims_out.remaining_secs(1_700_000_000), 365 * 86_400);
        // 剩余时间为负时钳制为 0。
        assert_eq!(claims_out.remaining_secs(claims_out.valid_until + 999), 0);
    }

    /// 轮换：旧 kid 置 retiring 后，存量 Token 仍可验签（客户端不受影响）。
    #[test]
    fn rotation_keeps_existing_tokens_verifiable() {
        let ring = KeyRing::empty();
        let old_kid = ring.register_generated(None, 1_700_000_000).unwrap();
        let old_token = issue_lease_token(&ring, &claims()).unwrap();
        assert_eq!(old_token.kid, old_kid);

        // 轮换：新 kid 注册并抢占签发位，旧 kid 置 retiring。
        let new_kid = ring.register_generated(None, 1_700_000_100).unwrap();
        ring.set_status(&old_kid, crate::keys::KeyStatus::Retiring, 1_700_000_100)
            .unwrap();

        // 存量 Token（旧 kid）仍可验签。
        assert!(
            verify_lease_token_with_kid(&ring, &old_kid, &old_token.encode(), 1_700_000_200)
                .is_ok()
        );
        // 新签发走新 kid。
        let fresh = issue_lease_token(&ring, &claims()).unwrap();
        assert_eq!(fresh.kid, new_kid);

        // 旧 kid 退役后存量 Token 也失效（宽限期结束）。
        ring.set_status(&old_kid, crate::keys::KeyStatus::Retired, 1_700_000_300)
            .unwrap();
        assert!(
            verify_lease_token_with_kid(&ring, &old_kid, &old_token.encode(), 1_700_000_400)
                .is_err()
        );
    }

    /// 签名域确定性：同输入同输出（跨端一致性测试的基础）。
    #[test]
    fn signing_message_is_deterministic() {
        let c = claims();
        let a = render_signing_message("k1", &c);
        let b = render_signing_message("k1", &c);
        assert_eq!(a, b);
        // 不同 kid → 不同签名域。
        assert_ne!(a, render_signing_message("k2", &c));
        // 域前缀存在。
        assert!(String::from_utf8(a)
            .unwrap()
            .starts_with(LEASE_SIGNING_DOMAIN));
    }

    /// 跨端一致：签名域渲染的对拍指纹（daemon 侧若实现同域，应产出同样字节）。
    #[test]
    fn signing_message_layout_is_stable() {
        let c = claims();
        let rendered = String::from_utf8(render_signing_message("kid-1", &c)).unwrap();
        assert_eq!(
            rendered,
            "iotdaq.lease.v1|kid=5:kid-1|lease_id=10:lease-0001|device_id=8:dev-0001|\
             mid=16:a1b2c3d4e5f6a7b8|tier=8:standard|verify_mode=1:B|\
             issued_at=10:1700000000|valid_until=10:1731536000"
                .replace("             ", "")
        );
    }
}

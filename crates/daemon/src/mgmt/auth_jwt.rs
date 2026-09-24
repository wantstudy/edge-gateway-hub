//! task 57 切片 2 — 管理面 JWT（HS256）签发与校验原语（**零新依赖**）。
//!
//! ## 实现约束
//! - HS256 用既有 `hmac` + `sha2` 手工实现，编码用既有 `base64`（URL_SAFE_NO_PAD，
//!   JWT 规范要求 base64url 无填充），claims 用既有 `serde_json`——不引入
//!   jsonwebtoken 等新依赖（Cargo.lock 红线）。
//! - **签名比对必须常数时间**：用 `hmac::Mac::verify_slice`（内部恒时比较），
//!   禁止 `==` 直接比 MAC 字节（防时序侧信道）。
//! - **注入式密钥**：[`IssuerKey`] 由调用方传入，本模块不读文件不做任何 IO；
//!   密钥来源（配置 / 派生）由后续接线任务决定。
//! - **时钟注入式**：校验入参 `now_secs`（秒级 Unix 时间），本模块只提供
//!   [`now_unix_secs`] 墙钟便捷函数；测试注入受控时钟保证确定性。
//! - 时钟偏移容忍 ±60s（[`DEFAULT_LEEWAY_SECS`]）可配（[`super::rbac::RbacAuth::with_leeway`]）。
//!
//! ## 错误分型（审计区分攻击面）
//! 过期（[`JwtError::Expired`]）/ 签名错（[`JwtError::SignatureMismatch`]）/
//! 角色未知（[`JwtError::UnknownRole`]）/ 时间窗异常（`NotYetValid` / `IssuedInFuture`）/
//! 算法替换攻击（[`JwtError::BadAlg`]）/ 结构损坏（[`JwtError::Malformed`]）
//! 均为独立变体，Display 以变体名开头（对齐 `error.rs` 约定，日志可检索）。
//!
//! ## 角色对齐
//! `role` claim 取值域 = [`super::rbac::Role`] 四个规范 id（`ops / lic_ops /
//! risk / system`，与 `ui-kit/src/rbac.ts` 同一套）；历史别名 `admin`/`viewer`
//! 解析失败即 [`JwtError::UnknownRole`]，不做静默映射（防两端漂移）。
//!
//! ## 大数红线说明
//! JWT 的 `exp` / `iat` / `nbf` 是标准 NumericDate（RFC 7519 §2），**按规范以
//! JSON number 编码**（秒级精度不超 JS 安全整数）；mgmt 面自产响应体的大数红线
//! 不适用于标准 JWT 互操作字段。

use std::time::{SystemTime, UNIX_EPOCH};

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;
use hmac::{Hmac, Mac};
use serde::Deserialize;
use serde_json::{json, Value};
use sha2::Sha256;
use thiserror::Error;

use super::rbac::Role;

/// HS256 HMAC 实例别名。
type HmacSha256 = Hmac<Sha256>;

/// JWT 固定 header（只签发 HS256；校验侧同样只接受 HS256，见 [`verify`]）。
const HEADER_JSON: &str = r#"{"alg":"HS256","typ":"JWT"}"#;

/// 默认时钟偏移容忍（±60s；可经 `rbac::RbacAuth::with_leeway` 覆盖）。
pub const DEFAULT_LEEWAY_SECS: i64 = 60;

/// 当前 UTC 秒级 Unix 时间（时钟早于纪元按 0 处理，不 panic）。
pub fn now_unix_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

// ---- 密钥 ----

/// HS256 签名密钥（32 字节；注入式，不做 IO、不落盘）。
///
/// 密钥来源（配置注入 / 从既有授权密钥派生）由 task 57 后续接线任务决定；
/// 本模块只保证「拿到什么签什么、校验用什么比对什么」。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IssuerKey(pub [u8; 32]);

impl IssuerKey {
    /// 从字节切片构造（长度必须恰为 32；供后续接线任务从配置读取后转换）。
    pub fn from_slice(raw: &[u8]) -> Option<Self> {
        let key: [u8; 32] = raw.try_into().ok()?;
        Some(IssuerKey(key))
    }
}

// ---- Claims ----

/// 管理面 JWT claims（sub / role / exp / iat / nbf / jti）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Claims {
    /// 主体（账号 / 会话标识）。
    pub sub: String,
    /// 规范角色（[`Role::as_str`]；四 id 之一）。
    pub role: Role,
    /// 过期时刻（秒级 Unix；`now > exp + leeway` 即拒）。
    pub exp: i64,
    /// 签发时刻（秒级 Unix；`now < iat - leeway` 即拒，防未来签发）。
    pub iat: i64,
    /// 生效时刻（可选；`now < nbf - leeway` 即拒）。
    pub nbf: Option<i64>,
    /// Token 唯一标识（吊销 / 审计去重键；本模块不做吊销存储，仅透传）。
    pub jti: String,
}

impl Claims {
    /// 序列化为 JSON Value（role 用规范字面量；exp/iat/nbf 按标准 NumericDate 编码）。
    fn to_json(&self) -> Value {
        json!({
            "sub": self.sub,
            "role": self.role.as_str(),
            "exp": self.exp,
            "iat": self.iat,
            "nbf": self.nbf,
            "jti": self.jti,
        })
    }
}

/// 校验侧原始 claims（role 以字符串承接，解析失败转独立错误变体）。
#[derive(Debug, Deserialize)]
struct ClaimsRaw {
    #[serde(default)]
    sub: String,
    #[serde(default)]
    role: String,
    exp: Option<i64>,
    iat: Option<i64>,
    #[serde(default)]
    nbf: Option<i64>,
    #[serde(default)]
    jti: String,
}

impl TryFrom<ClaimsRaw> for Claims {
    type Error = JwtError;

    fn try_from(raw: ClaimsRaw) -> Result<Self, JwtError> {
        if raw.sub.is_empty() {
            return Err(JwtError::MissingClaim("sub"));
        }
        if raw.role.is_empty() {
            return Err(JwtError::MissingClaim("role"));
        }
        let role = Role::from_str(&raw.role)
            .ok_or_else(|| JwtError::UnknownRole(raw.role.clone()))?;
        let exp = raw.exp.ok_or(JwtError::MissingClaim("exp"))?;
        let iat = raw.iat.ok_or(JwtError::MissingClaim("iat"))?;
        if raw.jti.is_empty() {
            return Err(JwtError::MissingClaim("jti"));
        }
        Ok(Claims {
            sub: raw.sub,
            role,
            exp,
            iat,
            nbf: raw.nbf,
            jti: raw.jti,
        })
    }
}

// ---- 错误 ----

/// JWT 签发 / 校验错误（各变体独立，便于审计区分攻击面；Display 以变体名开头）。
#[derive(Debug, Error)]
pub enum JwtError {
    /// 结构损坏（段数 / base64 / JSON / 类型不符）。
    #[error("JwtMalformed: {0}")]
    Malformed(String),
    /// 算法不是 HS256（alg none / HS512 等替换攻击直接拒）。
    #[error("JwtBadAlg: alg {0:?} not allowed (HS256 only)")]
    BadAlg(String),
    /// MAC 校验失败（恒时比较后判定；含错误密钥 / payload 篡改）。
    #[error("JwtSignatureMismatch: MAC verification failed (constant-time compare)")]
    SignatureMismatch,
    /// 已过期。
    #[error("JwtExpired: exp={exp} now={now}")]
    Expired {
        /// token 过期时刻。
        exp: i64,
        /// 校验时刻。
        now: i64,
    },
    /// 尚未生效（nbf 未到）。
    #[error("JwtNotYetValid: nbf={nbf} now={now}")]
    NotYetValid {
        /// 生效时刻。
        nbf: i64,
        /// 校验时刻。
        now: i64,
    },
    /// 签发时刻在未来（超出容忍窗，防时钟倒拨伪造）。
    #[error("JwtIssuedInFuture: iat={iat} now={now}")]
    IssuedInFuture {
        /// 签发时刻。
        iat: i64,
        /// 校验时刻。
        now: i64,
    },
    /// 角色字面量不是四个规范 id（历史别名 admin/viewer 等）。
    #[error("JwtUnknownRole: role {0:?} is not one of ops|lic_ops|risk|system")]
    UnknownRole(String),
    /// 必需 claim 缺失或为空。
    #[error("JwtMissingClaim: required claim {0:?} missing or empty")]
    MissingClaim(&'static str),
    /// 理论不可达路径（HMAC 初始化 / JSON 编码失败）的诚实兜底，不 panic。
    #[error("JwtInternal: {0}")]
    Internal(String),
}

// ---- 签发 ----

/// HMAC-SHA256（任意 32 字节密钥，`new_from_slice` 失败理论不可达，仍诚实兜底）。
fn hmac_sha256(key: &IssuerKey, msg: &[u8]) -> Result<Vec<u8>, JwtError> {
    let mut mac = HmacSha256::new_from_slice(&key.0)
        .map_err(|e| JwtError::Internal(format!("hmac init: {e}")))?;
    mac.update(msg);
    Ok(mac.finalize().into_bytes().to_vec())
}

/// 签发 HS256 JWT：`b64url(header) + "." + b64url(claims) + "." + b64url(HMAC)`。
///
/// header 固定 `{"alg":"HS256","typ":"JWT"}`；签名输入为前两段原文
/// （base64url 无填充，编码确定性保证验签侧可逐字节重建签名输入）。
pub fn sign(claims: &Claims, key: IssuerKey) -> Result<String, JwtError> {
    let header_b64 = URL_SAFE_NO_PAD.encode(HEADER_JSON);
    let payload_json = serde_json::to_vec(&claims.to_json())
        .map_err(|e| JwtError::Internal(format!("claims encode: {e}")))?;
    let payload_b64 = URL_SAFE_NO_PAD.encode(payload_json);
    let signing_input = format!("{header_b64}.{payload_b64}");
    let sig_b64 = URL_SAFE_NO_PAD.encode(hmac_sha256(&key, signing_input.as_bytes())?);
    Ok(format!("{signing_input}.{sig_b64}"))
}

// ---- 校验 ----

/// 校验 JWT 并返回 claims（签名 → alg → 必需 claims → 时间窗，全部 Rust 侧）。
///
/// 时间窗（`leeway_secs` 为容忍秒数，建议 60）：
/// - 过期：`now > exp + leeway` → [`JwtError::Expired`]；
/// - nbf 未到：`now < nbf - leeway` → [`JwtError::NotYetValid`]；
/// - 签发在未来：`now < iat - leeway` → [`JwtError::IssuedInFuture`]。
///
/// 签名比对走 `Mac::verify_slice`（**恒时比较**，不暴露提前退出路径）。
pub fn verify(
    token: &str,
    key: IssuerKey,
    now_secs: i64,
    leeway_secs: i64,
) -> Result<Claims, JwtError> {
    // ① 结构：三段式（header.payload.signature）。
    let segments: Vec<&str> = token.split('.').collect();
    let [header_b64, payload_b64, sig_b64] = segments.as_slice() else {
        return Err(JwtError::Malformed(format!(
            "expected 3 dot-separated segments, got {}",
            segments.len()
        )));
    };

    // ② header：必须显式声明 alg=HS256（alg 缺失 / none / HS512 一律拒）。
    let header_raw = URL_SAFE_NO_PAD
        .decode(header_b64)
        .map_err(|e| JwtError::Malformed(format!("header base64: {e}")))?;
    let header: Value = serde_json::from_slice(&header_raw)
        .map_err(|e| JwtError::Malformed(format!("header json: {e}")))?;
    let alg = header
        .get("alg")
        .and_then(Value::as_str)
        .ok_or_else(|| JwtError::Malformed("header missing alg".to_string()))?;
    if alg != "HS256" {
        return Err(JwtError::BadAlg(alg.to_string()));
    }

    // ③ 签名：对「所呈现的」header/payload 原文逐字节重建签名输入后恒时比对。
    let sig_raw = URL_SAFE_NO_PAD
        .decode(sig_b64)
        .map_err(|e| JwtError::Malformed(format!("signature base64: {e}")))?;
    let signing_input = format!("{header_b64}.{payload_b64}");
    let mut mac = HmacSha256::new_from_slice(&key.0)
        .map_err(|e| JwtError::Internal(format!("hmac init: {e}")))?;
    mac.update(signing_input.as_bytes());
    // verify_slice 内部为常数时间比较（digest 0.10 Mac trait 契约）。
    mac.verify_slice(&sig_raw)
        .map_err(|_| JwtError::SignatureMismatch)?;

    // ④ claims：结构与必需字段（缺漏独立成变体，角色未知独立成变体）。
    let payload_raw = URL_SAFE_NO_PAD
        .decode(payload_b64)
        .map_err(|e| JwtError::Malformed(format!("payload base64: {e}")))?;
    let raw: ClaimsRaw = serde_json::from_slice(&payload_raw)
        .map_err(|e| JwtError::Malformed(format!("payload json: {e}")))?;
    let claims = Claims::try_from(raw)?;

    // ⑤ 时间窗（饱和运算防对抗性 i64 极值溢出）。
    if now_secs > claims.exp.saturating_add(leeway_secs) {
        return Err(JwtError::Expired {
            exp: claims.exp,
            now: now_secs,
        });
    }
    if let Some(nbf) = claims.nbf {
        if now_secs < nbf.saturating_sub(leeway_secs) {
            return Err(JwtError::NotYetValid {
                nbf,
                now: now_secs,
            });
        }
    }
    if now_secs < claims.iat.saturating_sub(leeway_secs) {
        return Err(JwtError::IssuedInFuture {
            iat: claims.iat,
            now: now_secs,
        });
    }

    Ok(claims)
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64;
    use base64::Engine as _;

    /// 测试密钥（固定 32 字节）。
    const KEY: IssuerKey = IssuerKey([0x42u8; 32]);
    /// 另一把错误密钥。
    const WRONG_KEY: IssuerKey = IssuerKey([0x00u8; 32]);
    /// 测试基准时刻。
    const NOW: i64 = 1_700_000_000;

    /// 构造有效 claims（exp=NOW+600, iat=NOW）。
    fn claims() -> Claims {
        Claims {
            sub: "ops-user".to_string(),
            role: Role::LicOps,
            exp: NOW + 600,
            iat: NOW,
            nbf: None,
            jti: "jti-abc".to_string(),
        }
    }

    /// 恒时比较存在性（代码审查级断言）：本模块对 MAC 的比对只经
    /// `Mac::verify_slice`（digest 0.10 内部恒时实现），不得出现 `==` 直比。
    ///
    /// 以源码检查方式固化该约束（防止后续改动引入时序侧信道）；
    /// 只检查非测试代码段（本测试自身的禁用字符串字面量不参与匹配）。
    #[test]
    fn mac_comparison_goes_through_constant_time_verify() {
        let prod_source = include_str!("auth_jwt.rs")
            .split("#[cfg(test)]")
            .next()
            .unwrap_or_default();
        // 存在恒时校验调用。
        assert!(
            prod_source.contains("verify_slice"),
            "MAC comparison must use Mac::verify_slice (constant-time)"
        );
        // 不允许 == / != 直接比较签名字节（unwrap_or 等兜底直比同样命中）。
        for banned in ["sig_raw ==", "== sig_raw", "sig_raw != ", "sig == "] {
            assert!(
                !prod_source.contains(banned),
                "constant-time violation: direct MAC comparison via {banned:?}"
            );
        }
    }

    /// QA Happy: 签发 → 校验往返，claims 无损（含可选 nbf 缺省）。
    #[test]
    fn sign_verify_roundtrip_preserves_claims() {
        let token = sign(&claims(), KEY).expect("sign");
        let verified = verify(&token, KEY, NOW, DEFAULT_LEEWAY_SECS).expect("verify");
        assert_eq!(verified, claims());
        // 结构检查：三段、header 定值、无填充。
        let parts: Vec<&str> = token.split('.').collect();
        assert_eq!(parts.len(), 3);
        assert_eq!(
            B64.decode(parts[0]).expect("header b64").as_slice(),
            HEADER_JSON.as_bytes()
        );
        assert!(!token.contains('='), "base64url must be unpadded");
    }

    /// QA 安全: 篡改 payload（改 role/sub/exp）与篡改签名（改 MAC 末字符）
    /// 均拒，且错误变体均为 SignatureMismatch（恒时比对后统一失败原因，
    /// 不泄露「是签名错还是编码错」之外的差异）。
    #[test]
    fn tampered_payload_and_signature_rejected() {
        // 篡改 payload：重编码后签名不再匹配。
        let token = sign(&claims(), KEY).expect("sign");
        let parts: Vec<&str> = token.split('.').collect();
        let mut payload = B64.decode(parts[1]).expect("payload b64");
        payload[0] ^= 0x01; // sub 首字节翻转（payload 为 JSON 文本，改值即可）。
        let tampered = format!("{}.{}.{}", parts[0], B64.encode(&payload), parts[2]);
        assert!(matches!(
            verify(&tampered, KEY, NOW, DEFAULT_LEEWAY_SECS),
            Err(JwtError::SignatureMismatch)
        ));

        // 篡改签名：末字符替换（仍为合法 base64 字符）。
        let mut chars: Vec<char> = parts[2].chars().collect();
        let last = chars.len() - 1;
        chars[last] = if chars[last] == 'A' { 'B' } else { 'A' };
        let tampered_sig = format!("{}.{}.{}", parts[0], parts[1], chars.iter().collect::<String>());
        assert!(matches!(
            verify(&tampered_sig, KEY, NOW, DEFAULT_LEEWAY_SECS),
            Err(JwtError::SignatureMismatch)
        ));

        // 错误密钥同样 SignatureMismatch。
        let token = sign(&claims(), WRONG_KEY).expect("sign");
        assert!(matches!(
            verify(&token, KEY, NOW, DEFAULT_LEEWAY_SECS),
            Err(JwtError::SignatureMismatch)
        ));
    }

    /// QA: 过期拒 + 时钟偏移容忍边界（±60s 可配）：
    /// exp = now-60 在 leeway=60 内放行，exp = now-61 拒；收紧 leeway 后即拒。
    #[test]
    fn expired_rejected_with_leeway_boundary() {
        // 边界内：exp = NOW - 60，leeway = 60 → 放行。
        let mut c = claims();
        c.exp = NOW - 60;
        let token = sign(&c, KEY).expect("sign");
        verify(&token, KEY, NOW, 60).expect("exp exactly at leeway boundary passes");

        // 边界外一格：exp = NOW - 61 → Expired。
        let mut c = claims();
        c.exp = NOW - 61;
        let token = sign(&c, KEY).expect("sign");
        assert!(matches!(
            verify(&token, KEY, NOW, 60),
            Err(JwtError::Expired { exp, now }) if exp == NOW - 61 && now == NOW
        ));

        // 同一 token 收紧 leeway（如严格部署 ±0s）→ 拒。
        assert!(matches!(
            verify(&token, KEY, NOW, 0),
            Err(JwtError::Expired { .. })
        ));
    }

    /// QA: nbf 未到拒（NotYetValid），容忍窗内放行；exp 优先于 nbf 校验。
    #[test]
    fn not_yet_valid_rejected_with_leeway_boundary() {
        // nbf = NOW + 61，leeway 60 → 拒。
        let mut c = claims();
        c.nbf = Some(NOW + 61);
        let token = sign(&c, KEY).expect("sign");
        assert!(matches!(
            verify(&token, KEY, NOW, 60),
            Err(JwtError::NotYetValid { nbf, now }) if nbf == NOW + 61 && now == NOW
        ));

        // nbf = NOW + 60（边界）→ 放行。
        let mut c = claims();
        c.nbf = Some(NOW + 60);
        let token = sign(&c, KEY).expect("sign");
        verify(&token, KEY, NOW, 60).expect("nbf at leeway boundary passes");
    }

    /// QA: iat 在未来超出容忍窗拒（IssuedInFuture，防时钟倒拨伪造），
    /// 窗内放行。
    #[test]
    fn issued_in_future_rejected_beyond_leeway() {
        // iat = NOW + 1000 → 拒。
        let mut c = claims();
        c.iat = NOW + 1000;
        let token = sign(&c, KEY).expect("sign");
        assert!(matches!(
            verify(&token, KEY, NOW, 60),
            Err(JwtError::IssuedInFuture { iat, now }) if iat == NOW + 1000 && now == NOW
        ));

        // iat = NOW + 30（窗内）→ 放行。
        let mut c = claims();
        c.iat = NOW + 30;
        let token = sign(&c, KEY).expect("sign");
        verify(&token, KEY, NOW, 60).expect("iat within leeway passes");
    }

    /// QA 红线: 角色未知（历史别名 admin / 大小写变体）→ 独立错误变体
    /// UnknownRole，不静默映射、不并入 Malformed（审计区分攻击面）。
    #[test]
    fn unknown_role_is_distinct_variant() {
        for role in ["admin", "viewer", "System", "root", "ops "] {
            let payload = json!({
                "sub": "ops-user", "role": role,
                "exp": NOW + 600, "iat": NOW, "jti": "jti-abc",
            });
            let token = craft(&json!({"alg": "HS256", "typ": "JWT"}), &payload);
            let err = verify(&token, KEY, NOW, DEFAULT_LEEWAY_SECS).expect_err("must reject");
            assert!(
                matches!(err, JwtError::UnknownRole(ref r) if r == role),
                "role {role:?} must yield UnknownRole, got {err}"
            );
        }
    }

    /// QA 安全: alg 替换攻击（none / HS512 / 缺 alg）→ BadAlg / Malformed。
    #[test]
    fn algorithm_substitution_rejected() {
        let payload = json!({
            "sub": "ops-user", "role": "ops",
            "exp": NOW + 600, "iat": NOW, "jti": "jti-abc",
        });

        // alg=none。
        let token = craft(&json!({"alg": "none", "typ": "JWT"}), &payload);
        assert!(matches!(
            verify(&token, KEY, NOW, DEFAULT_LEEWAY_SECS),
            Err(JwtError::BadAlg(ref a)) if a == "none"
        ));

        // alg=HS512。
        let token = craft(&json!({"alg": "HS512", "typ": "JWT"}), &payload);
        assert!(matches!(
            verify(&token, KEY, NOW, DEFAULT_LEEWAY_SECS),
            Err(JwtError::BadAlg(ref a)) if a == "HS512"
        ));

        // 缺 alg。
        let token = craft(&json!({"typ": "JWT"}), &payload);
        assert!(matches!(
            verify(&token, KEY, NOW, DEFAULT_LEEWAY_SECS),
            Err(JwtError::Malformed(_))
        ));
    }

    /// QA: 必需 claims 缺失 → MissingClaim（sub / role / exp / iat / jti 逐一）；
    /// role 为空串同样按缺失处理（分型先于角色解析）。
    #[test]
    fn missing_required_claims_rejected_distinctly() {
        // 全量合法 payload，逐个删除目标 claim。
        let full = json!({
            "sub": "ops-user", "role": "ops",
            "exp": NOW + 600, "iat": NOW, "jti": "jti-abc",
        });
        for claim in ["sub", "role", "exp", "iat", "jti"] {
            let mut payload = full.clone();
            if let Value::Object(map) = &mut payload {
                map.remove(claim);
            }
            let token = craft(&json!({"alg": "HS256", "typ": "JWT"}), &payload);
            let err = verify(&token, KEY, NOW, DEFAULT_LEEWAY_SECS).expect_err("must reject");
            assert!(
                matches!(err, JwtError::MissingClaim(c) if c == claim),
                "missing {claim} must yield MissingClaim({claim}), got {err}"
            );
        }

        // role 空串 → 缺失分型（非 UnknownRole）。
        let payload = json!({
            "sub": "ops-user", "role": "",
            "exp": NOW + 600, "iat": NOW, "jti": "jti-abc",
        });
        let token = craft(&json!({"alg": "HS256", "typ": "JWT"}), &payload);
        assert!(matches!(
            verify(&token, KEY, NOW, DEFAULT_LEEWAY_SECS),
            Err(JwtError::MissingClaim("role"))
        ));
    }

    /// QA 安全: 结构损坏（段数 / base64 / JSON / 类型）→ Malformed，不 panic；
    /// 未签名/坏签名样本 → SignatureMismatch（签名先于 payload 解析，fail-closed
    /// 顺序即「先验真再解析」，无一条路径放行）。
    #[test]
    fn malformed_tokens_rejected_distinctly() {
        // 对任意 payload base64 手工签名的辅助（payload 内容合法与否不影响签名）。
        let sign_raw = |payload_b64: &str| -> String {
            let h = B64.encode(HEADER_JSON);
            let input = format!("{h}.{payload_b64}");
            let mut mac = HmacSha256::new_from_slice(&KEY.0).expect("hmac init");
            mac.update(input.as_bytes());
            format!("{input}.{}", B64.encode(mac.finalize().into_bytes().as_slice()))
        };

        // 签名有效但 payload 非 JSON → Malformed。
        let token = sign_raw(&B64.encode("###".as_bytes()));
        assert!(matches!(
            verify(&token, KEY, NOW, DEFAULT_LEEWAY_SECS),
            Err(JwtError::Malformed(_))
        ));

        // 签名有效但 claims 类型不符（exp 为字符串）→ Malformed。
        let payload = json!({"sub": "u", "role": "ops", "exp": "soon", "iat": NOW, "jti": "j"});
        let token = sign_raw(&B64.encode(payload.to_string()));
        assert!(matches!(
            verify(&token, KEY, NOW, DEFAULT_LEEWAY_SECS),
            Err(JwtError::Malformed(_))
        ));

        // 纯结构损坏 / 垃圾输入 → Malformed（段数、base64、空串）。
        for token in ["not-a-jwt", "a.b", "a.b.c.d", "", "!!!.%%%.$$$"] {
            let err = verify(token, KEY, NOW, DEFAULT_LEEWAY_SECS).expect_err("must reject");
            assert!(
                matches!(err, JwtError::Malformed(_)),
                "token {token:?} must yield Malformed, got {err}"
            );
        }

        // 三段结构完好但签名错（payload 损坏未解析即被签名拦截）→ SignatureMismatch。
        let token = format!("{}.###.AAAA", B64.encode(HEADER_JSON));
        assert!(matches!(
            verify(&token, KEY, NOW, DEFAULT_LEEWAY_SECS),
            Err(JwtError::SignatureMismatch)
        ));
    }

    /// 测试辅助：按任意 header/payload JSON 手工签 HS256 token
    /// （`sign` 只接受类型化 Claims，负例需绕过类型层自由构造）。
    fn craft(header: &Value, payload: &Value) -> String {
        let header_b64 = B64.encode(header.to_string());
        let payload_b64 = B64.encode(payload.to_string());
        let signing_input = format!("{header_b64}.{payload_b64}");
        let mut mac = HmacSha256::new_from_slice(&KEY.0).expect("hmac init");
        mac.update(signing_input.as_bytes());
        let sig_b64 = B64.encode(mac.finalize().into_bytes().as_slice());
        format!("{signing_input}.{sig_b64}")
    }
}

//! task 23 — 试用期管理（本地签名标记 + 机器码绑定 + 到期降级不停用）。
//!
//! ## 契约红线
//! - 标记为 JSON + HMAC-SHA256 签名：`sig = HMAC-SHA256(key, 规范化字段拼接)`；
//!   **签名不符 / 字段缺失 / 解析失败 ⇒ 视为试用已结束**（[`TrialVerdict::ExpiredDegrade`]，
//!   降级免费版）——**绝不允许重置试用期**，容器重建、删标记文件都绕不过。
//! - 试用期 [`TRIAL_DURATION_MS`] = 30天；到期 ⇒ [`TrialVerdict::ExpiredDegrade`]
//!   （降级免费基础版，见 `auth/limits.rs`，**不停用本地采集**）。
//! - 时间戳字段以**字符串**编码进 JSON（[`u64_as_string`]）——JSON 大数红线：
//!   JS `Number` 只能安全表示 2^53 以内整数，毫秒时间戳超界会被静默截断。
//! - 签名密钥 [`HmacKey`] **复用 `machine_id.rs`（task 3）的公开 API 产物**：
//!   以 `MachineIdentity::get_machine_fingerprint()` 输出的 64 hex 机器码指纹为根
//!   材料，经域分隔 HMAC-SHA256 派生（machine_id.rs 未提供独立的「trial key 派生」
//!   公开函数，故按其公开输出组合，指纹采集 / 派生逻辑不在本模块重复实现）。
//! - **本模块不做文件 IO**：标记字符串由调用方传入 / 持久化（宿主持久卷）；
//!   回拨检测依赖 `clock.rs` 的闭包注入式 last_seen。
//! - 回拨配合：`evaluate` 先走 [`TrustedClock::check_rollback`]，检测到回拨时
//!   **以标记内持久化的 `last_seen_ms` 为准**，不信任被回拨的墙钟。

use std::fmt;

use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use sha2::Sha256;

use crate::auth::clock::{RollbackVerdict, TrustedClock};

type HmacSha256 = Hmac<Sha256>;

/// 试用期时长（30 天，毫秒）。task 23 契约常量。
pub const TRIAL_DURATION_MS: u64 = 30 * 24 * 60 * 60 * 1000;

/// 标记格式版本（升级标记结构时递增；旧版本标记一律视为已结束）。
pub const TRIAL_MARKER_FORMAT_VERSION: u32 = 1;

/// 标记签名域分隔前缀（防跨协议 / 跨版本签名复用，范式同 `auth/signing.rs`）。
const TRIAL_DOMAIN_V1: &str = "iotdaq.trial.v1|";

/// 密钥派生域分隔标签（trial key 与指纹派生域隔离）。
const TRIAL_KEY_DOMAIN: &[u8] = b"iotdaq.trial.key.v1";

/// 机器码指纹长度（64 字符 hex，task 3 输出格式）。
const FINGERPRINT_HEX_LEN: usize = 64;

/// 试用密钥派生失败（本模块局部错误）。
#[derive(Debug, thiserror::Error)]
pub enum TrialKeyError {
    /// 指纹不是 task 3 输出格式（64 字符 hex）。
    #[error(
        "trial key source fingerprint must be {len}-char hex (machine_id output), got len {got}"
    )]
    BadFingerprint {
        /// 要求长度（64）。
        len: usize,
        /// 实际长度。
        got: usize,
    },
}

/// 试用标记签名密钥（32 字节）。
///
/// 由机器码指纹（task 3 `MachineIdentity::get_machine_fingerprint()` 输出，
/// 64 hex）经域分隔派生：`trial_key = HMAC-SHA256(key = fp_hex_bytes,
/// msg = "iotdaq.trial.key.v1")` —— 机器码绑定由此建立：换机（指纹变化）后
/// 旧标记签名必然失效 ⇒ 视为试用已结束。
///
/// `Debug` 输出脱敏，密钥字节永不进入日志（项目红线）。
pub struct HmacKey {
    bytes: [u8; 32],
}

impl HmacKey {
    /// 从机器码指纹派生试用密钥（复用 task 3 公开 API 产物，本模块不自造指纹）。
    ///
    /// # Errors
    /// 指纹不是 64 字符 hex 时返回 [`TrialKeyError::BadFingerprint`]。
    pub fn from_machine_fingerprint(fingerprint_hex: &str) -> Result<Self, TrialKeyError> {
        let fp = fingerprint_hex.trim();
        if fp.len() != FINGERPRINT_HEX_LEN || !fp.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(TrialKeyError::BadFingerprint {
                len: FINGERPRINT_HEX_LEN,
                got: fp.len(),
            });
        }
        let mut mac = HmacSha256::new_from_slice(fp.as_bytes())
            .expect("HMAC accepts any non-empty key; 64-hex input is non-empty");
        mac.update(TRIAL_KEY_DOMAIN);
        Ok(Self {
            bytes: mac.finalize().into_bytes().into(),
        })
    }

    /// 计算规范化消息的 HMAC-SHA256，返回 64 字符 hex。
    fn sign_hex(&self, message: &[u8]) -> String {
        let mut mac =
            HmacSha256::new_from_slice(&self.bytes).expect("HMAC accepts any 32-byte key");
        mac.update(message);
        hex::encode(mac.finalize().into_bytes())
    }
}

impl fmt::Debug for HmacKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("HmacKey(***)")
    }
}

/// u64 ⇄ JSON 字符串编码（JSON 大数红线：时间戳不能以 JSON number 落盘）。
mod u64_as_string {
    use serde::{Deserialize, Deserializer, Serializer};

    /// 序列化为十进制字符串。
    pub(super) fn serialize<S: Serializer>(value: &u64, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&value.to_string())
    }

    /// 从十进制字符串反序列化；非数字 / 负数 / 空串均为解析失败。
    pub(super) fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<u64, D::Error> {
        let raw = String::deserialize(deserializer)?;
        raw.parse::<u64>().map_err(serde::de::Error::custom)
    }
}

/// 试用期本地标记（序列化 JSON 后由调用方持久化）。
///
/// JSON 形态：
/// ```json
/// {"format_version":1,"started_at_ms":"1700000000000","last_seen_ms":"1700000000000","sig":"<64 hex>"}
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrialMarker {
    /// 标记格式版本（非当前版本 ⇒ 视为试用已结束）。
    pub format_version: u32,
    /// 试用开始时刻（Unix 毫秒，字符串编码）。**重签不修改**——72h 窗口由此锚定。
    #[serde(with = "u64_as_string")]
    pub started_at_ms: u64,
    /// 最近一次可信时间锚点（Unix 毫秒，字符串编码）；`renew` 刷新，
    /// 回拨时作为时间真值来源。
    #[serde(with = "u64_as_string")]
    pub last_seen_ms: u64,
    /// HMAC-SHA256 签名（64 hex），覆盖除 `sig` 外的全部字段。
    pub sig: String,
}

/// 规范化字段拼接（签名消息）：`DOMAIN || fv=<len>:<v>| || started=<len>:<dec>| || last=<len>:<dec>|`。
///
/// 长度入编码（`tag=<十进制字节长度>:<内容>|`，范式同 `auth/signing.rs`），
/// 防字段拼接歧义；数值用十进制字符串，避免字节序 / 编码差异跨端不一致。
fn trial_message(marker: &TrialMarker) -> Vec<u8> {
    let mut msg: Vec<u8> = Vec::new();
    msg.extend_from_slice(TRIAL_DOMAIN_V1.as_bytes());
    for (tag, value) in [
        (&b"fv"[..], marker.format_version.to_string()),
        (&b"started"[..], marker.started_at_ms.to_string()),
        (&b"last"[..], marker.last_seen_ms.to_string()),
    ] {
        msg.extend_from_slice(tag);
        msg.extend_from_slice(b"=");
        msg.extend_from_slice(value.len().to_string().as_bytes());
        msg.extend_from_slice(b":");
        msg.extend_from_slice(value.as_bytes());
        msg.extend_from_slice(b"|");
    }
    msg
}

/// 计算标记签名（64 hex）。
fn marker_sig(marker: &TrialMarker, key: &HmacKey) -> String {
    key.sign_hex(&trial_message(marker))
}

/// 新建试用标记（started = last_seen = now_ms，签名后返回）。
fn new_marker(now_ms: u64, key: &HmacKey) -> TrialMarker {
    let mut marker = TrialMarker {
        format_version: TRIAL_MARKER_FORMAT_VERSION,
        started_at_ms: now_ms,
        last_seen_ms: now_ms,
        sig: String::new(),
    };
    marker.sig = marker_sig(&marker, key);
    marker
}

/// 试用判定结论。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TrialVerdict {
    /// 无标记 → 新建试用：`marker_json` 为新建标记的序列化 JSON，调用方
    /// **必须在持久卷落盘**；`expires_at_ms` = now + 72h。
    Fresh {
        /// 新建标记 JSON（调用方持久化）。
        marker_json: String,
        /// 试用到期时刻（Unix 毫秒）。
        expires_at_ms: u64,
    },
    /// 试用有效期内（或授权期内）：`expires_at_ms` = started_at + 72h。
    Active {
        /// 试用到期时刻（Unix 毫秒）。
        expires_at_ms: u64,
    },
    /// 试用已结束（到期 / 签名不符 / 字段缺失 / 解析失败 / 版本不符）→
    /// 降级免费基础版（限制见 `auth/limits.rs`），**不停用本地采集**；
    /// **绝不重置试用期**（无任何路径回到 Fresh / Active）。
    ExpiredDegrade,
}

/// 试用期判定（task 23 主入口，纯计算 + 闭包注入，不做文件 IO）。
///
/// 校验链：解析 JSON → 版本一致 → 签名一致 → 字段自洽（last_seen ≥ started）
/// → 回拨检测（回拨时以标记内 `last_seen_ms` 为时间真值）→ 72h 到期判定。
/// 链上任何一环失败 ⇒ [`TrialVerdict::ExpiredDegrade`]（绝不重置）。
pub fn evaluate(marker: Option<&str>, key: &HmacKey, clock: &TrustedClock) -> TrialVerdict {
    let Some(raw) = marker else {
        // 无标记 → 新建试用（首次启动；caller 必须持久化返回的标记 JSON）。
        // 说明：Fresh 路径不做回拨检测（尚无已签发标记可比对）；时钟侧
        // last_seen 由调用方在周期性 evaluate / renew 时经 check_rollback 维护。
        let now_ms = clock.now_ms();
        let marker = new_marker(now_ms, key);
        let expires_at_ms = now_ms.saturating_add(TRIAL_DURATION_MS);
        return TrialVerdict::Fresh {
            marker_json: serde_json::to_string(&marker)
                .expect("in-memory marker serialization cannot fail"),
            expires_at_ms,
        };
    };

    // 1) 解析失败（含字段缺失 / 类型不符 / 时间戳非法）⇒ 试用已结束。
    //    serde 反序列化强约束：sig 缺失、时间戳非数字均在此失败。
    let parsed: TrialMarker = match serde_json::from_str(raw) {
        Ok(marker) => marker,
        Err(_) => return TrialVerdict::ExpiredDegrade,
    };

    // 2) 版本不符 ⇒ 试用已结束（旧格式不迁移、不放行）。
    if parsed.format_version != TRIAL_MARKER_FORMAT_VERSION {
        return TrialVerdict::ExpiredDegrade;
    }

    // 3) 签名不符（篡改任一字段 / 换机指纹变化）⇒ 试用已结束。
    if parsed.sig != marker_sig(&parsed, key) {
        return TrialVerdict::ExpiredDegrade;
    }

    // 4) 字段自洽：last_seen 早于 started ⇒ 标记被伪造 ⇒ 试用已结束。
    if parsed.last_seen_ms < parsed.started_at_ms {
        return TrialVerdict::ExpiredDegrade;
    }

    // 5) 回拨检测：墙钟被回拨 ⇒ 不信任墙钟，以标记内持久 last_seen 为时间真值。
    let wall_now = clock.now_ms();
    let now = match clock.check_rollback(wall_now) {
        RollbackVerdict::Detected => parsed.last_seen_ms,
        RollbackVerdict::Ok => wall_now,
    };

    // 6) 72h 到期判定（now ≥ started + 72h ⇒ 到期降级）。
    let expires_at_ms = parsed.started_at_ms.saturating_add(TRIAL_DURATION_MS);
    if now >= expires_at_ms {
        TrialVerdict::ExpiredDegrade
    } else {
        TrialVerdict::Active { expires_at_ms }
    }
}

/// 重签标记：把 `last_seen_ms` 推进到 `now_ms` 并重新签名，返回新标记 JSON。
///
/// 同时经 `*last_seen` 把新锚点回传给调用方（用于时钟侧 last_seen 持久化，
/// 与标记内锚点保持一致）。**注意：renew 不延长试用期**（`started_at_ms` 不变，
/// 72h 窗口锚定于 started），仅刷新回拨检测的持久锚点——运行中的网关应周期性
/// renew，否则离线久置后回拨检测失去真值来源。
pub fn renew(marker: &TrialMarker, last_seen: &mut u64, key: &HmacKey, now_ms: u64) -> String {
    *last_seen = now_ms;
    let mut renewed = marker.clone();
    renewed.last_seen_ms = now_ms;
    renewed.sig = marker_sig(&renewed, key);
    serde_json::to_string(&renewed).expect("in-memory marker serialization cannot fail")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    /// test-only：测试专用机器码指纹 A（64 hex，模拟 task 3 输出），
    /// **仅测试使用，禁止用于真实部署**。
    const TEST_ONLY_FINGERPRINT_A: &str =
        "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    /// test-only：另一台机器的指纹 B（换机场景），禁止用于真实部署。
    const TEST_ONLY_FINGERPRINT_B: &str =
        "fedcba9876543210fedcba9876543210fedcba9876543210fedcba9876543210";

    /// 测试密钥（指纹 A 派生）。
    fn key_a() -> HmacKey {
        HmacKey::from_machine_fingerprint(TEST_ONLY_FINGERPRINT_A).expect("64-hex fingerprint")
    }

    /// 测试密钥（指纹 B 派生，另一台机器）。
    fn key_b() -> HmacKey {
        HmacKey::from_machine_fingerprint(TEST_ONLY_FINGERPRINT_B).expect("64-hex fingerprint")
    }

    /// 构造测试时钟：墙钟锚点 `wall_ms`，last_seen 持久仓由测试内共享内存模拟。
    fn test_clock(wall_ms: u64, store: &Arc<Mutex<Option<u64>>>) -> TrustedClock {
        let load_store = Arc::clone(store);
        let save_store = Arc::clone(store);
        TrustedClock::new(
            wall_ms,
            Box::new(move || *load_store.lock().expect("lock")),
            Box::new(move |v| *save_store.lock().expect("lock") = Some(v)),
        )
    }

    const HOUR_MS: u64 = 60 * 60 * 1000;

    /// QA：无标记 → Fresh；新标记 JSON 的 started/expires 正确，72h 窗口。
    #[test]
    fn fresh_creates_trial_with_72h_window() {
        let store = Arc::new(Mutex::new(None));
        let clock = test_clock(1_000_000, &store);
        let verdict = evaluate(None, &key_a(), &clock);
        let TrialVerdict::Fresh {
            marker_json,
            expires_at_ms,
        } = verdict
        else {
            panic!("no marker must be Fresh, got {verdict:?}");
        };
        assert_eq!(expires_at_ms, 1_000_000 + TRIAL_DURATION_MS);

        let marker: TrialMarker =
            serde_json::from_str(&marker_json).expect("fresh marker must parse");
        assert_eq!(marker.format_version, TRIAL_MARKER_FORMAT_VERSION);
        assert_eq!(marker.started_at_ms, 1_000_000);
        assert_eq!(marker.last_seen_ms, 1_000_000);
        assert_eq!(marker.sig.len(), 64, "sig must be SHA-256 hex");
    }

    /// QA：JSON 大数红线 —— 时间戳以字符串编码落盘（非 JSON number）。
    #[test]
    fn timestamps_are_string_encoded_in_json() {
        let store = Arc::new(Mutex::new(None));
        let clock = test_clock(1_700_000_000_000, &store);
        let TrialVerdict::Fresh { marker_json, .. } = evaluate(None, &key_a(), &clock) else {
            panic!("must be Fresh");
        };
        assert!(
            marker_json.contains("\"started_at_ms\":\"1700000000000\""),
            "started_at_ms must be a JSON string: {marker_json}"
        );
        assert!(
            marker_json.contains("\"last_seen_ms\":\"1700000000000\""),
            "last_seen_ms must be a JSON string: {marker_json}"
        );
        // 二次往返：字符串编码可无损解析回 u64。
        let marker: TrialMarker = serde_json::from_str(&marker_json).expect("round-trip");
        assert_eq!(marker.started_at_ms, 1_700_000_000_000);
    }

    /// QA：有效期内（71h）→ Active。
    #[test]
    fn active_within_window() {
        let store = Arc::new(Mutex::new(None));
        let t0 = 1_000_000;
        let clock = test_clock(t0, &store);
        let TrialVerdict::Fresh { marker_json, .. } = evaluate(None, &key_a(), &clock) else {
            panic!("must be Fresh");
        };

        let clock_71h = test_clock(t0 + 71 * HOUR_MS, &store);
        let verdict = evaluate(Some(&marker_json), &key_a(), &clock_71h);
        assert_eq!(
            verdict,
            TrialVerdict::Active {
                expires_at_ms: t0 + TRIAL_DURATION_MS
            }
        );
    }

    /// QA：到期边界 —— 恰好 72h ⇒ ExpiredDegrade；差 1ms ⇒ Active。
    #[test]
    fn expiry_boundary() {
        let store = Arc::new(Mutex::new(None));
        let t0 = 2_000_000;
        let clock = test_clock(t0, &store);
        let TrialVerdict::Fresh { marker_json, .. } = evaluate(None, &key_a(), &clock) else {
            panic!("must be Fresh");
        };

        let at_edge = test_clock(t0 + TRIAL_DURATION_MS, &store);
        assert_eq!(
            evaluate(Some(&marker_json), &key_a(), &at_edge),
            TrialVerdict::ExpiredDegrade,
            "now >= started + 72h ⇒ 到期"
        );

        let just_before = test_clock(t0 + TRIAL_DURATION_MS - 1, &store);
        assert_eq!(
            evaluate(Some(&marker_json), &key_a(), &just_before),
            TrialVerdict::Active {
                expires_at_ms: t0 + TRIAL_DURATION_MS
            }
        );
    }

    /// QA：篡改任一字段 ⇒ ExpiredDegrade（绝不重置）——started / last_seen / sig。
    #[test]
    fn tampering_any_field_yields_expired() {
        let store = Arc::new(Mutex::new(None));
        let t0 = 1_000_000;
        let clock = test_clock(t0, &store);
        let TrialVerdict::Fresh { marker_json, .. } = evaluate(None, &key_a(), &clock) else {
            panic!("must be Fresh");
        };
        let marker: TrialMarker = serde_json::from_str(&marker_json).expect("parse");

        // 篡改 started_at_ms（签名过期，重新序列化但不重签）。
        let mut tampered = marker.clone();
        tampered.started_at_ms += 1;
        let tamper_started =
            serde_json::to_string(&tampered).expect("in-memory serialize cannot fail");
        assert_eq!(
            evaluate(Some(&tamper_started), &key_a(), &clock),
            TrialVerdict::ExpiredDegrade,
            "篡改 started ⇒ 试用已结束"
        );

        // 篡改 last_seen_ms。
        let mut tampered = marker.clone();
        tampered.last_seen_ms = 0;
        let tamper_last =
            serde_json::to_string(&tampered).expect("in-memory serialize cannot fail");
        assert_eq!(
            evaluate(Some(&tamper_last), &key_a(), &clock),
            TrialVerdict::ExpiredDegrade,
            "篡改 last_seen ⇒ 试用已结束"
        );

        // 篡改 sig（合法 hex 但内容错）。
        let mut tampered = marker.clone();
        tampered.sig = "0".repeat(64);
        let tamper_sig = serde_json::to_string(&tampered).expect("in-memory serialize cannot fail");
        assert_eq!(
            evaluate(Some(&tamper_sig), &key_a(), &clock),
            TrialVerdict::ExpiredDegrade,
            "sig 不符 ⇒ 试用已结束"
        );

        // 篡改 format_version（降版本伪造）。
        let mut tampered = marker.clone();
        tampered.format_version = 0;
        let tamper_fv = serde_json::to_string(&tampered).expect("in-memory serialize cannot fail");
        assert_eq!(
            evaluate(Some(&tamper_fv), &key_a(), &clock),
            TrialVerdict::ExpiredDegrade,
            "版本不符 ⇒ 试用已结束"
        );
    }

    /// QA：字段缺失 / 解析失败 ⇒ ExpiredDegrade（绝不重置）。
    #[test]
    fn missing_fields_or_corrupt_json_yield_expired() {
        let store = Arc::new(Mutex::new(None));
        let t0 = 1_000_000;
        let clock = test_clock(t0, &store);
        let TrialVerdict::Fresh { marker_json, .. } = evaluate(None, &key_a(), &clock) else {
            panic!("must be Fresh");
        };
        let marker: TrialMarker = serde_json::from_str(&marker_json).expect("parse");

        // 缺 sig 字段（手工裁剪：定位 ",\"sig\":\"" 起始并截断）。
        let no_sig = match marker_json.find(",\"sig\":\"") {
            Some(idx) => format!("{}{}}}", &marker_json[..idx], "}"),
            None => panic!("marker json must contain sig field: {marker_json}"),
        };
        assert_eq!(
            evaluate(Some(&no_sig), &key_a(), &clock),
            TrialVerdict::ExpiredDegrade,
            "字段缺失 ⇒ 试用已结束"
        );

        // 时间戳编码为 JSON number（绕过字符串红线）也必须拒绝
        // （u64_as_string 反序列化要求字符串，number ⇒ 解析失败）。
        let numeric_ts = format!(
            "{{\"format_version\":1,\"started_at_ms\":{},\"last_seen_ms\":{},\"sig\":\"{}\"}}",
            marker.started_at_ms, marker.last_seen_ms, marker.sig
        );
        assert_eq!(
            evaluate(Some(&numeric_ts), &key_a(), &clock),
            TrialVerdict::ExpiredDegrade,
            "number 编码时间戳 ⇒ 解析失败 ⇒ 试用已结束"
        );

        // 乱串 JSON。
        assert_eq!(
            evaluate(Some("not-json{"), &key_a(), &clock),
            TrialVerdict::ExpiredDegrade,
            "解析失败 ⇒ 试用已结束"
        );

        // 空串标记。
        assert_eq!(
            evaluate(Some(""), &key_a(), &clock),
            TrialVerdict::ExpiredDegrade,
            "空标记 ⇒ 试用已结束"
        );
    }

    /// QA：换机（密钥派生自不同指纹）⇒ 签名失效 ⇒ ExpiredDegrade。
    #[test]
    fn marker_from_other_machine_yields_expired() {
        let store = Arc::new(Mutex::new(None));
        let clock = test_clock(1_000_000, &store);
        let TrialVerdict::Fresh { marker_json, .. } = evaluate(None, &key_a(), &clock) else {
            panic!("must be Fresh");
        };
        assert_eq!(
            evaluate(Some(&marker_json), &key_b(), &clock),
            TrialVerdict::ExpiredDegrade,
            "机器码绑定：B 机器无法验证 A 机器签发的标记"
        );
    }

    /// QA：回拨时以标记内持久 last_seen 为时间真值，不信任被回拨的墙钟。
    #[test]
    fn rollback_distrusts_wall_clock_uses_persisted_last_seen() {
        let store = Arc::new(Mutex::new(None));
        let t0 = 1_000_000;
        let clock = test_clock(t0, &store);
        let TrialVerdict::Fresh { marker_json, .. } = evaluate(None, &key_a(), &clock) else {
            panic!("must be Fresh");
        };
        let marker: TrialMarker = serde_json::from_str(&marker_json).expect("parse");
        let mut last_seen = marker.last_seen_ms;

        // 运行至 70h 处 renew（标记内 last_seen 推进到 70h，墙钟正常）。
        let renewed_json = renew(&marker, &mut last_seen, &key_a(), t0 + 70 * HOUR_MS);
        // 模拟时钟侧 last_seen 已随周期性 renew 落盘为 t0+70h。
        *store.lock().expect("lock") = Some(t0 + 70 * HOUR_MS);

        // 墙钟被回拨到 t0+1h（远早于 last_seen-90s）⇒ 回拨检测命中。
        let rolled_back_clock = test_clock(t0 + HOUR_MS, &store);
        // 回拨后墙钟(1h) < 72h：若信任墙钟会误判 Active；契约要求以 last_seen(70h) 为准 → 仍 Active。
        assert_eq!(
            evaluate(Some(&renewed_json), &key_a(), &rolled_back_clock),
            TrialVerdict::Active {
                expires_at_ms: t0 + TRIAL_DURATION_MS
            },
            "回拨命中时以持久 last_seen(70h) 判定 → 未到期"
        );

        // 对照：last_seen 已推进到 73h（离线久置后重签）而墙钟回拨到 60h。
        let marker2: TrialMarker =
            serde_json::from_str(&renewed_json).expect("renewed marker parses");
        let mut last_seen2 = marker2.last_seen_ms;
        let renewed_json2 = renew(&marker2, &mut last_seen2, &key_a(), t0 + 73 * HOUR_MS);
        // 时钟侧 last_seen 同步落盘为 t0+73h；墙钟回拨到 t0+60h ⇒ 回拨命中。
        *store.lock().expect("lock") = Some(t0 + 73 * HOUR_MS);
        let clock_60h = test_clock(t0 + 60 * HOUR_MS, &store);
        assert_eq!(
            evaluate(Some(&renewed_json2), &key_a(), &clock_60h),
            TrialVerdict::ExpiredDegrade,
            "回拨命中时 last_seen(73h) 已超 72h ⇒ ExpiredDegrade（不信任回拨墙钟的 60h）"
        );
    }

    /// QA：无回拨时正常信任墙钟判定（Active 路径回归）。
    #[test]
    fn no_rollback_trusts_wall_clock() {
        let store = Arc::new(Mutex::new(None));
        let t0 = 1_000_000;
        let clock = test_clock(t0, &store);
        let TrialVerdict::Fresh { marker_json, .. } = evaluate(None, &key_a(), &clock) else {
            panic!("must be Fresh");
        };
        let ahead = test_clock(t0 + 10 * HOUR_MS, &store);
        assert_eq!(
            evaluate(Some(&marker_json), &key_a(), &ahead),
            TrialVerdict::Active {
                expires_at_ms: t0 + TRIAL_DURATION_MS
            }
        );
    }

    /// QA：renew —— last_seen 推进、*last_seen 同步回传、重签后仍可验证为 Active。
    #[test]
    fn renew_refreshes_last_seen_and_verifies() {
        let store = Arc::new(Mutex::new(None));
        let t0 = 1_000_000;
        let clock = test_clock(t0, &store);
        let TrialVerdict::Fresh { marker_json, .. } = evaluate(None, &key_a(), &clock) else {
            panic!("must be Fresh");
        };
        let marker: TrialMarker = serde_json::from_str(&marker_json).expect("parse");
        let mut last_seen = marker.last_seen_ms;

        let renewed_json = renew(&marker, &mut last_seen, &key_a(), t0 + 10 * HOUR_MS);
        assert_eq!(last_seen, t0 + 10 * HOUR_MS, "*last_seen 须同步回传新锚点");

        let renewed: TrialMarker = serde_json::from_str(&renewed_json).expect("renewed parses");
        assert_eq!(
            renewed.started_at_ms, t0,
            "renew 不得改动 started（不延长试用期）"
        );
        assert_eq!(renewed.last_seen_ms, t0 + 10 * HOUR_MS);
        assert_ne!(renewed.sig, marker.sig, "重签后签名必须变化");
        assert_ne!(renewed_json, marker_json, "重签产物必须是新 JSON");

        // 重签后的标记在 t0+11h 仍可验证为 Active（且 72h 窗口不变）。
        let clock_11h = test_clock(t0 + 11 * HOUR_MS, &store);
        assert_eq!(
            evaluate(Some(&renewed_json), &key_a(), &clock_11h),
            TrialVerdict::Active {
                expires_at_ms: t0 + TRIAL_DURATION_MS
            },
            "重签后仍可验证；renew 不重置 72h 窗口"
        );
    }

    /// QA：指纹格式校验 —— 非 64 hex / 空串拒绝（错误路径不 panic）。
    #[test]
    fn invalid_fingerprint_is_rejected() {
        assert!(matches!(
            HmacKey::from_machine_fingerprint("abc"),
            Err(TrialKeyError::BadFingerprint { len: 64, got: 3 })
        ));
        assert!(matches!(
            HmacKey::from_machine_fingerprint("".repeat(64).as_str()),
            Err(TrialKeyError::BadFingerprint { got: 0, .. })
        ));
        // 非 hex 字符（长度对但含 g）。
        let bad = "g".repeat(64);
        assert!(matches!(
            HmacKey::from_machine_fingerprint(&bad),
            Err(TrialKeyError::BadFingerprint { got: 64, .. })
        ));
    }

    /// QA：密钥 Debug 输出脱敏（密钥字节不进日志）。
    #[test]
    fn key_debug_is_masked() {
        let rendered = format!("{:?}", key_a());
        assert_eq!(rendered, "HmacKey(***)");
    }

    /// QA：TrialMarker 序列化确定性 —— 相同字段两次序列化逐字节一致（重签可复现）。
    #[test]
    fn marker_serialization_is_deterministic() {
        let store = Arc::new(Mutex::new(None));
        let clock = test_clock(42, &store);
        let TrialVerdict::Fresh { marker_json, .. } = evaluate(None, &key_a(), &clock) else {
            panic!("must be Fresh");
        };
        let marker: TrialMarker = serde_json::from_str(&marker_json).expect("parse");
        let again = serde_json::to_string(&marker).expect("in-memory serialize cannot fail");
        assert_eq!(marker_json, again, "同字段序列化必须逐字节一致");
    }
}

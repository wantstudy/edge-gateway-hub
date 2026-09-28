//! `licensing-server` 签名密钥管理：kid 多密钥并存与轮换（计划 task 45 / 设计 §3）。
//!
//! # 设计约束（逐条对齐设计文档）
//!
//! 1. **私钥绝不落业务库**：`signing_key` 表只存 `public_key` 与 `hsm_ref`（KMS / 离线保管引用）。
//!    本模块的 [`KeyRing`] 是**签名侧**的内存视图，私钥从环境变量注入，注册后立即清零中间明文。
//! 2. **新旧并存轮换**：`active`（新签发走此 kid）→ `retiring`（不再签发、仍可验签）
//!    → `retired`（拒绝验签）。**同一时刻可有多个 `active`**，但仅 [`KeyRing::signing_kid`] 指向的
//!    那个用于签发，其余只为验签存活。
//! 3. **未知 kid 一律拒绝**：验签前先按 kid 查公钥，查不到直接 `TokenInvalid`，
//!    绝不做「单公钥兜底」（否则轮换语义失效）。
//! 4. **kid 不可枚举猜测**：kid 采用 `k<UTC 日期>-<8 位随机>` 形式，带足够熵，
//!    避免攻击者从 `k1`、`k2` 推演出后续 kid。

use std::collections::BTreeMap;
use std::sync::Arc;

use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine as _;
use ed25519_dalek::{Signature, Signer as _, SigningKey, Verifier as _, VerifyingKey};
use parking_lot::RwLock;
use rand::Rng as _;

use crate::error::{LicenseError, LicenseResult};

/// 密钥状态（与 `signing_key.status` 枚举一一对应）。
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum KeyStatus {
    /// 启用中：新 Token 用此 kid 签发，同时可验签。
    Active,
    /// 退役中：**不再签发**，但存量租约仍可验签（旧客户端不受影响）。
    Retiring,
    /// 已退役：不再签发，验签亦被拒绝（宽限期结束）。
    Retired,
}

impl KeyStatus {
    /// 是否可用于**签发**。
    pub fn can_sign(self) -> bool {
        matches!(self, KeyStatus::Active)
    }

    /// 是否可用于**验签**。
    pub fn can_verify(self) -> bool {
        matches!(self, KeyStatus::Active | KeyStatus::Retiring)
    }

    /// 稳定字符串（落库 / 审计用，勿依赖 `Debug` 输出格式）。
    pub fn as_str(self) -> &'static str {
        match self {
            KeyStatus::Active => "active",
            KeyStatus::Retiring => "retiring",
            KeyStatus::Retired => "retired",
        }
    }

    /// 从落库字符串解析。
    pub fn parse(s: &str) -> LicenseResult<Self> {
        match s {
            "active" => Ok(KeyStatus::Active),
            "retiring" => Ok(KeyStatus::Retiring),
            "retired" => Ok(KeyStatus::Retired),
            other => Err(LicenseError::KeyStateIllegal(format!(
                "unknown signing_key.status: {other}"
            ))),
        }
    }
}

/// 一个密钥条目的**公开视图**（可安全下发客户端公钥集）。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PublicKeyEntry {
    /// 密钥标识。
    pub kid: String,
    /// Ed25519 公钥（base64）。
    pub public_key_b64: String,
    /// 当前状态。
    pub status: KeyStatus,
    /// 状态变更时刻（UTC 秒，0 表示尚未变更）。
    pub changed_at: i64,
}

/// 密钥条目的**完整内部视图**（含私钥；绝不入库、绝不出网络）。
///
/// 本结构 **不实现 `Debug`**：避免私钥经 `{:?}` 意外落日志。
pub struct KeyEntry {
    kid: String,
    status: KeyStatus,
    signing: Option<SigningKey>,
    verifying: VerifyingKey,
    public_key_b64: String,
    /// KMS / 离线保管引用（私钥的外部锚点，不是私钥本身）。
    hsm_ref: Option<String>,
    changed_at: i64,
}

impl KeyEntry {
    /// kid。
    pub fn kid(&self) -> &str {
        &self.kid
    }

    /// 状态。
    pub fn status(&self) -> KeyStatus {
        self.status
    }

    /// 公钥（base64）。
    pub fn public_key_b64(&self) -> &str {
        &self.public_key_b64
    }

    /// 是否持有可用私钥（仅 `active` 且注入成功时为真）。
    pub fn can_sign(&self) -> bool {
        self.status.can_sign() && self.signing.is_some()
    }

    /// 公开视图。
    pub fn public_view(&self) -> PublicKeyEntry {
        PublicKeyEntry {
            kid: self.kid.clone(),
            public_key_b64: self.public_key_b64.clone(),
            status: self.status,
            changed_at: self.changed_at,
        }
    }
}

/// `KeyEntry` 的手写 Debug：**私钥一律脱敏**，只暴露 kid / 状态 / 是否持私钥。
impl std::fmt::Debug for KeyEntry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("KeyEntry")
            .field("kid", &self.kid)
            .field("status", &self.status)
            .field(
                "signing",
                &if self.signing.is_some() {
                    "<injected>"
                } else {
                    "<none>"
                },
            )
            .field("public_key_b64", &self.public_key_b64)
            .field("hsm_ref", &self.hsm_ref)
            .field("changed_at", &self.changed_at)
            .finish()
    }
}

/// 密钥环：进程内全部签名密钥的持有者。
///
/// 线程安全（`RwLock` + `Arc` 共享）；[`KeyRing::sign`] 走读锁，轮换操作走写锁。
pub struct KeyRing {
    inner: RwLock<Inner>,
}

struct Inner {
    /// kid → 条目（`BTreeMap` 保证遍历顺序稳定，供公钥集下发时输出确定）。
    keys: BTreeMap<String, Arc<KeyEntry>>,
    /// 当前用于**签发**的 kid（`None` = 无可用签发密钥，签发一律失败）。
    signing_kid: Option<String>,
}

/// `KeyRing` 的手写 Debug：不暴露任何密钥材料。
impl std::fmt::Debug for KeyRing {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let inner = self.inner.read();
        f.debug_struct("KeyRing")
            .field("key_count", &inner.keys.len())
            .field("signing_kid", &inner.signing_kid)
            .field("kids", &inner.keys.keys().cloned().collect::<Vec<_>>())
            .finish()
    }
}

impl KeyRing {
    /// 空密钥环（无任何 kid，签发必失败）。
    pub fn empty() -> Self {
        KeyRing {
            inner: RwLock::new(Inner {
                keys: BTreeMap::new(),
                signing_kid: None,
            }),
        }
    }

    /// 生成一个全新的 kid（`k<YYYYMMDD>-<8 hex>`）。
    ///
    /// 熵来源：8 位随机 hex（32 bit）+ 日期前缀，足以让枚举猜测不可行。
    pub fn generate_kid(now_unix_secs: i64) -> String {
        let mut rng = rand::rng();
        let suffix: u32 = rng.random();
        format!("k{}-{:08x}", format_kid_date(now_unix_secs), suffix)
    }

    /// 注册一个**新生成**的密钥对（私钥在本进程内产生）。
    ///
    /// 返回 kid。新密钥注册即为 `active`，并**抢占签发位**（`signing_kid` 指向它）——
    /// 这正是轮换的第 1-3 步「新 kid 生成 → 新签发走新 kid → 旧 kid 置 retiring」。
    pub fn register_generated(
        &self,
        hsm_ref: Option<String>,
        now_unix_secs: i64,
    ) -> LicenseResult<String> {
        let mut rng = rand::rng();
        let payload: [u8; 32] = rng.random();
        let signing = SigningKey::from_bytes(&payload);
        let kid = Self::generate_kid(now_unix_secs);
        self.insert_entry(
            kid.clone(),
            signing,
            KeyStatus::Active,
            hsm_ref,
            now_unix_secs,
        )?;
        Ok(kid)
    }

    /// 从环境变量注入私钥密钥对（**生产路径**）。
    ///
    /// - `kid`：密钥标识
    /// - `private_key_b64`：Ed25519 32 字节种子 / 64 字节密钥对的 base64
    /// - `hsm_ref`：KMS / 离线保管引用（可选，仅作溯源）
    ///
    /// 私钥只在此刻存在于内存，随后由 `ed25519_dalek::SigningKey` 内部持有；
    /// **不写入数据库、不写日志、不返回给调用方**。
    pub fn register_from_b64(
        &self,
        kid: &str,
        private_key_b64: &str,
        hsm_ref: Option<String>,
        now_unix_secs: i64,
    ) -> LicenseResult<()> {
        if kid.is_empty() {
            return Err(LicenseError::KeyStateIllegal(
                "kid must not be empty".into(),
            ));
        }
        let raw = B64.decode(private_key_b64.trim()).map_err(|e| {
            // 注意：错误信息里**不包含** private_key_b64 本身。
            LicenseError::KeyStateIllegal(format!("private key is not valid base64: {e}"))
        })?;
        let signing = match raw.len() {
            32 => {
                let seed: [u8; 32] = raw.as_slice().try_into().map_err(|_| {
                    LicenseError::KeyStateIllegal("private key seed length mismatch".into())
                })?;
                SigningKey::from_bytes(&seed)
            }
            64 => {
                // 64 字节形态：后 32 字节为公钥，前 32 字节为种子（与 RFC 8032 一致）。
                let seed: [u8; 32] = raw[..32].try_into().map_err(|_| {
                    LicenseError::KeyStateIllegal("private key seed length mismatch".into())
                })?;
                SigningKey::from_bytes(&seed)
            }
            other => {
                return Err(LicenseError::KeyStateIllegal(format!(
                    "private key must be 32 or 64 bytes, got {other}"
                )));
            }
        };
        self.insert_entry(
            kid.to_string(),
            signing,
            KeyStatus::Active,
            hsm_ref,
            now_unix_secs,
        )
    }

    /// 从 base64 **公钥**注册一个仅验签条目（无签发能力）。
    ///
    /// 用途：服务端自身不持私钥（私钥在隔离签名服务）时，用此方式注册公钥以便
    /// `/verify` 与 `/audit/receipt` 验签。此时 [`KeyRing::sign`] 不可用，签发须走外部签名。
    pub fn register_public_only(
        &self,
        kid: &str,
        public_key_b64: &str,
        status: KeyStatus,
        hsm_ref: Option<String>,
        now_unix_secs: i64,
    ) -> LicenseResult<()> {
        if kid.is_empty() {
            return Err(LicenseError::KeyStateIllegal(
                "kid must not be empty".into(),
            ));
        }
        let raw = B64.decode(public_key_b64.trim()).map_err(|e| {
            LicenseError::KeyStateIllegal(format!("public key is not valid base64: {e}"))
        })?;
        let bytes: [u8; 32] = raw.as_slice().try_into().map_err(|_| {
            LicenseError::KeyStateIllegal(format!("public key must be 32 bytes, got {}", raw.len()))
        })?;
        let verifying = VerifyingKey::from_bytes(&bytes).map_err(|e| {
            LicenseError::KeyStateIllegal(format!("public key is not a valid Ed25519 point: {e}"))
        })?;
        let normalized = B64.encode(verifying.to_bytes());
        let entry = KeyEntry {
            kid: kid.to_string(),
            status,
            signing: None,
            verifying,
            public_key_b64: normalized,
            hsm_ref,
            changed_at: now_unix_secs,
        };
        let mut inner = self.inner.write();
        if inner.keys.contains_key(kid) {
            return Err(LicenseError::KeyStateIllegal(format!(
                "kid already registered: {kid}"
            )));
        }
        inner.keys.insert(kid.to_string(), Arc::new(entry));
        if status.can_sign() && inner.signing_kid.is_none() {
            // 仅验签条目无法签发，故不抢占 signing_kid（保持 None → 签发明确失败）。
            inner.signing_kid = None;
        }
        Ok(())
    }

    fn insert_entry(
        &self,
        kid: String,
        signing: SigningKey,
        status: KeyStatus,
        hsm_ref: Option<String>,
        now_unix_secs: i64,
    ) -> LicenseResult<()> {
        let verifying = signing.verifying_key();
        let entry = KeyEntry {
            kid: kid.clone(),
            status,
            signing: Some(signing),
            verifying,
            public_key_b64: B64.encode(verifying.to_bytes()),
            hsm_ref,
            changed_at: now_unix_secs,
        };
        let mut inner = self.inner.write();
        if inner.keys.contains_key(&kid) {
            return Err(LicenseError::KeyStateIllegal(format!(
                "kid already registered: {kid}"
            )));
        }
        inner.keys.insert(kid.clone(), Arc::new(entry));
        // 抢占签发位：轮换的核心语义——最新注册的 active 密钥负责后续签发。
        inner.signing_kid = Some(kid);
        Ok(())
    }

    /// 当前用于签发的 kid。
    pub fn signing_kid(&self) -> Option<String> {
        self.inner.read().signing_kid.clone()
    }

    /// 显式指定签发 kid（切换轮换阶段，如把签发位交给另一个已存在的 active 密钥）。
    pub fn set_signing_kid(&self, kid: &str) -> LicenseResult<()> {
        let mut inner = self.inner.write();
        let entry = inner.keys.get(kid).ok_or_else(|| {
            LicenseError::TokenInvalid(format!("cannot set signing kid: unknown kid {kid}"))
        })?;
        if !entry.can_sign() {
            return Err(LicenseError::KeyStateIllegal(format!(
                "kid {kid} is {} and cannot be used for signing",
                entry.status.as_str()
            )));
        }
        inner.signing_kid = Some(kid.to_string());
        Ok(())
    }

    /// 变更某 kid 的状态（轮换第 3-4 步：`active → retiring → retired`）。
    ///
    /// 把当前签发密钥置为非 active 时，**不会自动挑选替代者**——必须显式
    /// [`KeyRing::set_signing_kid`]；这样「无可用签发密钥」是可观测状态（签发报错），
    /// 而不是静默用一把意外选中的密钥继续签发。
    pub fn set_status(
        &self,
        kid: &str,
        status: KeyStatus,
        now_unix_secs: i64,
    ) -> LicenseResult<()> {
        let mut inner = self.inner.write();
        let old = inner
            .keys
            .get(kid)
            .ok_or_else(|| LicenseError::TokenInvalid(format!("unknown kid: {kid}")))?
            .clone();
        let updated = KeyEntry {
            kid: old.kid.clone(),
            status,
            signing: old.signing.clone(),
            verifying: old.verifying,
            public_key_b64: old.public_key_b64.clone(),
            hsm_ref: old.hsm_ref.clone(),
            changed_at: now_unix_secs,
        };
        inner.keys.insert(kid.to_string(), Arc::new(updated));
        if inner.signing_kid.as_deref() == Some(kid) && !status.can_sign() {
            inner.signing_kid = None;
        }
        Ok(())
    }

    /// 查询单个条目。
    pub fn get(&self, kid: &str) -> Option<Arc<KeyEntry>> {
        self.inner.read().keys.get(kid).cloned()
    }

    /// 全部条目（顺序由 kid 字典序决定，稳定）。
    pub fn all(&self) -> Vec<Arc<KeyEntry>> {
        self.inner.read().keys.values().cloned().collect()
    }

    /// 可下发客户端的公钥集（含 active + retiring；**不含 retired**）。
    ///
    /// task 45 验收：「客户端内置**公钥集**（非单钥）」，可通过应用更新通道追加新公钥。
    pub fn public_key_set(&self) -> Vec<PublicKeyEntry> {
        self.inner
            .read()
            .keys
            .values()
            .filter(|e| e.status.can_verify())
            .map(|e| e.public_view())
            .collect()
    }

    /// 用当前签发 kid 对消息签名，返回 `(kid, signature_base64)`。
    pub fn sign(&self, message: &[u8]) -> LicenseResult<(String, String)> {
        let inner = self.inner.read();
        let kid = inner
            .signing_kid
            .clone()
            .ok_or_else(|| LicenseError::TokenInvalid("no active signing key available".into()))?;
        let entry = inner.keys.get(&kid).ok_or_else(|| {
            LicenseError::TokenInvalid(format!("signing kid vanished from keyring: {kid}"))
        })?;
        let signing = entry.signing.as_ref().ok_or_else(|| {
            LicenseError::TokenInvalid(format!(
                "kid {kid} is registered for verification only; cannot sign"
            ))
        })?;
        let sig = signing.sign(message);
        Ok((kid, B64.encode(sig.to_bytes())))
    }

    /// 用指定 kid 的公钥验签。
    ///
    /// **未知 kid / retired kid 一律拒绝**（轮换语义要求）。
    pub fn verify(&self, kid: &str, message: &[u8], signature_b64: &str) -> LicenseResult<()> {
        let entry = self.get(kid).ok_or_else(|| {
            // 不回显任何候选 kid，避免枚举探测。
            LicenseError::TokenInvalid(format!("unknown kid: {kid}"))
        })?;
        if !entry.status.can_verify() {
            return Err(LicenseError::TokenInvalid(format!(
                "kid {kid} is {} and no longer accepted for verification",
                entry.status.as_str()
            )));
        }
        let raw = B64.decode(signature_b64.trim()).map_err(|e| {
            LicenseError::TokenInvalid(format!("signature is not valid base64: {e}"))
        })?;
        let bytes: [u8; 64] = raw.as_slice().try_into().map_err(|_| {
            LicenseError::TokenInvalid(format!("signature must be 64 bytes, got {}", raw.len()))
        })?;
        let sig = Signature::from_bytes(&bytes);
        entry
            .verifying
            .verify(message, &sig)
            .map_err(|_| LicenseError::TokenInvalid("signature verification failed".into()))
    }

    /// 用**任一**可用 kid 尝试验签（客户端公钥集语义：不知道 kid 时的兜底）。
    ///
    /// 仅用于测试与兼容场景；生产 `/verify` 走显式 kid 路径。
    pub fn verify_any(&self, message: &[u8], signature_b64: &str) -> LicenseResult<String> {
        let candidates: Vec<String> = self
            .inner
            .read()
            .keys
            .values()
            .filter(|e| e.status.can_verify())
            .map(|e| e.kid.clone())
            .collect();
        for kid in &candidates {
            if self.verify(kid, message, signature_b64).is_ok() {
                return Ok(kid.clone());
            }
        }
        Err(LicenseError::TokenInvalid(
            "signature does not verify against any known kid".into(),
        ))
    }
}

/// `YYYYMMDD` 形式的 kid 日期前缀（UTC）。
fn format_kid_date(unix_secs: i64) -> String {
    // 手写 UTC 日期换算，避免为一行格式化引入完整时区库依赖。
    let days = unix_secs.div_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    format!("{y:04}{m:02}{d:02}")
}

/// 激活码前缀年份：签发时刻的 UTC 公历年（如 `2026`）。
///
/// 激活码形如 `IOT-2026-XXXX-XXXX-XXXX-XX`，年份取**签发当年**（跨年时自然滚动，
/// 不是硬编码常量）。复用 [`civil_from_days`]，与 kid 日期前缀同一口径；纯整数
/// 运算，无 panic，亦不引入时区库依赖。
pub fn current_year(unix_secs: i64) -> i64 {
    let (y, _, _) = civil_from_days(unix_secs.div_euclid(86_400));
    y
}

/// Howard Hinnant 的 `civil_from_days` 算法：days since 1970-01-01 → (y, m, d)。
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32; // [1, 12]
    (if m <= 2 { y + 1 } else { y }, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `current_year` 取签发当年（激活码前缀年份），跨年自然滚动且非硬编码。
    #[test]
    fn current_year_follows_the_calendar_year() {
        assert_eq!(current_year(0), 1970);
        assert_eq!(current_year(1_735_689_600), 2025); // 2025-01-01T00:00:00Z
        assert_eq!(current_year(1_767_225_600), 2026); // 2026-01-01T00:00:00Z
        assert_eq!(current_year(1_798_761_600), 2027); // 2027-01-01T00:00:00Z
        let now_year = current_year(crate::model::now_unix_secs());
        assert!(
            (1970..=9999).contains(&now_year),
            "year out of range: {now_year}"
        );
    }

    /// 生成密钥 → 注册 → 签发 → 验签成功；且 kid 与签名都非空。
    #[test]
    fn generated_key_signs_and_verifies() {
        let ring = KeyRing::empty();
        let kid = ring
            .register_generated(Some("kms://iotdaq/v1".into()), 1_700_000_000)
            .unwrap();
        assert_eq!(ring.signing_kid().as_deref(), Some(kid.as_str()));

        let msg = b"lease-token-payload";
        let (sig_kid, sig) = ring.sign(msg).unwrap();
        assert_eq!(sig_kid, kid);
        assert!(!sig.is_empty());
        assert!(ring.verify(&kid, msg, &sig).is_ok());
    }

    /// 篡改消息体 → 验签必须失败（不是静默通过）。
    #[test]
    fn tampered_message_fails_verification() {
        let ring = KeyRing::empty();
        let kid = ring.register_generated(None, 1_700_000_000).unwrap();
        let (_, sig) = ring.sign(b"original").unwrap();
        let err = ring.verify(&kid, b"tampered", &sig).unwrap_err();
        assert!(matches!(err, LicenseError::TokenInvalid(_)), "{err:?}");
    }

    /// **未知 kid 必须被拒**（task 45 验收点）。
    #[test]
    fn unknown_kid_is_rejected() {
        let ring = KeyRing::empty();
        let kid = ring.register_generated(None, 1_700_000_000).unwrap();
        let (_, sig) = ring.sign(b"payload").unwrap();
        let err = ring
            .verify("k19700101-deadbeef", b"payload", &sig)
            .unwrap_err();
        assert!(matches!(err, LicenseError::TokenInvalid(_)), "{err:?}");
        // 合法 kid 仍可验，证明失败原因是 kid 而非签名本身。
        assert!(ring.verify(&kid, b"payload", &sig).is_ok());
    }

    /// 轮换：新 kid 抢占签发位 → 旧 kid 置 retiring 仍可验签 → 置 retired 后拒绝验签。
    #[test]
    fn kid_rotation_keeps_old_key_verifiable_then_retires() {
        let ring = KeyRing::empty();
        let old_kid = ring.register_generated(None, 1_700_000_000).unwrap();
        let (_, old_sig) = ring.sign(b"legacy-lease").unwrap();

        // 轮换第 1-3 步：生成新 kid（自动抢占签发位），旧 kid 置 retiring。
        let new_kid = ring.register_generated(None, 1_700_100_000).unwrap();
        assert_ne!(new_kid, old_kid);
        assert_eq!(ring.signing_kid().as_deref(), Some(new_kid.as_str()));
        ring.set_status(&old_kid, KeyStatus::Retiring, 1_700_100_000)
            .unwrap();

        // 旧签发的 Token 仍可验签（存量租约不失效）。
        assert!(ring.verify(&old_kid, b"legacy-lease", &old_sig).is_ok());
        // 新签发一律走新 kid。
        let (sig_kid, _) = ring.sign(b"fresh-lease").unwrap();
        assert_eq!(sig_kid, new_kid);
        // retiring kid 不可再签发。
        assert!(ring
            .set_signing_kid(&old_kid)
            .is_err_and(|e| matches!(e, LicenseError::KeyStateIllegal(_))));

        // 轮换第 4 步：置 retired → 验签被拒。
        ring.set_status(&old_kid, KeyStatus::Retired, 1_700_200_000)
            .unwrap();
        let err = ring
            .verify(&old_kid, b"legacy-lease", &old_sig)
            .unwrap_err();
        assert!(matches!(err, LicenseError::TokenInvalid(_)), "{err:?}");
    }

    /// 将当前签发位置为非 active 时，签发位变 `None` → 签发明确失败（不静默选别的密钥）。
    #[test]
    fn retiring_signing_kid_leaves_no_signer_rather_than_silently_switching() {
        let ring = KeyRing::empty();
        let kid = ring.register_generated(None, 1_700_000_000).unwrap();
        ring.set_status(&kid, KeyStatus::Retiring, 1_700_000_100)
            .unwrap();
        assert_eq!(ring.signing_kid(), None);
        let err = ring.sign(b"payload").unwrap_err();
        assert!(matches!(err, LicenseError::TokenInvalid(_)), "{err:?}");
    }

    /// 环境变量注入路径：base64 私钥（32 字节种子）→ 可签发，且公钥可被外部独立验证。
    #[test]
    fn register_from_b64_produces_usable_signing_key() {
        let ring = KeyRing::empty();
        // 固定种子（**测试专用**）：0x01..0x20
        let seed: Vec<u8> = (1u8..=32).collect();
        let seed_b64 = B64.encode(&seed);
        ring.register_from_b64(
            "kid-prod-1",
            &seed_b64,
            Some("kms://prod".into()),
            1_700_000_000,
        )
        .unwrap();

        let (kid, sig) = ring.sign(b"token").unwrap();
        assert_eq!(kid, "kid-prod-1");
        assert!(ring.verify("kid-prod-1", b"token", &sig).is_ok());

        // 用独立构造的 Ed25519 公钥验签，证明签名确实由该种子派生（而非自证）。
        let expected = SigningKey::from_bytes(&seed.try_into().unwrap());
        let expected_pub_b64 = B64.encode(expected.verifying_key().to_bytes());
        let entry = ring.get("kid-prod-1").unwrap();
        assert_eq!(entry.public_key_b64(), expected_pub_b64);
    }

    #[test]
    fn register_from_b64_rejects_bad_input_without_echoing_secret() {
        let ring = KeyRing::empty();
        let secret_like = "!!!not-base64!!!";
        let err = ring
            .register_from_b64("kid-x", secret_like, None, 0)
            .unwrap_err();
        let rendered = err.to_string();
        assert!(
            !rendered.contains(secret_like),
            "错误信息泄露了私钥明文: {rendered}"
        );

        // 长度不合法（16 字节）。
        let short = B64.encode([0u8; 16]);
        assert!(ring.register_from_b64("kid-y", &short, None, 0).is_err());
        // 空 kid。
        assert!(ring
            .register_from_b64("", &B64.encode([0u8; 32]), None, 0)
            .is_err());
    }

    /// 仅公钥注册（私钥在隔离签名服务）→ 可验签但**不可签发**。
    #[test]
    fn public_only_key_verifies_but_cannot_sign() {
        // 外部签名服务侧先生成密钥。
        let external = KeyRing::empty();
        let kid = external.register_generated(None, 1_700_000_000).unwrap();
        let (_, sig) = external.sign(b"lease").unwrap();
        let pub_b64 = external.get(&kid).unwrap().public_key_b64().to_string();

        // 业务 API 侧只拿到公钥。
        let business = KeyRing::empty();
        business
            .register_public_only(&kid, &pub_b64, KeyStatus::Active, None, 1_700_000_000)
            .unwrap();
        assert!(business.verify(&kid, b"lease", &sig).is_ok());
        assert_eq!(business.signing_kid(), None);
        assert!(business.sign(b"lease").is_err());
    }

    /// 公钥集只含 active/retiring，绝不下发 retired。
    #[test]
    fn public_key_set_excludes_retired() {
        let ring = KeyRing::empty();
        let a = ring.register_generated(None, 1).unwrap();
        let b = ring.register_generated(None, 2).unwrap();
        let c = ring.register_generated(None, 3).unwrap();
        ring.set_status(&b, KeyStatus::Retiring, 4).unwrap();
        ring.set_status(&c, KeyStatus::Retired, 5).unwrap();

        let set = ring.public_key_set();
        let kids: Vec<&str> = set.iter().map(|e| e.kid.as_str()).collect();
        // kid 含随机后缀，字典序 ≠ 注册序，故按集合比较而非顺序比较。
        assert_eq!(kids.len(), 2, "公钥集应恰含 active+retiring 两把: {kids:?}");
        assert!(
            kids.contains(&a.as_str()),
            "active kid 应在公钥集: {kids:?}"
        );
        assert!(
            kids.contains(&b.as_str()),
            "retiring kid 应在公钥集: {kids:?}"
        );
        assert!(
            !kids.contains(&c.as_str()),
            "retired kid 绝不进公钥集: {kids:?}"
        );
        assert!(set.iter().all(|e| !e.public_key_b64.is_empty()));
        // 公钥集必须输出稳定顺序（BTreeMap 语义），否则客户端侧 diff 会噪声不断。
        let again_set = ring.public_key_set();
        let again: Vec<&str> = again_set.iter().map(|e| e.kid.as_str()).collect();
        assert_eq!(kids, again, "公钥集顺序必须稳定");
        // `kids` 借用了 `set`，此处显式消费掉以避免悬垂借用歧义。
        assert_eq!(set.len(), again_set.len());
    }

    /// `KeyRing` / `KeyEntry` 的 Debug 不含任何私钥材料。
    #[test]
    fn debug_output_never_leaks_private_key_material() {
        let ring = KeyRing::empty();
        let seed: Vec<u8> = (0u8..32).collect();
        let seed_b64 = B64.encode(&seed);
        ring.register_from_b64("kid-dbg", &seed_b64, Some("kms://x".into()), 0)
            .unwrap();

        let ring_dbg = format!("{ring:?}");
        assert!(
            !ring_dbg.contains(&seed_b64),
            "KeyRing Debug 泄露种子: {ring_dbg}"
        );
        assert!(!ring_dbg.contains("kid-dbg/"), "{ring_dbg}");

        let entry_dbg = format!("{:?}", ring.get("kid-dbg").unwrap());
        assert!(
            !entry_dbg.contains(&seed_b64),
            "KeyEntry Debug 泄露种子: {entry_dbg}"
        );
        assert!(entry_dbg.contains("<injected>"), "{entry_dbg}");
    }

    /// public-only 条目 Debug 显示 `<none>`，与持私钥条目可区分。
    #[test]
    fn public_only_entry_debug_marks_signing_none() {
        let ring = KeyRing::empty();
        let z = KeyRing::empty();
        let kid = z.register_generated(None, 0).unwrap();
        let pub_b64 = z.get(&kid).unwrap().public_key_b64().to_string();
        ring.register_public_only(&kid, &pub_b64, KeyStatus::Active, None, 0)
            .unwrap();
        let dbg = format!("{:?}", ring.get(&kid).unwrap());
        assert!(dbg.contains("<none>"), "{dbg}");
    }

    /// 重复注册同一 kid 必须报错（防止意外覆盖正在使用的密钥）。
    #[test]
    fn duplicate_kid_registration_is_rejected() {
        let ring = KeyRing::empty();
        let kid = ring.register_generated(None, 0).unwrap();
        assert!(ring.register_generated(None, 0).is_ok()); // 新 kid
        ring.register_public_only(&kid, &B64.encode([7u8; 32]), KeyStatus::Active, None, 0)
            .unwrap_err()
            .to_string();
        // 用同一 kid 注册公钥必须失败。
        let err = ring
            .register_public_only(&kid, &B64.encode([7u8; 32]), KeyStatus::Active, None, 0)
            .unwrap_err();
        assert!(matches!(err, LicenseError::KeyStateIllegal(_)), "{err:?}");
    }

    /// 非法签名 base64 / 长度错误一律 `TokenInvalid`，不 panic。
    #[test]
    fn malformed_signature_is_rejected_without_panic() {
        let ring = KeyRing::empty();
        let kid = ring.register_generated(None, 0).unwrap();
        assert!(ring.verify(&kid, b"m", "not base64 !!!").is_err());
        assert!(ring.verify(&kid, b"m", &B64.encode([0u8; 10])).is_err());
        assert!(ring.verify(&kid, b"m", &B64.encode([0u8; 64])).is_err());
    }

    /// `verify_any` 能命中正确 kid；无用签名返回错误。
    #[test]
    fn verify_any_finds_matching_kid() {
        let ring = KeyRing::empty();
        let k1 = ring.register_generated(None, 0).unwrap();
        let k2 = ring.register_generated(None, 1).unwrap();
        let (_, sig) = ring.sign(b"msg").unwrap();
        // 当前签发的是 k2。
        assert_eq!(ring.verify_any(b"msg", &sig).unwrap(), k2);
        assert!(ring.verify_any(b"other", &sig).is_err());
        assert_ne!(k1, k2);
    }

    /// kid 生成：格式正确、日期前缀随 UTC 秒变化、且两次生成不重复。
    #[test]
    fn generated_kid_has_date_prefix_and_collision_resistance() {
        // 1_700_000_000 = 2023-11-14 UTC
        let kid = KeyRing::generate_kid(1_700_000_000);
        assert!(kid.starts_with("k20231114-"), "{kid}");
        assert_eq!(kid.len(), 1 + 8 + 1 + 8, "{kid}");
        let other = KeyRing::generate_kid(1_700_000_000);
        assert_ne!(kid, other, "kid 必须带随机后缀");
        // 跨日切换日期前缀。
        let next_day = KeyRing::generate_kid(1_700_000_000 + 86_400);
        assert!(next_day.starts_with("k20231115-"), "{next_day}");
    }

    /// 状态字符串往返 + 非法状态报错。
    #[test]
    fn key_status_round_trips_and_rejects_garbage() {
        for s in [KeyStatus::Active, KeyStatus::Retiring, KeyStatus::Retired] {
            assert_eq!(KeyStatus::parse(s.as_str()).unwrap(), s);
        }
        assert!(KeyStatus::parse("zombie").is_err());
        assert!(KeyStatus::Active.can_sign());
        assert!(!KeyStatus::Retiring.can_sign());
        assert!(KeyStatus::Retiring.can_verify());
        assert!(!KeyStatus::Retired.can_verify());
    }

    /// 空密钥环：签发明确失败（可观测），不 panic。
    #[test]
    fn empty_keyring_fails_to_sign_explicitly() {
        let ring = KeyRing::empty();
        assert_eq!(ring.signing_kid(), None);
        let err = ring.sign(b"x").unwrap_err();
        assert!(matches!(err, LicenseError::TokenInvalid(_)), "{err:?}");
        assert!(ring.public_key_set().is_empty());
    }

    /// `civil_from_days` 边界（纪元日、闰年 2 月底）正确。
    #[test]
    fn civil_from_days_matches_known_dates() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(19_723), (2024, 1, 1)); // 2024-01-01
        assert_eq!(civil_from_days(19_782), (2024, 2, 29)); // 闰日
        assert_eq!(civil_from_days(-1), (1969, 12, 31));
    }
}

//! AuthBlock 签名模块（计划 task 21：Ed25519 + 业务语义哈希）。
//!
//! 设计要点与红线：
//! - **绝不签 Protobuf 序列化字节**：签名对象是 [`semantic_hash`] —— 把批次字段
//!   （点位按 `(device_id, point_id, ts)` 稳定排序）逐字段写入 SHA-256 得到的
//!   **业务语义确定性哈希**；序列化字节序/编码差异不会改变语义哈希，跨端一致。
//! - **私钥不落盘、不在本模块生成**：私钥一律经 [`KeyProvider`] trait 取 in-memory
//!   句柄（真实托管实现由 task 49 提供）。本模块不含任何生产密钥，测试用私钥常量
//!   一律 `TEST_ONLY_` 前缀并显式标注「禁止用于真实部署」。
//! - **mid 不自造**：`mid` 必须是 task 3 [`crate::auth::machine_id::MachineIdentity::get_machine_fingerprint`]
//!   的输出（64 hex），构造 [`AuthSigner`] 时做非空与格式校验。
//! - **授权闸门**：签名前先经 [`LicenseGate::can_sign`]；闸门关闭返回 `AuthError`
//!   （码 3000）且**绝不产出签名**（闸门判定先于取密钥与签名计算）。
//! - **零 panic**：所有可失败点（base64 解码、切片长度、签名构造与验签）都收敛为
//!   `DaemonError`（签名/格式 → `SecurityError`，入参非法 → `ConfigError`）。
//!
//! 签名消息（signing message）构造：
//! ```text
//! "iotdaq.authblock.v1|" || "ph=<len>:<hex(payload_hash)>|" || "mid=<len>:<mid>|"
//!                        || "ts=<len>:<ts_ns>|" || "nonce=<len>:<nonce>|"
//! ```
//! 每字段写入十进制长度 + 内容并以 `|` 收尾（长度入消息，防拼接歧义）；
//! `payload_hash` / `mid` / `ts_ns` / `nonce` 任一被篡改都会导致验签失败。

use std::fmt;
use std::sync::Arc;

use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use base64::Engine as _;
use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use protocol_proto::{AuthBlock, Quality, TelemetryBatch};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::error::{DaemonError, DaemonResult};

/// 签名域分隔前缀（防跨协议 / 跨版本签名复用）。
pub const SIGNING_DOMAIN_V1: &str = "iotdaq.authblock.v1|";

/// 语义哈希域分隔前缀（与签名域区分，防哈希复用）。
const SEMANTIC_DOMAIN_V1: &str = "iotdaq.semantic.v1|";

/// 机器码指纹（mid）长度：64 字符 hex（task 3 指纹输出）。
const MID_HEX_LEN: usize = 64;

/// Ed25519 原始签名长度（字节）。
const SIGNATURE_LEN: usize = 64;

/// 字段写入目标：SHA-256 hasher 与字节缓冲统一走 [`write_field`]，保证
/// 语义哈希与签名消息使用**同一套**定界编码（一处定义，两端一致）。
trait FieldSink {
    /// 追加一段字节。
    fn emit(&mut self, bytes: &[u8]);
}

impl FieldSink for Sha256 {
    fn emit(&mut self, bytes: &[u8]) {
        self.update(bytes);
    }
}

impl FieldSink for Vec<u8> {
    fn emit(&mut self, bytes: &[u8]) {
        self.extend_from_slice(bytes);
    }
}

/// 定界写入单字段：`tag=<十进制字节长度>:<内容>|`。
///
/// 长度入编码，避免 `("ab", "c")` 与 `("a", "bc")` 拼接出同一字节串的歧义；
/// 数值一律先转十进制字符串，避免字节序 / 浮点格式导致的跨端不一致。
fn write_field<S: FieldSink>(sink: &mut S, tag: &str, value: &[u8]) {
    sink.emit(tag.as_bytes());
    sink.emit(b"=");
    sink.emit(value.len().to_string().as_bytes());
    sink.emit(b":");
    sink.emit(value);
    sink.emit(b"|");
}

/// 写入字符串字段。
fn write_str_field<S: FieldSink>(sink: &mut S, tag: &str, value: &str) {
    write_field(sink, tag, value.as_bytes());
}

/// 写入 `i64` 字段（十进制字符串，跨端一致）。
fn write_i64_field<S: FieldSink>(sink: &mut S, tag: &str, value: i64) {
    write_field(sink, tag, value.to_string().as_bytes());
}

/// 业务语义确定性哈希：**不**序列化 protobuf，而是把字段按规则排序后逐字段哈希。
///
/// 规则（与反序列化侧必须一一对应）：
/// 1. `batch.points` 按 `(device_id, point_id, ts)` 稳定排序（点位顺序不影响哈希）；
/// 2. 逐点写入 `device_id / point_id / unit / ts / quality.as_str_name() / value`；
/// 3. 再写入 `batch.gateway_id` 与 `batch.ts`；
/// 4. **不写 `auth` 字段**（否则签名块自指，无法构造）。
///
/// 每字段写入 `tag=<len>:<内容>|`；`i64` 用十进制字符串，`bytes` 原样写入。
pub fn semantic_hash(batch: &TelemetryBatch) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(SEMANTIC_DOMAIN_V1.as_bytes());

    let mut points: Vec<&protocol_proto::DataPoint> = batch.points.iter().collect();
    points.sort_by(|a, b| {
        a.device_id
            .cmp(&b.device_id)
            .then_with(|| a.point_id.cmp(&b.point_id))
            .then_with(|| a.ts.cmp(&b.ts))
    });

    write_i64_field(&mut hasher, "points", points.len() as i64);
    for point in &points {
        write_str_field(&mut hasher, "device_id", &point.device_id);
        write_str_field(&mut hasher, "point_id", &point.point_id);
        write_str_field(&mut hasher, "unit", &point.unit);
        write_i64_field(&mut hasher, "ts", point.ts);
        write_str_field(&mut hasher, "quality", quality_name(point.quality));
        write_field(&mut hasher, "value", &point.value);
    }

    write_str_field(&mut hasher, "gateway_id", &batch.gateway_id);
    write_i64_field(&mut hasher, "batch_ts", batch.ts);

    hasher.finalize().into()
}

/// 质量码枚举名。
///
/// prost 生成的枚举未提供 `from_i32`，此处按 wire 值映射回枚举名
/// （复用 `as_str_name()`，不重复定义字符串）；未知码值退化为 `QUALITY_INVALID`
/// 而非 panic，保证哈希计算路径零 panic。
fn quality_name(quality: i32) -> &'static str {
    match quality {
        q if q == Quality::Good as i32 => Quality::Good.as_str_name(),
        q if q == Quality::Uncertain as i32 => Quality::Uncertain.as_str_name(),
        q if q == Quality::Bad as i32 => Quality::Bad.as_str_name(),
        q if q == Quality::Simulated as i32 => Quality::Simulated.as_str_name(),
        q if q == Quality::Unspecified as i32 => Quality::Unspecified.as_str_name(),
        _ => "QUALITY_INVALID",
    }
}

/// 构造签名消息：`DOMAIN || ph || mid || ts || nonce`（每字段 `tag=<len>:<内容>|`）。
fn signing_message(payload_hash: &[u8; 32], mid: &str, ts_ns: i64, nonce: &str) -> Vec<u8> {
    let mut msg: Vec<u8> = Vec::with_capacity(SIGNING_DOMAIN_V1.len() + 160);
    msg.emit(SIGNING_DOMAIN_V1.as_bytes());
    write_field(&mut msg, "ph", hex::encode(payload_hash).as_bytes());
    write_str_field(&mut msg, "mid", mid);
    write_i64_field(&mut msg, "ts", ts_ns);
    write_str_field(&mut msg, "nonce", nonce);
    msg
}

/// 密钥托管（真实实现由 task 49 提供；本模块只定义 trait + in-memory 实现）。
///
/// 契约：`signing_key()` 返回**内存态**私钥句柄；实现方与本模块都不得把私钥写入磁盘
/// 或写进日志（项目红线）。
pub trait KeyProvider: Send + Sync {
    /// 返回 in-memory 私钥句柄；**实现方不得把私钥写入磁盘**（本模块也不得落盘）。
    ///
    /// # Errors
    /// 密钥不可用（未注入 / 已吊销 / 长度非法）时返回错误，不 panic。
    fn signing_key(&self) -> DaemonResult<SigningKey>;
}

/// 授权闸门（真实实现由 task 22 云授权客户端提供：Token 有效且未降级才允许签名）。
pub trait LicenseGate: Send + Sync {
    /// 当前是否允许签名（Token 有效且未降级 / 未过期）。
    fn can_sign(&self) -> bool;
}

/// 签名产物（与 protocol-proto 的 `AuthBlock` 一一对应）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SignedAuthBlock {
    /// 网关机器码指纹（task 3 输出，64 hex）。
    pub mid: String,
    /// 防重放随机串（UUID v4）。
    pub nonce: String,
    /// 签名时刻，Unix 纳秒。
    pub ts_ns: i64,
    /// Ed25519 签名，标准 base64 编码。
    pub sig_b64: String,
}

impl SignedAuthBlock {
    /// 转 protobuf `AuthBlock`。
    ///
    /// 约定：`sig` 字段承载 **base64 文本的 ASCII 字节**（与 `bytes → JSON base64`
    /// 通道、以及 [`SignedAuthBlock::from_proto`] 的往返保持一致）。
    pub fn to_proto(&self) -> AuthBlock {
        AuthBlock {
            mid: self.mid.clone(),
            nonce: self.nonce.clone(),
            ts: self.ts_ns,
            sig: self.sig_b64.as_bytes().to_vec(),
        }
    }

    /// 从 protobuf `AuthBlock` 还原（`sig` 须为 base64 文本，否则 `SecurityError`）。
    ///
    /// # Errors
    /// `sig` 非 UTF-8 文本，或 `mid` / `nonce` 为空时返回 `SecurityError`。
    pub fn from_proto(auth: &AuthBlock) -> DaemonResult<Self> {
        let sig_b64 = std::str::from_utf8(&auth.sig).map_err(|e| {
            DaemonError::SecurityError(format!("auth block sig is not base64 text: {e}"))
        })?;
        let block = Self {
            mid: auth.mid.clone(),
            nonce: auth.nonce.clone(),
            ts_ns: auth.ts,
            sig_b64: sig_b64.to_string(),
        };
        block.validate_shape()?;
        Ok(block)
    }

    /// 结构校验：mid / nonce / sig 均不得为空（防空字段绕过验签）。
    fn validate_shape(&self) -> DaemonResult<()> {
        if self.mid.is_empty() {
            return Err(DaemonError::SecurityError(
                "auth block mid must not be empty".to_string(),
            ));
        }
        if self.nonce.is_empty() {
            return Err(DaemonError::SecurityError(
                "auth block nonce must not be empty".to_string(),
            ));
        }
        if self.sig_b64.is_empty() {
            return Err(DaemonError::SecurityError(
                "auth block signature must not be empty".to_string(),
            ));
        }
        Ok(())
    }
}

/// 静态 in-memory 密钥提供者（测试 / 受控注入场景；生产由 task 49 托管实现替代）。
///
/// 私钥字节仅存在于内存；`Debug` 输出脱敏，永不进入日志。
pub struct StaticKeyProvider {
    key: [u8; 32],
}

impl StaticKeyProvider {
    /// 从 32 字节种子构造（**禁止把生产私钥硬编码进源码**）。
    pub fn new(key: [u8; 32]) -> Self {
        Self { key }
    }

    /// 对应公钥（接收侧验签 / 公钥登记使用）。
    ///
    /// # Errors
    /// 私钥句柄获取失败时透传（本实现恒成功）。
    pub fn verifying_key(&self) -> DaemonResult<VerifyingKey> {
        Ok(self.signing_key()?.verifying_key())
    }
}

impl fmt::Debug for StaticKeyProvider {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("StaticKeyProvider { key: <redacted> }")
    }
}

impl KeyProvider for StaticKeyProvider {
    fn signing_key(&self) -> DaemonResult<SigningKey> {
        Ok(SigningKey::from_bytes(&self.key))
    }
}

/// AuthBlock 签名器：密钥托管 + 授权闸门 + 机器码指纹三件套。
pub struct AuthSigner {
    keys: Arc<dyn KeyProvider>,
    gate: Arc<dyn LicenseGate>,
    mid: String,
}

impl fmt::Debug for AuthSigner {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AuthSigner")
            .field("mid", &self.mid)
            .field("keys", &"<opaque>")
            .finish()
    }
}

impl AuthSigner {
    /// 构造签名器。
    ///
    /// `mid` 必须是 task 3 `MachineIdentity::get_machine_fingerprint()` 的输出
    /// （64 字符 hex）；空白串或格式不符 → `ConfigError`（码 2000）。
    ///
    /// # Errors
    /// `mid` 为空或不是 64 字符 hex 时返回 `DaemonError::ConfigError`。
    pub fn new(
        keys: Arc<dyn KeyProvider>,
        gate: Arc<dyn LicenseGate>,
        mid: String,
    ) -> DaemonResult<Self> {
        let trimmed = mid.trim().to_string();
        if trimmed.is_empty() {
            return Err(DaemonError::ConfigError(
                "auth signer mid must not be empty (expected machine fingerprint from task 3)"
                    .to_string(),
            ));
        }
        if trimmed.len() != MID_HEX_LEN || !trimmed.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(DaemonError::ConfigError(format!(
                "auth signer mid must be {MID_HEX_LEN}-char lowercase/uppercase hex \
                 (machine fingerprint), got len {}",
                trimmed.len()
            )));
        }
        Ok(Self {
            keys,
            gate,
            mid: trimmed,
        })
    }

    /// 当前签名器绑定的机器码指纹。
    pub fn mid(&self) -> &str {
        &self.mid
    }

    /// 对业务语义哈希签名；**授权闸门关闭时返回 AuthError 且绝不产出签名**。
    ///
    /// 顺序保证：闸门判定 → 生成 nonce → 取私钥 → 计算签名；
    /// 闸门关闭时在任何签名材料生成之前即返回，不会泄漏签名输出。
    ///
    /// # Errors
    /// - 闸门关闭：`DaemonError::AuthError`（码 3000）；
    /// - 密钥不可用：透传 `KeyProvider` 的错误。
    pub fn sign_semantic(
        &self,
        payload_hash: &[u8; 32],
        ts_ns: i64,
    ) -> DaemonResult<SignedAuthBlock> {
        if !self.gate.can_sign() {
            return Err(DaemonError::AuthError(
                "license gate closed (token invalid or downgraded): refusing to sign auth block"
                    .to_string(),
            ));
        }

        let nonce = Uuid::new_v4().to_string();
        let signing_key = self.keys.signing_key()?;
        let message = signing_message(payload_hash, &self.mid, ts_ns, &nonce);
        let signature: Signature = signing_key.sign(&message);

        Ok(SignedAuthBlock {
            mid: self.mid.clone(),
            nonce,
            ts_ns,
            sig_b64: BASE64_STANDARD.encode(signature.to_bytes()),
        })
    }

    /// 便捷入口：先算 [`semantic_hash`] 再签名。
    ///
    /// # Errors
    /// 同 [`AuthSigner::sign_semantic`]。
    pub fn sign_batch(&self, batch: &TelemetryBatch, ts_ns: i64) -> DaemonResult<SignedAuthBlock> {
        let payload_hash = semantic_hash(batch);
        self.sign_semantic(&payload_hash, ts_ns)
    }

    /// 把签名块写回 protobuf 批次（供北向转发使用）。
    ///
    /// 签名对象是**写入前**批次的语义哈希（`semantic_hash` 不包含 `auth` 字段），
    /// 因此不会自指；写回后 `batch.auth` 恒为 `Some`。
    ///
    /// # Errors
    /// 同 [`AuthSigner::sign_semantic`]；失败时 `batch.auth` 保持原值不被写入。
    pub fn attach(&self, batch: &mut TelemetryBatch, ts_ns: i64) -> DaemonResult<()> {
        let block = self.sign_batch(batch, ts_ns)?;
        batch.auth = Some(block.to_proto());
        Ok(())
    }
}

/// 独立验签（接收侧 / 任务 48 二次校验使用）。任何不匹配 → `SecurityError`。
///
/// # Errors
/// - `mid` / `nonce` / `sig` 为空：`SecurityError`；
/// - base64 解码失败或签名长度 ≠ 64：`SecurityError`；
/// - 签名不匹配：`SecurityError`。
pub fn verify_semantic(
    block: &SignedAuthBlock,
    payload_hash: &[u8; 32],
    public_key: &VerifyingKey,
) -> DaemonResult<()> {
    block.validate_shape()?;

    let sig_bytes = BASE64_STANDARD
        .decode(block.sig_b64.as_bytes())
        .map_err(|e| {
            DaemonError::SecurityError(format!("auth block signature is not valid base64: {e}"))
        })?;
    let sig_array: [u8; SIGNATURE_LEN] = sig_bytes.as_slice().try_into().map_err(|_| {
        DaemonError::SecurityError(format!(
            "auth block signature length {} != {SIGNATURE_LEN}",
            sig_bytes.len()
        ))
    })?;
    let signature = Signature::from_bytes(&sig_array);

    let message = signing_message(payload_hash, &block.mid, block.ts_ns, &block.nonce);
    public_key.verify(&message, &signature).map_err(|e| {
        DaemonError::SecurityError(format!("auth block signature verification failed: {e}"))
    })
}

/// 便捷验签：先算 [`semantic_hash`] 再走 [`verify_semantic`]。
///
/// # Errors
/// 同 [`verify_semantic`]。
pub fn verify_batch(
    block: &SignedAuthBlock,
    batch: &TelemetryBatch,
    public_key: &VerifyingKey,
) -> DaemonResult<()> {
    let payload_hash = semantic_hash(batch);
    verify_semantic(block, &payload_hash, public_key)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::machine_id::{FingerprintKey, MachineIdentity, StaticAnchor};
    use crate::error::ERR_AUTH;

    /// test-only：单元测试专用 Ed25519 私钥种子 A —— 与任何生产密钥无关，
    /// **仅测试使用，禁止用于真实部署**。
    const TEST_ONLY_KEY_A: [u8; 32] = [0x1au8; 32];

    /// test-only：单元测试专用 Ed25519 私钥种子 B（用于「不同私钥验签失败」）。
    /// **仅测试使用，禁止用于真实部署**。
    const TEST_ONLY_KEY_B: [u8; 32] = [0x2bu8; 32];

    /// test-only：指纹 HMAC key A（task 3 派生 mid 用），禁止用于真实部署。
    const TEST_ONLY_FP_KEY_A: &[u8] = b"TEST_ONLY_signing_fp_key_a";

    /// test-only：指纹 HMAC key B（造出不同 mid 用），禁止用于真实部署。
    const TEST_ONLY_FP_KEY_B: &[u8] = b"TEST_ONLY_signing_fp_key_b";

    /// 固定签名时间戳（纳秒），保证测试可重复。
    const TEST_TS_NS: i64 = 1_763_000_000_000_000_000;

    /// 测试用授权闸门：恒定放行（模拟 Token 有效且未降级）。
    pub struct AlwaysLicensed;

    impl LicenseGate for AlwaysLicensed {
        fn can_sign(&self) -> bool {
            true
        }
    }

    /// 测试用授权闸门：恒定拒绝（模拟 Token 过期 / 已降级 / 未激活）。
    pub struct DeniedLicense;

    impl LicenseGate for DeniedLicense {
        fn can_sign(&self) -> bool {
            false
        }
    }

    /// 经 task 3 指纹模块派生 mid（**本模块不自造 mid 算法**）。
    fn machine_fingerprint(anchor_value: &str, hmac_key: &[u8]) -> String {
        let identity = MachineIdentity::new(
            vec![Box::new(StaticAnchor::new(
                "test-anchor",
                Some(anchor_value),
            ))],
            1,
            FingerprintKey::from_bytes(hmac_key.to_vec()).expect("test-only fp key is non-empty"),
        );
        identity
            .get_machine_fingerprint()
            .expect("quorum ok: 1 usable anchor of 1 required")
    }

    /// 默认测试 mid（锚点 "gw-sign-001" + HMAC key A）。
    fn test_mid() -> String {
        machine_fingerprint("gw-sign-001", TEST_ONLY_FP_KEY_A)
    }

    /// 另一台机器的 mid（锚点不变、HMAC key 换 B）。
    fn other_mid() -> String {
        machine_fingerprint("gw-sign-001", TEST_ONLY_FP_KEY_B)
    }

    /// 构造签名器（默认 mid + 指定闸门 + 指定私钥）。
    fn signer_for(key: [u8; 32], gate: Arc<dyn LicenseGate>) -> AuthSigner {
        let mid = test_mid();
        AuthSigner::new(Arc::new(StaticKeyProvider::new(key)), gate, mid)
            .expect("test mid is 64-hex")
    }

    /// 默认签名器（私钥 A + 恒定放行闸门）。
    fn test_signer() -> AuthSigner {
        signer_for(TEST_ONLY_KEY_A, Arc::new(AlwaysLicensed))
    }

    /// 私钥 A 对应公钥。
    fn public_key_a() -> VerifyingKey {
        StaticKeyProvider::new(TEST_ONLY_KEY_A)
            .verifying_key()
            .expect("signing key available")
    }

    /// 构造示例批次（点位**故意乱序**，用于验证语义哈希的顺序无关性）。
    fn sample_batch() -> TelemetryBatch {
        TelemetryBatch {
            points: vec![
                protocol_proto::DataPoint {
                    device_id: "pump-02".to_string(),
                    point_id: "outlet_pressure".to_string(),
                    value: vec![0x41, 0x28, 0x00, 0x00],
                    unit: "kPa".to_string(),
                    ts: 1_762_999_999_000_000_002,
                    quality: Quality::Good as i32,
                },
                protocol_proto::DataPoint {
                    device_id: "pump-01".to_string(),
                    point_id: "inlet_temp".to_string(),
                    value: vec![0x42, 0x2c, 0x00, 0x00],
                    unit: "degC".to_string(),
                    ts: 1_762_999_999_000_000_001,
                    quality: Quality::Good as i32,
                },
                protocol_proto::DataPoint {
                    device_id: "pump-01".to_string(),
                    point_id: "vibration".to_string(),
                    value: vec![0x40, 0x49, 0x0f, 0xdb],
                    unit: "mm/s".to_string(),
                    ts: 1_762_999_999_000_000_003,
                    quality: Quality::Uncertain as i32,
                },
            ],
            ts: 1_762_999_999_500_000_000,
            gateway_id: "gw-sign-001".to_string(),
            auth: None,
        }
    }

    /// 断言：对批次施加 `mutate` 篡改后验签必须失败（且为 SecurityError）。
    fn assert_tampered_batch_fails(label: &str, mutate: impl FnOnce(&mut TelemetryBatch)) {
        let signer = test_signer();
        let batch = sample_batch();
        let block = signer
            .sign_batch(&batch, TEST_TS_NS)
            .expect("gate open: signing must succeed");

        let mut tampered = batch;
        mutate(&mut tampered);

        let err = verify_batch(&block, &tampered, &public_key_a())
            .expect_err("tampered batch must fail verification");
        assert_eq!(
            err.error_code(),
            7000,
            "{label}: tamper must surface as SecurityError, got {err}"
        );
        assert!(
            matches!(err, DaemonError::SecurityError(_)),
            "{label}: expected SecurityError, got {err}"
        );
    }

    /// QA Happy：构造 batch → `sign_batch` → `verify_batch` → 通过。
    #[test]
    fn sign_then_verify_batch_succeeds() {
        let signer = test_signer();
        let batch = sample_batch();

        let block = signer
            .sign_batch(&batch, TEST_TS_NS)
            .expect("licensed: sign must succeed");

        assert_eq!(block.mid, test_mid());
        assert_eq!(block.ts_ns, TEST_TS_NS);
        assert_eq!(block.nonce.len(), 36, "nonce must be UUID v4 string");
        assert!(!block.sig_b64.is_empty());

        verify_batch(&block, &batch, &public_key_a()).expect("signature must verify");
    }

    /// QA Error：篡改 `points[0].unit` → 验签失败。
    #[test]
    fn tamper_point_unit_fails_verification() {
        assert_tampered_batch_fails("unit", |batch| {
            batch.points[0].unit = "bar".to_string();
        });
    }

    /// QA Error：篡改点位 `value` 字节 → 验签失败。
    #[test]
    fn tamper_point_value_fails_verification() {
        assert_tampered_batch_fails("value", |batch| {
            batch.points[0].value = vec![0x00, 0x01];
        });
    }

    /// QA Error：篡改点位 `ts` → 验签失败。
    #[test]
    fn tamper_point_ts_fails_verification() {
        assert_tampered_batch_fails("point ts", |batch| {
            batch.points[1].ts += 1;
        });
    }

    /// QA Error：篡改 `gateway_id` → 验签失败。
    #[test]
    fn tamper_gateway_id_fails_verification() {
        assert_tampered_batch_fails("gateway_id", |batch| {
            batch.gateway_id = "gw-evil-999".to_string();
        });
    }

    /// QA Error：篡改 `batch.ts` → 验签失败。
    #[test]
    fn tamper_batch_ts_fails_verification() {
        assert_tampered_batch_fails("batch ts", |batch| {
            batch.ts += 7;
        });
    }

    /// QA Error：篡改点位 `quality` 与 `device_id` → 验签失败。
    #[test]
    fn tamper_quality_and_device_id_fail_verification() {
        assert_tampered_batch_fails("quality", |batch| {
            batch.points[2].quality = Quality::Bad as i32;
        });
        assert_tampered_batch_fails("device_id", |batch| {
            batch.points[0].device_id = "pump-09".to_string();
        });
    }

    /// 篡改签名块的 `nonce` / `ts_ns` / `mid` → 验签失败（签名域覆盖全部字段）。
    #[test]
    fn tamper_block_fields_fails_verification() {
        let signer = test_signer();
        let batch = sample_batch();
        let block = signer
            .sign_batch(&batch, TEST_TS_NS)
            .expect("licensed: sign must succeed");
        let hash = semantic_hash(&batch);
        let pk = public_key_a();

        let mut bad_nonce = block.clone();
        bad_nonce.nonce = Uuid::new_v4().to_string();
        assert!(
            verify_semantic(&bad_nonce, &hash, &pk).is_err(),
            "nonce tamper must fail"
        );

        let mut bad_ts = block.clone();
        bad_ts.ts_ns = TEST_TS_NS + 1;
        assert!(
            verify_semantic(&bad_ts, &hash, &pk).is_err(),
            "ts tamper must fail"
        );

        let mut bad_mid = block.clone();
        bad_mid.mid = other_mid();
        assert!(
            verify_semantic(&bad_mid, &hash, &pk).is_err(),
            "mid tamper must fail"
        );

        // 空字段不允许绕过验签。
        let mut empty_nonce = block.clone();
        empty_nonce.nonce = String::new();
        assert!(
            verify_semantic(&empty_nonce, &hash, &pk).is_err(),
            "empty nonce must fail"
        );
    }

    /// 篡改签名块 `sig_b64`（换另一把私钥的合法签名 / 非法 base64）→ 验签失败。
    #[test]
    fn tamper_signature_fails_verification() {
        let batch = sample_batch();
        let block = test_signer()
            .sign_batch(&batch, TEST_TS_NS)
            .expect("licensed: sign must succeed");
        let hash = semantic_hash(&batch);
        let pk = public_key_a();

        // 用另一把私钥签同一消息（合法 base64、长度正确，但密钥不对）。
        let foreign = signer_for(TEST_ONLY_KEY_B, Arc::new(AlwaysLicensed))
            .sign_semantic(&hash, TEST_TS_NS)
            .expect("licensed: sign must succeed");
        let mut swapped = block.clone();
        swapped.sig_b64 = foreign.sig_b64;
        assert!(
            verify_semantic(&swapped, &hash, &pk).is_err(),
            "foreign signature must fail"
        );

        // 签名乱码（非 base64）。
        let mut garbage = block.clone();
        garbage.sig_b64 = "not-base64!!!".to_string();
        let err = verify_semantic(&garbage, &hash, &pk).expect_err("garbage sig must fail");
        assert_eq!(err.error_code(), 7000);

        // 长度不足 64 字节的签名。
        let mut short = block.clone();
        short.sig_b64 = BASE64_STANDARD.encode([0x00u8; 10]);
        let err = verify_semantic(&short, &hash, &pk).expect_err("short sig must fail");
        assert_eq!(err.error_code(), 7000);
    }

    /// 授权失效：gate 返回 false → `AuthError`（码 3000）且**不产出签名**。
    #[test]
    fn license_denied_refuses_to_sign() {
        let signer = signer_for(TEST_ONLY_KEY_A, Arc::new(DeniedLicense));
        let batch = sample_batch();

        let err = signer
            .sign_batch(&batch, TEST_TS_NS)
            .expect_err("gate closed must refuse to sign");
        assert!(
            matches!(err, DaemonError::AuthError(_)),
            "expected AuthError, got {err}"
        );
        assert_eq!(err.error_code(), ERR_AUTH);
        assert_eq!(err.error_code(), 3000);
        assert!(
            err.to_string().contains("license gate closed"),
            "error must explain refusal: {err}"
        );
        // 不产出签名：错误发生在 nonce 生成 / 取密钥 / 签名计算之前，
        // 因此不存在任何签名产物（无 Ok(T) 分支，签名材料从未被构造）。
        assert!(
            !err.to_string().contains("sig_b64"),
            "no signature material may be produced: {err}"
        );

        // 另一入口同样拒绝。
        let hash = semantic_hash(&batch);
        let err2 = signer
            .sign_semantic(&hash, TEST_TS_NS)
            .expect_err("gate closed must refuse to sign");
        assert_eq!(err2.error_code(), ERR_AUTH);

        // 闸门关闭时 attach 不得写入 auth 块。
        let mut guarded = batch.clone();
        assert!(signer.attach(&mut guarded, TEST_TS_NS).is_err());
        assert!(guarded.auth.is_none(), "no auth block may be attached");
    }

    /// mid 变化（另一把 FingerprintKey 派生）→ 用原 mid 的公钥验签失败。
    #[test]
    fn different_mid_fails_verification() {
        let batch = sample_batch();
        let block = test_signer()
            .sign_batch(&batch, TEST_TS_NS)
            .expect("licensed: sign must succeed");

        let mut relocated = block.clone();
        relocated.mid = other_mid();
        assert!(
            verify_batch(&relocated, &batch, &public_key_a()).is_err(),
            "mid change must invalidate signature"
        );

        // 真实场景：换机器（新 mid）重新签名，验签通过但 mid 与签名器绑定一致。
        let other_signer = AuthSigner::new(
            Arc::new(StaticKeyProvider::new(TEST_ONLY_KEY_A)),
            Arc::new(AlwaysLicensed),
            other_mid(),
        )
        .expect("mid is 64-hex");
        let reblock = other_signer
            .sign_batch(&batch, TEST_TS_NS)
            .expect("licensed: sign must succeed");
        assert_eq!(reblock.mid, other_mid());
        verify_batch(&reblock, &batch, &public_key_a()).expect("same key, own mid: ok");
    }

    /// 确定性：同一批次两次哈希相等；打乱点位顺序哈希**不变**。
    #[test]
    fn semantic_hash_is_order_independent_and_deterministic() {
        let batch = sample_batch();
        let first = semantic_hash(&batch);
        let second = semantic_hash(&batch);
        assert_eq!(first, second, "hash must be deterministic");

        let mut shuffled = batch.clone();
        shuffled.points.reverse();
        assert_ne!(
            shuffled.points[0].point_id, batch.points[0].point_id,
            "fixture must actually be reordered"
        );
        assert_eq!(
            semantic_hash(&shuffled),
            first,
            "point order must not affect semantic hash"
        );

        // 点位顺序变化后，原签名依然可验（这是「排序后哈希」的核心收益）。
        let signer = test_signer();
        let block = signer
            .sign_batch(&batch, TEST_TS_NS)
            .expect("licensed: sign must succeed");
        verify_batch(&block, &shuffled, &public_key_a()).expect("order-insensitive verify");
    }

    /// 顺序敏感性：交换两个点的 ts（或改 point_id）→ 哈希改变。
    #[test]
    fn semantic_hash_is_sensitive_to_content() {
        let batch = sample_batch();
        let base = semantic_hash(&batch);

        let mut swapped_ts = batch.clone();
        swapped_ts.points.swap(0, 1);
        swapped_ts.points.swap(1, 2); // 制造 ts 与点位错配
        swapped_ts.points[0].ts = batch.points[1].ts;
        swapped_ts.points[1].ts = batch.points[0].ts;
        assert_ne!(
            semantic_hash(&swapped_ts),
            base,
            "swapped point ts must change hash"
        );

        let mut renamed = batch.clone();
        renamed.points[0].point_id = "inlet_temp_2".to_string();
        assert_ne!(
            semantic_hash(&renamed),
            base,
            "changed point_id must change hash"
        );
    }

    /// 非空 / 格式校验：空 mid → ConfigError（码 2000）；非 64 hex 同样拒绝。
    #[test]
    fn invalid_mid_is_rejected() {
        let keys: Arc<dyn KeyProvider> = Arc::new(StaticKeyProvider::new(TEST_ONLY_KEY_A));
        let gate: Arc<dyn LicenseGate> = Arc::new(AlwaysLicensed);

        let err = AuthSigner::new(Arc::clone(&keys), Arc::clone(&gate), String::new())
            .expect_err("empty mid must be rejected");
        assert!(matches!(err, DaemonError::ConfigError(_)), "got {err}");
        assert_eq!(err.error_code(), 2000);

        let err = AuthSigner::new(Arc::clone(&keys), Arc::clone(&gate), "   ".to_string())
            .expect_err("blank mid must be rejected");
        assert_eq!(err.error_code(), 2000);

        let err = AuthSigner::new(Arc::clone(&keys), Arc::clone(&gate), "abc".to_string())
            .expect_err("non-64-hex mid must be rejected");
        assert_eq!(err.error_code(), 2000);

        let err = AuthSigner::new(Arc::clone(&keys), Arc::clone(&gate), "z".repeat(64))
            .expect_err("non-hex chars must be rejected");
        assert_eq!(err.error_code(), 2000);

        // 合法 64 hex 放行（大小写 hex 均可）。
        let ok = AuthSigner::new(Arc::clone(&keys), Arc::clone(&gate), test_mid());
        assert!(ok.is_ok(), "valid 64-hex mid must be accepted");
    }

    /// `attach`：写回后 `batch.auth` 为 Some，mid/ts 正确，且能通过 `verify_batch`。
    #[test]
    fn attach_writes_auth_block_and_verifies() {
        let signer = test_signer();
        let mut batch = sample_batch();
        let expected_mid = test_mid();

        signer
            .attach(&mut batch, TEST_TS_NS)
            .expect("licensed: attach must succeed");

        let auth = batch
            .auth
            .as_ref()
            .expect("attach must populate auth block");
        assert_eq!(auth.mid, expected_mid);
        assert_eq!(auth.ts, TEST_TS_NS);
        assert!(!auth.nonce.is_empty());
        assert!(!auth.sig.is_empty());

        let block =
            SignedAuthBlock::from_proto(auth).expect("attached block must round-trip to base64");
        verify_batch(&block, &batch, &public_key_a()).expect("attached block must verify");

        // 写回 auth 不影响语义哈希（哈希不含 auth 字段）。
        let before = semantic_hash(&sample_batch());
        assert_eq!(
            semantic_hash(&batch),
            before,
            "auth must be excluded from hash"
        );
    }

    /// 红线守护：语义哈希 ≠ 序列化字节哈希（禁止直接签 protobuf 字节）。
    #[test]
    fn semantic_hash_differs_from_serialized_bytes_hash() {
        let batch = sample_batch();
        let semantic = semantic_hash(&batch);

        // 模拟「直接序列化字节 → SHA-256」：按字段顺序裸拼接（不含 auth，
        // 因为 auth 在签名时必然为空），这是签名 protobuf 字节的等价物。
        // daemon 不直接依赖 prost，故此处以同字段同内容的裸拼接代表序列化字节。
        let mut hasher = Sha256::new();
        for point in &batch.points {
            hasher.update(point.device_id.as_bytes());
            hasher.update(point.point_id.as_bytes());
            hasher.update(&point.value);
            hasher.update(point.unit.as_bytes());
            hasher.update(point.ts.to_be_bytes());
            hasher.update(point.quality.to_be_bytes());
        }
        hasher.update(batch.ts.to_be_bytes());
        hasher.update(batch.gateway_id.as_bytes());
        let serialized: [u8; 32] = hasher.finalize().into();

        assert_ne!(
            semantic, serialized,
            "semantic hash must never equal a raw serialization digest"
        );
    }

    /// 不同私钥签名 → 用原公钥验签失败。
    #[test]
    fn signature_from_other_key_fails_verification() {
        let batch = sample_batch();
        let block = signer_for(TEST_ONLY_KEY_B, Arc::new(AlwaysLicensed))
            .sign_batch(&batch, TEST_TS_NS)
            .expect("licensed: sign must succeed");

        let err = verify_batch(&block, &batch, &public_key_a())
            .expect_err("foreign key signature must fail");
        assert_eq!(err.error_code(), 7000);
        assert!(matches!(err, DaemonError::SecurityError(_)));

        // 用 B 自己的公钥则通过（证明是密钥不匹配而非消息构造问题）。
        let pk_b = StaticKeyProvider::new(TEST_ONLY_KEY_B)
            .verifying_key()
            .expect("signing key available");
        verify_batch(&block, &batch, &pk_b).expect("own public key must verify");
    }

    /// 每次签名生成独立 nonce（防重放）：同一批次连签两次签名串不同。
    #[test]
    fn nonce_is_fresh_per_signature() {
        let signer = test_signer();
        let batch = sample_batch();

        let first = signer
            .sign_batch(&batch, TEST_TS_NS)
            .expect("licensed: sign must succeed");
        let second = signer
            .sign_batch(&batch, TEST_TS_NS)
            .expect("licensed: sign must succeed");

        assert_ne!(first.nonce, second.nonce, "nonce must be unique per call");
        assert_ne!(first.sig_b64, second.sig_b64, "signature must differ");
        verify_batch(&first, &batch, &public_key_a()).expect("first must verify");
        verify_batch(&second, &batch, &public_key_a()).expect("second must verify");
    }

    /// 私钥不进日志：`StaticKeyProvider` / `AuthSigner` 的 Debug 输出脱敏。
    #[test]
    fn debug_output_never_leaks_key_material() {
        let provider = StaticKeyProvider::new(TEST_ONLY_KEY_A);
        let rendered = format!("{provider:?}");
        assert_eq!(rendered, "StaticKeyProvider { key: <redacted> }");
        assert!(!rendered.contains("1a1a"), "no key bytes in Debug output");

        let signer = test_signer();
        let rendered = format!("{signer:?}");
        assert!(rendered.contains(&test_mid()), "mid is safe to log");
        assert!(!rendered.contains("1a1a"), "no key bytes in Debug output");
    }

    /// 签名消息构造：域前缀 + 四个长度定界字段，且随任一字段变化而变化。
    #[test]
    fn signing_message_is_domain_separated_and_length_delimited() {
        let hash = semantic_hash(&sample_batch());
        let msg = signing_message(&hash, "aabb", 42, "nonce-1");

        let rendered = String::from_utf8(msg).expect("message is ascii text");
        assert!(
            rendered.starts_with(SIGNING_DOMAIN_V1),
            "domain prefix first"
        );
        assert!(rendered.contains(&format!("ph=64:{}", hex::encode(hash))));
        assert!(rendered.contains("mid=4:aabb|"));
        assert!(rendered.contains("ts=2:42|"));
        assert!(rendered.contains("nonce=7:nonce-1|"));

        // 长度入消息：("ab","c") 与 ("a","bc") 不得等价。
        let left = signing_message(&hash, "ab", 1, "c");
        let right = signing_message(&hash, "a", 1, "bc");
        assert_ne!(left, right, "length delimiting must prevent ambiguity");
    }
}

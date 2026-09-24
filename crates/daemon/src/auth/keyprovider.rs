//! `auth::keyprovider` — 客户端密钥托管（计划 task 49）。
//!
//! 为 AuthBlock 签名（task 21 `signing.rs`）提供持久化的 Ed25519 密钥托管：
//! 首次启动生成密钥对，私钥经「机器码指纹 HKDF 派生密钥」加密后落盘；
//! 重启加载解密复用**同一密钥**（跨实例一致）。
//!
//! # V1 方案与平台边界（模块头声明，硬性红线）
//! - **纯 Rust 栈**：不做 Windows DPAPI / Linux keyring 等 OS 原生密钥库调用；
//!   V1 采用「受限权限文件 + HKDF 机器码派生加密」方案。OS 原生密钥集成
//!   （DPAPI / keyring / TPM）留给后续平台差异任务。
//! - **拷盘即失效**：加密密钥由机器码指纹（task 3 `MachineIdentity` 输出，
//!   64 hex）经 HKDF-SHA256 域分隔派生（与 `trial.rs` 的机器码派生同源思路）；
//!   密钥文件拷贝到另一台机器后必然解密失败。
//! - **失败即拒绝（fail-closed）**：完整性校验 / 机器绑定失败时返回明确错误，
//!   **绝不静默重建密钥**（防克隆攻击；是否重置由上层运维策略决定）。
//! - **私钥红线**：私钥永不明文落盘、永不进日志、永不外传（只上传公钥）；
//!   [`KeyHandle`] 不提供任何导出原始字节的方法（无 `to_bytes` / `as_bytes`），
//!   只暴露 [`KeyHandle::sign`] / [`KeyHandle::verify`] 操作与
//!   [`KeyHandle::public_key`]（公钥可自由导出）。
//! - **Windows ACL 从简**：文件权限仅在 Unix 上收紧为 0600
//!   （`std::fs::set_permissions`，仅 `cfg(unix)` 分支）；Windows 侧依赖
//!   当前用户 profile 的默认 ACL，精细 ACL 留给平台差异任务。
//! - **零 panic**：所有可失败点收敛为 [`KeyError`]；不使用 `unwrap` /
//!   裸索引 panic 路径（切片均先做长度校验）。
//! - **HKDF-SHA256 就地实现**：daemon 依赖树无 `hkdf` crate（禁止新依赖），
//!   本模块按 RFC 5869 用 `sha2` 实现 Extract/Expand 两步（约 20 行，
//!   含 SHA-256 手写 HMAC 内核，避免不可达的错误分支引入 panic 点）。
//! - **无 JSON**：密钥容器为定长二进制格式，不涉及 JSON 大数红线。
//!
//! # 容器格式（85 字节定长）
//! ```text
//! MAGIC(8 "IOTDAQKP") || ver(1) || nonce(12) || ciphertext(32) || hmac(32)
//! ```
//! - `ciphertext = ed25519_seed XOR HKDF-Expand(enc_key, info = nonce, 32)`
//!   （每 nonce 独立密钥流，同机重装容器密文也不同）；
//! - `hmac = HMAC-SHA256(mac_key, MAGIC || ver || nonce || ciphertext)`，
//!   完整性 + 机器绑定双重防线（mac_key 同样由指纹派生，换机必然失配）。

use std::fmt;
use std::io;
use std::path::PathBuf;
use std::sync::OnceLock;

use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::error::DaemonError;

/// 机器码指纹长度：64 字符 hex（task 3 输出，与 signing.rs 的 mid 校验一致）。
const FINGERPRINT_HEX_LEN: usize = 64;

/// 容器魔数（域分隔，防把其他文件误当密钥容器加载）。
const MAGIC: &[u8; 8] = b"IOTDAQKP";

/// 容器格式版本。
const CONTAINER_VERSION: u8 = 1;

/// 容器总长：8 + 1 + 12 + 32 + 32。
const CONTAINER_LEN: usize = 8 + 1 + 12 + 32 + 32;

/// Ed25519 种子长度（字节）。
const SEED_LEN: usize = 32;

/// 容器 nonce 长度（字节）。
const NONCE_LEN: usize = 12;

/// HKDF salt（域分隔；指纹本身即 IKM，salt 固定即可满足域隔离）。
const HKDF_SALT: &[u8] = b"iotdaq.keyprovider.v1.hkdf-salt";

/// HKDF info：加密密钥流派生域。
const HKDF_INFO_ENC: &[u8] = b"iotdaq.keyprovider.v1.enc-key";

/// HKDF info：完整性 MAC 密钥派生域。
const HKDF_INFO_MAC: &[u8] = b"iotdaq.keyprovider.v1.mac-key";

/// 公钥类型别名（公钥可自由导出 / 上传，与私钥红线区分）。
pub type PublicKey = VerifyingKey;

/// 密钥托管错误（零 panic：所有可失败点收敛于此）。
#[derive(Debug, thiserror::Error)]
pub enum KeyError {
    /// 指纹不是 task 3 输出格式（64 字符 hex）。
    #[error("key provider fingerprint must be {len}-char hex (machine_id output), got len {got}")]
    BadFingerprint {
        /// 要求长度（64）。
        len: usize,
        /// 实际长度。
        got: usize,
    },

    /// 容器完整性校验失败：被篡改，或从另一台机器拷贝而来（指纹失配）。
    ///
    /// 策略为 fail-closed：**不静默重建**，是否重置密钥由上层决定（防克隆）。
    #[error(
        "key file failed integrity/machine-binding check (tampered or copied from \
         another machine); refusing service — manual reset by operator required"
    )]
    TamperedOrForeign,

    /// 容器结构损坏（长度 / 魔数 / 版本非法）。
    #[error("key file corrupt: {0}")]
    Corrupt(String),

    /// 文件 IO 失败（读取 / 创建 / 写入 / 权限）。
    #[error("key file io: {0}")]
    Io(#[from] io::Error),
}

impl From<KeyError> for DaemonError {
    fn from(err: KeyError) -> Self {
        match err {
            KeyError::BadFingerprint { .. } => DaemonError::ConfigError(err.to_string()),
            KeyError::TamperedOrForeign | KeyError::Corrupt(_) => {
                DaemonError::SecurityError(err.to_string())
            }
            KeyError::Io(_) => DaemonError::StorageError(err.to_string()),
        }
    }
}

/// 手写 HMAC-SHA256 内核（RFC 2104）。
///
/// 不用 `hmac` crate 的原因：其 `new_from_slice` 返回 `Result`，为满足零 panic
/// 红线需引入不可达分支的兜底 panic；RFC 2104 全流程无失败点，就地实现更干净。
fn hmac_sha256(key: &[u8], data: &[u8]) -> [u8; 32] {
    const BLOCK: usize = 64;
    let mut block_key = [0u8; BLOCK];
    if key.len() > BLOCK {
        block_key[..32].copy_from_slice(&Sha256::digest(key));
    } else {
        block_key[..key.len()].copy_from_slice(key);
    }

    let mut ipad = [0x36u8; BLOCK];
    let mut opad = [0x5cu8; BLOCK];
    for (i, k) in block_key.iter().enumerate() {
        ipad[i] ^= k;
        opad[i] ^= k;
    }

    let mut inner = Sha256::new();
    inner.update(&ipad);
    inner.update(data);
    let inner_digest = inner.finalize();

    let mut outer = Sha256::new();
    outer.update(&opad);
    outer.update(inner_digest);
    outer.finalize().into()
}

/// HKDF-Extract（RFC 5869 §2.2）：`PRK = HMAC-Hash(salt, IKM)`。
fn hkdf_extract(salt: &[u8], ikm: &[u8]) -> [u8; 32] {
    hmac_sha256(salt, ikm)
}

/// HKDF-Expand（RFC 5869 §2.3）：`OKM = T(1) || T(2) || ...` 截断到 `out_len`。
fn hkdf_expand(prk: &[u8; 32], info: &[u8], out_len: usize) -> Vec<u8> {
    let mut okm: Vec<u8> = Vec::with_capacity(out_len.max(32));
    let mut prev: Vec<u8> = Vec::new();
    let mut counter: u8 = 1;
    while okm.len() < out_len {
        let mut input = prev.clone();
        input.extend_from_slice(info);
        input.push(counter);
        let tag = hmac_sha256(prk, &input);
        prev = tag.to_vec();
        okm.extend_from_slice(&tag);
        counter = counter.wrapping_add(1);
    }
    okm.truncate(out_len);
    okm
}

/// 校验指纹为 64 字符 hex（task 3 输出格式），失败返回 [`KeyError::BadFingerprint`]。
fn validate_fingerprint(fingerprint_hex: &str) -> Result<String, KeyError> {
    let fp = fingerprint_hex.trim();
    if fp.len() != FINGERPRINT_HEX_LEN || !fp.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(KeyError::BadFingerprint {
            len: FINGERPRINT_HEX_LEN,
            got: fp.len(),
        });
    }
    Ok(fp.to_string())
}

/// 由机器码指纹派生容器密钥（enc_key + mac_key，域分隔）。
///
/// 与 `trial.rs` 同源思路：指纹 hex 文本作 IKM，HKDF 两步派生；
/// 换机（指纹变化）⇒ 密钥变化 ⇒ 容器 MAC 必然失配 ⇒ 拒绝服务。
fn derive_container_keys(fingerprint_hex: &str) -> Result<ContainerKeys, KeyError> {
    let fp = validate_fingerprint(fingerprint_hex)?;
    let prk = hkdf_extract(HKDF_SALT, fp.as_bytes());
    let enc: [u8; 32] = hkdf_expand(&prk, HKDF_INFO_ENC, 32)
        .as_slice()
        .try_into()
        .map_err(|_| KeyError::Corrupt("hkdf enc key length mismatch".to_string()))?;
    let mac: [u8; 32] = hkdf_expand(&prk, HKDF_INFO_MAC, 32)
        .as_slice()
        .try_into()
        .map_err(|_| KeyError::Corrupt("hkdf mac key length mismatch".to_string()))?;
    Ok(ContainerKeys { enc, mac })
}

/// 容器密钥对（加密 + 完整性）。
struct ContainerKeys {
    /// 加密密钥流根（HKDF-Expand 的 PRK 角色，info = nonce）。
    enc: [u8; 32],
    /// 完整性 MAC 密钥。
    mac: [u8; 32],
}

/// 生成 Ed25519 种子（32 字节）。
///
/// 熵源：两枚 UUID v4（每枚 122 bit 随机，uuid crate 内部走 OS CSPRNG，
/// 无需引入 rand 依赖），串联后 SHA-256 摊平固定版本位 → 244 bit 熵。
fn generate_seed() -> [u8; SEED_LEN] {
    let a = Uuid::new_v4().into_bytes();
    let b = Uuid::new_v4().into_bytes();
    let mut hasher = Sha256::new();
    hasher.update(a);
    hasher.update(b);
    hasher.finalize().into()
}

/// 生成容器 nonce（12 字节，取自一枚 UUID v4）。
fn generate_nonce() -> [u8; NONCE_LEN] {
    let bytes = Uuid::new_v4().into_bytes();
    let mut nonce = [0u8; NONCE_LEN];
    nonce.copy_from_slice(&bytes[..NONCE_LEN]);
    nonce
}

/// 密封：种子 → 容器（加密 + MAC）。
///
/// 纯函数（nonce 由调用方注入），便于测试确定性与密文不泄密。
fn seal_container(seed: &[u8; SEED_LEN], nonce: &[u8; NONCE_LEN], keys: &ContainerKeys) -> [u8; CONTAINER_LEN] {
    let mut out = [0u8; CONTAINER_LEN];
    out[..MAGIC.len()].copy_from_slice(MAGIC);
    out[MAGIC.len()] = CONTAINER_VERSION;
    let nonce_off = MAGIC.len() + 1;
    out[nonce_off..nonce_off + NONCE_LEN].copy_from_slice(nonce);

    // 密钥流：HKDF-Expand(enc_key, info = nonce, 32)——每 nonce 独立。
    let pad = hkdf_expand(&keys.enc, nonce, SEED_LEN);
    let ct_off = nonce_off + NONCE_LEN;
    for (i, byte) in seed.iter().enumerate() {
        out[ct_off + i] = byte ^ pad[i];
    }

    let tag_off = ct_off + SEED_LEN;
    let tag = hmac_sha256(&keys.mac, &out[..tag_off]);
    out[tag_off..tag_off + 32].copy_from_slice(&tag);
    out
}

/// 开封：容器 → 种子。任何完整性 / 机器绑定失败都拒绝（fail-closed）。
fn unseal_container(bytes: &[u8], keys: &ContainerKeys) -> Result<[u8; SEED_LEN], KeyError> {
    if bytes.len() != CONTAINER_LEN {
        return Err(KeyError::Corrupt(format!(
            "container length {} != {CONTAINER_LEN}",
            bytes.len()
        )));
    }
    if &bytes[..MAGIC.len()] != MAGIC {
        return Err(KeyError::Corrupt("bad magic bytes".to_string()));
    }
    if bytes[MAGIC.len()] != CONTAINER_VERSION {
        return Err(KeyError::Corrupt(format!(
            "unsupported container version {}",
            bytes[MAGIC.len()]
        )));
    }

    let nonce_off = MAGIC.len() + 1;
    let ct_off = nonce_off + NONCE_LEN;
    let tag_off = ct_off + SEED_LEN;
    let nonce: [u8; NONCE_LEN] = bytes[nonce_off..ct_off]
        .try_into()
        .map_err(|_| KeyError::Corrupt("nonce slice length mismatch".to_string()))?;
    let expected_tag = hmac_sha256(&keys.mac, &bytes[..tag_off]);
    if !constant_time_eq(&bytes[tag_off..tag_off + 32], &expected_tag) {
        // 覆盖两类威胁：容器被篡改；或从另一台机器拷贝（mac_key 不同）。
        return Err(KeyError::TamperedOrForeign);
    }

    let pad = hkdf_expand(&keys.enc, &nonce, SEED_LEN);
    let mut seed = [0u8; SEED_LEN];
    for (i, byte) in bytes[ct_off..tag_off].iter().enumerate() {
        seed[i] = byte ^ pad[i];
    }
    Ok(seed)
}

/// 常数时间字节比较（MAC 校验防时序侧信道）。
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

/// 密钥托管 trait：`load_or_create` 产出 in-memory 密钥句柄。
///
/// 契约：实现方不得把私钥明文落盘 / 写日志；句柄不导出原始字节。
pub trait KeyProvider: Send + Sync {
    /// 加载既有密钥或首次创建并落盘，返回内存态句柄。
    ///
    /// # Errors
    /// 指纹非法（[`KeyError::BadFingerprint`]）、容器损坏 /
    /// 被篡改 / 异机拷贝（[`KeyError::TamperedOrForeign`] /
    /// [`KeyError::Corrupt`]）、文件 IO 失败（[`KeyError::Io`]）时返回错误，
    /// 不 panic、不静默重建。
    fn load_or_create(&self) -> Result<KeyHandle, KeyError>;
}

/// in-memory 密钥句柄：私钥常驻内存（记忆化），多次签名不重复解密。
///
/// **红线**：本类型不提供任何导出原始字节的方法（无 `to_bytes` / `as_bytes`，
/// 编译期保证）；Debug 输出脱敏，永不泄私钥。公钥经 [`KeyHandle::public_key`]
/// 自由导出。
pub struct KeyHandle {
    /// 私钥 OnceLock 缓存：构造时填充一次，此后 sign / verify 直接复用。
    key: OnceLock<SigningKey>,
}

impl fmt::Debug for KeyHandle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("KeyHandle { key: <redacted> }")
    }
}

impl KeyHandle {
    /// 由种子构造（crate 内部使用；外部只能经 [`KeyProvider`] 获得）。
    fn from_seed(seed: &[u8; SEED_LEN]) -> Self {
        let key = OnceLock::new();
        let _ = key.set(SigningKey::from_bytes(seed));
        Self { key }
    }

    /// 对消息签名。
    ///
    /// # Errors
    /// 句柄未初始化（构造路径保证不会发生）时返回 `SecurityError`，不 panic。
    pub fn sign(&self, msg: &[u8]) -> Result<Signature, KeyError> {
        let key = self.key.get().ok_or_else(|| {
            KeyError::Corrupt("key handle not initialized (internal invariant broken)".to_string())
        })?;
        Ok(key.sign(msg))
    }

    /// 验签（布尔语义，便于调用方直接判定）。
    pub fn verify(&self, msg: &[u8], sig: &Signature) -> bool {
        match self.key.get() {
            Some(key) => key.verifying_key().verify(msg, sig).is_ok(),
            None => false,
        }
    }

    /// 导出公钥（公钥红线外：可自由上传 / 登记）。
    ///
    /// # Errors
    /// 句柄未初始化时返回 `Corrupt`（构造路径保证不会发生），不 panic。
    pub fn public_key(&self) -> Result<PublicKey, KeyError> {
        self.key
            .get()
            .map(|key| key.verifying_key())
            .ok_or_else(|| KeyError::Corrupt("key handle not initialized".to_string()))
    }
}

/// 文件密钥托管（V1 主实现）：受限权限文件 + HKDF 机器码派生加密。
///
/// 路径与指纹均由调用方注入：测试注入临时路径与假指纹；
/// 生产由调用方传 task 3 `machine_id` 模块产物（本模块不接线、不采集指纹）。
#[derive(Debug)]
pub struct FileKeyProvider {
    /// 密钥容器文件路径。
    path: PathBuf,
    /// 机器码指纹（64 hex，task 3 输出）。
    fingerprint_hex: String,
}

impl FileKeyProvider {
    /// 构造文件密钥托管器。
    ///
    /// # Errors
    /// 指纹非 64 字符 hex 时返回 [`KeyError::BadFingerprint`]（构造期早失败）。
    pub fn new(path: PathBuf, machine_fingerprint: &str) -> Result<Self, KeyError> {
        Ok(Self {
            path,
            fingerprint_hex: validate_fingerprint(machine_fingerprint)?,
        })
    }

    /// 当前绑定（派生用）指纹。
    pub fn fingerprint(&self) -> &str {
        &self.fingerprint_hex
    }

    /// 写入容器文件（`create_new` 防覆盖；Unix 上收紧为 0600）。
    fn write_container(&self, container: &[u8; CONTAINER_LEN]) -> Result<(), KeyError> {
        use std::io::Write as _;
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&self.path)?;
        file.write_all(container)?;
        file.sync_all()?;

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(&self.path, std::fs::Permissions::from_mode(0o600))?;
        }
        // cfg(not(unix))：Windows ACL 从简（依赖用户 profile 默认 ACL），
        // 精细 ACL 留给平台差异任务（见模块头声明）。
        Ok(())
    }
}

impl KeyProvider for FileKeyProvider {
    fn load_or_create(&self) -> Result<KeyHandle, KeyError> {
        let keys = derive_container_keys(&self.fingerprint_hex)?;
        match std::fs::read(&self.path) {
            Ok(bytes) => {
                // 已有容器：开封复用同一密钥。任何失败都拒绝服务，绝不重建。
                let seed = unseal_container(&bytes, &keys)?;
                Ok(KeyHandle::from_seed(&seed))
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                // 首次启动：生成密钥对，加密落盘。
                let seed = generate_seed();
                let nonce = generate_nonce();
                let container = seal_container(&seed, &nonce, &keys);
                self.write_container(&container)?;
                Ok(KeyHandle::from_seed(&seed))
            }
            Err(e) => Err(KeyError::Io(e)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    use tempfile::TempDir;

    /// test-only 假指纹 A（64 hex），禁止用于真实部署。
    fn fingerprint_a() -> String {
        "a1".repeat(32)
    }

    /// test-only 假指纹 B（模拟另一台机器），禁止用于真实部署。
    fn fingerprint_b() -> String {
        "b2".repeat(32)
    }

    /// 在临时目录中构造 provider（路径注入 + 假指纹注入）。
    fn provider_at(dir: &TempDir, name: &str, fingerprint: &str) -> FileKeyProvider {
        FileKeyProvider::new(dir.path().join(name), fingerprint).expect("64-hex test fingerprint")
    }

    /// QA Happy：首次启动创建容器 —— 文件存在、定长、魔数正确，
    /// 且**不含明文私钥**（全文任一 32 字节窗口均 ≠ 种子原文）。
    #[test]
    fn first_run_creates_container_without_plaintext_seed() {
        let dir = TempDir::new().expect("tempdir");
        let path = dir.path().join("gw.key");
        let provider = FileKeyProvider::new(path.clone(), &fingerprint_a()).expect("valid fp");
        let handle = provider.load_or_create().expect("first run must create");

        let bytes = fs::read(&path).expect("container must exist on disk");
        assert_eq!(bytes.len(), CONTAINER_LEN, "fixed-size binary container");
        assert_eq!(&bytes[..MAGIC.len()], MAGIC, "magic prefix");

        // 测试内经 unseal 复原种子原文，再断言其不出现在容器任何字节窗口中。
        let keys = derive_container_keys(&fingerprint_a()).expect("valid fp");
        let seed = unseal_container(&bytes, &keys).expect("fresh container must unseal");
        assert!(
            !bytes.windows(SEED_LEN).any(|w| w == seed),
            "plaintext seed must never appear in container bytes"
        );
        assert!(handle.public_key().is_ok(), "fresh handle has public key");
    }

    /// QA Happy：重启（新实例 load）复用**同一密钥** —— 公钥一致、旧签名可验。
    #[test]
    fn reload_reuses_same_key_across_instances() {
        let dir = TempDir::new().expect("tempdir");
        let provider = provider_at(&dir, "gw.key", &fingerprint_a());

        let first = provider.load_or_create().expect("create");
        let pk_first = first.public_key().expect("public key");
        let msg = b"iotdaq reload probe";
        let sig = first.sign(msg).expect("sign before restart");

        drop(first);
        // 模拟重启：全新 provider / 全新 handle，同一文件同一指纹。
        let second = provider_at(&dir, "gw.key", &fingerprint_a())
            .load_or_create()
            .expect("reload");
        assert_eq!(
            second.public_key().expect("public key"),
            pk_first,
            "reloaded instance must reuse the same key"
        );
        assert!(
            second.verify(msg, &sig),
            "old signature must verify under reloaded key"
        );
    }

    /// QA Happy：签名 → 自验通过（句柄内公钥）。
    #[test]
    fn sign_then_verify_roundtrip() {
        let dir = TempDir::new().expect("tempdir");
        let handle = provider_at(&dir, "gw.key", &fingerprint_a())
            .load_or_create()
            .expect("handle");

        let msg = b"iotdaq sign probe";
        let sig = handle.sign(msg).expect("sign");
        assert!(handle.verify(msg, &sig), "own signature must verify");
    }

    /// QA Error：篡改消息 / 换外签 → verify 返回 false（不 panic）。
    #[test]
    fn verify_rejects_tampered_message_and_foreign_signature() {
        let dir = TempDir::new().expect("tempdir");
        let handle = provider_at(&dir, "gw.key", &fingerprint_a())
            .load_or_create()
            .expect("handle");
        let msg = b"iotdaq verify probe";
        let sig = handle.sign(msg).expect("sign");

        assert!(!handle.verify(b"iotdaq verify profX", &sig), "msg tamper");
        assert!(!handle.verify(msg, &sig) == false, "sanity: own sig ok");

        let foreign = provider_at(&dir, "other.key", &fingerprint_b())
            .load_or_create()
            .expect("handle b");
        let foreign_sig = foreign.sign(msg).expect("sign b");
        assert!(
            !handle.verify(msg, &foreign_sig),
            "foreign signature must fail"
        );
    }

    /// QA Error：容器密文或 MAC 被篡改 → 解密拒绝（TamperedOrForeign）。
    #[test]
    fn tampered_container_is_rejected() {
        let dir = TempDir::new().expect("tempdir");
        let path = dir.path().join("gw.key");
        let provider = FileKeyProvider::new(path.clone(), &fingerprint_a()).expect("valid fp");
        provider.load_or_create().expect("create");
        let original = fs::read(&path).expect("read container");

        // 篡改密文区一个字节（不触碰 MAC）。
        let mut ct_flip = original.clone();
        let ct_off = MAGIC.len() + 1 + NONCE_LEN;
        ct_flip[ct_off] ^= 0x01;
        fs::write(&path, &ct_flip).expect("rewrite");
        let err = provider_at(&dir, "gw.key", &fingerprint_a())
            .load_or_create()
            .expect_err("tampered ciphertext must be rejected");
        assert!(matches!(err, KeyError::TamperedOrForeign), "got {err}");

        // 篡改 MAC 区一个字节。
        fs::write(&path, &original).expect("restore");
        let mut mac_flip = original.clone();
        let tag_off = CONTAINER_LEN - 32;
        mac_flip[tag_off] ^= 0x80;
        fs::write(&path, &mac_flip).expect("rewrite");
        let err = provider_at(&dir, "gw.key", &fingerprint_a())
            .load_or_create()
            .expect_err("tampered mac must be rejected");
        assert!(matches!(err, KeyError::TamperedOrForeign), "got {err}");
    }

    /// QA Error：截断 / 魔数 / 版本非法 → 结构性损坏（Corrupt），不 panic。
    #[test]
    fn truncated_or_bad_magic_container_is_rejected() {
        let dir = TempDir::new().expect("tempdir");
        let path = dir.path().join("gw.key");
        let provider = FileKeyProvider::new(path.clone(), &fingerprint_a()).expect("valid fp");
        provider.load_or_create().expect("create");
        let original = fs::read(&path).expect("read container");

        // 截断。
        fs::write(&path, &original[..original.len() - 1]).expect("truncate");
        let err = provider_at(&dir, "gw.key", &fingerprint_a())
            .load_or_create()
            .expect_err("truncated container must be rejected");
        assert!(matches!(err, KeyError::Corrupt(_)), "got {err}");

        // 魔数错。
        let mut bad_magic = original.clone();
        bad_magic[0] = b'X';
        fs::write(&path, &bad_magic).expect("rewrite");
        let err = provider_at(&dir, "gw.key", &fingerprint_a())
            .load_or_create()
            .expect_err("bad magic must be rejected");
        assert!(matches!(err, KeyError::Corrupt(_)), "got {err}");

        // 版本错。
        let mut bad_version = original;
        bad_version[MAGIC.len()] = 0xFF;
        fs::write(&path, &bad_version).expect("rewrite");
        let err = provider_at(&dir, "gw.key", &fingerprint_a())
            .load_or_create()
            .expect_err("bad version must be rejected");
        assert!(matches!(err, KeyError::Corrupt(_)), "got {err}");
    }

    /// QA 防克隆：同一容器文件换指纹（模拟拷到另一台机器）→ 解密拒绝，
    /// 且**不静默重建**（文件字节保持不变、错误类型为 TamperedOrForeign）。
    #[test]
    fn copied_container_rejected_on_other_machine_without_recreate() {
        let dir = TempDir::new().expect("tempdir");
        let path = dir.path().join("gw.key");
        FileKeyProvider::new(path.clone(), &fingerprint_a())
            .expect("valid fp")
            .load_or_create()
            .expect("create on machine A");
        let before = fs::read(&path).expect("read container");

        // 「另一台机器」：同一路径注入指纹 B（测试假指纹 = 机器指纹注入点）。
        let err = FileKeyProvider::new(path.clone(), &fingerprint_b())
            .expect("valid fp")
            .load_or_create()
            .expect_err("copied container must be rejected");
        assert!(
            matches!(err, KeyError::TamperedOrForeign),
            "machine mismatch must surface as TamperedOrForeign, got {err}"
        );
        assert_eq!(
            fs::read(&path).expect("read after failed load"),
            before,
            "fail-closed: file must NOT be silently recreated"
        );
    }

    /// 密封纯函数：同 nonce 确定性；密文 ≠ 种子原文（加密生效）。
    #[test]
    fn seal_is_deterministic_per_nonce_and_hides_seed() {
        let keys = derive_container_keys(&fingerprint_a()).expect("valid fp");
        let seed: [u8; 32] = core::array::from_fn(|i| i as u8);
        let nonce: [u8; 12] = core::array::from_fn(|i| (i as u8) ^ 0x5A);

        let first = seal_container(&seed, &nonce, &keys);
        let second = seal_container(&seed, &nonce, &keys);
        assert_eq!(first, second, "same nonce => deterministic container");

        let ct_off = MAGIC.len() + 1 + NONCE_LEN;
        assert_ne!(
            &first[ct_off..ct_off + SEED_LEN],
            &seed[..],
            "ciphertext must differ from plaintext seed"
        );

        // 换 nonce（同机重装）→ 密文区不同（每 nonce 独立密钥流）。
        let mut other_nonce = nonce;
        other_nonce[0] ^= 0x01;
        let other = seal_container(&seed, &other_nonce, &keys);
        assert_ne!(
            &first[ct_off..ct_off + SEED_LEN],
            &other[ct_off..ct_off + SEED_LEN],
            "nonce change must re-randomize ciphertext"
        );
    }

    /// 红线（公钥侧）：公钥可自由导出（32 字节），跨实例一致；
    /// 私钥侧无字节导出 API（编译期保证，见 KeyHandle 文档——类型上不存在
    /// `to_bytes` / `as_bytes` 方法，仅 sign / verify / public_key 三个入口）。
    #[test]
    fn public_key_is_freely_exportable() {
        let dir = TempDir::new().expect("tempdir");
        let handle = provider_at(&dir, "gw.key", &fingerprint_a())
            .load_or_create()
            .expect("handle");

        let pk = handle.public_key().expect("public key");
        assert_eq!(pk.to_bytes().len(), 32, "ed25519 public key is 32 bytes");

        let reloaded = provider_at(&dir, "gw.key", &fingerprint_a())
            .load_or_create()
            .expect("reload");
        assert_eq!(
            reloaded.public_key().expect("public key"),
            pk,
            "public key stable across loads"
        );
    }

    /// 记忆化：句柄内私钥常驻（OnceLock）——文件删除后签名依旧可用，
    /// 证明签名路径不回读磁盘、不重复解密。
    #[test]
    fn memoized_handle_survives_file_deletion() {
        let dir = TempDir::new().expect("tempdir");
        let path = dir.path().join("gw.key");
        let handle = FileKeyProvider::new(path.clone(), &fingerprint_a())
            .expect("valid fp")
            .load_or_create()
            .expect("handle");
        let pk = handle.public_key().expect("public key");

        fs::remove_file(&path).expect("remove container");
        let msg = b"iotdaq memo probe";
        let sig = handle.sign(msg).expect("sign after file removal");
        assert!(handle.verify(msg, &sig), "memoized verify");
        assert_eq!(
            handle.public_key().expect("public key"),
            pk,
            "memoized public key unchanged"
        );

        // 连续多次签名行为等价（同消息同签名，Ed25519 确定性）。
        let again = handle.sign(msg).expect("sign again");
        assert_eq!(sig, again, "deterministic signing with memoized key");
    }

    /// QA Error：指纹格式非法 → 构造期早失败（BadFingerprint），不 panic。
    #[test]
    fn bad_fingerprint_rejected_without_panic() {
        let dir = TempDir::new().expect("tempdir");
        let path = dir.path().join("gw.key");

        let err = FileKeyProvider::new(path.clone(), "abc")
            .expect_err("short fingerprint must be rejected");
        assert!(
            matches!(err, KeyError::BadFingerprint { len: 64, got: 3 }),
            "got {err}"
        );

        let err = FileKeyProvider::new(path.clone(), "")
            .expect_err("empty fingerprint must be rejected");
        assert!(matches!(err, KeyError::BadFingerprint { got: 0, .. }));

        // 长度对但含非 hex 字符。
        let bad = "g".repeat(64);
        let err = FileKeyProvider::new(path.clone(), &bad)
            .expect_err("non-hex fingerprint must be rejected");
        assert!(matches!(err, KeyError::BadFingerprint { got: 64, .. }));

        // 非法指纹不产生任何文件。
        assert!(!path.exists(), "no container may be written on bad input");
    }

    /// QA Error：路径不可写（父目录缺失）→ Io 错误，不 panic、不产生半成品。
    #[test]
    fn unwritable_path_yields_io_error_without_panic() {
        let dir = TempDir::new().expect("tempdir");
        let bad_path = dir.path().join("no_such_dir").join("gw.key");
        let provider = FileKeyProvider::new(bad_path.clone(), &fingerprint_a())
            .expect("valid fp");

        let err = provider
            .load_or_create()
            .expect_err("unwritable path must fail");
        assert!(matches!(err, KeyError::Io(_)), "got {err}");
        assert!(
            !bad_path.exists(),
            "no partial container may be left behind"
        );
    }

    /// 错误映射：KeyError → DaemonError（Security 7000 / Config 2000 / Storage）。
    #[test]
    fn key_error_maps_to_daemon_error() {
        let tampered: DaemonError = KeyError::TamperedOrForeign.into();
        assert!(matches!(tampered, DaemonError::SecurityError(_)));
        assert_eq!(tampered.error_code(), 7000);

        let bad_fp: DaemonError = KeyError::BadFingerprint { len: 64, got: 3 }.into();
        assert!(matches!(bad_fp, DaemonError::ConfigError(_)));
        assert_eq!(bad_fp.error_code(), 2000);

        let io_err: DaemonError =
            KeyError::Io(io::Error::new(io::ErrorKind::PermissionDenied, "denied")).into();
        assert!(matches!(io_err, DaemonError::StorageError(_)));
    }

    /// 红线（日志侧）：KeyHandle / FileKeyProvider 的 Debug 输出脱敏，
    /// 不含密钥材料。FileKeyProvider 仅含路径与指纹（非密钥，可记录）。
    #[test]
    fn debug_output_redacted() {
        let dir = TempDir::new().expect("tempdir");
        let handle = provider_at(&dir, "gw.key", &fingerprint_a())
            .load_or_create()
            .expect("handle");
        let rendered = format!("{handle:?}");
        assert_eq!(rendered, "KeyHandle { key: <redacted> }");

        let provider = provider_at(&dir, "gw.key", &fingerprint_a());
        let rendered = format!("{provider:?}");
        assert!(
            rendered.contains("gw.key"),
            "path is safe to log: {rendered}"
        );
    }

    /// 两次独立首次创建：nonce / 密文互不相同（加密非确定性泄漏），
    /// 且各自为独立密钥（公钥不同）。
    #[test]
    fn separate_creates_use_independent_nonces_and_keys() {
        let dir = TempDir::new().expect("tempdir");
        let handle_a = provider_at(&dir, "a.key", &fingerprint_a())
            .load_or_create()
            .expect("create a");
        let handle_b = provider_at(&dir, "b.key", &fingerprint_a())
            .load_or_create()
            .expect("create b");

        let bytes_a = fs::read(dir.path().join("a.key")).expect("read a");
        let bytes_b = fs::read(dir.path().join("b.key")).expect("read b");
        let nonce_off = MAGIC.len() + 1;
        assert_ne!(
            &bytes_a[nonce_off..nonce_off + NONCE_LEN],
            &bytes_b[nonce_off..nonce_off + NONCE_LEN],
            "fresh creates must use independent nonces"
        );
        assert_ne!(
            handle_a.public_key().expect("pk a"),
            handle_b.public_key().expect("pk b"),
            "independent creates must yield independent keys"
        );
    }
}

//! 敏感配置字段级加密（计划 task 34）。
//!
//! ## 方案（相对计划原文的偏差，已由主理人裁决）
//! 计划原文写「SQLCipher + HKDF」。SQLCipher 需 `libsqlcipher-sys`（系统 C 库构建），
//! **违反本仓库「纯 Rust 依赖栈」红线**（`AGENTS.md` 依赖红线 1）。故改为
//! **纯 Rust 应用层字段级加密**，安全目标与验收标准完全不变：
//! - 敏感配置（北向 MQTT 口令等）**加密落盘**，明文永不写入配置文件；
//! - 密钥 = `HKDF-SHA256(ikm = machine_code, salt, info = b"iot-daq config v1")`
//!   → 32 字节 AES-256 密钥；
//! - 拷盘到另一台机器 → machine_code 不同 → 派生密钥不同 → GCM tag 校验失败
//!   → [`ConfigCryptoError::DecryptFailed`]（**fail-closed**，不 panic、不返回空）。
//!
//! ## 信封格式
//! ```text
//! seal 输出 = MAGIC(8, b"IOTDAQEC") || VERSION(1) || NONCE(12) || ciphertext || GCM tag(16)
//! ```
//! - 头部 21 字节整体作为 AEAD 的 **AAD**（魔数 + 版本被认证，篡改头部即失败）；
//! - nonce 每次 seal 由 `ring::rand::SystemRandom` 生成，**绝不复用**；
//! - 落盘到 TOML 时的文本形态 = `enc:v1:<base64(信封)>`（见 [`SEALED_PREFIX`]）。
//!
//! ## 密钥材料
//! - `machine_code` = [`crate::auth::machine_id::MachineIdentity::get_machine_id`]
//!   的 hex 输出（硬件锚点 N-of-M 聚合指纹，一机一码）；
//! - `salt` **非秘密**，但必须**随机且稳定**（换 salt = 换密钥 = 旧密文解不开），
//!   持久化在 data_dir 下独立文件（[`SALT_FILE_NAME`]，见 [`ensure_salt_file`]）。

use std::path::Path;

use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine as _;
use hmac::{Hmac, Mac};
use ring::aead::{self, Aad, LessSafeKey, Nonce, UnboundKey};
use ring::rand::{SecureRandom, SystemRandom};
use sha2::Sha256;

use crate::error::DaemonError;

type HmacSha256 = Hmac<Sha256>;

/// 信封魔数（8 字节）：`IOTDAQEC` = iot-daq encrypted config。
pub const ENVELOPE_MAGIC: [u8; 8] = *b"IOTDAQEC";
/// 当前信封版本（未来换算法/换 KDF 时递增，旧版本走显式不支持错误）。
pub const ENVELOPE_VERSION: u8 = 1;
/// AES-GCM nonce 长度（12 字节）。
pub const NONCE_LEN: usize = 12;
/// AES-256-GCM 认证标签长度（16 字节）。
pub const TAG_LEN: usize = 16;
/// 信封头长度 = 魔数 8 + 版本 1 + nonce 12 = 21。
pub const HEADER_LEN: usize = ENVELOPE_MAGIC.len() + 1 + NONCE_LEN;
/// 最小合法信封长度 = 头 21 + 空明文的 tag 16。
pub const MIN_ENVELOPE_LEN: usize = HEADER_LEN + TAG_LEN;
/// HKDF info（域分隔：本密钥只用于「配置字段加密」这一用途）。
pub const HKDF_INFO: &[u8] = b"iot-daq config v1";
/// 派生密钥长度（AES-256 = 32 字节）。
pub const KEY_LEN: usize = 32;
/// salt 长度（HKDF-SHA256 建议 ≥ 哈希输出长度 = 32 字节）。
pub const SALT_LEN: usize = 32;
/// 加密值在配置文件中的文本前缀：`enc:v1:<base64(信封)>`。
pub const SEALED_PREFIX: &str = "enc:v1:";
/// salt 落盘文件名（位于 `gateway.data_dir` 下；非秘密，但需稳定）。
pub const SALT_FILE_NAME: &str = "config.salt";

/// 敏感配置加密域的结构化错误。
///
/// 设计纪律：**错误分支一律按变体匹配，禁止 `msg.contains(...)` 字符串判断**。
#[derive(Debug, thiserror::Error)]
pub enum ConfigCryptoError {
    /// 信封结构非法（空 / 截断 / 魔数不符）。
    #[error("ConfigCrypto: bad envelope: {reason}")]
    BadEnvelope { reason: &'static str },

    /// 信封版本不是本实现支持的版本。
    #[error("ConfigCrypto: unsupported envelope version: {found}")]
    UnsupportedVersion { found: u8 },

    /// 解密失败：密钥不对（跨机器）或密文/头部被篡改（GCM tag 校验失败）。
    #[error("ConfigCrypto: decryption failed (wrong key or tampered payload)")]
    DecryptFailed,

    /// 加密失败（AEAD 内部错误）。
    #[error("ConfigCrypto: encryption failed")]
    EncryptFailed,

    /// 密钥长度不是 AES-256 要求的 32 字节。
    #[error("ConfigCrypto: bad key length: {len} (expected {expected})")]
    BadKeyLength { len: usize, expected: usize },

    /// 机器码为空 → 无法派生密钥（fail-closed：绝不退化成固定密钥）。
    #[error("ConfigCrypto: machine_code is empty (refusing to derive config key)")]
    EmptyMachineCode,

    /// salt 非法（空 / 长度不足）。
    #[error("ConfigCrypto: bad salt length: {len} (expected {expected})")]
    BadSaltLength { len: usize, expected: usize },

    /// salt 文件不可读 / 内容非法 / 无法创建。
    #[error("ConfigCrypto: salt file {path}: {reason}")]
    SaltUnavailable { path: String, reason: String },

    /// CSPRNG 取 nonce / salt 失败。
    #[error("ConfigCrypto: system RNG unavailable")]
    RngUnavailable,

    /// HMAC-SHA256 初始化失败（HKDF 内部；正常路径不可达）。
    #[error("ConfigCrypto: hmac-sha256 init failed: {reason}")]
    HmacInit { reason: String },

    /// base64 解码失败（加密值被手工改坏）。
    #[error("ConfigCrypto: field `{field}` sealed value is not valid base64")]
    BadBase64 { field: &'static str },

    /// 解密后的明文不是合法 UTF-8。
    #[error("ConfigCrypto: field `{field}` sealed payload is not valid utf-8: {reason}")]
    NotUtf8 { field: &'static str, reason: String },

    /// **旧明文迁移路径**：字段有值但不是 `enc:v1:` 前缀 → 明确报错，
    /// 既不静默当明文用，也不静默丢弃（重新保存一次配置即完成迁移）。
    #[error(
        "ConfigCrypto: field `{field}` holds legacy plaintext — re-save the config \
         (with the config key installed) to migrate it to `enc:v1:`"
    )]
    LegacyPlaintext { field: &'static str },

    /// 配置值是加密形态，但进程内**没有**安装配置密钥（fail-closed）。
    #[error("ConfigCrypto: field `{field}` is sealed but no config key is installed")]
    SealedWithoutKey { field: &'static str },
}

impl From<ConfigCryptoError> for DaemonError {
    fn from(err: ConfigCryptoError) -> Self {
        match &err {
            // 旧明文 = 配置形态问题，走配置域（迁移指引在 Display 里）。
            ConfigCryptoError::LegacyPlaintext { .. } => DaemonError::ConfigError(err.to_string()),
            // salt 文件 = 本地存储域。
            ConfigCryptoError::SaltUnavailable { .. } => DaemonError::StorageError(err.to_string()),
            // 其余全部是密码学 / 安全域（篡改、跨机器、坏信封……）。
            _ => DaemonError::SecurityError(err.to_string()),
        }
    }
}

/// 敏感配置加解密器（AES-256-GCM，密钥由机器码经 HKDF-SHA256 派生）。
///
/// 手写 [`std::fmt::Debug`]：密钥**绝不**出现在日志 / 诊断输出中。
pub struct ConfigEncryptor {
    key: LessSafeKey,
}

impl std::fmt::Debug for ConfigEncryptor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ConfigEncryptor")
            .field("key", &"<redacted>")
            .finish()
    }
}

impl ConfigEncryptor {
    /// 由机器码 + salt 派生密钥构造（生产路径）。
    ///
    /// # Errors
    /// 机器码为空 → [`ConfigCryptoError::EmptyMachineCode`]；salt 非法 →
    /// [`ConfigCryptoError::BadSaltLength`]。
    pub fn from_machine_code(machine_code: &str, salt: &[u8]) -> Result<Self, ConfigCryptoError> {
        let key = derive_config_key(machine_code, salt)?;
        Self::from_key_bytes(&key)
    }

    /// 由 32 字节原始密钥构造（高级 / 测试路径）。
    ///
    /// # Errors
    /// 长度非 32 → [`ConfigCryptoError::BadKeyLength`]；ring 拒绝 → [`ConfigCryptoError::EncryptFailed`]。
    pub fn from_key_bytes(key: &[u8]) -> Result<Self, ConfigCryptoError> {
        if key.len() != KEY_LEN {
            return Err(ConfigCryptoError::BadKeyLength {
                len: key.len(),
                expected: KEY_LEN,
            });
        }
        let unbound = UnboundKey::new(&aead::AES_256_GCM, key)
            .map_err(|_| ConfigCryptoError::EncryptFailed)?;
        Ok(Self {
            key: LessSafeKey::new(unbound),
        })
    }

    /// 加密（seal）：输出 `MAGIC||VER||NONCE||ciphertext||tag`。
    ///
    /// nonce 每次随机生成，同一明文两次 seal 输出**不同**（见单测）。
    ///
    /// # Errors
    /// RNG 失败 → [`ConfigCryptoError::RngUnavailable`]；AEAD 失败 →
    /// [`ConfigCryptoError::EncryptFailed`]。
    pub fn seal(&self, plain: &[u8]) -> Result<Vec<u8>, ConfigCryptoError> {
        let mut nonce = [0u8; NONCE_LEN];
        SystemRandom::new()
            .fill(&mut nonce)
            .map_err(|_| ConfigCryptoError::RngUnavailable)?;

        let mut header = Vec::with_capacity(HEADER_LEN);
        header.extend_from_slice(&ENVELOPE_MAGIC);
        header.push(ENVELOPE_VERSION);
        header.extend_from_slice(&nonce);

        let mut payload = plain.to_vec();
        self.key
            .seal_in_place_append_tag(
                Nonce::assume_unique_for_key(nonce),
                Aad::from(header.as_slice()),
                &mut payload,
            )
            .map_err(|_| ConfigCryptoError::EncryptFailed)?;

        let mut envelope = header;
        envelope.extend_from_slice(&payload);
        Ok(envelope)
    }

    /// 解密（open）：**fail-closed**——魔数 / 版本 / 长度 / GCM tag 任一不符
    /// 均返回结构化错误，**绝不** panic、**绝不**静默返回空明文。
    ///
    /// # Errors
    /// - 长度不足 / 魔数不符 → [`ConfigCryptoError::BadEnvelope`]；
    /// - 版本不符 → [`ConfigCryptoError::UnsupportedVersion`]；
    /// - 密钥错误（跨机器）或数据被篡改 → [`ConfigCryptoError::DecryptFailed`]。
    pub fn open(&self, blob: &[u8]) -> Result<Vec<u8>, ConfigCryptoError> {
        if blob.len() < MIN_ENVELOPE_LEN {
            return Err(ConfigCryptoError::BadEnvelope {
                reason: "truncated",
            });
        }
        if blob[..ENVELOPE_MAGIC.len()] != ENVELOPE_MAGIC {
            return Err(ConfigCryptoError::BadEnvelope {
                reason: "bad magic",
            });
        }
        let version = blob[ENVELOPE_MAGIC.len()];
        if version != ENVELOPE_VERSION {
            return Err(ConfigCryptoError::UnsupportedVersion { found: version });
        }
        let mut nonce = [0u8; NONCE_LEN];
        nonce.copy_from_slice(&blob[ENVELOPE_MAGIC.len() + 1..HEADER_LEN]);

        let mut payload = blob[HEADER_LEN..].to_vec();
        let plain = self
            .key
            .open_in_place(
                Nonce::assume_unique_for_key(nonce),
                Aad::from(&blob[..HEADER_LEN]),
                &mut payload,
            )
            .map_err(|_| ConfigCryptoError::DecryptFailed)?;
        Ok(plain.to_vec())
    }

    /// 加密一个配置字段值 → `enc:v1:<base64>` 文本（可直接写进 TOML）。
    ///
    /// # Errors
    /// 见 [`Self::seal`]。
    pub fn seal_str(&self, plain: &str) -> Result<String, ConfigCryptoError> {
        let envelope = self.seal(plain.as_bytes())?;
        Ok(format!("{SEALED_PREFIX}{}", BASE64.encode(envelope)))
    }

    /// 解密一个配置字段值（文本 → 明文）。
    ///
    /// 无 [`SEALED_PREFIX`] 前缀的**非空**值 = 旧明文 → 返回
    /// [`ConfigCryptoError::LegacyPlaintext`]（明确迁移路径，**不**静默当明文用）。
    ///
    /// # Errors
    /// 见 [`Self::open`] 与变体文档。
    pub fn open_str(&self, stored: &str, field: &'static str) -> Result<String, ConfigCryptoError> {
        let encoded = stored
            .strip_prefix(SEALED_PREFIX)
            .ok_or(ConfigCryptoError::LegacyPlaintext { field })?;
        let envelope = BASE64
            .decode(encoded)
            .map_err(|_| ConfigCryptoError::BadBase64 { field })?;
        let plain = self.open(&envelope)?;
        String::from_utf8(plain).map_err(|e| ConfigCryptoError::NotUtf8 {
            field,
            reason: e.to_string(),
        })
    }
}

/// 值是否为加密形态（`enc:v1:` 前缀）——**不需要密钥**即可判定，供加载路径
/// 在无密钥时快速 fail-closed。
pub fn is_sealed(value: &str) -> bool {
    value.starts_with(SEALED_PREFIX)
}

/// HKDF-SHA256（RFC 5869：extract-then-expand）——纯 Rust，基于 `hmac` + `sha2`
/// （两者均已是 daemon 直接依赖；`hkdf` crate 不在离线缓存内，故就地实现并
/// 用 RFC 5869 测试向量锁定正确性）。
///
/// `out` 长度上限 255 × 32 = 8160 字节（单字节 counter 的 HKDF 上限）。
///
/// # Errors
/// HMAC 初始化失败 → [`ConfigCryptoError::HmacInit`]；`out.len()` 超限 →
/// [`ConfigCryptoError::BadKeyLength`]（复用：长度类错误）。
pub fn hkdf_sha256(
    ikm: &[u8],
    salt: &[u8],
    info: &[u8],
    out: &mut [u8],
) -> Result<(), ConfigCryptoError> {
    const HASH_LEN: usize = 32;
    let max_out = 255 * HASH_LEN;
    if out.is_empty() || out.len() > max_out {
        return Err(ConfigCryptoError::BadKeyLength {
            len: out.len(),
            expected: max_out,
        });
    }

    // extract: prk = HMAC-SHA256(key = salt, msg = ikm)
    let prk = hmac_sha256(salt, &[ikm])?;

    // expand: T(0) = b""; T(i) = HMAC(prk, T(i-1) || info || [i])
    let mut block: Vec<u8> = Vec::new();
    let mut filled = 0usize;
    let mut counter: u8 = 1;
    while filled < out.len() {
        let mut mac =
            HmacSha256::new_from_slice(&prk).map_err(|e| ConfigCryptoError::HmacInit {
                reason: e.to_string(),
            })?;
        mac.update(&block);
        mac.update(info);
        mac.update(&[counter]);
        block = mac.finalize().into_bytes().to_vec();

        let take = block.len().min(out.len() - filled);
        out[filled..filled + take].copy_from_slice(&block[..take]);
        filled += take;
        counter += 1;
    }
    Ok(())
}

/// HMAC-SHA256(多段输入拼接)：HKDF 的基元。
fn hmac_sha256(key: &[u8], parts: &[&[u8]]) -> Result<[u8; 32], ConfigCryptoError> {
    let mut mac = HmacSha256::new_from_slice(key).map_err(|e| ConfigCryptoError::HmacInit {
        reason: e.to_string(),
    })?;
    for part in parts {
        mac.update(part);
    }
    let bytes = mac.finalize().into_bytes();
    let mut out = [0u8; 32];
    out.copy_from_slice(&bytes);
    Ok(out)
}

/// 由机器码派生配置加密密钥（AES-256 的 32 字节）。
///
/// ```text
/// key = HKDF-SHA256(ikm = machine_code.trim(), salt, info = b"iot-daq config v1")
/// ```
///
/// # Errors
/// 机器码为空 → [`ConfigCryptoError::EmptyMachineCode`]；salt 为空 →
/// [`ConfigCryptoError::BadSaltLength`]；HKDF 内部错误原样透出。
pub fn derive_config_key(
    machine_code: &str,
    salt: &[u8],
) -> Result<[u8; KEY_LEN], ConfigCryptoError> {
    let ikm = machine_code.trim().as_bytes();
    if ikm.is_empty() {
        return Err(ConfigCryptoError::EmptyMachineCode);
    }
    if salt.is_empty() {
        return Err(ConfigCryptoError::BadSaltLength {
            len: salt.len(),
            expected: SALT_LEN,
        });
    }
    let mut key = [0u8; KEY_LEN];
    hkdf_sha256(ikm, salt, HKDF_INFO, &mut key)?;
    Ok(key)
}

/// 生成 32 字节随机 salt（CSPRNG）。
///
/// # Errors
/// RNG 失败 → [`ConfigCryptoError::RngUnavailable`]。
pub fn generate_salt() -> Result<[u8; SALT_LEN], ConfigCryptoError> {
    let mut salt = [0u8; SALT_LEN];
    SystemRandom::new()
        .fill(&mut salt)
        .map_err(|_| ConfigCryptoError::RngUnavailable)?;
    Ok(salt)
}

/// 读取（缺失则创建并持久化）salt：落在 `path`（通常为
/// `<data_dir>/config.salt`），hex 文本。
///
/// - salt **非秘密**，但必须**稳定**：换 salt = 换密钥 = 既有密文全部解不开；
/// - 文件权限收敛为 `0600`（仅 unix；Windows 侧继承目录 ACL，不改）。
///
/// # Errors
/// 目录不可建 / 文件不可读 / 内容非 32 字节 hex →
/// [`ConfigCryptoError::SaltUnavailable`]（fail-closed，绝不静默换 salt）。
pub fn ensure_salt_file(path: &Path) -> Result<[u8; SALT_LEN], ConfigCryptoError> {
    let fail = |reason: String| ConfigCryptoError::SaltUnavailable {
        path: path.display().to_string(),
        reason,
    };

    if path.exists() {
        let raw = std::fs::read_to_string(path).map_err(|e| fail(format!("read: {e}")))?;
        let decoded = hex::decode(raw.trim()).map_err(|e| fail(format!("not hex: {e}")))?;
        if decoded.len() != SALT_LEN {
            return Err(fail(format!(
                "bad length {} (expected {SALT_LEN} bytes hex)",
                decoded.len()
            )));
        }
        let mut salt = [0u8; SALT_LEN];
        salt.copy_from_slice(&decoded);
        return Ok(salt);
    }

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| fail(format!("create dir: {e}")))?;
    }
    let salt = generate_salt()?;
    std::fs::write(path, format!("{}\n", hex::encode(salt)))
        .map_err(|e| fail(format!("write: {e}")))?;
    tighten_salt_permissions(path);
    Ok(salt)
}

/// 收敛 salt 文件权限：unix 下 `0600`（属主读写）。失败**不**阻断
/// （权限收紧是加固项，salt 本身非秘密；绝不因此让启动失败）。
#[cfg(unix)]
fn tighten_salt_permissions(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let perms = std::fs::Permissions::from_mode(0o600);
    let _ = std::fs::set_permissions(path, perms);
}

/// Windows 侧无 POSIX mode 概念：salt 文件沿用所在目录 ACL（data_dir 已按部署
/// 收紧），此处保持空实现以维持跨平台接口一致。
#[cfg(not(unix))]
fn tighten_salt_permissions(_path: &Path) {}

#[cfg(test)]
mod tests {
    use super::*;

    /// test-only 机器码 A（与任何真实机器无关）。
    const TEST_ONLY_MACHINE_A: &str =
        "a1b2c3d4e5f60718293a4b5c6d7e8f90a1b2c3d4e5f60718293a4b5c6d7e8f90";
    /// test-only 机器码 B（模拟「拷盘到另一台机器」）。
    const TEST_ONLY_MACHINE_B: &str =
        "0f0e0d0c0b0a090807060504030201000f0e0d0c0b0a09080706050403020100";
    /// test-only salt（固定值，保证单测可重现）。
    const TEST_ONLY_SALT: [u8; SALT_LEN] = [0x5a; SALT_LEN];
    /// test-only 明文口令（仅单测使用）。
    const TEST_ONLY_PASSWORD: &str = "TEST_ONLY-s3cr3t-password";

    /// 构造测试用加密器（机器码 A + 固定 salt）。
    fn encryptor_a() -> ConfigEncryptor {
        ConfigEncryptor::from_machine_code(TEST_ONLY_MACHINE_A, &TEST_ONLY_SALT)
            .expect("derive key from test machine A")
    }

    /// QA: RFC 5869 Test Case 1（SHA-256）向量——锁定 HKDF 实现正确性。
    #[test]
    fn hkdf_matches_rfc5869_test_vector_1() {
        let ikm = [0x0bu8; 22];
        let salt = [
            0x00u8, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0a, 0x0b, 0x0c,
        ];
        let info = [0xf0u8, 0xf1, 0xf2, 0xf3, 0xf4, 0xf5, 0xf6, 0xf7, 0xf8, 0xf9];
        let mut okm = [0u8; 42];
        hkdf_sha256(&ikm, &salt, &info, &mut okm).expect("hkdf expand");
        assert_eq!(
            hex::encode(okm),
            "3cb25f25faacd57a90434f64d0362f2a2d2d0a90cf1a5a4c5db02d56ecc4c5bf\
             34007208d5b887185865",
            "HKDF-SHA256 must match RFC 5869 TC1"
        );
    }

    /// QA Happy: seal → open 往返，明文一致；且信封头正确、明文不出现在密文里。
    #[test]
    fn seal_open_roundtrip() {
        let enc = encryptor_a();
        let blob = enc.seal(TEST_ONLY_PASSWORD.as_bytes()).expect("seal");

        assert_eq!(
            &blob[..ENVELOPE_MAGIC.len()],
            &ENVELOPE_MAGIC,
            "envelope must start with magic"
        );
        assert_eq!(blob[ENVELOPE_MAGIC.len()], ENVELOPE_VERSION);
        assert_eq!(blob.len(), HEADER_LEN + TEST_ONLY_PASSWORD.len() + TAG_LEN);
        assert!(
            !contains_subslice(&blob, TEST_ONLY_PASSWORD.as_bytes()),
            "ciphertext must not contain plaintext"
        );

        let plain = enc.open(&blob).expect("open with same key");
        assert_eq!(plain, TEST_ONLY_PASSWORD.as_bytes());
    }

    /// QA: 同一明文两次 seal 输出不同（nonce 随机，绝不复用）。
    #[test]
    fn seal_uses_fresh_nonce_each_call() {
        let enc = encryptor_a();
        let first = enc.seal(b"same-plaintext").expect("seal 1");
        let second = enc.seal(b"same-plaintext").expect("seal 2");
        assert_ne!(first, second, "fresh nonce must change the envelope");
        assert_eq!(
            enc.open(&first).expect("open 1"),
            enc.open(&second).expect("open 2")
        );
    }

    /// QA **验收标准**：跨机器失效——机器码 A 加密、机器码 B 解密必须**失败**
    /// （返回结构化 `DecryptFailed`，既不 panic 也不返回空）。
    #[test]
    fn cross_machine_decryption_fails() {
        let enc_a = encryptor_a();
        let enc_b = ConfigEncryptor::from_machine_code(TEST_ONLY_MACHINE_B, &TEST_ONLY_SALT)
            .expect("derive key from test machine B");

        // 两台机器派生出的密钥确实不同。
        let key_a = derive_config_key(TEST_ONLY_MACHINE_A, &TEST_ONLY_SALT).expect("key a");
        let key_b = derive_config_key(TEST_ONLY_MACHINE_B, &TEST_ONLY_SALT).expect("key b");
        assert_ne!(key_a, key_b, "different machine → different key");

        let blob = enc_a
            .seal(TEST_ONLY_PASSWORD.as_bytes())
            .expect("seal on A");
        let err = enc_b.open(&blob).expect_err("must fail on machine B");
        assert!(
            matches!(err, ConfigCryptoError::DecryptFailed),
            "cross-machine open must be DecryptFailed, got {err:?}"
        );
        // 同机仍可解开（证明失败只因密钥不同，而非数据损坏）。
        assert_eq!(
            enc_a.open(&blob).expect("open on A"),
            TEST_ONLY_PASSWORD.as_bytes()
        );
    }

    /// QA: 跨机器失效在**字段文本层**同样成立（`seal_str` / `open_str`）。
    #[test]
    fn cross_machine_decryption_fails_for_str_fields() {
        let enc_a = encryptor_a();
        let enc_b = ConfigEncryptor::from_machine_code(TEST_ONLY_MACHINE_B, &TEST_ONLY_SALT)
            .expect("derive key from test machine B");

        let sealed = enc_a.seal_str(TEST_ONLY_PASSWORD).expect("seal_str on A");
        assert!(is_sealed(&sealed), "sealed value must carry `enc:v1:`");
        assert!(
            !sealed.contains(TEST_ONLY_PASSWORD),
            "sealed text must not contain plaintext: {sealed}"
        );

        let err = enc_b
            .open_str(&sealed, "outlets.password")
            .expect_err("must fail on machine B");
        assert!(
            matches!(err, ConfigCryptoError::DecryptFailed),
            "got {err:?}"
        );
        assert_eq!(
            enc_a
                .open_str(&sealed, "outlets.password")
                .expect("open on A"),
            TEST_ONLY_PASSWORD
        );
    }

    /// QA 篡改: 翻转密文任一字节 → GCM tag 校验失败。
    #[test]
    fn tampered_ciphertext_fails() {
        let enc = encryptor_a();
        let blob = enc.seal(TEST_ONLY_PASSWORD.as_bytes()).expect("seal");
        for idx in [HEADER_LEN, HEADER_LEN + 1, blob.len() - 1] {
            let mut broken = blob.clone();
            broken[idx] ^= 0x01;
            let err = enc.open(&broken);
            assert!(err.is_err(), "tampering at {idx} must fail (GCM tag)");
        }
        // 逐字节翻转全部失败（含 tag 区）。
        let mut failures = 0usize;
        for idx in 0..blob.len() {
            let mut broken = blob.clone();
            broken[idx] ^= 0x80;
            if enc.open(&broken).is_err() {
                failures += 1;
            }
        }
        assert_eq!(
            failures,
            blob.len(),
            "every single-bit flip must be rejected by GCM"
        );
    }

    /// QA 坏信封: 空 / 截断 / 错魔数 / 错版本 → 结构化错误，不 panic。
    #[test]
    fn bad_envelopes_return_structured_errors() {
        let enc = encryptor_a();
        let blob = enc.seal(b"payload").expect("seal");

        let err = enc.open(&[]).expect_err("empty must fail");
        assert!(
            matches!(
                err,
                ConfigCryptoError::BadEnvelope {
                    reason: "truncated"
                }
            ),
            "got {err:?}"
        );

        let err = enc.open(&blob[..MIN_ENVELOPE_LEN - 1]).expect_err("short");
        assert!(
            matches!(
                err,
                ConfigCryptoError::BadEnvelope {
                    reason: "truncated"
                }
            ),
            "got {err:?}"
        );

        let mut bad_magic = blob.clone();
        bad_magic[0] = b'X';
        let err = enc.open(&bad_magic).expect_err("bad magic");
        assert!(
            matches!(
                err,
                ConfigCryptoError::BadEnvelope {
                    reason: "bad magic"
                }
            ),
            "got {err:?}"
        );

        let mut bad_version = blob.clone();
        bad_version[ENVELOPE_MAGIC.len()] = ENVELOPE_VERSION + 1;
        let err = enc.open(&bad_version).expect_err("bad version");
        assert!(
            matches!(err, ConfigCryptoError::UnsupportedVersion { found } if found == ENVELOPE_VERSION + 1),
            "got {err:?}"
        );

        // 头部被认证：改 nonce 区同样失败（AAD 绑定）。
        let mut bad_nonce = blob.clone();
        bad_nonce[ENVELOPE_MAGIC.len() + 1] ^= 0xff;
        assert!(
            enc.open(&bad_nonce).is_err(),
            "nonce is authenticated via AAD"
        );
    }

    /// QA 旧明文迁移路径: 无 `enc:v1:` 前缀 → `LegacyPlaintext`（明确报错，
    /// 既不静默当明文用，也不静默丢弃）。
    #[test]
    fn legacy_plaintext_value_is_rejected_with_migration_hint() {
        let enc = encryptor_a();
        let err = enc
            .open_str("plain-old-password", "outlets.password")
            .expect_err("legacy plaintext must not be accepted");
        assert!(
            matches!(err, ConfigCryptoError::LegacyPlaintext { field } if field == "outlets.password"),
            "got {err:?}"
        );
        // 迁移：重新 seal 后即可正常解开。
        let sealed = enc.seal_str("plain-old-password").expect("migrate seal");
        assert_eq!(
            enc.open_str(&sealed, "outlets.password").expect("open"),
            "plain-old-password"
        );
    }

    /// QA: 坏 base64 / 空值前缀判定。
    #[test]
    fn malformed_sealed_value_is_rejected() {
        let enc = encryptor_a();
        let err = enc
            .open_str("enc:v1:!!!!not-base64!!!!", "outlets.password")
            .expect_err("bad base64");
        assert!(
            matches!(err, ConfigCryptoError::BadBase64 { .. }),
            "got {err:?}"
        );

        assert!(is_sealed("enc:v1:anything"));
        assert!(!is_sealed("plaintext"));
        assert!(!is_sealed(""));
    }

    /// QA: 密钥派生是确定性的、salt 相关的；空机器码 / 空 salt 一律拒绝。
    #[test]
    fn key_derivation_is_deterministic_and_fail_closed() {
        let k1 = derive_config_key(TEST_ONLY_MACHINE_A, &TEST_ONLY_SALT).expect("k1");
        let k2 = derive_config_key(TEST_ONLY_MACHINE_A, &TEST_ONLY_SALT).expect("k2");
        assert_eq!(k1, k2, "same machine + same salt → same key");

        let other_salt = [0x11u8; SALT_LEN];
        let k3 = derive_config_key(TEST_ONLY_MACHINE_A, &other_salt).expect("k3");
        assert_ne!(k1, k3, "different salt → different key");

        let err = derive_config_key("", &TEST_ONLY_SALT).expect_err("empty machine");
        assert!(
            matches!(err, ConfigCryptoError::EmptyMachineCode),
            "got {err:?}"
        );
        // 全空白机器码同样拒绝（trim 后为空）。
        assert!(derive_config_key("   ", &TEST_ONLY_SALT).is_err());

        let err = derive_config_key(TEST_ONLY_MACHINE_A, &[]).expect_err("empty salt");
        assert!(
            matches!(err, ConfigCryptoError::BadSaltLength { .. }),
            "got {err:?}"
        );

        // 密钥长度不是 32 字节 → BadKeyLength。
        let err = ConfigEncryptor::from_key_bytes(&[0u8; 16]).expect_err("short key");
        assert!(
            matches!(err, ConfigCryptoError::BadKeyLength { len: 16, .. }),
            "got {err:?}"
        );
    }

    /// QA: `Debug` 绝不泄露密钥（日志红线）。
    #[test]
    fn encryptor_debug_redacts_key() {
        let enc = encryptor_a();
        let dbg = format!("{enc:?}");
        assert!(dbg.contains("<redacted>"), "must redact: {dbg}");
        assert!(!dbg.contains("ConfigEncryptor { key: ["), "leaked: {dbg}");
    }

    /// QA: salt 文件首次创建、二次复用（内容不变）、内容非法时 fail-closed。
    #[test]
    fn salt_file_is_created_once_and_reused() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join(SALT_FILE_NAME);
        assert!(!path.exists());

        let first = ensure_salt_file(&path).expect("create salt");
        assert!(path.exists(), "salt file must be persisted");
        let second = ensure_salt_file(&path).expect("reuse salt");
        assert_eq!(first, second, "salt must be stable across calls");

        let raw = std::fs::read_to_string(&path).expect("read salt file");
        assert_eq!(raw.trim().len(), SALT_LEN * 2, "salt is hex-encoded");

        // 内容被改坏 → 明确失败（绝不静默换 salt）。
        std::fs::write(&path, "not-hex-at-all").expect("corrupt");
        let err = ensure_salt_file(&path).expect_err("corrupt salt must fail");
        assert!(
            matches!(err, ConfigCryptoError::SaltUnavailable { .. }),
            "got {err:?}"
        );
    }

    /// QA: 结构化错误按域收敛进 `DaemonError`（安全 / 配置 / 存储）。
    #[test]
    fn crypto_errors_map_into_daemon_error_domains() {
        use crate::error::{ERR_CONFIG, ERR_SECURITY, ERR_STORAGE};

        let security: DaemonError = ConfigCryptoError::DecryptFailed.into();
        assert_eq!(security.error_code(), ERR_SECURITY);

        let config: DaemonError = ConfigCryptoError::LegacyPlaintext { field: "f" }.into();
        assert_eq!(config.error_code(), ERR_CONFIG);

        let storage: DaemonError = ConfigCryptoError::SaltUnavailable {
            path: "p".to_string(),
            reason: "r".to_string(),
        }
        .into();
        assert_eq!(storage.error_code(), ERR_STORAGE);
    }

    /// 辅助：判断 `haystack` 是否包含 `needle` 子切片。
    fn contains_subslice(haystack: &[u8], needle: &[u8]) -> bool {
        if needle.is_empty() || needle.len() > haystack.len() {
            return false;
        }
        haystack
            .windows(needle.len())
            .any(|window| window == needle)
    }
}

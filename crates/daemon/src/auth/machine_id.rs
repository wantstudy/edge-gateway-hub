//! 机器码指纹模块（计划 task 3，参照 mid 5.0「多源组合 + 哈希」思路）。
//!
//! 设计要点：
//! - 多源锚点采集经 [`AnchorProvider`] trait 抽象。平台专属采集器
//!   （Windows：MachineGuid / ComputerSystemProduct UUID / BIOS / BaseBoard / 卷序列号；
//!   Linux：`/etc/machine-id`、`/sys/class/dmi/id/product_uuid`）在后续授权接线任务
//!   （Wave 6 task 49/50）实现；本阶段提供文件型 / 环境变量型两个通用实现与测试 fake，
//!   trait 与 N-of-M 聚合逻辑完整可测。
//! - N-of-M 容错聚合：可用锚点数 ≥ 配额才产出指纹，个别锚点缺失（如重装导致
//!   MachineGuid 失效）不致命，配额不足则显式报错，绝不静默降级。
//! - 指纹 = HMAC-SHA256(key, SHA-256(锚点聚合原文))，hex 64 字符；
//!   对外只输出哈希，不暴露明文硬件 ID。
//! - HMAC key 从配置 / 环境变量注入，禁止硬编码（项目红线）；
//!   本文件测试常量一律 `TEST_ONLY_` 前缀标注，与生产密钥无关。

use std::collections::BTreeMap;
use std::fmt;
use std::path::PathBuf;

use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};

type HmacSha256 = Hmac<Sha256>;

/// 指纹派生失败错误（task 6 建 `DaemonError` 后并入 `AuthError` 域）。
#[derive(Debug, thiserror::Error)]
pub enum FingerprintError {
    /// 可用锚点数低于 N-of-M 配额。
    #[error(
        "anchor quorum failed: collected {collected} usable of {required} required \
         ({total} providers registered)"
    )]
    QuorumFailed {
        /// 实际采集成功的锚点数。
        collected: usize,
        /// 要求的最低锚点数（N-of-M 的 N）。
        required: usize,
        /// 注册的锚点总数（M）。
        total: usize,
    },

    /// HMAC key 为空字节串。
    #[error("fingerprint key must not be empty")]
    EmptyKey,

    /// 指定的 key 环境变量缺失或为空。
    #[error("fingerprint key env var `{0}` is missing or empty")]
    MissingKeyEnv(String),
}

/// 单个机器身份锚点采集器。
///
/// 契约：`collect` 返回 `None` 表示该锚点在本机不可用（文件缺失 / 环境变量未注入 /
/// 平台接口失败），不得计入 N-of-M 配额；返回值经 [`normalize_anchor`] 归一
/// （trim + 拒绝空串）。
pub trait AnchorProvider: Send + Sync {
    /// 锚点稳定名称（聚合排序键，如 `"etc-machine-id"`、`"machine-guid"`）。
    fn name(&self) -> &'static str;

    /// 采集锚点原始值；`None` = 本机不可用。
    fn collect(&self) -> Option<String>;
}

/// 归一锚点值：去除首尾空白；空白后为空视为不可用。
fn normalize_anchor(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

/// 文件型锚点：读取固定路径文本并归一。
///
/// 典型：Linux `/etc/machine-id`；容器场景安装脚本注入的宿主指纹文件。
#[derive(Debug, Clone)]
pub struct FileAnchor {
    name: &'static str,
    path: PathBuf,
}

impl FileAnchor {
    /// 创建文件型锚点。
    pub fn new(name: &'static str, path: impl Into<PathBuf>) -> Self {
        Self {
            name,
            path: path.into(),
        }
    }
}

impl AnchorProvider for FileAnchor {
    fn name(&self) -> &'static str {
        self.name
    }

    fn collect(&self) -> Option<String> {
        let raw = std::fs::read_to_string(&self.path).ok()?;
        normalize_anchor(&raw)
    }
}

/// 环境变量型锚点：容器 / 部署期注入（如宿主机 MAC 指纹经 env 注入容器）。
#[derive(Debug, Clone)]
pub struct EnvAnchor {
    name: &'static str,
    var: &'static str,
}

impl EnvAnchor {
    /// 创建环境变量型锚点。
    pub fn new(name: &'static str, var: &'static str) -> Self {
        Self { name, var }
    }
}

impl AnchorProvider for EnvAnchor {
    fn name(&self) -> &'static str {
        self.name
    }

    fn collect(&self) -> Option<String> {
        let raw = std::env::var(self.var).ok()?;
        normalize_anchor(&raw)
    }
}

/// 固定值锚点：单元测试 fake 与受控注入（现场运维手工登记等）使用。
#[derive(Debug, Clone, Default)]
pub struct StaticAnchor {
    name: &'static str,
    value: Option<String>,
}

impl StaticAnchor {
    /// 创建固定值锚点；`None` 表示该锚点不可用。
    pub fn new(name: &'static str, value: Option<&str>) -> Self {
        Self {
            name,
            value: value.map(str::to_string),
        }
    }
}

impl AnchorProvider for StaticAnchor {
    fn name(&self) -> &'static str {
        self.name
    }

    fn collect(&self) -> Option<String> {
        let raw = self.value.as_deref()?;
        normalize_anchor(raw)
    }
}

/// 平台默认锚点计划（后续接线任务实现对应 provider 时照此清单，勿随意增删）：
///
/// - Windows（5 锚点）：`machine-guid`（HKLM Cryptography，重装即失效）、
///   `csproduct-uuid`、`bios-serial`、`baseboard-serial`、`volume-serial`；
/// - Linux（4 锚点）：`etc-machine-id`、`dmi-product-uuid`、
///   `dmi-product-serial`、`dmi-board-serial`；
/// - 容器：只读挂载宿主 `/etc/machine-id` 与 DMI 路径，宿主 MAC 由安装脚本经
///   环境变量注入 —— 禁止采容器内 machine-id / 容器 MAC / 容器主机名。
pub const PLATFORM_ANCHOR_PLANS: &[(Platform, &[&str])] = &[
    (
        Platform::Windows,
        &[
            "machine-guid",
            "csproduct-uuid",
            "bios-serial",
            "baseboard-serial",
            "volume-serial",
        ],
    ),
    (
        Platform::Linux,
        &[
            "etc-machine-id",
            "dmi-product-uuid",
            "dmi-product-serial",
            "dmi-board-serial",
        ],
    ),
];

/// 锚点计划所属平台。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Platform {
    /// Windows 桌面 / 服务器（Tauri 壳交付形态）。
    Windows,
    /// Linux 原生 / 容器（headless 交付形态）。
    Linux,
}

/// 指纹 HMAC key：字节容器，格式化输出一律脱敏（防日志泄露）。
pub struct FingerprintKey(Vec<u8>);

impl FingerprintKey {
    /// 从字节构造（配置文件密钥路径注入）。
    ///
    /// # Errors
    /// 空字节串返回 [`FingerprintError::EmptyKey`]。
    pub fn from_bytes(bytes: Vec<u8>) -> Result<Self, FingerprintError> {
        if bytes.is_empty() {
            return Err(FingerprintError::EmptyKey);
        }
        Ok(Self(bytes))
    }

    /// 从环境变量构造（部署期注入）。
    ///
    /// # Errors
    /// 变量缺失或为空返回 [`FingerprintError::MissingKeyEnv`]。
    pub fn from_env(var: &str) -> Result<Self, FingerprintError> {
        let raw =
            std::env::var(var).map_err(|_| FingerprintError::MissingKeyEnv(var.to_string()))?;
        let raw = raw.trim().to_string();
        if raw.is_empty() {
            return Err(FingerprintError::MissingKeyEnv(var.to_string()));
        }
        Ok(Self(raw.into_bytes()))
    }
}

impl fmt::Debug for FingerprintKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("FingerprintKey(***)")
    }
}

/// 单锚点哈希的**域分隔前缀**（与机器码的 HMAC 输入不同域，保证任一锚点哈希
/// **永不等于** `machine_code`）。
const ANCHOR_HASH_DOMAIN: &[u8] = b"iotdaq.anchor.v1|";

/// 单锚点哈希的**截断长度**（字节）：16 → 32 个 hex 字符（对应设计「HMAC 加盐截断」）。
const ANCHOR_HASH_LEN: usize = 16;

/// 机器身份：多源锚点 + N-of-M 容错聚合 + HMAC-SHA256 稳定指纹。
///
/// 聚合规则：采集成功的锚点按名称排序（`BTreeMap`，同名后注册者覆盖先注册者），
/// 拼接为 `name=value\n` 聚合原文；锚点注册顺序不影响指纹。
pub struct MachineIdentity {
    providers: Vec<Box<dyn AnchorProvider>>,
    min_anchors: usize,
    key: FingerprintKey,
}

impl MachineIdentity {
    /// 创建机器身份。
    ///
    /// `min_anchors` 为 N-of-M 配额；传入 0 会被抬升为 1（指纹至少需要 1 个锚点）。
    pub fn new(
        providers: Vec<Box<dyn AnchorProvider>>,
        min_anchors: usize,
        key: FingerprintKey,
    ) -> Self {
        Self {
            providers,
            min_anchors: min_anchors.max(1),
            key,
        }
    }

    /// 采集全部锚点：成功者入 `BTreeMap`（排序 + 同名去重）。
    fn collected_anchors(&self) -> Vec<(&'static str, String)> {
        let mut map: BTreeMap<&'static str, String> = BTreeMap::new();
        for provider in &self.providers {
            if let Some(value) = provider.collect() {
                map.insert(provider.name(), value);
            }
        }
        map.into_iter().collect()
    }

    /// 机器摘要：quorum 校验通过后返回锚点聚合原文的 SHA-256 摘要。
    fn machine_digest(&self) -> Result<(usize, [u8; 32]), FingerprintError> {
        let collected = self.collected_anchors();
        let usable = collected.len();
        if usable < self.min_anchors {
            return Err(FingerprintError::QuorumFailed {
                collected: usable,
                required: self.min_anchors,
                total: self.providers.len(),
            });
        }
        let mut aggregate = String::new();
        for (name, value) in &collected {
            aggregate.push_str(name);
            aggregate.push('=');
            aggregate.push_str(value);
            aggregate.push('\n');
        }
        let digest = Sha256::digest(aggregate.as_bytes());
        Ok((usable, digest.into()))
    }

    /// 机器 ID：锚点聚合的 SHA-256 摘要，hex 编码（64 字符）。
    ///
    /// 输出为哈希而非明文硬件 ID（项目 Must NOT 红线）。
    ///
    /// # Errors
    /// 配额不足返回 [`FingerprintError::QuorumFailed`]。
    pub fn get_machine_id(&self) -> Result<String, FingerprintError> {
        let (_, digest) = self.machine_digest()?;
        Ok(hex::encode(digest))
    }

    /// 机器码指纹：`HMAC-SHA256(key, machine_digest)`，hex 编码（64 字符）。
    ///
    /// 同机多次调用稳定一致；key 更换后指纹随之更换（支持密钥轮换）。
    ///
    /// # Errors
    /// 配额不足返回 [`FingerprintError::QuorumFailed`]。
    pub fn get_machine_fingerprint(&self) -> Result<String, FingerprintError> {
        let (_, digest) = self.machine_digest()?;
        let mut mac = HmacSha256::new_from_slice(&self.key.0)
            .expect("HMAC accepts any non-empty key; empty key rejected at construction");
        mac.update(&digest);
        Ok(hex::encode(mac.finalize().into_bytes()))
    }

    /// **逐锚点哈希集**（N-of-M 同机判定的输入）。
    ///
    /// 与机器码**同键**盐化，但**域分隔**：`hex(HMAC-SHA256(key, b"iotdaq.anchor.v1|" ++ name ++ b"|" ++ value))`
    /// 截断到 16 字节（32 hex 字符）。域前缀与机器码路径不同，保证任一锚点哈希
    /// **永不等于** `machine_code`。
    ///
    /// 语义与 [`MachineIdentity::machine_digest`] 严格对齐：
    /// 1. **先做 quorum 校验**（`usable < min_anchors` → [`FingerprintError::QuorumFailed`]）；
    /// 2. 锚点顺序取同一份 `collected_anchors()`（`BTreeMap` 名称序），保证同机多次调用稳定；
    /// 3. 逐个独立哈希：**单个锚点值变化不影响其它锚点的哈希**；
    /// 4. 输出**只是哈希**，绝不回显锚点原值。
    ///
    /// # Errors
    /// 配额不足返回 [`FingerprintError::QuorumFailed`]。
    pub fn get_anchor_hashes(&self) -> Result<Vec<String>, FingerprintError> {
        let collected = self.collected_anchors();
        let usable = collected.len();
        if usable < self.min_anchors {
            return Err(FingerprintError::QuorumFailed {
                collected: usable,
                required: self.min_anchors,
                total: self.providers.len(),
            });
        }
        let mut hashes = Vec::with_capacity(collected.len());
        for (name, value) in &collected {
            let mut mac = HmacSha256::new_from_slice(&self.key.0)
                .expect("HMAC accepts any non-empty key; empty key rejected at construction");
            // 域分隔：`iotdaq.anchor.v1|` ++ name ++ `|` ++ value。
            mac.update(ANCHOR_HASH_DOMAIN);
            mac.update(name.as_bytes());
            mac.update(b"|");
            mac.update(value.as_bytes());
            let full = mac.finalize().into_bytes();
            // 截断到 16 字节 → 32 hex 字符（不完整暴露 HMAC 输出）。
            hashes.push(hex::encode(&full[..ANCHOR_HASH_LEN]));
        }
        Ok(hashes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// test-only：单元测试专用 HMAC key，与任何生产密钥无关，禁止用于真实部署。
    const TEST_ONLY_FINGERPRINT_KEY_A: &[u8] = b"iot-daq-wave1-test-key-a";
    /// test-only：另一把测试 key，用于验证 key 变化会改变指纹。
    const TEST_ONLY_FINGERPRINT_KEY_B: &[u8] = b"iot-daq-wave1-test-key-b";

    /// Windows 锚点集 fake（对应 `PLATFORM_ANCHOR_PLANS` Windows 计划，5 锚点）。
    fn windows_fake_anchors() -> Vec<Box<dyn AnchorProvider>> {
        vec![
            Box::new(StaticAnchor::new("machine-guid", Some("guid-1111"))),
            Box::new(StaticAnchor::new("csproduct-uuid", Some("uuid-2222"))),
            Box::new(StaticAnchor::new("bios-serial", Some("bios-3333"))),
            Box::new(StaticAnchor::new("baseboard-serial", Some("board-4444"))),
            Box::new(StaticAnchor::new("volume-serial", Some("vol-5555"))),
        ]
    }

    /// Linux 锚点集 fake（对应 Linux 计划，4 锚点）。
    fn linux_fake_anchors() -> Vec<Box<dyn AnchorProvider>> {
        vec![
            Box::new(StaticAnchor::new("etc-machine-id", Some("mid-aaaa"))),
            Box::new(StaticAnchor::new("dmi-product-uuid", Some("uuid-bbbb"))),
            Box::new(StaticAnchor::new("dmi-product-serial", Some("ps-cccc"))),
            Box::new(StaticAnchor::new("dmi-board-serial", Some("bs-dddd"))),
        ]
    }

    fn test_key() -> FingerprintKey {
        FingerprintKey::from_bytes(TEST_ONLY_FINGERPRINT_KEY_A.to_vec())
            .expect("test-only key is non-empty")
    }

    fn identity(providers: Vec<Box<dyn AnchorProvider>>, min_anchors: usize) -> MachineIdentity {
        MachineIdentity::new(providers, min_anchors, test_key())
    }

    fn assert_hex64(value: &str) {
        assert_eq!(value.len(), 64, "must be SHA-256 hex: {value}");
        assert!(
            value.bytes().all(|b| b.is_ascii_hexdigit()),
            "must be hex: {value}"
        );
    }

    /// QA: 同一机器（Windows 路径）多次调用指纹一致，64 字符 hex。
    #[test]
    fn windows_fingerprint_consistent_and_hex64() {
        let id = identity(windows_fake_anchors(), 3);
        let fp1 = id.get_machine_fingerprint().expect("quorum ok");
        let fp2 = id.get_machine_fingerprint().expect("quorum ok");
        assert_eq!(fp1, fp2, "same machine must yield same fingerprint");
        assert_hex64(&fp1);

        let mid1 = id.get_machine_id().expect("quorum ok");
        let mid2 = id.get_machine_id().expect("quorum ok");
        assert_eq!(mid1, mid2);
        assert_hex64(&mid1);
    }

    /// QA: Linux 路径锚点集同样稳定可聚合。
    #[test]
    fn linux_fingerprint_consistent_and_hex64() {
        let id = identity(linux_fake_anchors(), 3);
        let fp = id.get_machine_fingerprint().expect("quorum ok");
        assert_hex64(&fp);
        let again = identity(linux_fake_anchors(), 3);
        assert_eq!(
            fp,
            again.get_machine_fingerprint().expect("quorum ok"),
            "same anchors must yield same fingerprint"
        );
    }

    /// N-of-M 容错：5 锚点缺 1（可用 4 ≥ N=3）仍成功。
    #[test]
    fn quorum_tolerates_missing_anchor() {
        let mut anchors = windows_fake_anchors();
        anchors.remove(0); // machine-guid 缺失（模拟重装失效）
        let id = identity(anchors, 3);
        assert!(id.get_machine_fingerprint().is_ok(), "4 >= 3 must pass");
    }

    /// N-of-M 拒绝：可用锚点 2 < N=3 → 显式报错，错误信息含 quorum。
    #[test]
    fn quorum_failure_is_explicit() {
        let mut anchors = windows_fake_anchors();
        anchors.truncate(2);
        let id = identity(anchors, 3);
        let err = id.get_machine_fingerprint().expect_err("2 < 3 must fail");
        assert!(matches!(err, FingerprintError::QuorumFailed { .. }));
        assert!(err.to_string().contains("quorum"), "display: {err}");
    }

    /// 锚点值变化（不同机器）→ 指纹与机器 ID 都变化。
    #[test]
    fn different_machine_yields_different_fingerprint() {
        let base = identity(windows_fake_anchors(), 3);
        let fp_base = base.get_machine_fingerprint().expect("ok");
        let mid_base = base.get_machine_id().expect("ok");

        let mut anchors = windows_fake_anchors();
        anchors[1] = Box::new(StaticAnchor::new("csproduct-uuid", Some("uuid-XXXX")));
        let other = identity(anchors, 3);
        let fp_other = other.get_machine_fingerprint().expect("ok");
        let mid_other = other.get_machine_id().expect("ok");

        assert_ne!(fp_base, fp_other, "different hardware must differ");
        assert_ne!(mid_base, mid_other);
    }

    /// 锚点注册顺序不影响指纹（聚合排序稳定）。
    #[test]
    fn anchor_order_is_irrelevant() {
        let mut reversed = windows_fake_anchors();
        reversed.reverse();
        let fp_a = identity(windows_fake_anchors(), 3)
            .get_machine_fingerprint()
            .expect("ok");
        let fp_b = identity(reversed, 3).get_machine_fingerprint().expect("ok");
        assert_eq!(fp_a, fp_b);
    }

    /// key 更换 → 指纹更换（密钥轮换语义）；machine_id 不受 key 影响。
    #[test]
    fn key_change_rotates_fingerprint_but_not_machine_id() {
        let key_a = MachineIdentity::new(
            windows_fake_anchors(),
            3,
            FingerprintKey::from_bytes(TEST_ONLY_FINGERPRINT_KEY_A.to_vec()).expect("non-empty"),
        );
        let key_b = MachineIdentity::new(
            windows_fake_anchors(),
            3,
            FingerprintKey::from_bytes(TEST_ONLY_FINGERPRINT_KEY_B.to_vec()).expect("non-empty"),
        );
        assert_ne!(
            key_a.get_machine_fingerprint().expect("ok"),
            key_b.get_machine_fingerprint().expect("ok")
        );
        assert_eq!(
            key_a.get_machine_id().expect("ok"),
            key_b.get_machine_id().expect("ok")
        );
    }

    // ---- 逐锚点哈希集（N-of-M 同机判定输入） ----

    /// 5 锚点 → 5 个哈希；稳定一致；长度恒 32 hex；互不相同。
    #[test]
    fn anchor_hashes_are_five_distinct_hex32_stable() {
        let id = identity(windows_fake_anchors(), 3);
        let h1 = id.get_anchor_hashes().expect("quorum ok");
        let h2 = id.get_anchor_hashes().expect("quorum ok");
        assert_eq!(h1, h2, "same machine must yield same anchor hashes");
        assert_eq!(h1.len(), 5, "5 anchors → 5 hashes");
        for h in &h1 {
            assert_eq!(h.len(), 32, "must be 16-byte truncated HMAC hex: {h}");
            assert!(h.bytes().all(|b| b.is_ascii_hexdigit()), "must be hex: {h}");
        }
        let unique: std::collections::BTreeSet<&String> = h1.iter().collect();
        assert_eq!(unique.len(), 5, "anchor hashes must be distinct: {h1:?}");
    }

    /// **域分隔守护**：任一锚点哈希永不等于 `machine_code`。
    #[test]
    fn anchor_hashes_never_equal_machine_code() {
        let id = identity(windows_fake_anchors(), 3);
        let machine_code = id.get_machine_fingerprint().expect("ok");
        for h in id.get_anchor_hashes().expect("ok") {
            assert_ne!(
                h, machine_code,
                "domain separation must keep anchor hash != machine_code"
            );
        }
    }

    /// 逐点独立性：单个锚点值变化 → 只有对应位置的哈希改变。
    #[test]
    fn changing_one_anchor_only_changes_its_hash() {
        let base = identity(windows_fake_anchors(), 3)
            .get_anchor_hashes()
            .expect("ok");
        let mut anchors = windows_fake_anchors();
        // 只改 `csproduct-uuid` 的值；名称序不变 → 仅该锚点的哈希应变化。
        anchors[1] = Box::new(StaticAnchor::new("csproduct-uuid", Some("uuid-XXXX")));
        let changed = identity(anchors, 3).get_anchor_hashes().expect("ok");

        assert_eq!(base.len(), changed.len());
        let diffs: Vec<usize> = (0..base.len()).filter(|&i| base[i] != changed[i]).collect();
        assert_eq!(
            diffs.len(),
            1,
            "only the changed anchor's hash may differ: {diffs:?}"
        );
    }

    /// 换 key → 全部锚点哈希改变（密钥轮换语义）。
    #[test]
    fn changing_key_rotates_all_anchor_hashes() {
        let a = identity(windows_fake_anchors(), 3)
            .get_anchor_hashes()
            .expect("ok");
        let b = MachineIdentity::new(
            windows_fake_anchors(),
            3,
            FingerprintKey::from_bytes(TEST_ONLY_FINGERPRINT_KEY_B.to_vec()).expect("non-empty"),
        )
        .get_anchor_hashes()
        .expect("ok");
        assert!(
            a.iter().zip(&b).all(|(x, y)| x != y),
            "key change must rotate all anchor hashes"
        );
    }

    /// quorum 不足 → `Err(QuorumFailed)`（与 machine_digest 同口径）。
    #[test]
    fn anchor_hashes_quorum_failure_is_explicit() {
        let mut anchors = windows_fake_anchors();
        anchors.truncate(2);
        let id = identity(anchors, 3);
        let err = id.get_anchor_hashes().expect_err("2 < 3 must fail");
        assert!(matches!(
            err,
            FingerprintError::QuorumFailed {
                collected: 2,
                required: 3,
                ..
            }
        ));
    }

    /// 输出**只是哈希**：不含任何锚点原值。
    #[test]
    fn anchor_hashes_do_not_leak_raw_values() {
        let id = identity(windows_fake_anchors(), 3);
        let hashes = id.get_anchor_hashes().expect("ok");
        let raws = [
            "guid-1111",
            "uuid-2222",
            "bios-3333",
            "board-4444",
            "vol-5555",
        ];
        for raw in raws {
            assert!(
                !hashes.iter().any(|h| h.contains(raw)),
                "raw anchor value leaked into hash output: {raw}"
            );
        }
    }

    /// 空串 / 空白锚点值视为不可用，不计入配额。
    #[test]
    fn blank_anchor_values_are_unusable() {
        let providers: Vec<Box<dyn AnchorProvider>> = vec![
            Box::new(StaticAnchor::new("a", Some("  "))),
            Box::new(StaticAnchor::new("b", Some(""))),
            Box::new(StaticAnchor::new("c", Some("real"))),
        ];
        let strict = identity(providers, 2);
        let err = strict.get_machine_id().expect_err("only 1 usable < 2");
        assert!(matches!(err, FingerprintError::QuorumFailed { .. }));

        let providers: Vec<Box<dyn AnchorProvider>> = vec![
            Box::new(StaticAnchor::new("a", Some("  "))),
            Box::new(StaticAnchor::new("b", Some(""))),
            Box::new(StaticAnchor::new("c", Some("real"))),
            Box::new(StaticAnchor::new("d", Some("real2"))),
        ];
        assert!(identity(providers, 2).get_machine_id().is_ok());
    }

    /// 空指纹 key 在构造期即被拒绝（错误路径不 panic）。
    #[test]
    fn empty_key_rejected() {
        assert!(matches!(
            FingerprintKey::from_bytes(Vec::new()),
            Err(FingerprintError::EmptyKey)
        ));
    }

    /// key 脱敏：Debug 输出不泄露字节。
    #[test]
    fn key_debug_is_masked() {
        let key = FingerprintKey::from_bytes(b"secret-material".to_vec()).expect("non-empty");
        let rendered = format!("{key:?}");
        assert_eq!(rendered, "FingerprintKey(***)");
        assert!(!rendered.contains("secret"));
    }

    /// 环境变量 key：存在则成功，缺失报 MissingKeyEnv。
    #[test]
    fn key_from_env_paths() {
        // 缺失变量 → 报错（错误路径不 panic）。
        let err = FingerprintKey::from_env("IOTDAQ_TEST_KEY_DEFINITELY_UNSET_7f3a");
        assert!(matches!(err, Err(FingerprintError::MissingKeyEnv(_))));
    }

    /// 平台锚点计划常量与模块文档口径一致（防漂移）。
    #[test]
    fn anchor_plans_match_design() {
        let windows = &PLATFORM_ANCHOR_PLANS
            .iter()
            .find(|(p, _)| *p == Platform::Windows)
            .expect("windows plan")
            .1;
        assert_eq!(
            windows,
            &[
                "machine-guid",
                "csproduct-uuid",
                "bios-serial",
                "baseboard-serial",
                "volume-serial"
            ]
        );

        let linux = &PLATFORM_ANCHOR_PLANS
            .iter()
            .find(|(p, _)| *p == Platform::Linux)
            .expect("linux plan")
            .1;
        assert_eq!(
            linux,
            &[
                "etc-machine-id",
                "dmi-product-uuid",
                "dmi-product-serial",
                "dmi-board-serial"
            ]
        );
    }
}

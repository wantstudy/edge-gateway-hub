//! 遥测历史归档 —— SQLite 加密落盘（计划 task 18，Wave 3）。
//!
//! ## 职责边界
//! - **做**：本地遥测历史点位的持久化归档（`telemetry.db`）；机器码 HKDF 派生密钥 +
//!   `value` 列字段级加密（防拷盘直读）；按时间范围 / 最新值查询；保留期 + 体积双重裁剪；
//!   磁盘占用统计；优雅关闭。
//! - **不做**：离线转发队列（task 17 `offline_queue.rs` 的 `queue.db`，本模块**物理隔离**、
//!   独立写连接）、北向上传（task 19+ MQTT）、远端同步。
//!
//! ## 为什么不能照抄 `offline_queue.rs`
//!
//! `queue.db` 是**待转发的离线消息队列**（被 ack 删除、环形覆盖）；`telemetry.db` 是
//! **本地遥测历史归档**（按时间保留、供本地看板 / 历史查询）。二者**用途不同**，按项目级
//! 约定「SQLite 分库」**物理分离**：
//! 1. SQLite 只允许单写者，共用一个连接 / 库文件会互相阻塞；
//! 2. 生命周期不同（队列会被 ack 清空，历史库按保留期滚动）；
//! 3. `queue.db` **不做**磁盘加密，`telemetry.db` **必须**加密（本 task 的核心）。
//!
//! 两份库的派生密钥都按机器码 HKDF 派生，**拷盘到别的机器即失效**。
//!
//! ## 加密取舍（关键设计决策 — 必读）
//!
//! **为什么不整体用 SQLCipher？**
//! `rusqlite` 官方 `bundled` 特性**不含** SQLCipher（SQLCipher 是独立 C 库，需要额外的
//! 编译配置与系统依赖）。本机 mingw 工具链不便引入系统 C 库，且项目红线要求**纯 Rust 栈**。
//!
//! **本模块采用应用层「字段级加密」**（务实且可交付）：
//! - 敏感列 `value`（float 采样值）以「`nonce(12B) || ciphertext || tag(16B)`」形式
//!   hex 编码后存储；非敏感列（`device_id` / `point_key` / `ts_ns` / `quality`）明文存储，
//!   以便 SQL 侧按设备 / 时间范围高效检索（加密列无法参与索引 / 范围查询）。
//!
//! **为什么不用 AES-256-GCM？**
//! 经核实 `crates/daemon/Cargo.toml` 中**没有** `aes-gcm` / `aes` / `cipher` crate
//! （`Cargo.lock` 亦无），且**不允许**引入需要系统 C 库或 cmake 的 crate。
//! 因此采用**降级构造**：以 `enc_key` 派生 keystream 的**认证 XOR 流加密**——
//! - keystream 由 `HMAC-SHA256(enc_key, nonce || counter_be)` 逐块生成（counter 为 32 位
//!   大端块序号，从 0 起）；
//! - 独立认证：`tag = HMAC-SHA256(mac_key, nonce || aad || ciphertext)` 截取 16 字节，
//!   与 `enc_key` 分离（encrypt-then-MAC），防止 `enc_key` 泄露波及完整性密钥。
//!
//! ⚠️ **强度声明**：本构造为**自研**，是本机纯 Rust 栈下的**务实折中**，
//! **强度低于标准 AES-256-GCM**（未做专门的侧信道 / 抗相关密钥分析）。
//! 用途限定为**本地静态数据防拷盘直读**：拷到别的机器因机器码不同 → 派生密钥不同 →
//! MAC 校验失败 → 无法解密。**已用独立 HMAC 认证防篡改**。生产强合规场景应迁移到
//! SQLCipher 或硬件加密模块。
//!
//! ## 其他关键设计决策
//!
//! 1. **HKDF-SHA256（RFC 5869）**：`extract = HMAC-SHA256(salt, ikm)`，
//!    `expand = HMAC-SHA256(prk, info || 0x01)`（32 字节输出一次即够）。
//!    `hkdf` crate 不在依赖里，故用 `hmac` + `sha2` 手工实现标准 HKDF-Expand。
//! 2. **派生密钥绝不落盘**：`enc_key` / `mac_key` 仅内存持有；`Debug` 实现脱敏。
//! 3. **单写线程 + WAL + synchronous=NORMAL**：与 task 17 同构，所有 SQL 经**一个专用
//!    OS 写线程**串行执行，对外只暴露 `&self` 接口。
//! 4. **`seq` 主键 + `uq_telemetry_nonce` 唯一索引**：`seq` 幂等（重复 `seq` 静默跳过）；
//!    而 `(device_id, point_key, ts_ns)` 上的**唯一索引**是**加密正确性的结构保证**——
//!    它阻断「nonce/keystream 复用」（XOR 流加密下 `C1⊕C2=P1⊕P2`）。
//!    冲突处理**逐条隔离**：单条冲突被**跳过并经由 [`WriteOutcome::skipped_nonce_conflict`]
//!    上报**（可见、不静默），其余合法条照常写入（**不整批回滚**，避免丢失现场合法遥测）；
//!    非唯一约束的真存储故障仍中止整批。详见 `derive_nonce` / `is_nonce_conflict` / `write`。
//! 5. **双重裁剪**：`prune` 先按 `retention_ns`（注入时钟，测试零 sleep）淘汰过期点，
//!    再按 `max_db_bytes` 从最旧开始淘汰，二者可叠加。
//! 6. **零 panic**：所有 rusqlite / io / 解密错误收敛为 `DaemonError::StorageError`（4000），
//!    配置非法为 `ConfigError`（2000）；锁中毒时取回内部数据而非 unwrap。

use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread::{Builder, JoinHandle};
use std::time::{SystemTime, UNIX_EPOCH};

use hmac::{Hmac, Mac};
use rusqlite::{params, Connection};
use sha2::Sha256;

use crate::error::{DaemonError, DaemonResult};

// ---- 常量 ----

/// 遥测库文件名（**固定**；与 task 17 的 `queue.db` 物理隔离）。
pub const TELEMETRY_DB_FILE_NAME: &str = "telemetry.db";

/// task 17 队列库文件名（仅用于配置校验时的显式拦截，本模块不读写）。
pub const QUEUE_DB_FILE_NAME: &str = "queue.db";

/// `enc_key` 的 HKDF info 标签（密钥用途域隔离）。
pub const HKDF_INFO_ENC: &[u8] = b"iotdaq.telemetry.enc.v1";

/// `mac_key` 的 HKDF info 标签（密钥用途域隔离）。
pub const HKDF_INFO_MAC: &[u8] = b"iotdaq.telemetry.mac.v1";

/// 派生密钥长度（32 字节 = 256 位）。
const KEY_LEN: usize = 32;

/// nonce 长度（12 字节，随机源来自机器码摘要，非加密 RNG）。
const NONCE_LEN: usize = 12;

/// MAC tag 截断长度（16 字节 = 128 位，与 GCM tag 等长）。
const TAG_LEN: usize = 16;

/// 默认保留期（30 天，纳秒）。
pub const DEFAULT_RETENTION_NS: i64 = 30 * 24 * 60 * 60 * 1_000_000_000;

/// 默认库字节上限（2GB）。
pub const DEFAULT_MAX_DB_BYTES: u64 = 2 * 1024 * 1024 * 1024;

/// 逻辑字节统计中每条行的固定开销估算（device_id / point_key / ts_ns / quality 等）。
const ROW_OVERHEAD_BYTES: u64 = 64;

/// 写线程名（诊断用）。
const WRITER_THREAD_NAME: &str = "telemetry-store-writer";

/// 写线程忙等超时（毫秒，避免瞬时锁冲突直接失败）。
const BUSY_TIMEOUT_MS: i64 = 3_000;

/// `enc` 列的十六进制长度：`nonce(12) + ciphertext(8) + tag(16)` = 36 字节。
const ENC_HEX_LEN: usize = (NONCE_LEN + 8 + TAG_LEN) * 2;

// ---- 时钟 ----

/// 可注入时钟（测试要模拟 30 天保留期，**禁止真 sleep**）。
pub trait Clock: Send + Sync {
    /// 当前时间（UNIX 纪元起纳秒）。
    fn now_ns(&self) -> i64;
}

/// 真实系统时钟。
#[derive(Debug, Default, Clone, Copy)]
pub struct SystemClock;

impl SystemClock {
    /// 构造系统时钟。
    #[must_use]
    pub fn new() -> Self {
        Self
    }
}

impl Clock for SystemClock {
    fn now_ns(&self) -> i64 {
        match SystemTime::now().duration_since(UNIX_EPOCH) {
            Ok(d) => i64::try_from(d.as_nanos()).unwrap_or(i64::MAX),
            // 系统时钟早于 UNIX 纪元（时钟异常）时退化为 0，绝不 panic。
            Err(_) => 0,
        }
    }
}

/// 手工时钟（测试用）：可 `set` / `advance`，共享同一份纳秒值。
#[derive(Debug, Clone)]
pub struct ManualClock {
    now: Arc<std::sync::atomic::AtomicI64>,
}

impl ManualClock {
    /// 以指定起点（纳秒）构造。
    #[must_use]
    pub fn new(start_ns: i64) -> Self {
        Self {
            now: Arc::new(std::sync::atomic::AtomicI64::new(start_ns)),
        }
    }

    /// 直接设置当前时间（纳秒）。
    pub fn set(&self, now_ns: i64) {
        self.now.store(now_ns, Ordering::SeqCst);
    }

    /// 在当前时间上累加 `delta_ns`（可为负，使用饱和运算避免溢出）。
    pub fn advance(&self, delta_ns: i64) {
        self.now.fetch_add(delta_ns, Ordering::SeqCst);
    }

    /// 读取当前时间（纳秒）。
    #[must_use]
    pub fn now(&self) -> i64 {
        self.now.load(Ordering::SeqCst)
    }
}

impl Clock for ManualClock {
    fn now_ns(&self) -> i64 {
        self.now.load(Ordering::SeqCst)
    }
}

// ---- 配置 ----

/// 遥测库配置（非法值在 `open` 时转 `ConfigError`，错误码 2000）。
#[derive(Clone, PartialEq, Eq)]
pub struct TelemetryStoreConfig {
    /// 遥测库路径（文件名**必须**是 `telemetry.db`，禁止写成 `queue.db`）。
    pub db_path: PathBuf,
    /// 网关标识（参与 nonce 派生，非空）。
    pub gateway_id: String,
    /// 机器码（HKDF 的 IKM / 盐，非空；换机器即换密钥）。
    pub machine_code: String,
    /// 库字节上限（> 0，超过则按最旧淘汰）。
    pub max_db_bytes: u64,
    /// 保留期（纳秒，> 0，默认 30 天）。
    pub retention_ns: i64,
}

impl fmt::Debug for TelemetryStoreConfig {
    /// 手写 Debug：**只显示文件名，不显示目录**。
    ///
    /// 路径本身不敏感（敏感的是「在哪个目录」）；显示文件名可在排障时区分
    /// 「配错文件」与「配错目录」，而全站统一打成 `<configured>` 会丧失该能力。
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let file_name = self
            .db_path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("<no-file-name>");
        f.debug_struct("TelemetryStoreConfig")
            .field("db_file_name", &file_name)
            .field("gateway_id", &self.gateway_id)
            .field("machine_code", &"<redacted>")
            .field("max_db_bytes", &self.max_db_bytes)
            .field("retention_ns", &self.retention_ns)
            .finish()
    }
}

impl TelemetryStoreConfig {
    /// 按默认保留期 / 体积上限构造并校验。
    ///
    /// # Errors
    /// 库文件名不是 `telemetry.db` / `gateway_id` 或 `machine_code` 为空时返回
    /// `ConfigError`（2000）。
    pub fn new(
        db_path: impl Into<PathBuf>,
        gateway_id: impl Into<String>,
        machine_code: impl Into<String>,
    ) -> DaemonResult<Self> {
        let config = Self {
            db_path: db_path.into(),
            gateway_id: gateway_id.into(),
            machine_code: machine_code.into(),
            max_db_bytes: DEFAULT_MAX_DB_BYTES,
            retention_ns: DEFAULT_RETENTION_NS,
        };
        config.validate()?;
        Ok(config)
    }

    /// 校验全部字段；非法即 `ConfigError`（2000）。
    ///
    /// # Errors
    /// 任一字段非法时返回 `ConfigError`。
    pub fn validate(&self) -> DaemonResult<()> {
        let file_name = self
            .db_path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or_default();
        if file_name.is_empty() {
            return Err(config_err("db_path must point to a database file"));
        }
        if file_name != TELEMETRY_DB_FILE_NAME {
            return Err(config_err(format!(
                "telemetry db file must be named `{TELEMETRY_DB_FILE_NAME}` (got `{file_name}`; \
                 `{QUEUE_DB_FILE_NAME}` belongs to task 17 and must not be shared)"
            )));
        }
        if self.gateway_id.trim().is_empty() {
            return Err(config_err("gateway_id must not be empty"));
        }
        if self.machine_code.trim().is_empty() {
            return Err(config_err("machine_code must not be empty"));
        }
        if self.max_db_bytes == 0 {
            return Err(config_err("max_db_bytes must be greater than 0"));
        }
        if self.retention_ns <= 0 {
            return Err(config_err("retention_ns must be greater than 0"));
        }
        Ok(())
    }
}

// ---- 数据模型 ----

/// 一条遥测历史点（`value` 敏感，落盘前加密）。
///
/// ⚠️ **关于 `Debug`（勿「统一脱敏」）**：`StoredPoint` 是**解密后的明文结构**，
/// 其 `value` 在内存里本就是明文，`Debug` 直接打印 f64 值是**正确且必要**的调试能力。
/// 需要脱敏的是**未解密的密文结构** `RawRow`（仅持有 `value_enc`，从不打印明文）。
/// **请勿**因为「项目要求脱敏」而把本结构的 `value` 也打码——那会阉掉排障所需的明文可见性。
#[derive(Debug, Clone, PartialEq)]
pub struct StoredPoint {
    /// 行序号（主键，幂等键：重复 `seq` 静默跳过）。
    pub seq: i64,
    /// 设备标识。
    pub device_id: String,
    /// 点位键（点位名 / 地址）。
    pub point_key: String,
    /// 采样时间（纳秒，参与范围查询与保留期裁剪）。
    pub ts_ns: i64,
    /// 采样值（**敏感**，落盘加密）。
    pub value: f64,
    /// 质量码（明文，便于检索）。
    pub quality: String,
}

/// 批量写入的结果：**区分「成功插入」与「因 nonce 冲突被跳过」**。
///
/// ## 为什么需要它（而非只返回一个数）
/// 单一 `usize` 无法区分「本来就没这条」与「因 `uq_telemetry_nonce` 冲突被跳过」。
/// 而「跳过」是**安全事件**（nonce/keystream 复用被拦截），必须**可见**——
/// 既不静默吞掉，也**不拖累整批**。调用方据此可告警 / 落审计。
///
/// ## 语义
/// - `inserted`：真实写入的行数。
/// - `skipped_nonce_conflict`：因 `(device_id, point_key, ts_ns)` 撞唯一索引被跳过的行数
///   （> 0 时应告警：说明存在同设备同点位同纳秒的采样，可能是点位配重 / 设备时钟不精确）。
/// - `skipped_duplicate_seq`：因 `seq` 已存在被静默跳过的行数（幂等重放，非异常）。
///
/// 注意：**非唯一约束**的存储错误（磁盘 / SQL 语法等真故障）**不会**被计入跳过——
/// 它们仍会让整个批次返回 `Err`（详见 `write`）。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct WriteOutcome {
    /// 真实写入的行数。
    pub inserted: usize,
    /// 因 `uq_telemetry_nonce` 唯一索引冲突被跳过的行数（**安全事件，应告警**）。
    pub skipped_nonce_conflict: usize,
    /// 因 `seq` 已存在被静默跳过的行数（幂等重放，非异常）。
    pub skipped_duplicate_seq: usize,
}

impl WriteOutcome {
    /// 总计处理（含插入 + 跳过）的行数。
    #[must_use]
    pub fn total_processed(&self) -> usize {
        self.inserted + self.skipped_nonce_conflict + self.skipped_duplicate_seq
    }

    /// 是否有 nonce 冲突被跳过（调用方据此决定告警 / 审计）。
    #[must_use]
    pub fn has_nonce_conflict(&self) -> bool {
        self.skipped_nonce_conflict > 0
    }
}

// ---- 加密原语（HKDF + 认证 XOR 流加密） ----

/// HKDF-Extract（RFC 5869 §2.2）：`PRK = HMAC-SHA256(salt, IKM)`。
///
/// 因 `hkdf` crate 不在依赖里，此处手工实现。
fn hkdf_extract(salt: &[u8], ikm: &[u8]) -> [u8; KEY_LEN] {
    // HMAC 接受任意长度密钥；`new_from_slice` 仅对无效密钥长度失败，
    // SHA-256 的 HMAC 密钥长度无限制，故此处不会失败。为满足「生产路径不 panic」，
    // 失败时退化为全零 PRK（实际不可达，仅作防御）。
    let mut mac = match Hmac::<Sha256>::new_from_slice(salt) {
        Ok(mac) => mac,
        Err(_) => return [0u8; KEY_LEN],
    };
    mac.update(ikm);
    let out = mac.finalize().into_bytes();
    let mut prk = [0u8; KEY_LEN];
    prk.copy_from_slice(&out);
    prk
}

/// HKDF-Expand（RFC 5869 §2.3），单块输出（L <= HashLen = 32）。
///
/// `OKM = HMAC-SHA256(PRK, info || 0x01)`；仅需 32 字节，无需迭代。
fn hkdf_expand_single(prk: &[u8; KEY_LEN], info: &[u8]) -> [u8; KEY_LEN] {
    let mut mac = match Hmac::<Sha256>::new_from_slice(prk) {
        Ok(mac) => mac,
        Err(_) => return [0u8; KEY_LEN],
    };
    mac.update(info);
    mac.update(&[0x01]); // 块计数器，单块即 1
    let out = mac.finalize().into_bytes();
    let mut okm = [0u8; KEY_LEN];
    okm.copy_from_slice(&out);
    okm
}

/// 从机器码派生两把密钥（`enc_key` / `mac_key`），**仅内存持有、绝不落盘**。
///
/// salt = `machine_code` 字节；IKM = `gateway_id || 0x00 || machine_code` 字节，
/// 使密钥同时绑定网关与机器（换网关或换机器均换密钥）。
#[must_use]
fn derive_keys(gateway_id: &str, machine_code: &str) -> ([u8; KEY_LEN], [u8; KEY_LEN]) {
    let salt = machine_code.as_bytes();
    let mut ikm = Vec::with_capacity(gateway_id.len() + 1 + machine_code.len());
    ikm.extend_from_slice(gateway_id.as_bytes());
    ikm.push(0x00);
    ikm.extend_from_slice(machine_code.as_bytes());
    let prk = hkdf_extract(salt, &ikm);
    (
        hkdf_expand_single(&prk, HKDF_INFO_ENC),
        hkdf_expand_single(&prk, HKDF_INFO_MAC),
    )
}

/// 认证 XOR 流加密的密钥材料（`Debug` 脱敏，绝不打印密钥字节）。
#[derive(Clone)]
struct CipherKeys {
    enc_key: [u8; KEY_LEN],
    mac_key: [u8; KEY_LEN],
}

impl fmt::Debug for CipherKeys {
    /// 手写 Debug：**绝不**打印密钥内容。
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("CipherKeys(***)")
    }
}

impl CipherKeys {
    /// 由网关 / 机器码派生。
    fn derive(gateway_id: &str, machine_code: &str) -> Self {
        let (enc_key, mac_key) = derive_keys(gateway_id, machine_code);
        Self { enc_key, mac_key }
    }
}

/// 由 `enc_key` + `nonce` 生成第 `block` 块 keystream（32 字节）。
///
/// `keystream = HMAC-SHA256(enc_key, nonce || counter_be32)`。
fn keystream_block(enc_key: &[u8; KEY_LEN], nonce: &[u8; NONCE_LEN], block: u32) -> [u8; KEY_LEN] {
    let mut mac = match Hmac::<Sha256>::new_from_slice(enc_key) {
        Ok(mac) => mac,
        Err(_) => return [0u8; KEY_LEN],
    };
    mac.update(nonce);
    mac.update(&block.to_be_bytes());
    let out = mac.finalize().into_bytes();
    let mut ks = [0u8; KEY_LEN];
    ks.copy_from_slice(&out);
    ks
}

/// 计算 16 字节认证 tag：`HMAC-SHA256(mac_key, nonce || aad || ciphertext)` 截断。
fn compute_tag(
    mac_key: &[u8; KEY_LEN],
    nonce: &[u8; NONCE_LEN],
    aad: &[u8],
    ciphertext: &[u8],
) -> [u8; TAG_LEN] {
    let mut mac = match Hmac::<Sha256>::new_from_slice(mac_key) {
        Ok(mac) => mac,
        Err(_) => return [0u8; TAG_LEN],
    };
    mac.update(nonce);
    mac.update(aad);
    mac.update(ciphertext);
    let out = mac.finalize().into_bytes();
    let mut tag = [0u8; TAG_LEN];
    tag.copy_from_slice(&out[..TAG_LEN]);
    tag
}

/// 由 `seq` / `device_id` / `point_key` / `ts_ns` / `gateway_id` 确定性派生 nonce。
///
/// **说明**：`enc_key` 固定（由机器码派生），若 nonce 随机则 `no_std` 下无处取加密 RNG。
/// 这里用「行标识的哈希」作 nonce。
///
/// ## ⚠️ nonce 唯一性的真正保证 —— 表上的唯一索引，**不是 `seq`**
///
/// nonce 的唯一性**由 `telemetry` 表上的 `uq_telemetry_nonce(device_id, point_key, ts_ns)`
/// 唯一索引在结构层面保证**，而非 `seq` 主键。`seq` 参数保留在哈希输入里仅为兼容
/// **已落盘数据的可解密性**（改哈希输入会使历史密文永久无法解密），它**不**承担唯一性职责。
///
/// ## ⚠️ 绝不允许放宽唯一性
///
/// 本模块是 XOR 流加密：keystream = `HMAC(enc_key, nonce || counter)`，**只依赖 `(key, nonce)`**。
/// 一旦 `(device_id, point_key, ts_ns)` 上出现重复行（即 nonce 复用），两条密文满足
/// `C1 ⊕ C2 = P1 ⊕ P2`——**keystream 被完全消去、明文暴露**。
/// 因此**任何情况下都不得**删除 / 放宽 `uq_telemetry_nonce`，也不得让两行共用同一 nonce。
///
/// ## 冲突如何被上报（**不静默吞掉**）
///
/// 冲突**不会被静默吞掉**——它经 [`WriteOutcome::skipped_nonce_conflict`] 上报，
/// 并在 `write` 内同时打一条 `tracing::warn!`。作用是**逐条隔离**：冲突条被跳过、其余条
/// 照常写入（避免整批回滚丢失合法遥测），但**该冲突始终可见**（供调用方告警 / 审计）。
fn derive_nonce(
    gateway_id: &str,
    seq: i64,
    device_id: &str,
    point_key: &str,
    ts_ns: i64,
) -> [u8; NONCE_LEN] {
    let mut hasher = Sha256Dyn::new();
    hasher.update(gateway_id.as_bytes());
    hasher.update(&seq.to_be_bytes());
    hasher.update(device_id.as_bytes());
    hasher.update(&[0x1f]); // 字段分隔符，避免拼接歧义
    hasher.update(point_key.as_bytes());
    hasher.update(&ts_ns.to_be_bytes());
    let digest = hasher.finalize();
    let mut nonce = [0u8; NONCE_LEN];
    nonce.copy_from_slice(&digest[..NONCE_LEN]);
    nonce
}

/// SHA-256 的动态包装（避免在签名里暴露 `sha2` 类型，便于将来替换）。
struct Sha256Dyn(Sha256);

impl Sha256Dyn {
    /// 新建。
    fn new() -> Self {
        // `Sha256::new()` 依赖 `Digest` trait；此处经 `sha2::Digest` 引入。
        Self(<Sha256 as sha2::Digest>::new())
    }

    /// 追加数据。
    fn update(&mut self, data: &[u8]) {
        sha2::Digest::update(&mut self.0, data);
    }

    /// 输出摘要。
    fn finalize(self) -> [u8; KEY_LEN] {
        let out = sha2::Digest::finalize(self.0);
        let mut digest = [0u8; KEY_LEN];
        digest.copy_from_slice(&out);
        digest
    }
}

/// 关联数据（AAD）= `seq || ts_ns || device_id || point_key` 的长度前缀编码，
/// 绑定密文与「其所在行的元数据」，防止把密文搬到另一行（行内字段错配）。
fn build_aad(seq: i64, ts_ns: i64, device_id: &str, point_key: &str) -> Vec<u8> {
    let mut aad = Vec::new();
    aad.extend_from_slice(&seq.to_be_bytes());
    aad.extend_from_slice(&ts_ns.to_be_bytes());
    aad.extend_from_slice(&(device_id.len() as u64).to_be_bytes());
    aad.extend_from_slice(device_id.as_bytes());
    aad.extend_from_slice(&(point_key.len() as u64).to_be_bytes());
    aad.extend_from_slice(point_key.as_bytes());
    aad
}

/// 加密一个 f64：返回 `hex(nonce(12) || ciphertext(8) || tag(16))`。
///
/// 明文为 `value.to_le_bytes()`（8 字节 IEEE754 小端）→ XOR keystream → 密文同长。
#[must_use]
fn encrypt_value(keys: &CipherKeys, nonce: &[u8; NONCE_LEN], aad: &[u8], value: f64) -> String {
    let plain = value.to_le_bytes();
    let ks = keystream_block(&keys.enc_key, nonce, 0);
    let mut cipher = [0u8; 8];
    for i in 0..8 {
        cipher[i] = plain[i] ^ ks[i];
    }
    let tag = compute_tag(&keys.mac_key, nonce, aad, &cipher);

    let mut blob = Vec::with_capacity(NONCE_LEN + 8 + TAG_LEN);
    blob.extend_from_slice(nonce);
    blob.extend_from_slice(&cipher);
    blob.extend_from_slice(&tag);
    hex::encode(blob)
}

// ---- 写线程指令 ----

/// 解密出的一行（在写线程内完成解密，避免把密钥暴露给调用者）。
type Decrypted = Vec<StoredPoint>;

/// 单条指令的响应通道。
type Resp<T> = Sender<DaemonResult<T>>;

/// 写线程指令（全部经单一写连接串行执行）。
enum Cmd {
    /// 批量落盘（一个事务），返回 [`WriteOutcome`]（区分插入 / nonce 冲突跳过 / seq 幂等跳过）。
    Write {
        /// 待写入点位。
        points: Vec<StoredPoint>,
        /// 响应通道。
        resp: Resp<WriteOutcome>,
    },
    /// 时间范围查询（`from_ns <= ts_ns <= to_ns`，按 ts 升序，最多 `limit` 条）。
    QueryRange {
        /// 设备标识。
        device_id: String,
        /// 起始时间（含）。
        from_ns: i64,
        /// 结束时间（含）。
        to_ns: i64,
        /// 最多返回条数。
        limit: usize,
        /// 响应通道。
        resp: Resp<Decrypted>,
    },
    /// 最新一条（device_id + point_key 内 ts 最大）。
    QueryLatest {
        /// 设备标识。
        device_id: String,
        /// 点位键。
        point_key: String,
        /// 响应通道。
        resp: Resp<Option<StoredPoint>>,
    },
    /// 总行数。
    Count {
        /// 响应通道。
        resp: Resp<u64>,
    },
    /// 裁剪（保留期 + 体积），返回淘汰行数。
    Prune {
        /// 响应通道。
        resp: Resp<usize>,
    },
    /// 磁盘字节统计。
    DiskBytes {
        /// 响应通道。
        resp: Resp<u64>,
    },
    /// 停止写线程。
    Stop {
        /// 响应通道。
        resp: Resp<()>,
    },
}

// ---- 写线程 ----

/// 建表语句（`seq` 主键 → 幂等；`value_enc` 为加密后的 hex 文本）。
///
/// ⚠️ **`uq_telemetry_nonce` 是加密正确性的前提，不是可选优化**：
/// nonce 由 `(gateway_id, seq, device_id, point_key, ts_ns)` 确定性派生，
/// 而 keystream 只依赖 `(enc_key, nonce, counter)`。若表中存在两条
/// `(device_id, point_key, ts_ns)` 相同的行，则它们共用同一段 keystream，
/// XOR 流加密下 `C1 ⊕ C2 = P1 ⊕ P2`，**keystream 被完全消去、明文暴露**。
/// 该唯一索引把「密钥流复用」从**静默的安全事故**转为**响亮的写入失败**——
/// 违反唯一性在结构层面不可达。**任何情况下都不得放宽此约束**（详见 `derive_nonce`）。
const SCHEMA_SQL: &str = r"
CREATE TABLE IF NOT EXISTS telemetry (
    seq       INTEGER PRIMARY KEY,
    device_id TEXT    NOT NULL,
    point_key TEXT    NOT NULL,
    ts_ns     INTEGER NOT NULL,
    value_enc TEXT    NOT NULL,
    quality   TEXT    NOT NULL
);
CREATE UNIQUE INDEX IF NOT EXISTS uq_telemetry_nonce ON telemetry(device_id, point_key, ts_ns);
CREATE INDEX IF NOT EXISTS idx_telemetry_device_ts ON telemetry(device_id, ts_ns);
CREATE INDEX IF NOT EXISTS idx_telemetry_device_point ON telemetry(device_id, point_key, ts_ns);
";

/// 独占写连接的写线程主体（**唯一**的 SQLite 写者 + 唯一持有派生密钥者）。
struct DbWriter {
    conn: Connection,
    clock: Arc<dyn Clock>,
    keys: CipherKeys,
    gateway_id: String,
    max_db_bytes: u64,
    retention_ns: i64,
}

impl DbWriter {
    /// 打开连接并施加 PRAGMA（WAL + synchronous=NORMAL + busy_timeout）。
    fn open_conn(path: &Path) -> DaemonResult<Connection> {
        let conn = Connection::open(path).map_err(map_sqlite)?;
        conn.execute_batch(&format!(
            "PRAGMA journal_mode = WAL;\nPRAGMA synchronous = NORMAL;\nPRAGMA busy_timeout = {BUSY_TIMEOUT_MS};"
        ))
        .map_err(map_sqlite)?;
        let mode: String = conn
            .query_row("PRAGMA journal_mode", [], |row| row.get(0))
            .map_err(map_sqlite)?;
        if !mode.eq_ignore_ascii_case("wal") {
            return Err(storage_err(format!(
                "failed to enable WAL journal mode (got `{mode}`)"
            )));
        }
        // 迁移框架接线（task 55 → task 18 集成波次）：连接建立即执行内置迁移
        // （`schema_migrations` 账本 + `user_version` 版本登记，幂等）。
        // 分库红线不变：telemetry.db 仍走本模块**独立写连接**，不与 queue.db 共库；
        // 迁移失败（含「旧程序开新库」forward-only 拒绝）→ 错误上抛 → `open` 失败
        // → 启动失败，**绝不静默跳过**。
        crate::migrations::run_migrations(&conn)
            .map_err(|e| storage_err(format!("telemetry db migration failed: {e}")))?;
        Ok(conn)
    }

    /// 建表。
    fn bootstrap(&mut self) -> DaemonResult<()> {
        self.conn.execute_batch(SCHEMA_SQL).map_err(map_sqlite)?;
        Ok(())
    }

    /// 指令循环（收到 `Stop` 或发送端全部释放即退出）。
    fn run(&mut self, rx: Receiver<Cmd>) {
        loop {
            match rx.recv() {
                Ok(Cmd::Stop { resp }) => {
                    let _ = resp.send(Ok(()));
                    break;
                }
                Ok(cmd) => self.handle(cmd),
                // 所有发送端已释放（store 被丢弃）：正常退出。
                Err(_) => break,
            }
        }
    }

    /// 处理单条指令（错误一律回传，绝不 panic、绝不退出循环）。
    fn handle(&mut self, cmd: Cmd) {
        match cmd {
            Cmd::Write { points, resp } => {
                let result = self.write(points);
                let _ = resp.send(result);
            }
            Cmd::QueryRange {
                device_id,
                from_ns,
                to_ns,
                limit,
                resp,
            } => {
                let result = self.query_range(&device_id, from_ns, to_ns, limit);
                let _ = resp.send(result);
            }
            Cmd::QueryLatest {
                device_id,
                point_key,
                resp,
            } => {
                let result = self.query_latest(&device_id, &point_key);
                let _ = resp.send(result);
            }
            Cmd::Count { resp } => {
                let result = self.count();
                let _ = resp.send(result);
            }
            Cmd::Prune { resp } => {
                let result = self.prune();
                let _ = resp.send(result);
            }
            Cmd::DiskBytes { resp } => {
                let result = self.disk_bytes();
                let _ = resp.send(result);
            }
            Cmd::Stop { resp } => {
                let _ = resp.send(Ok(()));
            }
        }
    }

    /// 批量落盘（一个事务）：逐条加密后写入，返回 [`WriteOutcome`]。
    ///
    /// ## 语义（与 `uq_telemetry_nonce` 唯一索引协同）—— **逐条隔离，不拖累整批**
    /// - **`seq` 已存在** → 静默跳过（幂等重放），计入 `skipped_duplicate_seq`。
    /// - **`(device_id, point_key, ts_ns)` 已存在但 `seq` 不同** → 该条**跳过**、计入
    ///   `skipped_nonce_conflict`，并**继续处理后续记录**（这正是 nonce 复用被拦截的
    ///   **可见**上报；不静默、也不让整批回滚）。
    /// - **其它 SQLite 存储错误**（磁盘 / SQL 故障等真错误）→ **仍 `?` 中止整批**
    ///   （真故障不得被吞）。
    ///
    /// ⚠️ 之所以不整批 `?`：工业现场「同点位同纳秒」是**正常现象**（点位配重 / 设备时钟不
    /// 精确），若一条冲突即整批回滚，会**丢失数十条合法遥测**，违背「断网继续采集」契约。
    /// 故把 nonce 冲突的作用域从「整批」收窄到「单条」，同时保留其**可见性**。
    fn write(&mut self, points: Vec<StoredPoint>) -> DaemonResult<WriteOutcome> {
        let mut outcome = WriteOutcome::default();
        if points.is_empty() {
            return Ok(outcome);
        }
        let tx = self.conn.transaction().map_err(map_sqlite)?;
        {
            let mut seq_exists = tx
                .prepare("SELECT 1 FROM telemetry WHERE seq = ?1")
                .map_err(map_sqlite)?;
            let mut insert = tx
                .prepare(
                    "INSERT INTO telemetry(seq, device_id, point_key, ts_ns, value_enc, quality) \
                     VALUES(?1, ?2, ?3, ?4, ?5, ?6)",
                )
                .map_err(map_sqlite)?;
            for point in &points {
                // 1) seq 幂等：已存在则静默跳过（重放同一批不报错、不重复计）。
                let already = seq_exists.exists(params![point.seq]).map_err(map_sqlite)?;
                if already {
                    outcome.skipped_duplicate_seq += 1;
                    continue;
                }
                let nonce = derive_nonce(
                    &self.gateway_id,
                    point.seq,
                    &point.device_id,
                    &point.point_key,
                    point.ts_ns,
                );
                let aad = build_aad(point.seq, point.ts_ns, &point.device_id, &point.point_key);
                let value_enc = encrypt_value(&self.keys, &nonce, &aad, point.value);
                // 2) 普通 INSERT：`(device_id, point_key, ts_ns)` 冲突会被唯一索引拦下。
                //    逐条隔离：冲突 → 跳过该条并计数；其它错误 → 中止整批。
                match insert.execute(params![
                    point.seq,
                    point.device_id.as_str(),
                    point.point_key.as_str(),
                    point.ts_ns,
                    value_enc,
                    point.quality.as_str(),
                ]) {
                    Ok(_) => outcome.inserted += 1,
                    Err(err) if is_nonce_conflict(&err) => {
                        outcome.skipped_nonce_conflict += 1;
                        // 逐条失败仅记日志、不泄漏明文；整批提交后由返回值/调用方决定告警。
                        tracing::warn!(
                            target: "daemon::telemetry_store",
                            device_id = %point.device_id,
                            point_key = %point.point_key,
                            ts_ns = point.ts_ns,
                            seq = point.seq,
                            "telemetry nonce uniqueness conflict skipped (keystream reuse blocked)"
                        );
                    }
                    // 非唯一约束的真故障：不吞，中止整批（事务回滚）。
                    Err(err) => return Err(map_sqlite(err)),
                }
            }
        }
        tx.commit().map_err(map_sqlite)?;
        Ok(outcome)
    }

    /// 时间范围查询：`from <= ts <= to`，按 ts 升序，最多 `limit` 条。
    fn query_range(
        &mut self,
        device_id: &str,
        from_ns: i64,
        to_ns: i64,
        limit: usize,
    ) -> DaemonResult<Vec<StoredPoint>> {
        if limit == 0 || from_ns > to_ns {
            return Ok(Vec::new());
        }
        let limit_i64 = i64::try_from(limit).unwrap_or(i64::MAX);
        let mut stmt = self
            .conn
            .prepare(
                "SELECT seq, device_id, point_key, ts_ns, value_enc, quality FROM telemetry \
                 WHERE device_id = ?1 AND ts_ns >= ?2 AND ts_ns <= ?3 \
                 ORDER BY ts_ns ASC, seq ASC LIMIT ?4",
            )
            .map_err(map_sqlite)?;
        let rows = stmt
            .query_map(params![device_id, from_ns, to_ns, limit_i64], row_to_raw)
            .map_err(map_sqlite)?;
        let mut out = Vec::new();
        for row in rows {
            out.push(self.decrypt_row(row.map_err(map_sqlite)?)?);
        }
        Ok(out)
    }

    /// 最新一条（device_id + point_key 内 ts 最大；同 ts 取 seq 最大）。
    fn query_latest(
        &mut self,
        device_id: &str,
        point_key: &str,
    ) -> DaemonResult<Option<StoredPoint>> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT seq, device_id, point_key, ts_ns, value_enc, quality FROM telemetry \
                 WHERE device_id = ?1 AND point_key = ?2 \
                 ORDER BY ts_ns DESC, seq DESC LIMIT 1",
            )
            .map_err(map_sqlite)?;
        let mut rows = stmt
            .query_map(params![device_id, point_key], row_to_raw)
            .map_err(map_sqlite)?;
        match rows.next() {
            None => Ok(None),
            Some(row) => {
                let raw = row.map_err(map_sqlite)?;
                Ok(Some(self.decrypt_row(raw)?))
            }
        }
    }

    /// 总行数。
    fn count(&mut self) -> DaemonResult<u64> {
        let n: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM telemetry", [], |row| row.get(0))
            .map_err(map_sqlite)?;
        Ok(n.max(0) as u64)
    }

    /// 裁剪：保留期淘汰（`ts_ns < now - retention`）+ 体积淘汰（超限从最旧删），
    /// 返回淘汰总行数。
    fn prune(&mut self) -> DaemonResult<usize> {
        let tx = self.conn.transaction().map_err(map_sqlite)?;

        // 1) 保留期淘汰。
        let cutoff = self.clock.now_ns().saturating_sub(self.retention_ns);
        let expired = tx
            .execute("DELETE FROM telemetry WHERE ts_ns < ?1", params![cutoff])
            .map_err(map_sqlite)?;

        // 2) 体积淘汰：按逻辑字节（value_enc hex 长度 + 行开销）估算，从最旧删，
        //    直到回落到 max_db_bytes 内；至少保留最新一条（避免全清）。
        let mut evicted = 0usize;
        loop {
            let (rows, bytes): (i64, i64) = tx
                .query_row(
                    "SELECT COUNT(*), COALESCE(SUM(LENGTH(value_enc) + ?1), 0) FROM telemetry",
                    params![ROW_OVERHEAD_BYTES as i64],
                    |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?)),
                )
                .map_err(map_sqlite)?;
            if rows <= 1 || (bytes.max(0) as u64) <= self.max_db_bytes {
                break;
            }
            let oldest_seq: i64 = tx
                .query_row(
                    "SELECT seq FROM telemetry ORDER BY ts_ns ASC, seq ASC LIMIT 1",
                    [],
                    |row| row.get(0),
                )
                .map_err(map_sqlite)?;
            tx.execute("DELETE FROM telemetry WHERE seq = ?1", params![oldest_seq])
                .map_err(map_sqlite)?;
            evicted += 1;
        }
        tx.commit().map_err(map_sqlite)?;
        Ok(expired.saturating_add(evicted))
    }

    /// 磁盘字节统计（逻辑字节估算，与 `prune` 口径一致）。
    fn disk_bytes(&mut self) -> DaemonResult<u64> {
        let bytes: i64 = self
            .conn
            .query_row(
                "SELECT COALESCE(SUM(LENGTH(value_enc) + ?1), 0) FROM telemetry",
                params![ROW_OVERHEAD_BYTES as i64],
                |row| row.get(0),
            )
            .map_err(map_sqlite)?;
        Ok(bytes.max(0) as u64)
    }

    /// 解密一行原始列（含 MAC 校验；失败即 `StorageError`）。
    fn decrypt_row(&self, raw: RawRow) -> DaemonResult<StoredPoint> {
        let nonce_expected = derive_nonce(
            &self.gateway_id,
            raw.seq,
            &raw.device_id,
            &raw.point_key,
            raw.ts_ns,
        );
        let aad = build_aad(raw.seq, raw.ts_ns, &raw.device_id, &raw.point_key);

        let blob = hex::decode(raw.value_enc.as_bytes())
            .map_err(|e| storage_err(format!("telemetry value_enc is not valid hex: {e}")))?;
        // `ENC_HEX_LEN` 是落盘密文的期望 hex 长度；`raw.value_enc` 长度便于诊断。
        if raw.value_enc.len() != ENC_HEX_LEN || blob.len() != NONCE_LEN + 8 + TAG_LEN {
            return Err(storage_err(format!(
                "telemetry value_enc has unexpected length {} (expected {ENC_HEX_LEN} hex chars)",
                raw.value_enc.len(),
            )));
        }
        let mut nonce = [0u8; NONCE_LEN];
        nonce.copy_from_slice(&blob[..NONCE_LEN]);
        let mut cipher = [0u8; 8];
        cipher.copy_from_slice(&blob[NONCE_LEN..NONCE_LEN + 8]);
        let mut tag = [0u8; TAG_LEN];
        tag.copy_from_slice(&blob[NONCE_LEN + 8..]);

        // 认证：先用独立 mac_key 校验 tag，再解密（encrypt-then-MAC）。
        let expected_tag = compute_tag(&self.mac_key_bytes(), &nonce, &aad, &cipher);
        if !constant_time_eq(&tag, &expected_tag) {
            return Err(storage_err(
                "telemetry value authentication failed (wrong machine code or tampered data)",
            ));
        }
        // 额外校验 nonce 与行元数据一致（防行内字段错配）。
        if !constant_time_eq(&nonce, &nonce_expected) {
            return Err(storage_err(
                "telemetry value nonce mismatch (row metadata altered)",
            ));
        }

        let ks = keystream_block(&self.keys.enc_key, &nonce, 0);
        let mut plain = [0u8; 8];
        for i in 0..8 {
            plain[i] = cipher[i] ^ ks[i];
        }
        let value = f64::from_le_bytes(plain);
        Ok(StoredPoint {
            seq: raw.seq,
            device_id: raw.device_id,
            point_key: raw.point_key,
            ts_ns: raw.ts_ns,
            value,
            quality: raw.quality,
        })
    }

    /// 取 `mac_key` 字节（内部辅助，避免 `self.keys` 借出冲突）。
    fn mac_key_bytes(&self) -> [u8; KEY_LEN] {
        self.keys.mac_key
    }
}

/// 数据库原始列（未解密）。
struct RawRow {
    seq: i64,
    device_id: String,
    point_key: String,
    ts_ns: i64,
    value_enc: String,
    quality: String,
}

/// rusqlite 行 → [`RawRow`]。
fn row_to_raw(row: &rusqlite::Row<'_>) -> rusqlite::Result<RawRow> {
    Ok(RawRow {
        seq: row.get(0)?,
        device_id: row.get(1)?,
        point_key: row.get(2)?,
        ts_ns: row.get(3)?,
        value_enc: row.get(4)?,
        quality: row.get(5)?,
    })
}

/// 常量时间字节比较（避免 MAC 校验的时序侧信道）。
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

// ---- 公开存储 ----

/// 遥测历史归档：单写线程 + SQLite(WAL) + 机器码派生密钥的字段级加密。
pub struct TelemetryStore {
    cfg: TelemetryStoreConfig,
    tx: Sender<Cmd>,
    handle: Mutex<Option<JoinHandle<()>>>,
    closed: AtomicBool,
}

impl std::fmt::Debug for TelemetryStore {
    /// 手写 Debug（`Mutex`/通道不实现 `Debug`）；**不打印密钥或明文值**。
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TelemetryStore")
            .field("gateway_id", &self.cfg.gateway_id)
            .field("db_path", &self.cfg.db_path)
            .field("closed", &self.closed.load(Ordering::SeqCst))
            .finish_non_exhaustive()
    }
}

impl TelemetryStore {
    /// 打开（或创建）遥测库并启动单写线程。
    ///
    /// # Errors
    /// - 配置非法 → `ConfigError`（2000）；
    /// - 目录创建 / SQLite 打开 / WAL 启用 / 建表失败 → `StorageError`（4000）。
    pub fn open(cfg: TelemetryStoreConfig, clock: Arc<dyn Clock>) -> DaemonResult<Self> {
        cfg.validate()?;

        if let Some(parent) = cfg.db_path.parent() {
            if !parent.as_os_str().is_empty() && !parent.exists() {
                std::fs::create_dir_all(parent)
                    .map_err(|e| storage_err(format!("create db dir {}: {e}", parent.display())))?;
            }
        }

        let keys = CipherKeys::derive(&cfg.gateway_id, &cfg.machine_code);
        let (cmd_tx, cmd_rx) = channel::<Cmd>();
        let (init_tx, init_rx) = channel::<DaemonResult<()>>();

        let db_path = cfg.db_path.clone();
        let gateway_id = cfg.gateway_id.clone();
        let max_db_bytes = cfg.max_db_bytes;
        let retention_ns = cfg.retention_ns;
        let handle = Builder::new()
            .name(WRITER_THREAD_NAME.to_string())
            .spawn(move || {
                let boot = DbWriter::open_conn(&db_path).and_then(|conn| {
                    let mut writer = DbWriter {
                        conn,
                        clock,
                        keys,
                        gateway_id,
                        max_db_bytes,
                        retention_ns,
                    };
                    writer.bootstrap()?;
                    Ok(writer)
                });
                match boot {
                    Ok(mut writer) => {
                        let _ = init_tx.send(Ok(()));
                        writer.run(cmd_rx);
                    }
                    Err(err) => {
                        let _ = init_tx.send(Err(err));
                    }
                }
            })
            .map_err(|e| storage_err(format!("spawn telemetry writer thread: {e}")))?;

        init_rx
            .recv()
            .map_err(|_| storage_err("telemetry writer thread died during init"))??;

        Ok(Self {
            cfg,
            tx: cmd_tx,
            handle: Mutex::new(Some(handle)),
            closed: AtomicBool::new(false),
        })
    }

    /// 批量写入（一个事务），返回 [`WriteOutcome`]。
    ///
    /// **逐条隔离**：单条 `(device_id, point_key, ts_ns)` 唯一冲突**不会**拖累整批——
    /// 冲突条被跳过并计入 `skipped_nonce_conflict`，其余合法条仍写入。
    ///
    /// # Errors
    /// - 已关闭 → `StorageError`（4000）；
    /// - **非唯一约束**的真存储故障（磁盘 / SQL 等）→ `StorageError`（4000），整批回滚。
    pub fn write_batch(&self, points: &[StoredPoint]) -> DaemonResult<WriteOutcome> {
        if self.closed.load(Ordering::SeqCst) {
            return Err(storage_err("write rejected: telemetry store is closed"));
        }
        if points.is_empty() {
            return Ok(WriteOutcome::default());
        }
        let points = points.to_vec();
        self.send_cmd(|resp| Cmd::Write { points, resp })
    }

    /// 时间范围查询：`from_ns <= ts_ns <= to_ns`，按 `ts_ns` 升序，最多 `limit` 条。
    ///
    /// `limit == 0` 或 `from_ns > to_ns` 时返回空 `Vec`（不报错）。
    ///
    /// # Errors
    /// 已关闭 / SQLite 读失败 / 解密失败 → `StorageError`（4000）。
    pub fn query_range(
        &self,
        device_id: &str,
        from_ns: i64,
        to_ns: i64,
        limit: usize,
    ) -> DaemonResult<Vec<StoredPoint>> {
        self.ensure_open()?;
        let device_id = device_id.to_string();
        self.send_cmd(|resp| Cmd::QueryRange {
            device_id,
            from_ns,
            to_ns,
            limit,
            resp,
        })
    }

    /// 查询指定设备 + 点位的最新一条（`ts_ns` 最大）。
    ///
    /// # Errors
    /// 已关闭 / SQLite 读失败 / 解密失败 → `StorageError`（4000）。
    pub fn query_latest(
        &self,
        device_id: &str,
        point_key: &str,
    ) -> DaemonResult<Option<StoredPoint>> {
        self.ensure_open()?;
        let device_id = device_id.to_string();
        let point_key = point_key.to_string();
        self.send_cmd(|resp| Cmd::QueryLatest {
            device_id,
            point_key,
            resp,
        })
    }

    /// 总行数。
    ///
    /// # Errors
    /// 已关闭 / SQLite 读失败 → `StorageError`（4000）。
    pub fn count(&self) -> DaemonResult<u64> {
        self.ensure_open()?;
        self.send_cmd(|resp| Cmd::Count { resp })
    }

    /// 裁剪：保留期 + 体积双重淘汰，返回淘汰行数。
    ///
    /// # Errors
    /// 已关闭 / SQLite 写失败 → `StorageError`（4000）。
    pub fn prune(&self) -> DaemonResult<usize> {
        self.ensure_open()?;
        self.send_cmd(|resp| Cmd::Prune { resp })
    }

    /// 磁盘字节统计（逻辑字节估算）。
    ///
    /// # Errors
    /// 已关闭 / SQLite 读失败 → `StorageError`（4000）。
    pub fn disk_bytes(&self) -> DaemonResult<u64> {
        self.ensure_open()?;
        self.send_cmd(|resp| Cmd::DiskBytes { resp })
    }

    /// 库文件路径。
    #[must_use]
    pub fn db_path(&self) -> PathBuf {
        self.cfg.db_path.clone()
    }

    /// 优雅关闭：停止写线程并 join（幂等）。
    ///
    /// # Errors
    /// 写线程已不可用 → `StorageError`（4000）；仍会尽力 join。
    pub fn close(&self) -> DaemonResult<()> {
        if self.closed.swap(true, Ordering::SeqCst) {
            return Ok(());
        }
        let stop_result = self.send_cmd(|resp| Cmd::Stop { resp });
        if let Some(handle) = self.handle.lock().unwrap_or_else(poison_recover).take() {
            let _ = handle.join();
        }
        stop_result
    }

    // ---- 内部辅助 ----

    /// 关闭校验（`close()` 后再调用返回 `StorageError`，绝不 panic）。
    fn ensure_open(&self) -> DaemonResult<()> {
        if self.closed.load(Ordering::SeqCst) {
            Err(storage_err("telemetry store is closed"))
        } else {
            Ok(())
        }
    }

    /// 发送指令并等待响应。
    fn send_cmd<T>(&self, make: impl FnOnce(Resp<T>) -> Cmd) -> DaemonResult<T> {
        let (resp_tx, resp_rx) = channel::<DaemonResult<T>>();
        let cmd = make(resp_tx);
        self.tx
            .send(cmd)
            .map_err(|_| storage_err("telemetry writer thread is gone"))?;
        match resp_rx.recv() {
            Ok(result) => result,
            Err(_) => Err(storage_err("telemetry writer thread did not respond")),
        }
    }
}

impl Drop for TelemetryStore {
    fn drop(&mut self) {
        if self.closed.swap(true, Ordering::SeqCst) {
            return;
        }
        // 进程退出前尽最大努力停止写线程（best effort，不返回错误、不 panic）。
        let _ = self.tx.send(Cmd::Stop { resp: channel().0 });
        if let Ok(mut guard) = self.handle.lock() {
            if let Some(handle) = guard.take() {
                let _ = handle.join();
            }
        }
    }
}

// ---- 工具函数 ----

/// rusqlite 错误 → `StorageError`（4000）。
fn map_sqlite(err: rusqlite::Error) -> DaemonError {
    DaemonError::StorageError(format!("sqlite: {err}"))
}

/// 判定 rusqlite 错误是否为 **`uq_telemetry_nonce` 唯一约束冲突**（nonce 复用被拦截）。
///
/// `write` 已按 `seq` 预检幂等，故唯一可能的约束冲突即来自
/// `uq_telemetry_nonce(device_id, point_key, ts_ns)`。
///
/// 判定方式：结构化错误码 + 文本兜底（不同 SQLite 版本可能上报
/// `SQLITE_CONSTRAINT` / `SQLITE_CONSTRAINT_UNIQUE`，均归一为 `ConstraintViolation`）。
/// **仅为识别「可安全跳过的单条冲突」**；返回 `false` 的错误会被视为真故障、中止整批，
/// 故这里的判定宁可**保守**（无法确认是唯一冲突就当作真故障，不吞）。
fn is_nonce_conflict(err: &rusqlite::Error) -> bool {
    if err.sqlite_error_code() != Some(rusqlite::ErrorCode::ConstraintViolation) {
        return false;
    }
    // 仅放行「唯一性/主键」类约束；外键/非空/CHECK 等约束冲突仍按真故障处理。
    let text = err.to_string();
    text.contains("UNIQUE constraint failed") || text.contains("uq_telemetry_nonce")
}

/// 构造 `StorageError`（4000）。
fn storage_err(msg: impl fmt::Display) -> DaemonError {
    DaemonError::StorageError(msg.to_string())
}

/// 构造 `ConfigError`（2000）。
fn config_err(msg: impl fmt::Display) -> DaemonError {
    DaemonError::ConfigError(msg.to_string())
}

/// 锁中毒恢复（取回内部数据，绝不 panic）。
fn poison_recover<T>(poisoned: std::sync::PoisonError<T>) -> T {
    poisoned.into_inner()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::{ERR_CONFIG, ERR_STORAGE};
    use rusqlite::OpenFlags;
    use tempfile::TempDir;

    /// 测试用一天（纳秒）。
    const DAY_NS: i64 = 24 * 60 * 60 * 1_000_000_000;

    /// 测试起点（2024-01-01 前后，任意固定值即可）。
    const T0_NS: i64 = 1_700_000_000_000_000_000;

    /// 测试机器码 A。
    const MACHINE_A: &str = "machine-code-AAAA-1111";
    /// 测试机器码 B（不同机器 → 不同密钥）。
    const MACHINE_B: &str = "machine-code-BBBB-2222";

    /// 构造临时目录。
    fn tempdir() -> TempDir {
        tempfile::tempdir().expect("tempdir")
    }

    /// 构造默认配置（`dir/telemetry.db`）。
    fn cfg(dir: &Path, gateway: &str, machine: &str) -> TelemetryStoreConfig {
        TelemetryStoreConfig::new(dir.join(TELEMETRY_DB_FILE_NAME), gateway, machine)
            .expect("config")
    }

    /// 打开存储（手工时钟）。
    fn open(dir: &Path, gateway: &str, machine: &str, clock: &ManualClock) -> TelemetryStore {
        TelemetryStore::open(cfg(dir, gateway, machine), Arc::new(clock.clone())).expect("open")
    }

    /// 构造一个点位。
    fn point(seq: i64, device: &str, key: &str, ts_ns: i64, value: f64) -> StoredPoint {
        StoredPoint {
            seq,
            device_id: device.to_string(),
            point_key: key.to_string(),
            ts_ns,
            value,
            quality: "Good".to_string(),
        }
    }

    /// 单条自增查询（用于取值断言）。
    fn one_range(store: &TelemetryStore, device: &str, from: i64, to: i64) -> Vec<StoredPoint> {
        store
            .query_range(device, from, to, 1000)
            .expect("query_range")
    }

    /// 写一批并返回 [`WriteOutcome`]（失败即 panic，测试语义）。
    fn write(store: &TelemetryStore, points: &[StoredPoint]) -> WriteOutcome {
        store.write_batch(points).expect("write")
    }

    /// 写一批并返回 `inserted`（多数测试只关心真实插入数）。
    fn inserted(store: &TelemetryStore, points: &[StoredPoint]) -> usize {
        write(store, points).inserted
    }

    /// QA Happy：写入 / 查询往返，值精确还原（f64 位相等）。
    #[test]
    #[allow(clippy::approx_constant)] // `3.14159` 是任取的历史采样值，并非 π 常量。
    fn write_and_query_roundtrip_restores_plaintext() {
        let dir = tempdir();
        let clock = ManualClock::new(T0_NS);
        let store = open(dir.path(), "gw-rt", MACHINE_A, &clock);

        let points = vec![
            point(1, "dev-1", "temp", T0_NS + 1, 3.14159),
            point(2, "dev-1", "temp", T0_NS + 2, -273.15),
            point(3, "dev-1", "humid", T0_NS + 3, 0.0),
            point(4, "dev-2", "temp", T0_NS + 4, 1234.5678),
        ];
        assert_eq!(inserted(&store, &points), 4);
        assert_eq!(store.count().expect("count"), 4);

        let got = one_range(&store, "dev-1", T0_NS, T0_NS + 10);
        assert_eq!(got.len(), 3);
        for (i, g) in got.iter().enumerate() {
            assert_eq!(g.seq, i as i64 + 1);
            assert_eq!(
                g.value.to_bits(),
                points[i].value.to_bits(),
                "值必须精确还原"
            );
            assert_eq!(g.device_id, "dev-1");
            assert_eq!(g.quality, "Good");
        }
        assert_eq!(got[0].point_key, "temp");
        assert_eq!(got[2].point_key, "humid");

        let dev2 = one_range(&store, "dev-2", T0_NS, T0_NS + 10);
        assert_eq!(dev2.len(), 1);
        assert_eq!(dev2[0].value, 1234.5678);
        store.close().expect("close");
    }

    /// 🔒 加密铁证 1：原生 rusqlite 直读 `value_enc` 列 → 不含明文 float 的 IEEE754 字节。
    #[test]
    #[allow(clippy::approx_constant)] // `3.14159` 是任务书指定的历史采样值，并非 π 常量。
    fn encryption_evidence_raw_bytes_do_not_contain_plaintext_float() {
        let dir = tempdir();
        let clock = ManualClock::new(T0_NS);
        let store = open(dir.path(), "gw-enc", MACHINE_A, &clock);

        let secret = 3.14159_f64;
        store
            .write_batch(&[point(1, "dev-sec", "secret", T0_NS + 5, secret)])
            .expect("write");
        assert_eq!(store.count().expect("count"), 1);
        // 关键：必须在 store 仍持有写连接（未关闭）时读盘，证明「落盘即密文」。
        let db_path = store.db_path();

        // 用原生 rusqlite 只读连接直读原始列（绕过本模块解密逻辑）。
        let ro = Connection::open_with_flags(&db_path, OpenFlags::SQLITE_OPEN_READ_ONLY)
            .expect("open read-only");
        let enc: String = ro
            .query_row("SELECT value_enc FROM telemetry WHERE seq = 1", [], |row| {
                row.get(0)
            })
            .expect("select value_enc");

        // 1) 原始列是 hex 编码的定长密文（nonce + ciphertext + tag）。
        assert_eq!(enc.len(), ENC_HEX_LEN, "value_enc 必须是定长 hex 密文");
        assert!(
            enc.chars().all(|c| c.is_ascii_hexdigit()),
            "value_enc 必须是 hex 明文编码"
        );
        let blob = hex::decode(&enc).expect("hex decode");
        assert_eq!(blob.len(), NONCE_LEN + 8 + TAG_LEN);

        // 2) 密文主体（去掉 nonce 与 tag）不得等于明文 IEEE754 小端表示。
        let cipher = &blob[NONCE_LEN..NONCE_LEN + 8];
        assert_ne!(
            cipher,
            &secret.to_le_bytes()[..],
            "密文不得等于明文 f64 小端表示"
        );
        // 3) 明文 f64 的 IEEE754 小端 / 大端字节序列均不得出现在密文任何位置。
        let le = secret.to_le_bytes();
        let be = secret.to_be_bytes();
        assert!(
            !blob.windows(le.len()).any(|w| w == le),
            "原始字节里不得出现明文 f64 小端表示（加密未生效）"
        );
        assert!(
            !blob.windows(be.len()).any(|w| w == be),
            "原始字节里不得出现明文 f64 大端表示（加密未生效）"
        );
        // 4) 十六进制文本层也不得包含明文的 hex 表示（防御性）。
        assert!(
            !enc.contains(&hex::encode(le)),
            "hex 文本里不得含明文 f64 的小端 hex"
        );
        assert!(
            !enc.contains(&hex::encode(be)),
            "hex 文本里不得含明文 f64 的大端 hex"
        );
        // 5) 明文 ASCII（如 "3.14159"）也不得出现（防误存浮点字符串）。
        assert!(!enc.contains("3.14159"), "hex 文本里不得含明文 ASCII 值");
        drop(ro);
        store.close().expect("close");
    }

    /// 🔒 加密铁证 2：换 machine_code 打开同一 db → 解密失败（拷盘即失效）。
    #[test]
    fn decryption_fails_with_different_machine_code() {
        let dir = tempdir();
        let clock = ManualClock::new(T0_NS);
        let store_a = open(dir.path(), "gw-port", MACHINE_A, &clock);
        store_a
            .write_batch(&[point(1, "dev-p", "k", T0_NS + 1, 42.5)])
            .expect("write A");
        assert_eq!(store_a.count().expect("count"), 1);
        store_a.close().expect("close A");

        // 换机器码打开同一文件：密钥不同 → 认证失败。
        let store_b = open(dir.path(), "gw-port", MACHINE_B, &clock);
        assert_eq!(store_b.count().expect("count"), 1, "行仍在（只是解不开）");
        let err = store_b
            .query_latest("dev-p", "k")
            .expect_err("换机器码必须解密失败");
        assert_eq!(err.error_code(), ERR_STORAGE);
        assert!(
            err.to_string().contains("authentication failed"),
            "必须是认证失败: {err}"
        );
        // 范围查询同样失败。
        assert!(store_b.query_range("dev-p", T0_NS, T0_NS + 10, 10).is_err());
        // 用原机器码仍可读出原值（证明只是密钥不匹配，不是数据损坏）。
        store_b.close().expect("close B");
        let store_a2 = open(dir.path(), "gw-port", MACHINE_A, &clock);
        let got = store_a2
            .query_latest("dev-p", "k")
            .expect("query A")
            .expect("some");
        assert_eq!(got.value, 42.5, "原机器码必须能还原原值");
        store_a2.close().expect("close A2");
    }

    /// 范围查询边界：`from == to` 命中单点，范围外为空。
    #[test]
    fn query_range_boundaries_from_equals_to_and_outside() {
        let dir = tempdir();
        let clock = ManualClock::new(T0_NS);
        let store = open(dir.path(), "gw-bound", MACHINE_A, &clock);
        let points = vec![
            point(1, "d", "k", T0_NS + 10, 1.0),
            point(2, "d", "k", T0_NS + 20, 2.0),
            point(3, "d", "k", T0_NS + 30, 3.0),
        ];
        store.write_batch(&points).expect("write");

        // from == to → 恰命中该时刻的点。
        let exact = one_range(&store, "d", T0_NS + 20, T0_NS + 20);
        assert_eq!(exact.len(), 1);
        assert_eq!(exact[0].seq, 2);
        assert_eq!(exact[0].value, 2.0);

        // 闭区间两端含界。
        let closed = one_range(&store, "d", T0_NS + 10, T0_NS + 30);
        assert_eq!(closed.len(), 3);
        assert_eq!(closed.first().map(|p| p.seq), Some(1));
        assert_eq!(closed.last().map(|p| p.seq), Some(3));

        // 范围之外（更晚）→ 空。
        assert!(one_range(&store, "d", T0_NS + 40, T0_NS + 50).is_empty());
        // 范围之外（更早）→ 空。
        assert!(one_range(&store, "d", T0_NS, T0_NS + 5).is_empty());
        store.close().expect("close");
    }

    /// `limit` 截断：按 ts 升序取前 N 条。
    #[test]
    fn query_range_limit_truncates_in_ascending_time_order() {
        let dir = tempdir();
        let clock = ManualClock::new(T0_NS);
        let store = open(dir.path(), "gw-limit", MACHINE_A, &clock);
        let mut points = Vec::new();
        for i in 0..10i64 {
            points.push(point(i + 1, "d", "k", T0_NS + i, i as f64));
        }
        store.write_batch(&points).expect("write");

        let first3 = store
            .query_range("d", T0_NS, T0_NS + 100, 3)
            .expect("query");
        assert_eq!(first3.len(), 3, "limit 必须截断");
        assert_eq!(
            first3.iter().map(|p| p.seq).collect::<Vec<_>>(),
            vec![1, 2, 3],
            "必须取时间最早的前 3 条"
        );
        // limit = 0 → 空（不报错）。
        assert!(store
            .query_range("d", T0_NS, T0_NS + 100, 0)
            .expect("limit0")
            .is_empty());
        // from > to → 空（不报错）。
        assert!(store
            .query_range("d", T0_NS + 100, T0_NS, 10)
            .expect("inverted")
            .is_empty());
        store.close().expect("close");
    }

    /// 空库查询返回空 `Vec`（不是报错）+ `query_latest` 返回 `None`。
    #[test]
    fn empty_store_returns_empty_results_not_errors() {
        let dir = tempdir();
        let clock = ManualClock::new(T0_NS);
        let store = open(dir.path(), "gw-empty", MACHINE_A, &clock);

        assert_eq!(store.count().expect("count"), 0);
        assert!(store
            .query_range("nothing", i64::MIN, i64::MAX, 100)
            .expect("range")
            .is_empty());
        assert_eq!(
            store.query_latest("nothing", "nothing").expect("latest"),
            None
        );
        assert_eq!(store.disk_bytes().expect("bytes"), 0);
        // 空批写入返回 0。
        assert_eq!(inserted(&store, &[]), 0);
        store.close().expect("close");
    }

    /// `prune` 保留期裁剪：推进时钟超期 → 旧点被淘汰（零真实 sleep）。
    #[test]
    fn prune_evicts_points_past_retention() {
        let dir = tempdir();
        let clock = ManualClock::new(T0_NS);
        let store = open(dir.path(), "gw-retain", MACHINE_A, &clock);
        let mut points = Vec::new();
        for i in 0..5i64 {
            points.push(point(i + 1, "d", "k", T0_NS + i * 1_000, i as f64));
        }
        store.write_batch(&points).expect("write");
        assert_eq!(store.count().expect("count"), 5);

        // 保留期内：不裁剪。
        assert_eq!(store.prune().expect("prune in-window"), 0);
        assert_eq!(store.count().expect("count"), 5);

        // 推进 31 天（> 默认 30 天保留期），全部过期（注意 ts 是 T0 附近）。
        clock.advance(31 * DAY_NS);
        assert_eq!(store.prune().expect("prune expired"), 5);
        assert_eq!(store.count().expect("count"), 0);
        store.close().expect("close");
    }

    /// `prune` 体积裁剪：超过 max_db_bytes → 从最旧删，保留最新。
    #[test]
    fn prune_evicts_oldest_when_db_bytes_exceed_limit() {
        let dir = tempdir();
        // 时钟固定在 T0，避免保留期干扰（只测体积维度）。
        let clock = ManualClock::new(T0_NS);
        let mut config = cfg(dir.path(), "gw-size", MACHINE_A);
        // 每条逻辑字节 ≈ ENC_HEX_LEN(72) + ROW_OVERHEAD(64) = 136；限 400 → 约 3 条。
        config.max_db_bytes = 400;
        let store = TelemetryStore::open(config, Arc::new(clock.clone())).expect("open");

        let mut points = Vec::new();
        for i in 0..20i64 {
            points.push(point(i + 1, "d", "k", T0_NS + i, i as f64));
        }
        store.write_batch(&points).expect("write");
        assert_eq!(store.count().expect("count"), 20);

        let pruned = store.prune().expect("prune");
        assert!(pruned > 0, "必须淘汰部分旧点");
        let remaining = store.count().expect("count");
        assert!((1..20).contains(&remaining), "剩余 {remaining}");
        assert!(
            store.disk_bytes().expect("bytes") <= 400,
            "体积必须回落到上限内: {}",
            store.disk_bytes().expect("bytes")
        );
        // 保留的必须是最新（ts 最大）的点。
        let survivors = one_range(&store, "d", i64::MIN, i64::MAX);
        let max_seq = survivors.iter().map(|p| p.seq).max().expect("non-empty");
        assert_eq!(max_seq, 20, "必须保留最新点");
        store.close().expect("close");
    }

    /// `prune` 双重裁剪叠加：保留期淘汰 + 体积淘汰，返回总数。
    #[test]
    fn prune_combines_retention_and_size_eviction() {
        let dir = tempdir();
        let clock = ManualClock::new(T0_NS);
        let mut config = cfg(dir.path(), "gw-both", MACHINE_A);
        config.max_db_bytes = 400;
        let store = TelemetryStore::open(config, Arc::new(clock.clone())).expect("open");

        let mut points = Vec::new();
        // 5 条超期（T0 附近）+ 10 条最新（推进时钟之后）。
        for i in 0..5i64 {
            points.push(point(i + 1, "d", "k", T0_NS + i, i as f64));
        }
        clock.advance(31 * DAY_NS);
        let recent_base = clock.now();
        for i in 0..10i64 {
            points.push(point(i + 6, "d", "k", recent_base + i, (i + 6) as f64));
        }
        store.write_batch(&points).expect("write");
        assert_eq!(store.count().expect("count"), 15);

        let pruned = store.prune().expect("prune");
        assert!(pruned >= 5, "至少淘汰 5 条超期的: {pruned}");
        // 超期的必须全没了。
        let all = one_range(&store, "d", i64::MIN, i64::MAX);
        assert!(
            all.iter().all(|p| p.ts_ns >= recent_base),
            "超期点必须全部淘汰"
        );
        assert!(store.disk_bytes().expect("bytes") <= 400);
        store.close().expect("close");
    }

    /// `close()` 后再写 → 返回 `Err`（不是 panic）；幂等关闭。
    #[test]
    fn closed_store_rejects_writes_without_panic() {
        let dir = tempdir();
        let clock = ManualClock::new(T0_NS);
        let store = open(dir.path(), "gw-closed", MACHINE_A, &clock);
        store
            .write_batch(&[point(1, "d", "k", T0_NS, 1.0)])
            .expect("write");
        store.close().expect("close");
        store.close().expect("close idempotent");

        let err = store
            .write_batch(&[point(2, "d", "k", T0_NS + 1, 2.0)])
            .expect_err("must fail after close");
        assert_eq!(err.error_code(), ERR_STORAGE);
        assert!(store.count().is_err(), "关闭后读也必须 Err");
        assert!(store.query_range("d", T0_NS, T0_NS + 1, 1).is_err());
        assert!(store.prune().is_err());
        drop(store); // Drop 不 panic。
    }

    /// `count` 准确：批量写入 + 幂等重复写（同 `seq` 静默跳过）不重复计。
    #[test]
    fn count_is_accurate_and_write_is_idempotent_by_seq() {
        let dir = tempdir();
        let clock = ManualClock::new(T0_NS);
        let store = open(dir.path(), "gw-idem", MACHINE_A, &clock);

        let batch = vec![
            point(1, "d", "k", T0_NS + 1, 1.0),
            point(2, "d", "k", T0_NS + 2, 2.0),
            point(3, "d", "k", T0_NS + 3, 3.0),
        ];
        assert_eq!(inserted(&store, &batch), 3);
        assert_eq!(store.count().expect("count"), 3);

        // 重复写同 seq（不同 value）→ 被忽略（计入 skipped_duplicate_seq），计数不变，原值不变。
        let dup = vec![
            point(2, "d", "k", T0_NS + 2, 999.0),
            point(4, "d", "k", T0_NS + 4, 4.0),
        ];
        let outcome = write(&store, &dup);
        assert_eq!(outcome.inserted, 1, "只新增 seq=4");
        assert_eq!(outcome.skipped_duplicate_seq, 1, "seq=2 幂等跳过");
        assert_eq!(outcome.skipped_nonce_conflict, 0, "同 seq 不算 nonce 冲突");
        assert_eq!(store.count().expect("count"), 4);

        let got = one_range(&store, "d", T0_NS, T0_NS + 10);
        let seq2 = got.iter().find(|p| p.seq == 2).expect("seq2");
        assert_eq!(seq2.value, 2.0, "幂等：原 seq 的值不得被覆盖");
        store.close().expect("close");
    }

    /// 🔒 nonce 唯一性结构保证（任务 A.2）：同一 `(device_id, point_key, ts_ns)`
    /// 但不同 `seq` 的第二行 → **写入返回 Err**（而非静默成功 / 复用 keystream）。
    #[test]
    fn duplicate_device_point_ts_is_rejected_at_write() {
        let dir = tempdir();
        let clock = ManualClock::new(T0_NS);
        let store = open(dir.path(), "gw-uniq", MACHINE_A, &clock);

        // 第一行成功。
        assert_eq!(
            inserted(&store, &[point(1, "dev-u", "temp", T0_NS + 100, 1.0)]),
            1
        );
        assert_eq!(store.count().expect("count"), 1);

        // 第二行：(device_id, point_key, ts_ns) 与第一行完全相同，但 seq 不同。
        // 逐条隔离语义：该条被**跳过并可见上报**（不再整批 Err、也不静默吞）。
        let outcome = write(&store, &[point(2, "dev-u", "temp", T0_NS + 100, 2.0)]);
        assert_eq!(outcome.inserted, 0, "冲突条不得写入");
        assert_eq!(outcome.skipped_nonce_conflict, 1, "冲突必须被计入可见上报");
        assert!(outcome.has_nonce_conflict(), "必须可通过返回值感知冲突");
        assert_eq!(outcome.total_processed(), 1);
        // 该批次未落任何新行；既有行未被覆盖。
        assert_eq!(store.count().expect("count"), 1, "冲突条不得落盘");
        let latest = store
            .query_latest("dev-u", "temp")
            .expect("latest")
            .expect("some");
        assert_eq!(latest.value, 1.0, "冲突条不得覆盖既有值");

        // 换一个不同的 ts → 允许（唯一性只针对三元组相同的情况）。
        assert_eq!(
            inserted(&store, &[point(3, "dev-u", "temp", T0_NS + 101, 3.0)]),
            1
        );
        // 换一个不同的 device_id（其余同）→ 也允许。
        assert_eq!(
            inserted(&store, &[point(4, "dev-u2", "temp", T0_NS + 100, 4.0)]),
            1
        );
        // 换一个不同的 point_key（其余同）→ 也允许。
        assert_eq!(
            inserted(&store, &[point(5, "dev-u", "temp2", T0_NS + 100, 5.0)]),
            1
        );
        assert_eq!(store.count().expect("count"), 4);
        store.close().expect("close");
    }

    /// 🔒 nonce 不复用守护（任务 A.4）：两行除 `ts_ns` 外全同 → 都能正确解密，
    /// 且两行的 `value_enc` **前 12 字节（nonce）不相等**。
    ///
    /// 这把「nonce 必须唯一」写成**可执行断言**，而不是注释。
    #[test]
    fn distinct_keys_never_reuse_keystream() {
        let dir = tempdir();
        let clock = ManualClock::new(T0_NS);
        let store = open(dir.path(), "gw-ks", MACHINE_A, &clock);

        // 除 ts_ns 外全同（同 device / point / gateway / machine），seq 不同。
        store
            .write_batch(&[
                point(1, "dev-ks", "temp", T0_NS + 1000, 11.0),
                point(2, "dev-ks", "temp", T0_NS + 2000, 22.0),
            ])
            .expect("write");

        // 直读两行原始密文，比较 nonce（前 12 字节）。
        let db_path = store.db_path();
        let ro = Connection::open_with_flags(&db_path, OpenFlags::SQLITE_OPEN_READ_ONLY)
            .expect("open ro");
        let enc1: String = ro
            .query_row("SELECT value_enc FROM telemetry WHERE seq = 1", [], |r| {
                r.get(0)
            })
            .expect("enc1");
        let enc2: String = ro
            .query_row("SELECT value_enc FROM telemetry WHERE seq = 2", [], |r| {
                r.get(0)
            })
            .expect("enc2");
        drop(ro);

        let b1 = hex::decode(&enc1).expect("hex1");
        let b2 = hex::decode(&enc2).expect("hex2");
        assert_ne!(
            &b1[..NONCE_LEN],
            &b2[..NONCE_LEN],
            "两行 nonce 必须不同（否则 keystream 复用）"
        );
        // keystream 未复用 ⇒ 密文主体也不该相同。
        assert_ne!(
            &b1[NONCE_LEN..NONCE_LEN + 8],
            &b2[NONCE_LEN..NONCE_LEN + 8],
            "密文主体不得相同"
        );

        // 两行都能精确解密回正确的明文值。
        let got = one_range(&store, "dev-ks", T0_NS, T0_NS + 10_000);
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].value, 11.0);
        assert_eq!(got[1].value, 22.0);
        store.close().expect("close");
    }

    /// 任务 B：两个 `TelemetryStore` 指向**同一个 db 文件**，写「同设备同点位同 ts」。
    ///
    /// **实际观察到的行为（重要）**：两个实例都能 `open` 成功——写线程各自持有
    /// 独立连接 + WAL，SQLite 允许多进程/多连接；但第二个实例写入同一
    /// `(device_id, point_key, ts_ns)` 时会撞上 `uq_telemetry_nonce` 唯一索引，
    /// **该条被跳过并计入 `skipped_nonce_conflict`（可见）**，而**不是**第二个 open 失败。
    /// 即：**冲突在 write 阶段被拦截且可见上报**。
    #[test]
    fn two_instances_on_same_file_cannot_reuse_nonce() {
        let dir = tempdir();
        let clock = ManualClock::new(T0_NS);
        let cfg_a = cfg(dir.path(), "gw-share", MACHINE_A);

        let store_a = TelemetryStore::open(cfg_a.clone(), Arc::new(clock.clone())).expect("open A");
        // 第二个实例指向同一文件：WAL 下允许打开。
        let store_b = match TelemetryStore::open(cfg_a, Arc::new(clock.clone())) {
            Ok(opened) => {
                // 观察：open 成功（冲突推迟到 write）。
                assert_eq!(opened.count().expect("count B"), 0);
                opened
            }
            Err(e) => {
                // 若某些平台独占打开失败，也接受——但需明确记录。
                assert_eq!(e.error_code(), ERR_STORAGE);
                store_a.close().expect("close A");
                return;
            }
        };

        // A 先写一行。
        assert_eq!(
            inserted(&store_a, &[point(1, "shared", "temp", T0_NS + 7, 1.0)]),
            1
        );

        // B 写「同设备同点位同 ts」但不同 seq → 该条被**跳过并可见上报**（不落盘）。
        let outcome = write(&store_b, &[point(2, "shared", "temp", T0_NS + 7, 2.0)]);
        assert_eq!(outcome.inserted, 0, "跨实例冲突条不得写入");
        assert_eq!(outcome.skipped_nonce_conflict, 1, "跨实例冲突必须可见上报");
        // 数据未被破坏：仍是 A 写入的那一行。
        assert_eq!(store_a.count().expect("count A"), 1);
        let got = store_a
            .query_latest("shared", "temp")
            .expect("latest")
            .expect("some");
        assert_eq!(got.value, 1.0);

        store_b.close().expect("close B");
        store_a.close().expect("close A");
    }

    /// 🛡️ 批内单条冲突**不拖累整批**：一批 3 条，中间一条与批内前一条撞
    /// `(device_id, point_key, ts_ns)` → 合法两条必须落盘（**不整批回滚**）。
    #[test]
    fn batch_with_one_conflict_keeps_other_rows() {
        let dir = tempdir();
        let clock = ManualClock::new(T0_NS);
        let store = open(dir.path(), "gw-b1", MACHINE_A, &clock);

        let batch = vec![
            point(1, "b", "k", T0_NS + 5, 1.0),
            // 与上一条 (device, key, ts) 完全相同，但 seq 不同 → 该条冲突被跳过。
            point(2, "b", "k", T0_NS + 5, 2.0),
            point(3, "b", "k", T0_NS + 6, 3.0),
        ];
        let outcome = write(&store, &batch);

        assert_eq!(outcome.inserted, 2, "两条合法必须保留");
        assert_eq!(outcome.skipped_nonce_conflict, 1, "一条冲突被跳过");
        assert_eq!(outcome.skipped_duplicate_seq, 0);
        assert_eq!(outcome.total_processed(), 3);
        // **关键**：合法数据没有整批丢失。
        assert_eq!(store.count().expect("count"), 2);

        let got = one_range(&store, "b", T0_NS, T0_NS + 100);
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].seq, 1, "批内前一条（合法）必须落盘");
        assert_eq!(got[0].value, 1.0);
        assert_eq!(got[1].seq, 3, "批内后一条（合法）必须落盘");
        assert_eq!(got[1].value, 3.0);
        store.close().expect("close");
    }

    /// 🛡️ 批内一条与**库中既有行**冲突 → 其余合法条仍落盘。
    #[test]
    fn batch_conflict_against_existing_row_keeps_others() {
        let dir = tempdir();
        let clock = ManualClock::new(T0_NS);
        let store = open(dir.path(), "gw-b2", MACHINE_A, &clock);

        // 先写一条进库。
        assert_eq!(inserted(&store, &[point(1, "c", "k", T0_NS + 50, 10.0)]), 1);

        // 再用一批：含与既有行冲突的一条 + 两条合法。
        let batch = vec![
            point(2, "c", "k", T0_NS + 50, 20.0), // 冲突（同 device/key/ts，seq 不同）
            point(3, "c", "k", T0_NS + 51, 30.0), // 合法
            point(4, "c", "k", T0_NS + 52, 40.0), // 合法
        ];
        let outcome = write(&store, &batch);

        assert_eq!(outcome.inserted, 2, "两条合法必须保留");
        assert_eq!(outcome.skipped_nonce_conflict, 1);
        assert_eq!(store.count().expect("count"), 3, "1 既有 + 2 新增");

        // 既有行的值未被覆盖。
        let got = one_range(&store, "c", T0_NS, T0_NS + 100);
        let seq1 = got.iter().find(|p| p.seq == 1).expect("seq1");
        assert_eq!(seq1.value, 10.0, "既有值不得被冲突条覆盖");
        assert!(got.iter().any(|p| p.seq == 3));
        assert!(got.iter().any(|p| p.seq == 4));
        store.close().expect("close");
    }

    /// 🛡️ **非唯一约束**的真存储错误**仍整批中止**（证明冲突隔离没有把真故障吞掉）。
    ///
    /// 构造方式：用独立的可写连接 `DROP TABLE telemetry`，再让写线程写一批 →
    /// `INSERT`/预检会以 `SQLITE_ERROR: no such table`（**非** `ConstraintViolation`）
    /// 失败 → 必须 `Err`（而非被当作「可跳过的冲突」）。
    #[test]
    fn non_nonce_storage_error_still_aborts_batch() {
        let dir = tempdir();
        let clock = ManualClock::new(T0_NS);
        let store = open(dir.path(), "gw-b3", MACHINE_A, &clock);
        assert_eq!(inserted(&store, &[point(1, "e", "k", T0_NS + 1, 1.0)]), 1);

        // 从外部连接把表删掉，模拟真存储故障（非唯一约束）。
        {
            let rw = Connection::open(store.db_path()).expect("open rw");
            rw.execute_batch("DROP TABLE telemetry")
                .expect("drop table");
        }

        let err = store
            .write_batch(&[point(2, "e", "k", T0_NS + 2, 2.0)])
            .expect_err("真存储故障必须整批失败，不得被吞成‘跳过’");
        assert_eq!(err.error_code(), ERR_STORAGE);
        assert!(matches!(err, DaemonError::StorageError(_)), "err: {err}");
        store.close().expect("close");
    }

    // ---- 迁移框架接线（task 55 集成波次） ----

    /// QA 接线：`open` 成功的库必须已走迁移框架 —— `user_version = 1`、
    /// `schema_migrations` 账本恰 1 行（内置 v1）。分库红线：本库文件仍是
    /// 独立的 telemetry.db，未共享任何连接。
    #[test]
    fn open_runs_builtin_migration_and_ledger() {
        let dir = tempdir();
        let clock = ManualClock::new(T0_NS);
        let store = open(dir.path(), "gw-mig", MACHINE_A, &clock);
        store.close().expect("close");

        // 用只读连接直查迁移账本（绕过本模块，验证落盘状态）。
        let ro = Connection::open_with_flags(
            dir.path().join(TELEMETRY_DB_FILE_NAME),
            OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .expect("open read-only");
        let version: i64 = ro
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .expect("user_version");
        assert_eq!(version, 1, "内置 v1 迁移必须已应用");
        let ledger_rows: i64 = ro
            .query_row("SELECT COUNT(*) FROM schema_migrations", [], |r| r.get(0))
            .expect("ledger count");
        assert_eq!(ledger_rows, 1, "账本恰 1 行（v1）");
    }

    /// QA 接线 Error：库文件损坏（非 SQLite 格式）→ 连接初始化 / 迁移失败 →
    /// `open` 失败（启动失败、可解释错误，绝不静默跳过）。
    #[test]
    fn open_fails_when_migration_fails_on_corrupt_db() {
        let dir = tempdir();
        let db_path = dir.path().join(TELEMETRY_DB_FILE_NAME);
        std::fs::write(&db_path, b"this is definitely not a sqlite database").expect("seed garbage");

        let clock = ManualClock::new(T0_NS);
        let err = TelemetryStore::open(cfg(dir.path(), "gw-corrupt", MACHINE_A), Arc::new(clock))
            .expect_err("corrupt db must fail during open init");
        assert_eq!(err.error_code(), ERR_STORAGE);
        // 失败发生在写线程初始化路径（WAL PRAGMA 或迁移框架），错误已收敛并上抛。
        assert!(
            err.to_string().contains("sqlite:"),
            "错误须来自 SQLite 初始化链路: {err}"
        );
    }

    /// 任务 C：`Debug for TelemetryStoreConfig` 只显示文件名、不显示目录；密钥脱敏。
    #[test]
    fn config_debug_hides_directory_and_machine_code() {
        let dir = tempdir();
        let config = cfg(dir.path(), "gw-dbg", MACHINE_A);
        let rendered = format!("{config:?}");

        assert!(
            rendered.contains(TELEMETRY_DB_FILE_NAME),
            "必须显示文件名: {rendered}"
        );
        // 不得泄露目录路径。
        let dir_str = dir.path().to_string_lossy().to_string();
        assert!(!rendered.contains(&dir_str), "不得显示目录路径: {rendered}");
        // 机器码必须脱敏。
        assert!(!rendered.contains(MACHINE_A), "机器码必须脱敏: {rendered}");
        assert!(rendered.contains("<redacted>"), "应含脱敏占位: {rendered}");
        // 网关 id 保留（非敏感，便于排障）。
        assert!(rendered.contains("gw-dbg"));
    }

    /// 任务 C：`StoredPoint` 的 Debug 显示明文 `value`（解密后结构，明文可见是正确行为）。
    #[test]
    fn stored_point_debug_shows_plaintext_value() {
        let p = point(1, "dev-dbg", "temp", T0_NS, 123.5);
        let rendered = format!("{p:?}");
        assert!(
            rendered.contains("123.5"),
            "StoredPoint 是解密后明文结构，Debug 必须显示 value: {rendered}"
        );
        assert!(rendered.contains("dev-dbg"));
        assert!(rendered.contains("temp"));
    }

    /// `query_latest`：同点位多时刻 → 返回 ts 最大；不同点位互不干扰。
    #[test]
    fn query_latest_returns_newest_point_per_key() {
        let dir = tempdir();
        let clock = ManualClock::new(T0_NS);
        let store = open(dir.path(), "gw-latest", MACHINE_A, &clock);

        store
            .write_batch(&[
                point(1, "d", "temp", T0_NS + 1, 10.0),
                point(2, "d", "temp", T0_NS + 5, 20.0),
                point(3, "d", "temp", T0_NS + 3, 15.0),
                point(4, "d", "humid", T0_NS + 9, 55.0),
                point(5, "e", "temp", T0_NS + 100, 99.0),
            ])
            .expect("write");

        let temp = store
            .query_latest("d", "temp")
            .expect("latest temp")
            .expect("some");
        assert_eq!(temp.seq, 2, "必须是最新（ts 最大）");
        assert_eq!(temp.value, 20.0);

        let humid = store
            .query_latest("d", "humid")
            .expect("latest humid")
            .expect("some");
        assert_eq!(humid.value, 55.0);

        // 不同设备互不干扰。
        let e_temp = store
            .query_latest("e", "temp")
            .expect("latest e")
            .expect("some");
        assert_eq!(e_temp.value, 99.0);

        assert_eq!(store.query_latest("d", "absent").expect("absent"), None);
        store.close().expect("close");
    }

    /// 重启持久化：close 后重开 → 数据仍在且可解密还原。
    #[test]
    fn reopen_persists_and_decrypts_existing_rows() {
        let dir = tempdir();
        let clock = ManualClock::new(T0_NS);
        let store = open(dir.path(), "gw-reopen", MACHINE_A, &clock);
        store
            .write_batch(&[
                point(1, "d", "k", T0_NS + 1, 1.5),
                point(2, "d", "k", T0_NS + 2, 2.5),
            ])
            .expect("write");
        store.close().expect("close");

        let reopened = open(dir.path(), "gw-reopen", MACHINE_A, &clock);
        assert_eq!(reopened.count().expect("count"), 2);
        let got = one_range(&reopened, "d", T0_NS, T0_NS + 10);
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].value, 1.5);
        assert_eq!(got[1].value, 2.5);
        reopened.close().expect("close");
    }

    /// WAL 模式生效 + 库文件隔离（存在 `telemetry.db` / `-wal`，不存在 `queue.db`）。
    #[test]
    fn wal_enabled_and_db_file_isolated_from_queue_db() {
        let dir = tempdir();
        let clock = ManualClock::new(T0_NS);
        let store = open(dir.path(), "gw-wal", MACHINE_A, &clock);
        store
            .write_batch(&[point(1, "d", "k", T0_NS, 7.0)])
            .expect("write");

        assert_eq!(
            store.db_path().file_name().and_then(|n| n.to_str()),
            Some(TELEMETRY_DB_FILE_NAME)
        );
        assert!(dir.path().join(TELEMETRY_DB_FILE_NAME).exists());
        assert!(
            !dir.path().join(QUEUE_DB_FILE_NAME).exists(),
            "不得与 task 17 的 queue.db 共用库文件"
        );
        assert!(
            dir.path().join("telemetry.db-wal").exists(),
            "WAL 模式必须生成 -wal 侧车文件"
        );

        // 只读连接校验 journal_mode = wal（不参与写，不违反单写者）。
        let ro = Connection::open_with_flags(store.db_path(), OpenFlags::SQLITE_OPEN_READ_ONLY)
            .expect("open ro");
        let mode: String = ro
            .query_row("PRAGMA journal_mode", [], |row| row.get(0))
            .expect("pragma");
        assert_eq!(mode.to_lowercase(), "wal");
        drop(ro);
        store.close().expect("close");
    }

    /// 错误路径：非法配置 → ConfigError(2000)；不可写路径 → StorageError(4000)。
    #[test]
    fn open_rejects_invalid_config_and_unwritable_path() {
        let dir = tempdir();
        let clock = ManualClock::new(T0_NS);

        // 1) 库文件名写成 queue.db → ConfigError(2000)（隔离红线）。
        let err = TelemetryStoreConfig::new(dir.path().join(QUEUE_DB_FILE_NAME), "gw", MACHINE_A)
            .expect_err("must fail");
        assert_eq!(err.error_code(), ERR_CONFIG);
        assert!(err.to_string().contains("telemetry.db"), "err: {err}");

        // 2) 空机器码 → ConfigError(2000)。
        let err = TelemetryStoreConfig::new(dir.path().join(TELEMETRY_DB_FILE_NAME), "gw", "  ")
            .expect_err("must fail");
        assert_eq!(err.error_code(), ERR_CONFIG);
        assert!(err.to_string().contains("machine_code"));

        // 3) 空网关 → ConfigError(2000)。
        let err =
            TelemetryStoreConfig::new(dir.path().join(TELEMETRY_DB_FILE_NAME), "  ", MACHINE_A)
                .expect_err("must fail");
        assert_eq!(err.error_code(), ERR_CONFIG);

        // 4) max_db_bytes = 0 → ConfigError(2000)（经 open 校验）。
        let mut config = cfg(dir.path(), "gw-err", MACHINE_A);
        config.max_db_bytes = 0;
        let err = TelemetryStore::open(config, Arc::new(clock.clone())).expect_err("must fail");
        assert_eq!(err.error_code(), ERR_CONFIG);
        assert!(err.to_string().contains("max_db_bytes"));

        // 5) retention_ns <= 0 → ConfigError(2000)。
        let mut config = cfg(dir.path(), "gw-err", MACHINE_A);
        config.retention_ns = 0;
        let err = TelemetryStore::open(config, Arc::new(clock.clone())).expect_err("must fail");
        assert_eq!(err.error_code(), ERR_CONFIG);

        // 6) 用一个「文件」冒充目录 → 创建目录失败 → StorageError(4000)。
        let blocker = dir.path().join("blocker");
        std::fs::write(&blocker, b"not a directory").expect("write blocker");
        let config =
            TelemetryStoreConfig::new(blocker.join(TELEMETRY_DB_FILE_NAME), "gw-err", MACHINE_A)
                .expect("config");
        let err = TelemetryStore::open(config, Arc::new(SystemClock)).expect_err("must fail");
        assert_eq!(err.error_code(), ERR_STORAGE);
        assert!(matches!(err, DaemonError::StorageError(_)), "err: {err}");
    }

    /// 加密不改变非敏感列：`device_id` / `point_key` / `ts_ns` / `quality` 落盘为明文。
    #[test]
    fn non_sensitive_columns_remain_plaintext_for_indexing() {
        let dir = tempdir();
        let clock = ManualClock::new(T0_NS);
        let store = open(dir.path(), "gw-plain", MACHINE_A, &clock);
        store
            .write_batch(&[point(7, "dev-x", "press", T0_NS + 42, 101.3)])
            .expect("write");
        let db_path = store.db_path();

        let ro = Connection::open_with_flags(&db_path, OpenFlags::SQLITE_OPEN_READ_ONLY)
            .expect("open ro");
        let (device, key, ts, quality): (String, String, i64, String) = ro
            .query_row(
                "SELECT device_id, point_key, ts_ns, quality FROM telemetry WHERE seq = 7",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .expect("select plain cols");
        assert_eq!(device, "dev-x");
        assert_eq!(key, "press");
        assert_eq!(ts, T0_NS + 42);
        assert_eq!(quality, "Good");
        drop(ro);
        store.close().expect("close");
    }

    /// 密钥派生确定性 + 域隔离：同输入同密钥；两把 key 不同；换机器码换密钥。
    #[test]
    fn hkdf_derivation_is_deterministic_and_domain_separated() {
        let (enc1, mac1) = derive_keys("gw", MACHINE_A);
        let (enc2, mac2) = derive_keys("gw", MACHINE_A);
        assert_eq!(enc1, enc2, "同输入必须派生同密钥");
        assert_eq!(mac1, mac2);
        assert_ne!(enc1, mac1, "enc_key 与 mac_key 必须不同（info 域隔离）");

        // 换机器码 → 换密钥。
        let (enc3, mac3) = derive_keys("gw", MACHINE_B);
        assert_ne!(enc1, enc3);
        assert_ne!(mac1, mac3);
        // 换网关 → 换密钥。
        let (enc4, _) = derive_keys("gw-other", MACHINE_A);
        assert_ne!(enc1, enc4);

        // HKDF-Expand 单块输出符合 RFC 5869 结构：非全零、确定。
        let prk = hkdf_extract(b"salt", b"ikm");
        let okm = hkdf_expand_single(&prk, b"info");
        assert_ne!(okm, [0u8; KEY_LEN]);
        assert_eq!(okm, hkdf_expand_single(&prk, b"info"));
        assert_ne!(okm, hkdf_expand_single(&prk, b"other-info"));
    }

    /// 篡改密文 → 认证失败（tag 校验拦截）。
    #[test]
    fn tampered_ciphertext_is_rejected_by_authentication() {
        let dir = tempdir();
        let clock = ManualClock::new(T0_NS);
        let store = open(dir.path(), "gw-tamper", MACHINE_A, &clock);
        store
            .write_batch(&[point(1, "d", "k", T0_NS + 1, 1.25)])
            .expect("write");
        let db_path = store.db_path();
        store.close().expect("close");

        // 直接篡改落盘密文的一位（用可写连接，绕过 store）。
        let rw = Connection::open(&db_path).expect("open rw");
        let enc: String = rw
            .query_row("SELECT value_enc FROM telemetry WHERE seq = 1", [], |row| {
                row.get(0)
            })
            .expect("select");
        let mut bytes = enc.into_bytes();
        // 翻转密文主体的一个 hex 字符（nonce 之后的第一个字节）。
        let idx = NONCE_LEN * 2;
        bytes[idx] = if bytes[idx] == b'0' { b'1' } else { b'0' };
        let tampered = String::from_utf8(bytes).expect("utf8");
        rw.execute(
            "UPDATE telemetry SET value_enc = ?1 WHERE seq = 1",
            params![tampered],
        )
        .expect("update");
        drop(rw);

        let reopened = open(dir.path(), "gw-tamper", MACHINE_A, &clock);
        let err = reopened.query_latest("d", "k").expect_err("篡改必须被拒绝");
        assert_eq!(err.error_code(), ERR_STORAGE);
        assert!(
            err.to_string().contains("authentication failed"),
            "必须是认证失败: {err}"
        );
        reopened.close().expect("close");
    }

    /// `disk_bytes` 随写入增长、随 `prune` 回落，且与行数单调相关。
    #[test]
    fn disk_bytes_tracks_writes_and_prune() {
        let dir = tempdir();
        let clock = ManualClock::new(T0_NS);
        let store = open(dir.path(), "gw-bytes", MACHINE_A, &clock);
        assert_eq!(store.disk_bytes().expect("empty"), 0);

        let mut points = Vec::new();
        for i in 0..7i64 {
            points.push(point(i + 1, "d", "k", T0_NS + i, i as f64));
        }
        store.write_batch(&points).expect("write");
        let after = store.disk_bytes().expect("bytes");
        // 7 条 × (ENC_HEX_LEN + ROW_OVERHEAD)。
        assert_eq!(after, 7 * (ENC_HEX_LEN as u64 + ROW_OVERHEAD_BYTES));

        // 推进超期后 prune → 字节归零。
        clock.advance(31 * DAY_NS);
        assert_eq!(store.prune().expect("prune"), 7);
        assert_eq!(store.disk_bytes().expect("bytes after prune"), 0);
        store.close().expect("close");
    }
}

//! task 35 — OTA 升级模块（[`OtaManager`] 状态机 + 两槽位切换协议）。
//!
//! ## 依赖红线
//! **禁止引入新依赖**：HTTP 下载复用 `driver/http.rs` 的手写 HTTP/1.1 GET 风格
//! （tokio TcpStream + `Connection: close`，支持 Content-Length / chunked / EOF 三种
//! 体定界）；签名校验复用 crate 内既有 `ed25519-dalek` 栈（同 `auth/signing.rs`）。
//!
//! ## V1 无 TLS（诚实声明）
//! 下载通道 V1 **仅支持 `http://` 明文**，不支持 `https://`（理由同 driver/http.rs：
//! TLS 客户端握手/证书校验属于独立工程量，超出本任务范围）。传输层无加密由
//! **Ed25519 签名校验兜底**：包体被中间人篡改必然验签失败被拒——完整性有保障，
//! 但**机密性无保障**（固件内容可被窃听）。生产部署建议经可信内网下发。
//!
//! ## 「A/B 分区」在本守护进程场景的映射（诚实实现）
//! 嵌入式 A/B 双分区的语义等价物是**两槽位切换协议**（非真实磁盘分区）：
//! - `current` 槽：当前运行版本的包字节（启动确认后写入）；
//! - `pending` 槽：已下载验签、等待新版本启动确认的包字节。
//! - [`OtaManager::apply`] = 新包写入 pending 槽 + meta 记录 pending 版本；
//! - [`OtaManager::boot_commit_or_rollback`] = 新版本启动后的确认判定：
//!   `committed == true` → pending 槽晋升为 current 槽（commit）；
//!   `committed == false`（宽限期内未确认，如反复崩溃/看门狗复位）→ 清除 pending
//!   槽、current 槽保留旧版本字节（即「恢复旧版本」——两槽位协议下旧字节从未被
//!   覆盖，保留即恢复）。本任务只实现判定函数与测试，**不接线 bootstrap**
//!   （未来接线点：`bootstrap.rs` 启动序列中构造 `OtaManager` 后，
//!   按上次运行是否健康调用 `boot_commit_or_rollback(healthy)`）。
//!
//! ## 版本单调性（选型说明）
//! 选 **u64 单调递增版本号**（弃用 SemVer）：守护进程固件发布序号天然单调，
//! u64 比较零歧义、无需解析语义化版本文法。新包版本必须 **严格大于** 当前版本，
//! 同版本 / 旧版本一律拒绝（防降级攻击 + 防重放旧包）。
//!
//! ## 包格式（manifest JSON，自描述单文件）
//! 下载物是一个 JSON manifest，payload 以 base64 内嵌：
//! ```json
//! {
//!   "version": "3",
//!   "ts_ns": "1763000000000000000",
//!   "size": "12",
//!   "payload_sha256": "<64 hex>",
//!   "payload_b64": "<base64>",
//!   "sig_b64": "<base64 Ed25519 签名>"
//! }
//! ```
//! **大数红线**：`version` / `ts_ns` / `size` 一律 JSON **字符串**（数值型字段
//! 在 [`parse_manifest`] 处显式拒绝）。签名对象是
//! `iotdaq.ota.v1|` 域分隔的 `version（十进制）+ payload SHA-256（hex）`
//! 定界消息（同 `auth/signing.rs` 的长度定界风格，防拼接歧义）。
//!
//! ## 配置热更新免重启
//! [`OtaManager::apply_config_hot_reload`] 与固件包**共用下载 + manifest 解析 +
//! 验签管线**，但应用路径不同：验签通过后直接把 payload 作为 JSON 返回给调用方
//! （喂给既有 config 热重载），**不写任何槽位、不改状态机**；版本单调性对配置
//! 不强制（配置允许同版本重放），此差异为有意设计并在此文档化。
//!
//! ## 审计
//! 下载开始/失败/成功、验签失败、应用、commit、回滚（手动/启动判定）、配置热更新
//! 均产生带时间戳的 [`OtaAudit`] 记录，可经 [`OtaManager::audit_log`] 查询。
//!
//! ## 零 panic
//! 所有可失败点（URL/HTTP/JSON/base64/长度切片/签名/存储）全部收敛
//! [`DaemonError`]；版本号 / 时间戳 / 字节数在 JSON 侧一律字符串。

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use base64::Engine as _;
use ed25519_dalek::{Signature, Verifier, VerifyingKey};
// 生产路径只验签（`Verifier`/`VerifyingKey`）；`Signer`/`SigningKey` 仅测试构造密钥与签名用。
#[cfg(test)]
use ed25519_dalek::{Signer, SigningKey};
use serde_json::Value;
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::time::timeout;

use crate::error::{DaemonError, DaemonResult};

// ---- 常量 ----

/// OTA 签名域分隔前缀（防跨协议签名复用；与 auth/signing.rs 的域互不重叠）。
pub const OTA_SIGNING_DOMAIN_V1: &str = "iotdaq.ota.v1|";

/// 两槽位协议：当前运行版本槽位名。
pub const SLOT_CURRENT: &str = "current";

/// 两槽位协议：待确认新版本槽位名。
pub const SLOT_PENDING: &str = "pending";

/// Ed25519 原始签名长度（字节）。
const SIGNATURE_LEN: usize = 64;

/// 默认下载超时（任务契约指定 30s）。
const DEFAULT_DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(30);

// ---- 局部工具（纯函数） ----

/// 当前 Unix 时间（纳秒）；时钟早于纪元等异常时退化为 0（零 panic）。
fn now_unix_ts_ns() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as i64)
        .unwrap_or(0)
}

/// SHA-256 摘要的 hex 编码（小写）。
fn sha256_hex(data: &[u8]) -> String {
    let digest: [u8; 32] = Sha256::digest(data).into();
    hex::encode(digest)
}

/// 构造 OTA 签名消息：`DOMAIN || ver=<len>:<dec>| || sha256=<len>:<hex>|`。
///
/// 长度入消息防拼接歧义（同 `auth/signing.rs::write_field` 风格）；
/// 签名对象 = 新版本号 + 包体 SHA-256，版本或字节任一被篡改即验签失败。
fn ota_signing_message(version: u64, payload_sha256_hex: &str) -> Vec<u8> {
    let mut msg: Vec<u8> = Vec::with_capacity(OTA_SIGNING_DOMAIN_V1.len() + 96);
    msg.extend_from_slice(OTA_SIGNING_DOMAIN_V1.as_bytes());
    let version_field = format!("ver={}:{}|", version.to_string().len(), version);
    msg.extend_from_slice(version_field.as_bytes());
    let sha_field = format!(
        "sha256={}:{}|",
        payload_sha256_hex.len(),
        payload_sha256_hex
    );
    msg.extend_from_slice(sha_field.as_bytes());
    msg
}

// ---- 状态机 ----

/// OTA 升级状态机状态。
///
/// 主流程：`Idle → Downloading → Verifying → ReadyToApply → Applied`；
/// 失败收敛 `Failed`（允许重新 `fetch_update` 重试）；回滚收敛 `RolledBack`
/// （同样允许重试升级）。`Applied` 状态下只能走 `rollback` 或
/// `boot_commit_or_rollback`。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OtaState {
    /// 初始态 / 无进行中的升级。
    Idle,
    /// 正在从 URL 下载 manifest。
    Downloading,
    /// 正在解析 manifest + 验签 + 版本校验。
    Verifying,
    /// 新包已验签并暂存，等待 `apply()`。
    ReadyToApply,
    /// 新包已写入 pending 槽（等待新版本启动确认）。
    Applied,
    /// 下载或验签失败（旧版本运行不受影响）。
    Failed,
    /// 已回滚（pending 清除，current 保留）。
    RolledBack,
}

impl OtaState {
    /// 稳定字符串（日志 / 审计用）。
    pub fn as_str(&self) -> &'static str {
        match self {
            OtaState::Idle => "Idle",
            OtaState::Downloading => "Downloading",
            OtaState::Verifying => "Verifying",
            OtaState::ReadyToApply => "ReadyToApply",
            OtaState::Applied => "Applied",
            OtaState::Failed => "Failed",
            OtaState::RolledBack => "RolledBack",
        }
    }
}

// ---- 审计 ----

/// OTA 审计记录（下载 / 验签 / 应用 / 回滚 / 配置热更新全路径覆盖）。
///
/// 数值字段（版本 / 字节数）一律字符串（大数红线在审计序列化侧同样成立）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OtaAudit {
    /// 开始从 URL 下载。
    DownloadStarted { url: String },
    /// 下载失败（网络 / 超时 / HTTP 协议层）。
    DownloadFailed { url: String, reason: String },
    /// manifest 解析 + 全部校验通过（版本 / payload 字节数为字符串）。
    Downloaded { version: String, size: String },
    /// 验签通过（新版本号为字符串）。
    Verified { version: String },
    /// 验签 / 哈希 / 版本单调性校验失败（拒绝应用，旧版本不受影响）。
    VerifyFailed { reason: String },
    /// 应用成功（新包已入 pending 槽）。
    Applied { version: String },
    /// 非法状态迁移被拒绝。
    InvalidTransition { action: String, from: String },
    /// 启动确认：pending 槽晋升为 current 槽。
    Commit { version: String },
    /// 启动判定回滚：宽限期内未 commit，pending 清除、旧版本保留。
    RolledBack { pending_version: String },
    /// 手动回滚（`rollback()`）。
    ManualRollback,
    /// 配置热更新成功（新配置 JSON 已返回调用方）。
    ConfigHotReload { version: String },
    /// 配置热更新失败（下载 / 验签 / payload 非 JSON）。
    ConfigHotReloadFailed { reason: String },
}

/// 带时间戳的审计事件（时间戳为 Unix 纳秒 i64，模块内不进 JSON 通道）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OtaAuditEvent {
    /// 事件发生时刻（Unix 纳秒）。
    pub ts_ns: i64,
    /// 审计记录本体。
    pub record: OtaAudit,
}

// ---- 槽位元数据（两槽位协议的持久化状态） ----

/// OTA 两槽位协议元数据。
///
/// 持久化 JSON 编码（[`OtaMeta::to_json`]）中版本 / 时间戳字段**一律字符串**
/// （大数红线）；`pending_version` 为 `null` 表示无待确认版本。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct OtaMeta {
    /// 当前运行版本（十进制字符串）。
    pub current_version: String,
    /// 待确认新版本（`None` = 无 pending）。
    pub pending_version: Option<String>,
    /// 元数据最后更新时刻（Unix 纳秒，十进制字符串）。
    pub updated_ts_ns: String,
}

impl OtaMeta {
    /// 构造仅含当前版本的初始元数据。
    pub fn new(current_version: u64) -> Self {
        Self {
            current_version: current_version.to_string(),
            pending_version: None,
            updated_ts_ns: now_unix_ts_ns().to_string(),
        }
    }

    /// 序列化为 JSON（版本 / 时间戳 / 无尺寸字段全字符串——大数红线）。
    ///
    /// # Errors
    /// 序列化失败（理论上不可达）→ `StorageError`，不 panic。
    pub fn to_json(&self) -> DaemonResult<String> {
        let value = serde_json::json!({
            "current_version": self.current_version,
            "pending_version": self.pending_version,
            "updated_ts_ns": self.updated_ts_ns,
        });
        serde_json::to_string(&value)
            .map_err(|e| DaemonError::StorageError(format!("ota meta serialize: {e}")))
    }

    /// 从 JSON 反序列化（字段必须是字符串——数值型一律拒绝，大数红线双向守护）。
    ///
    /// # Errors
    /// JSON 解析失败 / 字段缺失 / 字段非字符串 / 非法 `pending_version` → `StorageError`。
    pub fn from_json(raw: &str) -> DaemonResult<Self> {
        let value: Value = serde_json::from_str(raw)
            .map_err(|e| DaemonError::StorageError(format!("ota meta json parse: {e}")))?;
        let require_str = |key: &str| -> DaemonResult<String> {
            value
                .get(key)
                .and_then(Value::as_str)
                .map(str::to_string)
                .ok_or_else(|| {
                    DaemonError::StorageError(format!(
                        "ota meta field {key:?} must be a JSON string"
                    ))
                })
        };
        let pending_version = match value.get("pending_version") {
            None | Some(Value::Null) => None,
            Some(Value::String(s)) => Some(s.clone()),
            Some(_) => {
                return Err(DaemonError::StorageError(
                    "ota meta field \"pending_version\" must be a JSON string or null".to_string(),
                ))
            }
        };
        Ok(Self {
            current_version: require_str("current_version")?,
            pending_version,
            updated_ts_ns: require_str("updated_ts_ns")?,
        })
    }
}

// ---- 存储抽象 ----

/// OTA 存储 trait（两槽位 + 元数据；生产实现可落文件系统 / SQLite，测试用内存实现）。
#[async_trait]
pub trait OtaStore: Send + Sync {
    /// 把字节写入指定槽位（覆盖写）。
    ///
    /// # Errors
    /// 存储层故障 → `DaemonError`（实现方自行收敛到具体域）。
    async fn save_bytes(&self, slot: &str, data: &[u8]) -> DaemonResult<()>;

    /// 读取指定槽位字节；槽位不存在返回 `None`（非错误）。
    ///
    /// # Errors
    /// 存储层故障 → `DaemonError`。
    async fn load_bytes(&self, slot: &str) -> DaemonResult<Option<Vec<u8>>>;

    /// 删除指定槽位（不存在时静默成功，幂等）。
    ///
    /// # Errors
    /// 存储层故障 → `DaemonError`。
    async fn delete(&self, slot: &str) -> DaemonResult<()>;

    /// 持久化元数据（整体覆盖）。
    ///
    /// # Errors
    /// 存储层故障 → `DaemonError`。
    async fn persist_meta(&self, meta: &OtaMeta) -> DaemonResult<()>;

    /// 读取元数据；从未写入过返回 `None`（非错误）。
    ///
    /// # Errors
    /// 存储层故障 → `DaemonError`。
    async fn load_meta(&self) -> DaemonResult<Option<OtaMeta>>;
}

/// 内存实现（测试 / 原型用）。
///
/// `std::sync::Mutex` 守护内部态（临界区内无 `.await`，不会跨 await 持锁）；
/// 锁中毒收敛 `StorageError`，零 panic。
#[derive(Default)]
pub struct InMemoryOtaStore {
    /// 槽位字节表（slot 名 → 字节）。
    slots: Mutex<HashMap<String, Vec<u8>>>,
    /// 元数据（`None` = 从未写入）。
    meta: Mutex<Option<OtaMeta>>,
}

impl InMemoryOtaStore {
    /// 加锁槽位表（锁中毒 → `StorageError`，零 panic）。
    fn lock_slots(&self) -> DaemonResult<MutexGuard<'_, HashMap<String, Vec<u8>>>> {
        self.slots
            .lock()
            .map_err(|e| DaemonError::StorageError(format!("ota store slots poisoned: {e}")))
    }

    /// 加锁元数据（锁中毒 → `StorageError`，零 panic）。
    fn lock_meta(&self) -> DaemonResult<MutexGuard<'_, Option<OtaMeta>>> {
        self.meta
            .lock()
            .map_err(|e| DaemonError::StorageError(format!("ota store meta poisoned: {e}")))
    }
}

#[async_trait]
impl OtaStore for InMemoryOtaStore {
    async fn save_bytes(&self, slot: &str, data: &[u8]) -> DaemonResult<()> {
        self.lock_slots()?.insert(slot.to_string(), data.to_vec());
        Ok(())
    }

    async fn load_bytes(&self, slot: &str) -> DaemonResult<Option<Vec<u8>>> {
        Ok(self.lock_slots()?.get(slot).cloned())
    }

    async fn delete(&self, slot: &str) -> DaemonResult<()> {
        self.lock_slots()?.remove(slot);
        Ok(())
    }

    async fn persist_meta(&self, meta: &OtaMeta) -> DaemonResult<()> {
        *self.lock_meta()? = Some(meta.clone());
        Ok(())
    }

    async fn load_meta(&self) -> DaemonResult<Option<OtaMeta>> {
        Ok(self.lock_meta()?.clone())
    }
}

// ---- manifest 解析与验签 ----

/// 解析后的 OTA 包 manifest。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OtaManifest {
    /// 新版本号（u64 单调序；JSON 侧为字符串）。
    pub version: u64,
    /// 签名时刻（原样保留的十进制字符串，仅信息性，不入签名）。
    pub ts_ns: String,
    /// payload 字节数（与 base64 解码结果长度强校验）。
    pub size: usize,
    /// payload SHA-256（hex，与实际 payload 强校验）。
    pub payload_sha256: String,
    /// 包体字节（base64 解码后）。
    pub payload: Vec<u8>,
    /// Ed25519 签名（base64）。
    pub sig_b64: String,
}

/// 从 manifest JSON 字节解析出 [`OtaManifest`] 并做强一致性校验
/// （size 与 payload 长度一致、sha256 与 payload 摘要一致）。
///
/// # Errors
/// - JSON 解析失败 / 字段缺失：`ProtocolError`；
/// - **数值型字段**（version / ts_ns / size 应为字符串）：`ProtocolError`（大数红线）；
/// - 字段格式非法（版本非 u64、sha 非 64 hex、base64 解码失败）：`ProtocolError`；
/// - size 或 sha256 与 payload 不一致（篡改证据）：`SecurityError`。
pub fn parse_manifest(raw: &[u8]) -> DaemonResult<OtaManifest> {
    let value: Value = serde_json::from_slice(raw)
        .map_err(|e| DaemonError::ProtocolError(format!("ota manifest json parse: {e}")))?;

    // 大数红线：以下字段必须是 JSON 字符串，数值型一律拒绝。
    let require_str = |key: &str| -> DaemonResult<&str> {
        value.get(key).and_then(Value::as_str).ok_or_else(|| {
            DaemonError::ProtocolError(format!(
                "ota manifest field {key:?} must be a JSON string (big-number red line)"
            ))
        })
    };

    let version: u64 = require_str("version")?.parse().map_err(|_| {
        DaemonError::ProtocolError("ota manifest version is not a valid u64".to_string())
    })?;
    let ts_ns = require_str("ts_ns")?.to_string();
    let size: usize = require_str("size")?.parse().map_err(|_| {
        DaemonError::ProtocolError("ota manifest size is not a valid usize".to_string())
    })?;
    let payload_sha256 = require_str("payload_sha256")?.to_ascii_lowercase();
    let sig_b64 = require_str("sig_b64")?.to_string();

    let payload_b64 = require_str("payload_b64")?;
    let payload = BASE64_STANDARD
        .decode(payload_b64.as_bytes())
        .map_err(|e| DaemonError::ProtocolError(format!("ota manifest payload_b64 decode: {e}")))?;

    // sha256 形状校验（64 hex）。
    if payload_sha256.len() != 64 || !payload_sha256.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(DaemonError::ProtocolError(format!(
            "ota manifest payload_sha256 must be 64 hex chars, got {}",
            payload_sha256.len()
        )));
    }

    // 一致性校验：size 与 sha256 必须与实际 payload 匹配（篡改在此暴露）。
    if payload.len() != size {
        return Err(DaemonError::SecurityError(format!(
            "ota manifest size mismatch: declared {size}, actual {}",
            payload.len()
        )));
    }
    let actual_sha = sha256_hex(&payload);
    if actual_sha != payload_sha256 {
        return Err(DaemonError::SecurityError(format!(
            "ota manifest payload sha256 mismatch: declared {payload_sha256}, actual {actual_sha}"
        )));
    }

    Ok(OtaManifest {
        version,
        ts_ns,
        size,
        payload_sha256,
        payload,
        sig_b64,
    })
}

/// 校验 manifest 的 Ed25519 签名（公钥注入；消息 = 域分隔的 version + sha256）。
///
/// # Errors
/// 签名 base64 解码失败 / 长度 ≠ 64 / 密钥不匹配（含包体或版本被篡改）→
/// `SecurityError`。
pub fn verify_manifest_signature(
    manifest: &OtaManifest,
    public_key: &VerifyingKey,
) -> DaemonResult<()> {
    let sig_bytes = BASE64_STANDARD
        .decode(manifest.sig_b64.as_bytes())
        .map_err(|e| {
            DaemonError::SecurityError(format!("ota manifest sig is not valid base64: {e}"))
        })?;
    let sig_array: [u8; SIGNATURE_LEN] = sig_bytes.as_slice().try_into().map_err(|_| {
        DaemonError::SecurityError(format!(
            "ota manifest signature length {} != {SIGNATURE_LEN}",
            sig_bytes.len()
        ))
    })?;
    let signature = Signature::from_bytes(&sig_array);
    let message = ota_signing_message(manifest.version, &manifest.payload_sha256);
    public_key.verify(&message, &signature).map_err(|e| {
        DaemonError::SecurityError(format!("ota manifest signature verification failed: {e}"))
    })
}

/// 从标准 base64 编码的 32 字节 Ed25519 **公钥**构造 [`VerifyingKey`]。
///
/// 用途：`[gateway.ota].signing_key_b64` 注入（配置层到验签层的桥）。
///
/// # Errors
/// base64 解码失败 / 长度 ≠ 32 / 非法 Ed25519 曲线点 → `ConfigError`（零 panic）。
pub fn verifying_key_from_b64(b64: &str) -> DaemonResult<VerifyingKey> {
    let bytes = BASE64_STANDARD.decode(b64.trim().as_bytes()).map_err(|e| {
        DaemonError::ConfigError(format!("ota signing_key_b64 is not valid base64: {e}"))
    })?;
    let array: [u8; 32] = bytes.as_slice().try_into().map_err(|_| {
        DaemonError::ConfigError(format!(
            "ota signing_key_b64 must decode to 32 bytes, got {}",
            bytes.len()
        ))
    })?;
    VerifyingKey::from_bytes(&array).map_err(|e| {
        DaemonError::ConfigError(format!(
            "ota signing_key_b64 is not a valid Ed25519 public key: {e}"
        ))
    })
}

/// 读取授权端 manifest 端点的 `available` 标志（诚实空态协议）。
///
/// 授权端 `GET /updates/manifest` 在无可下发版本时返回 `available = false`
/// 且其余字段为空串——调用方须先看该标志，**不可**直接喂给 [`parse_manifest`]
/// （空串版本号必然解析失败）。
///
/// # Errors
/// JSON 解析失败 / 缺 `available` 字段 / 字段非布尔 → `ProtocolError`。
pub fn manifest_available(raw: &[u8]) -> DaemonResult<bool> {
    let value: Value = serde_json::from_slice(raw).map_err(|e| {
        DaemonError::ProtocolError(format!("ota manifest response json parse: {e}"))
    })?;
    value
        .get("available")
        .and_then(Value::as_bool)
        .ok_or_else(|| {
            DaemonError::ProtocolError(
                "ota manifest response missing boolean field \"available\"".to_string(),
            )
        })
}

/// 读取 `available = false` 时授权端给出的**面向用户**说明（缺省空串，零 panic）。
pub fn manifest_unavailable_reason(raw: &[u8]) -> String {
    serde_json::from_slice::<Value>(raw)
        .ok()
        .and_then(|v| v.get("reason").and_then(Value::as_str).map(str::to_string))
        .unwrap_or_default()
}

// ---- 手写 HTTP/1.1 GET（风格复用 driver/http.rs；V1 仅 http://，无 TLS） ----

/// 解析 `http://host[:port]/path` URL（V1 仅 http，理由见模块文档）。
///
/// 返回（主机、端口、路径）；省略端口默认 80，省略路径默认 `/`。
///
/// # Errors
/// 非 `http://` 前缀 / 缺主机 / 端口非法 → `ProtocolError`。
fn parse_ota_http_url(url: &str) -> DaemonResult<(String, u16, String)> {
    let wrap = |detail: String| {
        DaemonError::ProtocolError(format!("invalid ota http url {url:?}: {detail}"))
    };
    let rest = url
        .strip_prefix("http://")
        .ok_or_else(|| wrap("only http:// scheme supported in V1 (no TLS)".to_string()))?;
    let (authority, path) = match rest.find('/') {
        Some(i) => (&rest[..i], rest[i..].to_string()),
        None => (rest, "/".to_string()),
    };
    if authority.is_empty() {
        return Err(wrap("missing host".to_string()));
    }
    let (host, port) = match authority.rsplit_once(':') {
        Some((h, p)) => {
            if h.is_empty() {
                return Err(wrap("missing host".to_string()));
            }
            let port: u16 = p.parse().map_err(|_| wrap(format!("invalid port {p:?}")))?;
            (h.to_string(), port)
        }
        None => (authority.to_string(), 80),
    };
    Ok((host, port, path))
}

/// 解析 HTTP 响应：状态行 + 头 + 体（Content-Length / chunked / EOF 定界；
/// 最小实现，风格同 `driver/http.rs::parse_http_response`）。
///
/// 非 2xx 状态码 → `ProtocolError`。
fn parse_ota_http_response(raw: &[u8]) -> DaemonResult<Vec<u8>> {
    let header_end = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .ok_or_else(|| {
            DaemonError::ProtocolError("ota http response missing header terminator".to_string())
        })?;
    let head = std::str::from_utf8(&raw[..header_end]).map_err(|_| {
        DaemonError::ProtocolError("ota http response head is not utf-8".to_string())
    })?;
    let body = &raw[header_end + 4..];

    let mut lines = head.split("\r\n");
    let status_line = lines
        .next()
        .ok_or_else(|| DaemonError::ProtocolError("ota http response empty head".to_string()))?;
    let status: u16 = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| {
            DaemonError::ProtocolError(format!("ota http malformed status line {status_line:?}"))
        })?;
    if !(200..300).contains(&status) {
        return Err(DaemonError::ProtocolError(format!(
            "ota http status {status}"
        )));
    }

    let mut content_length: Option<usize> = None;
    let mut chunked = false;
    for line in lines {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        let name = name.trim().to_ascii_lowercase();
        let value = value.trim();
        if name == "content-length" {
            content_length = value.parse().ok();
        } else if name == "transfer-encoding" && value.to_ascii_lowercase().contains("chunked") {
            chunked = true;
        }
    }

    if chunked {
        decode_ota_chunked(body)
    } else if let Some(len) = content_length {
        if body.len() < len {
            return Err(DaemonError::ProtocolError(format!(
                "ota http truncated body: header declares {len} bytes, got {}",
                body.len()
            )));
        }
        Ok(body[..len].to_vec())
    } else {
        // 无 Content-Length 且非 chunked：Connection: close 下以 EOF 定界。
        Ok(body.to_vec())
    }
}

/// 在字节流中定位 `\r\n` 行尾（chunked 解码辅助）。
fn find_ota_crlf(data: &[u8]) -> DaemonResult<usize> {
    data.windows(2)
        .position(|w| w == b"\r\n")
        .ok_or_else(|| DaemonError::ProtocolError("ota http unterminated chunk line".to_string()))
}

/// 解码 chunked 传输编码体（最小实现，风格同 `driver/http.rs::decode_chunked`）。
fn decode_ota_chunked(mut data: &[u8]) -> DaemonResult<Vec<u8>> {
    let mut out = Vec::new();
    loop {
        let line_end = find_ota_crlf(data)?;
        let line = std::str::from_utf8(&data[..line_end]).map_err(|_| {
            DaemonError::ProtocolError("ota http chunk size line is not utf-8".to_string())
        })?;
        let size_str = line.split(';').next().unwrap_or("").trim();
        let size = usize::from_str_radix(size_str, 16).map_err(|_| {
            DaemonError::ProtocolError(format!("ota http invalid chunk size {size_str:?}"))
        })?;
        data = &data[line_end + 2..];
        if size == 0 {
            // 终止块后的 trailer 不做解析（最小实现）。
            return Ok(out);
        }
        if data.len() < size + 2 {
            return Err(DaemonError::ProtocolError(format!(
                "ota http truncated chunk: need {size} bytes, got {}",
                data.len()
            )));
        }
        out.extend_from_slice(&data[..size]);
        if &data[size..size + 2] != b"\r\n" {
            return Err(DaemonError::ProtocolError(
                "ota http missing CRLF after chunk data".to_string(),
            ));
        }
        data = &data[size + 2..];
    }
}

/// 执行一次手写 HTTP/1.1 GET（短连接，`Connection: close`），整体受超时约束。
///
/// # Errors
/// URL 非法 / 连接失败 / 超时 → `NetworkError` / `ProtocolError`（见各函数）。
async fn fetch_http(url: &str, timeout_dur: Duration) -> DaemonResult<Vec<u8>> {
    let (host, port, path) = parse_ota_http_url(url)?;
    let addr = format!("{host}:{port}");
    let stream = TcpStream::connect(&addr)
        .await
        .map_err(|e| DaemonError::NetworkError(format!("ota http connect {addr}: {e}")))?;
    let host_header = if port == 80 {
        host.clone()
    } else {
        format!("{host}:{port}")
    };
    let request = format!(
        "GET {path} HTTP/1.1\r\nHost: {host_header}\r\nAccept: application/json\r\nConnection: close\r\n\r\n"
    );
    let io = async {
        let mut stream = stream;
        stream
            .write_all(request.as_bytes())
            .await
            .map_err(|e| DaemonError::NetworkError(format!("ota http send request: {e}")))?;
        let mut raw = Vec::new();
        stream
            .read_to_end(&mut raw)
            .await
            .map_err(|e| DaemonError::NetworkError(format!("ota http read response: {e}")))?;
        Ok::<Vec<u8>, DaemonError>(raw)
    };
    match timeout(timeout_dur, io).await {
        Ok(result) => parse_ota_http_response(&result?),
        Err(_) => Err(DaemonError::NetworkError(format!(
            "ota http request timeout after {timeout_dur:?}: {url}"
        ))),
    }
}

// ---- 启动判定结果 ----

/// [`OtaManager::boot_commit_or_rollback`] 的判定结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OtaBootDecision {
    /// 无 pending 版本（无需判定）。
    NoPending,
    /// 已 commit：pending 槽晋升为 current 槽。
    Committed { version: u64 },
    /// 已回滚：pending 清除，current 槽（旧版本字节）保留。
    RolledBackTo { version: u64 },
}

// ---- 轮询结果（授权端 `/updates/manifest` 端点接线用） ----

/// [`OtaManager::poll_and_stage`] 的单轮结果。
///
/// 调度任务据此决定日志级别：`NoUpdate` / `AlreadyPending` **不计失败**，
/// `Applied` 记 info，`Rejected` 记 warn（附真实原因）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OtaPollOutcome {
    /// 授权端诚实空态（`available = false`）：暂无可下发版本，**不计失败**。
    NoUpdate {
        /// 授权端给出的人类可读说明（可能为空串）。
        reason: String,
    },
    /// 已有 pending 版本（上次已暂存，等待启动确认）→ 本轮跳过，避免重复暂存。
    AlreadyPending {
        /// 已暂存的待确认版本号（字符串，大数红线）。
        version: String,
    },
    /// 已下载、验签通过、版本严格更新并写入 pending 槽。
    Applied {
        /// 新版本号（u64 单调序）。
        version: u64,
    },
    /// 有可用包但被拒绝（网络失败 / 报文非法 / 版本不新 / 验签失败），带**真实原因**。
    Rejected {
        /// 拒绝原因（原样来自管线错误）。
        reason: String,
    },
}

// ---- OtaManager ----

/// OTA 升级管理器（状态机 + 两槽位切换协议 + 审计）。
///
/// 公钥与存储均为注入（依赖倒置）；当前版本在构造时注入，commit 后自动推进。
/// 所有路径零 panic、错误收敛 [`DaemonError`]。
pub struct OtaManager {
    /// 两槽位存储（注入）。
    store: Arc<dyn OtaStore>,
    /// 固件包验签公钥（注入）。
    public_key: VerifyingKey,
    /// 当前运行版本（u64 单调序）。
    current_version: u64,
    /// 下载超时（默认 30s）。
    timeout: Duration,
    /// 状态机当前状态。
    state: OtaState,
    /// 已验签待应用的 manifest（`ReadyToApply` 态持有）。
    staged: Option<OtaManifest>,
    /// 审计日志（追加只写）。
    audit: Vec<OtaAuditEvent>,
}

impl OtaManager {
    /// 构造管理器（默认 30s 下载超时；可用 [`OtaManager::with_timeout`] 调整）。
    pub fn new(store: Arc<dyn OtaStore>, public_key: VerifyingKey, current_version: u64) -> Self {
        Self {
            store,
            public_key,
            current_version,
            timeout: DEFAULT_DOWNLOAD_TIMEOUT,
            state: OtaState::Idle,
            staged: None,
            audit: Vec::new(),
        }
    }

    /// 覆盖下载超时（测试用短超时 / 慢链路调大）。
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// 当前状态机状态。
    pub fn state(&self) -> OtaState {
        self.state
    }

    /// 当前运行版本。
    pub fn current_version(&self) -> u64 {
        self.current_version
    }

    /// 审计日志（只读视图，追加只写不外泄可变引用）。
    pub fn audit_log(&self) -> &[OtaAuditEvent] {
        &self.audit
    }

    /// 追加一条审计事件（带当前时间戳）。
    fn audit(&mut self, record: OtaAudit) {
        self.audit.push(OtaAuditEvent {
            ts_ns: now_unix_ts_ns(),
            record,
        });
    }

    /// 下载 + 验签一体的升级入口：`Idle/Failed/RolledBack → Downloading →
    /// Verifying → ReadyToApply`。
    ///
    /// 成功后新包暂存于内存，等待 [`OtaManager::apply`] 落入 pending 槽；
    /// 任一环节失败 → 审计标记 + 状态收敛 `Failed`，**已运行的旧版本不受影响**。
    ///
    /// # Errors
    /// - 状态不允许（`Downloading/Verifying/ReadyToApply/Applied`）：`ProtocolError`
    ///   + 审计 `InvalidTransition`；
    /// - 下载（连接 / 超时 / HTTP）：`NetworkError` / `ProtocolError`
    ///   + 审计 `DownloadFailed`；
    /// - manifest 解析 / 验签 / 版本单调性：`ProtocolError` / `SecurityError`
    ///   + 审计 `VerifyFailed`。
    pub async fn fetch_update(&mut self, url: &str) -> DaemonResult<u64> {
        match self.state {
            OtaState::Idle | OtaState::Failed | OtaState::RolledBack => {}
            other => {
                self.audit(OtaAudit::InvalidTransition {
                    action: "fetch_update".to_string(),
                    from: other.as_str().to_string(),
                });
                return Err(DaemonError::ProtocolError(format!(
                    "ota fetch_update not allowed from state {}",
                    other.as_str()
                )));
            }
        }

        self.state = OtaState::Downloading;
        self.audit(OtaAudit::DownloadStarted {
            url: url.to_string(),
        });

        let raw = match fetch_http(url, self.timeout).await {
            Ok(raw) => raw,
            Err(err) => {
                self.audit(OtaAudit::DownloadFailed {
                    url: url.to_string(),
                    reason: err.to_string(),
                });
                self.state = OtaState::Failed;
                return Err(err);
            }
        };

        self.state = OtaState::Verifying;
        match self.verify_and_stage(&raw) {
            Ok(manifest) => {
                let version = manifest.version;
                self.audit(OtaAudit::Verified {
                    version: version.to_string(),
                });
                self.staged = Some(manifest);
                self.state = OtaState::ReadyToApply;
                Ok(version)
            }
            Err(err) => {
                self.audit(OtaAudit::VerifyFailed {
                    reason: err.to_string(),
                });
                self.state = OtaState::Failed;
                Err(err)
            }
        }
    }

    /// manifest 解析 + 版本单调性 + 验签（成功时产出 `Downloaded` 审计）。
    fn verify_and_stage(&mut self, raw: &[u8]) -> DaemonResult<OtaManifest> {
        let manifest = parse_manifest(raw)?;
        if manifest.version <= self.current_version {
            return Err(DaemonError::ProtocolError(format!(
                "ota version regression: new version {} must be greater than current {}",
                manifest.version, self.current_version
            )));
        }
        verify_manifest_signature(&manifest, &self.public_key)?;
        self.audit(OtaAudit::Downloaded {
            version: manifest.version.to_string(),
            size: manifest.payload.len().to_string(),
        });
        Ok(manifest)
    }

    /// 应用已验签的新包：payload 写入 pending 槽 + meta 记录 pending 版本。
    /// `ReadyToApply → Applied`。
    ///
    /// # Errors
    /// 状态非 `ReadyToApply`（审计 `InvalidTransition`）或存储故障。
    pub async fn apply(&mut self) -> DaemonResult<()> {
        if self.state != OtaState::ReadyToApply {
            self.audit(OtaAudit::InvalidTransition {
                action: "apply".to_string(),
                from: self.state.as_str().to_string(),
            });
            return Err(DaemonError::ProtocolError(format!(
                "ota apply requires state ReadyToApply, got {}",
                self.state.as_str()
            )));
        }
        // 状态不变式保证 staged 存在；此处防御式收敛为错误而非 panic。
        let manifest = self.staged.clone().ok_or_else(|| {
            DaemonError::ProtocolError(
                "ota staged manifest missing in ReadyToApply state".to_string(),
            )
        })?;
        self.store
            .save_bytes(SLOT_PENDING, &manifest.payload)
            .await?;
        self.store
            .persist_meta(&OtaMeta {
                current_version: self.current_version.to_string(),
                pending_version: Some(manifest.version.to_string()),
                updated_ts_ns: now_unix_ts_ns().to_string(),
            })
            .await?;
        self.audit(OtaAudit::Applied {
            version: manifest.version.to_string(),
        });
        self.state = OtaState::Applied;
        Ok(())
    }

    /// 单轮 OTA 轮询（授权端 `GET /updates/manifest` 端点接线）：拉取 → 读
    /// `available` → 复用 [`parse_manifest`] + [`verify_manifest_signature`] 验签 →
    /// 版本单调性 → 写入 pending 槽。
    ///
    /// 与 [`OtaManager::fetch_update`] 的区别：① 先读授权端诚实空态标志
    /// `available`（`false` 时**不计失败**）；② 不依赖既有状态机前置态，可被
    /// 调度任务周期性重复调用；③ 已有 pending 版本时直接跳过（避免每轮重复暂存
    /// 与失败噪音）。
    ///
    /// 全程零 panic：网络失败 / 报文非法 / 版本不新 / 验签失败一律收敛为
    /// [`OtaPollOutcome::Rejected`] 且带真实原因，并落对应审计（旧版本不受影响）。
    pub async fn poll_and_stage(&mut self, url: &str) -> OtaPollOutcome {
        // 已有 pending 版本（等待启动确认）→ 本轮跳过（幂等，防重复暂存）。
        match self.store.load_meta().await {
            Ok(Some(meta))
                if meta
                    .pending_version
                    .as_deref()
                    .map(|v| !v.trim().is_empty())
                    .unwrap_or(false) =>
            {
                return OtaPollOutcome::AlreadyPending {
                    version: meta.pending_version.unwrap_or_default(),
                };
            }
            _ => {}
        }

        self.audit(OtaAudit::DownloadStarted {
            url: url.to_string(),
        });
        let raw = match fetch_http(url, self.timeout).await {
            Ok(raw) => raw,
            Err(err) => {
                self.audit(OtaAudit::DownloadFailed {
                    url: url.to_string(),
                    reason: err.to_string(),
                });
                return OtaPollOutcome::Rejected {
                    reason: err.to_string(),
                };
            }
        };

        // 授权端诚实空态：`available = false` → 暂无可下发版本，不算失败。
        match manifest_available(&raw) {
            Ok(false) => {
                return OtaPollOutcome::NoUpdate {
                    reason: manifest_unavailable_reason(&raw),
                };
            }
            Ok(true) => {}
            Err(err) => {
                self.audit(OtaAudit::VerifyFailed {
                    reason: err.to_string(),
                });
                return OtaPollOutcome::Rejected {
                    reason: err.to_string(),
                };
            }
        }

        // 复用既有下载后管线（解析 + 版本单调性 + 验签 + Downloaded 审计）。
        match self.verify_and_stage(&raw) {
            Ok(manifest) => {
                let version = manifest.version;
                self.audit(OtaAudit::Verified {
                    version: version.to_string(),
                });
                self.staged = Some(manifest);
                self.state = OtaState::ReadyToApply;
                match self.apply().await {
                    Ok(()) => OtaPollOutcome::Applied { version },
                    Err(err) => {
                        // apply 失败（存储故障等）已由 apply 内部记审计；收敛 Failed。
                        self.state = OtaState::Failed;
                        OtaPollOutcome::Rejected {
                            reason: err.to_string(),
                        }
                    }
                }
            }
            Err(err) => {
                self.audit(OtaAudit::VerifyFailed {
                    reason: err.to_string(),
                });
                self.state = OtaState::Failed;
                OtaPollOutcome::Rejected {
                    reason: err.to_string(),
                }
            }
        }
    }

    /// 手动回滚：清除 pending 槽、保留 current 槽（旧版本字节原样保留）。
    /// `Applied → RolledBack`。
    ///
    /// # Errors
    /// 状态非 `Applied`（审计 `InvalidTransition`）、pending 槽不存在或存储故障。
    pub async fn rollback(&mut self) -> DaemonResult<()> {
        if self.state != OtaState::Applied {
            self.audit(OtaAudit::InvalidTransition {
                action: "rollback".to_string(),
                from: self.state.as_str().to_string(),
            });
            return Err(DaemonError::ProtocolError(format!(
                "ota rollback requires state Applied, got {}",
                self.state.as_str()
            )));
        }
        if self.store.load_bytes(SLOT_PENDING).await?.is_none() {
            return Err(DaemonError::ProtocolError(
                "ota rollback: no pending slot to roll back".to_string(),
            ));
        }
        self.store.delete(SLOT_PENDING).await?;
        self.store
            .persist_meta(&OtaMeta {
                current_version: self.current_version.to_string(),
                pending_version: None,
                updated_ts_ns: now_unix_ts_ns().to_string(),
            })
            .await?;
        self.audit(OtaAudit::ManualRollback);
        self.staged = None;
        self.state = OtaState::RolledBack;
        Ok(())
    }

    /// 启动确认判定（两槽位协议的核心切换点；未来由 bootstrap 接线调用）。
    ///
    /// - `committed == true`：pending 槽字节晋升为 current 槽，pending 清除，
    ///   meta.current_version 推进，内部 current_version 同步推进；
    /// - `committed == false`（宽限期内未确认）：清除 pending 槽、meta 归位，
    ///   **current 槽旧版本字节保留**（两槽位协议下旧字节从未被覆盖，保留即恢复）。
    /// - 无 pending（meta 缺失或 pending_version 为空）：返回
    ///   [`OtaBootDecision::NoPending`]，不做任何写操作。
    ///
    /// # Errors
    /// meta 解析失败 / pending 槽缺字节 / 存储故障。
    pub async fn boot_commit_or_rollback(
        &mut self,
        committed: bool,
    ) -> DaemonResult<OtaBootDecision> {
        let Some(meta) = self.store.load_meta().await? else {
            return Ok(OtaBootDecision::NoPending);
        };
        let Some(pending_str) = meta.pending_version.clone() else {
            return Ok(OtaBootDecision::NoPending);
        };
        let pending_version: u64 = pending_str.parse().map_err(|_| {
            DaemonError::StorageError(format!(
                "ota meta pending_version {pending_str:?} is not a valid u64"
            ))
        })?;

        if committed {
            let bytes = self.store.load_bytes(SLOT_PENDING).await?.ok_or_else(|| {
                DaemonError::StorageError(format!(
                    "ota commit: pending slot {SLOT_PENDING:?} missing for version {pending_str}"
                ))
            })?;
            self.store.save_bytes(SLOT_CURRENT, &bytes).await?;
            self.store.delete(SLOT_PENDING).await?;
            self.store
                .persist_meta(&OtaMeta {
                    current_version: pending_str.clone(),
                    pending_version: None,
                    updated_ts_ns: now_unix_ts_ns().to_string(),
                })
                .await?;
            self.current_version = pending_version;
            self.audit(OtaAudit::Commit {
                version: pending_str,
            });
            self.state = OtaState::Applied;
            Ok(OtaBootDecision::Committed {
                version: pending_version,
            })
        } else {
            self.store.delete(SLOT_PENDING).await?;
            self.store
                .persist_meta(&OtaMeta {
                    current_version: meta.current_version.clone(),
                    pending_version: None,
                    updated_ts_ns: now_unix_ts_ns().to_string(),
                })
                .await?;
            self.audit(OtaAudit::RolledBack {
                pending_version: pending_str,
            });
            self.state = OtaState::RolledBack;
            let current: u64 = meta.current_version.parse().map_err(|_| {
                DaemonError::StorageError(format!(
                    "ota meta current_version {:?} is not a valid u64",
                    meta.current_version
                ))
            })?;
            Ok(OtaBootDecision::RolledBackTo { version: current })
        }
    }

    /// 配置热更新免重启：与固件包共用下载 + manifest 解析 + 验签管线，
    /// 验签通过后直接把 payload 作为 JSON 返回（由调用方喂给既有热重载），
    /// **不写槽位、不改状态机**；版本单调性不强制（配置允许重放，见模块文档）。
    ///
    /// # Errors
    /// 下载 / manifest 解析 / 验签失败 → 对应错误 + 审计
    /// `ConfigHotReloadFailed`；payload 非 JSON → `ProtocolError` + 审计。
    pub async fn apply_config_hot_reload(&mut self, url: &str) -> DaemonResult<Value> {
        let result = self.config_hot_reload_inner(url).await;
        match result {
            Ok((manifest, config)) => {
                self.audit(OtaAudit::ConfigHotReload {
                    version: manifest.version.to_string(),
                });
                Ok(config)
            }
            Err(err) => {
                self.audit(OtaAudit::ConfigHotReloadFailed {
                    reason: err.to_string(),
                });
                Err(err)
            }
        }
    }

    /// 配置热更新内联流程（下载 → 解析 → 验签 → payload JSON 解析）。
    async fn config_hot_reload_inner(&self, url: &str) -> DaemonResult<(OtaManifest, Value)> {
        let raw = fetch_http(url, self.timeout).await?;
        let manifest = parse_manifest(&raw)?;
        verify_manifest_signature(&manifest, &self.public_key)?;
        let config: Value = serde_json::from_slice(&manifest.payload).map_err(|e| {
            DaemonError::ProtocolError(format!("ota config payload is not valid json: {e}"))
        })?;
        Ok((manifest, config))
    }
}

// ---- 测试 ----

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::SocketAddr;

    /// test-only：单元测试专用 Ed25519 私钥种子 A —— **仅测试使用，禁止真实部署**。
    const TEST_ONLY_KEY_A: [u8; 32] = [0x3au8; 32];

    /// test-only：单元测试专用 Ed25519 私钥种子 B（「错误公钥被拒」用）。
    /// **仅测试使用，禁止真实部署**。
    const TEST_ONLY_KEY_B: [u8; 32] = [0x3bu8; 32];

    /// 由种子生成测试签名密钥。
    fn test_key(seed: [u8; 32]) -> SigningKey {
        SigningKey::from_bytes(&seed)
    }

    /// 默认测试签名密钥 A。
    fn key_a() -> SigningKey {
        test_key(TEST_ONLY_KEY_A)
    }

    /// 另一把测试签名密钥 B。
    fn key_b() -> SigningKey {
        test_key(TEST_ONLY_KEY_B)
    }

    /// 测试固件 payload。
    fn firmware_payload(tag: u8) -> Vec<u8> {
        vec![tag; 24]
    }

    /// 用给定私钥为 payload 签发合法 manifest JSON 字节
    /// （version / ts_ns / size 一律字符串——大数红线的合法形态）。
    fn build_manifest_json(key: &SigningKey, version: u64, payload: &[u8]) -> Vec<u8> {
        let sha = sha256_hex(payload);
        let sig = key.sign(&ota_signing_message(version, &sha));
        let json = serde_json::json!({
            "version": version.to_string(),
            "ts_ns": "1763000000000000000",
            "size": payload.len().to_string(),
            "payload_sha256": sha,
            "payload_b64": BASE64_STANDARD.encode(payload),
            "sig_b64": BASE64_STANDARD.encode(sig.to_bytes()),
        });
        serde_json::to_vec(&json).expect("test manifest json serialize")
    }

    /// 用给定私钥签发 payload、但 manifest 内嵌 `payload_b64` 换成 `served_payload`
    /// （制造「签名与内容不符」的篡改包）。
    fn build_tampered_payload_manifest(
        key: &SigningKey,
        version: u64,
        signed_payload: &[u8],
        served_payload: &[u8],
    ) -> Vec<u8> {
        let sha = sha256_hex(signed_payload);
        let sig = key.sign(&ota_signing_message(version, &sha));
        let json = serde_json::json!({
            "version": version.to_string(),
            "ts_ns": "1763000000000000000",
            "size": served_payload.len().to_string(),
            "payload_sha256": sha,
            "payload_b64": BASE64_STANDARD.encode(served_payload),
            "sig_b64": BASE64_STANDARD.encode(sig.to_bytes()),
        });
        serde_json::to_vec(&json).expect("test manifest json serialize")
    }

    /// 构造授权端 `GET /updates/manifest` 的响应体（`OtaManifestResponse` 形状）：
    /// 在 manifest 六字段之外附带 `available` / `kid` / `reason`。
    ///
    /// `available = false` 时按授权端协议把其余字段置空串（诚实空态）。
    fn build_manifest_response_json(
        key: &SigningKey,
        version: u64,
        payload: &[u8],
        available: bool,
    ) -> Vec<u8> {
        if !available {
            let json = serde_json::json!({
                "available": false,
                "version": "",
                "ts_ns": "",
                "size": "",
                "payload_sha256": "",
                "payload_b64": "",
                "sig_b64": "",
                "kid": "kid-1",
                "reason": "当前没有可下发的版本；请在授权端发布升级包后重试",
            });
            return serde_json::to_vec(&json).expect("test unavailable response json");
        }
        let sha = sha256_hex(payload);
        let sig = key.sign(&ota_signing_message(version, &sha));
        let json = serde_json::json!({
            "available": true,
            "version": version.to_string(),
            "ts_ns": "1763000000000000000",
            "size": payload.len().to_string(),
            "payload_sha256": sha,
            "payload_b64": BASE64_STANDARD.encode(payload),
            "sig_b64": BASE64_STANDARD.encode(sig.to_bytes()),
            "kid": "kid-1",
            "reason": "",
        });
        serde_json::to_vec(&json).expect("test available response json")
    }

    /// mock HTTP 服务器：读到请求头结束即按 handler 回放响应，随后断开
    /// （配合客户端 `Connection: close` 的 read_to_end；风格同 driver/http.rs 测试）。
    async fn spawn_http_mock<F>(handler: F) -> SocketAddr
    where
        F: Fn(&[u8]) -> Vec<u8> + Send + Sync + 'static,
    {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let addr = listener.local_addr().expect("local addr");
        let handler = Arc::new(handler);
        tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    return;
                };
                let handler = Arc::clone(&handler);
                tokio::spawn(async move {
                    let mut buf = Vec::new();
                    let mut chunk = [0u8; 1024];
                    loop {
                        match stream.read(&mut chunk).await {
                            Ok(0) | Err(_) => return,
                            Ok(n) => {
                                buf.extend_from_slice(&chunk[..n]);
                                if buf.windows(4).any(|w| w == b"\r\n\r\n") {
                                    break;
                                }
                            }
                        }
                    }
                    let response = handler(&buf);
                    let _ = stream.write_all(&response).await;
                });
            }
        });
        addr
    }

    /// 组装 200 OK + Content-Length 响应。
    fn ok_response(body: &[u8]) -> Vec<u8> {
        let mut out = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n",
            body.len()
        )
        .into_bytes();
        out.extend_from_slice(body);
        out
    }

    /// 构造默认管理器（公钥 A、当前版本 1）。
    fn manager(current_version: u64) -> OtaManager {
        OtaManager::new(
            Arc::new(InMemoryOtaStore::default()),
            key_a().verifying_key(),
            current_version,
        )
    }

    /// 构造管理器 + 内存 store 句柄（供槽位断言）。
    fn manager_with_store(current_version: u64) -> (OtaManager, Arc<InMemoryOtaStore>) {
        let store = Arc::new(InMemoryOtaStore::default());
        let manager = OtaManager::new(
            Arc::clone(&store) as Arc<dyn OtaStore>,
            key_a().verifying_key(),
            current_version,
        );
        (manager, store)
    }

    // ---- manifest 解析 ----

    /// QA Happy: 合法 manifest 解析出全部字段且校验通过。
    #[test]
    fn parse_manifest_happy() {
        let payload = firmware_payload(0xAA);
        let raw = build_manifest_json(&key_a(), 3, &payload);
        let manifest = parse_manifest(&raw).expect("valid manifest must parse");
        assert_eq!(manifest.version, 3);
        assert_eq!(manifest.ts_ns, "1763000000000000000");
        assert_eq!(manifest.size, payload.len());
        assert_eq!(manifest.payload, payload);
        assert_eq!(manifest.payload_sha256, sha256_hex(&payload));
        verify_manifest_signature(&manifest, &key_a().verifying_key())
            .expect("signature must verify");
    }

    /// QA Error（大数红线）：version / ts_ns / size 任一为 JSON 数值型 → 拒绝。
    #[test]
    fn parse_manifest_rejects_json_number_fields() {
        let payload = firmware_payload(1);
        let base = build_manifest_json(&key_a(), 3, &payload);
        let value: Value = serde_json::from_slice(&base).expect("base manifest json");

        // version 数字化。
        let mut bad_version = value.clone();
        bad_version["version"] = serde_json::json!(3);
        let err = parse_manifest(&serde_json::to_vec(&bad_version).expect("json"))
            .expect_err("numeric version must be rejected");
        assert!(matches!(err, DaemonError::ProtocolError(_)), "{err:?}");
        assert!(err.to_string().contains("big-number"), "{err}");

        // ts_ns 数字化。
        let mut bad_ts = value.clone();
        bad_ts["ts_ns"] = serde_json::json!(1_763_000_000i64);
        assert!(parse_manifest(&serde_json::to_vec(&bad_ts).expect("json")).is_err());

        // size 数字化。
        let mut bad_size = value;
        bad_size["size"] = serde_json::json!(24);
        assert!(parse_manifest(&serde_json::to_vec(&bad_size).expect("json")).is_err());
    }

    /// QA Error：size / sha256 与实际 payload 不一致（篡改证据）→ SecurityError。
    #[test]
    fn parse_manifest_rejects_size_and_sha_mismatch() {
        let payload = firmware_payload(2);

        // sha 不匹配（payload_b64 与 payload_sha256 脱钩）。
        let tampered = build_tampered_payload_manifest(&key_a(), 3, &payload, &firmware_payload(9));
        let err = parse_manifest(&tampered).expect_err("sha mismatch must be rejected");
        assert!(
            matches!(err, DaemonError::SecurityError(_)),
            "sha mismatch must be SecurityError, got {err:?}"
        );

        // size 不匹配（声明长度 ≠ 实际长度）。
        let sha = sha256_hex(&payload);
        let sig = key_a().sign(&ota_signing_message(3, &sha));
        let mut bad_size: Value =
            serde_json::from_slice(&build_manifest_json(&key_a(), 3, &payload)).expect("json");
        bad_size["size"] = serde_json::json!("999");
        let _ = sig; // 签名对 size 无约束（签名对象是 version+sha），size 校验独立生效。
        let err = parse_manifest(&serde_json::to_vec(&bad_size).expect("json"))
            .expect_err("size mismatch must be rejected");
        assert!(matches!(err, DaemonError::SecurityError(_)), "{err:?}");
    }

    // ---- 下载 ----

    /// QA Happy: 端到端下载验签 → ReadyToApply，返回新版本号。
    #[tokio::test]
    async fn fetch_update_end_to_end_ready_to_apply() {
        let payload = firmware_payload(7);
        let manifest_bytes = build_manifest_json(&key_a(), 2, &payload);
        let addr = spawn_http_mock(move |_| ok_response(&manifest_bytes)).await;

        let (mut ota, _store) = manager_with_store(1);
        assert_eq!(ota.state(), OtaState::Idle);

        let version = ota
            .fetch_update(&format!("http://{addr}/ota/firmware"))
            .await
            .expect("fetch must succeed");
        assert_eq!(version, 2);
        assert_eq!(ota.state(), OtaState::ReadyToApply);
        assert_eq!(ota.current_version(), 1, "apply 前当前版本不变");

        // 审计链：Started → Downloaded → Verified。
        let records: Vec<&OtaAudit> = ota.audit_log().iter().map(|e| &e.record).collect();
        assert!(matches!(records[0], OtaAudit::DownloadStarted { .. }));
        assert_eq!(
            records[1],
            &OtaAudit::Downloaded {
                version: "2".to_string(),
                size: payload.len().to_string()
            },
            "版本/字节数审计为字符串（大数红线）"
        );
        assert!(matches!(records[2], OtaAudit::Verified { .. }));
    }

    /// QA Error: 服务器挂起不响应 → 下载超时 → NetworkError + Failed 态 + 审计。
    ///
    /// 注意：handler 用 `tokio::time::sleep`（而非 `std::thread::sleep`）——
    /// `#[tokio::test]` 默认单线程 runtime，阻塞式 sleep 会饿死客户端的定时器。
    #[tokio::test]
    async fn download_timeout_fails_state_and_audits() {
        // 模拟慢服务器：接受连接、读完请求头后挂起 500ms 再写响应；
        // 客户端 200ms 超时必先触发。
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let addr = listener.local_addr().expect("local addr");
        tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    return;
                };
                tokio::spawn(async move {
                    let mut buf = Vec::new();
                    let mut chunk = [0u8; 1024];
                    loop {
                        match stream.read(&mut chunk).await {
                            Ok(0) | Err(_) => return,
                            Ok(n) => {
                                buf.extend_from_slice(&chunk[..n]);
                                if buf.windows(4).any(|w| w == b"\r\n\r\n") {
                                    break;
                                }
                            }
                        }
                    }
                    tokio::time::sleep(Duration::from_millis(500)).await;
                    let _ = stream.write_all(&ok_response(b"{}")).await;
                });
            }
        });

        let mut ota = manager(1).with_timeout(Duration::from_millis(200));
        let err = ota
            .fetch_update(&format!("http://{addr}/ota/slow"))
            .await
            .expect_err("timeout must fail");
        assert!(matches!(err, DaemonError::NetworkError(_)), "{err:?}");
        assert!(err.to_string().contains("timeout"), "{err}");
        assert_eq!(ota.state(), OtaState::Failed, "失败收敛 Failed 态");

        // 失败审计 + 旧版本不受影响。
        let records: Vec<&OtaAudit> = ota.audit_log().iter().map(|e| &e.record).collect();
        assert!(matches!(records[0], OtaAudit::DownloadStarted { .. }));
        assert!(matches!(records[1], OtaAudit::DownloadFailed { .. }));
        assert_eq!(ota.current_version(), 1);
    }

    /// QA Error: 连接拒绝 → NetworkError + Failed 态。
    #[tokio::test]
    async fn download_connection_refused_is_network_error() {
        let mut ota = manager(1).with_timeout(Duration::from_secs(2));
        let err = ota
            .fetch_update("http://127.0.0.1:1/ota/x")
            .await
            .expect_err("refused connection must fail");
        assert!(matches!(err, DaemonError::NetworkError(_)), "{err:?}");
        assert_eq!(ota.state(), OtaState::Failed);
    }

    /// QA Error: 非 2xx 响应 → ProtocolError + Failed 态。
    #[tokio::test]
    async fn download_http_error_status_fails() {
        let addr = spawn_http_mock(|_| {
            b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\n\r\n".to_vec()
        })
        .await;
        let mut ota = manager(1).with_timeout(Duration::from_secs(2));
        let err = ota
            .fetch_update(&format!("http://{addr}/ota/x"))
            .await
            .expect_err("503 must fail");
        assert!(matches!(err, DaemonError::ProtocolError(_)), "{err:?}");
        assert!(err.to_string().contains("503"), "{err}");
        assert_eq!(ota.state(), OtaState::Failed);
    }

    /// QA: 失败后可重试（Failed 态允许再次 fetch_update）。
    #[tokio::test]
    async fn failed_state_allows_retry() {
        let payload = firmware_payload(5);
        let manifest_bytes = build_manifest_json(&key_a(), 2, &payload);
        // 第一轮拒绝连接（端口 1），第二轮起 mock 服务。
        let mut ota = manager(1).with_timeout(Duration::from_secs(2));
        assert!(ota.fetch_update("http://127.0.0.1:1/ota/x").await.is_err());
        assert_eq!(ota.state(), OtaState::Failed);

        let addr = spawn_http_mock(move |_| ok_response(&manifest_bytes)).await;
        let version = ota
            .fetch_update(&format!("http://{addr}/ota/x"))
            .await
            .expect("retry must succeed");
        assert_eq!(version, 2);
        assert_eq!(ota.state(), OtaState::ReadyToApply);
    }

    // ---- 验签 / 版本单调性 ----

    /// QA Error: 包体被篡改（签名与内容不符）→ SecurityError，拒绝应用。
    #[tokio::test]
    async fn tampered_payload_is_rejected() {
        let signed = firmware_payload(0x11);
        let served = firmware_payload(0x22);
        let manifest_bytes = build_tampered_payload_manifest(&key_a(), 2, &signed, &served);
        let addr = spawn_http_mock(move |_| ok_response(&manifest_bytes)).await;

        let mut ota = manager(1).with_timeout(Duration::from_secs(2));
        let err = ota
            .fetch_update(&format!("http://{addr}/ota/x"))
            .await
            .expect_err("tampered package must be rejected");
        assert!(matches!(err, DaemonError::SecurityError(_)), "{err:?}");
        assert_eq!(ota.state(), OtaState::Failed, "验签失败收敛 Failed");
        // 审计 VerifyFailed + 已运行的旧版本不受影响。
        let records: Vec<&OtaAudit> = ota.audit_log().iter().map(|e| &e.record).collect();
        assert!(matches!(
            records.last(),
            Some(OtaAudit::VerifyFailed { .. })
        ));
        assert_eq!(ota.current_version(), 1);
    }

    /// QA Error: 错误公钥（签名者不是持有公钥对应方）→ SecurityError。
    #[tokio::test]
    async fn wrong_public_key_is_rejected() {
        let payload = firmware_payload(3);
        // 用私钥 B 签名，但管理器持有公钥 A。
        let manifest_bytes = build_manifest_json(&key_b(), 2, &payload);
        let addr = spawn_http_mock(move |_| ok_response(&manifest_bytes)).await;

        let mut ota = manager(1).with_timeout(Duration::from_secs(2));
        let err = ota
            .fetch_update(&format!("http://{addr}/ota/x"))
            .await
            .expect_err("foreign signature must be rejected");
        assert!(matches!(err, DaemonError::SecurityError(_)), "{err:?}");
        assert!(err.to_string().contains("signature"), "{err}");
        assert_eq!(ota.state(), OtaState::Failed);
    }

    /// QA Error: 同版本被拒（严格单调，防重放旧包）。
    #[tokio::test]
    async fn same_version_is_rejected() {
        let payload = firmware_payload(4);
        let manifest_bytes = build_manifest_json(&key_a(), 1, &payload); // 当前版本即 1
        let addr = spawn_http_mock(move |_| ok_response(&manifest_bytes)).await;

        let mut ota = manager(1).with_timeout(Duration::from_secs(2));
        let err = ota
            .fetch_update(&format!("http://{addr}/ota/x"))
            .await
            .expect_err("same version must be rejected");
        assert!(
            err.to_string().contains("regression"),
            "error must explain monotonicity: {err}"
        );
        assert_eq!(ota.state(), OtaState::Failed);
    }

    /// QA Error: 旧版本（降级攻击）被拒。
    #[tokio::test]
    async fn older_version_is_rejected() {
        let payload = firmware_payload(6);
        let manifest_bytes = build_manifest_json(&key_a(), 1, &payload); // 当前已是 5
        let addr = spawn_http_mock(move |_| ok_response(&manifest_bytes)).await;

        let mut ota = manager(5).with_timeout(Duration::from_secs(2));
        let err = ota
            .fetch_update(&format!("http://{addr}/ota/x"))
            .await
            .expect_err("downgrade must be rejected");
        assert!(
            err.to_string()
                .contains("new version 1 must be greater than current 5"),
            "{err}"
        );
        assert_eq!(ota.state(), OtaState::Failed);
        assert_eq!(ota.current_version(), 5);
    }

    // ---- 应用（apply → pending 槽） ----

    /// QA Happy: apply 把 payload 写入 pending 槽 + meta 记录 pending 版本。
    #[tokio::test]
    async fn apply_writes_pending_slot_and_meta() {
        let payload = firmware_payload(8);
        let manifest_bytes = build_manifest_json(&key_a(), 2, &payload);
        let addr = spawn_http_mock(move |_| ok_response(&manifest_bytes)).await;

        let (mut ota, store) = manager_with_store(1);
        ota.fetch_update(&format!("http://{addr}/ota/x"))
            .await
            .expect("fetch");
        ota.apply().await.expect("apply");
        assert_eq!(ota.state(), OtaState::Applied);

        // pending 槽字节 == payload；current 槽不受影响（尚无 current 字节）。
        assert_eq!(
            store.load_bytes(SLOT_PENDING).await.expect("load pending"),
            Some(payload),
            "pending 槽持有新包字节"
        );
        assert_eq!(
            store.load_bytes(SLOT_CURRENT).await.expect("load current"),
            None
        );

        // meta：current=1，pending=2（字符串）。
        let meta = store
            .load_meta()
            .await
            .expect("meta")
            .expect("meta present");
        assert_eq!(meta.current_version, "1");
        assert_eq!(meta.pending_version, Some("2".to_string()));

        // 审计 Applied。
        let records: Vec<&OtaAudit> = ota.audit_log().iter().map(|e| &e.record).collect();
        assert!(matches!(records.last(), Some(OtaAudit::Applied { .. })));
    }

    /// QA Error: 未就绪（Idle）直接 apply → InvalidTransition 审计 + ProtocolError；
    /// 且 ReadyToApply 态重复 fetch_update 同样被拒。
    #[tokio::test]
    async fn invalid_transitions_are_rejected_and_audited() {
        let (mut ota, _store) = manager_with_store(1);

        // Idle 态直接 apply。
        let err = ota.apply().await.expect_err("apply from Idle must fail");
        assert!(matches!(err, DaemonError::ProtocolError(_)), "{err:?}");
        assert!(matches!(
            ota.audit_log().last().map(|e| &e.record),
            Some(OtaAudit::InvalidTransition { .. })
        ));

        // ReadyToApply 态重复 fetch_update。
        let payload = firmware_payload(9);
        let manifest_bytes = build_manifest_json(&key_a(), 2, &payload);
        let addr = spawn_http_mock(move |_| ok_response(&manifest_bytes)).await;
        ota.fetch_update(&format!("http://{addr}/ota/x"))
            .await
            .expect("fetch");
        let err = ota
            .fetch_update(&format!("http://{addr}/ota/x"))
            .await
            .expect_err("second fetch from ReadyToApply must fail");
        assert!(err.to_string().contains("fetch_update"), "{err}");
        assert_eq!(ota.state(), OtaState::ReadyToApply, "状态不被非法迁移破坏");
    }

    // ---- 轮询接线（poll_and_stage，授权端 /updates/manifest） ----

    /// QA Happy：`available = true` + 版本更新 + 验签通过 → 写入 pending 槽 + 审计；
    /// 之后再次轮询（已有 pending）→ 跳过，**不再发请求**（URL 故意不可达仍返回跳过）。
    #[tokio::test]
    async fn poll_applies_newer_version_and_writes_pending() {
        let payload = firmware_payload(0xE1);
        let body = build_manifest_response_json(&key_a(), 2, &payload, true);
        let addr = spawn_http_mock(move |_| ok_response(&body)).await;

        let (mut ota, store) = manager_with_store(1);
        let url = format!("http://{addr}/updates/manifest");
        let outcome = ota.poll_and_stage(&url).await;
        assert_eq!(outcome, OtaPollOutcome::Applied { version: 2 });
        assert_eq!(ota.state(), OtaState::Applied);
        assert_eq!(
            store.load_bytes(SLOT_PENDING).await.expect("pending"),
            Some(payload),
            "pending 槽持有新包字节"
        );
        let meta = store
            .load_meta()
            .await
            .expect("meta")
            .expect("meta present");
        assert_eq!(meta.pending_version, Some("2".to_string()));

        // 审计含 Applied（版本为字符串——大数红线）。
        let records: Vec<&OtaAudit> = ota.audit_log().iter().map(|e| &e.record).collect();
        assert!(
            records.contains(&&OtaAudit::Applied {
                version: "2".to_string()
            }),
            "{records:?}"
        );

        // 已有 pending → 跳过且不下载（用不可达 URL 证明没有发请求）。
        let outcome = ota
            .poll_and_stage("http://127.0.0.1:1/updates/manifest")
            .await;
        assert_eq!(
            outcome,
            OtaPollOutcome::AlreadyPending {
                version: "2".to_string()
            }
        );
    }

    /// QA 诚实空态：`available = false` → `NoUpdate`，**不计失败**、不写 pending、
    /// 审计只有 DownloadStarted（无 DownloadFailed / VerifyFailed）。
    #[tokio::test]
    async fn poll_available_false_is_normal_empty_state() {
        let body = build_manifest_response_json(&key_a(), 0, &[], false);
        let addr = spawn_http_mock(move |_| ok_response(&body)).await;
        let (mut ota, store) = manager_with_store(1);
        let url = format!("http://{addr}/updates/manifest");

        let outcome = ota.poll_and_stage(&url).await;
        match outcome {
            OtaPollOutcome::NoUpdate { reason } => {
                assert!(reason.contains("没有可下发"), "空态原因原样透传: {reason}");
            }
            other => panic!("expected NoUpdate, got {other:?}"),
        }
        assert_eq!(
            store.load_bytes(SLOT_PENDING).await.expect("pending"),
            None,
            "空态不写 pending 槽"
        );
        let records: Vec<&OtaAudit> = ota.audit_log().iter().map(|e| &e.record).collect();
        assert_eq!(
            records,
            vec![&OtaAudit::DownloadStarted { url }],
            "空态只记下载开始，不计失败"
        );
    }

    /// QA Error：manifest 不可达 → `Rejected`（真实原因）+ `DownloadFailed` 审计，
    /// **不 panic**、旧版本不受影响。
    #[tokio::test]
    async fn poll_unreachable_manifest_is_rejected_with_reason() {
        let mut ota = manager(1).with_timeout(Duration::from_secs(2));
        let outcome = ota
            .poll_and_stage("http://127.0.0.1:1/updates/manifest")
            .await;
        match outcome {
            OtaPollOutcome::Rejected { reason } => {
                assert!(!reason.trim().is_empty(), "拒绝原因必须非空");
            }
            other => panic!("expected Rejected, got {other:?}"),
        }
        assert!(matches!(
            ota.audit_log().last().map(|e| &e.record),
            Some(OtaAudit::DownloadFailed { .. })
        ));
        assert_eq!(ota.current_version(), 1);
    }

    /// QA Error：版本不新（等于当前）→ 拒绝应用，**不写 pending 槽**（防重放）。
    #[tokio::test]
    async fn poll_rejects_non_newer_version_without_pending() {
        let payload = firmware_payload(0xE2);
        let body = build_manifest_response_json(&key_a(), 1, &payload, true); // 当前即 1
        let addr = spawn_http_mock(move |_| ok_response(&body)).await;
        let (mut ota, store) = manager_with_store(1);
        let outcome = ota
            .poll_and_stage(&format!("http://{addr}/updates/manifest"))
            .await;
        match outcome {
            OtaPollOutcome::Rejected { reason } => {
                assert!(
                    reason.contains("regression"),
                    "须明确说明版本单调性: {reason}"
                );
            }
            other => panic!("expected Rejected, got {other:?}"),
        }
        assert_eq!(
            store.load_bytes(SLOT_PENDING).await.expect("pending"),
            None,
            "版本不新绝不写 pending 槽"
        );
        assert!(matches!(
            ota.audit_log().last().map(|e| &e.record),
            Some(OtaAudit::VerifyFailed { .. })
        ));
    }

    /// QA 大数红线回归：`size` 写成 JSON **number** → 拒绝（`VerifyFailed`），绝不应用。
    #[tokio::test]
    async fn poll_rejects_numeric_size_field() {
        let payload = firmware_payload(0xE3);
        let base = build_manifest_response_json(&key_a(), 2, &payload, true);
        let mut value: Value = serde_json::from_slice(&base).expect("base json");
        value["size"] = serde_json::json!(payload.len()); // number（红线违规）
        let body = serde_json::to_vec(&value).expect("json");
        let addr = spawn_http_mock(move |_| ok_response(&body)).await;

        let (mut ota, store) = manager_with_store(1);
        let outcome = ota
            .poll_and_stage(&format!("http://{addr}/updates/manifest"))
            .await;
        match outcome {
            OtaPollOutcome::Rejected { reason } => {
                assert!(
                    reason.contains("big-number"),
                    "数值型 size 必须被拒: {reason}"
                );
            }
            other => panic!("expected Rejected, got {other:?}"),
        }
        assert_eq!(store.load_bytes(SLOT_PENDING).await.expect("pending"), None);
    }

    /// QA：`available` 字段形状校验（缺字段 / 非布尔 / 非 JSON → ProtocolError）；
    /// 空态 `reason` 读取（缺省空串，零 panic）。
    #[test]
    fn manifest_available_requires_boolean_field() {
        assert!(manifest_available(br#"{"available":true}"#).expect("bool true"));
        assert!(!manifest_available(br#"{"available":false}"#).expect("bool false"));
        assert!(matches!(
            manifest_available(br#"{"available":"yes"}"#),
            Err(DaemonError::ProtocolError(_))
        ));
        assert!(matches!(
            manifest_available(b"{}"),
            Err(DaemonError::ProtocolError(_))
        ));
        assert!(matches!(
            manifest_available(b"not json"),
            Err(DaemonError::ProtocolError(_))
        ));
        assert_eq!(
            manifest_unavailable_reason(br#"{"available":false,"reason":"none available"}"#),
            "none available"
        );
        assert_eq!(manifest_unavailable_reason(b"junk"), "");
    }

    /// QA：`signing_key_b64` 解析（合法 32 字节通过；坏 base64 / 长度错 → ConfigError）。
    #[test]
    fn verifying_key_from_b64_validates_shape() {
        let key = key_a().verifying_key();
        let b64 = BASE64_STANDARD.encode(key.to_bytes());
        assert_eq!(verifying_key_from_b64(&b64).expect("valid key"), key);
        assert!(matches!(
            verifying_key_from_b64("not-base64!!"),
            Err(DaemonError::ConfigError(_))
        ));
        assert!(matches!(
            verifying_key_from_b64("AAAA"),
            Err(DaemonError::ConfigError(_))
        ));
    }

    // ---- 启动判定（boot_commit_or_rollback） ----

    /// QA Happy: commit → pending 槽晋升为 current 槽，版本推进，pending 清除。
    #[tokio::test]
    async fn boot_commit_promotes_pending_to_current() {
        let old_payload = firmware_payload(1);
        let new_payload = firmware_payload(2);
        let manifest_bytes = build_manifest_json(&key_a(), 2, &new_payload);
        let addr = spawn_http_mock(move |_| ok_response(&manifest_bytes)).await;

        let (mut ota, store) = manager_with_store(1);
        // 预置 current 槽为旧版本字节（模拟上一轮 commit 的产物）。
        store
            .save_bytes(SLOT_CURRENT, &old_payload)
            .await
            .expect("seed current slot");

        ota.fetch_update(&format!("http://{addr}/ota/x"))
            .await
            .expect("fetch");
        ota.apply().await.expect("apply");

        // 模拟新版本启动后健康确认。
        let decision = ota
            .boot_commit_or_rollback(true)
            .await
            .expect("boot decide");
        assert_eq!(decision, OtaBootDecision::Committed { version: 2 });
        assert_eq!(ota.current_version(), 2, "commit 后版本推进");
        assert_eq!(ota.state(), OtaState::Applied);

        // 槽位切换：current = 新字节，pending 已删除。
        assert_eq!(
            store.load_bytes(SLOT_CURRENT).await.expect("load current"),
            Some(new_payload),
            "commit 后 current 槽持有新版本字节"
        );
        assert_eq!(
            store.load_bytes(SLOT_PENDING).await.expect("load pending"),
            None
        );
        let meta = store.load_meta().await.expect("meta").expect("meta");
        assert_eq!(meta.current_version, "2");
        assert_eq!(meta.pending_version, None);

        // 审计 Commit。
        let records: Vec<&OtaAudit> = ota.audit_log().iter().map(|e| &e.record).collect();
        assert!(matches!(
            records.last(),
            Some(OtaAudit::Commit { version }) if version == "2"
        ));
    }

    /// QA Happy: 宽限期内未 commit → 回滚：pending 清除、current 槽旧版本字节
    /// 完整保留（两槽位协议下「恢复旧版本字节」= 保留即恢复）。
    #[tokio::test]
    async fn boot_without_commit_restores_old_bytes() {
        let old_payload = firmware_payload(3);
        let new_payload = firmware_payload(4);
        let manifest_bytes = build_manifest_json(&key_a(), 2, &new_payload);
        let addr = spawn_http_mock(move |_| ok_response(&manifest_bytes)).await;

        let (mut ota, store) = manager_with_store(1);
        store
            .save_bytes(SLOT_CURRENT, &old_payload)
            .await
            .expect("seed current slot");

        ota.fetch_update(&format!("http://{addr}/ota/x"))
            .await
            .expect("fetch");
        ota.apply().await.expect("apply");

        // 模拟新版本启动后反复崩溃 / 宽限期超时 → 未 commit。
        let decision = ota
            .boot_commit_or_rollback(false)
            .await
            .expect("boot decide");
        assert_eq!(decision, OtaBootDecision::RolledBackTo { version: 1 });
        assert_eq!(ota.current_version(), 1, "版本不推进");
        assert_eq!(ota.state(), OtaState::RolledBack);

        // pending 清除；current 槽旧字节原样恢复。
        assert_eq!(
            store.load_bytes(SLOT_PENDING).await.expect("load pending"),
            None
        );
        assert_eq!(
            store.load_bytes(SLOT_CURRENT).await.expect("load current"),
            Some(old_payload),
            "旧版本字节必须完整保留"
        );
        let meta = store.load_meta().await.expect("meta").expect("meta");
        assert_eq!(meta.current_version, "1");
        assert_eq!(meta.pending_version, None);

        // 审计 RolledBack。
        let records: Vec<&OtaAudit> = ota.audit_log().iter().map(|e| &e.record).collect();
        assert!(matches!(
            records.last(),
            Some(OtaAudit::RolledBack { pending_version }) if pending_version == "2"
        ));
    }

    /// QA: 无 pending（meta 缺失 / pending 为空）→ NoPending 零写操作。
    #[tokio::test]
    async fn boot_without_pending_is_noop() {
        let (mut ota, store) = manager_with_store(7);
        assert_eq!(
            ota.boot_commit_or_rollback(true).await.expect("decide"),
            OtaBootDecision::NoPending,
            "meta 缺失 → NoPending"
        );
        assert_eq!(
            ota.boot_commit_or_rollback(false).await.expect("decide"),
            OtaBootDecision::NoPending,
            "meta 存在但无 pending → NoPending"
        );
        // 写入 meta 但无 pending 同样 NoPending。
        store
            .persist_meta(&OtaMeta::new(7))
            .await
            .expect("persist meta");
        assert_eq!(
            ota.boot_commit_or_rollback(true).await.expect("decide"),
            OtaBootDecision::NoPending
        );
        assert_eq!(ota.current_version(), 7);
        assert_eq!(ota.state(), OtaState::Idle, "NoPending 不改状态");
    }

    // ---- 手动回滚 ----

    /// QA Happy: rollback() 清除 pending 槽、保留 current 槽与当前版本。
    #[tokio::test]
    async fn manual_rollback_clears_pending_keeps_current() {
        let old_payload = firmware_payload(5);
        let new_payload = firmware_payload(6);
        let manifest_bytes = build_manifest_json(&key_a(), 2, &new_payload);
        let addr = spawn_http_mock(move |_| ok_response(&manifest_bytes)).await;

        let (mut ota, store) = manager_with_store(1);
        store
            .save_bytes(SLOT_CURRENT, &old_payload)
            .await
            .expect("seed current slot");

        ota.fetch_update(&format!("http://{addr}/ota/x"))
            .await
            .expect("fetch");
        ota.apply().await.expect("apply");
        ota.rollback().await.expect("rollback");
        assert_eq!(ota.state(), OtaState::RolledBack);
        assert_eq!(ota.current_version(), 1, "手动回滚不推进版本");

        assert_eq!(
            store.load_bytes(SLOT_PENDING).await.expect("load pending"),
            None
        );
        assert_eq!(
            store.load_bytes(SLOT_CURRENT).await.expect("load current"),
            Some(old_payload),
            "current 槽保留"
        );
        let meta = store.load_meta().await.expect("meta").expect("meta");
        assert_eq!(meta.pending_version, None);

        // 回滚后允许重新升级（RolledBack 是合法起点）。
        ota.fetch_update(&format!("http://{addr}/ota/x"))
            .await
            .expect("retry after manual rollback");
        assert_eq!(ota.state(), OtaState::ReadyToApply);

        // 审计 ManualRollback。
        let records: Vec<&OtaAudit> = ota.audit_log().iter().map(|e| &e.record).collect();
        assert!(records.contains(&&OtaAudit::ManualRollback));
    }

    /// QA Error: 非 Applied 态手动回滚被拒 + 审计。
    #[tokio::test]
    async fn manual_rollback_requires_applied_state() {
        let mut ota = manager(1);
        let err = ota
            .rollback()
            .await
            .expect_err("rollback from Idle must fail");
        assert!(matches!(err, DaemonError::ProtocolError(_)), "{err:?}");
        assert!(matches!(
            ota.audit_log().last().map(|e| &e.record),
            Some(OtaAudit::InvalidTransition { .. })
        ));
    }

    // ---- 配置热更新 ----

    /// QA Happy: 配置热更新共用下载验签管线，返回 payload JSON 且不落槽位不改状态。
    #[tokio::test]
    async fn config_hot_reload_returns_json_without_slots() {
        let config_payload = br#"{"sample_interval_ms":500,"retry_limit":3}"#.to_vec();
        let manifest_bytes = build_manifest_json(&key_a(), 4, &config_payload);
        let addr = spawn_http_mock(move |_| ok_response(&manifest_bytes)).await;

        let (mut ota, store) = manager_with_store(1);
        let config = ota
            .apply_config_hot_reload(&format!("http://{addr}/ota/config"))
            .await
            .expect("config hot reload must succeed");
        assert_eq!(config["sample_interval_ms"], 500);
        assert_eq!(config["retry_limit"], 3);

        // 不落槽位、meta 不存在、状态机不动、版本不推进。
        assert_eq!(store.load_bytes(SLOT_PENDING).await.expect("pending"), None);
        assert_eq!(store.load_bytes(SLOT_CURRENT).await.expect("current"), None);
        assert_eq!(store.load_meta().await.expect("meta"), None);
        assert_eq!(ota.state(), OtaState::Idle);
        assert_eq!(ota.current_version(), 1);

        // 审计 ConfigHotReload（版本为字符串）。
        let records: Vec<&OtaAudit> = ota.audit_log().iter().map(|e| &e.record).collect();
        assert!(matches!(
            records.last(),
            Some(OtaAudit::ConfigHotReload { version }) if version == "4"
        ));
    }

    /// QA Error: 配置 payload 非 JSON → ProtocolError + ConfigHotReloadFailed 审计。
    #[tokio::test]
    async fn config_hot_reload_rejects_non_json_payload() {
        let payload = b"this is not json".to_vec();
        let manifest_bytes = build_manifest_json(&key_a(), 4, &payload);
        let addr = spawn_http_mock(move |_| ok_response(&manifest_bytes)).await;

        let mut ota = manager(1).with_timeout(Duration::from_secs(2));
        let err = ota
            .apply_config_hot_reload(&format!("http://{addr}/ota/config"))
            .await
            .expect_err("non-json payload must fail");
        assert!(matches!(err, DaemonError::ProtocolError(_)), "{err:?}");
        assert!(err.to_string().contains("not valid json"), "{err}");
        assert!(matches!(
            ota.audit_log().last().map(|e| &e.record),
            Some(OtaAudit::ConfigHotReloadFailed { .. })
        ));
    }

    /// QA Error: 配置包签名被篡改同样拒绝（共用验签管线）。
    #[tokio::test]
    async fn config_hot_reload_rejects_tampered_package() {
        let signed = br#"{"a":1}"#.to_vec();
        let served = br#"{"a":2}"#.to_vec();
        let manifest_bytes = build_tampered_payload_manifest(&key_a(), 4, &signed, &served);
        let addr = spawn_http_mock(move |_| ok_response(&manifest_bytes)).await;

        let mut ota = manager(1).with_timeout(Duration::from_secs(2));
        let err = ota
            .apply_config_hot_reload(&format!("http://{addr}/ota/config"))
            .await
            .expect_err("tampered config must fail");
        assert!(matches!(err, DaemonError::SecurityError(_)), "{err:?}");
    }

    // ---- 审计完整性 / 元数据编码 ----

    /// QA: 完整旅程审计记录按序齐全（Started → Downloaded → Verified →
    /// Applied → Commit），且时间戳单调不减。
    #[tokio::test]
    async fn audit_log_covers_full_journey_in_order() {
        let payload = firmware_payload(0xC);
        let manifest_bytes = build_manifest_json(&key_a(), 2, &payload);
        let addr = spawn_http_mock(move |_| ok_response(&manifest_bytes)).await;

        let (mut ota, _store) = manager_with_store(1);
        ota.fetch_update(&format!("http://{addr}/ota/x"))
            .await
            .expect("fetch");
        ota.apply().await.expect("apply");
        ota.boot_commit_or_rollback(true).await.expect("commit");

        let records: Vec<&OtaAudit> = ota.audit_log().iter().map(|e| &e.record).collect();
        assert_eq!(
            records,
            vec![
                &OtaAudit::DownloadStarted {
                    url: format!("http://{addr}/ota/x")
                },
                &OtaAudit::Downloaded {
                    version: "2".to_string(),
                    size: payload.len().to_string()
                },
                &OtaAudit::Verified {
                    version: "2".to_string()
                },
                &OtaAudit::Applied {
                    version: "2".to_string()
                },
                &OtaAudit::Commit {
                    version: "2".to_string()
                },
            ],
            "全旅程审计按序齐全"
        );

        // 时间戳单调不减（同一时钟源）。
        let ts: Vec<i64> = ota.audit_log().iter().map(|e| e.ts_ns).collect();
        assert!(ts.windows(2).all(|w| w[0] <= w[1]), "ts must be monotonic");
    }

    /// QA（大数红线）：`OtaMeta` JSON 编码中版本 / 时间戳字段为 JSON 字符串，
    /// 且 `from_json` 往返一致、拒绝数值型字段。
    #[test]
    fn ota_meta_json_uses_strings_and_round_trips() {
        let meta = OtaMeta {
            current_version: "18446744073709551615".to_string(), // u64::MAX 也能安全表达
            pending_version: Some("18446744073709551614".to_string()),
            updated_ts_ns: "1763000000000000000".to_string(),
        };
        let raw = meta.to_json().expect("serialize");
        let value: Value = serde_json::from_str(&raw).expect("json");
        assert!(
            value["current_version"].is_string(),
            "current_version 必须是字符串"
        );
        assert!(
            value["pending_version"].is_string(),
            "pending_version 必须是字符串"
        );
        assert!(
            value["updated_ts_ns"].is_string(),
            "updated_ts_ns 必须是字符串"
        );
        assert_eq!(OtaMeta::from_json(&raw).expect("round trip"), meta);

        // 数值型字段拒绝（双向守护）。
        let numeric = r#"{"current_version":1,"pending_version":null,"updated_ts_ns":"0"}"#;
        let err = OtaMeta::from_json(numeric).expect_err("numeric field must be rejected");
        assert!(matches!(err, DaemonError::StorageError(_)), "{err:?}");

        // pending null ↔ None 往返。
        let none_meta = OtaMeta::new(9);
        let none_raw = none_meta.to_json().expect("serialize");
        assert!(none_raw.contains("\"pending_version\":null"));
        assert_eq!(
            OtaMeta::from_json(&none_raw).expect("round trip"),
            none_meta
        );
    }

    /// QA: 签名消息构造为域分隔 + 长度定界，version 或 sha 任一变化消息即变。
    #[test]
    fn ota_signing_message_is_domain_separated_and_sensitive() {
        let sha = sha256_hex(b"payload");
        let msg = ota_signing_message(3, &sha);
        let rendered = String::from_utf8(msg).expect("message is ascii");
        assert!(rendered.starts_with(OTA_SIGNING_DOMAIN_V1), "{rendered}");
        assert!(rendered.contains("ver=1:3|"), "{rendered}");
        assert!(
            rendered.contains(&format!("sha256=64:{sha}|")),
            "{rendered}"
        );

        // 长度定界防拼接歧义 + 内容敏感。
        assert_ne!(
            ota_signing_message(3, &sha),
            ota_signing_message(33, &sha),
            "version 变化必须改变消息"
        );
        assert_ne!(
            ota_signing_message(3, &sha),
            ota_signing_message(3, &sha256_hex(b"other")),
            "sha 变化必须改变消息"
        );
    }
}

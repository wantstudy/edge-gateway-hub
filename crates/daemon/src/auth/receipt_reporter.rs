//! task 48 — 审计回执上报客户端（B 档控制通道，daemon 侧）。
//!
//! # 背景（B 档契约）
//!
//! 客户业务数据直连客户自己的 Broker、不经厂商云 → 消息级云端校验没有落点。
//! 补救 = 网关对发出的数据批次生成回执（**只有序号区间 + 语义摘要，绝不含业务数值**），
//! 经控制通道 `POST /audit/receipt` 上报厂商云端；服务端已交付
//! `crates/licensing-server/src/receipt.rs`（验签）与 `src/audit.rs`（批次账本）。
//!
//! # 与服务端的契约对齐点清单（**字节级一致，改动任一即验签失败**）
//!
//! 1. **签名域常量**：`iotdaq.receipt.v1`，**不带**尾随 `|`（分隔符由长度前缀字段前置）。
//!    与 server `receipt.rs::RECEIPT_SIGNING_DOMAIN`、daemon `client.rs` 同名常量一致。
//! 2. **长度前缀字段**：`|name=<字节长度>:<value>`（`push_len_field` 逐字节同构）。
//!    长度取**字节长度**（`str::len()`），不是字符数——含非 ASCII 的 mid 会因此不同。
//! 3. **域串字段顺序**（7 个业务字段，固定）：`mid → lease_id → seq_from → seq_to →
//!    count → payload_digest → ts`；数值字段以**十进制字符串**参与（大数不走 JSON number）。
//! 4. **签名管线：先哈希、对哈希签**（⚠️ 不是对域串直签——那是 Lease Token 的形状）：
//!    `SHA-256(b"iotdaq.receipt.semantic.v1|" ++ 域串)` → 对 **32 字节哈希**做 Ed25519。
//!    二级域前缀**以 `|` 结尾**（与域常量不同，这是服务端既有事实）。
//! 5. **签名编码**：STANDARD base64（带填充）；URL-safe / no-pad 服务端明确拒绝。
//! 6. **请求体白名单恰好 8 字段**：`device_mid, lease_id, seq_from, seq_to, count,
//!    payload_digest, ts, sig`；其中 `seq_from/seq_to/count/ts` 一律 **JSON 字符串**
//!    （安全整数上限 2^53−1 红线）。**绝不携带业务数值**（点位值 / 设备名 / 样本）。
//! 7. **幂等**：服务端批次键 = `SHA-256(iotdaq.audit.batch.v1|mid=..|lease_id=..|
//!    seq_from=..|seq_to=..)`（server `audit.rs::batch_key`）——批次身份**不含** sig /
//!    payload_digest / ts，故本客户端**重试时原样重放同一 JSON body** 即天然幂等安全。
//! 8. **响应形状**：`ApiEnvelope{code,data:{accepted,gap,server_time,warnings}}`
//!    （server `proto.rs`）；`code != "OK"` 视为服务端拒绝。`warnings` 非空时经
//!    `eprintln!` 输出告警（含 gap 明细），**绝不 panic**。
//!
//! # 断网降级（B 档红线）
//!
//! `submit` 只做「签名 + 入队」，**绝不发起网络 IO、绝不阻塞调用方**（网络由内部
//! worker 任务异步执行）。POST 失败 / 超时 / 响应非法 → 回执留在重试队列队首，
//! 按退避间隔**按序补报**（同批次重试内容不变，幂等由服务端批次键保证）。
//! 重试队列容量上限（默认 256），满时**丢最旧** + 计数器 + `eprintln!` 告警标记。
//!
//! # 已知限制
//!
//! - 仅支持 `http://` 控制通道（与服务端 http 部署口径一致；TLS 客户端属独立工程量，
//!   与 `driver/http.rs` V1 同一取舍，见其模块文档）。
//! - `seq_from / seq_to` 超出 `i64` 范围（如 `u64::MAX`）→ 构造期即拒绝：服务端
//!   `parse_receipt_ints` 按 `i64` 解析会拒绝该回执，客户端 fail-fast 避免无效重试。
//! - 签名密钥经 [`ReceiptSigner`] trait 注入；时钟经 [`ReceiptClock`] 闭包注入
//!   （测试受控）。

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use async_trait::async_trait;
use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine as _;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::Notify;
use tokio::time::timeout;

use crate::auth::client::RECEIPT_FIELD_WHITELIST;
use crate::error::{DaemonError, DaemonResult};
use crate::north::encoder::semantic_digest;
use protocol_proto::TelemetryBatch;

// ============================================================================
// 常量（默认值集中声明，避免散落魔法数字）
// ============================================================================

/// 回执签名域（**单源自** `auth::client` 的同名常量，编译期保证逐字节一致）。
pub const RECEIPT_SIGNING_DOMAIN: &str = crate::auth::client::RECEIPT_SIGNING_DOMAIN;

/// 回执语义哈希的二级域前缀（**以 `|` 结尾**；与 server `receipt.rs::RECEIPT_SEMANTIC_DOMAIN`
/// 一致。`client.rs` 将其内联在 `semantic_digest` 中，本模块提为常量便于测试对齐）。
pub const RECEIPT_SEMANTIC_DOMAIN_PREFIX: &[u8] = b"iotdaq.receipt.semantic.v1|";

/// 默认请求超时（秒）。
pub const DEFAULT_HTTP_TIMEOUT_SECS: u64 = 2;
/// 默认重试队列容量。
pub const DEFAULT_QUEUE_CAPACITY: usize = 256;
/// 默认重试退避间隔（毫秒）。
pub const DEFAULT_RETRY_DELAY_MS: u64 = 5_000;
/// 响应体上限（1 MiB）：防御恶意 / 异常服务端把内存读爆。
const MAX_RESPONSE_BYTES: usize = 1 << 20;
/// `last_warnings` 保留的最近告警条数上限（防无限增长）。
const MAX_KEPT_WARNINGS: usize = 32;

// ============================================================================
// 密钥与时钟注入
// ============================================================================

/// 回执签名密钥注入点。
///
/// 生产由宿主注入设备 Ed25519 私钥（与 Lease Token / AuthBlock 同一把设备私钥）；
/// 测试用 crate 内 [`Ed25519ReceiptSigner`] 以固定种子生成 keypair。
pub trait ReceiptSigner: Send + Sync {
    /// 对 **32 字节语义哈希**（`SHA-256(二级域前缀 ++ 域串)`）做 Ed25519 签名，
    /// 返回 STANDARD base64（带填充）文本。
    ///
    /// # Errors
    /// 密钥不可用 / 签名失败 → [`DaemonError`]（绝不返回空签名冒充有效签名）。
    fn sign_payload_hash(&self, payload_hash: &[u8; 32]) -> DaemonResult<String>;
}

/// Ed25519 签名器（包装 `ed25519_dalek::SigningKey`；生产与测试共用同一实现）。
pub struct Ed25519ReceiptSigner {
    key: ed25519_dalek::SigningKey,
}

impl Ed25519ReceiptSigner {
    /// 从 32 字节种子构造（Ed25519 私钥即种子；**生产密钥绝不硬编码**）。
    #[must_use]
    pub fn from_seed(seed: &[u8; 32]) -> Self {
        Ed25519ReceiptSigner {
            key: ed25519_dalek::SigningKey::from_bytes(seed),
        }
    }

    /// 对应的 Ed25519 公钥原始 32 字节（注册进服务端密钥环用）。
    #[must_use]
    pub fn public_key_bytes(&self) -> [u8; 32] {
        self.key.verifying_key().to_bytes()
    }
}

impl ReceiptSigner for Ed25519ReceiptSigner {
    fn sign_payload_hash(&self, payload_hash: &[u8; 32]) -> DaemonResult<String> {
        use ed25519_dalek::Signer as _;
        let signature = self.key.sign(payload_hash);
        Ok(B64.encode(signature.to_bytes()))
    }
}

/// 回执时钟注入点：返回 unix 秒。测试注入固定闭包以受控 `ts`。
pub type ReceiptClock = Arc<dyn Fn() -> i64 + Send + Sync>;

/// 固定值时钟（测试便捷入口）。
#[must_use]
pub fn fixed_clock(ts: i64) -> ReceiptClock {
    Arc::new(move || ts)
}

// ============================================================================
// 域串重建（与服务端 receipt.rs 逐字节一致）
// ============================================================================

/// 追加一个长度前缀字段：`|name=<value 的字节长度>:<value>`。
///
/// 与 server `receipt.rs::push_len_field` / daemon `client.rs::push_len_field`
/// **逐字节一致**（含非 ASCII 字段时长度取字节长度）。
fn push_len_field(out: &mut String, name: &str, value: &str) {
    out.push('|');
    out.push_str(name);
    out.push('=');
    out.push_str(&value.len().to_string());
    out.push(':');
    out.push_str(value);
}

/// 渲染回执签名域串（**未哈希**），与 server `receipt.rs::render_receipt_signing_message`
/// 逐字节一致。格式（7 个字段，顺序固定）：
///
/// ```text
/// iotdaq.receipt.v1|mid=<len>:<mid>|lease_id=<len>:<id>|seq_from=<len>:<n>|seq_to=<len>:<n>|count=<len>:<n>|payload_digest=<len>:<s>|ts=<len>:<ts>
/// ```
#[must_use]
pub fn receipt_signing_message(
    device_mid: &str,
    lease_id: &str,
    seq_from: i64,
    seq_to: i64,
    count: i64,
    payload_digest: &str,
    ts: i64,
) -> String {
    let mut out = String::with_capacity(192);
    out.push_str(RECEIPT_SIGNING_DOMAIN);
    push_len_field(&mut out, "mid", device_mid);
    push_len_field(&mut out, "lease_id", lease_id);
    push_len_field(&mut out, "seq_from", &seq_from.to_string());
    push_len_field(&mut out, "seq_to", &seq_to.to_string());
    push_len_field(&mut out, "count", &count.to_string());
    push_len_field(&mut out, "payload_digest", payload_digest);
    push_len_field(&mut out, "ts", &ts.to_string());
    out
}

/// 计算回执待签数据：`SHA-256(RECEIPT_SEMANTIC_DOMAIN_PREFIX ++ 域串)`，32 字节。
///
/// 与 server `receipt.rs::receipt_payload_hash` / daemon `client.rs::semantic_digest`
/// 等价——**先哈希、对哈希签**，不是对域串直签。
#[must_use]
pub fn receipt_payload_hash(signing_message: &str) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(RECEIPT_SEMANTIC_DOMAIN_PREFIX);
    hasher.update(signing_message.as_bytes());
    let digest = hasher.finalize();
    let mut out = [0u8; 32];
    out.copy_from_slice(&digest);
    out
}

// ============================================================================
// 回执批次与签名
// ============================================================================

/// 一次待上报的回执批次（**只有序号区间与摘要，绝不携带业务数值**）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReceiptBatch {
    /// 设备机位码（服务端 cursor 归属者）。
    pub device_mid: String,
    /// 租约 ID。
    pub lease_id: String,
    /// 序号区间起点（含）。
    pub seq_from: i64,
    /// 序号区间终点（含）。
    pub seq_to: i64,
    /// 区间内消息条数（`seq_to - seq_from + 1`）。
    pub count: i64,
    /// 业务负载语义摘要（不透明 hex 字符串；服务端不解其内容）。
    pub payload_digest: String,
}

impl ReceiptBatch {
    /// 构造批次并做客户端侧前置校验（fail-fast，避免注定被服务端拒绝的回执进重试队列）。
    ///
    /// `seq_from` / `seq_to` 以 `u64` 传入：批次身份在服务端批次键中以 `u64` 参与，
    /// 但域串 / 验签按 `i64` 解析——超出 `i64` 的序号（如 `u64::MAX`）服务端必然拒绝，
    /// 此处直接返回错误（**绝不静默截断、绝不 panic**）。
    ///
    /// # Errors
    /// - `device_mid` / `lease_id` 空白 → [`DaemonError::ConfigError`]
    /// - 序号超出 `i64` 范围 / `seq_from > seq_to` / 计数溢出 → [`DaemonError::ConfigError`]
    pub fn new(
        device_mid: impl Into<String>,
        lease_id: impl Into<String>,
        seq_from: u64,
        seq_to: u64,
        payload_digest: impl Into<String>,
    ) -> DaemonResult<Self> {
        let device_mid = device_mid.into();
        let lease_id = lease_id.into();
        if device_mid.trim().is_empty() {
            return Err(DaemonError::ConfigError(
                "receipt device_mid must not be empty".to_string(),
            ));
        }
        if lease_id.trim().is_empty() {
            return Err(DaemonError::ConfigError(
                "receipt lease_id must not be empty".to_string(),
            ));
        }
        // 服务端 parse_receipt_ints 按 i64 解析；越界必然被拒 → fail-fast。
        let seq_from: i64 = i64::try_from(seq_from).map_err(|_| {
            DaemonError::ConfigError(format!(
                "receipt seq_from {seq_from} exceeds i64 range (server would reject)"
            ))
        })?;
        let seq_to: i64 = i64::try_from(seq_to).map_err(|_| {
            DaemonError::ConfigError(format!(
                "receipt seq_to {seq_to} exceeds i64 range (server would reject)"
            ))
        })?;
        if seq_from > seq_to {
            return Err(DaemonError::ConfigError(format!(
                "receipt seq_from {seq_from} is greater than seq_to {seq_to}"
            )));
        }
        // seq_to >= seq_from（u64），差 + 1 不会下溢；只防 i64 上溢。
        let count_u64 = seq_to_u64(seq_to) - seq_from_u64(seq_from) + 1;
        let count: i64 = i64::try_from(count_u64).map_err(|_| {
            DaemonError::ConfigError(format!(
                "receipt count {count_u64} exceeds i64 range (server would reject)"
            ))
        })?;
        Ok(ReceiptBatch {
            device_mid,
            lease_id,
            seq_from,
            seq_to,
            count,
            payload_digest: payload_digest.into(),
        })
    }

    /// 从北向数据批次构造回执（摘要复用 `north::encoder::semantic_digest` 的语义哈希）。
    ///
    /// # Errors
    /// 同 [`ReceiptBatch::new`]。
    pub fn from_telemetry(
        batch: &TelemetryBatch,
        device_mid: impl Into<String>,
        lease_id: impl Into<String>,
        seq_from: u64,
        seq_to: u64,
    ) -> DaemonResult<Self> {
        // 单一定义：语义哈希只来自 north::encoder（禁止复制第二份规范化实现）。
        let payload_digest = hex::encode(semantic_digest(batch));
        ReceiptBatch::new(device_mid, lease_id, seq_from, seq_to, payload_digest)
    }
}

/// `i64 → u64` 的显式回转（仅用于区间差计算；值域已由构造期保证非负）。
fn seq_from_u64(v: i64) -> u64 {
    u64::try_from(v).unwrap_or(0)
}

/// 同 [`seq_from_u64`]。
fn seq_to_u64(v: i64) -> u64 {
    u64::try_from(v).unwrap_or(0)
}

/// 已签名的回执（待 / 已入重试队列）。`body_json` 一经生成**不再改变**——
/// 重试原样重放，幂等由服务端批次键保证。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SignedReceipt {
    /// 批次内容。
    pub batch: ReceiptBatch,
    /// 签名时刻（unix 秒，来自注入时钟）。
    pub ts: i64,
    /// STANDARD base64 Ed25519 签名（对语义哈希）。
    pub sig: String,
    /// 序列化好的 JSON 请求体（8 字段白名单；重试期间逐字节不变）。
    pub body_json: String,
}

/// 构造回执请求体：**白名单恰好 8 字段**，数值一律 JSON 字符串。
#[must_use]
pub fn build_receipt_body(batch: &ReceiptBatch, ts: i64, sig: &str) -> Value {
    json!({
        "device_mid": batch.device_mid,
        "lease_id": batch.lease_id,
        "seq_from": batch.seq_from.to_string(),
        "seq_to": batch.seq_to.to_string(),
        "count": batch.count.to_string(),
        "payload_digest": batch.payload_digest,
        "ts": ts.to_string(),
        "sig": sig,
    })
}

/// 签名一个批次：域串重建 → 语义哈希 → Ed25519 → 序列化白名单请求体。
///
/// # Errors
/// - `ts` 为负 / 签名失败 → [`DaemonError`]
/// - 签名器返回空签名 → [`DaemonError::SecurityError`]（绝不冒充有效签名）
pub fn sign_batch(
    batch: &ReceiptBatch,
    ts: i64,
    signer: &dyn ReceiptSigner,
) -> DaemonResult<SignedReceipt> {
    let message = receipt_signing_message(
        &batch.device_mid,
        &batch.lease_id,
        batch.seq_from,
        batch.seq_to,
        batch.count,
        &batch.payload_digest,
        ts,
    );
    let payload_hash = receipt_payload_hash(&message);
    let sig = signer.sign_payload_hash(&payload_hash)?;
    if sig.trim().is_empty() {
        return Err(DaemonError::SecurityError(
            "receipt signer returned an empty signature".to_string(),
        ));
    }
    let body = build_receipt_body(batch, ts, &sig);
    let body_json = serde_json::to_string(&body)
        .map_err(|e| DaemonError::ProtocolError(format!("receipt body serialize: {e}")))?;
    Ok(SignedReceipt {
        batch: batch.clone(),
        ts,
        sig,
        body_json,
    })
}

// ============================================================================
// 响应解析
// ============================================================================

/// `POST /audit/receipt` 的响应（与 server `proto.rs::AuditReceiptResponse` 对齐；
/// 兼容 `ApiEnvelope` 包裹与裸对象两种形状）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuditReceiptResponse {
    /// 是否接受（幂等重放也算接受）。
    pub accepted: bool,
    /// 跳空 / 回退 / 缺失判定（`none` / `gap` / `overlap` / `missing`）。
    pub gap: String,
    /// 服务端时间（UTC 秒，字符串）。
    pub server_time: String,
    /// 告警明细（跳空 / 回退 / 重叠时非空）。
    pub warnings: Vec<String>,
}

/// 解析响应体 JSON。
///
/// - 若顶层含 `code` 字段且非 `"OK"` → 服务端业务拒绝（`AuthError`）。
/// - 数据取 `data` 对象（envelope 形状），否则取顶层（裸对象形状）。
///
/// # Errors
/// 非 JSON / 缺 `accepted` / envelope 拒绝 → [`DaemonError`]。
pub fn parse_receipt_response(body: &str) -> DaemonResult<AuditReceiptResponse> {
    let value: Value = serde_json::from_str(body)
        .map_err(|e| DaemonError::ProtocolError(format!("receipt response json: {e}")))?;

    if let Some(code) = value.get("code").and_then(|c| c.as_str()) {
        if code != "OK" {
            return Err(DaemonError::AuthError(format!(
                "receipt rejected by server with code {code}"
            )));
        }
    }

    // envelope 形状取 data；裸形状取顶层。data 非对象时回退顶层。
    let data = match value.get("data") {
        Some(d) if d.is_object() => d,
        _ => &value,
    };

    // `accepted` 必有；兼容 `ok` 字面（任务口径 ok/accepted 双名）。
    let accepted = data
        .get("accepted")
        .and_then(|b| b.as_bool())
        .or_else(|| data.get("ok").and_then(|b| b.as_bool()))
        .ok_or_else(|| {
            DaemonError::ProtocolError("receipt response is missing 'accepted'".to_string())
        })?;

    let gap = data
        .get("gap")
        .and_then(|g| g.as_str())
        .unwrap_or("none")
        .to_string();
    let server_time = data
        .get("server_time")
        .and_then(|s| s.as_str())
        .unwrap_or("")
        .to_string();
    let warnings = data
        .get("warnings")
        .and_then(|w| w.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|w| w.as_str().map(str::to_string))
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();

    Ok(AuditReceiptResponse {
        accepted,
        gap,
        server_time,
        warnings,
    })
}

// ============================================================================
// 传输层（可注入；默认手写 HTTP/1.1 POST）
// ============================================================================

/// 回执上报传输抽象（测试注入 fake；生产用 [`HttpPostTransport`]）。
#[async_trait]
pub trait ReceiptTransport: Send + Sync {
    /// POST 一个 JSON 文本请求体，2xx 时返回响应体文本。
    ///
    /// # Errors
    /// 连接失败 / 超时 / 非 2xx / 协议错误 → [`DaemonError`]（一律可重试）。
    async fn post_json(&self, url: &str, body: &str) -> DaemonResult<String>;
}

/// 手写 HTTP/1.1 POST 传输（风格与 `driver/http.rs` V1 一致：tokio TcpStream、
/// Content-Length 定界、`Connection: close`、仅 `http://`）。
pub struct HttpPostTransport {
    /// 单次请求超时（默认 2 秒）。
    pub timeout: Duration,
}

impl Default for HttpPostTransport {
    fn default() -> Self {
        HttpPostTransport {
            timeout: Duration::from_secs(DEFAULT_HTTP_TIMEOUT_SECS),
        }
    }
}

#[async_trait]
impl ReceiptTransport for HttpPostTransport {
    async fn post_json(&self, url: &str, body: &str) -> DaemonResult<String> {
        let io = post_once(url, body);
        match timeout(self.timeout, io).await {
            Ok(result) => result,
            Err(_) => Err(DaemonError::NetworkError(format!(
                "receipt post timeout after {:?}",
                self.timeout
            ))),
        }
    }
}

/// 解析 `http://host[:port]/path` URL（仅 http；与 `driver/http.rs` 同口径）。
///
/// # Errors
/// 非 `http://` / 缺主机 / 端口非法 → [`DaemonError::ProtocolError`]。
fn parse_http_url(url: &str) -> DaemonResult<(String, u16, String)> {
    let wrap = |detail: String| {
        DaemonError::ProtocolError(format!("invalid receipt endpoint url {url:?}: {detail}"))
    };
    let rest = url
        .strip_prefix("http://")
        .ok_or_else(|| wrap("only http:// scheme supported (no TLS)".to_string()))?;
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

/// 构建最小 HTTP/1.1 POST 报文（`\r\n\r\n` + body；Content-Length 定界）。
fn build_post_request(host: &str, port: u16, path: &str, body: &str) -> String {
    let host_header = if port == 80 {
        host.to_string()
    } else {
        format!("{host}:{port}")
    };
    format!(
        "POST {path} HTTP/1.1\r\n\
         Host: {host_header}\r\n\
         Content-Type: application/json\r\n\
         Content-Length: {}\r\n\
         Accept: application/json\r\n\
         Connection: close\r\n\
         \r\n{}",
        body.len(),
        body
    )
}

/// 定位字节流中的 `\r\n` 行尾。
fn find_crlf(data: &[u8]) -> DaemonResult<usize> {
    data.windows(2)
        .position(|w| w == b"\r\n")
        .ok_or_else(|| DaemonError::ProtocolError("unterminated line (missing CRLF)".to_string()))
}

/// 解码 chunked 传输编码体（最小实现；与 `driver/http.rs::decode_chunked` 同口径）。
fn decode_chunked(mut data: &[u8]) -> DaemonResult<Vec<u8>> {
    let mut out = Vec::new();
    loop {
        let line_end = find_crlf(data)?;
        let line = std::str::from_utf8(&data[..line_end])
            .map_err(|_| DaemonError::ProtocolError("chunk size line is not utf-8".to_string()))?;
        let size_str = line.split(';').next().unwrap_or("").trim();
        let size = usize::from_str_radix(size_str, 16)
            .map_err(|_| DaemonError::ProtocolError(format!("invalid chunk size {size_str:?}")))?;
        data = &data[line_end + 2..];
        if size == 0 {
            return Ok(out);
        }
        if out.len() + size > MAX_RESPONSE_BYTES {
            return Err(DaemonError::ProtocolError(
                "receipt response exceeds 1 MiB cap".to_string(),
            ));
        }
        if data.len() < size + 2 {
            return Err(DaemonError::ProtocolError(format!(
                "truncated chunk: need {size} bytes, got {}",
                data.len()
            )));
        }
        out.extend_from_slice(&data[..size]);
        if &data[size..size + 2] != b"\r\n" {
            return Err(DaemonError::ProtocolError(
                "missing CRLF after chunk data".to_string(),
            ));
        }
        data = &data[size + 2..];
    }
}

/// 解析 HTTP 响应：状态行 + 头 + 体（Content-Length / chunked / EOF 定界）。
///
/// 非 2xx 状态码视为可重试的传输层失败（[`DaemonError::NetworkError`]）。
fn parse_http_response(raw: &[u8]) -> DaemonResult<String> {
    let header_end = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .ok_or_else(|| {
            DaemonError::ProtocolError("missing header terminator \\r\\n\\r\\n".to_string())
        })?;
    let head = std::str::from_utf8(&raw[..header_end])
        .map_err(|_| DaemonError::ProtocolError("response head is not utf-8".to_string()))?;
    let body = &raw[header_end + 4..];

    let mut lines = head.split("\r\n");
    let status_line = lines
        .next()
        .ok_or_else(|| DaemonError::ProtocolError("empty response head".to_string()))?;
    let status: u16 = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| {
            DaemonError::ProtocolError(format!("malformed status line {status_line:?}"))
        })?;
    if !(200..300).contains(&status) {
        return Err(DaemonError::NetworkError(format!(
            "receipt endpoint returned http status {status}"
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
        let bytes = decode_chunked(body)?;
        String::from_utf8(bytes).map_err(|_| {
            DaemonError::ProtocolError("receipt response body is not utf-8".to_string())
        })
    } else if let Some(len) = content_length {
        if body.len() < len {
            return Err(DaemonError::ProtocolError(format!(
                "truncated body: header declares {len} bytes, got {}",
                body.len()
            )));
        }
        String::from_utf8(body[..len].to_vec()).map_err(|_| {
            DaemonError::ProtocolError("receipt response body is not utf-8".to_string())
        })
    } else {
        // 无 Content-Length 也非 chunked：Connection: close 下以 EOF 定界。
        String::from_utf8(body.to_vec()).map_err(|_| {
            DaemonError::ProtocolError("receipt response body is not utf-8".to_string())
        })
    }
}

/// 执行一次 POST（连接 → 发送 → 读至 EOF（1 MiB 上限）→ 解析）。
async fn post_once(url: &str, body: &str) -> DaemonResult<String> {
    let (host, port, path) = parse_http_url(url)?;
    let addr = format!("{host}:{port}");
    let mut stream = TcpStream::connect(&addr)
        .await
        .map_err(|e| DaemonError::NetworkError(format!("receipt connect {addr}: {e}")))?;
    let request = build_post_request(&host, port, &path, body);
    stream
        .write_all(request.as_bytes())
        .await
        .map_err(|e| DaemonError::NetworkError(format!("receipt send request: {e}")))?;
    let mut raw = Vec::new();
    let mut buf = [0u8; 4096];
    loop {
        let n = stream
            .read(&mut buf)
            .await
            .map_err(|e| DaemonError::NetworkError(format!("receipt read response: {e}")))?;
        if n == 0 {
            break;
        }
        if raw.len() + n > MAX_RESPONSE_BYTES {
            return Err(DaemonError::ProtocolError(
                "receipt response exceeds 1 MiB cap".to_string(),
            ));
        }
        raw.extend_from_slice(&buf[..n]);
    }
    parse_http_response(&raw)
}

// ============================================================================
// 上报客户端（submit 不阻塞 + 重试队列 + worker）
// ============================================================================

/// 上报客户端配置（URL 由装配注入，**禁止硬编码**）。
#[derive(Debug, Clone)]
pub struct ReceiptReporterConfig {
    /// 完整端点 URL（如 `http://licensing.vendor.com/v1/audit/receipt`；仅 `http://`）。
    pub endpoint_url: String,
    /// 单次请求超时（默认 2 秒）。
    pub timeout: Duration,
    /// 重试队列容量上限（默认 256；满时丢最旧）。
    pub queue_capacity: usize,
    /// 重试退避间隔（默认 5 秒；测试可调小）。
    pub retry_delay: Duration,
}

impl Default for ReceiptReporterConfig {
    fn default() -> Self {
        ReceiptReporterConfig {
            endpoint_url: String::new(),
            timeout: Duration::from_secs(DEFAULT_HTTP_TIMEOUT_SECS),
            queue_capacity: DEFAULT_QUEUE_CAPACITY,
            retry_delay: Duration::from_millis(DEFAULT_RETRY_DELAY_MS),
        }
    }
}

/// 重试队列里的一条待报回执（`body_json` 重试期间不变）。
#[derive(Debug, Clone)]
struct PendingReceipt {
    /// 完整 JSON 请求体（逐字节不变，服务端幂等键保证重放安全）。
    body_json: String,
    /// 人读标识（日志 / 告警用）：`mid|lease|[from,to]`。
    identity: String,
}

/// 运行统计（计数器；`queued` 由队列长度实时得出）。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReceiptReporterStats {
    /// 成功上报且响应解析通过的次数。
    pub sent_ok: u64,
    /// 发送 / 解析失败次数（含每次重试）。
    pub send_failed: u64,
    /// 队列满丢弃的最旧回执条数。
    pub dropped_overflow: u64,
    /// 服务端返回的告警总条数（warnings 非空即触达操作员）。
    pub warnings_total: u64,
    /// 正在发送中的条数（0 或 1；单 worker）。
    pub in_flight: u64,
}

/// 客户端共享内部状态。
struct Inner {
    cfg: ReceiptReporterConfig,
    transport: Arc<dyn ReceiptTransport>,
    signer: Arc<dyn ReceiptSigner>,
    clock: ReceiptClock,
    /// 重试队列（FIFO：失败回到队首，保证按序补报）。
    queue: Mutex<VecDeque<PendingReceipt>>,
    /// worker 唤醒信号。
    notify: Notify,
    /// 优雅停机标记（队列排空后退出）。
    shutdown: AtomicBool,
    /// worker 是否已启动（惰性：首次 submit 时在 tokio 运行时内 spawn）。
    worker_started: AtomicBool,
    /// worker 句柄（shutdown 时 join）。
    worker: Mutex<Option<tokio::task::JoinHandle<()>>>,
    stats: Mutex<ReceiptReporterStats>,
    /// 最近收到的服务端告警（cap [`MAX_KEPT_WARNINGS`]）。
    last_warnings: Mutex<Vec<String>>,
}

/// 审计回执上报客户端（B 档控制通道）。
///
/// - `submit` 只做签名 + 入队，**绝不阻塞调用方**（网络在内部 worker 异步执行）；
/// - 断网 / 超时 → 回执留队按退避重试，恢复后按序补报，重试内容不变；
/// - 队列满 → 丢最旧 + 计数 + `eprintln!` 告警；
/// - 零 panic：所有锁中毒恢复、算术 checked、网络失败一律返回错误。
#[derive(Clone)]
pub struct ReceiptReporter {
    inner: Arc<Inner>,
}

/// 中毒恢复的锁获取（**绝不 panic**：poison 时取回内部数据继续运行）。
fn lock_recover<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    match m.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

impl ReceiptReporter {
    /// 构造客户端（默认手写 HTTP 传输）。
    ///
    /// worker 惰性启动：首次 [`ReceiptReporter::submit`] 时在当前 tokio 运行时内 spawn。
    #[must_use]
    pub fn new(
        cfg: ReceiptReporterConfig,
        signer: Arc<dyn ReceiptSigner>,
        clock: ReceiptClock,
    ) -> Self {
        Self::with_transport(cfg, signer, clock, Arc::new(HttpPostTransport::default()))
    }

    /// 构造客户端并注入传输实现（测试用）。
    #[must_use]
    pub fn with_transport(
        cfg: ReceiptReporterConfig,
        signer: Arc<dyn ReceiptSigner>,
        clock: ReceiptClock,
        transport: Arc<dyn ReceiptTransport>,
    ) -> Self {
        ReceiptReporter {
            inner: Arc::new(Inner {
                cfg,
                transport,
                signer,
                clock,
                queue: Mutex::new(VecDeque::new()),
                notify: Notify::new(),
                shutdown: AtomicBool::new(false),
                worker_started: AtomicBool::new(false),
                worker: Mutex::new(None),
                stats: Mutex::new(ReceiptReporterStats::default()),
                last_warnings: Mutex::new(Vec::new()),
            }),
        }
    }

    /// 提交一个回执批次：签名（注入时钟打 `ts`）→ 入队 → 立即返回。
    ///
    /// **绝不发起网络 IO、绝不阻塞**；发送由内部 worker 异步执行，失败自动重试。
    /// 必须在 tokio 运行时内调用（worker 惰性 spawn）。
    ///
    /// # Errors
    /// - 端点 URL 为空 → [`DaemonError::ConfigError`]
    /// - 批次校验失败 / 签名失败 → 对应 [`DaemonError`]
    pub async fn submit(&self, batch: &ReceiptBatch) -> DaemonResult<()> {
        if self.inner.cfg.endpoint_url.trim().is_empty() {
            return Err(DaemonError::ConfigError(
                "receipt reporter endpoint_url must not be empty".to_string(),
            ));
        }
        let ts = (self.inner.clock)();
        let signed = sign_batch(batch, ts, self.inner.signer.as_ref())?;
        self.enqueue(signed);
        Ok(())
    }

    /// 入队（容量满 → 丢最旧 + 计数 + 告警）并唤醒 worker。
    fn enqueue(&self, signed: SignedReceipt) {
        self.ensure_worker();
        let identity = format!(
            "{}|{}|[{},{}]",
            signed.batch.device_mid,
            signed.batch.lease_id,
            signed.batch.seq_from,
            signed.batch.seq_to
        );
        {
            let mut queue = lock_recover(&self.inner.queue);
            let capacity = self.inner.cfg.queue_capacity.max(1);
            if queue.len() >= capacity {
                if let Some(dropped) = queue.pop_front() {
                    let mut stats = lock_recover(&self.inner.stats);
                    stats.dropped_overflow = stats.dropped_overflow.saturating_add(1);
                    let total = stats.dropped_overflow;
                    drop(stats);
                    eprintln!(
                        "[receipt_reporter] retry queue overflow: dropped oldest receipt {} (dropped_total={total})",
                        dropped.identity
                    );
                }
            }
            queue.push_back(PendingReceipt {
                body_json: signed.body_json,
                identity,
            });
        }
        self.inner.notify.notify_one();
    }

    /// 惰性启动 worker（必须在 tokio 运行时上下文内调用）。
    fn ensure_worker(&self) {
        if !self.inner.worker_started.swap(true, Ordering::SeqCst) {
            let inner = Arc::clone(&self.inner);
            let handle = tokio::spawn(async move {
                run_worker(inner).await;
            });
            *lock_recover(&self.inner.worker) = Some(handle);
        }
    }

    // ---- 观测（测试与运维） ----

    /// 当前统计快照（`queued` 见 [`ReceiptReporter::queue_len`]）。
    #[must_use]
    pub fn stats(&self) -> ReceiptReporterStats {
        lock_recover(&self.inner.stats).clone()
    }

    /// 当前重试队列长度。
    #[must_use]
    pub fn queue_len(&self) -> usize {
        lock_recover(&self.inner.queue).len()
    }

    /// 最近收到的服务端告警明细（最多 [`MAX_KEPT_WARNINGS`] 条）。
    #[must_use]
    pub fn last_warnings(&self) -> Vec<String> {
        lock_recover(&self.inner.last_warnings).clone()
    }

    /// 队列内待报回执的人读标识（FIFO 顺序；测试断言丢最旧 / 按序补报用）。
    fn queue_identities(&self) -> Vec<String> {
        lock_recover(&self.inner.queue)
            .iter()
            .map(|p| p.identity.clone())
            .collect()
    }

    /// 优雅停机：置停机标记、唤醒 worker 并等待其退出（队列排空后返回）。
    pub async fn shutdown(&self) {
        self.inner.shutdown.store(true, Ordering::SeqCst);
        self.inner.notify.notify_one();
        let handle = lock_recover(&self.inner.worker).take();
        if let Some(handle) = handle {
            let _ = handle.await;
        }
    }
}

/// worker 主循环：单线程顺序消费队列（保证按序补报）。
///
/// - 成功 → 解析响应；`warnings` 非空 → `eprintln!` 告警 + 计数；
/// - 失败（网络 / 非 2xx / 响应非法）→ 回执放回**队首**（保序），退避后重试；
/// - 队列空 → 等 [`Notify`] 唤醒；停机标记置位且队列空 → 退出。
async fn run_worker(inner: Arc<Inner>) {
    loop {
        if inner.shutdown.load(Ordering::SeqCst) && lock_recover(&inner.queue).is_empty() {
            break;
        }
        let pending = lock_recover(&inner.queue).pop_front();
        match pending {
            Some(pending) => {
                {
                    let mut stats = lock_recover(&inner.stats);
                    stats.in_flight = stats.in_flight.saturating_add(1);
                }
                let result = inner
                    .transport
                    .post_json(&inner.cfg.endpoint_url, &pending.body_json)
                    .await
                    .and_then(|body| parse_receipt_response(&body));
                {
                    let mut stats = lock_recover(&inner.stats);
                    stats.in_flight = stats.in_flight.saturating_sub(1);
                }
                match result {
                    Ok(resp) => {
                        let mut stats = lock_recover(&inner.stats);
                        stats.sent_ok = stats.sent_ok.saturating_add(1);
                        drop(stats);
                        if !resp.warnings.is_empty() {
                            let mut stats = lock_recover(&inner.stats);
                            stats.warnings_total = stats
                                .warnings_total
                                .saturating_add(resp.warnings.len() as u64);
                            drop(stats);
                            let mut kept = lock_recover(&inner.last_warnings);
                            for detail in &resp.warnings {
                                // 告警触达操作员（含 gap 明细）；stderr 输出失败被忽略（绝不 panic）。
                                eprintln!(
                                    "[receipt_reporter] audit warning: gap={} detail={detail}",
                                    resp.gap
                                );
                                kept.push(detail.clone());
                            }
                            let excess = kept.len().saturating_sub(MAX_KEPT_WARNINGS);
                            kept.drain(..excess);
                        }
                    }
                    Err(_) => {
                        {
                            let mut stats = lock_recover(&inner.stats);
                            stats.send_failed = stats.send_failed.saturating_add(1);
                        }
                        // 放回队首（保序：恢复后按序补报，同批次内容不变）。
                        lock_recover(&inner.queue).push_front(pending);
                        tokio::time::sleep(inner.cfg.retry_delay).await;
                    }
                }
            }
            None => {
                if inner.shutdown.load(Ordering::SeqCst) {
                    break;
                }
                inner.notify.notified().await;
            }
        }
    }
}

// ============================================================================
// 测试（17 条）
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use std::future::pending;
    use std::sync::atomic::AtomicU32;

    /// 测试签名密钥种子（**仅测试，禁止真实部署**）。
    const TEST_SEED: [u8; 32] = *b"iotdaq-receipt-reporter-test-000";
    /// 测试基准时间（UTC 秒，取远未来 2100-01-01）。
    const T0: i64 = 4_102_444_800;

    /// 测试签名器。
    fn signer() -> Arc<dyn ReceiptSigner> {
        Arc::new(Ed25519ReceiptSigner::from_seed(&TEST_SEED))
    }

    /// 构造一个常规批次。
    fn batch() -> ReceiptBatch {
        ReceiptBatch::new("MID-0001", "lease-0001", 1, 100, "sha256:abcdef").expect("valid batch")
    }

    /// 构造客户端（默认配置 + 指定传输）。
    fn reporter(transport: Arc<dyn ReceiptTransport>) -> ReceiptReporter {
        ReceiptReporter::with_transport(
            ReceiptReporterConfig {
                endpoint_url: "http://licensing.test/v1/audit/receipt".to_string(),
                queue_capacity: 64,
                retry_delay: Duration::from_millis(5),
                ..ReceiptReporterConfig::default()
            },
            signer(),
            fixed_clock(T0),
            transport,
        )
    }

    /// 轮询等待条件成立（超时 panic——测试辅助，非生产路径）。
    async fn wait_until(cond: impl Fn() -> bool, label: &'static str) {
        for _ in 0..500 {
            if cond() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("timeout waiting for {label}");
    }

    /// 构造成功 envelope 响应体。
    fn ok_envelope(warnings: Vec<&str>, gap: &str) -> String {
        serde_json::json!({
            "code": "OK",
            "data": {
                "accepted": true,
                "gap": gap,
                "server_time": T0.to_string(),
                "warnings": warnings,
            },
            "message": "ok",
            "trace_id": "trace-1",
        })
        .to_string()
    }

    // ------------------------------------------------------------------
    // Fake 传输
    // ------------------------------------------------------------------

    /// fake 动作。
    enum FakeAction {
        /// 返回成功响应体。
        Ok(String),
        /// 永不返回（模拟挂死的服务端 / 网络；worker 卡在 in-flight）。
        Pending,
    }

    /// 可编程 fake 传输：记录每次收到的 body。
    struct FakeTransport {
        action: Mutex<FakeAction>,
        bodies: Mutex<Vec<String>>,
    }

    impl FakeTransport {
        fn new(action: FakeAction) -> Arc<Self> {
            Arc::new(FakeTransport {
                action: Mutex::new(action),
                bodies: Mutex::new(Vec::new()),
            })
        }
    }

    #[async_trait]
    impl ReceiptTransport for FakeTransport {
        async fn post_json(&self, _url: &str, body: &str) -> DaemonResult<String> {
            lock_recover(&self.bodies).push(body.to_string());
            // 先取动作快照再 await（guard 不得跨 await）。
            let action: Option<DaemonResult<String>> = match &*lock_recover(&self.action) {
                FakeAction::Ok(s) => Some(Ok(s.clone())),
                FakeAction::Pending => None,
            };
            match action {
                Some(result) => result,
                None => pending().await,
            }
        }
    }

    /// 先失败 N 次、之后成功并记录 body 的 flaky 传输。
    struct FlakyTransport {
        failures_left: AtomicU32,
        bodies: Mutex<Vec<String>>,
    }

    impl FlakyTransport {
        fn new(failures: u32) -> Arc<Self> {
            Arc::new(FlakyTransport {
                failures_left: AtomicU32::new(failures),
                bodies: Mutex::new(Vec::new()),
            })
        }
    }

    #[async_trait]
    impl ReceiptTransport for FlakyTransport {
        async fn post_json(&self, _url: &str, body: &str) -> DaemonResult<String> {
            // 每次尝试都记录（含失败尝试）——供断言「重试内容逐字节不变」。
            lock_recover(&self.bodies).push(body.to_string());
            let should_fail = self
                .failures_left
                .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| {
                    if n > 0 {
                        Some(n - 1)
                    } else {
                        None
                    }
                })
                .is_ok();
            if should_fail {
                return Err(DaemonError::NetworkError("flaky: link down".to_string()));
            }
            Ok(ok_envelope(vec![], "none"))
        }
    }

    // ------------------------------------------------------------------
    // 契约对齐：域串 / 哈希 / 签名
    // ------------------------------------------------------------------

    /// 对齐点 1–4：域串与服务端期望串**逐字节一致**（硬编码向量，手工推导）。
    #[test]
    fn signing_message_is_byte_identical_to_server_contract() {
        let got = receipt_signing_message(
            "MID-0001",
            "lease-0001",
            1,
            100,
            100,
            "sha256:abcdef",
            1_700_000_000,
        );
        let expected = concat!(
            "iotdaq.receipt.v1",
            "|mid=8:MID-0001",
            "|lease_id=10:lease-0001",
            "|seq_from=1:1",
            "|seq_to=3:100",
            "|count=3:100",
            // `sha256:abcdef` 共 13 字节——长度前缀是**字节长度**。
            "|payload_digest=13:sha256:abcdef",
            "|ts=10:1700000000",
        );
        assert_eq!(got, expected, "回执签名域与服务端期望串不一致（跨端漂移）");
    }

    /// 对齐点 2：长度前缀保证值内 `|` / 字段切分不歧义。
    #[test]
    fn length_prefix_makes_delimiter_unambiguous() {
        let a = receipt_signing_message("ab", "c", 1, 2, 3, "d", 4);
        let b = receipt_signing_message("a", "bc", 1, 2, 3, "d", 4);
        assert_ne!(a, b, "不同字段切分必须产生不同域串");
        let c = receipt_signing_message("x|mid=1:y", "c", 1, 2, 3, "d", 4);
        let d = receipt_signing_message("x", "c", 1, 2, 3, "d", 4);
        assert_ne!(c, d, "值内含分隔符样式不得歧义");
    }

    /// 对齐点 4：哈希必须带二级域前缀（先哈希后签，非对域串裸哈希）。
    #[test]
    fn payload_hash_uses_second_level_domain_prefix() {
        let message = receipt_signing_message("m", "l", 1, 2, 3, "d", 4);
        let with_prefix = receipt_payload_hash(&message);

        // 独立重建：SHA-256("iotdaq.receipt.semantic.v1|" ++ message)。
        let mut hasher = Sha256::new();
        hasher.update(b"iotdaq.receipt.semantic.v1|");
        hasher.update(message.as_bytes());
        let expected: [u8; 32] = hasher.finalize().into();
        assert_eq!(with_prefix, expected, "语义哈希必须带二级域前缀");

        // 裸哈希（无前缀）必须不相等——守住「不是对域串直签」。
        let mut bare = Sha256::new();
        bare.update(message.as_bytes());
        let bare: [u8; 32] = bare.finalize().into();
        assert_ne!(with_prefix, bare);
    }

    /// 对齐点 5：签名可被同算法（同公钥）验签；异钥 / 篡改哈希验签失败。
    #[test]
    fn signature_verifies_with_ed25519_and_rejects_foreign_key() {
        use ed25519_dalek::{Signature, Verifier as _};

        let signer = Ed25519ReceiptSigner::from_seed(&TEST_SEED);
        let hash = receipt_payload_hash(&receipt_signing_message(
            "MID-0001",
            "lease-0001",
            1,
            100,
            100,
            "sha256:abcdef",
            T0,
        ));
        let sig_b64 = signer.sign_payload_hash(&hash).expect("sign ok");
        assert!(!sig_b64.is_empty());
        // STANDARD base64（带填充）。
        assert!(sig_b64.ends_with('='), "must be padded STANDARD base64");

        let sig_bytes: [u8; 64] = B64
            .decode(&sig_b64)
            .expect("valid base64")
            .as_slice()
            .try_into()
            .expect("64 bytes");
        let signature = Signature::from_bytes(&sig_bytes);

        let public = ed25519_dalek::VerifyingKey::from_bytes(&signer.public_key_bytes())
            .expect("valid public key");
        public.verify(&hash, &signature).expect("must verify");

        // 异钥必须验签失败。
        let other = ed25519_dalek::SigningKey::from_bytes(&[0x5du8; 32]).verifying_key();
        assert!(other.verify(&hash, &signature).is_err());

        // 篡改哈希必须验签失败。
        let mut tampered = hash;
        tampered[0] ^= 0xFF;
        assert!(public.verify(&tampered, &signature).is_err());
    }

    // ------------------------------------------------------------------
    // 白名单序列化 / 大数 / 禁带业务数值
    // ------------------------------------------------------------------

    /// 对齐点 6：请求体 key 集合**恰好等于** 8 字段白名单（缺一不可多一）。
    #[test]
    fn body_whitelist_is_exactly_eight_fields() {
        let batch = batch();
        let signed = sign_batch(&batch, T0, signer().as_ref()).expect("sign ok");
        let body: Value = serde_json::from_str(&signed.body_json).expect("valid json");

        let mut keys: Vec<String> = body.as_object().expect("object").keys().cloned().collect();
        keys.sort();
        let mut expected: Vec<String> = RECEIPT_FIELD_WHITELIST
            .iter()
            .map(|s| (*s).to_string())
            .collect();
        expected.sort();
        assert_eq!(keys, expected, "body keys must equal the 8-field whitelist");
        assert_eq!(keys.len(), 8);
    }

    /// 大数红线：数值字段一律 JSON 字符串；`u64::MAX` 序号在构造期被拒（不截断不 panic）。
    #[test]
    fn big_seq_numbers_are_json_strings_and_u64_max_is_rejected() {
        // 2^53 + 1 与 i64::MAX：JSON number 早已丢精度的区间，必须走字符串。
        let batch = ReceiptBatch::new(
            "MID-BIG",
            "lease-big",
            9_007_199_254_740_993,
            i64::MAX as u64,
            "sha256:big",
        )
        .expect("valid big batch");
        let signed = sign_batch(&batch, T0, signer().as_ref()).expect("sign ok");
        let body: Value = serde_json::from_str(&signed.body_json).expect("valid json");
        for field in ["seq_from", "seq_to", "count", "ts"] {
            assert!(
                body[field].is_string(),
                "{field} must be a JSON string, got {field:?} = {}",
                body[field]
            );
        }
        assert_eq!(body["seq_from"], serde_json::json!("9007199254740993"));
        assert_eq!(body["seq_to"], serde_json::json!("9223372036854775807"));
        assert_eq!(body["ts"], serde_json::json!(T0.to_string()));

        // u64::MAX：服务端按 i64 解析必拒 → 客户端构造期 fail-fast，绝不静默截断。
        let err = ReceiptBatch::new("MID-BIG", "lease-big", 1, u64::MAX, "sha256:big")
            .expect_err("u64::MAX must be rejected");
        assert!(err.to_string().contains("i64"), "{err}");

        // 区间倒置同样被拒。
        assert!(ReceiptBatch::new("MID-BIG", "lease-big", 10, 1, "d").is_err());
    }

    /// 红线：回执**绝不携带业务数值**——白名单 8 字段之外无任何业务字段。
    #[test]
    fn receipt_carries_no_business_values() {
        let batch = batch();
        let signed = sign_batch(&batch, T0, signer().as_ref()).expect("sign ok");
        let body: Value = serde_json::from_str(&signed.body_json).expect("valid json");
        for extra in [
            "value",
            "values",
            "points",
            "payload",
            "samples",
            "batch",
            "device_id",
            "gateway_id",
            "unit",
            "quality",
        ] {
            assert!(
                body.get(extra).is_none(),
                "receipt must not carry business field {extra}"
            );
        }
        // 摘要是语义哈希的 hex，不泄漏任何业务文本。
        assert_eq!(body["payload_digest"], serde_json::json!("sha256:abcdef"));
    }

    /// 摘要单一定义：`from_telemetry` 的 digest 与 `north::encoder::semantic_digest` 一致。
    #[test]
    fn from_telemetry_digest_matches_semantic_digest() {
        let telemetry = TelemetryBatch {
            points: vec![protocol_proto::DataPoint {
                device_id: "south-dev-42".to_string(),
                point_id: "outlet_pressure".to_string(),
                value: 42.5_f64.to_le_bytes().to_vec(),
                unit: "kPa".to_string(),
                ts: 1_762_999_999_000_000_001,
                quality: protocol_proto::Quality::Good as i32,
            }],
            ts: 1_762_999_999_500_000_000,
            gateway_id: "gw-001".to_string(),
            auth: None,
        };
        let batch =
            ReceiptBatch::from_telemetry(&telemetry, "MID-T", "lease-t", 7, 9).expect("valid");
        assert_eq!(
            batch.payload_digest,
            hex::encode(semantic_digest(&telemetry)),
            "digest 必须单源自 north::encoder::semantic_digest"
        );
        assert_eq!(batch.seq_from, 7);
        assert_eq!(batch.seq_to, 9);
        assert_eq!(batch.count, 3);
    }

    // ------------------------------------------------------------------
    // 响应解析 / 告警触达
    // ------------------------------------------------------------------

    /// 对齐点 8：成功路径（envelope 形状）解析 warnings 为空。
    #[test]
    fn parse_response_success_without_warnings() {
        let resp = parse_receipt_response(&ok_envelope(vec![], "none")).expect("parse ok");
        assert!(resp.accepted);
        assert_eq!(resp.gap, "none");
        assert_eq!(resp.server_time, T0.to_string());
        assert!(resp.warnings.is_empty());
    }

    /// 裸对象形状（无 envelope）同样可解析（`ok` 别名兼容）。
    #[test]
    fn parse_response_accepts_flat_shape_with_ok_alias() {
        let body = serde_json::json!({
            "ok": true,
            "gap": "gap",
            "server_time": "1700000000",
            "warnings": [],
        })
        .to_string();
        let resp = parse_receipt_response(&body).expect("parse ok");
        assert!(resp.accepted);
        assert_eq!(resp.gap, "gap");
    }

    /// 非法响应一律报错（不 panic）：非 JSON / 缺 accepted / envelope 拒绝。
    #[test]
    fn parse_response_rejects_invalid_bodies() {
        assert!(parse_receipt_response("not-json").is_err());
        assert!(parse_receipt_response("{}").is_err(), "缺 accepted");
        let rejected = parse_receipt_response(r#"{"code":"AUTH_FAILED","message":"sig invalid"}"#)
            .expect_err("envelope 拒绝必须报错");
        assert!(rejected.to_string().contains("AUTH_FAILED"), "{rejected}");
    }

    /// gap 告警触达：warnings 非空 → 计数 + 记录 + eprintln 路径执行，绝不 panic。
    #[tokio::test]
    async fn gap_warning_is_surfaced_to_operator() {
        let fake = FakeTransport::new(FakeAction::Ok(ok_envelope(
            vec!["gap: expected seq_from 11, got 25"],
            "gap",
        )));
        let reporter = reporter(fake);
        reporter.submit(&batch()).await.expect("submit ok");
        wait_until(|| reporter.stats().sent_ok == 1, "receipt to be sent").await;

        assert_eq!(reporter.stats().warnings_total, 1);
        let warnings = reporter.last_warnings();
        assert_eq!(warnings.len(), 1);
        assert!(
            warnings[0].contains("expected seq_from 11"),
            "{}",
            warnings[0]
        );
        reporter.shutdown().await;
    }

    // ------------------------------------------------------------------
    // 断网降级：不阻塞 / 重试 / 队列上限 / 并发
    // ------------------------------------------------------------------

    /// submit 在网络挂死场景 < 100ms 返回（B 档红线：调用方绝不阻塞）。
    #[tokio::test]
    async fn submit_returns_within_100ms_when_network_hangs() {
        let fake = FakeTransport::new(FakeAction::Pending);
        let reporter = reporter(fake);
        let start = std::time::Instant::now();
        reporter.submit(&batch()).await.expect("submit ok");
        let elapsed = start.elapsed();
        assert!(
            elapsed < Duration::from_millis(100),
            "submit must not block, took {elapsed:?}"
        );
        // 注意：Pending 传输下 worker 卡在 in-flight，**不可** shutdown().await
        // （会等待卡死的 in-flight 请求）；测试结束由运行时回收 worker。
    }

    /// 断网恢复补报：先失败后成功，重放的是**逐字节相同**的批次内容（服务端幂等安全）。
    #[tokio::test]
    async fn retry_replays_identical_body_after_recovery() {
        let flaky = FlakyTransport::new(2);
        let reporter = reporter(flaky.clone() as Arc<dyn ReceiptTransport>);
        reporter.submit(&batch()).await.expect("submit ok");

        wait_until(
            || reporter.stats().sent_ok == 1,
            "receipt to be sent after retries",
        )
        .await;

        let bodies = lock_recover(&flaky.bodies).clone();
        assert_eq!(
            bodies.len(),
            3,
            "2 次失败 + 1 次成功 = 3 次尝试，实际 {}/{}",
            bodies.len(),
            reporter.stats().send_failed
        );
        assert_eq!(reporter.stats().send_failed, 2);
        // 重试内容逐字节不变（sig / ts / body 全部一致）。
        assert_eq!(bodies[0], bodies[1]);
        assert_eq!(bodies[1], bodies[2], "重试必须原样重放同一 body");
        reporter.shutdown().await;
    }

    /// 队列容量上限：满时**丢最旧** + 计数器告警标记，保留的是最新批次（FIFO）。
    #[tokio::test]
    async fn queue_overflow_drops_oldest_and_counts() {
        // Pending 传输：worker 卡在第 1 条 in-flight，之后入队的都留在队列里。
        let fake = FakeTransport::new(FakeAction::Pending);
        let reporter = ReceiptReporter::with_transport(
            ReceiptReporterConfig {
                endpoint_url: "http://licensing.test/v1/audit/receipt".to_string(),
                queue_capacity: 2,
                retry_delay: Duration::from_millis(5),
                ..ReceiptReporterConfig::default()
            },
            signer(),
            fixed_clock(T0),
            fake,
        );

        reporter
            .submit(&ReceiptBatch::new("M-1", "L", 1, 10, "d1").expect("batch"))
            .await
            .expect("submit 1");
        // 等 worker 把第 1 条取走（in-flight），此后入队行为完全确定。
        wait_until(|| reporter.stats().in_flight == 1, "first item in flight").await;

        reporter
            .submit(&ReceiptBatch::new("M-2", "L", 11, 20, "d2").expect("batch"))
            .await
            .expect("submit 2");
        reporter
            .submit(&ReceiptBatch::new("M-3", "L", 21, 30, "d3").expect("batch"))
            .await
            .expect("submit 3");
        // 队列满（容量 2）：第 4 条入队 → 丢最旧（M-2）。
        reporter
            .submit(&ReceiptBatch::new("M-4", "L", 31, 40, "d4").expect("batch"))
            .await
            .expect("submit 4");

        let stats = reporter.stats();
        assert_eq!(stats.dropped_overflow, 1, "恰好丢 1 条最旧");
        assert_eq!(stats.in_flight, 1);
        assert_eq!(
            reporter.queue_identities(),
            vec!["M-3|L|[21,30]".to_string(), "M-4|L|[31,40]".to_string()],
            "保留的必须是最新两条（FIFO 丢最旧）"
        );
        // 总账：in_flight + queued + dropped == 4。
        assert_eq!(
            stats.in_flight + reporter.queue_len() as u64 + stats.dropped_overflow,
            4
        );
        // Pending 传输下 worker 永不返回，不能 shutdown().await；运行时结束即回收。
    }

    /// 并发 submit 线程安全：N 个并发提交全部入队（或在途），无丢失、无 panic。
    #[tokio::test]
    async fn concurrent_submit_is_thread_safe() {
        let fake = FakeTransport::new(FakeAction::Pending);
        let reporter = Arc::new(reporter(fake));
        const TASKS: usize = 8;

        let mut handles = Vec::new();
        for i in 0..TASKS {
            let reporter = Arc::clone(&reporter);
            handles.push(tokio::spawn(async move {
                let from = (i * 10 + 1) as u64;
                reporter
                    .submit(&ReceiptBatch::new("M-C", "L", from, from + 9, "d").expect("batch"))
                    .await
                    .expect("concurrent submit must succeed");
            }));
        }
        for handle in handles {
            handle.await.expect("task must not panic");
        }

        let stats = reporter.stats();
        assert_eq!(stats.in_flight, 1, "单 worker，恰有一条在途");
        assert_eq!(stats.dropped_overflow, 0, "容量 64 内不得丢弃");
        assert_eq!(stats.sent_ok, 0, "Pending 传输下不得有成功");
        assert_eq!(
            stats.in_flight + reporter.queue_len() as u64,
            TASKS as u64,
            "总账必须守恒（无丢失）"
        );
    }

    /// 优雅停机：成功路径下 shutdown().await 正常返回，worker 退出。
    #[tokio::test]
    async fn shutdown_worker_completes_cleanly() {
        let fake = FakeTransport::new(FakeAction::Ok(ok_envelope(vec![], "none")));
        let reporter = reporter(fake);
        reporter.submit(&batch()).await.expect("submit ok");
        wait_until(|| reporter.stats().sent_ok == 1, "receipt sent").await;
        assert_eq!(reporter.queue_len(), 0);
        reporter.shutdown().await;
        assert!(reporter.stats().in_flight == 0);
    }
}

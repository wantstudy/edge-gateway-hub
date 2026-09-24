//! 云授权客户端（计划 task 22）：本地授权状态机 + 租约本地验签 + 心跳 / 回执上报。
//!
//! # 职责与红线
//!
//! - **本地授权状态是唯一真相源**：[`LicenseState`] 由本模块（Rust 侧）持有，
//!   任何授权判定（是否允许签名 / 是否允许北向转发）都**只能**读
//!   [`LicensingClient::current_state`] 的返回值——**绝不**把授权判定下沉到
//!   JS / WebView 层。WebView 只做展示。
//! - **离线宽限 7 天**：断网后 [`LicensingClient::tick`] 按**单调推进量**逐日推进；
//!   宽限期耗尽（第 8 天）才降级为 [`LicenseState::Degraded`]。**降级 ≠ 停用**：
//!   降级后仍允许本地采集，只是停北向转发（转发控制在北向层，本模块只报告状态）。
//! - **B 档断网继续采集与转发**：心跳网络失败**不得**把状态打成 `Degraded`，
//!   错误返回给调用方由其决定；状态机只在**宽限期耗尽**时才降级。
//! - **试用 3 天**：`Trial { days_left }` 从 3 递减到 0 后转 `Degraded`（**不转**
//!   `Unlicensed`）。
//! - **系统时间回拨防御**：调用方传入的 `now` 若小于已记录最大时间，
//!   推进量按 0 计（否则把时钟调回去就能无限续期）。
//! - **本地验签用内置公钥集**：[`LicensingClient::verify_lease_locally`] 支持**多个 kid**
//!   （kid 轮换时旧 Token 仍可验），未知 kid / 签名不符 / 已过期 / 结构错误一律 `Err`。
//! - **回执字段白名单**：[`LicensingClient::report_receipt`] 的请求体**只能有** 8 个字段
//!   （`device_mid, lease_id, seq_from, seq_to, count, payload_digest, ts, sig`），
//!   **绝不携带任何业务数值**。
//! - **大整数不走 JSON number**：`seq_from` / `seq_to` / `count` / `ts` 在 JSON 里是
//!   **字符串**（安全整数上限 2^53−1 = 9007199254740991）。
//! - **红线：客户端没有「解绑 / 重置试用 / 换机」入口**——换机只能由厂商后台执行；
//!   本模块**连方法都不提供**。
//!
//! # 网络层设计（现实约束下的取舍）
//!
//! 本 crate 依赖栈为**纯 Rust**（禁止 `native-tls` / `openssl-sys` / 任何需 cmake / NASM
//! 的 C 依赖，见 workspace `Cargo.toml` 注释）。`Cargo.toml` 当前**没有** HTTP 客户端，
//! 且默认特性的 `reqwest` 会拉 `native-tls` → 触发系统 OpenSSL → 本机 mingw 构建失败
//! （项目红线）。
//!
//! 因此本模块把**网络传输抽象为 [`LicenseTransport`] trait**：
//! - 授权状态机、Token 验签、宽限推进、回执构造等**全部纯逻辑**在本模块内实现并被完整测试；
//! - 真实 HTTPS 传输由宿主在后续落地（用 `reqwest(rustls-tls)` 或 `hyper+rustls` 实现本
//!   trait 即可），仅需实现 [`LicenseTransport::post_json`] 一个方法；
//! - 测试用 [`FakeTransport`] 驱动端到端流程（激活成功 / 被拒 / 心跳超时 / 断网）。
//!
//! 这样做的收益：**无新增 C 依赖 → 门禁稳定通过**，且状态机的可测试性不依赖真实网络。

use std::collections::BTreeMap;
use std::fmt;
use std::sync::{Arc, Mutex};

use base64::engine::general_purpose::STANDARD as B64;
use base64::engine::general_purpose::STANDARD_NO_PAD as B64NP;
use base64::Engine as _;
use ed25519_dalek::{Signature, VerifyingKey};
// ed25519-dalek 2.x 的 `SigningKey::sign` 需 `Signer` trait 在作用域；生产路径只验签，仅测试用。
#[cfg(test)]
use ed25519_dalek::Signer as _;
use serde::{Deserialize, Serialize};

use crate::auth::signing::{AuthSigner, HandleSigner, LicenseGate};
use crate::error::{DaemonError, DaemonResult};

// ---- 常量（默认值集中声明，避免散落魔法数字） ----

/// 默认心跳周期（小时）。
pub const DEFAULT_HEARTBEAT_HOURS: u64 = 24;
/// 默认连接超时（秒）。
pub const DEFAULT_CONNECT_TIMEOUT_SECS: u64 = 10;
/// 默认请求超时（秒）。
pub const DEFAULT_REQUEST_TIMEOUT_SECS: u64 = 30;
/// 试用天数（3 天）。
pub const TRIAL_DAYS: i64 = 3;
/// 离线宽限天数（7 天）。
pub const GRACE_DAYS: i64 = 7;
/// 一天秒数。
const SECS_PER_DAY: i64 = 86_400;
/// JSON 安全整数上限（2^53 − 1）：超过此值不得以 JSON number 传输。
pub const JSON_SAFE_INT_MAX: i64 = 9_007_199_254_740_991;
/// Lease Token 签名域（与 licensing-server 的 `LEASE_SIGNING_DOMAIN` 一致）。
pub const LEASE_SIGNING_DOMAIN: &str = "iotdaq.lease.v1";
/// 回执签名域（确定性、无 nonce；服务端从回执字段重建此串后验签）。
///
/// 与 `LEASE_SIGNING_DOMAIN` 同风格：**不带**尾随 `|`（分隔符由 `push_len_field` 前置），
/// 渲染结果为 `iotdaq.receipt.v1|mid=...|...|ts=...`。
pub const RECEIPT_SIGNING_DOMAIN: &str = "iotdaq.receipt.v1";
/// Ed25519 公钥原始字节长度。
const ED25519_PUBLIC_KEY_LEN: usize = 32;
/// Ed25519 签名原始字节长度。
const ED25519_SIGNATURE_LEN: usize = 64;
/// 回执请求体的**字段白名单**（恰好 8 个，顺序固定）。
pub const RECEIPT_FIELD_WHITELIST: [&str; 8] = [
    "device_mid",
    "lease_id",
    "seq_from",
    "seq_to",
    "count",
    "payload_digest",
    "ts",
    "sig",
];

// ---- 授权服务端点配置 ----

/// 授权服务端点配置（从 `config` 注入，**禁止硬编码 URL / 激活码**）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LicensingClientConfig {
    /// 授权服务基址，如 `https://licensing.vendor.com/v1`。
    pub base_url: String,
    /// 心跳周期（小时，默认 24）。
    pub heartbeat_hours: u64,
    /// 连接超时（秒，默认 10）。
    pub connect_timeout_secs: u64,
    /// 请求超时（秒，默认 30）。
    pub request_timeout_secs: u64,
}

impl Default for LicensingClientConfig {
    fn default() -> Self {
        Self {
            base_url: String::new(),
            heartbeat_hours: DEFAULT_HEARTBEAT_HOURS,
            connect_timeout_secs: DEFAULT_CONNECT_TIMEOUT_SECS,
            request_timeout_secs: DEFAULT_REQUEST_TIMEOUT_SECS,
        }
    }
}

impl LicensingClientConfig {
    /// 构造配置并做基本校验（基址非空）。
    ///
    /// # Errors
    /// `base_url` 空白时返回 [`DaemonError::ConfigError`]（码 2000）。
    pub fn new(
        base_url: impl Into<String>,
        heartbeat_hours: u64,
        connect_timeout_secs: u64,
        request_timeout_secs: u64,
    ) -> DaemonResult<Self> {
        let url = base_url.into().trim().to_string();
        if url.is_empty() {
            return Err(DaemonError::ConfigError(
                "licensing base_url must not be empty".to_string(),
            ));
        }
        Ok(Self {
            base_url: url,
            heartbeat_hours,
            connect_timeout_secs,
            request_timeout_secs,
        })
    }

    /// 心跳周期（秒）。
    pub fn heartbeat_secs(&self) -> i64 {
        (self.heartbeat_hours as i64).saturating_mul(3_600)
    }

    /// 拼接端点 URL（`base_url + path`，去重斜杠）。
    fn endpoint(&self, path: &str) -> String {
        let base = self.base_url.trim_end_matches('/');
        let path = path.trim_start_matches('/');
        format!("{base}/{path}")
    }
}

// ---- 二次校验档位 ----

/// 二次校验档位（A / B / C，**随 Token 下发，客户端无切换接口**）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VerifyMode {
    /// A 档：严格在线校验。
    A,
    /// B 档：允许离线采集与转发，断网时延迟补报回执。
    B,
    /// C 档：宽松校验。
    C,
}

impl VerifyMode {
    /// 从 Token 载荷字符串解析（非法值 → `None`）。
    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "A" => Some(VerifyMode::A),
            "B" => Some(VerifyMode::B),
            "C" => Some(VerifyMode::C),
            _ => None,
        }
    }

    /// 稳定字符串（与 licensing-server 的 `verify_mode` 字段一致）。
    pub fn as_str(self) -> &'static str {
        match self {
            VerifyMode::A => "A",
            VerifyMode::B => "B",
            VerifyMode::C => "C",
        }
    }

    /// 是否允许离线采集与转发（B / C 档为真；A 档要求在线）。
    pub fn allows_offline(self) -> bool {
        matches!(self, VerifyMode::B | VerifyMode::C)
    }
}

impl fmt::Display for VerifyMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

// ---- 云端租约 ----

/// 云端返回的租约（字段与 licensing-server 的 `LeaseClaims` 对齐）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LeaseToken {
    /// 原始三段式串 `kid.payload.sig`（保留原文，供审计与二次验签）。
    pub raw: String,
    /// 租约 ID（服务端主键）。
    pub lease_id: String,
    /// 设备 ID。
    pub device_id: String,
    /// 签名所用密钥标识。
    pub kid: String,
    /// 授权档位（如 `standard` / `pro` / `trial`）。
    pub tier: String,
    /// 二次校验档位。
    pub verify_mode: VerifyMode,
    /// 失效时刻（UTC 秒）。
    pub valid_until: i64,
}

impl LeaseToken {
    /// 是否已过期（`now >= valid_until` 即过期）。
    pub fn is_expired_at(&self, now_unix_secs: i64) -> bool {
        now_unix_secs >= self.valid_until
    }

    /// 剩余有效期秒数（已过期钳制为 0）。
    pub fn remaining_secs(&self, now_unix_secs: i64) -> i64 {
        (self.valid_until - now_unix_secs).max(0)
    }
}

/// Token 载荷（对客户端可见，**不含任何私密信息**）。
///
/// 与 licensing-server 的 `LeaseClaims` **逐字段对齐**（含 `issued_at` / `mid`，
/// 这两个字段虽不在 [`LeaseToken`] 上暴露，但**必须参与签名域渲染**，否则验签失败）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct LeaseClaims {
    /// 租约 ID。
    lease_id: String,
    /// 设备 ID。
    device_id: String,
    /// 机器码指纹。
    mid: String,
    /// 授权档位。
    tier: String,
    /// 二次校验档位（A / B / C）。
    verify_mode: String,
    /// 签发时刻（UTC 秒）。
    issued_at: i64,
    /// 失效时刻（UTC 秒）。
    valid_until: i64,
}

// ---- 本地授权状态 ----

/// 本地授权状态（**授权判定的唯一真相源，必须在 Rust 侧**）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LicenseState {
    /// 未激活。
    Unlicensed,
    /// 试用中（默认 3 天）。
    Trial {
        /// 剩余天数。
        days_left: i64,
    },
    /// 已激活（持有有效租约）。
    Licensed {
        /// 当前租约。
        lease: LeaseToken,
    },
    /// 离线宽限（默认 7 天）。
    Grace {
        /// 仍持有的最后租约。
        lease: LeaseToken,
        /// 宽限剩余天数。
        days_left: i64,
    },
    /// 降级（免费基础版）：**仍允许本地采集**，仅停北向转发。
    Degraded {
        /// 降级原因。
        reason: String,
    },
}

impl LicenseState {
    /// 是否允许签名（对应 [`LicenseGate`] 语义：Token 有效且未降级 / 未过期）。
    ///
    /// 规则：`Licensed` 允许；`Trial` / `Grace` 允许（宽限期内仍算有效）；
    /// `Degraded` / `Unlicensed` 不允许。
    pub fn can_sign(&self) -> bool {
        matches!(
            self,
            LicenseState::Licensed { .. } | LicenseState::Trial { .. } | LicenseState::Grace { .. }
        )
    }

    /// 是否允许本地采集。
    ///
    /// **降级 ≠ 停用**：任何状态下都允许本地采集，本方法恒为 `true`（语义显式化，
    /// 供上层读取以免误判）。
    pub fn allows_local_capture(&self) -> bool {
        true
    }

    /// 是否允许北向转发。
    ///
    /// `Degraded` / `Unlicensed` 停转发；`Trial` / `Licensed` / `Grace` 允许。
    pub fn allows_northbound_forward(&self) -> bool {
        matches!(
            self,
            LicenseState::Licensed { .. } | LicenseState::Trial { .. } | LicenseState::Grace { .. }
        )
    }

    /// 状态名（稳定字符串，供日志与断言）。
    pub fn name(&self) -> &'static str {
        match self {
            LicenseState::Unlicensed => "Unlicensed",
            LicenseState::Trial { .. } => "Trial",
            LicenseState::Licensed { .. } => "Licensed",
            LicenseState::Grace { .. } => "Grace",
            LicenseState::Degraded { .. } => "Degraded",
        }
    }
}

// ---- 网络传输抽象（可注入） ----

/// 授权服务的网络传输抽象。
///
/// 真实实现（宿主提供）用 HTTPS 打到授权服务；本模块只依赖**这一层**，
/// 因此状态机与协议逻辑完全可测（用 [`FakeTransport`]）。
///
/// 契约：`post_json` 发送 JSON，成功（HTTP 2xx）返回响应体 JSON 文本；
/// 传输失败 / 超时 / 非 2xx 返回 [`DaemonError::NetworkError`]（码 6000）。
pub trait LicenseTransport: Send + Sync {
    /// POST 一个 JSON 请求体，返回响应体 JSON 文本。
    ///
    /// # Errors
    /// 连接失败 / 超时 → [`DaemonError::NetworkError`]；非 2xx → [`DaemonError::AuthError`]
    /// （服务端明确拒绝）或 `NetworkError`（可重试的传输/服务端错误）。
    fn post_json(
        &self,
        url: &str,
        body: &serde_json::Value,
    ) -> Result<serde_json::Value, DaemonError>;
}

// ---- 客户端 ----

/// 授权状态机的内部可变数据（受 `Mutex` 保护，保证跨线程一致）。
struct ClientInner {
    /// 当前授权状态（**唯一真相源**）。
    state: LicenseState,
    /// 已观察到的最大时间（UTC 秒）——用于系统时间回拨防御。
    ///
    /// 单调推进量 = `max(0, now - max_observed)`；`now < max_observed` 时推进量为 0。
    max_observed_secs: i64,
    /// 宽限推进的**基准时刻**（进入 `Grace` 或最近一次「已联网」的时刻）。
    grace_anchor_secs: i64,
    /// 试用推进的**基准时刻**（激活试用 / 进入试用态的时刻）。
    trial_anchor_secs: i64,
    /// 试用起点是否为「显式激活试用」。
    trial_active: bool,
    /// 最近一次心跳携带的回执游标（`(seq_from, seq_to)`）。
    last_cursor: Option<(i64, i64)>,
}

/// 云授权客户端。
///
/// 持有状态机、签名器（复用 [`AuthSigner`] 的 `semantic_hash` 语义）、机器码指纹与
/// 内置公钥集；所有网络交互经注入的 [`LicenseTransport`]。
pub struct LicensingClient {
    /// 端点与超时配置。
    cfg: LicensingClientConfig,
    /// 复用 task 21 的签名器（用于请求签名域与 `mid`）。
    signer: Arc<AuthSigner>,
    /// 本机机器码指纹（mid）。
    machine_code: String,
    /// 网络传输实现（生产 HTTPS / 测试 fake）。
    transport: Arc<dyn LicenseTransport>,
    /// 回执签名密钥提供者（**可选**；未注入时回执签名置空 → 服务端明确拒绝）。
    ///
    /// 与 [`AuthSigner`] 使用**同一把设备私钥**（生产由宿主注入同一个 `HandleSigner`）；
    /// 之所以单独持有：回执签名必须是**确定性、可被服务端用设备公钥重建验签**的签名，
    /// 而 [`AuthSigner::sign_semantic`] 会掺入随机 nonce（服务端无法重建），故不经其签名。
    receipt_signer: Option<Arc<dyn HandleSigner>>,
    /// 内置公钥集：`kid → Ed25519 公钥`（支持多 kid 轮换）。
    public_keys: BTreeMap<String, VerifyingKey>,
    /// 状态机内部数据。
    inner: Mutex<ClientInner>,
}

impl fmt::Debug for LicensingClient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let state = self
            .inner
            .lock()
            .map(|g| g.state.name().to_string())
            .unwrap_or_else(|_| "<poisoned>".to_string());
        f.debug_struct("LicensingClient")
            .field("base_url", &self.cfg.base_url)
            .field("machine_code", &self.machine_code)
            .field("kid_count", &self.public_keys.len())
            .field("state", &state)
            .finish()
    }
}

impl LicensingClient {
    /// 构造客户端（激活前状态为 [`LicenseState::Unlicensed`]）。
    ///
    /// 网络传输由 [`LicensingClient::with_transport`] 注入；本构造器使用
    /// [`UnavailableTransport`]（一切请求返回 `NetworkError`），便于「纯本地逻辑」
    /// 场景下先构造、后换 transport。
    pub fn new(cfg: LicensingClientConfig, signer: Arc<AuthSigner>, machine_code: String) -> Self {
        Self::with_transport(cfg, signer, machine_code, Arc::new(UnavailableTransport))
    }

    /// 构造客户端并注入网络传输实现。
    pub fn with_transport(
        cfg: LicensingClientConfig,
        signer: Arc<AuthSigner>,
        machine_code: String,
        transport: Arc<dyn LicenseTransport>,
    ) -> Self {
        Self {
            cfg,
            signer,
            machine_code,
            transport,
            receipt_signer: None,
            public_keys: BTreeMap::new(),
            inner: Mutex::new(ClientInner {
                state: LicenseState::Unlicensed,
                max_observed_secs: i64::MIN,
                grace_anchor_secs: 0,
                trial_anchor_secs: 0,
                trial_active: false,
                last_cursor: None,
            }),
        }
    }

    /// **增量、可选**：注入回执签名密钥（与 [`AuthSigner`] 用同一把设备私钥）。
    ///
    /// 未注入时 [`LicensingClient::report_receipt`] 产出的 `sig` 为空串——这是
    /// **明确的失败信号**（服务端收到空签名会拒绝），绝非「无签名冒充有效签名」。
    /// 注入后回执签名是**确定性**的：服务端可用设备公钥从回执字段独立重建并验签。
    ///
    /// 该方法是**新增**的（不改动 [`LicensingClient::new`] / [`LicensingClient::with_transport`]
    /// 签名，避免破坏既有调用方）。
    pub fn with_receipt_signer(mut self, provider: Arc<dyn HandleSigner>) -> Self {
        self.receipt_signer = Some(provider);
        self
    }

    /// 注册一个内置公钥（`kid → Ed25519 32 字节公钥`）。
    ///
    /// 支持多次调用注册**多个 kid**（轮换时新旧并存）。公钥字节长度非法 → `ConfigError`。
    ///
    /// # Errors
    /// `kid` 为空或公钥字节长度 ≠ 32 → [`DaemonError::ConfigError`]（码 2000）。
    pub fn register_public_key(&mut self, kid: &str, public_key: &[u8]) -> DaemonResult<()> {
        let kid = kid.trim().to_string();
        if kid.is_empty() {
            return Err(DaemonError::ConfigError(
                "lease public key kid must not be empty".to_string(),
            ));
        }
        let bytes: [u8; ED25519_PUBLIC_KEY_LEN] = public_key.try_into().map_err(|_| {
            DaemonError::ConfigError(format!(
                "ed25519 public key must be {ED25519_PUBLIC_KEY_LEN} bytes, got {}",
                public_key.len()
            ))
        })?;
        let key = VerifyingKey::from_bytes(&bytes).map_err(|e| {
            DaemonError::ConfigError(format!("invalid ed25519 public key for kid {kid}: {e}"))
        })?;
        self.public_keys.insert(kid, key);
        Ok(())
    }

    /// 内置公钥集中已登记的 kid 列表（供诊断 / 测试）。
    pub fn known_kids(&self) -> Vec<String> {
        self.public_keys.keys().cloned().collect()
    }

    /// 端点配置（只读）。
    pub fn config(&self) -> &LicensingClientConfig {
        &self.cfg
    }

    /// 当前本机机器码指纹。
    pub fn machine_code(&self) -> &str {
        &self.machine_code
    }

    /// 与本客户端绑定的 AuthBlock 签名器（task 21）。
    ///
    /// 供北向转发层复用**同一个** `Arc<AuthSigner>`（`mid` / 授权闸门一致，单一真相源）；
    /// 回执签名使用与之一致的设备私钥（经 [`LicensingClient::with_receipt_signer`] 注入）。
    pub fn signer(&self) -> &Arc<AuthSigner> {
        &self.signer
    }

    /// 当前状态快照（无锁竞争时返回克隆；锁中毒时退化为 `Unlicensed`，不 panic）。
    fn snapshot(&self) -> LicenseState {
        self.inner
            .lock()
            .map(|g| g.state.clone())
            .unwrap_or(LicenseState::Unlicensed)
    }

    /// 原子更新内部状态。
    fn set_state(&self, next: LicenseState) {
        if let Ok(mut guard) = self.inner.lock() {
            guard.state = next;
        }
    }

    // ---- 激活 ----

    /// 激活（**必须联网**；离线激活不允许）。
    ///
    /// 请求体：`{ device_mid, machine_code, activation_code, ts }`；
    /// 响应体须含 `lease_token` 字段（三段式串）。成功即本地验签 → 写入 `Licensed`。
    ///
    /// # Errors
    /// - 网络失败 / 超时 → `NetworkError`；
    /// - 服务端拒绝 → `AuthError`；
    /// - 响应缺 `lease_token` / 本地验签失败 → `AuthError` / `SecurityError`。
    pub async fn activate(&self, activation_code: &str) -> DaemonResult<LicenseState> {
        let code = activation_code.trim();
        if code.is_empty() {
            return Err(DaemonError::ConfigError(
                "activation code must not be empty".to_string(),
            ));
        }

        let url = self.cfg.endpoint("activate");
        let now = self.observed_now();
        let body = serde_json::json!({
            "device_mid": self.machine_code,
            "machine_code": self.machine_code,
            "activation_code": code,
            "ts": now.to_string(),
        });

        let resp = self.transport.post_json(&url, &body)?;
        let raw = resp
            .get("lease_token")
            .and_then(|v| v.as_str())
            .ok_or_else(|| {
                DaemonError::AuthError(
                    "activate response is missing 'lease_token' field".to_string(),
                )
            })?;

        let lease = self.verify_lease_locally(raw)?;
        self.set_state(LicenseState::Licensed {
            lease: lease.clone(),
        });
        Ok(LicenseState::Licensed { lease })
    }

    /// 24h 心跳。B 档必须携带最近回执序号区间（`cursor = Some((seq_from, seq_to))`）。
    ///
    /// **关键**：网络失败**不得**降级——错误直接返回给调用方；状态机只在
    /// **宽限期耗尽**时才降级。B 档断网时由调用方继续采集与转发，稍后补报。
    ///
    /// # Errors
    /// 网络失败 / 超时 → `NetworkError`；服务端拒绝 → `AuthError`；验签失败 → `SecurityError`。
    pub async fn heartbeat(&self, cursor: Option<(i64, i64)>) -> DaemonResult<LicenseState> {
        let url = self.cfg.endpoint("heartbeat");
        let now = self.observed_now();

        let mut body = serde_json::json!({
            "device_mid": self.machine_code,
            "machine_code": self.machine_code,
            "ts": now.to_string(),
        });
        // B 档必须携带最近回执序号区间（大整数走字符串）。
        if let Some((from, to)) = cursor {
            let obj = body
                .as_object_mut()
                .ok_or_else(|| DaemonError::AuthError("heartbeat body is not an object".into()))?;
            obj.insert(
                "cursor_from".to_string(),
                serde_json::json!(from.to_string()),
            );
            obj.insert("cursor_to".to_string(), serde_json::json!(to.to_string()));
        }

        let resp = self.transport.post_json(&url, &body)?;
        let raw = resp
            .get("lease_token")
            .and_then(|v| v.as_str())
            .ok_or_else(|| {
                DaemonError::AuthError(
                    "heartbeat response is missing 'lease_token' field".to_string(),
                )
            })?;

        let lease = self.verify_lease_locally(raw)?;

        // 心跳成功 → 视为「已联网」：刷新宽限基准，回到 Licensed。
        if let Ok(mut guard) = self.inner.lock() {
            guard.grace_anchor_secs = now;
            guard.last_cursor = cursor;
            guard.state = LicenseState::Licensed {
                lease: lease.clone(),
            };
        }
        Ok(LicenseState::Licensed { lease })
    }

    // ---- 回执上报 ----

    /// B 档审计回执上报（断网可延迟补报）。
    ///
    /// 请求体**严格白名单**：恰好 8 个字段
    /// （`device_mid, lease_id, seq_from, seq_to, count, payload_digest, ts, sig`），
    /// **绝不携带业务数值**。其中 `seq_from` / `seq_to` / `count` / `ts` 一律以
    /// **字符串**编码（大整数不走 JSON number，安全整数上限 2^53−1）。
    /// `sig` = 对请求体语义内容的 Ed25519 签名（复用 [`AuthSigner::sign_semantic`]，
    /// **绝不**另行发明一套签名）。
    ///
    /// # Errors
    /// 网络失败 → `NetworkError`；未激活（无租约）→ `AuthError`；计数非法 → `ConfigError`。
    pub async fn report_receipt(
        &self,
        seq_from: i64,
        seq_to: i64,
        count: i64,
        payload_digest: &str,
    ) -> DaemonResult<()> {
        if count < 0 {
            return Err(DaemonError::ConfigError(
                "receipt count must not be negative".to_string(),
            ));
        }
        let lease_id = match self.snapshot() {
            LicenseState::Licensed { lease } | LicenseState::Grace { lease, .. } => lease.lease_id,
            LicenseState::Trial { .. } => {
                return Err(DaemonError::AuthError(
                    "receipt reporting requires an active lease (trial has none)".to_string(),
                ));
            }
            _ => {
                return Err(DaemonError::AuthError(
                    "receipt reporting requires an activated lease".to_string(),
                ));
            }
        };

        let now = self.observed_now();
        // 请求签名：先算语义哈希（域分隔 + 长度前缀，复用 task 21 口径），再签名。
        let sig = self.sign_receipt(&lease_id, seq_from, seq_to, count, payload_digest, now);

        let body = serde_json::json!({
            "device_mid": self.machine_code,
            "lease_id": lease_id,
            "seq_from": seq_from.to_string(),
            "seq_to": seq_to.to_string(),
            "count": count.to_string(),
            "payload_digest": payload_digest,
            "ts": now.to_string(),
            "sig": sig,
        });

        // 白名单守护：构造出的请求体 key 集合必须恰好等于白名单。
        // 注：此断言只比长度（`debug_assert` 在 release 被裁掉），是「廉价前置闸门」；
        // 真正的内容级守护由测试 `receipt_signature_verifies_against_its_public_key`
        // （真实验签）与 `receipt_body_has_exactly_whitelisted_keys`（key 集合精确相等）承担。
        debug_assert_eq!(object_keys(&body).len(), RECEIPT_FIELD_WHITELIST.len());

        let url = self.cfg.endpoint("receipt");
        let _ = self.transport.post_json(&url, &body)?;
        if let Ok(mut guard) = self.inner.lock() {
            guard.last_cursor = Some((seq_from, seq_to));
        }
        Ok(())
    }

    /// 构造回执请求签名（**确定性** Ed25519 签名，服务端可用设备公钥重建验签）。
    ///
    /// 签名对象是**确定性规范化串**（非序列化字节，非含随机 nonce 的 AuthBlock 消息）：
    ///
    /// ```text
    /// iotdaq.receipt.v1|mid=<len>:<mid>|lease_id=<len>:<lease_id>|seq_from=<len>:<n>|
    ///                   seq_to=<len>:<n>|count=<len>:<n>|payload_digest=<len>:<s>|ts=<len>:<ts>
    /// ```
    ///
    /// 然后对该串的 SHA-256（`semantic_digest`）做 Ed25519 原始签名，输出 STANDARD base64。
    /// 服务端从回执请求体的 7 个字段（mid/lease_id/seq_from/seq_to/count/payload_digest/ts）
    /// 独立重建同一串并验签，**因此签名可被第三方校验**（这是 B 档二次校验的信任基础）。
    ///
    /// 私钥不落盘、不日志（由 [`HandleSigner`] 保证）；密钥不可用时返回**空串**——这是明确的
    /// 失败信号（服务端拒绝），**绝不**用「无签名」冒充有效签名；生产路径零 panic。
    fn sign_receipt(
        &self,
        lease_id: &str,
        seq_from: i64,
        seq_to: i64,
        count: i64,
        payload_digest: &str,
        ts: i64,
    ) -> String {
        let msg = receipt_signing_message(
            &self.machine_code,
            lease_id,
            seq_from,
            seq_to,
            count,
            payload_digest,
            ts,
        );
        let payload_hash = semantic_digest(msg.as_bytes());

        let provider = match &self.receipt_signer {
            Some(p) => p,
            // 未注入回执签名密钥：置空（明确失败信号），不伪造签名、不 panic。
            None => return String::new(),
        };
        let signature: Signature = match provider.sign_message(&payload_hash) {
            Ok(s) => s,
            // 密钥不可用：置空（明确失败信号）。
            Err(_) => return String::new(),
        };
        B64.encode(signature.to_bytes())
    }

    // ---- 本地验签 ----

    /// 本地验签（**内置公钥集**，非单钥）。
    ///
    /// 校验链（顺序即短路顺序）：
    /// 1. 结构解析（三段式 `kid.payload.sig`，base64 + JSON）；
    /// 2. 载荷业务校验（字段非空 / `verify_mode` 合法 / 时间区间合法）；
    /// 3. **按 kid 查内置公钥集**（未知 kid → 拒绝，不做单钥兜底）；
    /// 4. Ed25519 验签（签名域含 kid，改 kid 必然验签失败）；
    /// 5. 过期判定（`now >= valid_until` → 拒绝）。
    ///
    /// 支持多个 kid（轮换时旧 Token 仍可验）。
    ///
    /// # Errors
    /// - 结构错误 / 非法 base64 / 非 JSON → [`DaemonError::SecurityError`]（码 7000）；
    /// - 未知 kid → [`DaemonError::SecurityError`]；
    /// - 签名不符 → [`DaemonError::SecurityError`]；
    /// - 已过期 → [`DaemonError::AuthError`]（码 3000）。
    pub fn verify_lease_locally(&self, raw: &str) -> DaemonResult<LeaseToken> {
        let (kid, claims, sig_b64) = decode_lease(raw)?;

        // 载荷业务校验。
        validate_claims(&claims)?;

        // 3. kid 查表（未知 kid 拒绝）。
        let key = self.public_keys.get(&kid).ok_or_else(|| {
            DaemonError::SecurityError(format!("lease token uses unknown kid: {kid}"))
        })?;

        // 4. 验签。
        let message = render_signing_message(&kid, &claims);
        verify_ed25519(key, &message, &sig_b64)?;

        // 5. 过期判定。
        let now = self.observed_now();
        if claims.valid_until <= now {
            return Err(DaemonError::AuthError(format!(
                "lease token expired at {} (now {now})",
                claims.valid_until
            )));
        }

        let verify_mode = VerifyMode::parse(&claims.verify_mode).ok_or_else(|| {
            DaemonError::SecurityError(format!(
                "lease token verify_mode must be A|B|C, got {}",
                claims.verify_mode
            ))
        })?;

        Ok(LeaseToken {
            raw: raw.to_string(),
            lease_id: claims.lease_id,
            device_id: claims.device_id,
            kid,
            tier: claims.tier,
            verify_mode,
            valid_until: claims.valid_until,
        })
    }

    // ---- 状态读取 ----

    /// **本方法的返回值是唯一允许用于授权判定的依据**。
    ///
    /// 任何授权判定（能否签名 / 能否北向转发）都只能读这里返回的 [`LicenseState`]。
    pub fn current_state(&self) -> LicenseState {
        self.snapshot()
    }

    // ---- 时钟推进 ----

    /// 当前「已观察到」的时间（用于请求 ts / 过期判定）。
    ///
    /// 单调：返回 `max(记录的最大时间, 系统时钟)`。**不信任系统时间回拨**——
    /// 但本方法只用于生成请求时间戳；真正的**回拨防御在 [`LicensingClient::tick`]**。
    fn observed_now(&self) -> i64 {
        let sys = now_unix_secs();
        match self.inner.lock() {
            Ok(guard) => guard.max_observed_secs.max(sys),
            Err(_) => sys,
        }
    }

    /// 离线宽限推进（**按单调时钟，不信任系统时间回拨**）。
    ///
    /// 语义：
    /// - 用「推进量 = `max(0, now - max_observed)`」推进，`now < max_observed` 时推进量为 0；
    /// - `Licensed` → 超过租约有效期后进入 `Grace`（宽限 7 天）；
    /// - `Grace` → 宽限期（7 天）耗尽（第 8 天）转 `Degraded`；
    /// - `Trial` → `days_left` 从 3 递减到 0 后转 `Degraded`（**不转** `Unlicensed`）；
    /// - 其它状态原样返回。
    ///
    /// 返回推进后的状态（同时更新内部状态）。
    pub fn tick(&self, now_unix_secs: i64) -> LicenseState {
        let mut guard = match self.inner.lock() {
            Ok(g) => g,
            Err(_) => return LicenseState::Unlicensed,
        };

        // **单调时钟**：`max_observed_secs` 只增不减。时间回拨时它保持不变，
        // 因此推进量自然为 0——这正是「时钟调回去不能续期」的实现基础。
        if now_unix_secs > guard.max_observed_secs {
            guard.max_observed_secs = now_unix_secs;
        }
        let now = guard.max_observed_secs;

        let next = match &guard.state {
            LicenseState::Licensed { lease } => {
                let lease = lease.clone();
                // 若租约已过期（按单调时刻判定），进入宽限（锚点 = 当前单调时刻）。
                if lease.is_expired_at(now) {
                    guard.grace_anchor_secs = now;
                    LicenseState::Grace {
                        lease,
                        days_left: GRACE_DAYS,
                    }
                } else {
                    LicenseState::Licensed { lease }
                }
            }
            LicenseState::Grace { lease, .. } => {
                let lease = lease.clone();
                let elapsed_days = now.saturating_sub(guard.grace_anchor_secs) / SECS_PER_DAY;
                let days_left = GRACE_DAYS - elapsed_days;
                // 宽限耗尽判定：`days_left == 0` 仍属宽限（第 7 天），
                // 只有**超过** 7 天（`days_left < 0`，即第 8 天起）才降级。
                if days_left < 0 {
                    LicenseState::Degraded {
                        reason: "offline grace period (7 days) exhausted".to_string(),
                    }
                } else {
                    LicenseState::Grace { lease, days_left }
                }
            }
            LicenseState::Trial { .. } => {
                if !guard.trial_active {
                    // 未显式进入试用：保持原状态（避免误推进）。
                    guard.state.clone()
                } else {
                    // 试用的剩余天数按**单调时刻**与锚点之差累计（回拨不续期）。
                    let elapsed_days = now.saturating_sub(guard.trial_anchor_secs) / SECS_PER_DAY;
                    let days_left = TRIAL_DAYS - elapsed_days;
                    // 试用耗尽判定：`days_left == 0` 仍是试用最后时刻，
                    // 只有**超过** 3 天（`days_left < 0`）才降级；降级**不转** `Unlicensed`。
                    if days_left < 0 {
                        LicenseState::Degraded {
                            reason: "trial period (3 days) exhausted".to_string(),
                        }
                    } else {
                        LicenseState::Trial { days_left }
                    }
                }
            }
            other => other.clone(),
        };

        guard.state = next.clone();
        next
    }

    /// 进入试用态（3 天）。**仅供受控激活流程调用**；不提供任何「重置试用」入口。
    ///
    /// 这是首次启动 / 无租约时的显式试用起点：`trial_anchor_secs = now`。
    pub fn begin_trial(&self, now_unix_secs: i64) -> LicenseState {
        if let Ok(mut guard) = self.inner.lock() {
            guard.max_observed_secs = guard.max_observed_secs.max(now_unix_secs);
            guard.trial_anchor_secs = guard.max_observed_secs;
            guard.trial_active = true;
            guard.state = LicenseState::Trial {
                days_left: TRIAL_DAYS,
            };
            return guard.state.clone();
        }
        LicenseState::Unlicensed
    }

    /// 最近一次上报 / 心跳携带的回执游标（供诊断）。
    pub fn last_cursor(&self) -> Option<(i64, i64)> {
        self.inner.lock().ok().and_then(|g| g.last_cursor)
    }
}

/// 让 [`LicensingClient`] 直接充当 task 21 的 [`LicenseGate`]（授权判定单一真相源）。
///
/// 这样 [`AuthSigner`] 的「闸门关闭拒绝签名」契约与本地授权状态天然一致。
impl LicenseGate for LicensingClient {
    fn can_sign(&self) -> bool {
        self.current_state().can_sign()
    }
}

// ---- 不可用传输（默认 / 离线逻辑场景） ----

/// 默认传输：一切请求返回 [`DaemonError::NetworkError`]。
///
/// 用于「先构造客户端做纯本地逻辑、稍后注入真实传输」的场景；
/// 也保证 [`LicensingClient::new`] 在无网络实现时不 panic。
#[derive(Debug, Default)]
pub struct UnavailableTransport;

impl LicenseTransport for UnavailableTransport {
    fn post_json(
        &self,
        url: &str,
        _body: &serde_json::Value,
    ) -> Result<serde_json::Value, DaemonError> {
        Err(DaemonError::NetworkError(format!(
            "no license transport installed (would POST {url})"
        )))
    }
}

// ---- 辅助函数（纯逻辑，零 panic） ----

/// 当前系统时间（Unix 秒）。
///
/// 系统时钟早于 Unix 纪元（理论不可能）时返回 0，而非 panic。
fn now_unix_secs() -> i64 {
    match std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH) {
        Ok(d) => d.as_secs().min(i64::MAX as u64) as i64,
        Err(_) => 0,
    }
}

/// 写入长度前缀字段：`|name=<len>:<value>`（与 licensing-server 签名域一致）。
fn push_len_field(out: &mut String, name: &str, value: &str) {
    out.push('|');
    out.push_str(name);
    out.push('=');
    out.push_str(&value.len().to_string());
    out.push(':');
    out.push_str(value);
}

/// 对语义规范化串做 SHA-256（与 `signing::semantic_hash` 同族，不签序列化字节）。
fn semantic_digest(bytes: &[u8]) -> [u8; 32] {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(b"iotdaq.receipt.semantic.v1|");
    hasher.update(bytes);
    let digest = hasher.finalize();
    let mut out = [0u8; 32];
    out.copy_from_slice(&digest);
    out
}

/// 渲染回执签名消息（**确定性、无 nonce**；服务端可用同一函数从回执字段重建）。
///
/// 格式（逐字段长度前缀，防拼接歧义）：
/// ```text
/// iotdaq.receipt.v1|mid=<len>:<mid>|lease_id=<len>:<lease_id>|seq_from=<len>:<n>|
///                   seq_to=<len>:<n>|count=<len>:<n>|payload_digest=<len>:<s>|ts=<len>:<ts>
/// ```
///
/// 覆盖回执请求体的**全部 7 个业务字段**（`device_mid`/`lease_id`/`seq_from`/`seq_to`/
/// `count`/`payload_digest`/`ts`）——任何字段被中间人篡改都会破坏签名。
fn receipt_signing_message(
    mid: &str,
    lease_id: &str,
    seq_from: i64,
    seq_to: i64,
    count: i64,
    payload_digest: &str,
    ts: i64,
) -> String {
    let mut msg = String::with_capacity(192);
    msg.push_str(RECEIPT_SIGNING_DOMAIN);
    push_len_field(&mut msg, "mid", mid);
    push_len_field(&mut msg, "lease_id", lease_id);
    push_len_field(&mut msg, "seq_from", &seq_from.to_string());
    push_len_field(&mut msg, "seq_to", &seq_to.to_string());
    push_len_field(&mut msg, "count", &count.to_string());
    push_len_field(&mut msg, "payload_digest", payload_digest);
    push_len_field(&mut msg, "ts", &ts.to_string());
    msg
}

/// 渲染 Lease Token 签名域（与 licensing-server 的 `render_signing_message` **逐字节一致**）。
///
/// 格式：`iotdaq.lease.v1|kid=<len>:<kid>|lease_id=...|...|valid_until=<len>:<ts>`
fn render_signing_message(kid: &str, claims: &LeaseClaims) -> Vec<u8> {
    let mut out = String::with_capacity(256);
    out.push_str(LEASE_SIGNING_DOMAIN);
    push_len_field(&mut out, "kid", kid);
    push_len_field(&mut out, "lease_id", &claims.lease_id);
    push_len_field(&mut out, "device_id", &claims.device_id);
    push_len_field(&mut out, "mid", &claims.mid);
    push_len_field(&mut out, "tier", &claims.tier);
    push_len_field(&mut out, "verify_mode", &claims.verify_mode);
    push_len_field(&mut out, "issued_at", &claims.issued_at.to_string());
    push_len_field(&mut out, "valid_until", &claims.valid_until.to_string());
    out.into_bytes()
}

/// 解码三段式 Lease Token：`kid.base64url(payload_json).base64(sig)`。
///
/// 返回 `(kid, claims, signature_b64)`；`kid` 是**未经验证**的声明值，仅用于定位公钥。
///
/// # Errors
/// 段数不对 / 空段 / base64 非法 / 非 JSON → [`DaemonError::SecurityError`]。
fn decode_lease(raw: &str) -> DaemonResult<(String, LeaseClaims, String)> {
    let mut parts = raw.split('.');
    let (kid_seg, payload_b64, sig_b64) = match (parts.next(), parts.next(), parts.next()) {
        (Some(a), Some(b), Some(c)) => (a, b, c),
        _ => {
            return Err(DaemonError::SecurityError(
                "lease token must be 'kid.payload.signature'".to_string(),
            ));
        }
    };
    if parts.next().is_some() {
        return Err(DaemonError::SecurityError(
            "lease token has too many '.'-separated segments".to_string(),
        ));
    }
    if kid_seg.is_empty() || payload_b64.is_empty() || sig_b64.is_empty() {
        return Err(DaemonError::SecurityError(
            "lease token has an empty kid, payload or signature segment".to_string(),
        ));
    }

    let kid_bytes = B64NP.decode(kid_seg).map_err(|e| {
        DaemonError::SecurityError(format!("lease token kid segment is not valid base64: {e}"))
    })?;
    let kid = String::from_utf8(kid_bytes).map_err(|_| {
        DaemonError::SecurityError("lease token kid segment is not valid UTF-8".to_string())
    })?;

    let payload_bytes = B64NP.decode(payload_b64).map_err(|e| {
        DaemonError::SecurityError(format!("lease token payload is not valid base64: {e}"))
    })?;
    let claims: LeaseClaims = serde_json::from_slice(&payload_bytes).map_err(|e| {
        DaemonError::SecurityError(format!("lease token payload is not valid JSON: {e}"))
    })?;

    Ok((kid, claims, sig_b64.to_string()))
}

/// 载荷业务校验（字段非空 / verify_mode 合法 / 时间区间合法）。
///
/// # Errors
/// 任一不满足 → [`DaemonError::SecurityError`]。
fn validate_claims(claims: &LeaseClaims) -> DaemonResult<()> {
    if claims.lease_id.is_empty() {
        return Err(DaemonError::SecurityError(
            "lease_id must not be empty".to_string(),
        ));
    }
    if claims.device_id.is_empty() {
        return Err(DaemonError::SecurityError(
            "device_id must not be empty".to_string(),
        ));
    }
    if claims.mid.is_empty() {
        return Err(DaemonError::SecurityError(
            "mid must not be empty".to_string(),
        ));
    }
    if claims.tier.is_empty() {
        return Err(DaemonError::SecurityError(
            "tier must not be empty".to_string(),
        ));
    }
    if VerifyMode::parse(&claims.verify_mode).is_none() {
        return Err(DaemonError::SecurityError(format!(
            "verify_mode must be A|B|C, got {}",
            claims.verify_mode
        )));
    }
    if claims.valid_until <= claims.issued_at {
        return Err(DaemonError::SecurityError(format!(
            "valid_until ({}) must be greater than issued_at ({})",
            claims.valid_until, claims.issued_at
        )));
    }
    Ok(())
}

/// Ed25519 验签（原始 64 字节签名，STANDARD base64）。
///
/// # Errors
/// base64 非法 / 签名长度 ≠ 64 / 验签不通过 → [`DaemonError::SecurityError`]。
fn verify_ed25519(key: &VerifyingKey, message: &[u8], signature_b64: &str) -> DaemonResult<()> {
    use ed25519_dalek::Verifier as _;
    let sig_bytes = B64.decode(signature_b64.trim()).map_err(|e| {
        DaemonError::SecurityError(format!("lease token signature is not valid base64: {e}"))
    })?;
    let sig_array: [u8; ED25519_SIGNATURE_LEN] = sig_bytes.as_slice().try_into().map_err(|_| {
        DaemonError::SecurityError(format!(
            "lease token signature length {} != {ED25519_SIGNATURE_LEN}",
            sig_bytes.len()
        ))
    })?;
    let signature = Signature::from_bytes(&sig_array);
    key.verify(message, &signature).map_err(|e| {
        DaemonError::SecurityError(format!("lease token signature verification failed: {e}"))
    })
}

/// 取 JSON 对象的 key 集合（排序，便于白名单断言）。
fn object_keys(value: &serde_json::Value) -> Vec<String> {
    match value.as_object() {
        Some(map) => {
            let mut keys: Vec<String> = map.keys().cloned().collect();
            keys.sort();
            keys
        }
        None => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::machine_id::{FingerprintKey, MachineIdentity, StaticAnchor};
    use crate::auth::signing::StaticKeyProvider;
    use crate::error::{ERR_AUTH, ERR_CONFIG, ERR_NETWORK, ERR_SECURITY};
    use ed25519_dalek::{SigningKey, Verifier as _};
    use std::sync::Mutex as StdMutex;

    /// test-only：Ed25519 私钥种子 A（**仅测试，禁止用于真实部署**）。
    const TEST_ONLY_KEY_A: [u8; 32] = [0x1au8; 32];
    /// test-only：Ed25519 私钥种子 B（kid 轮换测试用，**仅测试**）。
    const TEST_ONLY_KEY_B: [u8; 32] = [0x2bu8; 32];
    /// test-only：指纹 HMAC key（派 mid 用，禁止真实部署）。
    const TEST_ONLY_FP_KEY: &[u8] = b"TEST_ONLY_client_fp_key";
    /// 测试基准时间（UTC 秒）。
    ///
    /// 取 **2100-01-01**（远未来）：保证相对它构造的租约 `valid_until` 恒大于真实系统
    /// 时钟，从而 `verify_lease_locally` 的过期判定不受宿主真实时间影响，测试可重复。
    const T0: i64 = 4_102_444_800;

    // ---- 测试辅助 ----

    /// 经 task 3 指纹模块派生 mid。
    fn test_mid() -> String {
        let identity = MachineIdentity::new(
            vec![Box::new(StaticAnchor::new(
                "test-anchor",
                Some("gw-client-001"),
            ))],
            1,
            FingerprintKey::from_bytes(TEST_ONLY_FP_KEY.to_vec())
                .expect("test-only fp key is non-empty"),
        );
        identity
            .get_machine_fingerprint()
            .expect("quorum ok: 1 usable anchor of 1 required")
    }

    /// 恒开放行的测试闸门。
    struct AlwaysLicensed;
    impl LicenseGate for AlwaysLicensed {
        fn can_sign(&self) -> bool {
            true
        }
    }

    /// 构造一个签名器（复用 task 21）。
    fn test_signer() -> Arc<AuthSigner> {
        Arc::new(
            AuthSigner::new(
                Arc::new(StaticKeyProvider::new(TEST_ONLY_KEY_A)),
                Arc::new(AlwaysLicensed),
                test_mid(),
            )
            .expect("test mid is 64-hex"),
        )
    }

    /// 构造带公钥集（A / B 两把 kid）的客户端；传输来自共享的 [`FakeTransport`]。
    ///
    /// 注入回执签名密钥（私钥 A，与 [`test_signer`] 同一把设备私钥），
    /// 使回执签名路径可用（正常路径）。
    fn client_with_fake(fake: &Arc<FakeTransport>) -> LicensingClient {
        let cfg = LicensingClientConfig::new("https://licensing.test/v1", 24, 10, 30)
            .expect("non-empty url");
        let signer = test_signer();
        let transport: Arc<dyn LicenseTransport> = Arc::clone(fake) as Arc<dyn LicenseTransport>;
        let mut client = LicensingClient::with_transport(cfg, signer, test_mid(), transport)
            .with_receipt_signer(Arc::new(StaticKeyProvider::new(TEST_ONLY_KEY_A)));
        client
            .register_public_key("kid-a", &public_key_bytes(TEST_ONLY_KEY_A))
            .expect("register kid-a");
        client
            .register_public_key("kid-b", &public_key_bytes(TEST_ONLY_KEY_B))
            .expect("register kid-b");
        client
    }

    /// 构造**未注入回执签名密钥**的客户端（用于 T2：空签名 = 失败信号）。
    fn client_with_fake_no_receipt_signer(fake: &Arc<FakeTransport>) -> LicensingClient {
        let cfg = LicensingClientConfig::new("https://licensing.test/v1", 24, 10, 30)
            .expect("non-empty url");
        let signer = test_signer();
        let transport: Arc<dyn LicenseTransport> = Arc::clone(fake) as Arc<dyn LicenseTransport>;
        let mut client = LicensingClient::with_transport(cfg, signer, test_mid(), transport);
        client
            .register_public_key("kid-a", &public_key_bytes(TEST_ONLY_KEY_A))
            .expect("register kid-a");
        client
    }

    /// 便捷：新建 fake + 客户端（多数测试用）。
    fn setup() -> (LicensingClient, Arc<FakeTransport>) {
        let fake = Arc::new(FakeTransport::default());
        let client = client_with_fake(&fake);
        (client, fake)
    }

    /// 私钥种子 → 公钥原始 32 字节。
    fn public_key_bytes(seed: [u8; 32]) -> [u8; 32] {
        SigningKey::from_bytes(&seed).verifying_key().to_bytes()
    }

    /// 构造一个合法租约 Token 的三段式串。
    ///
    /// `seed` 决定签名私钥；`kid` 决定外层 kid；`valid_until` / `verify_mode` 可定制。
    fn build_token(
        seed: [u8; 32],
        kid: &str,
        verify_mode: &str,
        issued_at: i64,
        valid_until: i64,
    ) -> String {
        build_token_full(seed, kid, verify_mode, issued_at, valid_until, false, false)
    }

    /// 更完整的 Token 构造（可篡改载荷 / 篡改签名）。
    fn build_token_full(
        seed: [u8; 32],
        kid: &str,
        verify_mode: &str,
        issued_at: i64,
        valid_until: i64,
        tamper_tier: bool,
        tamper_sig: bool,
    ) -> String {
        let claims = LeaseClaims {
            lease_id: "lease-0001".to_string(),
            device_id: "dev-0001".to_string(),
            mid: test_mid(),
            tier: "standard".to_string(),
            verify_mode: verify_mode.to_string(),
            issued_at,
            valid_until,
        };
        let message = render_signing_message(kid, &claims);
        let signing_key = SigningKey::from_bytes(&seed);
        let mut sig = signing_key.sign(&message).to_bytes();
        if tamper_sig {
            sig[0] ^= 0xFF;
        }
        // 载荷：先按原样序列化，再（可选）改为篡改后的 tier（签名域已用原值签）。
        let mut payload_claims = claims.clone();
        if tamper_tier {
            payload_claims.tier = "enterprise".to_string();
        }
        let payload_json = serde_json::to_vec(&payload_claims).expect("claims serialize");
        format!(
            "{}.{}.{}",
            B64NP.encode(kid.as_bytes()),
            B64NP.encode(payload_json),
            B64.encode(sig)
        )
    }

    /// 构造激活成功响应。
    fn activate_ok_response(token: &str) -> serde_json::Value {
        serde_json::json!({ "lease_token": token })
    }

    // ---- FakeTransport ----

    /// 测试用网络传输假实现：按编程动作返回结果并记录调用。
    #[derive(Default)]
    pub struct FakeTransport {
        /// 下一个响应的动作。
        action: StdMutex<FakeAction>,
        /// 记录每次 `post_json` 的 URL（供断言端点）。
        calls: StdMutex<Vec<String>>,
        /// 记录最近一次请求体快照（供白名单 / 大整数断言）。
        last_body: StdMutex<Option<serde_json::Value>>,
    }

    /// fake 的动作。
    #[derive(Default)]
    enum FakeAction {
        /// 返回成功响应体。
        Ok(serde_json::Value),
        /// 返回网络错误（模拟断网 / 超时）。
        NetFail(String),
        /// 返回服务端拒绝（AuthError）。
        Reject(String),
        /// 未编程（默认）。
        #[default]
        Unset,
    }

    impl FakeTransport {
        /// 编程：下一次请求返回给定响应体。
        fn respond_with(&self, value: serde_json::Value) {
            *self.action.lock().expect("fake action lock") = FakeAction::Ok(value);
        }

        /// 编程：下一次请求返回网络错误。
        fn fail_network(&self, msg: &str) {
            *self.action.lock().expect("fake action lock") = FakeAction::NetFail(msg.to_string());
        }

        /// 编程：下一次请求返回服务端拒绝。
        fn reject(&self, msg: &str) {
            *self.action.lock().expect("fake action lock") = FakeAction::Reject(msg.to_string());
        }

        /// 最近一次请求体（克隆）。
        fn last_body(&self) -> Option<serde_json::Value> {
            self.last_body.lock().expect("body lock").clone()
        }

        /// 调用过的 URL 列表。
        fn calls(&self) -> Vec<String> {
            self.calls.lock().expect("calls lock").clone()
        }
    }

    impl LicenseTransport for FakeTransport {
        fn post_json(
            &self,
            url: &str,
            body: &serde_json::Value,
        ) -> Result<serde_json::Value, DaemonError> {
            self.calls.lock().expect("calls lock").push(url.to_string());
            *self.last_body.lock().expect("body lock") = Some(body.clone());
            match &*self.action.lock().expect("fake action lock") {
                FakeAction::Ok(v) => Ok(v.clone()),
                FakeAction::NetFail(m) => Err(DaemonError::NetworkError(m.clone())),
                FakeAction::Reject(m) => Err(DaemonError::AuthError(m.clone())),
                FakeAction::Unset => Err(DaemonError::NetworkError(
                    "fake transport not programmed".to_string(),
                )),
            }
        }
    }

    // ================= 测试用例 =================

    /// 要求 5（端到端·激活成功）：fake 返回合法 Token → `Licensed`，kid / tier 正确。
    #[tokio::test]
    async fn activate_success_returns_licensed() {
        let (client, fake) = setup();
        let token = build_token(TEST_ONLY_KEY_A, "kid-a", "B", T0, T0 + 365 * 86_400);
        fake.respond_with(activate_ok_response(&token));

        let state = client
            .activate("ACT-CODE-1234")
            .await
            .expect("activation must succeed");
        match &state {
            LicenseState::Licensed { lease } => {
                assert_eq!(lease.lease_id, "lease-0001");
                assert_eq!(lease.device_id, "dev-0001");
                assert_eq!(lease.kid, "kid-a");
                assert_eq!(lease.tier, "standard");
                assert_eq!(lease.verify_mode, VerifyMode::B);
                assert_eq!(lease.raw, token);
            }
            other => panic!("expected Licensed, got {other:?}"),
        }
        assert_eq!(client.current_state().name(), "Licensed");
        assert!(client.current_state().can_sign());
        // 请求打到 activate 端点。
        assert!(fake.calls()[0].ends_with("/activate"), "{:?}", fake.calls());
    }

    /// 要求 5（端到端·激活被拒）：服务端拒绝 → AuthError，状态保持 Unlicensed。
    #[tokio::test]
    async fn activate_rejected_keeps_unlicensed() {
        let (client, fake) = setup();
        fake.reject("activation code invalid");

        let err = client
            .activate("BAD-CODE")
            .await
            .expect_err("rejected activation must error");
        assert_eq!(err.error_code(), ERR_AUTH);
        assert!(matches!(err, DaemonError::AuthError(_)));
        assert_eq!(client.current_state(), LicenseState::Unlicensed);
    }

    /// 激活响应缺 `lease_token` → AuthError（不 panic）。
    #[tokio::test]
    async fn activate_missing_token_field_errors() {
        let (client, fake) = setup();
        fake.respond_with(serde_json::json!({ "ok": true }));

        let err = client
            .activate("ACT-1")
            .await
            .expect_err("missing lease_token must error");
        assert_eq!(err.error_code(), ERR_AUTH);
        assert!(err.to_string().contains("lease_token"), "{err}");
    }

    /// 空激活码 → ConfigError（本地前置校验，不打网络）。
    #[tokio::test]
    async fn activate_empty_code_is_config_error() {
        let (client, _fake) = setup();
        let err = client.activate("   ").await.expect_err("empty code");
        assert_eq!(err.error_code(), ERR_CONFIG);
    }

    /// 要求 5（端到端·心跳超时）：网络失败 → NetworkError，**状态不降级**。
    #[tokio::test]
    async fn heartbeat_timeout_does_not_downgrade() {
        let (client, fake) = setup();
        let token = build_token(TEST_ONLY_KEY_A, "kid-a", "B", T0, T0 + 365 * 86_400);
        fake.respond_with(activate_ok_response(&token));
        client.activate("ACT-1").await.expect("activate ok");
        assert_eq!(client.current_state().name(), "Licensed");

        fake.fail_network("connection timed out");
        let err = client
            .heartbeat(Some((1, 10)))
            .await
            .expect_err("heartbeat timeout must error");
        assert_eq!(err.error_code(), ERR_NETWORK);
        // 关键断言：网络失败**不得**降级。
        assert_eq!(
            client.current_state().name(),
            "Licensed",
            "network failure must NOT downgrade state"
        );
        assert!(client.current_state().allows_northbound_forward());
    }

    /// 要求 5（端到端·断网）：反复心跳失败 + `tick` 推进，状态保持 Licensed（未到宽限）。
    #[tokio::test]
    async fn offline_heartbeats_keep_collecting_and_forwarding() {
        let (client, fake) = setup();
        let token = build_token(TEST_ONLY_KEY_A, "kid-a", "B", T0, T0 + 365 * 86_400);
        fake.respond_with(activate_ok_response(&token));
        client.activate("ACT-1").await.expect("activate ok");

        // 断网：连续 3 天反复心跳失败 + tick。
        for day in 1..=3 {
            fake.fail_network("offline");
            assert!(client.heartbeat(None).await.is_err());
            let state = client.tick(T0 + day * SECS_PER_DAY);
            assert_eq!(state.name(), "Licensed");
            assert!(state.allows_local_capture(), "降级也不停采集，何况未降级");
            assert!(state.allows_northbound_forward(), "B 档断网仍转发");
        }
    }

    /// B 档心跳携带回执游标 → 请求体含 cursor 字段，且大整数走字符串。
    #[tokio::test]
    async fn heartbeat_with_cursor_sends_string_numbers() {
        let (client, fake) = setup();
        let token = build_token(TEST_ONLY_KEY_A, "kid-a", "B", T0, T0 + 365 * 86_400);
        fake.respond_with(activate_ok_response(&token));
        client.activate("ACT-1").await.expect("activate ok");

        fake.respond_with(activate_ok_response(&token));
        client
            .heartbeat(Some((100, 9007199254740991)))
            .await
            .expect("heartbeat ok with big cursor");

        let body = fake.last_body().expect("body recorded");
        // cursor 是字符串（大整数不走 JSON number）。
        assert_eq!(body["cursor_from"], serde_json::json!("100"));
        assert_eq!(body["cursor_to"], serde_json::json!("9007199254740991"));
        assert!(body["cursor_to"].is_string(), "big int must be string");
    }

    /// 要求 7：回执请求体 key 集合**恰好等于** 8 个白名单字段。
    #[tokio::test]
    async fn receipt_body_has_exactly_whitelisted_keys() {
        let (client, fake) = setup();
        let token = build_token(TEST_ONLY_KEY_A, "kid-a", "B", T0, T0 + 365 * 86_400);
        fake.respond_with(activate_ok_response(&token));
        client.activate("ACT-1").await.expect("activate ok");

        fake.respond_with(serde_json::json!({ "ok": true }));
        client
            .report_receipt(0, 42, 42, "sha256:deadbeef")
            .await
            .expect("receipt report ok");

        let body = fake.last_body().expect("body recorded");
        let mut keys = object_keys(&body);
        keys.sort();
        let mut expected: Vec<String> = RECEIPT_FIELD_WHITELIST
            .iter()
            .map(|s| s.to_string())
            .collect();
        expected.sort();
        assert_eq!(
            keys, expected,
            "receipt body keys must equal the 8-field whitelist"
        );
        assert_eq!(keys.len(), 8);

        // **绝不携带业务数值**：确认没有额外的业务字段。
        for extra in [
            "value",
            "points",
            "payload",
            "samples",
            "batch",
            "device_id",
        ] {
            assert!(
                body.get(extra).is_none(),
                "receipt must not carry business field {extra}"
            );
        }
    }

    /// 要求 7：回执的 `seq_from/seq_to/count/ts` 均为 JSON **字符串**（大整数红线）。
    #[tokio::test]
    async fn receipt_numbers_are_json_strings() {
        let (client, fake) = setup();
        let token = build_token(TEST_ONLY_KEY_A, "kid-a", "B", T0, T0 + 365 * 86_400);
        fake.respond_with(activate_ok_response(&token));
        client.activate("ACT-1").await.expect("activate ok");

        fake.respond_with(serde_json::json!({ "ok": true }));
        // 用超过 2^53−1 的值，证明大整数不经 JSON number。
        let big = JSON_SAFE_INT_MAX + 12345;
        client
            .report_receipt(big, big + 1, 7, "sha256:cafe")
            .await
            .expect("receipt report ok");

        let body = fake.last_body().expect("body recorded");
        for field in ["seq_from", "seq_to", "count", "ts"] {
            assert!(
                body[field].is_string(),
                "{field} must be a JSON string (big int rule), got {:?}",
                body[field]
            );
        }
        assert_eq!(body["seq_from"], serde_json::json!(big.to_string()));
        assert_eq!(body["seq_to"], serde_json::json!((big + 1).to_string()));
        assert_eq!(body["count"], serde_json::json!("7"));
    }

    /// 未激活调 `report_receipt` → AuthError。
    #[tokio::test]
    async fn receipt_without_lease_is_rejected() {
        let (client, _fake) = setup();
        let err = client
            .report_receipt(0, 1, 1, "sha256:x")
            .await
            .expect_err("no lease must reject");
        assert_eq!(err.error_code(), ERR_AUTH);
    }

    /// 负 count → ConfigError。
    #[tokio::test]
    async fn receipt_negative_count_is_rejected() {
        let (client, fake) = setup();
        let token = build_token(TEST_ONLY_KEY_A, "kid-a", "B", T0, T0 + 365 * 86_400);
        fake.respond_with(activate_ok_response(&token));
        client.activate("ACT-1").await.expect("activate ok");

        let err = client
            .report_receipt(0, 1, -1, "sha256:x")
            .await
            .expect_err("negative count");
        assert_eq!(err.error_code(), ERR_CONFIG);
    }

    /// **T1（QA MAJOR-2 核心）**：正常路径下回执 `sig` 非空，且**真正能验签通过**。
    ///
    /// 这是 B 档二次校验信任机制的守护：测试独立重建「服务端会拼的签名消息」→ 算
    /// `semantic_digest` → 用设备公钥对 `sig` 做 Ed25519 验签。**同时守住两件事**：
    /// ① 签名非空且有效；② 签名域与字段集合均与预期一致（改任一字段都会验签失败）。
    #[tokio::test]
    async fn receipt_signature_verifies_against_its_public_key() {
        let (client, fake) = setup();
        let token = build_token(TEST_ONLY_KEY_A, "kid-a", "B", T0, T0 + 365 * 86_400);
        fake.respond_with(activate_ok_response(&token));
        client.activate("ACT-1").await.expect("activate ok");

        fake.respond_with(serde_json::json!({ "ok": true }));
        client
            .report_receipt(7, 42, 35, "sha256:deadbeef")
            .await
            .expect("receipt report ok");

        let body = fake.last_body().expect("body recorded");
        let sig_b64 = body["sig"].as_str().expect("sig must be a string");
        assert!(!sig_b64.is_empty(), "receipt sig must not be empty");

        // 独立重建服务端侧签名消息（**不使用客户端任何私有状态**，只用请求体字段）。
        let expected_msg = receipt_signing_message(
            body["device_mid"].as_str().expect("device_mid"),
            body["lease_id"].as_str().expect("lease_id"),
            7,
            42,
            35,
            "sha256:deadbeef",
            body["ts"]
                .as_str()
                .expect("ts")
                .parse::<i64>()
                .expect("ts is i64 string"),
        );
        let payload_hash = semantic_digest(expected_msg.as_bytes());

        // 用对应公钥真正验签。
        let sig_bytes: [u8; ED25519_SIGNATURE_LEN] = B64
            .decode(sig_b64)
            .expect("sig is valid base64")
            .as_slice()
            .try_into()
            .expect("sig is 64 bytes");
        let signature = Signature::from_bytes(&sig_bytes);
        let public_key = SigningKey::from_bytes(&TEST_ONLY_KEY_A).verifying_key();
        public_key
            .verify(&payload_hash, &signature)
            .expect("receipt signature must verify against the device public key");

        // 反向守护：用**另一把**公钥验签必须失败（证明确实是设备私钥签的）。
        let other_key = SigningKey::from_bytes(&TEST_ONLY_KEY_B).verifying_key();
        assert!(
            other_key.verify(&payload_hash, &signature).is_err(),
            "a foreign public key must not verify the receipt signature"
        );
    }

    /// **T2（守住设计意图）**：未注入回执签名密钥时，`sig` 为**空串**（明确失败信号）。
    ///
    /// 这条把「空签名 = 失败信号（服务端会拒绝）」写成可执行断言，防止后人把空串当成功路径。
    #[tokio::test]
    async fn receipt_signature_is_empty_when_signer_is_unavailable() {
        let fake = Arc::new(FakeTransport::default());
        let client = client_with_fake_no_receipt_signer(&fake);
        let token = build_token(TEST_ONLY_KEY_A, "kid-a", "B", T0, T0 + 365 * 86_400);
        fake.respond_with(activate_ok_response(&token));
        client.activate("ACT-1").await.expect("activate ok");

        fake.respond_with(serde_json::json!({ "ok": true }));
        client
            .report_receipt(0, 1, 1, "sha256:x")
            .await
            .expect("transport still called");

        let body = fake.last_body().expect("body recorded");
        // 空签名是**明确的失败信号**：服务端收到空串会拒绝，绝不冒充有效签名。
        assert_eq!(
            body["sig"].as_str().expect("sig is a string"),
            "",
            "unavailable signer must emit an EMPTY sig (failure signal), not a fake signature"
        );
    }

    /// **T3（防漏字段）**：签名消息覆盖回执白名单的**全部业务字段**。
    ///
    /// 对每个字段**单独变异**一个值 → 签名消息必须改变；若某字段被漏出签名域，
    /// 中间人篡改它就不会破坏签名，本测试即失败。最后再断言 Ed25519 签名随字段变化而不同。
    #[test]
    fn receipt_signing_message_covers_all_whitelisted_fields() {
        // 基线（无变异）。
        let base = receipt_signing_message("mid-1", "lease-1", 10, 20, 11, "digest-1", 1_000);

        // 逐字段单独变异：每个字段变更都必须让签名消息改变。
        let variants: Vec<(&str, String)> = vec![
            (
                "mid",
                receipt_signing_message("mid-2", "lease-1", 10, 20, 11, "digest-1", 1_000),
            ),
            (
                "lease_id",
                receipt_signing_message("mid-1", "lease-2", 10, 20, 11, "digest-1", 1_000),
            ),
            (
                "seq_from",
                receipt_signing_message("mid-1", "lease-1", 11, 20, 11, "digest-1", 1_000),
            ),
            (
                "seq_to",
                receipt_signing_message("mid-1", "lease-1", 10, 21, 11, "digest-1", 1_000),
            ),
            (
                "count",
                receipt_signing_message("mid-1", "lease-1", 10, 20, 12, "digest-1", 1_000),
            ),
            (
                "payload_digest",
                receipt_signing_message("mid-1", "lease-1", 10, 20, 11, "digest-2", 1_000),
            ),
            (
                "ts",
                receipt_signing_message("mid-1", "lease-1", 10, 20, 11, "digest-1", 1_001),
            ),
        ];
        // 白名单的 7 个业务字段（`sig` 本身不在签名消息内，它是签名输出）。
        assert_eq!(variants.len(), RECEIPT_FIELD_WHITELIST.len() - 1);
        for (field, mutated) in &variants {
            assert_ne!(
                base, *mutated,
                "field `{field}` must participate in the receipt signing message"
            );
        }

        // 端到端加固：同一签名密钥下，任一字段变异 → 实际 Ed25519 签名必然不同。
        let signing_key = SigningKey::from_bytes(&TEST_ONLY_KEY_A);
        let base_sig = signing_key
            .sign(&semantic_digest(base.as_bytes()))
            .to_bytes();
        for (field, mutated) in &variants {
            let sig = signing_key
                .sign(&semantic_digest(mutated.as_bytes()))
                .to_bytes();
            assert_ne!(
                base_sig, sig,
                "mutating `{field}` must change the actual signature"
            );
        }
    }

    /// 要求 5：本地验签支持多 kid——kid-a 与 kid-b 都能验通（轮换兼容）。
    #[test]
    fn verify_lease_supports_multiple_kids() {
        let (client, _fake) = setup();
        assert_eq!(client.known_kids(), vec!["kid-a", "kid-b"]);

        let token_a = build_token(TEST_ONLY_KEY_A, "kid-a", "B", T0, T0 + 365 * 86_400);
        let token_b = build_token(TEST_ONLY_KEY_B, "kid-b", "A", T0, T0 + 365 * 86_400);

        let lease_a = client
            .verify_lease_locally(&token_a)
            .expect("kid-a verifies");
        assert_eq!(lease_a.kid, "kid-a");
        assert_eq!(lease_a.verify_mode, VerifyMode::B);

        let lease_b = client
            .verify_lease_locally(&token_b)
            .expect("kid-b verifies");
        assert_eq!(lease_b.kid, "kid-b");
        assert_eq!(lease_b.verify_mode, VerifyMode::A);
    }

    /// 要求 5：验签失败各分支——未知 kid / 签名不符 / 已过期 / 结构错误。
    #[test]
    fn verify_lease_rejects_all_failure_modes() {
        let (client, _fake) = setup();

        // 未知 kid（用 kid-z 签名，公钥集里没有）。
        let unknown = build_token(TEST_ONLY_KEY_A, "kid-z", "B", T0, T0 + 86_400);
        let err = client
            .verify_lease_locally(&unknown)
            .expect_err("unknown kid must fail");
        assert_eq!(err.error_code(), ERR_SECURITY);
        assert!(err.to_string().contains("unknown kid"), "{err}");

        // 签名不符（篡改签名字节）。
        let bad_sig = build_token_full(TEST_ONLY_KEY_A, "kid-a", "B", T0, T0 + 86_400, false, true);
        let err = client
            .verify_lease_locally(&bad_sig)
            .expect_err("bad signature must fail");
        assert_eq!(err.error_code(), ERR_SECURITY);

        // 已过期（valid_until 在过去）。先把客户端单调时钟推进到 T0，
        // 使过期判定以 T0 为「现在」（T0 远未来，故 `now` 与真实系统时钟取大者仍为 T0）。
        let _ = client.tick(T0);
        let expired = build_token(TEST_ONLY_KEY_A, "kid-a", "B", T0 - 200_000, T0 - 100_000);
        let err = client
            .verify_lease_locally(&expired)
            .expect_err("expired must fail");
        assert_eq!(err.error_code(), ERR_AUTH);
        assert!(err.to_string().contains("expired"), "{err}");

        // 结构错误（非三段式）。
        let err = client
            .verify_lease_locally("not-a-token")
            .expect_err("malformed must fail");
        assert_eq!(err.error_code(), ERR_SECURITY);

        // 结构错误（空段）。
        let err = client
            .verify_lease_locally("..")
            .expect_err("empty segments must fail");
        assert_eq!(err.error_code(), ERR_SECURITY);
    }

    /// 验签：篡改载荷（tier 免费升档）→ 签名不符 → SecurityError。
    #[test]
    fn verify_lease_rejects_tampered_payload() {
        let (client, _fake) = setup();
        let tampered =
            build_token_full(TEST_ONLY_KEY_A, "kid-a", "B", T0, T0 + 86_400, true, false);
        let err = client
            .verify_lease_locally(&tampered)
            .expect_err("tampered tier must fail");
        assert_eq!(err.error_code(), ERR_SECURITY);
    }

    /// 验签：公钥长度非法 / kid 为空 → ConfigError。
    #[test]
    fn register_public_key_validates_input() {
        let (mut client, _fake) = setup();

        let err = client
            .register_public_key("", &public_key_bytes(TEST_ONLY_KEY_A))
            .expect_err("empty kid");
        assert_eq!(err.error_code(), ERR_CONFIG);

        let err = client
            .register_public_key("kid-x", &[0u8; 10])
            .expect_err("short key");
        assert_eq!(err.error_code(), ERR_CONFIG);
    }

    /// 要求 1：离线宽限 7 天——逐日推进，第 7 天仍 `Grace`，第 8 天转 `Degraded`。
    #[tokio::test]
    async fn offline_grace_seven_days_then_degraded() {
        let (client, fake) = setup();
        // 租约很快过期（1 天后），触发进入 Grace。
        let token = build_token(TEST_ONLY_KEY_A, "kid-a", "B", T0, T0 + SECS_PER_DAY);
        fake.respond_with(activate_ok_response(&token));
        client.activate("ACT-1").await.expect("activate ok");

        // 先推进到 1 天后 → 租约过期 → 进入 Grace（days_left = 7）。
        let state = client.tick(T0 + SECS_PER_DAY);
        match state {
            LicenseState::Grace { days_left, .. } => assert_eq!(days_left, 7),
            other => panic!("expected Grace day0, got {other:?}"),
        }

        // 逐日推进：第 1..7 天仍是 Grace。
        for day in 1..=7 {
            let state = client.tick(T0 + SECS_PER_DAY + day * SECS_PER_DAY);
            match &state {
                LicenseState::Grace { days_left, .. } => {
                    assert_eq!(*days_left, GRACE_DAYS - day, "day {day}");
                    assert!(state.can_sign(), "grace must still allow signing");
                    assert!(state.allows_northbound_forward(), "grace still forwards");
                }
                other => panic!("day {day}: expected Grace, got {other:?}"),
            }
        }

        // 第 8 天 → Degraded。
        let state = client.tick(T0 + SECS_PER_DAY + 8 * SECS_PER_DAY);
        match &state {
            LicenseState::Degraded { reason } => {
                assert!(reason.contains("grace"), "{reason}");
            }
            other => panic!("day 8: expected Degraded, got {other:?}"),
        }
        // 降级 ≠ 停用：仍允许本地采集，停北向转发。
        assert!(
            state.allows_local_capture(),
            "degraded still captures locally"
        );
        assert!(
            !state.allows_northbound_forward(),
            "degraded stops northbound"
        );
        assert!(!state.can_sign(), "degraded cannot sign");
    }

    /// 要求 3：试用 3 天——`days_left` 从 3 递减到 0 后转 `Degraded`（**不转** `Unlicensed`）。
    #[test]
    fn trial_three_days_then_degraded_not_unlicensed() {
        let (client, _fake) = setup();

        let state = client.begin_trial(T0);
        assert_eq!(state, LicenseState::Trial { days_left: 3 });

        // day1..day3 递减（day3 → days_left = 0，仍属试用最后时刻）。
        for day in 1..=3 {
            let state = client.tick(T0 + day * SECS_PER_DAY);
            match state {
                LicenseState::Trial { days_left } => {
                    assert_eq!(days_left, TRIAL_DAYS - day, "day {day}");
                }
                other => panic!("day {day}: expected Trial, got {other:?}"),
            }
        }

        // 超过 3 天（进入第 4 天）→ Degraded（不是 Unlicensed）。
        let state = client.tick(T0 + 4 * SECS_PER_DAY);
        assert!(
            matches!(state, LicenseState::Degraded { .. }),
            "trial exhaustion must degrade, got {state:?}"
        );
        assert_ne!(state, LicenseState::Unlicensed);
        assert!(state.allows_local_capture());
        assert!(!state.allows_northbound_forward());
    }

    /// 要求 4（MINOR-3 修正）：系统时间回拨防御——**在 `Trial` 上进行**，可区分回拨是否生效。
    ///
    /// 原测试在进入 `Degraded` 后调 `tick`，命中 `other => other.clone()`，任何输入都回不到
    /// `Trial`——是「无关变量通过」的假守护。改为在 `Trial` 上验证单调时钟：推进 1 天后把
    /// `now` 拨回激活时刻，`days_left` **不得**从 2 涨回 3（否则调时钟即可无限续期）。
    #[test]
    fn clock_rollback_does_not_extend_trial() {
        let (client, _fake) = setup();

        client.begin_trial(T0);
        assert_eq!(client.tick(T0), LicenseState::Trial { days_left: 3 });

        // 正常推进 1 天 → days_left = 2。
        let state = client.tick(T0 + SECS_PER_DAY);
        assert_eq!(state, LicenseState::Trial { days_left: 2 });

        // 把 now 拨回激活时刻（回拨）：单调时钟不后退，days_left 必须**仍为 2**。
        let state = client.tick(T0);
        assert_eq!(
            state,
            LicenseState::Trial { days_left: 2 },
            "clock rollback must NOT resurrect trial days"
        );

        // 再拨到「激活后 0.5 天」的中间点：仍不续期。
        let state = client.tick(T0 + SECS_PER_DAY / 2);
        assert_eq!(
            state,
            LicenseState::Trial { days_left: 2 },
            "any rollback within elapsed window must not extend trial"
        );

        // 单调性证明：继续正向推进 1 天 → days_left 如期减到 1（回拨没有「冻结」推进）。
        let state = client.tick(T0 + 2 * SECS_PER_DAY);
        assert_eq!(
            state,
            LicenseState::Trial { days_left: 1 },
            "forward progress must resume after a rollback attempt"
        );
    }

    /// 要求 4（Grace 版）：宽限期内时间回拨 → `days_left` **不增加**。
    #[tokio::test]
    async fn clock_rollback_does_not_extend_grace() {
        let (client, fake) = setup();
        let token = build_token(TEST_ONLY_KEY_A, "kid-a", "B", T0, T0 + SECS_PER_DAY);
        fake.respond_with(activate_ok_response(&token));
        client.activate("ACT-1").await.expect("activate ok");

        // 进入 Grace 并推进 3 天。
        client.tick(T0 + SECS_PER_DAY); // Grace day0
        let state = client.tick(T0 + SECS_PER_DAY + 3 * SECS_PER_DAY);
        let before = match state {
            LicenseState::Grace { days_left, .. } => days_left,
            other => panic!("expected Grace, got {other:?}"),
        };
        assert_eq!(before, 4, "3 days elapsed → 4 days left");

        // 时间回拨到宽限起点：days_left 不得增加。
        let state = client.tick(T0 + SECS_PER_DAY);
        match state {
            LicenseState::Grace { days_left, .. } => {
                assert_eq!(days_left, before, "rollback must not extend grace");
            }
            other => panic!("expected Grace, got {other:?}"),
        }
    }

    /// 心跳成功后刷新宽限基准：验证「重连回到 Licensed」。
    #[tokio::test]
    async fn successful_heartbeat_restores_licensed() {
        let (client, fake) = setup();
        let token = build_token(TEST_ONLY_KEY_A, "kid-a", "B", T0, T0 + SECS_PER_DAY);
        fake.respond_with(activate_ok_response(&token));
        client.activate("ACT-1").await.expect("activate ok");

        // 进入 Grace。
        client.tick(T0 + SECS_PER_DAY);
        assert_eq!(client.current_state().name(), "Grace");

        // 心跳成功（新租约，有效期更长）→ 回到 Licensed。
        let fresh = build_token(TEST_ONLY_KEY_A, "kid-a", "B", T0, T0 + 365 * 86_400);
        fake.respond_with(activate_ok_response(&fresh));
        let state = client.heartbeat(None).await.expect("heartbeat ok");
        assert_eq!(state.name(), "Licensed");
        assert_eq!(client.current_state().name(), "Licensed");
    }

    /// 心跳验签失败（服务端返回被篡改 Token）→ SecurityError，状态不变。
    #[tokio::test]
    async fn heartbeat_with_invalid_token_errors_and_keeps_state() {
        let (client, fake) = setup();
        let token = build_token(TEST_ONLY_KEY_A, "kid-a", "B", T0, T0 + 365 * 86_400);
        fake.respond_with(activate_ok_response(&token));
        client.activate("ACT-1").await.expect("activate ok");

        let bad = build_token_full(TEST_ONLY_KEY_A, "kid-a", "B", T0, T0 + 86_400, false, true);
        fake.respond_with(activate_ok_response(&bad));
        let err = client
            .heartbeat(None)
            .await
            .expect_err("invalid token must error");
        assert_eq!(err.error_code(), ERR_SECURITY);
        // 状态保持 Licensed（心跳失败不降级）。
        assert_eq!(client.current_state().name(), "Licensed");
    }

    /// 要求 6（信号守护）：`LicenseGate` 实现与 `current_state().can_sign()` 一致。
    #[tokio::test]
    async fn license_gate_tracks_state() {
        let (client, fake) = setup();

        // 未激活：闸门关闭。
        assert!(!LicenseGate::can_sign(&client));
        assert_eq!(client.current_state(), LicenseState::Unlicensed);

        // 激活：闸门开放。
        let token = build_token(TEST_ONLY_KEY_A, "kid-a", "B", T0, T0 + 365 * 86_400);
        fake.respond_with(activate_ok_response(&token));
        client.activate("ACT-1").await.expect("activate ok");
        assert!(LicenseGate::can_sign(&client));

        // 降级：闸门关闭。先用短有效期租约激活，再推进「过期 → 宽限 → 宽限耗尽」。
        let trial_token = build_token(TEST_ONLY_KEY_A, "kid-a", "B", T0, T0 + SECS_PER_DAY);
        fake.respond_with(activate_ok_response(&trial_token));
        client.activate("ACT-2").await.expect("activate ok");
        // 第 1 步：过期 → 进入 Grace。
        let state = client.tick(T0 + SECS_PER_DAY);
        assert_eq!(state.name(), "Grace");
        assert!(LicenseGate::can_sign(&client), "grace still signs");
        // 第 2 步：宽限期（7 天）耗尽 → Degraded → 闸门关闭。
        let state = client.tick(T0 + SECS_PER_DAY + 8 * SECS_PER_DAY);
        assert_eq!(state.name(), "Degraded");
        assert!(!LicenseGate::can_sign(&client), "degraded → gate closed");
    }

    /// 要求 2（B 档核心）：断网时**采集与转发不被阻断**，错误返回给调用方。
    #[tokio::test]
    async fn class_b_offline_still_collects_and_forwards() {
        let (client, fake) = setup();
        let token = build_token(TEST_ONLY_KEY_A, "kid-a", "B", T0, T0 + 365 * 86_400);
        fake.respond_with(activate_ok_response(&token));
        client.activate("ACT-1").await.expect("activate ok");

        fake.fail_network("offline");
        let err = client
            .heartbeat(Some((1, 5)))
            .await
            .expect_err("offline heartbeat errs");
        // 错误明确可识别（NetworkError），交给调用方决定重试。
        assert_eq!(err.error_code(), ERR_NETWORK);

        // 状态仍允许采集与转发。
        let state = client.current_state();
        assert!(state.allows_local_capture());
        assert!(
            state.allows_northbound_forward(),
            "B-class offline keeps forwarding"
        );
    }

    /// 端点配置：默认值 + 空 URL 拒绝 + 端点拼接。
    #[test]
    fn config_defaults_and_endpoint_joining() {
        let cfg = LicensingClientConfig::default();
        assert_eq!(cfg.heartbeat_hours, 24);
        assert_eq!(cfg.connect_timeout_secs, 10);
        assert_eq!(cfg.request_timeout_secs, 30);

        let cfg = LicensingClientConfig::new("https://x.test/v1/", 12, 5, 15).expect("ok");
        assert_eq!(cfg.base_url, "https://x.test/v1/");
        assert_eq!(cfg.heartbeat_secs(), 12 * 3600);
        assert_eq!(cfg.endpoint("activate"), "https://x.test/v1/activate");
        assert_eq!(cfg.endpoint("/heartbeat"), "https://x.test/v1/heartbeat");

        let err = LicensingClientConfig::new("  ", 24, 10, 30).expect_err("empty url");
        assert_eq!(err.error_code(), ERR_CONFIG);
    }

    /// 默认传输（未注入）：请求返回 NetworkError，不 panic。
    #[tokio::test]
    async fn default_transport_is_unavailable() {
        let cfg = LicensingClientConfig::new("https://licensing.test/v1", 24, 10, 30).expect("ok");
        let client = LicensingClient::new(cfg, test_signer(), test_mid());
        let err = client
            .activate("ACT-1")
            .await
            .expect_err("default transport unavailable");
        assert_eq!(err.error_code(), ERR_NETWORK);
        assert_eq!(client.current_state(), LicenseState::Unlicensed);
    }

    /// 签名域渲染与 licensing-server **逐字节一致**（跨端一致性的关键守护）。
    #[test]
    fn signing_message_layout_matches_server() {
        let claims = LeaseClaims {
            lease_id: "lease-0001".to_string(),
            device_id: "dev-0001".to_string(),
            mid: "a1b2c3d4e5f6a7b8".to_string(),
            tier: "standard".to_string(),
            verify_mode: "B".to_string(),
            issued_at: 1_700_000_000,
            valid_until: 1_731_536_000,
        };
        let rendered = String::from_utf8(render_signing_message("kid-1", &claims))
            .expect("ascii signing message");
        assert_eq!(
            rendered,
            "iotdaq.lease.v1|kid=5:kid-1|lease_id=10:lease-0001|device_id=8:dev-0001|\
             mid=16:a1b2c3d4e5f6a7b8|tier=8:standard|verify_mode=1:B|\
             issued_at=10:1700000000|valid_until=10:1731536000"
                .replace("             ", "")
        );
    }

    /// 大整数常量守护：`JSON_SAFE_INT_MAX` 恰为 2^53 − 1。
    #[test]
    fn json_safe_int_max_is_correct() {
        assert_eq!(JSON_SAFE_INT_MAX, (1i64 << 53) - 1);
        assert_eq!(JSON_SAFE_INT_MAX, 9_007_199_254_740_991);
    }

    /// 状态语义矩阵：采集 / 转发 / 签名三开关在各状态下的取值。
    #[test]
    fn state_capability_matrix() {
        let lease = LeaseToken {
            raw: "raw".to_string(),
            lease_id: "l".to_string(),
            device_id: "d".to_string(),
            kid: "kid-a".to_string(),
            tier: "standard".to_string(),
            verify_mode: VerifyMode::B,
            valid_until: T0 + 1000,
        };
        // 元组：`(状态, 可签名, 可本地采集, 可北向转发)`。
        // 注意：**任何状态**下都允许本地采集（降级 ≠ 停用），故第 3 列恒为 `true`。
        let cases = vec![
            (LicenseState::Unlicensed, false, true, false),
            (LicenseState::Trial { days_left: 2 }, true, true, true),
            (
                LicenseState::Licensed {
                    lease: lease.clone(),
                },
                true,
                true,
                true,
            ),
            (
                LicenseState::Grace {
                    lease: lease.clone(),
                    days_left: 3,
                },
                true,
                true,
                true,
            ),
            (
                LicenseState::Degraded {
                    reason: "x".to_string(),
                },
                false,
                true,
                false,
            ),
        ];
        for (state, sign, capture, forward) in cases {
            assert_eq!(state.can_sign(), sign, "{state:?} can_sign");
            assert_eq!(state.allows_local_capture(), capture, "{state:?} capture");
            assert_eq!(
                state.allows_northbound_forward(),
                forward,
                "{state:?} forward"
            );
        }
    }

    /// Debug 输出不泄漏私钥材料（脱敏守护）。
    #[test]
    fn debug_output_is_redacted() {
        let (client, _fake) = setup();
        let rendered = format!("{client:?}");
        assert!(rendered.contains("LicensingClient"), "{rendered}");
        assert!(rendered.contains("kid_count"), "{rendered}");
        assert!(!rendered.contains("1a1a"), "no key material in Debug");
    }
}

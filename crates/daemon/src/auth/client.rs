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
//! - **服务端响应验签（TOFU）**：激活 / 心跳响应携带响应级 `sig`，客户端验签后才推进状态；
//!   激活响应携带 `server_pubkey` 并被**钉定**（2026-09-25 主理人决策，见
//!   [`RESPONSE_SIG_DOMAIN_ACTIVATION`] 注释），心跳响应用钉定公钥验签——
//!   缺签 / 篡改 / 异钥一律 **fail-closed**（错误返回、状态不推进）。
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

/// 回执语义哈希的二级域前缀（**以 `|` 结尾**）。
///
/// 与 `licensing-server/src/receipt.rs` 的 `RECEIPT_SEMANTIC_DOMAIN` **逐字节一致**；
/// 改任一字节即跨端漂移（回执验签全部失败）。
pub const RECEIPT_SEMANTIC_DOMAIN: &[u8] = b"iotdaq.receipt.semantic.v1|";

// ---- 心跳 / A 档校验 / 激活的签名域（daemon 本地镜像，与服务端逐字节一致） ----
//
// ⚠️ 以下四组常量与函数是 `licensing-server/src/device_auth.rs` 的**逐字节镜像**：
// 域前缀、二级域前缀、`push_len_field` 长度前缀规则、字段顺序**全部必须一致**。
// **改任一字节即跨端漂移**（服务端 `verify_device_signature` 会拒签）。
// 跨端一致性由 `tests/licensing_contract_conformance.rs` 的逐字节比对测试守护。

/// `/heartbeat` 签名域前缀（与服务端 `HEARTBEAT_SIGNING_DOMAIN` 逐字节一致）。
pub const HEARTBEAT_SIGNING_DOMAIN: &str = "iotdaq.heartbeat.v1";

/// `/heartbeat` 语义哈希的二级域前缀（**以 `|` 结尾**，与服务端逐字节一致）。
pub const HEARTBEAT_SEMANTIC_DOMAIN: &[u8] = b"iotdaq.heartbeat.semantic.v1|";

/// `/verify` 签名域前缀（与服务端 `VERIFY_SIGNING_DOMAIN` 逐字节一致）。
pub const VERIFY_SIGNING_DOMAIN: &str = "iotdaq.verify.v1";

/// `/verify` 语义哈希的二级域前缀（**以 `|` 结尾**，与服务端逐字节一致）。
pub const VERIFY_SEMANTIC_DOMAIN: &[u8] = b"iotdaq.verify.semantic.v1|";

/// `/activation` 签名域前缀（与服务端 `ACTIVATION_SIGNING_DOMAIN` 逐字节一致）。
///
/// 服务端 `activate` **不验** `req_sig`，但形状要对；为保持「签名域单源且双端可重建」
/// 的既有纪律，双端共享同一域常量与渲染函数。
pub const ACTIVATION_SIGNING_DOMAIN: &str = "iotdaq.activation.v1";

/// `/activation` 语义哈希的二级域前缀（**以 `|` 结尾**，与服务端逐字节一致）。
pub const ACTIVATION_SEMANTIC_DOMAIN: &[u8] = b"iotdaq.activation.semantic.v1|";

/// `receipt_cursor` 为空时在签名域中占位的字面量（与服务端 `CURSOR_NONE` 一致）。
pub const CURSOR_NONE: &str = "none";

// ---- 服务端响应签名契约（daemon 侧镜像，与 licensing-server 逐字节一致） ----
//
// ⚠️ 以下常量与函数是 `licensing-server/src/service.rs` 响应签名契约的**逐字节镜像**：
// 激活 / 心跳响应携带响应级 `sig`（Ed25519，STANDARD base64），签名对象是
// `{domain}|{lease_id}|{nonce}|{server_time}` 域串（业务语义确定性串，非序列化字节；
// `server_time` 以十进制秒字符串渲染——大整数红线）。跨端一致性由
// `tests/response_signature_conformance.rs` 锁定。
//
// **公钥分发 = TOFU（Trust-On-First-Use），2026-09-25 主理人决策**：设计文档
// （`docs/design/licensing-api.md` §1.1/§1.2）未定义响应级 `sig` 的公钥来源；
// 采用「激活响应携带 `server_pubkey` + 客户端钉定该公钥」方案：激活响应先经
// 携带公钥验签（自洽），通过后钉定；后续心跳响应用钉定公钥验签——异钥 / 缺签 /
// 篡改一律 **fail-closed**（错误返回、状态不推进）。

/// 服务端响应签名域：`/activation` 响应（与服务端 `RESPONSE_SIG_DOMAIN_ACTIVATION`
/// 逐字节一致）。
pub const RESPONSE_SIG_DOMAIN_ACTIVATION: &str = "activation";

/// 服务端响应签名域：`/heartbeat` 响应（与服务端 `RESPONSE_SIG_DOMAIN_HEARTBEAT`
/// 逐字节一致）。
pub const RESPONSE_SIG_DOMAIN_HEARTBEAT: &str = "heartbeat";

/// 渲染服务端响应签名域串（**未哈希**；与服务端 `service::render_response_signing_message`
/// 逐字节一致）。
///
/// 格式：`{domain}|{lease_id}|{nonce}|{server_time}`。
#[must_use]
pub fn render_response_signing_message(
    domain: &str,
    lease_id: &str,
    nonce: &str,
    server_time: i64,
) -> String {
    format!("{domain}|{lease_id}|{nonce}|{server_time}")
}

/// 解码 STANDARD base64 编码的 Ed25519 公钥（32 字节原始形式）。
///
/// # Errors
/// base64 非法 / 长度 ≠ 32 / 非 Ed25519 曲线点 → [`DaemonError::SecurityError`]。
fn decode_ed25519_public_key_b64(
    public_key_b64: &str,
    context: &str,
) -> DaemonResult<VerifyingKey> {
    let raw = B64.decode(public_key_b64.trim()).map_err(|e| {
        DaemonError::SecurityError(format!("{context} public key is not valid base64: {e}"))
    })?;
    let bytes: [u8; ED25519_PUBLIC_KEY_LEN] = raw.as_slice().try_into().map_err(|_| {
        DaemonError::SecurityError(format!(
            "{context} public key length {} != {ED25519_PUBLIC_KEY_LEN}",
            raw.len()
        ))
    })?;
    VerifyingKey::from_bytes(&bytes)
        .map_err(|e| DaemonError::SecurityError(format!("invalid {context} public key: {e}")))
}

/// 校验服务端响应签名（TOFU 公钥；域串双端逐字节一致，契约测试锁定）。
///
/// 校验链：公钥解码 → 域串重建 → Ed25519 验签。任一失败 → [`DaemonError::SecurityError`]。
///
/// # Errors
/// 公钥非法 / 签名 base64 非法 / 验签不通过 → [`DaemonError::SecurityError`]。
pub fn verify_server_response_signature(
    server_pubkey_b64: &str,
    domain: &str,
    lease_id: &str,
    nonce: &str,
    server_time: i64,
    signature_b64: &str,
) -> DaemonResult<()> {
    if server_pubkey_b64.trim().is_empty() {
        return Err(DaemonError::SecurityError(
            "server response signature verification requires a non-empty server public key"
                .to_string(),
        ));
    }
    let key = decode_ed25519_public_key_b64(server_pubkey_b64, "server response")?;
    let message = render_response_signing_message(domain, lease_id, nonce, server_time);
    verify_signature_b64(&key, message.as_bytes(), signature_b64, "server response")
}

/// `/heartbeat` 请求体的字段集合（恰好 5 个，镜像服务端 `HeartbeatRequest`）。
pub const HEARTBEAT_FIELD_WHITELIST: [&str; 5] =
    ["lease_id", "ts", "nonce", "receipt_cursor", "device_sig"];

/// `/verify` 请求体的字段集合（恰好 6 个，**等于服务端 `VERIFY_WHITELIST`**）。
pub const VERIFY_FIELD_WHITELIST: [&str; 6] = [
    "device_mid",
    "lease_id",
    "payload_digest",
    "ts",
    "nonce",
    "device_sig",
];

/// `/activation` 请求体的字段集合（恰好 7 个，镜像服务端 `ActivationRequest`）。
pub const ACTIVATION_FIELD_WHITELIST: [&str; 7] = [
    "activation_code",
    "machine_code",
    "anchor_hashes",
    "device_pubkey",
    "nonce",
    "ts",
    "req_sig",
];

/// 设计口径的锚点数量 **M**（`docs/design/licensing-api.md`：`anchor_hashes[5]`）。
///
/// ⚠️ 服务端**同机判定阈值**是 **≥4/5**（`docs/design/machine-fingerprint.md` §3，N=4/M=5，
/// 允许 1 项合法漂移）；该阈值由**服务端**计算，客户端**不**参与判定 —— 客户端只负责产出并上传
/// 逐锚点哈希集。切勿把 M=5 误当成阈值。
///
/// 仅用于诊断告警：客户端**不强制**该数量（不同平台可暴露的锚点数不同），但数量不等于此值时
/// [`LicensingClient::activate`] 会 `tracing::warn!` 记录实际数量。
pub const ANCHOR_COUNT: usize = 5;

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
    /// 授权服务基址（canonical：`http://license.webscad.cn/licensing`）。
    ///
    /// **服务端路由不带额外前缀**：请求路径为 `{base_url}/{path}`，其中
    /// `path ∈ activate | heartbeat | verify | audit/receipt`；宿主 nginx 收到
    /// `/licensing/*` 后**剥掉前缀**转发到 licensing-server 根路径（如
    /// `http://license.webscad.cn/licensing/activate` → `http://127.0.0.1:9010/activate`）。
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
    /// **TOFU 钉定**的服务端响应签名公钥（原始 32 字节；2026-09-25 主理人决策）。
    ///
    /// 激活响应携带 `server_pubkey`，客户端验签激活响应通过后在此钉定；
    /// 后续心跳响应一律用钉定公钥验签（异钥 / 缺签 / 篡改 → fail-closed）。
    /// 存原始字节而非 `VerifyingKey`：`[u8; 32]` 是 `Copy`，免去对 `Clone` 的依赖，
    /// 使用时再重建（重建失败视为钉定密钥损坏，同样 fail-closed）。
    pinned_server_key: Option<[u8; 32]>,
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
    /// **设备私钥签名者**（可选；未注入时各签名字段置空 → 服务端明确拒绝）。
    ///
    /// 承载全部「服务端可重建验签」的设备签名：`/activation` 的 `req_sig`、
    /// `/heartbeat` 的 `device_sig`、`/verify` 的 `device_sig`、`/audit/receipt` 的 `sig`——
    /// 四者共用**同一把设备私钥**，故名 `device_signer`（历史命名 `receipt_signer` 由
    /// [`LicensingClient::with_receipt_signer`] 兼容保留）。
    ///
    /// 与 [`AuthSigner`] 使用**同一把设备私钥**（生产由宿主注入同一个 `HandleSigner`）；
    /// 之所以单独持有：这些签名必须是**确定性、可被服务端用设备公钥重建验签**的签名，
    /// 而 [`AuthSigner::sign_semantic`] 会掺入随机 nonce（服务端无法重建），故不经其签名。
    device_signer: Option<Arc<dyn HandleSigner>>,
    /// 本机**逐锚点哈希集**（供 `/activation` 的 `anchor_hashes`，承载 N-of-M 同机判定）。
    ///
    /// 由宿主经 [`LicensingClient::with_anchor_hashes`] 注入
    /// （来源：`auth::machine_id::MachineIdentity::get_anchor_hashes`）。
    /// 默认空——此时 [`LicensingClient::activate`] **fail-closed 拒绝激活**，绝不静默退化为 M=1。
    anchor_hashes: Vec<String>,
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
            .field(
                "server_key_pinned",
                &self
                    .inner
                    .lock()
                    .map(|g| g.pinned_server_key.is_some())
                    .unwrap_or(false),
            )
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
            device_signer: None,
            anchor_hashes: Vec::new(),
            public_keys: BTreeMap::new(),
            inner: Mutex::new(ClientInner {
                state: LicenseState::Unlicensed,
                max_observed_secs: i64::MIN,
                grace_anchor_secs: 0,
                trial_anchor_secs: 0,
                trial_active: false,
                last_cursor: None,
                pinned_server_key: None,
            }),
        }
    }

    /// **增量、可选**：注入设备私钥签名者（与 [`AuthSigner`] 用同一把设备私钥）。
    ///
    /// 承载 `/activation` 的 `req_sig`、`/heartbeat` 的 `device_sig`、`/verify` 的 `device_sig`
    /// 与 `/audit/receipt` 的 `sig`——四者共用同一把设备私钥，故名 `device_signer`。
    ///
    /// 未注入时上述签名字段产出**空串**——这是**明确的失败信号**（服务端收到空签名会拒绝），
    /// 绝非「无签名冒充有效签名」；不伪造、不 panic。
    pub fn with_device_signer(mut self, provider: Arc<dyn HandleSigner>) -> Self {
        self.device_signer = Some(provider);
        self
    }

    /// 兼容入口：与 [`LicensingClient::with_device_signer`] 等价。
    ///
    /// 保留历史命名 `receipt_signer` 的注入入口，避免破坏既有调用方（该私钥同时用于回执）。
    pub fn with_receipt_signer(self, provider: Arc<dyn HandleSigner>) -> Self {
        self.with_device_signer(provider)
    }

    /// **增量、可选**：注入本机**逐锚点哈希集**（供 `/activation` 的 `anchor_hashes`）。
    ///
    /// 来源：`crate::auth::machine_id::MachineIdentity::get_anchor_hashes()`（该 API 与机器码
    /// **同键盐化、域分隔**，逐点独立、截断 32 hex）。
    ///
    /// **fail-closed**：未注入（或注入为空）时 [`LicensingClient::activate`] 立即返回
    /// `ConfigError`——**绝不**用机器码冒充单个锚点（那会把服务端 N-of-M 判定静默退化为 M=1，
    /// 削弱一机一码绑定）。
    pub fn with_anchor_hashes(mut self, anchor_hashes: Vec<String>) -> Self {
        self.anchor_hashes = anchor_hashes;
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

    /// TOFU 钉定的服务端响应签名公钥（STANDARD base64；未钉定为 `None`）。
    ///
    /// 供诊断 / 测试；公钥本身是公开信息，不脱敏。
    pub fn pinned_server_key_b64(&self) -> Option<String> {
        self.inner
            .lock()
            .ok()
            .and_then(|g| g.pinned_server_key)
            .map(|bytes| B64.encode(bytes))
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
    /// 请求体**严格等于**服务端 `ActivationRequest` 形状（7 字段）：
    /// `{ activation_code, machine_code, anchor_hashes, device_pubkey, nonce, ts, req_sig }`
    /// （`ts` 为**字符串**，大整数红线）。`req_sig` = 对
    /// [`activation_payload_hash`] 的设备私钥 Ed25519 签名（服务端当前不验，但形状要对）。
    /// 响应体须含 `lease_token` 字段（三段式串）。
    ///
    /// **响应签名校验（TOFU，2026-09-25 主理人决策）**：激活响应携带 `server_pubkey`
    /// 与响应级 `sig`（域串 `activation|{lease_id}|{nonce}|{server_time}`）。客户端先校验
    /// nonce 回显一致，再用携带公钥验签；通过后**钉定**该公钥（后续心跳响应验签用）。
    /// **任一环节失败 → fail-closed**：返回 [`DaemonError::SecurityError`] / [`DaemonError::AuthError`]，
    /// 状态**不推进**（保持 `Unlicensed`），绝不钉定未验证的公钥。
    ///
    /// `anchor_hashes` 取注入的逐锚点哈希集（`with_anchor_hashes`）：**缺失即 fail-closed**。
    ///
    /// # Errors
    /// - 激活码为空 / **未注入锚点哈希集** → `ConfigError`；
    /// - 网络失败 / 超时 → `NetworkError`；
    /// - 服务端拒绝 → `AuthError`；
    /// - 响应缺 `lease_token` / `server_pubkey` / `sig` / `nonce` 回显不符 → `AuthError` / `SecurityError`；
    /// - 响应验签失败 / 本地验签失败 → `SecurityError`。
    pub async fn activate(&self, activation_code: &str) -> DaemonResult<LicenseState> {
        let code = activation_code.trim();
        if code.is_empty() {
            return Err(DaemonError::ConfigError(
                "activation code must not be empty".to_string(),
            ));
        }

        let now = self.observed_now();
        let nonce = uuid::Uuid::new_v4().simple().to_string();

        // **fail-closed**：必须有真实「逐锚点哈希集」。缺失即拒绝激活——用 machine_code 冒充
        // 单个锚点会把服务端 N-of-M 同机判定静默退化为 M=1，削弱一机一码绑定。
        let anchor_hashes = self.anchor_hashes.clone();
        if anchor_hashes.is_empty() {
            return Err(DaemonError::ConfigError(
                "activation requires the per-anchor hash set; sending the machine code as a \
                 single anchor would silently degrade N-of-M to M=1. Inject it via \
                 LicensingClient::with_anchor_hashes (see \
                 auth::machine_id::MachineIdentity::get_anchor_hashes)."
                    .to_string(),
            ));
        }
        // 数量不为设计口径（5）时**不拒绝**（平台可暴露的锚点数不同），仅告警以便诊断。
        if anchor_hashes.len() != ANCHOR_COUNT {
            tracing::warn!(
                anchor_count = anchor_hashes.len(),
                expected = ANCHOR_COUNT,
                "activation anchor hash count differs from the designed ANCHOR_COUNT; not \
                 rejecting (platform may expose fewer anchors than designed)"
            );
        }

        let device_pubkey = self.device_public_key_b64();
        let req_sig = self.sign_payload_hash(&activation_payload_hash(
            code,
            &self.machine_code,
            &anchor_hashes,
            &device_pubkey,
            &nonce,
            now,
        ));

        let body = activation_request_body(
            code,
            &self.machine_code,
            &anchor_hashes,
            &device_pubkey,
            &nonce,
            now,
            &req_sig,
        );
        // 白名单守护：构造出的 request 的 key 集合必须恰好等于 7 字段白名单。
        debug_assert_eq!(
            object_keys(&body).len(),
            ACTIVATION_FIELD_WHITELIST.len(),
            "activation request must have exactly 7 fields"
        );

        let url = self.cfg.endpoint("activate");
        let resp = self.transport.post_json(&url, &body)?;
        let resp = unwrap_api_envelope(&resp);
        let raw = resp
            .get("lease_token")
            .and_then(|v| v.as_str())
            .ok_or_else(|| {
                DaemonError::AuthError(
                    "activate response is missing 'lease_token' field".to_string(),
                )
            })?;

        // ---- 响应签名校验（TOFU）：先于任何状态推进，失败即 fail-closed ----
        let resp_lease_id = resp
            .get("lease_id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| {
                DaemonError::AuthError("activate response is missing 'lease_id' field".to_string())
            })?;
        let server_time = require_secs_field(resp, "server_time", "activation")?;
        let server_pubkey = resp
            .get("server_pubkey")
            .and_then(|v| v.as_str())
            .ok_or_else(|| {
                DaemonError::AuthError(
                    "activate response is missing 'server_pubkey' field (TOFU server public key)"
                        .to_string(),
                )
            })?;
        let resp_sig = resp.get("sig").and_then(|v| v.as_str()).ok_or_else(|| {
            DaemonError::AuthError("activate response is missing 'sig' field".to_string())
        })?;
        // nonce 回显必须与请求一致（防响应串扰；域串本身也绑定了请求 nonce）。
        let echoed = resp.get("nonce").and_then(|v| v.as_str()).ok_or_else(|| {
            DaemonError::AuthError("activate response is missing 'nonce' field".to_string())
        })?;
        if echoed != nonce {
            return Err(DaemonError::SecurityError(
                "activate response nonce does not match the request nonce".to_string(),
            ));
        }
        verify_server_response_signature(
            server_pubkey,
            RESPONSE_SIG_DOMAIN_ACTIVATION,
            resp_lease_id,
            &nonce,
            server_time,
            resp_sig,
        )?;

        // 验签通过 → 钉定服务端公钥（TOFU）。重复激活以最新激活响应重新钉定
        // （激活响应经携带公钥自洽验签；TOFU 的首信边界即首次激活，见模块注释）。
        let pinned = decode_ed25519_public_key_b64(server_pubkey, "server response")?.to_bytes();
        if let Ok(mut guard) = self.inner.lock() {
            guard.pinned_server_key = Some(pinned);
        }

        let lease = self.verify_lease_locally(raw)?;
        self.set_state(LicenseState::Licensed {
            lease: lease.clone(),
        });
        Ok(LicenseState::Licensed { lease })
    }

    /// 24h 心跳。B 档必须携带最近回执序号区间（`cursor = Some((seq_from, seq_to))`）。
    ///
    /// 请求体**严格等于**服务端 `HeartbeatRequest` 形状（5 字段）：
    /// `{ lease_id, ts, nonce, receipt_cursor, device_sig }`；`receipt_cursor` 在无游标时为
    /// `null`，有游标时为**嵌套对象** `{seq_from, seq_to}`（两侧均为字符串，大整数红线）。
    /// `device_sig` = 对 [`heartbeat_payload_hash`] 的设备私钥 Ed25519 签名。
    ///
    /// **服务端心跳响应不含新 Token**（`HeartbeatResponse` =
    /// `{server_time, next_deadline, valid_until, verify_mode, tier, sig}`），
    /// 故本方法按 `valid_until` / `server_time` 更新状态，**不再要求响应携带 `lease_token`**。
    ///
    /// **响应签名校验（TOFU 钉定公钥）**：心跳响应的 `sig` 域串为
    /// `heartbeat|{lease_id}|{nonce}|{server_time}`，用**激活时钉定**的服务端公钥验签。
    /// **验签失败（缺签 / 篡改 / 异钥）或未钉定 → fail-closed**：返回错误，
    /// 状态机**不推进**（`valid_until` 不刷新、宽限基准不动、时钟校准不生效）——
    /// 未经验证的服务端时间绝不可信。
    ///
    /// **关键**：网络失败**不得**降级——错误直接返回给调用方；状态机只在
    /// **宽限期耗尽**时才降级。B 档断网时由调用方继续采集与转发，稍后补报。
    ///
    /// # Errors
    /// 网络失败 / 超时 → `NetworkError`；服务端拒绝 / 响应缺字段 → `AuthError`；无租约 → `AuthError`；
    /// 响应验签失败 / 未钉定服务端公钥 → `SecurityError`。
    pub async fn heartbeat(&self, cursor: Option<(i64, i64)>) -> DaemonResult<LicenseState> {
        let lease = self.active_lease().ok_or_else(|| {
            DaemonError::AuthError("heartbeat requires an activated lease".to_string())
        })?;

        let now = self.observed_now();
        let nonce = uuid::Uuid::new_v4().simple().to_string();
        let device_sig = self.sign_payload_hash(&heartbeat_payload_hash(
            &lease.lease_id,
            now,
            &nonce,
            cursor,
        ));

        let body = heartbeat_request_body(&lease.lease_id, now, &nonce, cursor, &device_sig);
        debug_assert_eq!(
            object_keys(&body).len(),
            HEARTBEAT_FIELD_WHITELIST.len(),
            "heartbeat request must have exactly 5 fields"
        );

        let url = self.cfg.endpoint("heartbeat");
        let resp = self.transport.post_json(&url, &body)?;

        // 服务端权威时间与租约失效时刻（均为秒字符串）。
        let server_time = require_secs_field(&resp, "server_time", "heartbeat")?;
        let server_valid_until = require_secs_field(&resp, "valid_until", "heartbeat")?;

        // ---- 响应签名校验（TOFU 钉定公钥）：先于任何状态推进，失败即 fail-closed ----
        let pinned = self.inner.lock().ok().and_then(|g| g.pinned_server_key);
        let pinned = pinned.ok_or_else(|| {
            DaemonError::SecurityError(
                "heartbeat response rejected: no server public key pinned \
                 (activate first to pin the server key via TOFU)"
                    .to_string(),
            )
        })?;
        let pinned_key = VerifyingKey::from_bytes(&pinned).map_err(|e| {
            DaemonError::SecurityError(format!(
                "pinned server public key is no longer a valid Ed25519 point: {e}"
            ))
        })?;
        let resp_sig = resp.get("sig").and_then(|v| v.as_str()).ok_or_else(|| {
            DaemonError::AuthError("heartbeat response is missing 'sig' field".to_string())
        })?;
        let message = render_response_signing_message(
            RESPONSE_SIG_DOMAIN_HEARTBEAT,
            &lease.lease_id,
            &nonce,
            server_time,
        );
        verify_signature_b64(&pinned_key, message.as_bytes(), resp_sig, "server response")?;

        // 以服务端权威 `valid_until` 刷新本地租约有效期（服务端在激活时固定该值，心跳回显；
        // 若两者漂移，以服务端为准）。`raw` 原文保留不变（审计与再验签用）。
        let mut updated = lease;
        updated.valid_until = server_valid_until;

        // 心跳成功 → 视为「已联网」：以服务端时间校准单调时钟，刷新宽限基准，回到 Licensed。
        // 时钟校准仅**前进**（`max`），不因服务端报出更早时间而后退——回拨防御不变。
        if let Ok(mut guard) = self.inner.lock() {
            if server_time > guard.max_observed_secs {
                guard.max_observed_secs = server_time;
            }
            guard.grace_anchor_secs = guard.max_observed_secs;
            guard.last_cursor = cursor;
            guard.state = LicenseState::Licensed {
                lease: updated.clone(),
            };
        }
        Ok(LicenseState::Licensed { lease: updated })
    }

    // ---- A 档二次校验 ----

    /// A 档业务消息级二次校验（`POST /verify`，**仅 A 档**）。
    ///
    /// 请求体**严格等于**服务端 `VerifyRequest` 形状（6 字段，**出现白名单外字段即 422**）：
    /// `{ device_mid, lease_id, payload_digest, ts, nonce, device_sig }`（`ts` 为字符串）。
    /// `device_sig` = 对 [`verify_payload_hash`] 的设备私钥 Ed25519 签名。
    ///
    /// **响应必须回显请求 nonce**：`VerifyResponse{ok, server_time, nonce}`；回显不一致 →
    /// [`DaemonError::SecurityError`]（防响应重放 / 串扰）。返回服务端判定的 `ok`。
    ///
    /// # Errors
    /// - 无租约（Trial / Unlicensed / Degraded）→ `AuthError`；
    /// - 网络失败 → `NetworkError`；响应缺字段 → `AuthError`；
    /// - nonce 回显不一致 → `SecurityError`。
    pub async fn verify(&self, payload_digest: &str) -> DaemonResult<bool> {
        let lease = self.active_lease().ok_or_else(|| {
            DaemonError::AuthError("verify requires an activated lease".to_string())
        })?;

        let now = self.observed_now();
        let nonce = uuid::Uuid::new_v4().simple().to_string();
        let device_sig = self.sign_payload_hash(&verify_payload_hash(
            &self.machine_code,
            &lease.lease_id,
            payload_digest,
            now,
            &nonce,
        ));

        let body = verify_request_body(
            &self.machine_code,
            &lease.lease_id,
            payload_digest,
            now,
            &nonce,
            &device_sig,
        );
        debug_assert_eq!(
            object_keys(&body).len(),
            VERIFY_FIELD_WHITELIST.len(),
            "verify request must have exactly 6 fields"
        );

        let url = self.cfg.endpoint("verify");
        let resp = self.transport.post_json(&url, &body)?;

        let ok = resp.get("ok").and_then(|v| v.as_bool()).ok_or_else(|| {
            DaemonError::AuthError("verify response is missing 'ok' field".to_string())
        })?;
        let echoed = resp.get("nonce").and_then(|v| v.as_str()).ok_or_else(|| {
            DaemonError::AuthError("verify response is missing 'nonce' field".to_string())
        })?;
        if echoed != nonce {
            return Err(DaemonError::SecurityError(
                "verify response nonce does not match the request nonce".to_string(),
            ));
        }
        Ok(ok)
    }

    // ---- 内部辅助 ----

    /// 当前授权状态所持有的租约（`Licensed` / `Grace` 持有；其余为 `None`）。
    fn active_lease(&self) -> Option<LeaseToken> {
        match self.snapshot() {
            LicenseState::Licensed { lease } | LicenseState::Grace { lease, .. } => Some(lease),
            _ => None,
        }
    }

    /// 设备公钥 STANDARD base64（从注入的 [`HandleSigner`] 导出）。
    ///
    /// 密钥不可用（未注入 / 导出失败）时返回**空串**——明确的失败信号，不伪造、不 panic。
    fn device_public_key_b64(&self) -> String {
        match &self.device_signer {
            Some(p) => match p.public_key() {
                Ok(vk) => B64.encode(vk.to_bytes()),
                Err(_) => String::new(),
            },
            None => String::new(),
        }
    }

    /// 对 32 字节待验数据用设备私钥签名（STANDARD base64）。
    ///
    /// 密钥不可用（未注入 / 签名失败）时返回**空串**——明确的失败信号（服务端拒绝），
    /// 绝不用「无签名」冒充有效签名；生产路径零 panic。
    fn sign_payload_hash(&self, payload_hash: &[u8; 32]) -> String {
        let provider = match &self.device_signer {
            Some(p) => p,
            // 未注入设备签名密钥：置空（明确失败信号）。
            None => return String::new(),
        };
        match provider.sign_message(payload_hash) {
            Ok(signature) => B64.encode(signature.to_bytes()),
            // 密钥不可用：置空（明确失败信号）。
            Err(_) => String::new(),
        }
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
        self.sign_payload_hash(&payload_hash)
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
///
/// 回执的二级域前缀固定为 [`RECEIPT_SEMANTIC_DOMAIN`]，经 [`domain_separated_digest`] 计算，
/// **行为与历史实现逐字节一致**（回执路径不可回归）。
fn semantic_digest(bytes: &[u8]) -> [u8; 32] {
    domain_separated_digest(RECEIPT_SEMANTIC_DOMAIN, bytes)
}

/// 通用「二级域前缀 + 数据」SHA-256：`SHA-256(domain ++ bytes)`。
///
/// 用于心跳 / 校验 / 激活等**参数化二级域**的语义哈希；`domain` 由调用方给定
/// （回执走 [`RECEIPT_SEMANTIC_DOMAIN`]，心跳走 [`HEARTBEAT_SEMANTIC_DOMAIN`]，……）。
/// 单一实现保证「先哈希、对哈希签」的口径在所有端点一致。
fn domain_separated_digest(domain: &[u8], bytes: &[u8]) -> [u8; 32] {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(domain);
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

// ---- 心跳 / 校验 / 激活的签名域渲染（与服务端 device_auth.rs 逐字节一致） ----

/// 渲染 `/heartbeat` 签名域串（**未哈希**；与服务端 `render_heartbeat_signing_message` 逐字节一致）。
///
/// 形状（5 字段，顺序固定）：
/// ```text
/// iotdaq.heartbeat.v1|lease_id=<len>:<id>|ts=<len>:<ts>|nonce=<len>:<n>|cursor_from=<len>:<f>|cursor_to=<len>:<t>
/// ```
/// `cursor` 为 `Some((from, to))` 时以十进制落域；`None` 时两端均为 [`CURSOR_NONE`]（`"none"`）。
#[must_use]
pub fn render_heartbeat_signing_message(
    lease_id: &str,
    ts: i64,
    nonce: &str,
    cursor: Option<(i64, i64)>,
) -> String {
    let (from, to) = match cursor {
        Some((f, t)) => (f.to_string(), t.to_string()),
        None => (CURSOR_NONE.to_string(), CURSOR_NONE.to_string()),
    };
    let mut out = String::with_capacity(160);
    out.push_str(HEARTBEAT_SIGNING_DOMAIN);
    push_len_field(&mut out, "lease_id", lease_id);
    push_len_field(&mut out, "ts", &ts.to_string());
    push_len_field(&mut out, "nonce", nonce);
    push_len_field(&mut out, "cursor_from", &from);
    push_len_field(&mut out, "cursor_to", &to);
    out
}

/// `/heartbeat` 待验数据：`SHA-256(HEARTBEAT_SEMANTIC_DOMAIN ++ 域串)`（32 字节）。
///
/// 与服务端 `device_auth::heartbeat_payload_hash` 逐字节一致。
#[must_use]
pub fn heartbeat_payload_hash(
    lease_id: &str,
    ts: i64,
    nonce: &str,
    cursor: Option<(i64, i64)>,
) -> [u8; 32] {
    let message = render_heartbeat_signing_message(lease_id, ts, nonce, cursor);
    domain_separated_digest(HEARTBEAT_SEMANTIC_DOMAIN, message.as_bytes())
}

/// 渲染 `/verify` 签名域串（**未哈希**；与服务端 `render_verify_signing_message` 逐字节一致）。
///
/// 形状（5 字段，顺序固定）：
/// ```text
/// iotdaq.verify.v1|mid=<len>:<mid>|lease_id=<len>:<id>|payload_digest=<len>:<d>|ts=<len>:<ts>|nonce=<len>:<n>
/// ```
#[must_use]
pub fn render_verify_signing_message(
    device_mid: &str,
    lease_id: &str,
    payload_digest: &str,
    ts: i64,
    nonce: &str,
) -> String {
    let mut out = String::with_capacity(160);
    out.push_str(VERIFY_SIGNING_DOMAIN);
    push_len_field(&mut out, "mid", device_mid);
    push_len_field(&mut out, "lease_id", lease_id);
    push_len_field(&mut out, "payload_digest", payload_digest);
    push_len_field(&mut out, "ts", &ts.to_string());
    push_len_field(&mut out, "nonce", nonce);
    out
}

/// `/verify` 待验数据：`SHA-256(VERIFY_SEMANTIC_DOMAIN ++ 域串)`（32 字节）。
///
/// 与服务端 `device_auth::verify_payload_hash` 逐字节一致。
#[must_use]
pub fn verify_payload_hash(
    device_mid: &str,
    lease_id: &str,
    payload_digest: &str,
    ts: i64,
    nonce: &str,
) -> [u8; 32] {
    let message = render_verify_signing_message(device_mid, lease_id, payload_digest, ts, nonce);
    domain_separated_digest(VERIFY_SEMANTIC_DOMAIN, message.as_bytes())
}

/// 云端响应信封解包：licensing-server 统一以 `{code:"OK", data:{...}, message}`
/// 包裹成功负载（`http.rs ApiEnvelope::ok`）。设备端解析（激活 / 心跳）需要
/// 的是 `data` 内的裸负载——本函数在 `code=="OK"` 且携带 `data` 对象时返回
/// `data`，否则原样返回（兼容无信封的裸负载与测试 fake server，双形态皆收）。
fn unwrap_api_envelope(resp: &serde_json::Value) -> &serde_json::Value {
    match resp.get("code").and_then(|v| v.as_str()) {
        Some("OK") if resp.get("data").is_some_and(|d| d.is_object()) => {
            resp.get("data").unwrap_or(resp)
        }
        _ => resp,
    }
}

/// 渲染 `/activation` 签名域串（**未哈希**；与服务端 `render_activation_signing_message` 逐字节一致）。
///
/// 形状（字段顺序固定；`anchor_hashes` 用 `anchors=<count>` + 逐个 `anchor_<i>` 定界编码，
/// 保证空集与含 `|` 的值都不产生歧义）：
/// ```text
/// iotdaq.activation.v1|activation_code=<len>:<c>|machine_code=<len>:<m>|anchors=<len>:<n>|
///   anchor_0=<len>:<a0>|...|device_pubkey=<len>:<pk>|nonce=<len>:<n>|ts=<len>:<ts>
/// ```
#[must_use]
pub fn render_activation_signing_message(
    activation_code: &str,
    machine_code: &str,
    anchor_hashes: &[String],
    device_pubkey: &str,
    nonce: &str,
    ts: i64,
) -> String {
    let mut out = String::with_capacity(192);
    out.push_str(ACTIVATION_SIGNING_DOMAIN);
    push_len_field(&mut out, "activation_code", activation_code);
    push_len_field(&mut out, "machine_code", machine_code);
    push_len_field(&mut out, "anchors", &anchor_hashes.len().to_string());
    for (i, anchor) in anchor_hashes.iter().enumerate() {
        push_len_field(&mut out, &format!("anchor_{i}"), anchor);
    }
    push_len_field(&mut out, "device_pubkey", device_pubkey);
    push_len_field(&mut out, "nonce", nonce);
    push_len_field(&mut out, "ts", &ts.to_string());
    out
}

/// `/activation` 待验数据：`SHA-256(ACTIVATION_SEMANTIC_DOMAIN ++ 域串)`（32 字节）。
///
/// 与服务端 `device_auth::activation_payload_hash` 逐字节一致。
#[must_use]
pub fn activation_payload_hash(
    activation_code: &str,
    machine_code: &str,
    anchor_hashes: &[String],
    device_pubkey: &str,
    nonce: &str,
    ts: i64,
) -> [u8; 32] {
    let message = render_activation_signing_message(
        activation_code,
        machine_code,
        anchor_hashes,
        device_pubkey,
        nonce,
        ts,
    );
    domain_separated_digest(ACTIVATION_SEMANTIC_DOMAIN, message.as_bytes())
}

// ---- 请求体构造（纯函数；形状即服务端 proto 结构，供跨端一致性测试断言） ----

/// 构造 `/heartbeat` 请求体（**严格等于服务端 `HeartbeatRequest` 形状**，恰好 5 字段）。
///
/// `ts` 为秒字符串；`receipt_cursor` 无游标时为 `null`，有游标时为嵌套对象
/// `{seq_from, seq_to}`（均为秒/序号字符串，大整数红线）。`device_sig` 为标准 base64。
#[must_use]
pub fn heartbeat_request_body(
    lease_id: &str,
    ts: i64,
    nonce: &str,
    cursor: Option<(i64, i64)>,
    device_sig: &str,
) -> serde_json::Value {
    let receipt_cursor = match cursor {
        Some((from, to)) => serde_json::json!({
            "seq_from": from.to_string(),
            "seq_to": to.to_string(),
        }),
        None => serde_json::Value::Null,
    };
    serde_json::json!({
        "lease_id": lease_id,
        "ts": ts.to_string(),
        "nonce": nonce,
        "receipt_cursor": receipt_cursor,
        "device_sig": device_sig,
    })
}

/// 构造 `/verify` 请求体（**严格等于服务端 `VerifyRequest` 形状**，恰好 6 字段）。
///
/// 出现白名单外字段服务端即 422；`ts` 为秒字符串。
#[must_use]
pub fn verify_request_body(
    device_mid: &str,
    lease_id: &str,
    payload_digest: &str,
    ts: i64,
    nonce: &str,
    device_sig: &str,
) -> serde_json::Value {
    serde_json::json!({
        "device_mid": device_mid,
        "lease_id": lease_id,
        "payload_digest": payload_digest,
        "ts": ts.to_string(),
        "nonce": nonce,
        "device_sig": device_sig,
    })
}

/// 构造 `/activation` 请求体（**严格等于服务端 `ActivationRequest` 形状**，恰好 7 字段）。
///
/// `ts` 为秒字符串；`anchor_hashes` 为字符串数组；`req_sig` 为标准 base64。
#[must_use]
pub fn activation_request_body(
    activation_code: &str,
    machine_code: &str,
    anchor_hashes: &[String],
    device_pubkey: &str,
    nonce: &str,
    ts: i64,
    req_sig: &str,
) -> serde_json::Value {
    serde_json::json!({
        "activation_code": activation_code,
        "machine_code": machine_code,
        "anchor_hashes": anchor_hashes,
        "device_pubkey": device_pubkey,
        "nonce": nonce,
        "ts": ts.to_string(),
        "req_sig": req_sig,
    })
}

/// 读取服务端响应中的「秒字符串」字段并解析为 `i64`（大整数走字符串红线）。
///
/// # Errors
/// 字段缺失（非字符串）或不是合法 `i64` 十进制字符串 → [`DaemonError::AuthError`]。
fn require_secs_field(resp: &serde_json::Value, field: &str, context: &str) -> DaemonResult<i64> {
    let raw = resp.get(field).and_then(|v| v.as_str()).ok_or_else(|| {
        DaemonError::AuthError(format!("{context} response is missing '{field}' field"))
    })?;
    raw.trim().parse::<i64>().map_err(|_| {
        DaemonError::AuthError(format!(
            "{context} response field '{field}' is not integer seconds"
        ))
    })
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

/// Ed25519 验签（原始 64 字节签名，STANDARD base64；Lease Token 场景包装）。
///
/// # Errors
/// base64 非法 / 签名长度 ≠ 64 / 验签不通过 → [`DaemonError::SecurityError`]。
fn verify_ed25519(key: &VerifyingKey, message: &[u8], signature_b64: &str) -> DaemonResult<()> {
    verify_signature_b64(key, message, signature_b64, "lease token")
}

/// 通用 Ed25519 验签（原始 64 字节签名，STANDARD base64）。
///
/// `context` 进入错误消息（如 `lease token` / `server response`），便于定位失败环节。
///
/// # Errors
/// base64 非法 / 签名长度 ≠ 64 / 验签不通过 → [`DaemonError::SecurityError`]。
fn verify_signature_b64(
    key: &VerifyingKey,
    message: &[u8],
    signature_b64: &str,
    context: &str,
) -> DaemonResult<()> {
    use ed25519_dalek::Verifier as _;
    let sig_bytes = B64.decode(signature_b64.trim()).map_err(|e| {
        DaemonError::SecurityError(format!("{context} signature is not valid base64: {e}"))
    })?;
    let sig_array: [u8; ED25519_SIGNATURE_LEN] = sig_bytes.as_slice().try_into().map_err(|_| {
        DaemonError::SecurityError(format!(
            "{context} signature length {} != {ED25519_SIGNATURE_LEN}",
            sig_bytes.len()
        ))
    })?;
    let signature = Signature::from_bytes(&sig_array);
    key.verify(message, &signature).map_err(|e| {
        DaemonError::SecurityError(format!("{context} signature verification failed: {e}"))
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

    /// 便捷：5 个测试用逐锚点哈希（32 hex，互不相同；**仅测试**）。
    fn test_anchor_hashes() -> Vec<String> {
        (0..ANCHOR_COUNT).map(|i| format!("{i:032x}")).collect()
    }

    /// 构造带公钥集（A / B 两把 kid）的客户端；传输来自共享的 [`FakeTransport`]。
    ///
    /// 注入回执签名密钥（私钥 A，与 [`test_signer`] 同一把设备私钥），
    /// 并注入逐锚点哈希集（使 `activate` 的 fail-closed 不触发）。
    fn client_with_fake(fake: &Arc<FakeTransport>) -> LicensingClient {
        let cfg = LicensingClientConfig::new("https://licensing.test/v1", 24, 10, 30)
            .expect("non-empty url");
        let signer = test_signer();
        let transport: Arc<dyn LicenseTransport> = Arc::clone(fake) as Arc<dyn LicenseTransport>;
        let mut client = LicensingClient::with_transport(cfg, signer, test_mid(), transport)
            .with_receipt_signer(Arc::new(StaticKeyProvider::new(TEST_ONLY_KEY_A)))
            .with_anchor_hashes(test_anchor_hashes());
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
        let mut client = LicensingClient::with_transport(cfg, signer, test_mid(), transport)
            .with_anchor_hashes(test_anchor_hashes());
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

    // ---- 带服务端响应签名的测试响应构造（TOFU） ----

    /// **test-only** 服务端 Ed25519 私钥种子（响应签名用；**仅测试，禁止真实部署**）。
    const TEST_ONLY_SERVER_SEED: [u8; 32] = [0x5du8; 32];

    /// TEST_ONLY 私钥种子 → 服务端公钥 STANDARD base64（激活响应 `server_pubkey` 字段）。
    fn server_pubkey_b64(seed: [u8; 32]) -> String {
        B64.encode(SigningKey::from_bytes(&seed).verifying_key().to_bytes())
    }

    /// 服务端响应签名（测试侧按契约域串现算；与 daemon 验签函数逐字节一致）。
    fn sign_server_response(
        seed: [u8; 32],
        domain: &str,
        lease_id: &str,
        nonce: &str,
        server_time: i64,
    ) -> String {
        let message = render_response_signing_message(domain, lease_id, nonce, server_time);
        B64.encode(
            SigningKey::from_bytes(&seed)
                .sign(message.as_bytes())
                .to_bytes(),
        )
    }

    /// 构造**带服务端响应签名**的激活成功响应（TOFU：`server_pubkey` + `sig`）。
    ///
    /// `sig` 用 `seed` 私钥对 `activation|lease-0001|<请求 nonce>|<server_time>` 签名；
    /// `pubkey_override` 模拟「sig 与公钥不同源」（错钥负例）；`omit` 剔除字段（缺签负例）。
    /// `lease_id` / `tier` / `verify_mode` / `valid_until` 与 [`build_token`] 的默认租约一致。
    #[allow(clippy::too_many_arguments)]
    fn signed_activation_response(
        req: &serde_json::Value,
        token: &str,
        seed: [u8; 32],
        tamper_sig: bool,
        pubkey_override: Option<String>,
        omit: &[&str],
    ) -> serde_json::Value {
        let nonce = req
            .get("nonce")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();
        let server_time = T0;
        let mut sig = sign_server_response(
            seed,
            RESPONSE_SIG_DOMAIN_ACTIVATION,
            "lease-0001",
            &nonce,
            server_time,
        );
        if tamper_sig {
            // 篡改签名首字节（base64 安全，不影响解析）。
            sig.replace_range(0..1, if sig.starts_with('A') { "B" } else { "A" });
        }
        let mut body = serde_json::json!({
            "lease_id": "lease-0001",
            "lease_token": token,
            "verify_mode": "B",
            "tier": "standard",
            "valid_until": (T0 + 365 * SECS_PER_DAY).to_string(),
            "heartbeat_hours": 24,
            "server_time": server_time.to_string(),
            "nonce": nonce,
            "server_pubkey": pubkey_override.unwrap_or_else(|| server_pubkey_b64(seed)),
            "sig": sig,
        });
        if let Some(obj) = body.as_object_mut() {
            for key in omit {
                obj.remove(*key);
            }
        }
        body
    }

    /// 构造**带服务端响应签名**的心跳成功响应（`sig` 域串 `heartbeat|lease-0001|<nonce>|<ts>`）。
    ///
    /// `sig` 用 `seed` 私钥现算（`seed` ≠ 激活时钉定种子即「异钥」负例）。
    fn signed_heartbeat_response(
        req: &serde_json::Value,
        seed: [u8; 32],
        server_time: i64,
        valid_until: i64,
    ) -> serde_json::Value {
        let nonce = req
            .get("nonce")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();
        serde_json::json!({
            "server_time": server_time.to_string(),
            "next_deadline": (server_time + 86_400).to_string(),
            "valid_until": valid_until.to_string(),
            "verify_mode": "B",
            "tier": "standard",
            "sig": sign_server_response(
                seed,
                RESPONSE_SIG_DOMAIN_HEARTBEAT,
                "lease-0001",
                &nonce,
                server_time,
            ),
        })
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
        /// 返回**动态计算**的成功响应（请求到达时以请求体现算——响应签名需要请求 nonce）。
        Dyn(DynResponder),
        /// 返回网络错误（模拟断网 / 超时）。
        NetFail(String),
        /// 返回服务端拒绝（AuthError）。
        Reject(String),
        /// 回显请求 nonce 的 `/verify` 成功响应（供测试 nonce 回显校验）。
        VerifyEcho {
            /// 服务端判定的 `ok`。
            ok: bool,
        },
        /// 未编程（默认）。
        #[default]
        Unset,
    }

    /// 动态响应闭包：入参 `(url, request_body)`，返回响应体。
    type DynResponder = Box<dyn Fn(&str, &serde_json::Value) -> serde_json::Value + Send>;

    impl FakeTransport {
        /// 编程：下一次请求返回给定响应体。
        fn respond_with(&self, value: serde_json::Value) {
            *self.action.lock().expect("fake action lock") = FakeAction::Ok(value);
        }

        /// 编程：下一次请求返回动态计算的响应（闭包入参：URL、请求体）。
        fn respond_dyn(&self, f: DynResponder) {
            *self.action.lock().expect("fake action lock") = FakeAction::Dyn(f);
        }

        /// 编程：下一次激活请求返回**带 TOFU 服务端签名**的成功响应（契约默认形状）。
        fn respond_with_signed_activation(&self, token: &str, seed: [u8; 32]) {
            self.respond_with_activation_variants(token, seed, false, None, &[]);
        }

        /// 编程：激活响应变体（负例：篡改 `sig` / `server_pubkey` 与签名不同源 / 剔除字段）。
        fn respond_with_activation_variants(
            &self,
            token: &str,
            seed: [u8; 32],
            tamper_sig: bool,
            pubkey_override: Option<String>,
            omit: &'static [&'static str],
        ) {
            let token = token.to_string();
            self.respond_dyn(Box::new(move |_url, req| {
                signed_activation_response(
                    req,
                    &token,
                    seed,
                    tamper_sig,
                    pubkey_override.clone(),
                    omit,
                )
            }));
        }

        /// 编程：下一次心跳请求返回**带服务端签名**的成功响应（`seed` ≠ 钉定种子即异钥负例）。
        fn respond_with_signed_heartbeat(
            &self,
            seed: [u8; 32],
            server_time: i64,
            valid_until: i64,
        ) {
            self.respond_dyn(Box::new(move |_url, req| {
                signed_heartbeat_response(req, seed, server_time, valid_until)
            }));
        }

        /// 编程：下一次 `/verify` 请求返回**回显请求 nonce** 的成功响应。
        fn respond_with_verify_echo(&self, ok: bool) {
            *self.action.lock().expect("fake action lock") = FakeAction::VerifyEcho { ok };
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
                FakeAction::Dyn(f) => Ok(f(url, body)),
                FakeAction::NetFail(m) => Err(DaemonError::NetworkError(m.clone())),
                FakeAction::Reject(m) => Err(DaemonError::AuthError(m.clone())),
                FakeAction::VerifyEcho { ok } => {
                    let echoed = body
                        .get("nonce")
                        .cloned()
                        .unwrap_or(serde_json::Value::Null);
                    Ok(serde_json::json!({
                        "ok": ok,
                        "server_time": "1700000000",
                        "nonce": echoed,
                    }))
                }
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
        fake.respond_with_signed_activation(&token, TEST_ONLY_SERVER_SEED);

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

    // ---- 服务端响应签名（TOFU）：正例 ----

    /// TOFU 正例：合法签名的激活响应 → `Licensed`，且服务端公钥被钉定（后续心跳用）。
    #[tokio::test]
    async fn activate_success_pins_server_key() {
        let (client, fake) = setup();
        let token = build_token(TEST_ONLY_KEY_A, "kid-a", "B", T0, T0 + 365 * 86_400);
        fake.respond_with_signed_activation(&token, TEST_ONLY_SERVER_SEED);

        client
            .activate("ACT-1")
            .await
            .expect("activation must succeed");
        assert_eq!(client.current_state().name(), "Licensed");
        // 钉定的公钥 == 激活响应携带的公钥（由同一把 TEST_ONLY 服务端私钥派生）。
        assert_eq!(
            client.pinned_server_key_b64().as_deref(),
            Some(server_pubkey_b64(TEST_ONLY_SERVER_SEED).as_str()),
            "activation response server_pubkey must be pinned (TOFU)"
        );
    }

    /// 心跳正例：钉定公钥验签通过 → 状态推进（`valid_until` 以服务端权威值刷新）。
    #[tokio::test]
    async fn heartbeat_with_valid_server_signature_advances_state() {
        let (client, fake) = setup();
        let token = build_token(TEST_ONLY_KEY_A, "kid-a", "B", T0, T0 + 365 * 86_400);
        fake.respond_with_signed_activation(&token, TEST_ONLY_SERVER_SEED);
        client.activate("ACT-1").await.expect("activate ok");

        // 服务端报出更长的有效期（比 Token 原值 +35 天）。
        let extended = T0 + 400 * SECS_PER_DAY;
        fake.respond_with_signed_heartbeat(TEST_ONLY_SERVER_SEED, T0, extended);
        let state = client
            .heartbeat(None)
            .await
            .expect("heartbeat must succeed");
        match &state {
            LicenseState::Licensed { lease } => {
                assert_eq!(lease.valid_until, extended, "valid_until must be refreshed");
            }
            other => panic!("expected Licensed, got {other:?}"),
        }
        assert_eq!(client.current_state().name(), "Licensed");
    }

    // ---- 服务端响应签名（TOFU）：激活负例（fail-closed，状态不推进） ----

    /// 激活响应 `sig` 被篡改 → SecurityError，状态保持 `Unlicensed`，**不钉定**任何公钥。
    #[tokio::test]
    async fn activate_tampered_response_sig_is_fail_closed() {
        let (client, fake) = setup();
        let token = build_token(TEST_ONLY_KEY_A, "kid-a", "B", T0, T0 + 365 * 86_400);
        fake.respond_with_activation_variants(&token, TEST_ONLY_SERVER_SEED, true, None, &[]);

        let err = client
            .activate("ACT-1")
            .await
            .expect_err("tampered sig must fail");
        assert_eq!(err.error_code(), ERR_SECURITY);
        assert!(err.to_string().contains("server response"), "{err}");
        assert_eq!(client.current_state(), LicenseState::Unlicensed);
        assert!(
            client.pinned_server_key_b64().is_none(),
            "must not pin an unverified key"
        );
    }

    /// 错钥负例：`sig` 用 A 签、`server_pubkey` 报 B → 验签失败，fail-closed。
    #[tokio::test]
    async fn activate_response_sig_with_mismatched_pubkey_is_rejected() {
        let (client, fake) = setup();
        let token = build_token(TEST_ONLY_KEY_A, "kid-a", "B", T0, T0 + 365 * 86_400);
        let rogue_pubkey = server_pubkey_b64(TEST_ONLY_KEY_B);
        fake.respond_with_activation_variants(
            &token,
            TEST_ONLY_SERVER_SEED,
            false,
            Some(rogue_pubkey),
            &[],
        );

        let err = client
            .activate("ACT-1")
            .await
            .expect_err("mismatched key must fail");
        assert_eq!(err.error_code(), ERR_SECURITY);
        assert_eq!(client.current_state(), LicenseState::Unlicensed);
        assert!(client.pinned_server_key_b64().is_none());
    }

    /// 激活响应缺 `sig` → fail-closed（不 panic，状态不推进）。
    #[tokio::test]
    async fn activate_response_missing_sig_is_fail_closed() {
        let (client, fake) = setup();
        let token = build_token(TEST_ONLY_KEY_A, "kid-a", "B", T0, T0 + 365 * 86_400);
        fake.respond_with_activation_variants(&token, TEST_ONLY_SERVER_SEED, false, None, &["sig"]);

        let err = client
            .activate("ACT-1")
            .await
            .expect_err("missing sig must fail");
        assert_eq!(err.error_code(), ERR_AUTH);
        assert!(err.to_string().contains("'sig'"), "{err}");
        assert_eq!(client.current_state(), LicenseState::Unlicensed);
    }

    /// 激活响应缺 `server_pubkey` → fail-closed（TOFU 无公钥即无信任锚）。
    #[tokio::test]
    async fn activate_response_missing_server_pubkey_is_fail_closed() {
        let (client, fake) = setup();
        let token = build_token(TEST_ONLY_KEY_A, "kid-a", "B", T0, T0 + 365 * 86_400);
        fake.respond_with_activation_variants(
            &token,
            TEST_ONLY_SERVER_SEED,
            false,
            None,
            &["server_pubkey"],
        );

        let err = client
            .activate("ACT-1")
            .await
            .expect_err("missing pubkey must fail");
        assert_eq!(err.error_code(), ERR_AUTH);
        assert!(err.to_string().contains("server_pubkey"), "{err}");
        assert_eq!(client.current_state(), LicenseState::Unlicensed);
    }

    // ---- 服务端响应签名（TOFU）：心跳负例（fail-closed，状态不推进） ----

    /// 心跳响应 `sig` 被篡改 → SecurityError，状态机**不推进**（`valid_until` 不刷新）。
    #[tokio::test]
    async fn heartbeat_tampered_response_sig_does_not_advance_state() {
        let (client, fake) = setup();
        let token = build_token(TEST_ONLY_KEY_A, "kid-a", "B", T0, T0 + 365 * 86_400);
        fake.respond_with_signed_activation(&token, TEST_ONLY_SERVER_SEED);
        client.activate("ACT-1").await.expect("activate ok");

        // 异钥签名：服务端私钥换成 TEST_ONLY_KEY_B（≠ 激活时钉定的 TEST_ONLY_SERVER_SEED）。
        fake.respond_with_signed_heartbeat(TEST_ONLY_KEY_B, T0, T0 + 400 * SECS_PER_DAY);
        let err = client
            .heartbeat(None)
            .await
            .expect_err("foreign-key heartbeat sig must fail");
        assert_eq!(err.error_code(), ERR_SECURITY);
        // 状态不推进：仍是 Licensed，但 `valid_until` 保持激活时的原值（未被刷新）。
        match client.current_state() {
            LicenseState::Licensed { lease } => {
                assert_eq!(
                    lease.valid_until,
                    T0 + 365 * SECS_PER_DAY,
                    "unverified heartbeat must NOT refresh valid_until"
                );
            }
            other => panic!("expected Licensed, got {other:?}"),
        }
    }

    /// 心跳响应缺 `sig` → AuthError，状态不推进。
    #[tokio::test]
    async fn heartbeat_missing_sig_is_fail_closed() {
        let (client, fake) = setup();
        let token = build_token(TEST_ONLY_KEY_A, "kid-a", "B", T0, T0 + 365 * 86_400);
        fake.respond_with_signed_activation(&token, TEST_ONLY_SERVER_SEED);
        client.activate("ACT-1").await.expect("activate ok");

        // 用 Ok 动作给一个无 sig 的心跳响应（缺签负例）。
        fake.respond_with(serde_json::json!({
            "server_time": T0.to_string(),
            "next_deadline": (T0 + 86_400).to_string(),
            "valid_until": (T0 + 400 * SECS_PER_DAY).to_string(),
            "verify_mode": "B",
            "tier": "standard",
        }));
        let err = client
            .heartbeat(None)
            .await
            .expect_err("missing sig must fail");
        assert_eq!(err.error_code(), ERR_AUTH);
        assert!(err.to_string().contains("'sig'"), "{err}");
        match client.current_state() {
            LicenseState::Licensed { lease } => {
                assert_eq!(lease.valid_until, T0 + 365 * SECS_PER_DAY);
            }
            other => panic!("expected Licensed, got {other:?}"),
        }
    }

    /// TOFU 钉定后：心跳响应签名用异钥（哪怕响应其余字段合法）→ 一律拒绝。
    #[tokio::test]
    async fn heartbeat_after_tofu_pinning_rejects_other_key() {
        let (client, fake) = setup();
        let token = build_token(TEST_ONLY_KEY_A, "kid-a", "B", T0, T0 + 365 * 86_400);
        fake.respond_with_signed_activation(&token, TEST_ONLY_SERVER_SEED);
        client.activate("ACT-1").await.expect("activate ok");
        assert!(client.pinned_server_key_b64().is_some());

        // 第三把私钥签心跳响应。
        let rogue_seed: [u8; 32] = [0x7eu8; 32];
        fake.respond_with_signed_heartbeat(rogue_seed, T0, T0 + 400 * SECS_PER_DAY);
        let err = client
            .heartbeat(Some((1, 2)))
            .await
            .expect_err("rogue-key heartbeat must be rejected");
        assert_eq!(err.error_code(), ERR_SECURITY);
        assert!(err.to_string().contains("server response"), "{err}");
        assert_eq!(client.current_state().name(), "Licensed", "state unchanged");
    }

    /// 独立函数级正例：`verify_server_response_signature` 对契约域串的合法签名放行。
    #[test]
    fn verify_server_response_signature_accepts_valid_sig() {
        let sig = sign_server_response(
            TEST_ONLY_SERVER_SEED,
            RESPONSE_SIG_DOMAIN_HEARTBEAT,
            "lease-0001",
            "nonce-x",
            T0,
        );
        verify_server_response_signature(
            &server_pubkey_b64(TEST_ONLY_SERVER_SEED),
            RESPONSE_SIG_DOMAIN_HEARTBEAT,
            "lease-0001",
            "nonce-x",
            T0,
            &sig,
        )
        .expect("valid server response signature must verify");
    }

    /// 独立函数级负例：篡改域串任一字段（lease_id / nonce / server_time）→ 拒绝。
    #[test]
    fn verify_server_response_signature_rejects_tampered_fields() {
        let sig = sign_server_response(
            TEST_ONLY_SERVER_SEED,
            RESPONSE_SIG_DOMAIN_ACTIVATION,
            "lease-0001",
            "nonce-x",
            T0,
        );
        let pubkey = server_pubkey_b64(TEST_ONLY_SERVER_SEED);
        // lease_id 不同。
        assert!(verify_server_response_signature(
            &pubkey,
            RESPONSE_SIG_DOMAIN_ACTIVATION,
            "lease-0002",
            "nonce-x",
            T0,
            &sig
        )
        .is_err());
        // server_time 不同。
        assert!(verify_server_response_signature(
            &pubkey,
            RESPONSE_SIG_DOMAIN_ACTIVATION,
            "lease-0001",
            "nonce-x",
            T0 + 1,
            &sig
        )
        .is_err());
        // 跨域混用（heartbeat 的 sig 用于 activation）。
        assert!(verify_server_response_signature(
            &pubkey,
            RESPONSE_SIG_DOMAIN_HEARTBEAT,
            "lease-0001",
            "nonce-x",
            T0,
            &sig
        )
        .is_err());
    }

    /// 要求 5（端到端·心跳超时）：网络失败 → NetworkError，**状态不降级**。
    #[tokio::test]
    async fn heartbeat_timeout_does_not_downgrade() {
        let (client, fake) = setup();
        let token = build_token(TEST_ONLY_KEY_A, "kid-a", "B", T0, T0 + 365 * 86_400);
        fake.respond_with_signed_activation(&token, TEST_ONLY_SERVER_SEED);
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
        fake.respond_with_signed_activation(&token, TEST_ONLY_SERVER_SEED);
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

    /// B 档心跳携带回执游标 → 请求体含 **嵌套** `receipt_cursor`，且大整数走字符串。
    #[tokio::test]
    async fn heartbeat_with_cursor_sends_string_numbers() {
        let (client, fake) = setup();
        let token = build_token(TEST_ONLY_KEY_A, "kid-a", "B", T0, T0 + 365 * 86_400);
        fake.respond_with_signed_activation(&token, TEST_ONLY_SERVER_SEED);
        client.activate("ACT-1").await.expect("activate ok");

        fake.respond_with_signed_heartbeat(TEST_ONLY_SERVER_SEED, T0, T0 + 365 * 86_400);
        client
            .heartbeat(Some((100, 9007199254740991)))
            .await
            .expect("heartbeat ok with big cursor");

        let body = fake.last_body().expect("body recorded");
        // `receipt_cursor` 是**嵌套对象**，序号是字符串（大整数不走 JSON number）。
        assert_eq!(body["receipt_cursor"]["seq_from"], serde_json::json!("100"));
        assert_eq!(
            body["receipt_cursor"]["seq_to"],
            serde_json::json!("9007199254740991")
        );
        assert!(
            body["receipt_cursor"]["seq_to"].is_string(),
            "big int must be string"
        );
        // `ts` 同样是字符串。
        assert!(body["ts"].is_string(), "ts must be a JSON string");
    }

    /// 要求 7：回执请求体 key 集合**恰好等于** 8 个白名单字段。
    #[tokio::test]
    async fn receipt_body_has_exactly_whitelisted_keys() {
        let (client, fake) = setup();
        let token = build_token(TEST_ONLY_KEY_A, "kid-a", "B", T0, T0 + 365 * 86_400);
        fake.respond_with_signed_activation(&token, TEST_ONLY_SERVER_SEED);
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
        fake.respond_with_signed_activation(&token, TEST_ONLY_SERVER_SEED);
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
        fake.respond_with_signed_activation(&token, TEST_ONLY_SERVER_SEED);
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
        fake.respond_with_signed_activation(&token, TEST_ONLY_SERVER_SEED);
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
        fake.respond_with_signed_activation(&token, TEST_ONLY_SERVER_SEED);
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
        fake.respond_with_signed_activation(&token, TEST_ONLY_SERVER_SEED);
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
        fake.respond_with_signed_activation(&token, TEST_ONLY_SERVER_SEED);
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

    /// 心跳成功后刷新宽限基准：验证「重连回到 Licensed」（响应**不含新 Token**）。
    #[tokio::test]
    async fn successful_heartbeat_restores_licensed() {
        let (client, fake) = setup();
        let token = build_token(TEST_ONLY_KEY_A, "kid-a", "B", T0, T0 + SECS_PER_DAY);
        fake.respond_with_signed_activation(&token, TEST_ONLY_SERVER_SEED);
        client.activate("ACT-1").await.expect("activate ok");

        // 进入 Grace。
        client.tick(T0 + SECS_PER_DAY);
        assert_eq!(client.current_state().name(), "Grace");

        // 心跳成功（服务端回显更长的 valid_until）→ 回到 Licensed；**不要求响应含新 Token**。
        fake.respond_with_signed_heartbeat(
            TEST_ONLY_SERVER_SEED,
            T0 + SECS_PER_DAY,
            T0 + 365 * 86_400,
        );
        let state = client.heartbeat(None).await.expect("heartbeat ok");
        assert_eq!(state.name(), "Licensed");
        assert_eq!(client.current_state().name(), "Licensed");
        match state {
            LicenseState::Licensed { lease } => {
                assert_eq!(lease.lease_id, "lease-0001");
                // 本地租约有效期以服务端权威 `valid_until` 刷新。
                assert_eq!(lease.valid_until, T0 + 365 * 86_400);
            }
            other => panic!("expected Licensed, got {other:?}"),
        }
    }

    /// 心跳响应缺 `valid_until` → AuthError，状态不变（**不再要求响应携带 Token**）。
    #[tokio::test]
    async fn heartbeat_missing_server_fields_errors_and_keeps_state() {
        let (client, fake) = setup();
        let token = build_token(TEST_ONLY_KEY_A, "kid-a", "B", T0, T0 + 365 * 86_400);
        fake.respond_with_signed_activation(&token, TEST_ONLY_SERVER_SEED);
        client.activate("ACT-1").await.expect("activate ok");

        // 心跳响应只有 server_time，缺 valid_until → AuthError。
        fake.respond_with(serde_json::json!({ "server_time": "1700000000" }));
        let err = client
            .heartbeat(None)
            .await
            .expect_err("missing valid_until must error");
        assert_eq!(err.error_code(), ERR_AUTH);
        // 状态保持 Licensed（心跳失败不降级）。
        assert_eq!(client.current_state().name(), "Licensed");
    }

    /// 无租约（未激活）时心跳 → AuthError（心跳必须有租约）。
    #[tokio::test]
    async fn heartbeat_without_lease_is_auth_error() {
        let (client, _fake) = setup();
        let err = client
            .heartbeat(None)
            .await
            .expect_err("no lease must reject heartbeat");
        assert_eq!(err.error_code(), ERR_AUTH);
        assert!(err
            .to_string()
            .contains("heartbeat requires an activated lease"));
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
        fake.respond_with_signed_activation(&token, TEST_ONLY_SERVER_SEED);
        client.activate("ACT-1").await.expect("activate ok");
        assert!(LicenseGate::can_sign(&client));

        // 降级：闸门关闭。先用短有效期租约激活，再推进「过期 → 宽限 → 宽限耗尽」。
        let trial_token = build_token(TEST_ONLY_KEY_A, "kid-a", "B", T0, T0 + SECS_PER_DAY);
        fake.respond_with_signed_activation(&trial_token, TEST_ONLY_SERVER_SEED);
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
        fake.respond_with_signed_activation(&token, TEST_ONLY_SERVER_SEED);
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
        let client = LicensingClient::new(cfg, test_signer(), test_mid())
            .with_anchor_hashes(test_anchor_hashes());
        let err = client
            .activate("ACT-1")
            .await
            .expect_err("default transport unavailable");
        assert_eq!(err.error_code(), ERR_NETWORK);
        assert_eq!(client.current_state(), LicenseState::Unlicensed);
    }

    /// **fail-closed**：未注入逐锚点哈希集 → `activate` 立即 ConfigError，绝不退化为 M=1。
    #[tokio::test]
    async fn activate_without_anchor_hashes_is_config_error() {
        let cfg = LicensingClientConfig::new("https://licensing.test/v1", 24, 10, 30).expect("ok");
        // 不调用 with_anchor_hashes ⇒ 锚点集为空。
        let client = LicensingClient::new(cfg, test_signer(), test_mid());
        let err = client
            .activate("ACT-1")
            .await
            .expect_err("missing anchors must be fail-closed");
        assert_eq!(err.error_code(), ERR_CONFIG);
        assert!(
            err.to_string().contains("per-anchor hash set"),
            "error must explain the N-of-M degradation risk: {err}"
        );
        assert_eq!(client.current_state(), LicenseState::Unlicensed);

        // 注入**空**集合同样 fail-closed。
        let cfg2 = LicensingClientConfig::new("https://licensing.test/v1", 24, 10, 30).expect("ok");
        let client2 =
            LicensingClient::new(cfg2, test_signer(), test_mid()).with_anchor_hashes(Vec::new());
        let err2 = client2
            .activate("ACT-1")
            .await
            .expect_err("empty anchors must be fail-closed");
        assert_eq!(err2.error_code(), ERR_CONFIG);
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

    // ================= 协议契约（daemon ↔ licensing-server 对齐） =================

    /// 心跳签名域**逐字节**钉死形状（反向守护：改域串即红）。
    #[test]
    fn heartbeat_signing_message_layout_is_pinned() {
        let got =
            render_heartbeat_signing_message("lease-0001", 1_700_000_000, "n-1", Some((1, 100)));
        assert_eq!(
            got,
            concat!(
                "iotdaq.heartbeat.v1",
                "|lease_id=10:lease-0001",
                "|ts=10:1700000000",
                "|nonce=3:n-1",
                "|cursor_from=1:1",
                "|cursor_to=3:100",
            )
        );

        // cursor 为空 → 双端字面量 "none"。
        let none = render_heartbeat_signing_message("lease-0001", 1_700_000_000, "n-1", None);
        assert!(
            none.contains("|cursor_from=4:none|cursor_to=4:none"),
            "{none}"
        );
    }

    /// 校验签名域**逐字节**钉死形状。
    #[test]
    fn verify_signing_message_layout_is_pinned() {
        let got = render_verify_signing_message(
            "MID-0001",
            "lease-0001",
            "sha256:abcdef",
            1_700_000_000,
            "n-1",
        );
        assert_eq!(
            got,
            concat!(
                "iotdaq.verify.v1",
                "|mid=8:MID-0001",
                "|lease_id=10:lease-0001",
                "|payload_digest=13:sha256:abcdef",
                "|ts=10:1700000000",
                "|nonce=3:n-1",
            )
        );
    }

    /// 激活签名域**逐字节**钉死形状（含 anchor 计数 + 逐个定界）。
    #[test]
    fn activation_signing_message_layout_is_pinned() {
        let hashes = vec!["a".to_string(), "b".to_string()];
        let got = render_activation_signing_message(
            "ACT-CODE",
            "mid-1",
            &hashes,
            "PUBKEY",
            "n-1",
            1_700_000_000,
        );
        assert_eq!(
            got,
            concat!(
                "iotdaq.activation.v1",
                "|activation_code=8:ACT-CODE",
                "|machine_code=5:mid-1",
                "|anchors=1:2",
                "|anchor_0=1:a",
                "|anchor_1=1:b",
                "|device_pubkey=6:PUBKEY",
                "|nonce=3:n-1",
                "|ts=10:1700000000",
            )
        );
    }

    /// 心跳哈希确实带二级域前缀（`SHA256(域串)` ≠ `heartbeat_payload_hash`）。
    #[test]
    fn heartbeat_hash_carries_second_level_domain() {
        use sha2::{Digest, Sha256};
        let message = render_heartbeat_signing_message("l", 1, "n", None);
        let bare: [u8; 32] = Sha256::digest(message.as_bytes()).into();
        assert_ne!(heartbeat_payload_hash("l", 1, "n", None), bare);
    }

    /// 心跳请求体 key 集合**恰好等于** 5 字段白名单，且 `device_sig` 非空且真能验签。
    #[tokio::test]
    async fn heartbeat_body_has_exactly_whitelisted_keys_and_valid_sig() {
        let (client, fake) = setup();
        let token = build_token(TEST_ONLY_KEY_A, "kid-a", "B", T0, T0 + 365 * 86_400);
        fake.respond_with_signed_activation(&token, TEST_ONLY_SERVER_SEED);
        client.activate("ACT-1").await.expect("activate ok");

        fake.respond_with_signed_heartbeat(TEST_ONLY_SERVER_SEED, T0, T0 + 365 * 86_400);
        client
            .heartbeat(Some((1, 100)))
            .await
            .expect("heartbeat ok");

        let body = fake.last_body().expect("body recorded");
        let mut keys = object_keys(&body);
        keys.sort();
        let mut expected: Vec<String> = HEARTBEAT_FIELD_WHITELIST
            .iter()
            .map(|s| s.to_string())
            .collect();
        expected.sort();
        assert_eq!(keys, expected, "heartbeat body must have exactly 5 fields");

        // `device_sig` 非空且真能验签（独立重建 + 设备公钥验签）。
        let lease_id = body["lease_id"].as_str().expect("lease_id");
        let ts = body["ts"]
            .as_str()
            .expect("ts string")
            .parse::<i64>()
            .expect("ts i64");
        let nonce = body["nonce"].as_str().expect("nonce");
        let sig_b64 = body["device_sig"].as_str().expect("device_sig");
        assert!(!sig_b64.is_empty(), "device_sig must not be empty");
        let hash = heartbeat_payload_hash(lease_id, ts, nonce, Some((1, 100)));
        let sig_bytes: [u8; ED25519_SIGNATURE_LEN] = B64
            .decode(sig_b64)
            .expect("valid base64")
            .as_slice()
            .try_into()
            .expect("64 bytes");
        let signature = Signature::from_bytes(&sig_bytes);
        SigningKey::from_bytes(&TEST_ONLY_KEY_A)
            .verifying_key()
            .verify(&hash, &signature)
            .expect("heartbeat device_sig must verify against device public key");
    }

    /// 未注入设备签名者 → 心跳 `device_sig` 为空串（明确失败信号，不伪造）。
    #[tokio::test]
    async fn heartbeat_device_sig_empty_when_signer_unavailable() {
        let fake = Arc::new(FakeTransport::default());
        let client = client_with_fake_no_receipt_signer(&fake);
        let token = build_token(TEST_ONLY_KEY_A, "kid-a", "B", T0, T0 + 365 * 86_400);
        fake.respond_with_signed_activation(&token, TEST_ONLY_SERVER_SEED);
        client.activate("ACT-1").await.expect("activate ok");

        fake.respond_with_signed_heartbeat(TEST_ONLY_SERVER_SEED, T0, T0 + 365 * 86_400);
        client
            .heartbeat(None)
            .await
            .expect("transport still called");

        let body = fake.last_body().expect("body recorded");
        assert_eq!(
            body["device_sig"].as_str().expect("device_sig is string"),
            "",
            "unavailable signer must emit an EMPTY device_sig (failure signal)"
        );
    }

    /// `/verify` 成功：请求打到 verify 端点、nonce 回显一致 → 返回 `ok`。
    #[tokio::test]
    async fn verify_ok_echoes_nonce_and_returns_ok() {
        let (client, fake) = setup();
        let token = build_token(TEST_ONLY_KEY_A, "kid-a", "A", T0, T0 + 365 * 86_400);
        fake.respond_with_signed_activation(&token, TEST_ONLY_SERVER_SEED);
        client.activate("ACT-1").await.expect("activate ok");

        fake.respond_with_verify_echo(true);
        let ok = client.verify("sha256:deadbeef").await.expect("verify ok");
        assert!(ok);
        assert!(fake
            .calls()
            .last()
            .expect("a call happened")
            .ends_with("/verify"));

        let body = fake.last_body().expect("body recorded");
        let mut keys = object_keys(&body);
        keys.sort();
        let mut expected: Vec<String> = VERIFY_FIELD_WHITELIST
            .iter()
            .map(|s| s.to_string())
            .collect();
        expected.sort();
        assert_eq!(keys, expected, "verify body must have exactly 6 fields");
        assert!(body["ts"].is_string(), "ts must be a JSON string");
        assert!(
            !body["device_sig"].as_str().expect("device_sig").is_empty(),
            "verify device_sig must not be empty"
        );
    }

    /// `/verify` nonce 回显不一致 → SecurityError（防响应重放 / 串扰）。
    #[tokio::test]
    async fn verify_nonce_mismatch_is_security_error() {
        let (client, fake) = setup();
        let token = build_token(TEST_ONLY_KEY_A, "kid-a", "A", T0, T0 + 365 * 86_400);
        fake.respond_with_signed_activation(&token, TEST_ONLY_SERVER_SEED);
        client.activate("ACT-1").await.expect("activate ok");

        fake.respond_with(serde_json::json!({
            "ok": true, "server_time": "1700000000", "nonce": "not-the-request-nonce"
        }));
        let err = client
            .verify("sha256:x")
            .await
            .expect_err("nonce mismatch must error");
        assert_eq!(err.error_code(), ERR_SECURITY);
    }

    /// 无租约时 `verify` → AuthError。
    #[tokio::test]
    async fn verify_without_lease_is_auth_error() {
        let (client, _fake) = setup();
        let err = client.verify("sha256:x").await.expect_err("no lease");
        assert_eq!(err.error_code(), ERR_AUTH);
    }

    /// 激活请求体 key 集合**恰好等于** 7 字段白名单；`device_pubkey` / `req_sig` 非空。
    #[tokio::test]
    async fn activation_body_has_exactly_whitelisted_keys() {
        let (client, fake) = setup();
        let token = build_token(TEST_ONLY_KEY_A, "kid-a", "B", T0, T0 + 365 * 86_400);
        fake.respond_with_signed_activation(&token, TEST_ONLY_SERVER_SEED);
        client.activate("ACT-CODE").await.expect("activate ok");

        let body = fake.last_body().expect("body recorded");
        let mut keys = object_keys(&body);
        keys.sort();
        let mut expected: Vec<String> = ACTIVATION_FIELD_WHITELIST
            .iter()
            .map(|s| s.to_string())
            .collect();
        expected.sort();
        assert_eq!(keys, expected, "activation body must have exactly 7 fields");

        // anchor_hashes 是数组（未注入时退化为仅机器码指纹，故长度 ≥ 1）。
        assert!(body["anchor_hashes"].is_array(), "{body}");
        assert!(
            !body["anchor_hashes"].as_array().expect("array").is_empty(),
            "anchor_hashes must not be empty"
        );
        assert!(body["ts"].is_string(), "ts must be a JSON string");
        assert!(!body["device_pubkey"].as_str().expect("pk").is_empty());
        assert!(!body["req_sig"].as_str().expect("req_sig").is_empty());
    }

    /// 激活 `req_sig` 真能验签（独立重建 + 设备公钥），且 `device_pubkey` 与私钥一致。
    #[tokio::test]
    async fn activation_req_sig_verifies_and_pubkey_matches() {
        let (client, fake) = setup();
        let token = build_token(TEST_ONLY_KEY_A, "kid-a", "B", T0, T0 + 365 * 86_400);
        fake.respond_with_signed_activation(&token, TEST_ONLY_SERVER_SEED);
        client.activate("ACT-CODE").await.expect("activate ok");

        let body = fake.last_body().expect("body recorded");
        let code = body["activation_code"].as_str().expect("code");
        let machine = body["machine_code"].as_str().expect("machine");
        let nonce = body["nonce"].as_str().expect("nonce");
        let ts = body["ts"]
            .as_str()
            .expect("ts string")
            .parse::<i64>()
            .expect("ts i64");
        let pk_b64 = body["device_pubkey"].as_str().expect("pk");
        let anchors: Vec<String> = body["anchor_hashes"]
            .as_array()
            .expect("anchors array")
            .iter()
            .map(|v| v.as_str().expect("anchor string").to_string())
            .collect();

        // device_pubkey 必须等于设备私钥对应公钥。
        let expected_pk = B64.encode(
            SigningKey::from_bytes(&TEST_ONLY_KEY_A)
                .verifying_key()
                .to_bytes(),
        );
        assert_eq!(pk_b64, expected_pk, "device_pubkey must match device key");

        let hash = activation_payload_hash(code, machine, &anchors, pk_b64, nonce, ts);
        let sig_b64 = body["req_sig"].as_str().expect("req_sig");
        let sig_bytes: [u8; ED25519_SIGNATURE_LEN] = B64
            .decode(sig_b64)
            .expect("valid base64")
            .as_slice()
            .try_into()
            .expect("64 bytes");
        let signature = Signature::from_bytes(&sig_bytes);
        SigningKey::from_bytes(&TEST_ONLY_KEY_A)
            .verifying_key()
            .verify(&hash, &signature)
            .expect("activation req_sig must verify");
    }

    /// `with_anchor_hashes` 注入后，激活请求体带注入的真实锚点哈希。
    #[tokio::test]
    async fn activation_uses_injected_anchor_hashes() {
        let fake = Arc::new(FakeTransport::default());
        let cfg = LicensingClientConfig::new("https://licensing.test/v1", 24, 10, 30)
            .expect("non-empty url");
        let signer = test_signer();
        let transport: Arc<dyn LicenseTransport> = Arc::clone(&fake) as Arc<dyn LicenseTransport>;
        let mut client = LicensingClient::with_transport(cfg, signer, test_mid(), transport)
            .with_device_signer(Arc::new(StaticKeyProvider::new(TEST_ONLY_KEY_A)))
            .with_anchor_hashes(vec!["h0".to_string(), "h1".to_string()]);
        client
            .register_public_key("kid-a", &public_key_bytes(TEST_ONLY_KEY_A))
            .expect("register kid-a");

        let token = build_token(TEST_ONLY_KEY_A, "kid-a", "B", T0, T0 + 365 * 86_400);
        fake.respond_with_signed_activation(&token, TEST_ONLY_SERVER_SEED);
        client.activate("ACT-CODE").await.expect("activate ok");

        let body = fake.last_body().expect("body recorded");
        assert_eq!(
            body["anchor_hashes"],
            serde_json::json!(["h0", "h1"]),
            "injected anchor hashes must be sent verbatim"
        );
    }
}

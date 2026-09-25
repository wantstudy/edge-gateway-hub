//! `auth::assembly` — 授权链路**生产装配**（bootstrap 装配任务，接通 task 22/23 全部原语）。
//!
//! 在本模块之前，`MachineIdentity` / `LicensingClient` / 设备签名密钥托管全部
//! 已实现但**没有生产调用点**（`MachineIdentity::new` 全仓只有测试调用，
//! `LicensingClient` 从未在 bootstrap 构造）。本模块是唯一的生产装配点：
//!
//! ```text
//! [gateway.licensing]（config，只读）
//!   + gateway.data_dir（宿主持久卷，红线 #13）
//!   → MachineIdentity（平台锚点集 + IOT_DAQ_FINGERPRINT_KEY）→ machine_code + anchor_hashes
//!   → FileKeyProvider（data_dir/license/device-ed25519.key，首跑生成、HKDF 机器码绑定加密）
//!   → AuthSigner（gate = LicensingClient 状态，延迟弱引用绑定）+ device_signer（同一把密钥）
//!   → LicensingClient（device_signer + anchor_hashes + cloud_url + HTTPS transport）
//!   → LicenseRuntimeConfig（activation_code / heartbeat / data_dir）→ bootstrap.with_license_runtime
//! ```
//!
//! # fail-closed 装配纪律（与「降级必须可解释」契约一致）
//! 任一装配步骤失败（指纹 key 缺失 / 锚点 quorum 不足 / 密钥持久化失败）→
//! [`AssemblyOutcome::Failed`]，调用方（bin 入口）照常启动 daemon（**本地采集不受影响**），
//! 但经 `BootstrapBuilder::with_license_assembly_failed` 让 bootstrap：
//! ① 把可解释原因（含恢复路径）落入 `DaemonShared`；② 北向闸门保持关闭。
//! 错误消息只含环境变量**名**、文件路径与数量——**激活码 / 私钥材料绝不进日志与错误**。
//!
//! # 客户端红线
//! - 客户端**不预判**服务端 N-of-M 同机判定：本地只产出并上报逐锚点哈希集；
//!   [`PRODUCTION_MIN_ANCHORS`] 是「本地凑不够锚点数就算不出指纹」的采集 quorum，
//!   与服务端 4/5 同机阈值是两回事。
//! - 指纹 HMAC key 从 env 注入（禁止硬编码）；设备私钥不出 [`HandleSigner`] 句柄。
//! - 激活码优先取 `[gateway.licensing].activation_code`，回退 env
//!   [`ACTIVATION_CODE_ENV`]（与 `license.rs` 文档口径一致）。
//!
//! # 生产 HTTPS transport
//! [`LicenseTransport`] 是同步 trait；[`HttpsTransport`] 用**独立工作线程 +
//! current_thread runtime** 承载手写 HTTP/1.1 客户端（tokio + tokio-rustls，
//! 均已在 Cargo.lock——零新增下载；rustls provider 统一 `ring`，纯 Rust 栈合规）。
//! 每请求独立连接、总超时钳制、响应体限长（256 KiB）。

use std::path::{Path, PathBuf};
use std::sync::mpsc::{channel, sync_channel, Receiver, SyncSender};
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine as _;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tracing::{error, warn};

use crate::auth::client::{
    LicenseTransport, LicensingClient, LicensingClientConfig, DEFAULT_CONNECT_TIMEOUT_SECS,
    DEFAULT_REQUEST_TIMEOUT_SECS,
};
use crate::auth::keyprovider::{FileKeyProvider, KeyProvider as _};
use crate::auth::machine_id::{AnchorProvider, FingerprintKey, MachineIdentity};
use crate::auth::signing::{AuthSigner, HandleSigner, KeyHandleKeyProviderAdapter, LicenseGate};
use crate::config::LicensingSection;
use crate::error::DaemonError;
use crate::license::LicenseRuntimeConfig;

// ---- 环境变量与文件名约定（部署契约；只记名字，绝不记值） ----

/// 指纹 HMAC key 的环境变量（部署期注入；缺失 → 装配 fail-closed）。
pub const FINGERPRINT_KEY_ENV: &str = "IOT_DAQ_FINGERPRINT_KEY";

/// 激活码回退环境变量（config 缺省时使用；与 `license.rs` 文档口径一致）。
pub const ACTIVATION_CODE_ENV: &str = "IOTDAQ_ACTIVATION_CODE";

/// Lease Token 验签公钥集环境变量：`kid1=BASE64,kid2=BASE64`（随应用升级分发；
/// 未配置时激活将在本地验签处 fail-closed，日志显式提示）。
pub const LICENSE_PUBLIC_KEYS_ENV: &str = "IOT_DAQ_LICENSE_PUBLIC_KEYS";

/// 宿主 MAC 环境变量（容器形态由安装脚本注入；`machine-fingerprint.md` §6）。
pub const HOST_MAC_ENV: &str = "IOT_DAQ_HOST_MAC";

/// 宿主锚点只读挂载根（容器形态；与 bin preflight 的 `IOT_DAQ_HOST_ANCHOR_ROOT` 一致）。
pub const HOST_ANCHOR_ROOT_ENV: &str = "IOT_DAQ_HOST_ANCHOR_ROOT";

/// 宿主 machine-id 挂载路径（用于反推锚点根；与 bin preflight 口径一致）。
pub const HOST_MACHINE_ID_PATH_ENV: &str = "IOT_DAQ_HOST_MACHINE_ID_PATH";

/// 宿主签名指纹文件环境变量（锚点缺失降级；`machine-fingerprint.md` §6）。
pub const HOST_FINGERPRINT_FILE_ENV: &str = "IOT_DAQ_HOST_FINGERPRINT_FILE";

/// 设备签名密钥文件名（相对 `data_dir/license/`；宿主持久卷）。
pub const DEVICE_KEY_FILE: &str = "device-ed25519.key";

/// 设备签名密钥子目录（preflight 已保证 `license/` 存在于持久卷）。
pub const DEVICE_KEY_SUBDIR: &str = "license";

/// 本地锚点采集 quorum：可用锚点数低于该值就算不出指纹（fail-closed）。
///
/// ⚠️ 这**不是**服务端 N-of-M 同机判定阈值（4/5）——客户端不参与同机判定。
/// 取 2：容忍单锚点漂移，同时保证指纹至少组合两个独立锚点（单锚点不足以代表机器）。
pub const PRODUCTION_MIN_ANCHORS: usize = 2;

/// HTTPS 响应体最大字节数（防异常服务端 / 中间人灌大响应）。
const MAX_RESPONSE_BYTES: usize = 256 * 1024;

// ---- 装配输入 / 结果 ----

/// 装配输入（生产参数可注入，测试可全部替换）。
pub struct AssemblyInput<'a> {
    /// `[gateway.licensing]` 配置段（只读）。
    pub licensing: &'a LicensingSection,
    /// 授权数据落盘目录（**必须**来自 `gateway.data_dir`，宿主持久卷，红线 #13）。
    pub data_dir: &'a Path,
    /// 云授权 transport；`None` = 生产 HTTPS transport（[`HttpsTransport::production`]）。
    pub transport: Option<Arc<dyn LicenseTransport>>,
    /// 锚点提供者集；`None` = 生产平台锚点集（[`production_anchor_providers`]）。
    pub anchor_providers: Option<Vec<Box<dyn AnchorProvider>>>,
    /// 指纹 HMAC key；`None` = 从 [`FINGERPRINT_KEY_ENV`] 读取。
    pub fingerprint_key: Option<FingerprintKey>,
    /// 设备签名密钥文件路径；`None` = `data_dir/license/` + [`DEVICE_KEY_FILE`]。
    pub device_key_path: Option<PathBuf>,
    /// Lease 验签公钥集；`None` = 从 [`LICENSE_PUBLIC_KEYS_ENV`] 读取。
    pub lease_public_keys: Option<Vec<(String, Vec<u8>)>>,
}

/// 装配结果。
#[derive(Debug)]
pub enum AssemblyOutcome {
    /// 装配成功：bootstrap 直接 `with_license_runtime` 上岗。
    Assembled(LicenseRuntimeConfig),
    /// 未配置授权（无 cloud_url 且无 activation_code）→ 保持现行为（闸门 None 恒放行）。
    NotConfigured,
    /// 装配失败（fail-closed）：daemon 照常启动，北向闸门保持关闭；消息含恢复路径。
    Failed(String),
}

/// 生产装配入口（bin 调用）：全部参数取生产默认（平台锚点 / env 密钥 / HTTPS transport）。
pub fn assemble_production(licensing: &LicensingSection, data_dir: &Path) -> AssemblyOutcome {
    assemble(AssemblyInput {
        licensing,
        data_dir,
        transport: None,
        anchor_providers: None,
        fingerprint_key: None,
        device_key_path: None,
        lease_public_keys: None,
    })
}

/// 核心装配（可注入；详见 [`AssemblyInput`]）。
pub fn assemble(input: AssemblyInput<'_>) -> AssemblyOutcome {
    let licensing = input.licensing;
    let cloud_url = licensing
        .cloud_url
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let code_from_config = licensing
        .activation_code
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());

    // ⓪ 配置判定：未配置 → 保持现行为；码有而址无 → 显式 fail-closed（绝不猜测意图）。
    let (cloud_url, code) = match (cloud_url, code_from_config) {
        (None, None) => return AssemblyOutcome::NotConfigured,
        (None, Some(_)) => {
            return AssemblyOutcome::Failed(
                "licensing misconfiguration: [gateway.licensing].activation_code is set but \
                 cloud_url is missing; activation requires the cloud licensing service. \
                 Recovery: set [gateway.licensing].cloud_url (or clear activation_code to run \
                 local-trial-only) and restart"
                    .to_string(),
            )
        }
        (Some(url), code) => (url.to_string(), code.map(str::to_string)),
    };
    // 激活码：config 优先，回退部署环境变量（**值绝不进日志**）。
    let code = code.or_else(|| {
        std::env::var(ACTIVATION_CODE_ENV)
            .ok()
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
    });

    // ① 指纹 HMAC key（注入优先；缺失 → fail-closed，错误只引用变量名）。
    let fingerprint_key = match input.fingerprint_key {
        Some(key) => key,
        None => match FingerprintKey::from_env(FINGERPRINT_KEY_ENV) {
            Ok(key) => key,
            Err(err) => {
                return AssemblyOutcome::Failed(format!(
                    "machine fingerprint key unavailable ({err}); license assembly cannot \
                     proceed and northbound stays closed. Recovery: deploy \
                     {FINGERPRINT_KEY_ENV} and restart; local capture is unaffected — \
                     northbound resumes automatically after a successful activation"
                ))
            }
        },
    };

    // ② MachineIdentity：平台锚点集 → machine_code + anchor_hashes（quorum fail-closed）。
    let providers = input
        .anchor_providers
        .unwrap_or_else(|| production_anchor_providers(input.data_dir));
    let identity = MachineIdentity::new(providers, PRODUCTION_MIN_ANCHORS, fingerprint_key);
    let machine_code = match identity.get_machine_fingerprint() {
        Ok(code) => code,
        Err(err) => {
            return AssemblyOutcome::Failed(format!(
                "machine identity failed ({err}); license assembly cannot proceed and \
                 northbound stays closed. Recovery: mount the host anchor set (container: \
                 read-only /host mounts per container-machine-binding.md §3) or restore the \
                 fingerprint key, then restart; local capture is unaffected — northbound \
                 resumes automatically after a successful activation"
            ))
        }
    };
    let anchor_hashes = match identity.get_anchor_hashes() {
        Ok(hashes) => hashes,
        Err(err) => {
            return AssemblyOutcome::Failed(format!(
                "anchor hash collection failed ({err}); activation requires the per-anchor \
                 hash set (client never judges server-side N-of-M). Recovery: mount the host \
                 anchor set and restart; local capture is unaffected — northbound resumes \
                 automatically after a successful activation"
            ))
        }
    };

    // ③ 设备签名密钥：首跑生成 / 加载，HKDF 机器码绑定加密落盘（拷盘即失效）。
    let key_path = input
        .device_key_path
        .unwrap_or_else(|| default_device_key_path(input.data_dir));
    if let Some(parent) = key_path.parent() {
        if let Err(err) = std::fs::create_dir_all(parent) {
            return AssemblyOutcome::Failed(format!(
                "device key directory not writable ({}): {err}; license assembly cannot \
                 proceed. Recovery: make the persistent volume writable (preflight checks \
                 {DEVICE_KEY_SUBDIR}/) and restart",
                parent.display()
            ));
        }
    }
    let key_provider = match FileKeyProvider::new(key_path, &machine_code) {
        Ok(provider) => provider,
        Err(err) => {
            return AssemblyOutcome::Failed(format!(
                "device key provider rejected the machine fingerprint ({err}); license \
                 assembly cannot proceed. Recovery: verify the fingerprint pipeline and restart"
            ))
        }
    };
    let handle = match key_provider.load_or_create() {
        Ok(handle) => handle,
        Err(err) => {
            return AssemblyOutcome::Failed(format!(
                "device signing key unavailable ({err}); license assembly cannot proceed and \
                 northbound stays closed. Recovery: for TamperedOrForeign the operator must \
                 reset the key manually (never copy key files between machines); for IO errors \
                 fix the persistent volume and restart"
            ))
        }
    };
    let device_signer: Arc<dyn HandleSigner> = Arc::new(KeyHandleKeyProviderAdapter::new(handle));

    // ④ AuthSigner：gate 绑定客户端授权状态（客户端构造后再弱引用绑定，避免环）。
    let gate = Arc::new(ClientStateGate::default());
    let signer = match AuthSigner::new(
        Arc::clone(&device_signer),
        Arc::clone(&gate) as Arc<dyn LicenseGate>,
        machine_code.clone(),
    ) {
        Ok(signer) => signer,
        Err(err) => {
            return AssemblyOutcome::Failed(format!(
                "auth signer rejected the machine code ({err}); license assembly cannot \
                 proceed. Recovery: verify the fingerprint pipeline and restart"
            ))
        }
    };

    // ⑤ LicensingClient：transport + device_signer + anchor_hashes（缺锚点集即激活 fail-closed）。
    let client_cfg = match LicensingClientConfig::new(
        cloud_url.clone(),
        (licensing.heartbeat_interval_secs / 3_600).max(1),
        DEFAULT_CONNECT_TIMEOUT_SECS,
        DEFAULT_REQUEST_TIMEOUT_SECS,
    ) {
        Ok(cfg) => cfg,
        Err(err) => {
            return AssemblyOutcome::Failed(format!(
                "licensing client config rejected ({err}); check [gateway.licensing].cloud_url \
                 and restart"
            ))
        }
    };
    let transport = input
        .transport
        .unwrap_or_else(|| Arc::new(HttpsTransport::production()) as Arc<dyn LicenseTransport>);
    let mut client =
        LicensingClient::with_transport(client_cfg, Arc::new(signer), machine_code, transport)
            .with_device_signer(Arc::clone(&device_signer))
            .with_anchor_hashes(anchor_hashes);
    let lease_keys = match input.lease_public_keys {
        Some(keys) => Some(keys),
        None => std::env::var(LICENSE_PUBLIC_KEYS_ENV)
            .ok()
            .map(|raw| parse_lease_public_keys(&raw)),
    };
    match lease_keys {
        Some(pairs) => {
            for (kid, public_key) in pairs {
                if let Err(err) = client.register_public_key(&kid, &public_key) {
                    warn!(kid = %kid, error = %err, "licensing assembly: skip invalid lease public key");
                }
            }
        }
        None => warn!(
            "licensing assembly: no lease public keys provisioned ({}); activation will \
             fail closed at local lease verification until keys are deployed",
            LICENSE_PUBLIC_KEYS_ENV
        ),
    }
    let client = Arc::new(client);
    gate.bind(Arc::downgrade(&client));

    // ⑥ LicenseRuntimeConfig：心跳 / 激活码 / data_dir（trial marker 同盘）。
    let mut rt_cfg = LicenseRuntimeConfig::new(client, input.data_dir)
        .with_cloud_url(cloud_url)
        .with_heartbeat_interval(Duration::from_secs(licensing.heartbeat_interval_secs));
    if let Some(code) = code {
        rt_cfg = rt_cfg.with_activation_code(code);
    }
    AssemblyOutcome::Assembled(rt_cfg)
}

/// 设备签名密钥默认路径：`data_dir/license/device-ed25519.key`（宿主持久卷）。
#[must_use]
pub fn default_device_key_path(data_dir: &Path) -> PathBuf {
    data_dir.join(DEVICE_KEY_SUBDIR).join(DEVICE_KEY_FILE)
}

/// 解析 Lease 公钥集文本：`kid1=BASE64,kid2=BASE64`（分隔符 `,` / `;`；非法项跳过并告警）。
fn parse_lease_public_keys(raw: &str) -> Vec<(String, Vec<u8>)> {
    raw.split([',', ';'])
        .filter_map(|entry| {
            let entry = entry.trim();
            if entry.is_empty() {
                return None;
            }
            let (kid, b64) = match entry.split_once('=') {
                Some(pair) => pair,
                None => {
                    warn!(
                        entry = "len-redacted",
                        "licensing assembly: lease public key entry has no `kid=` prefix; skipped"
                    );
                    return None;
                }
            };
            let kid = kid.trim().to_string();
            if kid.is_empty() {
                warn!("licensing assembly: lease public key has empty kid; skipped");
                return None;
            }
            match B64.decode(b64.trim().as_bytes()) {
                Ok(bytes) if !bytes.is_empty() => Some((kid, bytes)),
                Ok(_) => {
                    warn!(kid = %kid, "licensing assembly: lease public key is empty; skipped");
                    None
                }
                Err(err) => {
                    warn!(kid = %kid, error = %err, "licensing assembly: lease public key is not valid base64; skipped");
                    None
                }
            }
        })
        .collect()
}

/// 绑定 [`LicensingClient`] 授权状态的签名闸门（弱引用，避免 client ⇄ signer 环）。
///
/// 未绑定 / 客户端已释放 / 锁中毒 → `false`（fail-closed，绝不放行未授权签名）。
#[derive(Default)]
struct ClientStateGate {
    client: Mutex<Option<Weak<LicensingClient>>>,
}

impl ClientStateGate {
    /// 客户端构造完成后绑定（弱引用；不延长生命周期）。
    fn bind(&self, weak: Weak<LicensingClient>) {
        match self.client.lock() {
            Ok(mut guard) => *guard = Some(weak),
            Err(poisoned) => *poisoned.into_inner() = Some(weak),
        }
    }
}

impl LicenseGate for ClientStateGate {
    fn can_sign(&self) -> bool {
        let guard = match self.client.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        guard
            .as_ref()
            .and_then(Weak::upgrade)
            .is_some_and(|client| client.current_state().can_sign())
    }
}

// ---- 生产锚点集 ----

/// 生产锚点提供者集：Windows 命令锚点（5）/ Linux 文件锚点（原生 4 或容器宿主 4
/// + 宿主 MAC env + 签名指纹文件降级）。锚点缺失一律返回 `None`（不计入 quorum）。
#[must_use]
pub fn production_anchor_providers(data_dir: &Path) -> Vec<Box<dyn AnchorProvider>> {
    #[cfg(windows)]
    {
        let _ = data_dir; // Windows 锚点走命令采集，无需 data_dir。
        windows_command_anchors()
    }
    #[cfg(not(windows))]
    {
        linux_anchor_providers(data_dir)
    }
}

/// Linux 锚点集：容器形态（宿主锚点可读）只用宿主锚点（**红线**：绝不采容器内
/// `/etc/machine-id`）；原生形态读本机路径。宿主 MAC / 签名指纹文件作降级锚点。
#[cfg(not(windows))]
fn linux_anchor_providers(data_dir: &Path) -> Vec<Box<dyn AnchorProvider>> {
    use crate::auth::machine_id::{EnvAnchor, FileAnchor};

    let host_root = host_anchor_root();
    // 宿主锚点存在 ⇒ 容器形态：etc/sys 根都切到宿主挂载。
    let host_machine_id = host_root.join("etc").join("machine-id");
    let (etc_root, sys_root) = if host_machine_id.is_file() {
        (host_root.join("etc"), host_root.join("sys"))
    } else {
        (PathBuf::from("/etc"), PathBuf::from("/sys"))
    };
    let dmi = sys_root.join("class").join("dmi").join("id");
    let fingerprint_file = std::env::var(HOST_FINGERPRINT_FILE_ENV)
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| data_dir.join("host-fingerprint.json"));

    vec![
        Box::new(FileAnchor::new(
            "etc-machine-id",
            etc_root.join("machine-id"),
        )),
        Box::new(FileAnchor::new(
            "dmi-product-uuid",
            dmi.join("product_uuid"),
        )),
        Box::new(FileAnchor::new(
            "dmi-product-serial",
            dmi.join("product_serial"),
        )),
        Box::new(FileAnchor::new(
            "dmi-board-serial",
            dmi.join("board_serial"),
        )),
        Box::new(EnvAnchor::new("host-mac", HOST_MAC_ENV)),
        Box::new(FileAnchor::new("host-fingerprint", fingerprint_file)),
    ]
}

/// 宿主锚点根解析（与 bin preflight 的 `anchor_root()` 口径一致）：
/// `IOT_DAQ_HOST_ANCHOR_ROOT` → 由 `IOT_DAQ_HOST_MACHINE_ID_PATH` 反推 → `/host`。
#[cfg(not(windows))]
fn host_anchor_root() -> PathBuf {
    if let Ok(root) = std::env::var(HOST_ANCHOR_ROOT_ENV) {
        let root = root.trim();
        if !root.is_empty() {
            return PathBuf::from(root);
        }
    }
    if let Ok(path) = std::env::var(HOST_MACHINE_ID_PATH_ENV) {
        let path = PathBuf::from(path.trim());
        if let Some(root) = path.parent().and_then(Path::parent) {
            if !root.as_os_str().is_empty() {
                return root.to_path_buf();
            }
        }
    }
    PathBuf::from("/host")
}

/// Windows 锚点集（`PLATFORM_ANCHOR_PLANS` Windows 计划，5 锚点）：注册表 / WMI / 卷序列号。
///
/// V1 限制：`wmic` 在 Win11 24H2 起缺省移除——对应锚点采集失败记 `None`，
/// 由 quorum 与其余锚点兜底；WMI PowerShell 化留待平台差异任务。
#[cfg(windows)]
fn windows_command_anchors() -> Vec<Box<dyn AnchorProvider>> {
    vec![
        Box::new(CommandAnchor::new(
            "machine-guid",
            "reg",
            &[
                "query",
                r"HKLM\SOFTWARE\Microsoft\Cryptography",
                "/v",
                "MachineGuid",
            ],
        )),
        Box::new(CommandAnchor::new(
            "csproduct-uuid",
            "wmic",
            &["csproduct", "get", "UUID"],
        )),
        Box::new(CommandAnchor::new(
            "bios-serial",
            "wmic",
            &["bios", "get", "SerialNumber"],
        )),
        Box::new(CommandAnchor::new(
            "baseboard-serial",
            "wmic",
            &["baseboard", "get", "SerialNumber"],
        )),
        Box::new(CommandAnchor::new(
            "volume-serial",
            "cmd",
            &["/C", "vol C:"],
        )),
    ]
}

/// 只读命令型锚点（Windows）：执行固定命令、归一 stdout 为锚点值；
/// 命令失败 / 非零退出 / 空输出 → `None`（锚点不可用，不计入 quorum）。
#[cfg(windows)]
struct CommandAnchor {
    name: &'static str,
    program: &'static str,
    args: &'static [&'static str],
}

#[cfg(windows)]
impl CommandAnchor {
    fn new(name: &'static str, program: &'static str, args: &'static [&'static str]) -> Self {
        Self {
            name,
            program,
            args,
        }
    }
}

#[cfg(windows)]
impl AnchorProvider for CommandAnchor {
    fn name(&self) -> &'static str {
        self.name
    }

    fn collect(&self) -> Option<String> {
        let output = std::process::Command::new(self.program)
            .args(self.args)
            .output()
            .ok()?;
        if !output.status.success() {
            return None;
        }
        let text = String::from_utf8(output.stdout).ok()?;
        // 归一（machine-fingerprint.md §2：trim / 大小写统一 / 分隔符统一）：
        // 逐行 trim、去空行、统一小写后按行拼接——确定性且不回显到任何日志。
        let mut lines: Vec<&str> = text
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .collect();
        lines.sort_unstable();
        let normalized = lines.join("\n").to_lowercase();
        (!normalized.is_empty()).then_some(normalized)
    }
}

// ---- 生产 HTTPS transport ----

/// 工作线程作业（post_json 的一次请求）。
struct TransportJob {
    url: String,
    body: serde_json::Value,
    reply: SyncSender<Result<serde_json::Value, DaemonError>>,
}

/// 生产 HTTPS transport：独立工作线程承载手写 HTTP/1.1 客户端（同步 trait 适配）。
///
/// - 每请求独立连接（激活 / 24h 心跳为低频路径，无需连接池）；
/// - `http` / `https` 双 scheme；https 用 rustls（`ring` provider + 操作系统根证书库，
///   无「跳过校验」路径）；IPv6 字面量 V1 不支持（显式报错，不猜测意图）；
/// - 总超时 = connect + request；响应体限长 [`MAX_RESPONSE_BYTES`]；
/// - 4xx → `AuthError`（服务端明确拒绝）；5xx / 3xx / 传输层 → `NetworkError`（可重试）；
///   2xx 非 JSON → `SecurityError`（fail-closed）。
/// - 依赖纪律：不引 hyper `client`（其特性需 Cargo.lock 之外的 `want` crate，
///   违反离线零新增依赖红线）——HTTP/1.1 客户端手写于本模块（[`send_http_request`]）。
pub struct HttpsTransport {
    job_tx: std::sync::mpsc::Sender<TransportJob>,
}

impl HttpsTransport {
    /// 构造并启动工作线程（线程启动失败 → 后续请求全部 `NetworkError`，不 panic）。
    #[must_use]
    pub fn new(connect_timeout: Duration, request_timeout: Duration) -> Self {
        let (job_tx, job_rx) = channel::<TransportJob>();
        let spawned = std::thread::Builder::new()
            .name("licensing-https".to_string())
            .spawn(move || transport_worker(job_rx, connect_timeout, request_timeout));
        if let Err(err) = spawned {
            error!(
                error = %err,
                "licensing transport: worker thread failed to start; all requests will fail"
            );
            // job_rx 随闭包丢弃 → job_tx.send 失败 → NetworkError（fail-closed）。
        }
        Self { job_tx }
    }

    /// 生产参数（连接 10s / 请求 30s，与 `LicensingClientConfig` 默认一致）。
    #[must_use]
    pub fn production() -> Self {
        Self::new(
            Duration::from_secs(DEFAULT_CONNECT_TIMEOUT_SECS),
            Duration::from_secs(DEFAULT_REQUEST_TIMEOUT_SECS),
        )
    }
}

impl LicenseTransport for HttpsTransport {
    fn post_json(
        &self,
        url: &str,
        body: &serde_json::Value,
    ) -> Result<serde_json::Value, DaemonError> {
        let (reply_tx, reply_rx) = sync_channel::<Result<serde_json::Value, DaemonError>>(1);
        self.job_tx
            .send(TransportJob {
                url: url.to_string(),
                body: serde_json::Value::clone(body),
                reply: reply_tx,
            })
            .map_err(|_| {
                DaemonError::NetworkError("licensing transport worker is unavailable".to_string())
            })?;
        let wait = || match reply_rx.recv() {
            Ok(result) => result,
            // 工作线程已退出（回复端被丢弃）：按网络错误处理（fail-closed、可重试）。
            Err(_) => Err(DaemonError::NetworkError(
                "licensing transport worker dropped the reply".to_string(),
            )),
        };
        // 本调用可能发生在 tokio worker 上（`LicenseRuntime::step` → `activate`）：
        // 多线程 runtime 内 `block_in_place` 让出 worker（其余任务不受阻塞）；
        // current_thread / 纯同步上下文直接等待（低频路径，超时有限）。
        match tokio::runtime::Handle::try_current() {
            Ok(handle) if handle.runtime_flavor() == tokio::runtime::RuntimeFlavor::MultiThread => {
                tokio::task::block_in_place(wait)
            }
            _ => wait(),
        }
    }
}

impl std::fmt::Debug for HttpsTransport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HttpsTransport").finish_non_exhaustive()
    }
}

/// 工作线程主循环：current_thread runtime 上逐个执行作业（串行——低频路径足够）。
fn transport_worker(
    job_rx: Receiver<TransportJob>,
    connect_timeout: Duration,
    request_timeout: Duration,
) {
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(err) => {
            error!(error = %err, "licensing transport: worker runtime build failed");
            // runtime 不可用：逐个回复网络错误（不 panic、不静默）。
            for job in job_rx {
                let _ = job.reply.send(Err(DaemonError::NetworkError(format!(
                    "licensing transport worker runtime unavailable: {err}"
                ))));
            }
            return;
        }
    };
    for job in job_rx {
        let result = runtime.block_on(execute_request(
            &job.url,
            &job.body,
            connect_timeout,
            request_timeout,
        ));
        // 回复端已放弃（调用方超时 / 关闭）时发送失败——静默丢弃即可。
        let _ = job.reply.send(result);
    }
}

/// URL 解析结果（scheme / host / port / path）。
#[derive(Debug, Clone, PartialEq, Eq)]
struct Endpoint {
    use_tls: bool,
    host: String,
    port: u16,
    path: String,
    authority: String,
}

/// 手工解析授权服务 URL（`http(s)://host[:port]/path`）。
///
/// V1 限制：不支持 IPv6 字面量与 userinfo（显式报错，绝不猜测意图）。
fn parse_endpoint(url: &str) -> Result<Endpoint, DaemonError> {
    let invalid = |reason: &str| {
        DaemonError::NetworkError(format!("licensing url invalid ({reason}): {url}"))
    };
    let (scheme, rest) = url
        .split_once("://")
        .ok_or_else(|| invalid("missing scheme"))?;
    let use_tls = match scheme {
        "http" => false,
        "https" => true,
        other => {
            return Err(invalid(&format!(
                "unsupported scheme `{other}` (use http/https)"
            )))
        }
    };
    let (authority, path) = match rest.find('/') {
        Some(idx) => (&rest[..idx], rest[idx..].to_string()),
        None => (rest, "/".to_string()),
    };
    if authority.is_empty() {
        return Err(invalid("empty host"));
    }
    if authority.contains('@') {
        return Err(invalid("userinfo is not supported in V1"));
    }
    let (host, port) = match authority.rsplit_once(':') {
        Some((host, port)) => {
            let port = port
                .parse::<u16>()
                .map_err(|_| invalid("port is not a valid u16"))?;
            (host.to_string(), port)
        }
        None => (authority.to_string(), if use_tls { 443 } else { 80 }),
    };
    // 剩余冒号 / 方括号 ⇒ IPv6 字面量（V1 显式拒绝，不猜测意图）。
    if host.is_empty() || host.contains(['[', ':']) {
        return Err(invalid("IPv6 literals are not supported in V1"));
    }
    Ok(Endpoint {
        use_tls,
        host,
        port,
        path,
        authority: authority.to_string(),
    })
}

/// 执行一次 POST JSON（含连接 / TLS / 超时 / 响应限长 / 状态码映射）。
async fn execute_request(
    url: &str,
    body: &serde_json::Value,
    connect_timeout: Duration,
    request_timeout: Duration,
) -> Result<serde_json::Value, DaemonError> {
    let overall = connect_timeout.saturating_add(request_timeout);
    match tokio::time::timeout(
        overall,
        request_once(url, body, connect_timeout, request_timeout),
    )
    .await
    {
        Ok(result) => result,
        Err(_) => Err(DaemonError::NetworkError(format!(
            "licensing request timed out after {}ms: {url}",
            overall.as_millis()
        ))),
    }
}

/// 单次请求的异步主体（由 [`execute_request`] 包超时）。
async fn request_once(
    url: &str,
    body: &serde_json::Value,
    connect_timeout: Duration,
    request_timeout: Duration,
) -> Result<serde_json::Value, DaemonError> {
    let endpoint = parse_endpoint(url)?;
    ensure_ring_provider()?;

    let tcp = tokio::time::timeout(
        connect_timeout,
        tokio::net::TcpStream::connect((endpoint.host.as_str(), endpoint.port)),
    )
    .await
    .map_err(|_| DaemonError::NetworkError("licensing connect timed out".to_string()))?
    .map_err(|err| DaemonError::NetworkError(format!("licensing connect failed: {err}")))?;

    // http/https 分支建流（TLS：rustls + 操作系统根证书库，无跳过校验路径）。
    let io = if endpoint.use_tls {
        let connector = tokio_rustls::TlsConnector::from(build_tls_client_config()?);
        let server_name =
            rustls::pki_types::ServerName::try_from(endpoint.host.clone()).map_err(|err| {
                DaemonError::NetworkError(format!("licensing tls server name invalid: {err}"))
            })?;
        let tls = tokio::time::timeout(connect_timeout, connector.connect(server_name, tcp))
            .await
            .map_err(|_| {
                DaemonError::NetworkError("licensing tls handshake timed out".to_string())
            })?
            .map_err(|err| {
                DaemonError::NetworkError(format!("licensing tls handshake failed: {err}"))
            })?;
        Conn::Tls(Box::new(tls))
    } else {
        Conn::Plain(tcp)
    };

    let payload = serde_json::to_vec(body).map_err(|err| {
        DaemonError::NetworkError(format!("licensing request serialize failed: {err}"))
    })?;
    let (status, body_bytes) =
        tokio::time::timeout(request_timeout, send_http_request(io, &endpoint, &payload))
            .await
            .map_err(|_| DaemonError::NetworkError("licensing request timed out".to_string()))??;

    if (400..500).contains(&status) {
        // 服务端明确拒绝：携带错误码提示（响应体是 JSON 错误码，非敏感）。
        Err(DaemonError::AuthError(format!(
            "licensing server rejected the request: HTTP {status}{}",
            server_error_hint(&body_bytes)
        )))
    } else if !(200..300).contains(&status) {
        // 5xx / 3xx / 1xx：可重试的传输 / 服务端问题（3xx 不跟随，授权服务不做重定向）。
        Err(DaemonError::NetworkError(format!(
            "licensing server returned HTTP {status} (transport-level; retryable)"
        )))
    } else {
        serde_json::from_slice(&body_bytes).map_err(|_| {
            DaemonError::SecurityError(
                "licensing response body is not valid JSON (possible tampering); \
                 refusing to process"
                    .to_string(),
            )
        })
    }
}

/// 连接抽象（TCP 明文 / TLS 二选一；避免泛型分发与 dyn 兼容性问题）。
enum Conn {
    /// 明文 HTTP 连接。
    Plain(tokio::net::TcpStream),
    /// TLS 连接（rustls 客户端流）。
    Tls(Box<tokio_rustls::client::TlsStream<tokio::net::TcpStream>>),
}

impl Conn {
    async fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        match self {
            Conn::Plain(stream) => stream.read(buf).await,
            Conn::Tls(stream) => stream.read(buf).await,
        }
    }

    async fn write_all(&mut self, buf: &[u8]) -> std::io::Result<()> {
        match self {
            Conn::Plain(stream) => stream.write_all(buf).await,
            Conn::Tls(stream) => stream.write_all(buf).await,
        }
    }

    async fn flush(&mut self) -> std::io::Result<()> {
        match self {
            Conn::Plain(stream) => stream.flush().await,
            Conn::Tls(stream) => stream.flush().await,
        }
    }
}

/// 响应头区最大字节数（防异常服务端无限头发包）。
const MAX_HEADER_BYTES: usize = 16 * 1024;

/// 发送 HTTP/1.1 POST JSON 并读取响应（`Connection: close` 单请求连接）。
///
/// 手写客户端（依赖红线：hyper `client` 特性需 Cargo.lock 之外的 `want` crate，
/// 离线零新增依赖故不引入；授权服务为固定 axum 后端，HTTP/1.1 足够）。
/// 定界支持：`Content-Length` / `Transfer-Encoding: chunked` / 连接关闭兜底；
/// 全程钳制 [`MAX_RESPONSE_BYTES`]。
async fn send_http_request(
    mut io: Conn,
    endpoint: &Endpoint,
    payload: &[u8],
) -> Result<(u16, Vec<u8>), DaemonError> {
    let transport_err = |reason: String| DaemonError::NetworkError(reason);
    let request = format!(
        "POST {path} HTTP/1.1\r\nHost: {authority}\r\nContent-Type: application/json\r\n\
         Content-Length: {len}\r\nConnection: close\r\nUser-Agent: iot-daq-daemon-licensing/1\r\n\r\n",
        path = endpoint.path,
        authority = endpoint.authority,
        len = payload.len(),
    );
    io.write_all(request.as_bytes())
        .await
        .map_err(|err| transport_err(format!("licensing request write failed: {err}")))?;
    io.write_all(payload)
        .await
        .map_err(|err| transport_err(format!("licensing request write failed: {err}")))?;
    io.flush()
        .await
        .map_err(|err| transport_err(format!("licensing request flush failed: {err}")))?;

    // ① 读到响应头结束（\r\n\r\n）。
    let mut buf: Vec<u8> = Vec::with_capacity(2048);
    let header_end = loop {
        if buf.len() > MAX_HEADER_BYTES {
            return Err(transport_err(
                "licensing response headers exceed the size limit".to_string(),
            ));
        }
        let mut chunk = [0u8; 4096];
        let n = io
            .read(&mut chunk)
            .await
            .map_err(|err| transport_err(format!("licensing response read failed: {err}")))?;
        if n == 0 {
            return Err(transport_err(
                "licensing connection closed before response headers completed".to_string(),
            ));
        }
        buf.extend_from_slice(&chunk[..n]);
        if let Some(pos) = find_subslice(&buf, b"\r\n\r\n") {
            break pos;
        }
    };

    // ② 解析响应头：状态码 + Content-Length / chunked 标记。
    let (status, content_length, chunked) = parse_response_head(&buf[..header_end])?;
    let mut body = buf[header_end + 4..].to_vec();

    // ③ 按 Content-Length / chunked / close-delimited 收齐响应体（全程限长）。
    if chunked {
        loop {
            if body.len() > MAX_RESPONSE_BYTES {
                return Err(transport_err(
                    "licensing response body exceeds the size limit".to_string(),
                ));
            }
            let mut chunk = [0u8; 4096];
            let n = io
                .read(&mut chunk)
                .await
                .map_err(|err| transport_err(format!("licensing response read failed: {err}")))?;
            if n == 0 {
                break;
            }
            body.extend_from_slice(&chunk[..n]);
        }
        body = decode_chunked(&body)?;
    } else if let Some(len) = content_length {
        // Content-Length 钳到响应体上限（超出即视为传输层异常）。
        let len = usize::try_from(len).unwrap_or(usize::MAX);
        if len > MAX_RESPONSE_BYTES {
            return Err(transport_err(
                "licensing response body exceeds the size limit".to_string(),
            ));
        }
        while body.len() < len {
            if body.len() > MAX_RESPONSE_BYTES {
                return Err(transport_err(
                    "licensing response body exceeds the size limit".to_string(),
                ));
            }
            let mut chunk = [0u8; 4096];
            let n = io
                .read(&mut chunk)
                .await
                .map_err(|err| transport_err(format!("licensing response read failed: {err}")))?;
            if n == 0 {
                return Err(transport_err(
                    "licensing connection closed before response body completed".to_string(),
                ));
            }
            body.extend_from_slice(&chunk[..n]);
        }
        body.truncate(len);
    } else {
        // close-delimited（服务器随响应关闭连接）。
        loop {
            if body.len() > MAX_RESPONSE_BYTES {
                return Err(transport_err(
                    "licensing response body exceeds the size limit".to_string(),
                ));
            }
            let mut chunk = [0u8; 4096];
            let n = io
                .read(&mut chunk)
                .await
                .map_err(|err| transport_err(format!("licensing response read failed: {err}")))?;
            if n == 0 {
                break;
            }
            body.extend_from_slice(&chunk[..n]);
        }
    }

    Ok((status, body))
}

/// 在缓冲中查找子串首个偏移（`memmem` 不在依赖树，手写足够——缓冲有界）。
fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

/// 解析 HTTP 响应头区：状态行 + 逐头（大小写不敏感）；返回
/// `(status, content_length, is_chunked)`。
fn parse_response_head(head: &[u8]) -> Result<(u16, Option<u64>, bool), DaemonError> {
    let text = std::str::from_utf8(head).map_err(|_| {
        DaemonError::NetworkError("licensing response head is not UTF-8".to_string())
    })?;
    let mut lines = text.split("\r\n");
    let status_line = lines.next().ok_or_else(|| {
        DaemonError::NetworkError("licensing response has no status line".to_string())
    })?;
    // `HTTP/1.1 200 OK` → 取第二个空格分隔段为状态码。
    let status = status_line
        .split(' ')
        .nth(1)
        .and_then(|code| code.trim().parse::<u16>().ok())
        .ok_or_else(|| {
            DaemonError::NetworkError(format!(
                "licensing response status line invalid: {status_line}"
            ))
        })?;
    let mut content_length: Option<u64> = None;
    let mut chunked = false;
    for line in lines {
        if line.is_empty() {
            continue;
        }
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        let name = name.trim().to_ascii_lowercase();
        let value = value.trim();
        match name.as_str() {
            "content-length" => {
                content_length = value.parse::<u64>().ok();
            }
            "transfer-encoding" => {
                chunked = value.to_ascii_lowercase().contains("chunked");
            }
            _ => {}
        }
    }
    Ok((status, content_length, chunked))
}

/// 解码 chunked 响应体（`<hex size>[;ext]\r\n<data>\r\n` … `0\r\n`；trailer 忽略）。
fn decode_chunked(raw: &[u8]) -> Result<Vec<u8>, DaemonError> {
    let err = |reason: &str| {
        DaemonError::NetworkError(format!("licensing chunked response invalid: {reason}"))
    };
    let mut out = Vec::with_capacity(raw.len());
    let mut cursor = 0usize;
    loop {
        let Some(line_end) = find_subslice(&raw[cursor..], b"\r\n") else {
            return Err(err("missing chunk size terminator"));
        };
        let size_line = std::str::from_utf8(&raw[cursor..cursor + line_end])
            .map_err(|_| err("chunk size is not UTF-8"))?;
        let size_str = size_line.split(';').next().unwrap_or("").trim();
        let size = usize::from_str_radix(size_str, 16).map_err(|_| err("chunk size is not hex"))?;
        cursor += line_end + 2;
        if size == 0 {
            return Ok(out); // 终止块（之后的 trailer 一并忽略）。
        }
        if cursor + size > raw.len() {
            return Err(err("chunk data truncated"));
        }
        if out.len() + size > MAX_RESPONSE_BYTES {
            return Err(err("reassembled body exceeds the size limit"));
        }
        out.extend_from_slice(&raw[cursor..cursor + size]);
        cursor += size;
        // 每个数据块后必须有 \r\n。
        if raw.get(cursor..cursor + 2) != Some(b"\r\n") {
            return Err(err("missing chunk data terminator"));
        }
        cursor += 2;
    }
}

/// 从服务端错误响应提取可读提示（`error` / `code` / `message` 字段；截断防膨胀）。
fn server_error_hint(body: &[u8]) -> String {
    let parsed: Option<serde_json::Value> = serde_json::from_slice(body).ok();
    let hint = parsed.as_ref().and_then(|value| {
        ["error", "code", "message"]
            .iter()
            .find_map(|key| value.get(*key).and_then(|v| v.as_str()))
    });
    match hint {
        Some(text) if !text.is_empty() => {
            let mut truncated: String = text.chars().take(120).collect();
            if text.chars().count() > 120 {
                truncated.push('…');
            }
            format!(" (server: {truncated})")
        }
        _ => String::new(),
    }
}

/// 安装 rustls `ring` CryptoProvider（幂等；与北向 mqtt 侧共用进程级默认）。
fn ensure_ring_provider() -> Result<(), DaemonError> {
    if rustls::crypto::CryptoProvider::get_default().is_some() {
        return Ok(());
    }
    match rustls::crypto::ring::default_provider().install_default() {
        Ok(()) => Ok(()),
        // 已有组件（北向 TLS）先装上了 → 只要确实存在即视为就绪。
        Err(_) if rustls::crypto::CryptoProvider::get_default().is_some() => Ok(()),
        Err(_) => Err(DaemonError::SecurityError(
            "licensing transport: failed to install a rustls CryptoProvider".to_string(),
        )),
    }
}

/// 构建 TLS 客户端配置：操作系统根证书库（`ca_cert_path` 概念不适用于授权服务——
/// 与北向出口「证书可选」决策一致；fail-closed：解析不到任何根证书即拒绝）。
fn build_tls_client_config() -> Result<Arc<rustls::ClientConfig>, DaemonError> {
    ensure_ring_provider()?;
    let mut roots = rustls::RootCertStore::empty();
    let loaded = rustls_native_certs::load_native_certs();
    for err in &loaded.errors {
        warn!(error = %err, "licensing tls: native root certificate load error (continuing)");
    }
    for cert in &loaded.certs {
        // 单张坏证书跳过（根库以「至少一张可用」为准）。
        let _ = roots.add(cert.clone());
    }
    if roots.is_empty() {
        return Err(DaemonError::SecurityError(
            "licensing tls: no system root certificates available; refusing to build an \
             unverified TLS client (no skip-verification path exists)"
                .to_string(),
        ));
    }
    let config = rustls::ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();
    Ok(Arc::new(config))
}

// ---------------------------------------------------------------------------
// 单元测试（装配判定 + URL 解析 + 公钥集解析；网络路径经集成测试覆盖）
// ---------------------------------------------------------------------------
#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::machine_id::StaticAnchor;

    /// test-only 指纹 key（禁止真实部署）。
    fn test_key() -> FingerprintKey {
        FingerprintKey::from_bytes(b"TEST_ONLY_assembly_fp_key".to_vec())
            .expect("test-only key non-empty")
    }

    /// 两个可用锚点（满足 PRODUCTION_MIN_ANCHORS=2 的最小集合）。
    fn two_anchors() -> Vec<Box<dyn AnchorProvider>> {
        vec![
            Box::new(StaticAnchor::new("assembly-anchor-a", Some("value-a"))),
            Box::new(StaticAnchor::new("assembly-anchor-b", Some("value-b"))),
        ]
    }

    fn licensing_section() -> LicensingSection {
        LicensingSection {
            cloud_url: Some("https://licensing.test/v1".to_string()),
            heartbeat_interval_secs: 86_400,
            activation_code: Some("ACT-TEST-0001".to_string()),
            ..LicensingSection::default()
        }
    }

    fn temp_dir() -> tempfile::TempDir {
        tempfile::tempdir().expect("tempdir")
    }

    /// 未配置（无 cloud_url 且无 activation_code）→ NotConfigured（保持现行为）。
    #[test]
    fn not_configured_when_licensing_section_empty() {
        let dir = temp_dir();
        let outcome = assemble(AssemblyInput {
            licensing: &LicensingSection::default(),
            data_dir: dir.path(),
            transport: None,
            anchor_providers: Some(two_anchors()),
            fingerprint_key: Some(test_key()),
            device_key_path: Some(dir.path().join("device.key")),
            lease_public_keys: Some(Vec::new()),
        });
        assert!(matches!(outcome, AssemblyOutcome::NotConfigured));
    }

    /// 码有而址无 → 显式 fail-closed（含字段名与恢复路径，绝不猜测意图）。
    #[test]
    fn fails_when_code_set_without_cloud_url() {
        let dir = temp_dir();
        let licensing = LicensingSection {
            activation_code: Some("ACT-TEST-0001".to_string()),
            ..LicensingSection::default()
        };
        let AssemblyOutcome::Failed(reason) = assemble(AssemblyInput {
            licensing: &licensing,
            data_dir: dir.path(),
            transport: None,
            anchor_providers: Some(two_anchors()),
            fingerprint_key: Some(test_key()),
            device_key_path: Some(dir.path().join("device.key")),
            lease_public_keys: Some(Vec::new()),
        }) else {
            panic!("expected Failed");
        };
        assert!(reason.contains("cloud_url"), "field name: {reason}");
        assert!(reason.contains("Recovery"), "recovery path: {reason}");
        // 激活码绝不进错误消息。
        assert!(!reason.contains("ACT-TEST-0001"), "code leaked: {reason}");
    }

    /// 指纹 key 缺失 → fail-closed，错误只引用环境变量名（绝不引用值）。
    #[test]
    fn fails_closed_when_fingerprint_key_missing() {
        let dir = temp_dir();
        // 仅当该变量确实未设置时才可断言；CI 环境不应设置此变量。
        if std::env::var(FINGERPRINT_KEY_ENV).is_ok() {
            return;
        }
        let AssemblyOutcome::Failed(reason) = assemble(AssemblyInput {
            licensing: &licensing_section(),
            data_dir: dir.path(),
            transport: None,
            anchor_providers: Some(two_anchors()),
            fingerprint_key: None,
            device_key_path: Some(dir.path().join("device.key")),
            lease_public_keys: Some(Vec::new()),
        }) else {
            panic!("expected Failed");
        };
        assert!(reason.contains(FINGERPRINT_KEY_ENV), "env name: {reason}");
        assert!(reason.contains("Recovery"), "recovery path: {reason}");
    }

    /// 锚点 quorum 不足（1 < 2）→ fail-closed、可解释（含 quorum 数字与恢复路径）。
    #[test]
    fn fails_closed_on_anchor_quorum_shortfall() {
        let dir = temp_dir();
        let AssemblyOutcome::Failed(reason) = assemble(AssemblyInput {
            licensing: &licensing_section(),
            data_dir: dir.path(),
            transport: None,
            anchor_providers: Some(vec![Box::new(StaticAnchor::new(
                "only-anchor",
                Some("value"),
            ))]),
            fingerprint_key: Some(test_key()),
            device_key_path: Some(dir.path().join("device.key")),
            lease_public_keys: Some(Vec::new()),
        }) else {
            panic!("expected Failed");
        };
        assert!(reason.contains("quorum"), "quorum detail: {reason}");
        assert!(reason.contains("Recovery"), "recovery path: {reason}");
    }

    /// 设备密钥容器损坏 → fail-closed（绝不静默重建密钥，防克隆）。
    #[test]
    fn fails_closed_on_corrupt_device_key() {
        let dir = temp_dir();
        let key_path = dir.path().join("device.key");
        std::fs::write(&key_path, b"not-a-key-container").expect("write garbage");
        let AssemblyOutcome::Failed(reason) = assemble(AssemblyInput {
            licensing: &licensing_section(),
            data_dir: dir.path(),
            transport: None,
            anchor_providers: Some(two_anchors()),
            fingerprint_key: Some(test_key()),
            device_key_path: Some(key_path),
            lease_public_keys: Some(Vec::new()),
        }) else {
            panic!("expected Failed");
        };
        assert!(reason.contains("device signing key"), "reason: {reason}");
        assert!(reason.contains("Recovery"), "recovery path: {reason}");
    }

    /// Happy：注入密钥 / 锚点 / transport → 装配成功；machine_code 与指纹模块一致；
    /// 设备密钥持久化在 data_dir/license/ 默认路径；试用标记同盘（红线 #13）。
    #[test]
    fn happy_path_assembles_client_and_runtime_config() {
        let dir = temp_dir();
        let data_dir = dir.path().join("data");
        let outcome = assemble(AssemblyInput {
            licensing: &licensing_section(),
            data_dir: &data_dir,
            transport: Some(Arc::new(crate::auth::client::UnavailableTransport)),
            anchor_providers: Some(two_anchors()),
            fingerprint_key: Some(test_key()),
            device_key_path: None, // 走默认路径 data_dir/license/device-ed25519.key
            lease_public_keys: Some(Vec::new()),
        });
        let AssemblyOutcome::Assembled(rt_cfg) = outcome else {
            panic!("expected Assembled");
        };
        let expected_mid = MachineIdentity::new(two_anchors(), PRODUCTION_MIN_ANCHORS, test_key())
            .get_machine_fingerprint()
            .expect("quorum ok");
        assert_eq!(rt_cfg.client.machine_code(), expected_mid);
        assert_eq!(
            rt_cfg.cloud_url.as_deref(),
            Some("https://licensing.test/v1")
        );
        assert_eq!(rt_cfg.activation_code.as_deref(), Some("ACT-TEST-0001"));
        assert_eq!(
            rt_cfg.heartbeat_interval,
            Duration::from_secs(86_400),
            "heartbeat from config"
        );
        assert!(
            default_device_key_path(&data_dir).is_file(),
            "device key persisted under data_dir/license/"
        );
        assert_eq!(
            rt_cfg.data_dir, data_dir,
            "trial marker dir must come from gateway.data_dir"
        );
    }

    /// lease 公钥集解析：合法项保留、非法项逐个跳过（不整体失败）。
    #[test]
    fn parse_lease_public_keys_skips_invalid_entries() {
        let valid = B64.encode([0x42u8; 32]);
        let keys = parse_lease_public_keys(&format!(
            "kid-a={valid};broken;=no-kid;kid-b=%%%notb64%%%,,kid-c="
        ));
        assert_eq!(keys.len(), 1, "only the valid entry survives: {keys:?}");
        assert_eq!(keys[0].0, "kid-a");
        assert_eq!(keys[0].1, vec![0x42u8; 32]);
    }

    /// URL 解析：scheme / 默认端口 / 显式端口 / path 缺省 / 非法 scheme / IPv6 拒绝。
    #[test]
    fn parse_endpoint_covers_happy_and_error_paths() {
        let plain = parse_endpoint("http://lic.test/v1/activate").expect("ok");
        assert_eq!(
            plain,
            Endpoint {
                use_tls: false,
                host: "lic.test".to_string(),
                port: 80,
                path: "/v1/activate".to_string(),
                authority: "lic.test".to_string(),
            }
        );
        let https_default = parse_endpoint("https://lic.test").expect("ok");
        assert_eq!(https_default.port, 443);
        assert_eq!(https_default.path, "/");
        let explicit = parse_endpoint("http://lic.test:8443/x").expect("ok");
        assert_eq!(explicit.port, 8443);

        assert!(parse_endpoint("lic.test/v1").is_err(), "missing scheme");
        assert!(parse_endpoint("ftp://lic.test").is_err(), "bad scheme");
        assert!(
            parse_endpoint("http://[::1]:8080/x").is_err(),
            "IPv6 V1 unsupported"
        );
        assert!(parse_endpoint("http:///x").is_err(), "empty host");
    }

    /// 响应头解析：状态码 / Content-Length / Transfer-Encoding（大小写不敏感）。
    #[test]
    fn parse_response_head_extracts_status_and_framing() {
        let head = b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 42\r\n";
        let (status, len, chunked) = parse_response_head(head).expect("parse");
        assert_eq!(status, 200);
        assert_eq!(len, Some(42));
        assert!(!chunked);

        let head = b"HTTP/1.1 403 Forbidden\r\nTRANSFER-ENCODING: Chunked\r\n";
        let (status, len, chunked) = parse_response_head(head).expect("parse");
        assert_eq!(status, 403);
        assert_eq!(len, None);
        assert!(chunked);

        let (status, ..) =
            parse_response_head(b"HTTP/1.1 500 Internal Server Error\r\n").expect("parse");
        assert_eq!(status, 500);
        assert!(
            parse_response_head(b"GARBAGE\r\n").is_err(),
            "no status code"
        );
    }

    /// chunked 解码：多块拼接、扩展位忽略、终止块截断、非法块报错。
    #[test]
    fn decode_chunked_reassembles_and_rejects_garbage() {
        let raw = b"4\r\nWiki\r\n5\r\npedia\r\n0\r\n\r\n";
        assert_eq!(decode_chunked(raw).expect("decode"), b"Wikipedia".to_vec());

        // 带扩展位的块大小。
        let raw = b"3;ext=1\r\nabc\r\n0\r\n\r\n";
        assert_eq!(decode_chunked(raw).expect("decode"), b"abc".to_vec());

        // 块数据被截断 → 报错。
        assert!(decode_chunked(b"10\r\nshort\r\n").is_err());
        // 块大小非 hex → 报错。
        assert!(decode_chunked(b"zz\r\nabc\r\n0\r\n\r\n").is_err());
    }
}

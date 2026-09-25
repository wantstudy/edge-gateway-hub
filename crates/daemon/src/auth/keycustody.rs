//! `auth::keycustody` — 客户端密钥托管后端（计划 task 49：OS 安全持久化 + 重装恢复）。
//!
//! 为 [`crate::auth::keyprovider`] 的持久化层提供**托管封装**（custody）：
//! 密钥容器字节在落盘前经 [`KeyCustody::seal`] 包装，加载时经 [`KeyCustody::open`]
//! 解封。密钥本体仍由 keyprovider 的 HKDF 机器码绑定容器保护——custody 是
//! 其外的第二道 OS 级防线。
//!
//! # 后端选择（env [`KEY_CUSTODY_ENV`] = `auto|dpapi|file`，默认 `auto`）
//! - **Windows（auto → dpapi）**：DPAPI（`CryptProtectData` /
//!   `CryptUnprotectData`，用户域）包装后落盘。封装格式见下。
//! - **非 Windows（auto → file）**：维持 0600 裸容器文件托管（数据目录在宿主
//!   持久卷）。**设计决策（记录于模块头，硬性约束）**：Linux 桌面 keyring /
//!   Secret Service 依赖 dbus 会话总线，交付形态为 headless 容器（USER=65532，
//!   无 dbus）——**明确不引入**任何 keyring 依赖；服务端服务侧密钥（如
//!   tpm2-tools）同样超出纯 Rust 红线。非 Windows 的安全边界由
//!   keyprovider 的机器码绑定容器 + 0600 文件权限承担。
//! - `file`：各平台可用（透传封装），供容器场景固定行为（确定性、无 OS 依赖）。
//! - `dpapi`：仅 Windows；非 Windows 显式报错（[`CustodyError::UnsupportedMode`]）。
//!
//! # 封装格式（dpapi 后端，变长）
//! ```text
//! CUSTODY_MAGIC(8 "IOTDAQKC") || ver(1) || dpapi_blob(variable)
//! ```
//! - `dpapi_blob = CryptProtectData(容器字节)`（用户域；换用户 / 换机必然解封失败）；
//! - `file` 后端不包装（`seal`/`open` 恒等）——落盘字节与旧版裸容器逐字节兼容。
//!
//! # 旧文件向后兼容
//! `file` 后端落盘的历史裸容器（无 custody 包装）在 `open` 时按原样透传，
//! 加载方（keyprovider）检测到未包装后按所选 custody **重新落盘并原子替换**
//! （升级幂等：`file` 下密文不变则跳过重写）。
//!
//! # 重装恢复语义（fail-closed，硬性红线）
//! - 密钥丢失 / 解封失败（[`CustodyError::OpenFailed`]）→ 装配失败 → 北向闸门
//!   关闭；恢复路径为**重新激活**（云端激活流程重新 provision 一把新密钥）。
//! - **绝不静默重建密钥**——静默重建会让云端一机一码绑定失效（防克隆红线）。
//! - 错误为**结构化变体**（调用方用 `matches!` 判别，禁 `msg.contains` 字符串
//!   匹配）；Display 文案自带恢复路径。
//! - **私钥红线**：密钥字节与解封结果禁止进日志——本模块错误消息只含错误码、
//!   长度等非敏感元数据，绝不回显任何密钥材料。
//!
//! # 依赖纪律
//! Windows DPAPI 走本仓既有的零依赖 FFI 模式（`hardening.rs` 先例：直接
//! `extern "system"` 声明所需函数，不引入 windows-sys / winapi）——
//! `crypt32` / `kernel32` 均为系统库，mingw 构建链自带导入库，纯 Rust 红线合规。

use std::io;
use std::sync::Arc;

use crate::error::DaemonError;

/// custody 选择开关环境变量（部署契约；值非敏感、可记录）。
pub const KEY_CUSTODY_ENV: &str = "IOT_DAQ_KEY_CUSTODY";

/// 托管封装魔数（域分隔：区分「dpapi 已包装」与「keyprovider 裸容器」）。
/// `pub(crate)`：keyprovider 侧测试断言包装格式时复用同一常量（禁硬编码重复）。
pub(crate) const CUSTODY_MAGIC: &[u8; 8] = b"IOTDAQKC";

/// 托管封装格式版本。
const CUSTODY_BLOB_VERSION: u8 = 1;

/// 封装头长：magic(8) + ver(1)。
const CUSTODY_HEADER_LEN: usize = 9;

/// `CRYPTPROTECT_UI_FORBIDDEN`：DPAPI 调用禁弹 UI（headless 服务红线）。
#[cfg(windows)]
const CRYPTPROTECT_UI_FORBIDDEN: u32 = 0x1;

/// 托管后端选择（env 解析产物；`auto` 按平台映射）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CustodyMode {
    /// 默认：Windows → dpapi，其余 → file。
    Auto,
    /// Windows DPAPI（用户域）；非 Windows 显式报错。
    Dpapi,
    /// 0600 裸容器文件（透传封装）；各平台可用。
    File,
}

impl CustodyMode {
    /// 解析 env 值（大小写不敏感；trim 后匹配）。
    ///
    /// # Errors
    /// 非法值返回 [`CustodyError::InvalidValue`]（值为模式名，非敏感、可记录）。
    pub fn parse(raw: &str) -> Result<Self, CustodyError> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "auto" => Ok(Self::Auto),
            "dpapi" => Ok(Self::Dpapi),
            "file" => Ok(Self::File),
            _ => Err(CustodyError::InvalidValue {
                value: raw.trim().to_string(),
            }),
        }
    }
}

/// 托管后端错误（零 panic：所有可失败点收敛于此；错误消息绝不含密钥材料）。
#[derive(Debug, thiserror::Error)]
pub enum CustodyError {
    /// env [`KEY_CUSTODY_ENV`] 值非法（仅接受 auto / dpapi / file）。
    #[error(
        "key custody mode invalid: `{value}` (accepted: auto|dpapi|file for \
         {env}); Recovery: set {env} to a supported value and restart",
        env = KEY_CUSTODY_ENV
    )]
    InvalidValue {
        /// 非法的模式名（模式名非敏感，可记录；绝不回显密钥材料）。
        value: String,
    },

    /// 所选后端在当前平台不可用（`dpapi` 用在非 Windows）。
    #[error(
        "key custody mode `dpapi` is not available on this platform (Windows only); \
         Recovery: set {env}=auto or {env}=file and restart",
        env = KEY_CUSTODY_ENV
    )]
    UnsupportedMode,

    /// 封装（落盘前）失败：OS 后端调用失败或输入超限。
    #[error(
        "device key custody seal failed ({reason}); refusing to persist the key. \
         Recovery: fix the underlying OS custody backend and restart; if the key \
         cannot be recovered, re-activate the device against the cloud licensing \
         service (activation re-provisions a fresh key)"
    )]
    SealFailed {
        /// 失败原因（仅 OS 错误码 / 长度等元数据，绝无密钥材料）。
        reason: String,
    },

    /// 解封（加载时）失败：OS 后端拒绝、封装结构损坏或跨用户 / 跨机迁移。
    ///
    /// fail-closed：**绝不静默重建密钥**（防克隆；恢复路径 = 重新激活）。
    #[error(
        "device key custody unseal failed ({reason}); refusing service — the device \
         key is never silently re-created (cloud one-key-per-device binding). \
         Recovery: re-activate the device against the cloud licensing service \
         (activation re-provisions a fresh key); if only the custody mode changed, \
         restore the previous {env} value and restart",
        env = KEY_CUSTODY_ENV
    )]
    OpenFailed {
        /// 失败原因（仅 OS 错误码 / 结构元数据，绝无密钥材料）。
        reason: String,
    },

    /// 托管落盘 / 读取的文件 IO 失败。
    #[error("device key custody io: {0}")]
    Io(#[from] io::Error),
}

impl From<CustodyError> for DaemonError {
    fn from(err: CustodyError) -> Self {
        match err {
            CustodyError::InvalidValue { .. } | CustodyError::UnsupportedMode => {
                DaemonError::ConfigError(err.to_string())
            }
            CustodyError::SealFailed { .. } | CustodyError::OpenFailed { .. } => {
                DaemonError::SecurityError(err.to_string())
            }
            CustodyError::Io(_) => DaemonError::StorageError(err.to_string()),
        }
    }
}

/// 密钥托管后端 trait：`seal`（落盘前包装）/ `open`（加载时解封），fail-closed。
///
/// 契约：实现方不得把密钥明文落盘（`file` 后端透传的前提是密钥已由
/// keyprovider 容器加密）、不得把任何密钥材料写日志；解封失败一律返回
/// 结构化错误（[`CustodyError::OpenFailed`]），绝不静默返回空数据或重建密钥。
pub trait KeyCustody: Send + Sync {
    /// 包装明文（keyprovider 容器字节）为托管密文。
    ///
    /// # Errors
    /// OS 后端失败 / 输入超限（[`CustodyError::SealFailed`]）时返回错误，不 panic。
    fn seal(&self, plaintext: &[u8]) -> Result<Vec<u8>, CustodyError>;

    /// 解封托管密文为明文（keyprovider 容器字节）。
    ///
    /// # Errors
    /// OS 后端拒绝 / 封装结构损坏 / 跨用户跨机迁移
    /// （[`CustodyError::OpenFailed`]）时返回错误，不 panic、不静默重建。
    fn open(&self, sealed: &[u8]) -> Result<Vec<u8>, CustodyError>;

    /// 判断落盘字节是否为**本后端**已包装格式（供加载方决定是否升级重写）。
    ///
    /// 默认实现按托管魔数判定（dpapi 后端适用）；透传型后端（file）覆写为
    /// 恒 `false`。
    fn is_sealed(&self, blob: &[u8]) -> bool {
        blob.starts_with(CUSTODY_MAGIC)
    }

    /// 后端名（用于 Debug / 日志；后端名非敏感）。
    fn name(&self) -> &'static str;
}

/// `file` 托管后端：透传封装（`seal` / `open` 恒等），各平台可用。
///
/// 密钥防护由 keyprovider 的 HKDF 机器码绑定容器 + 0600 文件权限承担；
/// 本后端只负责「显式固定行为」的容器场景（无 OS 托管依赖、确定性）。
#[derive(Debug, Default)]
pub struct FileCustody;

impl KeyCustody for FileCustody {
    fn seal(&self, plaintext: &[u8]) -> Result<Vec<u8>, CustodyError> {
        Ok(plaintext.to_vec())
    }

    fn open(&self, sealed: &[u8]) -> Result<Vec<u8>, CustodyError> {
        // fail-closed：file 后端读到 dpapi 已包装的文件（跨模式切换）——
        // 结构化报错，绝不猜测意图、绝不静默解包。
        if sealed.starts_with(CUSTODY_MAGIC) {
            return Err(CustodyError::OpenFailed {
                reason: "blob was sealed by the dpapi custody backend but the current \
                         custody mode is file"
                    .to_string(),
            });
        }
        Ok(sealed.to_vec())
    }

    fn is_sealed(&self, _blob: &[u8]) -> bool {
        false
    }

    fn name(&self) -> &'static str {
        "file"
    }
}

// ---------------------------------------------------------------------------
// Windows DPAPI 后端（零依赖 FFI；hardening.rs 先例模式）
// ---------------------------------------------------------------------------

/// Windows DPAPI 托管后端（用户域）：容器字节经 `CryptProtectData` 包装后落盘。
///
/// 威胁模型：其他用户账户 / 直接拷贝密钥文件到另一台机器无法解封；
/// 当前用户域内的进程仍可解封（DPAPI 用户域边界，文档如实声明）。
#[cfg(windows)]
#[derive(Debug, Default)]
pub struct DpapiCustody;

#[cfg(windows)]
mod winffi {
    // 零依赖 FFI：仅声明用到的 crypt32 / kernel32 函数（等价于 windows-sys 的
    // Win32_Security_Cryptography 极小子集）。mingw 构建链自带两个导入库。
    #[repr(C)]
    pub struct DataBlob {
        pub cb_data: u32,
        pub pb_data: *mut u8,
    }

    #[link(name = "crypt32")]
    extern "system" {
        pub fn CryptProtectData(
            data_in: *mut DataBlob,
            data_descr: *const u16,
            optional_entropy: *mut DataBlob,
            reserved: *mut core::ffi::c_void,
            prompt_struct: *mut core::ffi::c_void,
            flags: u32,
            data_out: *mut DataBlob,
        ) -> i32;

        pub fn CryptUnprotectData(
            data_in: *mut DataBlob,
            data_descr: *mut *mut u16,
            optional_entropy: *mut DataBlob,
            reserved: *mut core::ffi::c_void,
            prompt_struct: *mut core::ffi::c_void,
            flags: u32,
            data_out: *mut DataBlob,
        ) -> i32;
    }

    #[link(name = "kernel32")]
    extern "system" {
        pub fn GetLastError() -> u32;
        pub fn LocalFree(hmem: *mut core::ffi::c_void) -> *mut core::ffi::c_void;
    }
}

#[cfg(windows)]
impl DpapiCustody {
    /// DPAPI 保护（输入明文 → 输出受保护 blob；调用方负责 LocalFree 释放）。
    fn protect(&self, plaintext: &[u8]) -> Result<Vec<u8>, CustodyError> {
        let len = u32::try_from(plaintext.len()).map_err(|_| CustodyError::SealFailed {
            reason: format!("plaintext too large for DPAPI: {} bytes", plaintext.len()),
        })?;
        let mut input = winffi::DataBlob {
            cb_data: len,
            // SAFETY：CryptProtectData 只读该缓冲（参数为 pDataIn 只读语义，
            // C 接口声明为非 const 指针故保留 *mut；调用期间不写回）。
            pb_data: plaintext.as_ptr() as *mut u8,
        };
        let mut output = winffi::DataBlob {
            cb_data: 0,
            pb_data: std::ptr::null_mut(),
        };
        // SAFETY：入参 blob 指向合法缓冲；prompt/descr/entropy 全空（合法）；
        // UI_FORBIDDEN 保证 headless 不弹窗；输出 blob 由系统分配。
        let ok = unsafe {
            winffi::CryptProtectData(
                &mut input,
                std::ptr::null(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                CRYPTPROTECT_UI_FORBIDDEN,
                &mut output,
            )
        };
        if ok == 0 {
            // SAFETY：GetLastError 无参数、读取线程错误码。
            let code = unsafe { winffi::GetLastError() };
            return Err(CustodyError::SealFailed {
                reason: format!("CryptProtectData failed (win32 error {code})"),
            });
        }
        // SAFETY：成功返回后 output.pb_data 指向系统分配的 cb_data 字节缓冲；
        // 拷贝后必须 LocalFree 释放（不释放即泄漏）。
        let blob = unsafe {
            let bytes = std::slice::from_raw_parts(output.pb_data, output.cb_data as usize);
            let out = bytes.to_vec();
            winffi::LocalFree(output.pb_data.cast::<core::ffi::c_void>());
            out
        };
        Ok(blob)
    }

    /// DPAPI 解保护（受保护 blob → 明文）。
    fn unprotect(&self, protected: &[u8]) -> Result<Vec<u8>, CustodyError> {
        let len = u32::try_from(protected.len()).map_err(|_| CustodyError::OpenFailed {
            reason: format!(
                "protected blob too large for DPAPI: {} bytes",
                protected.len()
            ),
        })?;
        let mut input = winffi::DataBlob {
            cb_data: len,
            // SAFETY：CryptUnprotectData 只读该缓冲（同 protect 的只读语义）。
            pb_data: protected.as_ptr() as *mut u8,
        };
        let mut output = winffi::DataBlob {
            cb_data: 0,
            pb_data: std::ptr::null_mut(),
        };
        // SAFETY：入参 blob 指向合法缓冲；descr 输出指针不取（传空，系统不回填
        // 描述串，无泄漏路径）；UI_FORBIDDEN 保证 headless 不弹窗。
        let ok = unsafe {
            winffi::CryptUnprotectData(
                &mut input,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                CRYPTPROTECT_UI_FORBIDDEN,
                &mut output,
            )
        };
        if ok == 0 {
            // SAFETY：GetLastError 无参数、读取线程错误码。
            let code = unsafe { winffi::GetLastError() };
            return Err(CustodyError::OpenFailed {
                // 只含 OS 错误码与输入长度元数据，绝无密钥材料。
                reason: format!(
                    "CryptUnprotectData failed (win32 error {code}, blob len {})",
                    protected.len()
                ),
            });
        }
        // SAFETY：成功返回后 output.pb_data 指向系统分配的 cb_data 字节缓冲；
        // 拷贝后必须 LocalFree 释放（不释放即泄漏）。
        let plaintext = unsafe {
            let bytes = std::slice::from_raw_parts(output.pb_data, output.cb_data as usize);
            let out = bytes.to_vec();
            winffi::LocalFree(output.pb_data.cast::<core::ffi::c_void>());
            out
        };
        Ok(plaintext)
    }
}

#[cfg(windows)]
impl KeyCustody for DpapiCustody {
    fn seal(&self, plaintext: &[u8]) -> Result<Vec<u8>, CustodyError> {
        let protected = self.protect(plaintext)?;
        let mut out = Vec::with_capacity(CUSTODY_HEADER_LEN + protected.len());
        out.extend_from_slice(CUSTODY_MAGIC);
        out.push(CUSTODY_BLOB_VERSION);
        out.extend_from_slice(&protected);
        Ok(out)
    }

    fn open(&self, sealed: &[u8]) -> Result<Vec<u8>, CustodyError> {
        // 旧版裸容器（keyprovider 直写、无 custody 包装）→ 透传（加载方检测后升级重写）。
        if !sealed.starts_with(CUSTODY_MAGIC) {
            return Ok(sealed.to_vec());
        }
        if sealed.len() <= CUSTODY_HEADER_LEN {
            return Err(CustodyError::OpenFailed {
                reason: format!("custody blob truncated: len {}", sealed.len()),
            });
        }
        if sealed[CUSTODY_MAGIC.len()] != CUSTODY_BLOB_VERSION {
            return Err(CustodyError::OpenFailed {
                reason: format!(
                    "unsupported custody blob version {}",
                    sealed[CUSTODY_MAGIC.len()]
                ),
            });
        }
        self.unprotect(&sealed[CUSTODY_HEADER_LEN..])
    }

    fn name(&self) -> &'static str {
        "dpapi"
    }
}

// ---------------------------------------------------------------------------
// 后端选择（env → mode → 后端实例）
// ---------------------------------------------------------------------------

/// 从 env [`KEY_CUSTODY_ENV`] 解析托管模式（未设置 → [`CustodyMode::Auto`]）。
///
/// # Errors
/// env 值非法（非 auto/dpapi/file）时返回 [`CustodyError::InvalidValue`]。
pub fn custody_mode_from_env() -> Result<CustodyMode, CustodyError> {
    match std::env::var(KEY_CUSTODY_ENV) {
        Ok(raw) => CustodyMode::parse(&raw),
        Err(_) => Ok(CustodyMode::Auto),
    }
}

/// 按模式实例化托管后端（装配点唯一入口；`auto` 按平台映射）。
///
/// # Errors
/// `dpapi` 用在非 Windows 时返回 [`CustodyError::UnsupportedMode`]（fail-closed，
/// 绝不静默降级到 file——降级必须可解释且由运维显式选择）。
pub fn resolve_custody(mode: CustodyMode) -> Result<Arc<dyn KeyCustody>, CustodyError> {
    match mode {
        CustodyMode::Auto => {
            #[cfg(windows)]
            {
                Ok(Arc::new(DpapiCustody) as Arc<dyn KeyCustody>)
            }
            #[cfg(not(windows))]
            {
                Ok(Arc::new(FileCustody))
            }
        }
        CustodyMode::Dpapi => {
            #[cfg(windows)]
            {
                Ok(Arc::new(DpapiCustody))
            }
            #[cfg(not(windows))]
            {
                Err(CustodyError::UnsupportedMode)
            }
        }
        CustodyMode::File => Ok(Arc::new(FileCustody)),
    }
}

/// 装配一步式入口：env → 模式 → 后端实例（任一步失败即 fail-closed）。
///
/// # Errors
/// env 值非法 / 平台不支持时返回对应 [`CustodyError`]（结构化，带恢复路径）。
pub fn custody_from_env() -> Result<Arc<dyn KeyCustody>, CustodyError> {
    resolve_custody(custody_mode_from_env()?)
}

// ---------------------------------------------------------------------------
// 单元测试
// ---------------------------------------------------------------------------

#[cfg(test)]
pub(crate) mod test_support {
    /// custody env 测试互斥锁（同一测试进程内串行化 env 操作，
    /// 防止并行测试读到彼此设置的值）。
    pub(crate) fn custody_env_lock() -> &'static std::sync::Mutex<()> {
        use std::sync::Mutex;
        use std::sync::OnceLock;
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_support::custody_env_lock;

    /// file 后端：seal/open 恒等往返（透传封装，字节不变）。
    #[test]
    fn file_custody_roundtrip_is_identity() {
        let custody = FileCustody;
        let payload: &[u8] = b"fake-key-container-bytes-0123456789";
        let sealed = custody.seal(payload).expect("file seal cannot fail");
        assert_eq!(sealed, payload, "file custody must be a passthrough");
        let opened = custody.open(&sealed).expect("file open cannot fail");
        assert_eq!(opened, payload);
    }

    /// file 后端：`is_sealed` 恒 false（透传后端永远视为未包装）。
    #[test]
    fn file_custody_never_reports_sealed() {
        let custody = FileCustody;
        assert!(!custody.is_sealed(b""));
        assert!(!custody.is_sealed(CUSTODY_MAGIC));
        assert!(!custody.is_sealed(b"IOTDAQKC-anything".as_slice()));
        assert_eq!(custody.name(), "file");
    }

    /// file 后端：读到 dpapi 已包装文件 → 结构化 OpenFailed（fail-closed，不猜测意图）。
    #[test]
    fn file_custody_rejects_foreign_sealed_blob() {
        let custody = FileCustody;
        let mut foreign = CUSTODY_MAGIC.to_vec();
        foreign.push(CUSTODY_BLOB_VERSION);
        foreign.extend_from_slice(b"dpapi-output-not-readable-here");
        let err = custody.open(&foreign).expect_err("foreign blob must fail");
        assert!(
            matches!(err, CustodyError::OpenFailed { .. }),
            "structured error required, got {err}"
        );
        // 空的包装头（只有 magic+ver，无 dpapi 载荷）同样结构化拒绝。
        let err = custody
            .open(CUSTODY_MAGIC)
            .expect_err("truncated must fail");
        assert!(matches!(err, CustodyError::OpenFailed { .. }), "got {err}");
    }

    /// 模式解析：合法值（大小写不敏感 / 含空白）与非法值。
    #[test]
    fn custody_mode_parse_covers_valid_and_invalid() {
        assert_eq!(CustodyMode::parse("auto").expect("auto"), CustodyMode::Auto);
        assert_eq!(
            CustodyMode::parse("  DPAPI \n").expect("dpapi trimmed+ci"),
            CustodyMode::Dpapi
        );
        assert_eq!(
            CustodyMode::parse("File").expect("file ci"),
            CustodyMode::File
        );

        let err = CustodyMode::parse("keyring").expect_err("unknown mode must fail");
        assert!(
            matches!(err, CustodyError::InvalidValue { ref value } if value == "keyring"),
            "structured InvalidValue required, got {err}"
        );
        assert!(
            err.to_string().contains(KEY_CUSTODY_ENV),
            "names the env var: {err}"
        );
        assert!(err.to_string().contains("Recovery"), "recovery path: {err}");
        let err = CustodyMode::parse("").expect_err("empty must fail");
        assert!(matches!(err, CustodyError::InvalidValue { .. }));
    }

    /// env 解析：未设置 → Auto；合法值 → 对应模式；非法值 → 结构化错误。
    #[test]
    fn custody_mode_from_env_covers_unset_valid_and_invalid() {
        let _guard = custody_env_lock()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if std::env::var(KEY_CUSTODY_ENV).is_ok() {
            // CI 环境不应设置该变量；被占用则跳过（与既有 env 测试护栏一致）。
            return;
        }

        assert_eq!(
            custody_mode_from_env().expect("unset must default to auto"),
            CustodyMode::Auto
        );

        std::env::set_var(KEY_CUSTODY_ENV, "file");
        assert_eq!(custody_mode_from_env().expect("file"), CustodyMode::File);
        std::env::set_var(KEY_CUSTODY_ENV, "not-a-mode");
        let err = custody_mode_from_env().expect_err("invalid value must fail");
        assert!(
            matches!(err, CustodyError::InvalidValue { .. }),
            "got {err}"
        );

        // 收尾：清理进程环境（不残留到其它测试）。
        std::env::remove_var(KEY_CUSTODY_ENV);
    }

    /// 后端解析：auto 按平台映射（Windows → dpapi，其余 → file）；file 恒可用。
    #[test]
    fn resolve_custody_auto_maps_by_platform() {
        let auto = resolve_custody(CustodyMode::Auto).expect("auto always resolvable");
        if cfg!(windows) {
            assert_eq!(auto.name(), "dpapi", "windows auto must map to dpapi");
        } else {
            assert_eq!(auto.name(), "file", "non-windows auto must map to file");
        }
        assert_eq!(
            resolve_custody(CustodyMode::File)
                .expect("file always resolvable")
                .name(),
            "file"
        );
    }

    /// 后端解析：dpapi 仅 Windows；非 Windows 显式 UnsupportedMode（绝不静默降级）。
    #[test]
    fn resolve_custody_dpapi_is_windows_only() {
        #[cfg(windows)]
        {
            let backend = resolve_custody(CustodyMode::Dpapi).expect("dpapi on windows");
            assert_eq!(backend.name(), "dpapi");
        }
        #[cfg(not(windows))]
        {
            let err = resolve_custody(CustodyMode::Dpapi).expect_err("dpapi off-windows");
            assert!(
                matches!(err, CustodyError::UnsupportedMode),
                "structured UnsupportedMode required, got {err}"
            );
            assert!(err.to_string().contains("Recovery"), "recovery path: {err}");
        }
    }

    /// 错误映射：CustodyError → DaemonError（Security 7000 / Config 2000 / Storage）。
    #[test]
    fn custody_error_maps_to_daemon_error() {
        let open: DaemonError = CustodyError::OpenFailed {
            reason: "test".to_string(),
        }
        .into();
        assert!(matches!(open, DaemonError::SecurityError(_)));
        assert_eq!(open.error_code(), 7000);

        let invalid: DaemonError = CustodyError::InvalidValue {
            value: "x".to_string(),
        }
        .into();
        assert!(matches!(invalid, DaemonError::ConfigError(_)));
        assert_eq!(invalid.error_code(), 2000);

        let unsupported: DaemonError = CustodyError::UnsupportedMode.into();
        assert_eq!(unsupported.error_code(), 2000);

        let io_err: DaemonError =
            CustodyError::Io(io::Error::new(io::ErrorKind::PermissionDenied, "denied")).into();
        assert!(matches!(io_err, DaemonError::StorageError(_)));
    }

    // ---- Windows DPAPI 后端（本机可跑；非 Windows cfg 隔离、编译即验证） ----

    #[cfg(windows)]
    #[test]
    fn dpapi_roundtrip_hides_plaintext_and_marks_sealed() {
        let custody = DpapiCustody;
        let payload: &[u8] = b"fake-container-0123456789abcdef-32b!";
        let sealed = custody.seal(payload).expect("dpapi seal");

        assert!(sealed.starts_with(CUSTODY_MAGIC), "custody magic prefix");
        assert_eq!(
            sealed[CUSTODY_MAGIC.len()],
            CUSTODY_BLOB_VERSION,
            "version byte"
        );
        assert_ne!(sealed, payload, "wrapped output must differ from input");
        assert!(
            sealed.windows(payload.len()).all(|w| w != payload),
            "plaintext must never appear in the sealed blob"
        );
        assert!(
            custody.is_sealed(&sealed),
            "sealed blob must be reported as sealed"
        );
        assert!(
            !custody.is_sealed(payload),
            "legacy blob must not be reported sealed"
        );

        let opened = custody.open(&sealed).expect("dpapi open");
        assert_eq!(opened, payload, "roundtrip must restore the plaintext");
    }

    #[cfg(windows)]
    #[test]
    fn dpapi_open_passes_legacy_blob_through() {
        let custody = DpapiCustody;
        let legacy: &[u8] = b"IOTDAQKP-legacy-raw-container-bytes";
        let opened = custody.open(legacy).expect("legacy passthrough");
        assert_eq!(
            opened, legacy,
            "unwrapped legacy blob must pass through untouched"
        );
    }

    #[cfg(windows)]
    #[test]
    fn dpapi_open_rejects_corrupt_and_bad_version() {
        let custody = DpapiCustody;
        let payload: &[u8] = b"corrupt-probe-payload-0123456789";
        let sealed = custody.seal(payload).expect("dpapi seal");

        // 篡改 DPAPI 载荷一个字节 → 解封失败（结构化 OpenFailed）。
        let mut tampered = sealed.clone();
        let last = tampered.len() - 1;
        tampered[last] ^= 0x01;
        let err = custody
            .open(&tampered)
            .expect_err("tampered blob must fail");
        assert!(
            matches!(err, CustodyError::OpenFailed { .. }),
            "structured error required, got {err}"
        );
        // 错误消息绝不回显载荷（私钥红线：只含错误码 / 长度元数据）。
        let rendered = err.to_string();
        assert!(
            !rendered.contains("corrupt-probe-payload"),
            "error must never echo payload bytes: {rendered}"
        );

        // 版本字节非法 → 结构化 OpenFailed。
        let mut bad_version = sealed;
        bad_version[CUSTODY_MAGIC.len()] = 0xFF;
        let err = custody
            .open(&bad_version)
            .expect_err("bad version must fail");
        assert!(matches!(err, CustodyError::OpenFailed { .. }), "got {err}");

        // 只有封装头、无载荷 → 结构化 OpenFailed。
        let err = custody
            .open(CUSTODY_MAGIC)
            .expect_err("header-only must fail");
        assert!(matches!(err, CustodyError::OpenFailed { .. }), "got {err}");
    }

    /// Debug 输出不含任何托管载荷（红线：密钥材料不进日志通道）。
    #[test]
    fn debug_output_never_carries_payload() {
        let custody = FileCustody;
        let payload = b"secret-container-bytes";
        let sealed = custody.seal(payload).expect("seal");
        // FileCustody / DpapiCustody 均为无字段（或字段非敏感）类型：
        // Debug 输出只有类型名，天然不含任何托管载荷字节。
        let rendered = format!("{custody:?} {sealed:?}");
        assert!(
            !rendered.contains("secret-container-bytes"),
            "debug output must not echo payload: {rendered}"
        );
        assert_eq!(
            rendered,
            format!("FileCustody {sealed:?}"),
            "debug output is type name + Vec bytes, never a payload decode"
        );
        assert!(
            rendered.starts_with("FileCustody"),
            "type name only, no payload prefix: {rendered}"
        );
    }
}

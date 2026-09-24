//! `hardening` — Tier-1 防逆向加固原语（task 50）。
//!
//! # 定位契约（不可偏离）
//! 「提高成本 ≠ 绝对防破解」。Tier-1 的目标是拦掉约 90% 的非专业逆向尝试，
//! **不承诺、也不得在任何文档宣称「绝对防破解」**。本模块只提供低成本、可审计的
//! 原语：字符串常量混淆、文件/自身哈希完整性自检、低成本反调试探测、试用标记的
//! 多重冗余存储。明确**不做**：内核驱动、VMProtect 级壳、硬件加密狗、对抗性
//! 反调试（反-反调试军备）。
//!
//! # Fail-safe 而非 fail-crash（硬性红线）
//! 本模块所有检测/校验失败**绝不退出进程、绝不 panic**：
//! - 完整性校验失败 → 返回 [`IntegrityVerdict::Tampered`]，调用方据此进入
//!   [`RestrictedMode`]（受限模式，如禁用敏感 endpoint、只读降级）。
//! - 反调试探测的 *探测本身失败*（如 /proc 不可读）→ 视为未检测到（false），
//!   由调用方结合其它信号综合判断；探测到调试器 → 返回 true，**不做任何
//!   退出/死循环等对抗动作**，处置权完全在调用方。
//!
//! # 与其它 task 的联动
//! - **task 58（完整性 manifest）**：`expected` 哈希由打包期生成的 manifest 注入。
//!   V1 的诚实实现是「对当前可执行文件整体做 SHA-256」（[`verify_self_integrity`]）；
//!   更细粒度的按段（.text）哈希需要跨平台段枚举，留待 task 58 的 manifest 机制
//!   扩展 `expected` 的来源（文件哈希 → 段清单哈希），接口签名保持不变。
//! - **task 56（clock.rs 回拨检测）/ task 49（keyprovider.rs）**：均禁改，
//!   本模块只提供原语；接线方式见 [`RedundantStore`] 文档。
//! - **trial.rs（试用 72h，禁改）接线说明**：试用标记的冗余落盘应在 trial 的
//!   持久化层替换/包装为 `Box<dyn RedundantStore>`——真实后端实现
//!   （多路径文件 + 注册表等 N 个物理位置）由后续任务提供，`InMemoryRedundantStore`
//!   是接口契约与表决语义的参考实现。接线要点：
//!     1. 写入：`write_marker(试用起始时间戳 + HMAC)` 到全部 N 个槽位；
//!     2. 读取：`read_marker()` 多数派表决，`Ok(None)`（全擦/全不一致）→
//!        按首次安装重新计时（单点删除兜底语义）；
//!     3. 本模块不负责 HMAC 与时间逻辑，那些属于 trial.rs / clock.rs 的职责。
//!
//! # 性能红线
//! 字符串混淆仅为**敏感 endpoint / 密钥模式**服务（数量级：个位数常量），
//! 禁止在热路径（数据面逐点处理、MQTT 发布循环等）使用。

use std::path::Path;

// ---------------------------------------------------------------------------
// 1. 字符串常量混淆（编译期 XOR 掩码存储 + 运行时解码）
// ---------------------------------------------------------------------------

/// 运行时 XOR 混淆纯函数（自反：`obfuscate(obfuscate(x), key) == x`）。
///
/// 供非 const 场景与测试使用；编译期常量请用 [`xor_mask_bytes`] + [`obf_const!`]。
pub fn obfuscate(data: &[u8], key: u8) -> Vec<u8> {
    data.iter().map(|b| b ^ key).collect()
}

/// 编译期 XOR 掩码：`const` 环境（static/const 初始化）可用。
///
/// 用法：把敏感明文手工预异或后以字节数组形式入库，或直接用 [`obf_const!`]
/// 在源码中保留明文并在编译期完成掩码（构建产物中只留密文）。
pub const fn xor_mask_bytes<const N: usize>(data: &[u8; N], key: u8) -> [u8; N] {
    let mut out = [0u8; N];
    let mut i = 0usize;
    while i < N {
        out[i] = data[i] ^ key;
        i += 1;
    }
    out
}

/// 定义一个编译期掩码的混淆常量并返回其密文字节数组。
///
/// ```
/// use daemon::hardening::deobfuscate;
/// use daemon::obf_const;
/// const CIPHER: [u8; 11] = obf_const!(0x5A, *b"hello world");
/// assert_eq!(deobfuscate(&CIPHER, 0x5A).unwrap(), "hello world");
/// ```
#[macro_export]
macro_rules! obf_const {
    ($key:expr, $data:expr) => {{
        const OBF_CIPHER: [u8; $data.len()] =
            $crate::hardening::xor_mask_bytes(&$data, $key as u8);
        OBF_CIPHER
    }};
}

/// 运行时解码辅助（严格版）：XOR 还原并校验 UTF-8。
///
/// 失败返回 [`HardeningError::InvalidUtf8`]，不 panic。
pub fn deobfuscate(encoded: &[u8], key: u8) -> Result<String, HardeningError> {
    let decoded: Vec<u8> = encoded.iter().map(|b| b ^ key).collect();
    String::from_utf8(decoded).map_err(|_| HardeningError::InvalidUtf8)
}

/// 运行时解码辅助（宽松版 `obf`）：无效 UTF-8 时返回空串而非 panic。
///
/// 供不方便传播 Result 的场景；能传播错误的调用方请优先用 [`deobfuscate`]。
pub fn obf(encoded: &[u8], key: u8) -> String {
    deobfuscate(encoded, key).unwrap_or_default()
}

// ---------------------------------------------------------------------------
// 2. 完整性自检（文件哈希 / 自身哈希）+ 受限模式标记
// ---------------------------------------------------------------------------

/// SHA-256 摘要长度（task 58 manifest 的哈希口径）。
pub const INTEGRITY_HASH_LEN: usize = 32;

/// 完整性校验结论。任何非 `Ok` 结论调用方都应进入 [`RestrictedMode`]。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IntegrityVerdict {
    /// 校验通过。
    Ok,
    /// 检出篡改：实际哈希与 expected 不一致。
    Tampered {
        /// 十六进制实际哈希（日志/上报用，不含敏感信息）。
        actual_hex: String,
    },
    /// 无法执行校验（文件不存在 / IO 错误 / 无法定位自身路径）。
    /// 同样触发受限模式（fail-safe：拿不到完整性证据时按可疑处理）。
    Unavailable {
        /// 失败原因描述。
        reason: String,
    },
}

/// 受限模式标记类型：只能由检测结果构造，无法凭空捏造。
///
/// 调用方（bootstrap / mgmt API）持有 `Option<RestrictedMode>` 即可表达
/// 「正常 / 受限」两态，受限模式下应禁用敏感能力（敏感 endpoint、写关键配置等），
/// 但**进程继续运行**——禁止退出、禁止 panic。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RestrictedMode {
    reason: RestrictedReason,
}

/// 进入受限模式的原因。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RestrictedReason {
    /// 完整性校验检出篡改。
    IntegrityTampered,
    /// 完整性证据不可得（IO 失败等）。
    IntegrityUnavailable,
    /// 探测到调试器（仅上报信号，处置权在调用方）。
    DebuggerDetected,
}

impl RestrictedMode {
    /// 由完整性结论构造：`Ok → None`（正常模式），其余 → Some(受限)。
    pub fn from_integrity(verdict: &IntegrityVerdict) -> Option<Self> {
        match verdict {
            IntegrityVerdict::Ok => None,
            IntegrityVerdict::Tampered { .. } => Some(Self {
                reason: RestrictedReason::IntegrityTampered,
            }),
            IntegrityVerdict::Unavailable { .. } => Some(Self {
                reason: RestrictedReason::IntegrityUnavailable,
            }),
        }
    }

    /// 由反调试探测结果构造：`false → None`，`true → Some(受限)`。
    pub fn from_debugger(detected: bool) -> Option<Self> {
        if detected {
            Some(Self {
                reason: RestrictedReason::DebuggerDetected,
            })
        } else {
            None
        }
    }

    /// 受限原因。
    pub fn reason(&self) -> RestrictedReason {
        self.reason
    }
}

/// 对单个文件做 SHA-256 并与 expected 比对。
///
/// 这是 task 58 manifest 校验的最小原语：manifest 记录每个关键资产的哈希，
/// 启动时逐个调用本函数。
pub fn verify_file_integrity(path: &Path, expected: &[u8; 32]) -> IntegrityVerdict {
    use sha2::Digest;
    use std::io::Read;

    let file = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(e) => {
            return IntegrityVerdict::Unavailable {
                reason: format!("open {}: {e}", path.display()),
            }
        }
    };
    let mut buf = Vec::new();
    if let Err(e) = std::io::BufReader::new(file).read_to_end(&mut buf) {
        return IntegrityVerdict::Unavailable {
            reason: format!("read {}: {e}", path.display()),
        };
    }
    let actual: [u8; INTEGRITY_HASH_LEN] = sha2::Sha256::digest(&buf).into();
    if actual == *expected {
        IntegrityVerdict::Ok
    } else {
        IntegrityVerdict::Tampered {
            actual_hex: hex::encode(actual),
        }
    }
}

/// 自身完整性自检（V1 诚实实现）：对**当前可执行文件**整体做 SHA-256 比对。
///
/// `expected` 由 task 58 的 manifest 在打包期注入（构建后哈希 exe，写入 manifest，
/// 启动时从签名校验过的 manifest 读回）。诚实的限制：V1 校验的是文件整体哈希，
/// 不做内存段（.text）运行时比对——跨平台段枚举复杂度高，留给 task 58 扩展；
/// 本接口签名保持稳定，扩展时仅替换内部实现。
pub fn verify_self_integrity(expected: &[u8; 32]) -> IntegrityVerdict {
    match std::env::current_exe() {
        Ok(exe) => verify_file_integrity(&exe, expected),
        Err(e) => IntegrityVerdict::Unavailable {
            reason: format!("current_exe: {e}"),
        },
    }
}

// ---------------------------------------------------------------------------
// 3. 低成本反调试探测（Windows / Unix，零对抗性手段）
// ---------------------------------------------------------------------------

/// 探测当前进程是否被调试器附加。
///
/// - Windows：`IsDebuggerPresent`（PEB BeingDebugged）∨
///   `CheckRemoteDebuggerPresent`（远程调试句柄）。零依赖：直接 `extern "system"`
///   声明 kernel32（daemon 未引入 windows-sys，且本任务禁新增依赖）。
/// - Unix：读 `/proc/self/status` 的 `TracerPid:`，非 0 即被跟踪。
/// - 其它平台：恒 false。
///
/// 语义约定：
/// - **探测失败视为未检测到**（false）——反调试的成本控制原则：宁可漏报，
///   不冤枉正常环境；探测异常本身可由调用方记录日志。
/// - 返回 true 时**本函数不做任何处置**（不退出、不循环、不破坏）——
///   对抗性手段是 Tier-2/3 范畴，明确禁止。
pub fn debugger_detected() -> bool {
    #[cfg(windows)]
    {
        win_is_debugger_present() || win_is_remote_debugger_present()
    }
    #[cfg(unix)]
    {
        unix_tracer_pid().map(|pid| pid != 0).unwrap_or(false)
    }
    #[cfg(not(any(windows, unix)))]
    {
        false
    }
}

#[cfg(windows)]
mod winffi {
    // 零依赖 FFI：仅声明用到的三个 kernel32 函数（等价于 windows-sys 的
    // Win32_System_Diagnostics_Debug / Win32_System_Threading 极小子集）。
    #[link(name = "kernel32")]
    extern "system" {
        pub fn IsDebuggerPresent() -> i32;
        pub fn CheckRemoteDebuggerPresent(
            hprocess: *mut core::ffi::c_void,
            pb_debugger_present: *mut i32,
        ) -> i32;
        pub fn GetCurrentProcess() -> *mut core::ffi::c_void;
    }
}

/// `IsDebuggerPresent`：检查 PEB 的 BeingDebugged 标志。
#[cfg(windows)]
fn win_is_debugger_present() -> bool {
    // SAFETY：无参数、无副作用，IsDebuggerPresent 是纯查询 API。
    unsafe { winffi::IsDebuggerPresent() != 0 }
}

/// `CheckRemoteDebuggerPresent`：查询是否有远程调试器附加到当前进程。
#[cfg(windows)]
fn win_is_remote_debugger_present() -> bool {
    let mut flag: i32 = 0;
    // SAFETY：传入当前进程伪句柄与可写栈变量；API 失败（返回 0）时 flag 不可信，
    // 此处按未检测到处理（fail-open，见模块级语义约定）。
    unsafe {
        let handle = winffi::GetCurrentProcess();
        if winffi::CheckRemoteDebuggerPresent(handle, &mut flag) != 0 {
            flag != 0
        } else {
            false
        }
    }
}

/// 读取 `/proc/self/status` 的 TracerPid；文件不可读 / 字段缺失 → None。
#[cfg(unix)]
fn unix_tracer_pid() -> Option<u32> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    for line in status.lines() {
        if let Some(rest) = line.strip_prefix("TracerPid:") {
            return rest.trim().parse::<u32>().ok();
        }
    }
    None
}

// ---------------------------------------------------------------------------
// 4. 试用标记多重冗余存储原语（多数派表决）
// ---------------------------------------------------------------------------

/// 冗余存储 trait：同一标记写入 N 个槽位，读取时多数派表决。
///
/// 表决语义（默认 N=4）：
/// - ≥2 个槽位一致 → 可信，返回该值；
/// - 全部槽位为空 → `Ok(None)`（Missing）；
/// - 存在非空但**没有任何值达到 ≥2 票**，或最高票数出现并列（如 2 vs 2）→
///   **全部擦除**并返回 `Ok(None)`——「单点删除兜底」：攻击者删除任意 ≤N-2 个
///   槽位不影响读取；篡改到无法形成多数派则宁可重置（对试用计时而言，
///   重置 = 重新计时，损失可接受且方向安全）。
///
/// 与 trial.rs 的接线说明见本模块文档头；本 trait 不含 HMAC / 时间逻辑。
pub trait RedundantStore {
    /// 把标记全量写入所有槽位（覆盖式，含曾被擦除的槽位——下一次写入即重建）。
    fn write_marker(&mut self, marker: &[u8]) -> Result<(), HardeningError>;

    /// 多数派表决读取。`Ok(None)` = Missing（全空或全不一致已擦除）。
    fn read_marker(&mut self) -> Result<Option<Vec<u8>>, HardeningError>;

    /// 擦除指定槽位（测试模拟攻击者删除 / 运维清理）。
    fn erase_slot(&mut self, slot: usize) -> Result<(), HardeningError>;

    /// 向指定槽位写入任意脏数据（测试模拟攻击者篡改）。
    fn corrupt_slot(&mut self, slot: usize, bytes: &[u8]) -> Result<(), HardeningError>;

    /// 槽位总数 N。
    fn slot_count(&self) -> usize;
}

/// [`RedundantStore`] 的内存参考实现：契约与表决语义的权威定义。
/// 真实后端（多路径文件 / 注册表等多物理位置）由后续任务实现。
#[derive(Debug, Clone)]
pub struct InMemoryRedundantStore {
    slots: Vec<Option<Vec<u8>>>,
}

impl InMemoryRedundantStore {
    /// 默认槽位数（task 50 规格）。
    pub const DEFAULT_SLOTS: usize = 4;

    /// 默认 4 槽位。
    pub fn new() -> Self {
        Self {
            slots: vec![None; Self::DEFAULT_SLOTS],
        }
    }

    /// 自定义槽位数（≥1）。
    pub fn new_with_slots(n: usize) -> Result<Self, HardeningError> {
        if n == 0 {
            return Err(HardeningError::NoSlots);
        }
        Ok(Self { slots: vec![None; n] })
    }
}

impl Default for InMemoryRedundantStore {
    fn default() -> Self {
        Self::new()
    }
}

impl RedundantStore for InMemoryRedundantStore {
    fn write_marker(&mut self, marker: &[u8]) -> Result<(), HardeningError> {
        for slot in self.slots.iter_mut() {
            *slot = Some(marker.to_vec());
        }
        Ok(())
    }

    fn read_marker(&mut self) -> Result<Option<Vec<u8>>, HardeningError> {
        // 统计各候选值票数（按值内容比较；槽位数是个位数，线性扫描即可）。
        let mut counts: Vec<(Vec<u8>, usize)> = Vec::new();
        for value in self.slots.iter().flatten() {
            match counts.iter_mut().find(|(v, _)| v == value) {
                Some((_, c)) => *c += 1,
                None => counts.push((value.clone(), 1)),
            }
        }

        // 全空 → Missing。
        if counts.is_empty() {
            return Ok(None);
        }

        let max_votes = counts.iter().map(|(_, c)| *c).max().unwrap_or(0);
        let leaders = counts.iter().filter(|(_, c)| *c == max_votes).count();

        // 有唯一多数派（≥2 票）→ 可信。
        if max_votes >= 2 && leaders == 1 {
            let value = counts
                .iter()
                .find(|(_, c)| *c == max_votes)
                .map(|(v, _)| (*v).clone())
                .unwrap_or_default();
            return Ok(Some(value));
        }

        // 全不一致 / 并列 → 全部擦除，返回 Missing（单点删除兜底语义）。
        for slot in self.slots.iter_mut() {
            *slot = None;
        }
        Ok(None)
    }

    fn erase_slot(&mut self, slot: usize) -> Result<(), HardeningError> {
        let s = self.slots.get_mut(slot).ok_or(HardeningError::InvalidSlot(slot))?;
        *s = None;
        Ok(())
    }

    fn corrupt_slot(&mut self, slot: usize, bytes: &[u8]) -> Result<(), HardeningError> {
        let s = self.slots.get_mut(slot).ok_or(HardeningError::InvalidSlot(slot))?;
        *s = Some(bytes.to_vec());
        Ok(())
    }

    fn slot_count(&self) -> usize {
        self.slots.len()
    }
}

// ---------------------------------------------------------------------------
// 错误类型
// ---------------------------------------------------------------------------

/// 加固模块统一错误（零 panic：所有失败走 Result）。
#[derive(Debug, thiserror::Error)]
pub enum HardeningError {
    /// IO 失败（文件校验等）。
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    /// 解码后的字节不是合法 UTF-8。
    #[error("deobfuscated payload is not valid utf-8")]
    InvalidUtf8,
    /// 槽位下标越界。
    #[error("invalid slot index {0}")]
    InvalidSlot(usize),
    /// 槽位数为 0，无法构建冗余存储。
    #[error("redundant store requires at least 1 slot")]
    NoSlots,
}

// ---------------------------------------------------------------------------
// 测试（task 50 规格：≥12 项）
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use sha2::Digest;

    // ---- 1. 字符串常量混淆 ----

    /// T01 混淆 round-trip：混淆 → 解码 还原原文。
    #[test]
    fn obfuscate_round_trip_restores_plaintext() {
        let plaintext = b"https://licensing.example.com/v1/activate";
        let key = 0xA7u8;
        let cipher = obfuscate(plaintext, key);
        assert_eq!(deobfuscate(&cipher, key).unwrap(), "https://licensing.example.com/v1/activate");
    }

    /// T02 混淆后字节 ≠ 原文（key 非 0 时）。
    #[test]
    fn obfuscated_bytes_differ_from_plaintext() {
        let plaintext = b"secret-pattern";
        let key = 0x5Au8;
        let cipher = obfuscate(plaintext, key);
        assert_ne!(cipher.as_slice(), &plaintext[..]);
        // 密文中不应出现原文的非零字节。
        for (c, p) in cipher.iter().zip(plaintext.iter()) {
            if *p != 0 {
                assert_ne!(*c, *p);
            }
        }
    }

    /// T03 编译期掩码宏：const 环境生成密文，运行时解码还原。
    #[test]
    fn const_macro_compile_time_mask_round_trip() {
        const CIPHER: [u8; 11] = obf_const!(0x5A, *b"hello world");
        assert_ne!(CIPHER, *b"hello world");
        assert_eq!(deobfuscate(&CIPHER, 0x5A).unwrap(), "hello world");
    }

    /// T04 无效 UTF-8 检出为错误而非 panic。
    #[test]
    fn deobfuscate_rejects_invalid_utf8() {
        let cipher = obfuscate(&[0xFF, 0xFE, 0x80], 0x00); // key=0 → 原样，非法 UTF-8
        assert!(matches!(deobfuscate(&cipher, 0x00), Err(HardeningError::InvalidUtf8)));
    }

    /// T05 宽松版 `obf`：正常解码 + 非法输入返回空串、绝不 panic。
    #[test]
    fn obf_helper_never_panics() {
        let cipher = obfuscate(b"endpoint/mqtt", 0x11);
        assert_eq!(obf(&cipher, 0x11), "endpoint/mqtt");
        assert_eq!(obf(&[0xFF, 0xFE], 0x00), "");
    }

    // ---- 2. 完整性自检 ----

    /// T06 文件完整性：内容一致 → Ok。
    #[test]
    fn verify_file_integrity_ok() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("asset.bin");
        std::fs::write(&path, b"firmware payload v1").unwrap();
        let expected: [u8; 32] = sha2::Sha256::digest(b"firmware payload v1").into();
        assert_eq!(verify_file_integrity(&path, &expected), IntegrityVerdict::Ok);
    }

    /// T07 文件完整性：篡改检出 → Tampered。
    #[test]
    fn verify_file_integrity_detects_tampering() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("asset.bin");
        std::fs::write(&path, b"firmware payload v1").unwrap();
        let expected: [u8; 32] = sha2::Sha256::digest(b"firmware payload v1").into();
        std::fs::write(&path, b"firmware payload HACKED").unwrap();
        match verify_file_integrity(&path, &expected) {
            IntegrityVerdict::Tampered { actual_hex } => {
                assert_eq!(actual_hex.len(), 64);
            }
            other => panic!("expected Tampered, got {other:?}"),
        }
    }

    /// T08 文件缺失 → Unavailable（同样应触发受限模式）。
    #[test]
    fn verify_file_integrity_missing_file_is_unavailable() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("does-not-exist.bin");
        let expected = [0u8; 32];
        match verify_file_integrity(&path, &expected) {
            IntegrityVerdict::Unavailable { .. } => {}
            other => panic!("expected Unavailable, got {other:?}"),
        }
    }

    /// T09 受限模式标记：Ok → None；Tampered / Unavailable → Some。
    #[test]
    fn restricted_mode_from_integrity_verdicts() {
        assert_eq!(RestrictedMode::from_integrity(&IntegrityVerdict::Ok), None);
        let tampered = IntegrityVerdict::Tampered { actual_hex: "ab".into() };
        assert_eq!(
            RestrictedMode::from_integrity(&tampered).map(|m| m.reason()),
            Some(RestrictedReason::IntegrityTampered)
        );
        let unavailable = IntegrityVerdict::Unavailable { reason: "x".into() };
        assert_eq!(
            RestrictedMode::from_integrity(&unavailable).map(|m| m.reason()),
            Some(RestrictedReason::IntegrityUnavailable)
        );
    }

    /// T10 自身完整性：对当前测试二进制现算哈希 → 应为 Ok（真实调用路径）。
    #[test]
    fn verify_self_integrity_ok_against_current_exe() {
        let exe = std::env::current_exe().unwrap();
        let buf = std::fs::read(&exe).unwrap();
        let expected: [u8; 32] = sha2::Sha256::digest(&buf).into();
        assert_eq!(verify_self_integrity(&expected), IntegrityVerdict::Ok);
    }

    // ---- 3. 反调试 ----

    /// T11 Windows 真实调用：测试环境无调试器 → false。
    #[cfg(windows)]
    #[test]
    fn debugger_detected_false_without_debugger_on_windows() {
        assert!(!debugger_detected());
        // 两个探测通道单独调用也不 panic、返回 false。
        assert!(!win_is_debugger_present());
        assert!(!win_is_remote_debugger_present());
    }

    /// T12 反调试探测跨平台冒烟：可调用、不 panic（Unix 结果依环境而定，
    /// CI/本机均无调试器 → false）。
    #[test]
    fn debugger_detected_smoke_no_panic() {
        let detected = debugger_detected();
        // 本机/CI 正常运行必然无调试器；若真为 true 说明构建环境被跟踪，
        // 同样是有价值的信号，此处只断言「可执行、不 panic」。
        let _ = RestrictedMode::from_debugger(detected);
    }

    // ---- 4. 冗余存储多数派 ----

    /// T13 写入 → 读出一致。
    #[test]
    fn redundant_store_write_read_round_trip() {
        let mut store = InMemoryRedundantStore::new();
        assert_eq!(store.slot_count(), 4);
        store.write_marker(b"trial-2025-01-01T00:00:00Z").unwrap();
        assert_eq!(
            store.read_marker().unwrap(),
            Some(b"trial-2025-01-01T00:00:00Z".to_vec())
        );
    }

    /// T14 单槽被删：N=4 删 1 槽 → 仍可读出（单点删除兜底）。
    #[test]
    fn redundant_store_survives_single_slot_erase() {
        let mut store = InMemoryRedundantStore::new();
        store.write_marker(b"marker").unwrap();
        store.erase_slot(0).unwrap();
        assert_eq!(store.read_marker().unwrap(), Some(b"marker".to_vec()));
    }

    /// T15 N-2 槽被删：删 2 槽剩 2 个一致 → 仍可读出。
    #[test]
    fn redundant_store_survives_n_minus_2_erase() {
        let mut store = InMemoryRedundantStore::new();
        store.write_marker(b"marker").unwrap();
        store.erase_slot(1).unwrap();
        store.erase_slot(3).unwrap();
        assert_eq!(store.read_marker().unwrap(), Some(b"marker".to_vec()));
    }

    /// T16 单槽被篡改为脏数据 → 多数派仍还原原值。
    #[test]
    fn redundant_store_survives_single_slot_corruption() {
        let mut store = InMemoryRedundantStore::new();
        store.write_marker(b"marker").unwrap();
        store.corrupt_slot(2, b"junk").unwrap();
        assert_eq!(store.read_marker().unwrap(), Some(b"marker".to_vec()));
    }

    /// T17 全不一致（2 篡改不同脏数据 + 2 擦除 → 无任何值 ≥2 票）→ 全擦 + Missing。
    #[test]
    fn redundant_store_all_disagree_erases_all_and_missing() {
        let mut store = InMemoryRedundantStore::new();
        store.write_marker(b"marker").unwrap();
        store.corrupt_slot(0, b"junk-a").unwrap();
        store.corrupt_slot(1, b"junk-b").unwrap();
        store.erase_slot(2).unwrap();
        store.erase_slot(3).unwrap();
        assert_eq!(store.read_marker().unwrap(), None);
        // 全部槽位已擦除：后续读仍是 Missing。
        assert_eq!(store.read_marker().unwrap(), None);
    }

    /// T18 并列多数派（2 vs 2）→ 不可信，全擦 + Missing。
    #[test]
    fn redundant_store_tie_votes_missing() {
        let mut store = InMemoryRedundantStore::new();
        store.write_marker(b"marker").unwrap();
        store.corrupt_slot(0, b"junk-a").unwrap();
        store.corrupt_slot(1, b"junk-a").unwrap();
        // 剩余两槽为 "marker"：2 vs 2 并列 → Missing + 全擦。
        assert_eq!(store.read_marker().unwrap(), None);
        assert_eq!(store.read_marker().unwrap(), None);
    }

    /// T19 全部擦除 → Missing。
    #[test]
    fn redundant_store_all_erased_missing() {
        let mut store = InMemoryRedundantStore::new();
        store.write_marker(b"marker").unwrap();
        for i in 0..store.slot_count() {
            store.erase_slot(i).unwrap();
        }
        assert_eq!(store.read_marker().unwrap(), None);
    }

    /// T20 边界：0 槽位拒绝构建；越界槽位报错；擦除后写入可重建全部槽位。
    #[test]
    fn redundant_store_edge_cases() {
        assert!(matches!(
            InMemoryRedundantStore::new_with_slots(0),
            Err(HardeningError::NoSlots)
        ));
        let mut store = InMemoryRedundantStore::new_with_slots(3).unwrap();
        assert_eq!(store.slot_count(), 3);
        assert!(matches!(
            store.erase_slot(3),
            Err(HardeningError::InvalidSlot(3))
        ));
        store.erase_slot(0).unwrap();
        store.write_marker(b"rebuilt").unwrap();
        assert_eq!(store.read_marker().unwrap(), Some(b"rebuilt".to_vec()));
    }

    // ---- 5. 硬化 profile 存在性 ----

    /// T21 release 硬化 profile 已登记：workspace 根含 strip/lto/panic/codegen-units，
    /// daemon Cargo.toml 含本任务的硬化说明块（include_str! 编译期读取，存在性可断言）。
    #[test]
    fn release_profile_hardening_is_registered() {
        let root_toml = include_str!("../../../Cargo.toml");
        assert!(root_toml.contains("strip = true"), "workspace root missing strip");
        assert!(root_toml.contains("panic = \"abort\""), "workspace root missing panic=abort");
        assert!(root_toml.contains("codegen-units = 1"), "workspace root missing codegen-units");
        assert!(root_toml.contains("lto ="), "workspace root missing lto");

        let daemon_toml = include_str!("../Cargo.toml");
        assert!(
            daemon_toml.contains("task 50 Tier-1"),
            "daemon Cargo.toml missing task 50 hardening documentation block"
        );
    }
}

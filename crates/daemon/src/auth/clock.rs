//! task 56 — 可信时间（SNTP 校时 + 单调时钟锚点 + 时钟回拨检测）。
//!
//! 设计要点：
//! - **SNTP 客户端 hand-rolled**（tokio UdpSocket，48 字节包），零新增依赖；
//!   包编解码为**纯函数**（[`sntp_packet`] / [`parse_sntp_response`]），golden bytes
//!   单测覆盖，测试**绝不真实联网**；IO 层 [`sntp_query`] 只负责收发与超时，
//!   把纯函数串起来后经 `map_err` 收敛为 `DaemonError::NetworkError`。
//! - **单调锚点**：[`tokio::time::Instant`]（生产构建无 `test-util` 特性时其 `now()`
//!   退化为 `std::time::Instant::now()`，行为与 std 单调时钟一致；测试构建启用
//!   `test-util` 后遵循 tokio mock 时钟，可用 `tokio::time::advance` 精确推进）。
//!   墙钟估计 = 锚点墙钟 + 单调耗时（单调耗时只增不减，抵御墙钟跳变）。
//! - **回拨检测**：[`TrustedClock::check_rollback`] —— 墙钟比持久 last_seen 早超过
//!   [`ROLLBACK_TOLERANCE_MS`]（90s）⇒ 判定回拨。last_seen 的 load/save 由**闭包注入**
//!   （本模块不做文件 IO）；检测到回拨时**不落盘**，持久 last_seen 不被回拨墙钟污染。
//! - **零 panic**：所有可失败点（包长度 / 模式位 / 时间戳下溢 / UDP IO）收敛为
//!   本模块 [`LocalError`] 或 `DaemonError`（经 `map_err` 转换）。

use std::time::Duration;

use tokio::time::Instant as MonotonicInstant;

use crate::error::{DaemonError, DaemonResult};

/// 回拨容忍窗口（毫秒）：墙钟比持久 last_seen 早 ≤90s 视为正常抖动（手动对时 /
/// 时区修正），超过才判定为回拨。task 56 契约常量。
pub const ROLLBACK_TOLERANCE_MS: u64 = 90_000;

/// SNTP 查询超时（task 56 指定 2s）。
pub const SNTP_TIMEOUT: Duration = Duration::from_secs(2);

/// SNTP 标准包长（48 字节）。
const SNTP_PACKET_LEN: usize = 48;

/// NTP 1900 纪元与 Unix 1970 纪元的秒差（RFC 5905）。
const NTP_UNIX_EPOCH_DELTA_SECS: u64 = 2_208_988_800;

/// 本模块局部错误（SNTP 纯函数解码失败等）。
///
/// 进入 `DaemonResult` 时一律经 `map_err` 转换为 [`DaemonError::NetworkError`]。
#[derive(Debug, thiserror::Error)]
pub enum LocalError {
    /// 响应包不足 48 字节（截断 / 非法应答）。
    #[error("sntp packet too short: got {got} bytes, need {need}")]
    PacketTooShort {
        /// 实际收到的字节数。
        got: usize,
        /// 要求的最小字节数（48）。
        need: usize,
    },

    /// 应答方 Mode 位不是 4（server）——可能是误连客户端包 / 广播污染。
    #[error("sntp response mode invalid: byte0=0x{0:02X} (expected Mode=4 server)")]
    BadMode(u8),

    /// 应答方声明未同步（LI=3，leap alarm / kiss-of-death），时间不可信。
    #[error("sntp server unsynchronized (LI=3): byte0=0x{0:02X}")]
    Unsynchronized(u8),

    /// 时间戳早于 NTP→Unix 纪元分界（服务器时间明显非法）。
    #[error("sntp timestamp predates NTP->Unix epoch (seconds={0})")]
    BogusTimestamp(u64),
}

/// 回拨检测结论。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RollbackVerdict {
    /// 时间正常（或本机首次见到时间，尚无持久 last_seen 可比对）。
    Ok,
    /// 检测到回拨：墙钟早于持久 last_seen 超过 [`ROLLBACK_TOLERANCE_MS`]。
    Detected,
}

/// last_seen 加载闭包：返回持久化的最近一次可信墙钟（None = 尚无持久值）。
pub type LastSeenLoader = Box<dyn Fn() -> Option<u64> + Send + Sync>;

/// last_seen 保存闭包：把最近一次可信墙钟写入调用方选定的持久化位置。
pub type LastSeenSaver = Box<dyn Fn(u64) + Send + Sync>;

/// 可信时钟：墙钟锚点 + 单调耗时换算 + 回拨检测。
///
/// 墙钟（可能被篡改 / 回拨）只作为锚点初值；之后的时间推进以单调锚点为准，
/// 避免运行中被改系统时间直接拉扯 [`TrustedClock::now_ms`]。
pub struct TrustedClock {
    /// 单调锚点（tokio::time::Instant：生产=std 单调钟，测试=可推进 mock 钟）。
    anchor: MonotonicInstant,
    /// 锚点建立时的墙钟估计（Unix 毫秒）。
    anchor_wall_ms: u64,
    /// 持久 last_seen 读取闭包（调用方注入）。
    load_last_seen: LastSeenLoader,
    /// 持久 last_seen 写入闭包（调用方注入）。
    save_last_seen: LastSeenSaver,
}

impl TrustedClock {
    /// 以当前墙钟建立可信时钟。
    ///
    /// `wall_now_ms` 为建立时刻的 Unix 毫秒（调用方自系统时间读取）；
    /// load/save 闭包由调用方注入（生产=持久卷文件 / 测试=内存 fake）。
    pub fn new(
        wall_now_ms: u64,
        load_last_seen: LastSeenLoader,
        save_last_seen: LastSeenSaver,
    ) -> Self {
        Self {
            anchor: MonotonicInstant::now(),
            anchor_wall_ms: wall_now_ms,
            load_last_seen,
            save_last_seen,
        }
    }

    /// 当前墙钟估计（Unix 毫秒）= 锚点墙钟 + 单调耗时。
    pub fn now_ms(&self) -> u64 {
        self.anchor_wall_ms.saturating_add(self.elapsed_ms())
    }

    /// 自锚点建立起的单调耗时（毫秒）。
    pub fn elapsed_ms(&self) -> u64 {
        u64::try_from(self.anchor.elapsed().as_millis()).unwrap_or(u64::MAX)
    }

    /// 回拨检测：`wall_ms < last_seen - 90_000` ⇒ [`RollbackVerdict::Detected`]。
    ///
    /// 判定为 Ok（含首次 sighting）时把 `wall_ms` 经 save 闭包持久化为新 last_seen；
    /// 判定为 Detected 时**不落盘**——持久 last_seen 保持原值，不被回拨墙钟污染，
    /// 后续判定（trial 等）应以持久 last_seen 为准。
    pub fn check_rollback(&self, wall_ms: u64) -> RollbackVerdict {
        match (self.load_last_seen)() {
            // 首次见到时间：无历史可比，直接落盘并放行。
            None => {
                (self.save_last_seen)(wall_ms);
                RollbackVerdict::Ok
            }
            Some(last_seen) => {
                // saturating_sub：last_seen < 90_000 时阈值取 0，仍按语义比较。
                let threshold = last_seen.saturating_sub(ROLLBACK_TOLERANCE_MS);
                if wall_ms < threshold {
                    RollbackVerdict::Detected
                } else {
                    (self.save_last_seen)(wall_ms);
                    RollbackVerdict::Ok
                }
            }
        }
    }
}

/// 构造 SNTP 客户端请求包（纯函数）：`LI=0 / VN=4 / Mode=3` → 首字节 `0x23`，其余 47 字节全 0。
pub fn sntp_packet() -> [u8; SNTP_PACKET_LEN] {
    let mut packet = [0u8; SNTP_PACKET_LEN];
    // LI=0（无告警）VN=4（版本 4）Mode=3（client）→ 0b00_100_011 = 0x23。
    // 注意：0x1B = 0b00_011_011 是 VN=3，常见资料常把它误标为 VN=4。
    packet[0] = 0x23;
    packet
}

/// 解析 SNTP 服务端应答（**纯函数**，golden bytes 单测，不联网）。
///
/// 取 Receive 与 Transmit 时间戳中的较大者换算 Unix 毫秒（取 max 缓解单程延迟
/// 导致的偏早估计，RFC 5905 on-wire 协议的保守简化）；1900 纪元秒差与毫秒小数
/// 按 RFC 5905 fixed-point 换算（毫秒四舍五入）。
///
/// # Errors
/// 包长不足 / Mode≠4 / LI=3 / 时间戳早于纪元分界 → [`LocalError`]。
pub fn parse_sntp_response(packet: &[u8]) -> Result<u64, LocalError> {
    if packet.len() < SNTP_PACKET_LEN {
        return Err(LocalError::PacketTooShort {
            got: packet.len(),
            need: SNTP_PACKET_LEN,
        });
    }
    let first = packet[0];
    // 高 2 位 LI：3 = leap alarm（未同步），时间不可信。
    if first >> 6 == 3 {
        return Err(LocalError::Unsynchronized(first));
    }
    // 低 3 位 Mode：4 = server 应答。
    if first & 0b0000_0111 != 4 {
        return Err(LocalError::BadMode(first));
    }
    let receive = read_ntp_timestamp(&packet[32..40])?;
    let transmit = read_ntp_timestamp(&packet[40..48])?;
    Ok(receive.max(transmit))
}

/// 读取 8 字节 NTP 时间戳（秒 u32 大端 + 小数 u32 大端）并换算 Unix 毫秒。
fn read_ntp_timestamp(bytes: &[u8]) -> Result<u64, LocalError> {
    let secs = u64::from(u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]));
    let frac = u64::from(u32::from_be_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]));
    if secs < NTP_UNIX_EPOCH_DELTA_SECS {
        return Err(LocalError::BogusTimestamp(secs));
    }
    let unix_secs = secs - NTP_UNIX_EPOCH_DELTA_SECS;
    // 小数部分 → 毫秒：frac / 2^32 * 1000，加 2^31 做四舍五入（u64 范围内无溢出）。
    let frac_ms = (frac * 1_000 + (1u64 << 31)) >> 32;
    Ok(unix_secs * 1_000 + frac_ms)
}

/// SNTP 查询（IO 层）：发 [`sntp_packet`]，收应答后交纯函数解析；2s 超时。
///
/// 单发单收（不重试不校验 originate 匹配， task 56 边缘网关场景可接受；
/// 多应答竞争由 2s 超时与 first-recv 兜底）。
///
/// # Errors
/// 绑定 / 连接 / 收发失败、超时、应答非法（[`LocalError`]）一律收敛为
/// [`DaemonError::NetworkError`]（错误路径不 panic）。
pub async fn sntp_query(host: &str, port: u16) -> DaemonResult<u64> {
    let socket = tokio::net::UdpSocket::bind("0.0.0.0:0")
        .await
        .map_err(|e| DaemonError::NetworkError(format!("sntp bind: {e}")))?;
    socket
        .connect((host, port))
        .await
        .map_err(|e| DaemonError::NetworkError(format!("sntp connect {host}:{port}: {e}")))?;
    socket
        .send(&sntp_packet())
        .await
        .map_err(|e| DaemonError::NetworkError(format!("sntp send: {e}")))?;
    let mut buf = [0u8; SNTP_PACKET_LEN];
    let n = tokio::time::timeout(SNTP_TIMEOUT, socket.recv(&mut buf))
        .await
        .map_err(|_| {
            DaemonError::NetworkError(format!(
                "sntp query {host}:{port} timed out after {:?}",
                SNTP_TIMEOUT
            ))
        })?
        .map_err(|e| DaemonError::NetworkError(format!("sntp recv: {e}")))?;
    parse_sntp_response(&buf[..n])
        .map_err(|e| DaemonError::NetworkError(format!("sntp parse: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// QA：请求包 golden —— 首字节 0x23（LI=0/VN=4/Mode=3），其余全 0，长度 48。
    #[test]
    fn sntp_packet_golden_bytes() {
        let packet = sntp_packet();
        assert_eq!(packet.len(), 48);
        assert_eq!(packet[0], 0x23, "0x1B 是 VN=3，VN=4 应为 0x23");
        assert!(packet[1..].iter().all(|b| *b == 0), "rest must be zeros");
        // 位域复核：LI=0、VN=4、Mode=3。
        assert_eq!(packet[0] >> 6, 0, "LI");
        assert_eq!((packet[0] >> 3) & 0b111, 4, "VN");
        assert_eq!(packet[0] & 0b111, 3, "Mode");
    }

    /// QA：应答解析 golden —— NTP 秒 = 1900 纪元差（0x83AA7E80）、小数 0 → Unix 0ms。
    /// （真实应答 Receive/Transmit 都有值，测试包两者同填。）
    #[test]
    fn parse_golden_epoch_origin() {
        let mut packet = [0u8; 48];
        packet[0] = 0x24; // LI=0 VN=4 Mode=4
        packet[32..40].copy_from_slice(&[0x83, 0xAA, 0x7E, 0x80, 0x00, 0x00, 0x00, 0x00]);
        packet[40..48].copy_from_slice(&[0x83, 0xAA, 0x7E, 0x80, 0x00, 0x00, 0x00, 0x00]);
        assert_eq!(parse_sntp_response(&packet).expect("parse ok"), 0);
    }

    /// QA：应答解析 golden —— 秒 +1、小数 0x80000000（半秒）→ Unix 1500ms（四舍五入）。
    #[test]
    fn parse_golden_half_second_rounding() {
        let mut packet = [0u8; 48];
        packet[0] = 0x24;
        packet[32..40].copy_from_slice(&[0x83, 0xAA, 0x7E, 0x81, 0x80, 0x00, 0x00, 0x00]);
        packet[40..48].copy_from_slice(&[0x83, 0xAA, 0x7E, 0x81, 0x80, 0x00, 0x00, 0x00]);
        assert_eq!(parse_sntp_response(&packet).expect("parse ok"), 1_500);
    }

    /// QA：Receive < Transmit 时取较大者（缓解单程延迟偏早估计）。
    #[test]
    fn parse_takes_max_of_receive_and_transmit() {
        let mut packet = [0u8; 48];
        packet[0] = 0x24;
        // Receive = 纪元 + 1s，Transmit = 纪元 + 2s → 取 2000ms。
        packet[32..40].copy_from_slice(&[0x83, 0xAA, 0x7E, 0x81, 0x00, 0x00, 0x00, 0x00]);
        packet[40..48].copy_from_slice(&[0x83, 0xAA, 0x7E, 0x82, 0x00, 0x00, 0x00, 0x00]);
        assert_eq!(parse_sntp_response(&packet).expect("parse ok"), 2_000);
    }

    /// QA 错误路径：包长不足 48 → PacketTooShort（不 panic）。
    #[test]
    fn parse_rejects_short_packet() {
        let err = parse_sntp_response(&[0x24u8; 20]).expect_err("short packet must fail");
        assert!(matches!(
            err,
            LocalError::PacketTooShort { got: 20, need: 48 }
        ));
    }

    /// QA 错误路径：Mode≠4（客户端请求包冒充应答）→ BadMode。
    #[test]
    fn parse_rejects_non_server_mode() {
        let err = parse_sntp_response(&sntp_packet()).expect_err("client packet must fail");
        assert!(matches!(err, LocalError::BadMode(0x23)));
    }

    /// QA 错误路径：LI=3（未同步 / kiss-of-death）→ Unsynchronized。
    #[test]
    fn parse_rejects_unsynchronized() {
        let mut packet = [0u8; 48];
        packet[0] = 0xE4; // LI=3 VN=4 Mode=4
        let err = parse_sntp_response(&packet).expect_err("LI=3 must fail");
        assert!(matches!(err, LocalError::Unsynchronized(0xE4)));
    }

    /// QA 错误路径：Transmit 早于 1900→1970 分界 → BogusTimestamp（防下溢）。
    /// （Receive 合法、Transmit 非法 ⇒ 解析器在 Transmit 上报错，载荷 = 非法 secs。）
    #[test]
    fn parse_rejects_pre_epoch_timestamp() {
        let mut packet = [0u8; 48];
        packet[0] = 0x24;
        packet[32..40].copy_from_slice(&[0x83, 0xAA, 0x7E, 0x80, 0x00, 0x00, 0x00, 0x00]);
        packet[40..48].copy_from_slice(&[0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00]);
        let err = parse_sntp_response(&packet).expect_err("pre-epoch ts must fail");
        assert!(matches!(err, LocalError::BogusTimestamp(1)));
    }

    /// QA：回拨边界 —— 90s 整不判回拨（wall = last - 90_000 ⇒ Ok 且落盘）。
    #[test]
    fn rollback_boundary_exact_tolerance_is_ok() {
        let store: std::sync::Arc<std::sync::Mutex<Option<u64>>> =
            std::sync::Arc::new(std::sync::Mutex::new(Some(1_000_000)));
        let load_store = std::sync::Arc::clone(&store);
        let save_store = std::sync::Arc::clone(&store);
        let clock = TrustedClock::new(
            1_000_000,
            Box::new(move || *load_store.lock().expect("lock")),
            Box::new(move |v| *save_store.lock().expect("lock") = Some(v)),
        );
        assert_eq!(
            clock.check_rollback(1_000_000 - 90_000),
            RollbackVerdict::Ok
        );
        assert_eq!(
            *store.lock().expect("lock"),
            Some(1_000_000 - 90_000),
            "Ok 判定须把新墙钟落盘"
        );
    }

    /// QA：回拨边界 —— 超过 90s（wall = last - 90_000 - 1）⇒ Detected 且不落盘。
    #[test]
    fn rollback_beyond_tolerance_is_detected() {
        let store: std::sync::Arc<std::sync::Mutex<Option<u64>>> =
            std::sync::Arc::new(std::sync::Mutex::new(Some(1_000_000)));
        let load_store = std::sync::Arc::clone(&store);
        let save_store = std::sync::Arc::clone(&store);
        let clock = TrustedClock::new(
            1_000_000,
            Box::new(move || *load_store.lock().expect("lock")),
            Box::new(move |v| *save_store.lock().expect("lock") = Some(v)),
        );
        assert_eq!(
            clock.check_rollback(1_000_000 - 90_000 - 1),
            RollbackVerdict::Detected
        );
        assert_eq!(
            *store.lock().expect("lock"),
            Some(1_000_000),
            "回拨时持久 last_seen 不得被污染"
        );
    }

    /// QA：首次 sighting（持久值为 None）⇒ Ok 且把墙钟落盘为首个 last_seen。
    #[test]
    fn first_sighting_is_ok_and_persisted() {
        let store: std::sync::Arc<std::sync::Mutex<Option<u64>>> =
            std::sync::Arc::new(std::sync::Mutex::new(None));
        let load_store = std::sync::Arc::clone(&store);
        let save_store = std::sync::Arc::clone(&store);
        let clock = TrustedClock::new(
            7_777_777,
            Box::new(move || *load_store.lock().expect("lock")),
            Box::new(move |v| *save_store.lock().expect("lock") = Some(v)),
        );
        assert_eq!(clock.check_rollback(7_777_777), RollbackVerdict::Ok);
        assert_eq!(*store.lock().expect("lock"), Some(7_777_777));
    }

    /// QA：单调锚点推进 —— tokio pause 下 advance 只推单调锚点，墙钟估计随之线性推进。
    #[tokio::test(start_paused = true)]
    async fn monotonic_anchor_advances_with_mock_time() {
        let clock = TrustedClock::new(10_000_000, Box::new(|| None), Box::new(|_| {}));
        assert_eq!(clock.now_ms(), 10_000_000, "锚点建立时刻即墙钟估计");
        assert_eq!(clock.elapsed_ms(), 0);

        tokio::time::advance(Duration::from_millis(500)).await;
        assert_eq!(clock.now_ms(), 10_000_500);
        assert_eq!(clock.elapsed_ms(), 500);

        tokio::time::advance(Duration::from_millis(1_000)).await;
        assert_eq!(clock.now_ms(), 10_001_500);
        assert_eq!(clock.elapsed_ms(), 1_500);
    }

    /// QA：now_ms 随单调锚点只增不减（多次调用非递减）。
    #[tokio::test(start_paused = true)]
    async fn now_ms_is_monotonic() {
        let clock = TrustedClock::new(5_000, Box::new(|| None), Box::new(|_| {}));
        let mut prev = clock.now_ms();
        for _ in 0..8 {
            tokio::time::advance(Duration::from_millis(10)).await;
            let cur = clock.now_ms();
            assert!(cur >= prev, "now_ms must be non-decreasing");
            prev = cur;
        }
    }
}

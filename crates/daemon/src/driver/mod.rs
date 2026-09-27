//! 南向协议驱动核心抽象（计划 task 8，Wave 2 首任务，blocks task 9-14/15）。
//!
//! 设计要点：
//! - [`Driver`] trait：`#[async_trait]` 四方法（`connect` / `read` / `write` /
//!   `disconnect`），统一返回 [`DaemonResult`]；对象安全（`Box<dyn Driver>`），
//!   支撑后续 Modbus / OPC UA / S7 / MC / HTTP / MQTT 六类驱动（task 9-14）在此契约下实现。
//! - [`PointAddressParser`]：点位地址字符串 → 结构化 [`PointAddress`]。
//!   S7：位访问 `DB1.DBX0.0`、字节/字/双字 `DBn.DBB/DBW/DBD<off>`（area 记
//!   `B/W/D`、`db` 记 DB 号）与过程映像位 `I0.1` / `Q0.3`（真实位号入
//!   `bit_index`）；MC 元件 `M100` / `D100`。无效地址返回
//!   [`DaemonError::ProtocolError`]（错误码域 `ERR_PROTOCOL` = 1000）。
//! - [`Reconnector`]：带指数退避的重连基类（默认 1s → 2s → … → 封顶 60s，
//!   初始 / 上限 / 倍率可配置），纯逻辑无 IO；断线时驱动调用 `next_delay()` 获取
//!   下次重连等待时长，连接成功后 `reset()` 归零。
//!
//! 本任务不实现任何具体协议（Must NOT do：Modbus / OPC UA / S7 / MC / HTTP / MQTT
//! 驱动 = task 9-14）。

use std::time::Duration;

use async_trait::async_trait;

use crate::error::{DaemonError, DaemonResult};

// ---- 具体协议驱动 ----

/// Modbus TCP / RTU-over-TCP 驱动（plan task 9）。
pub mod http;
pub mod mc;
pub mod modbus;
pub mod mqtt_in;
/// OPC UA 客户端驱动（plan task 10）。
pub mod opcua;
pub mod s7;
/// S7 统一 [`Driver`] trait 适配层（薄壳：类型转换 + 错误映射，task 11 收尾切片）。
pub mod s7_adapter;

// ---- Driver trait ----

/// 南向协议驱动统一契约。
///
/// 约定：
/// - `read` 返回与 `points` 等长、按下标一一对应的采样；任一点失败整体返回 `Err`，
///   不产出部分结果；
/// - `write` 的 `value` 为原始字节，编码语义（寄存器字序 / 类型宽度）由具体协议驱动解释；
/// - 连接生命周期（重复 `connect` / 未连接即 `read` 的行为）由具体驱动定义，
///   错误一律收敛为 [`DaemonError::ProtocolError`] 或 [`DaemonError::NetworkError`]。
#[async_trait]
pub trait Driver: Send + Sync {
    /// 建立南向连接（TCP / 串口 / 会话）。
    async fn connect(&mut self) -> DaemonResult<()>;

    /// 批量读取点位（`count` 单位由协议解释：寄存器 / 字 / 字节 / 位）。
    async fn read(&mut self, points: &[ReadPoint]) -> DaemonResult<Vec<PointSample>>;

    /// 批量写入点位。
    async fn write(&mut self, points: &[WritePoint]) -> DaemonResult<()>;

    /// 断开连接并释放会话资源。
    async fn disconnect(&mut self) -> DaemonResult<()>;
}

/// 读取请求：结构化地址 + 连续读取长度。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReadPoint {
    /// 点位地址（[`PointAddressParser`] 解析后）。
    pub address: PointAddress,
    /// 连续读取长度（单位由具体协议解释；≥ 1）。
    pub count: u32,
}

/// 写入请求：结构化地址 + 原始字节值。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WritePoint {
    /// 点位地址。
    pub address: PointAddress,
    /// 原始字节值（编码语义由具体协议驱动解释）。
    pub value: Vec<u8>,
}

/// 读取采样：与请求 [`ReadPoint`] 下标一一对应。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PointSample {
    /// 回读的点位地址（与请求对应）。
    pub address: PointAddress,
    /// 原始字节采样值（类型解码交给数据处理链路，task 15+）。
    pub value: Vec<u8>,
}

// ---- 地址解析 ----

/// 解析后的点位地址（扁平结构化表示，便于测试断言与各协议驱动按自身语义解释）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PointAddress {
    /// S7 数据块号（`DBn` 的 n）；非 S7 地址恒为 `0`。
    pub db: u32,
    /// MC 元件区（`M100` → `Some('M')`、`D100` → `Some('D')`）；非 MC 地址为 `None`。
    pub area: Option<char>,
    /// 字节偏移（S7 `DBn.DBX<start>.<bit>` 的 start）或元件编号（MC `M<start>`）。
    pub start: u32,
    /// 是否位访问：S7 `DBX…` 与 MC `M…` 为 `true`；MC `D…` 为 `false`。
    pub bit: bool,
    /// 位号 0-7（仅 S7 位访问有意义，其余恒为 `0`）。
    pub bit_index: u8,
}

/// 点位地址解析器（无状态纯函数集，plan task 8）。
///
/// 支持语法（匹配前 trim 首尾空白并转大写）：
///
/// | 输入 | 结果 |
/// |---|---|
/// | `DB1.DBX0.0` | `db=1, start=0, bit=true, bit_index=0` |
/// | `DB2.DBB10` / `DB2.DBW20` / `DB2.DBD40` | `db=2, area=B/W/D, bit=false`（字节/字/双字） |
/// | `I0.1` / `Q2.7` | `area=I/Q, bit=true, bit_index=位号`（过程映像位） |
/// | `M100` | `area='M', start=100, bit=true` |
/// | `D100` | `area='D', start=100, bit=false` |
///
/// S7 记法与会话层解析器（`s7.rs::parse_s7_address`）对齐：字节偏移受三字节
/// 地址编码上限约束（≤ `S7_MAX_BYTE_OFFSET`）；`DBB/DBW/DBD` 不允许位后缀；
/// `I/Q` 仅支持 `<byte>.<bit>` 位形式（IB/IW/ID 等字节/字形式待后续扩展）。
#[derive(Debug, Clone, Copy, Default)]
pub struct PointAddressParser;

/// S7 三字节地址字段的字节偏移上限（与会话层 `s7.rs` 的 `MAX_BYTE_OFFSET`
/// 同值同规则：`(offset << 3) | bit` 须落在 24 位内）。
const S7_MAX_BYTE_OFFSET: u32 = 0x1F_FFFF;

impl PointAddressParser {
    /// 解析点位地址字符串为结构化 [`PointAddress`]。
    ///
    /// # Errors
    /// 无法识别的地址（空串 / 格式错误 / 越界 / 暂不支持的访问形式）返回
    /// [`DaemonError::ProtocolError`]，错误消息包含原始输入，错误码 `ERR_PROTOCOL` = 1000。
    pub fn parse(raw: &str) -> DaemonResult<PointAddress> {
        let normalized = raw.trim().to_ascii_uppercase();
        let wrap = |detail: String| {
            DaemonError::ProtocolError(format!("invalid point address {raw:?}: {detail}"))
        };

        if normalized.is_empty() {
            return Err(wrap("empty address".to_string()));
        }
        if normalized.starts_with("DB") {
            Self::parse_s7(&normalized).map_err(wrap)
        } else if normalized.starts_with(|c: char| c.is_ascii_digit()) {
            Self::parse_modbus(&normalized).map_err(wrap)
        } else if normalized.starts_with('I') || normalized.starts_with('Q') {
            Self::parse_piq(&normalized).map_err(wrap)
        } else {
            Self::parse_mc(&normalized).map_err(wrap)
        }
    }

    /// S7 位访问：`DB<db>.DBX<start>.<bit>`（bit 0-7）。
    fn parse_s7(upper: &str) -> Result<PointAddress, String> {
        let after_db = &upper["DB".len()..];
        let db_end = after_db
            .find(|c: char| !c.is_ascii_digit())
            .unwrap_or(after_db.len());
        if db_end == 0 {
            return Err("missing DB number (expected e.g. DB1.DBX0.0)".to_string());
        }
        let db: u32 = after_db[..db_end]
            .parse()
            .map_err(|_| "DB number out of range".to_string())?;

        let spec = after_db[db_end..]
            .strip_prefix(".DB")
            .ok_or_else(|| "expected .DB access spec (expected e.g. DB1.DBX0.0)".to_string())?;
        if spec.is_empty() {
            return Err("missing access type after .DB (expected e.g. DB1.DBX0.0)".to_string());
        }
        let (access, rest) = spec.split_at(1);
        match access {
            "X" => {
                let (start_raw, bit_raw) = rest.split_once('.').ok_or_else(|| {
                    "DBX requires bit suffix .<bit> 0-7 (expected e.g. DB1.DBX0.0)".to_string()
                })?;
                let start: u32 = start_raw
                    .parse()
                    .map_err(|_| format!("invalid byte offset {start_raw:?}"))?;
                let bit_index: u8 = bit_raw
                    .parse()
                    .map_err(|_| format!("bit index must be 0-7, got {bit_raw:?}"))?;
                if bit_index > 7 {
                    return Err(format!("bit index must be 0-7, got {bit_index}"));
                }
                Ok(PointAddress {
                    db,
                    area: None,
                    start,
                    bit: true,
                    bit_index,
                })
            }
            // DBB / DBW / DBD 字节/字/双字访问（task 11 扩展，记法对齐会话层）。
            "B" | "W" | "D" => {
                if rest.is_empty() {
                    return Err(format!("missing byte offset after DB{access}"));
                }
                if rest.contains('.') {
                    return Err(format!(
                        "DB{access} byte/word/dword address must not have bit suffix (got {rest:?})"
                    ));
                }
                if !rest.chars().all(|c| c.is_ascii_digit()) {
                    return Err(format!("invalid byte offset {rest:?}"));
                }
                let start: u32 = rest
                    .parse()
                    .map_err(|_| format!("byte offset out of range: {rest:?}"))?;
                if start > S7_MAX_BYTE_OFFSET {
                    return Err(format!(
                        "byte offset {start} out of range (max {S7_MAX_BYTE_OFFSET})"
                    ));
                }
                // area 记 'B'/'W'/'D'（非 MC 区，无冲突；'D' 与 MC D 区以
                // `db != 0` 区分——MC D 恒为 db=0，真实 PLC DB 号从 1 起）。
                let area = access.as_bytes()[0] as char;
                Ok(PointAddress {
                    db,
                    area: Some(area),
                    start,
                    bit: false,
                    bit_index: 0,
                })
            }
            _ => Err(format!(
                "unknown S7 access type {access:?} (expected DBX/DBB/DBW/DBD)"
            )),
        }
    }

    /// S7 过程映像位访问：`I<off>.<bit>` / `Q<off>.<bit>`（bit 0-7）。
    ///
    /// 仅位形式（IB/IW/ID、QB/QW/QD 字节/字/双字形式待后续扩展，会话层已支持）。
    fn parse_piq(upper: &str) -> Result<PointAddress, String> {
        // 'I' / 'Q' 由 parse() 分派保证。
        let area = upper.as_bytes()[0] as char;
        let rest = &upper[1..];
        let (off_raw, bit_raw) = rest.split_once('.').ok_or_else(|| {
            format!("{area} bit access requires <byte>.<bit> suffix (e.g. {area}0.1)")
        })?;
        if off_raw.is_empty() {
            return Err(format!("missing byte offset for {area} address"));
        }
        if !off_raw.chars().all(|c| c.is_ascii_digit()) {
            return Err(format!("invalid byte offset {off_raw:?}"));
        }
        let start: u32 = off_raw
            .parse()
            .map_err(|_| format!("byte offset out of range: {off_raw:?}"))?;
        if start > S7_MAX_BYTE_OFFSET {
            return Err(format!(
                "byte offset {start} out of range (max {S7_MAX_BYTE_OFFSET})"
            ));
        }
        if bit_raw.is_empty() {
            return Err(format!("missing bit index for {area} address"));
        }
        let bit_index: u8 = bit_raw
            .parse()
            .map_err(|_| format!("bit index must be 0-7, got {bit_raw:?}"))?;
        if bit_index > 7 {
            return Err(format!("bit index must be 0-7, got {bit_index}"));
        }
        Ok(PointAddress {
            db: 0,
            area: Some(area),
            start,
            bit: true,
            bit_index,
        })
    }

    /// Modbus 5 位区段编址：`4xxxx` = 保持寄存器（FC03）、`3xxxx` = 输入寄存器（FC04）。
    ///
    /// 首位数字为区段类型，其余数字为 **1 基** 寄存器号（`40001` → 协议地址 0）。
    /// 线圈 `0xxxx`（FC01）与离散输入 `1xxxx`（FC02）为位访问，不在 task 9 范围。
    /// 解析结果以 `area = Some('4' | '3')`、`start = 1 基寄存器号` 表示，
    /// 协议地址 = `start - 1` 由驱动读写时换算。
    fn parse_modbus(upper: &str) -> Result<PointAddress, String> {
        if !upper.chars().all(|c| c.is_ascii_digit()) {
            return Err(format!(
                "invalid modbus address {upper:?} (digits only expected)"
            ));
        }
        let (area_raw, register_raw) = upper.split_at(1);
        let area = match area_raw {
            "4" => '4',
            "3" => '3',
            "0" | "1" => {
                return Err(format!(
                    "coil/discrete input area {area_raw}xxxx (FC01/FC02) out of task 9 scope (registers 4xxxx/3xxxx only)"
                ));
            }
            other => {
                return Err(format!(
                    "unsupported modbus area {other}xxxx (expected 4xxxx holding or 3xxxx input)"
                ));
            }
        };
        let start: u32 = register_raw
            .parse()
            .map_err(|_| format!("invalid modbus register number {register_raw:?}"))?;
        if start == 0 {
            return Err(
                "modbus register number must be 1-based (e.g. 40001, not 40000)".to_string(),
            );
        }
        if start > u32::from(u16::MAX) + 1 {
            return Err(format!(
                "modbus register number {start} out of range (1..={})",
                u32::from(u16::MAX) + 1
            ));
        }
        Ok(PointAddress {
            db: 0,
            area: Some(area),
            start,
            bit: false,
            bit_index: 0,
        })
    }

    /// 线圈编址（控制面写路径专用）：`0xxxx` = 线圈（FC05），1 基线圈号。
    ///
    /// 与 [`Self::parse_modbus`] 刻意分离：读路径（`parse()`）仍按原契约**拒绝**
    /// `0xxxx` 线圈区（保持「读只支持 4xxxx/3xxxx」的语义不变），写路径单独用本
    /// 函数解析，避免改动既有读侧拒绝测试。协议地址 = 线圈号 - 1（与寄存器同口径）。
    fn parse_coil(upper: &str) -> Result<PointAddress, String> {
        if !upper.chars().all(|c| c.is_ascii_digit()) {
            return Err(format!(
                "invalid modbus coil address {upper:?} (digits only expected)"
            ));
        }
        let (area_raw, coil_raw) = upper.split_at(1);
        let number: u32 = coil_raw
            .parse()
            .map_err(|_| format!("invalid modbus coil number {coil_raw:?}"))?;
        if number == 0 {
            return Err(
                "modbus coil number must be 1-based (e.g. 00001, not 00000)".to_string(),
            );
        }
        if number > u32::from(u16::MAX) + 1 {
            return Err(format!(
                "modbus coil number {number} out of range (1..={})",
                u32::from(u16::MAX) + 1
            ));
        }
        Ok(PointAddress {
            db: 0,
            area: Some(area_raw.as_bytes()[0] as char), // '0'
            start: number,
            bit: true,
            bit_index: 0,
        })
    }

    /// 线圈编址（控制面写路径专用，见 [`Self::parse_coil`]）。
    pub fn parse_coil_address(raw: &str) -> DaemonResult<PointAddress> {
        let normalized = raw.trim().to_ascii_uppercase();
        let wrap = |detail: String| {
            DaemonError::ProtocolError(format!("invalid coil address {raw:?}: {detail}"))
        };
        if normalized.is_empty() {
            return Err(wrap("empty address".to_string()));
        }
        Self::parse_coil(&normalized).map_err(wrap)
    }

    /// MC 元件编址：`M<number>`（位）/ `D<number>`（字）。
    fn parse_mc(upper: &str) -> Result<PointAddress, String> {
        let mut chars = upper.chars();
        // 非空由 parse() 保证。
        let area = chars.next().expect("parse() rejects empty input");
        let number_raw = chars.as_str();
        let bit = match area {
            'M' => true,
            'D' => false,
            other => {
                return Err(format!(
                    "unsupported address area {other:?} (expected S7 DB… or MC M/D)"
                ))
            }
        };
        if number_raw.is_empty() {
            return Err(format!("missing device number for area {area}"));
        }
        if !number_raw.chars().all(|c| c.is_ascii_digit()) {
            return Err(format!("invalid device number {number_raw:?}"));
        }
        let start: u32 = number_raw
            .parse()
            .map_err(|_| format!("device number out of range: {number_raw:?}"))?;
        Ok(PointAddress {
            db: 0,
            area: Some(area),
            start,
            bit,
            bit_index: 0,
        })
    }
}

// ---- 断线重连 ----

/// 重连退避的最小间隔（防止 `initial = Duration::ZERO` 造成忙重试）。
const RECONNECT_MIN_DELAY: Duration = Duration::from_millis(1);

/// 带指数退避的断线重连基类（纯逻辑，无 IO，plan task 8）。
///
/// 退避序列：`initial, initial×multiplier, …`，每步以 `max` 封顶；
/// 默认 1s → 2s → 4s → … → 60s 封顶。连接成功后调用 [`Reconnector::reset`] 归零。
///
/// 构造时对非法参数做钳制（与 `MachineIdentity::new` 抬升 `min_anchors` 同思路）：
/// `initial` 抬升至 [`RECONNECT_MIN_DELAY`]、`max` 不小于 `initial`、`multiplier` 至少为 1。
#[derive(Debug, Clone)]
pub struct Reconnector {
    /// 初始退避间隔。
    initial: Duration,
    /// 退避上限。
    max: Duration,
    /// 每次失败后的倍率。
    multiplier: u32,
    /// 下一次 `next_delay()` 返回的间隔。
    current: Duration,
}

impl Reconnector {
    /// 创建重连器（参数按文档钳制，不报错）。
    pub fn new(initial: Duration, max: Duration, multiplier: u32) -> Self {
        let initial = initial.max(RECONNECT_MIN_DELAY);
        let max = max.max(initial);
        let multiplier = multiplier.max(1);
        Self {
            initial,
            max,
            multiplier,
            current: initial,
        }
    }

    /// 取下一次重连应等待的间隔并推进退避状态。
    ///
    /// 序列：`initial → initial×multiplier → …`（每步以 `max` 封顶，封顶后保持不变）。
    pub fn next_delay(&mut self) -> Duration {
        let delay = self.current;
        self.current = self.current.saturating_mul(self.multiplier).min(self.max);
        delay
    }

    /// 连接成功后归零：下次 `next_delay()` 重新从 `initial` 开始。
    pub fn reset(&mut self) {
        self.current = self.initial;
    }
}

impl Default for Reconnector {
    /// 默认：初始 1s、上限 60s、倍率 2（1s → 2s → … → 60s 封顶）。
    fn default() -> Self {
        Self::new(Duration::from_secs(1), Duration::from_secs(60), 2)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::ERR_PROTOCOL;

    // ---- 地址解析：happy path（QA 场景） ----

    /// QA Happy: 解析 "DB1.DBX0.0" → {db:1, start:0, bit:true}。
    #[test]
    fn parse_s7_dbx_bit_address() {
        let addr = PointAddressParser::parse("DB1.DBX0.0").expect("valid S7 bit address");
        assert_eq!(addr.db, 1, "db number");
        assert_eq!(addr.start, 0, "byte offset");
        assert!(addr.bit, "DBX is bit access");
        assert_eq!(addr.bit_index, 0, "bit index");
        assert_eq!(addr.area, None, "S7 address has no MC area");
    }

    /// S7 位访问非平凡取值：位号上限 7、多位偏移。
    #[test]
    fn parse_s7_dbx_nonzero_fields() {
        let addr = PointAddressParser::parse("DB12.DBX34.7").expect("valid");
        assert_eq!(
            (addr.db, addr.start, addr.bit, addr.bit_index),
            (12, 34, true, 7)
        );
    }

    /// QA Happy: MC 位元件 "M100" → {area:'M', start:100, bit:true}。
    #[test]
    fn parse_mc_bit_area_m() {
        let addr = PointAddressParser::parse("M100").expect("valid MC bit address");
        assert_eq!(addr.area, Some('M'), "area");
        assert_eq!(addr.start, 100, "device number");
        assert!(addr.bit, "M area is bit access");
        assert_eq!(addr.db, 0, "non-S7 db is 0");
        assert_eq!(addr.bit_index, 0, "MC has no explicit bit index");
    }

    /// QA Happy: MC 字元件 "D100" → {area:'D', start:100, bit:false}。
    #[test]
    fn parse_mc_word_area_d() {
        let addr = PointAddressParser::parse("D100").expect("valid MC word address");
        assert_eq!(addr.area, Some('D'), "area");
        assert_eq!(addr.start, 100, "device number");
        assert!(!addr.bit, "D area is word access");
    }

    /// 解析前 trim 空白 + 大小写归一（配置书写容错）。
    #[test]
    fn parse_is_trimmed_and_case_insensitive() {
        let lower = PointAddressParser::parse("  db1.dbx0.0  ").expect("lowercase ok");
        let upper = PointAddressParser::parse("DB1.DBX0.0").expect("uppercase ok");
        assert_eq!(lower, upper, "case/whitespace must not change result");
        let mc = PointAddressParser::parse(" m100 ").expect("mc lowercase ok");
        assert_eq!(mc.area, Some('M'));
        assert_eq!(mc.start, 100);
    }

    // ---- 地址解析：error path（QA 场景） ----

    /// QA Error: 解析无效地址 "INVALID" → 断言返回 ProtocolError（错误码 ERR_PROTOCOL）。
    #[test]
    fn parse_invalid_address_returns_protocol_error() {
        let err = PointAddressParser::parse("INVALID").expect_err("must be rejected");
        assert!(
            matches!(err, DaemonError::ProtocolError(_)),
            "must be ProtocolError: {err:?}"
        );
        assert_eq!(err.error_code(), ERR_PROTOCOL, "error code domain 1000");
        assert!(
            err.to_string().contains("INVALID"),
            "message keeps input: {err}"
        );
    }

    /// QA Error: 空串 / 纯空白 → ProtocolError。
    #[test]
    fn parse_empty_address_returns_error() {
        for raw in ["", "   ", "\t"] {
            let err = PointAddressParser::parse(raw).expect_err("must be rejected");
            assert!(
                matches!(err, DaemonError::ProtocolError(_)),
                "for {raw:?}: {err:?}"
            );
            assert_eq!(err.error_code(), ERR_PROTOCOL);
        }
    }

    /// QA Error: 格式错误集合全部收敛为 ProtocolError（错误码 1000，消息含原始输入）。
    #[test]
    fn parse_malformed_addresses_rejected() {
        let cases = [
            "DB",           // 缺 DB 号
            "DBX0.0",       // 缺 DB 号
            "DB1.",         // 缺访问规格
            "DB1.DB",       // 缺访问类型
            "DB1.DBX0",     // DBX 缺位号后缀
            "DB1.DBX0.",    // 位号为空
            "DB1.DBX.0",    // 字节偏移为空
            "DB1.DBX0.9",   // 位号越界（>7）
            "DB1.DBX0.0.1", // 多余分段
            "M",            // 缺元件号
            "D",            // 缺元件号
            "M-1",          // 负数元件号
            "D100.5",       // 字元件不支持位后缀
            "MX100",        // 元件号含字母
            "X100",         // 未支持的 MC 区
        ];
        for raw in cases {
            let err = PointAddressParser::parse(raw).expect_err(&format!("must reject {raw:?}"));
            assert!(
                matches!(err, DaemonError::ProtocolError(_)),
                "for {raw:?} must be ProtocolError: {err:?}"
            );
            assert_eq!(err.error_code(), ERR_PROTOCOL, "for {raw:?}");
            assert!(
                err.to_string().contains(raw),
                "message keeps input for {raw:?}: {err}"
            );
        }
    }

    // ---- 地址解析：Modbus 5 位区段编址（task 9 扩展） ----

    /// QA Happy: Modbus 保持寄存器 "40001" → {area:'4', start:1}，协议地址 0。
    #[test]
    fn modbus_parse_holding_register_40001() {
        let addr = PointAddressParser::parse("40001").expect("valid modbus holding address");
        assert_eq!(addr.area, Some('4'), "area '4' = FC03 holding");
        assert_eq!(addr.start, 1, "1-based register number");
        assert!(!addr.bit, "register is word access");
        assert_eq!(addr.db, 0, "non-S7 db is 0");
        assert_eq!(addr.bit_index, 0, "no bit index");
    }

    /// QA Happy: Modbus 输入寄存器 "30001" → {area:'3', start:1}（FC04 input）。
    #[test]
    fn modbus_parse_input_register_30001() {
        let addr = PointAddressParser::parse("30001").expect("valid modbus input address");
        assert_eq!(addr.area, Some('3'), "area '3' = FC04 input");
        assert_eq!(addr.start, 1, "1-based register number");
        assert!(!addr.bit, "register is word access");
    }

    /// 非平凡取值 + trim/大小写归一不影响纯数字地址。
    #[test]
    fn modbus_parse_nonzero_and_normalized() {
        let addr = PointAddressParser::parse("40100").expect("valid");
        assert_eq!((addr.area, addr.start), (Some('4'), 100));
        let padded = PointAddressParser::parse(" 465536 ").expect("trimmed");
        assert_eq!(
            (padded.area, padded.start),
            (Some('4'), 65536),
            "u16 max + 1"
        );
    }

    /// QA Error: Modbus 越界与线圈区收敛为 ProtocolError（错误码 1000）。
    #[test]
    fn modbus_parse_rejections_return_protocol_error() {
        let cases = [
            "00001",  // 线圈 FC01 — task 9 范围外
            "10001",  // 离散输入 FC02 — task 9 范围外
            "4",      // 缺寄存器号
            "40000",  // 寄存器号 0（1-based 起点为 1）
            "40001A", // 尾部非数字
            "465537", // 寄存器号 > 65536
            "90001",  // 未支持的首数字区段
        ];
        for raw in cases {
            let err = PointAddressParser::parse(raw).expect_err(&format!("must reject {raw:?}"));
            assert!(
                matches!(err, DaemonError::ProtocolError(_)),
                "for {raw:?} must be ProtocolError: {err:?}"
            );
            assert_eq!(err.error_code(), ERR_PROTOCOL, "for {raw:?}");
            assert!(
                err.to_string().contains(raw),
                "message keeps input for {raw:?}: {err}"
            );
        }
    }

    // ---- 地址解析：线圈编址（控制面写路径，task #140） ----

    /// QA Happy: 线圈 "00001" → {area:'0', start:1}，协议地址 0。
    #[test]
    fn parse_coil_00001_yields_area_zero() {
        let addr = PointAddressParser::parse_coil_address("00001").expect("valid coil");
        assert_eq!(addr.area, Some('0'), "coil area '0'");
        assert_eq!(addr.start, 1, "1-based coil number");
        assert!(addr.bit, "coil is bit access");
        assert_eq!(addr.db, 0);
        let padded = PointAddressParser::parse_coil_address(" 065536 ").expect("trimmed");
        assert_eq!((padded.area, padded.start), (Some('0'), 65536));
    }

    /// QA Error: 线圈编址越界与非法值收敛为 ProtocolError（码 1000）。
    #[test]
    fn parse_coil_rejections_return_protocol_error() {
        let cases = ["00000", "00001A", "70000", ""];
        for raw in cases {
            let err = PointAddressParser::parse_coil_address(raw)
                .expect_err(&format!("must reject {raw:?}"));
            assert!(
                matches!(err, DaemonError::ProtocolError(_)),
                "for {raw:?}: {err:?}"
            );
            assert_eq!(err.error_code(), ERR_PROTOCOL, "for {raw:?}");
        }
    }

    // ---- 地址解析：S7 字节/字/双字与 I/Q 位（task 11 扩展） ----

    /// QA Happy: DBB/DBW/DBD 解析为 {db, area=B/W/D, bit=false}，trim/大小写归一。
    #[test]
    fn parse_s7_dbb_dbw_dbd_sized_access() {
        let b = PointAddressParser::parse("DB2.DBB10").expect("valid byte access");
        assert_eq!(
            (b.db, b.area, b.start, b.bit, b.bit_index),
            (2, Some('B'), 10, false, 0)
        );
        let w = PointAddressParser::parse("DB2.DBW20").expect("valid word access");
        assert_eq!((w.db, w.area, w.start, w.bit), (2, Some('W'), 20, false));
        let d = PointAddressParser::parse("DB2.DBD40").expect("valid dword access");
        assert_eq!((d.db, d.area, d.start, d.bit), (2, Some('D'), 40, false));
        let padded = PointAddressParser::parse("  db1.dbw6  ").expect("normalized");
        assert_eq!((padded.db, padded.area, padded.start), (1, Some('W'), 6));
        // 三字节地址上限内最大值可解析。
        let max = PointAddressParser::parse("DB1.DBD2097151").expect("valid max offset");
        assert_eq!((max.area, max.start), (Some('D'), 0x1F_FFFF));
    }

    /// QA Happy: 过程映像位 I0.1 / Q0.3 → {area, bit=true, bit_index=位号}。
    #[test]
    fn parse_s7_iq_bit_addresses() {
        let i = PointAddressParser::parse("I0.1").expect("valid I bit address");
        assert_eq!(
            (i.db, i.area, i.start, i.bit, i.bit_index),
            (0, Some('I'), 0, true, 1)
        );
        let q = PointAddressParser::parse("Q2.7").expect("valid Q bit address");
        assert_eq!(
            (q.area, q.start, q.bit, q.bit_index),
            (Some('Q'), 2, true, 7)
        );
        let padded = PointAddressParser::parse(" i0.1 ").expect("normalized");
        assert_eq!(padded, i, "trim/case must not change result");
    }

    /// QA Error: 新地址形式的非法输入全部收敛为 ProtocolError（码 1000，消息含原文）。
    #[test]
    fn parse_s7_extended_errors_rejected() {
        let cases = [
            "DB1.DBB",        // 缺字节偏移
            "DB1.DBW1.1",     // 字访问不允许位后缀
            "DB1.DBD-2",      // 负偏移
            "DB1.DBB2097152", // 偏移超三字节地址上限
            "DB1.DBW1A",      // 偏移含非数字
            "I0",             // I 位访问缺位号后缀
            "I0.8",           // 位号越界（>7）
            "Q.1",            // 字节偏移为空
            "Q1.x",           // 位号非数字
            "I",              // 缺偏移与位号
            "Q",              // 同上
        ];
        for raw in cases {
            let err = PointAddressParser::parse(raw).expect_err(&format!("must reject {raw:?}"));
            assert!(
                matches!(err, DaemonError::ProtocolError(_)),
                "for {raw:?} must be ProtocolError: {err:?}"
            );
            assert_eq!(err.error_code(), ERR_PROTOCOL, "for {raw:?}");
            assert!(
                err.to_string().contains(raw),
                "message keeps input for {raw:?}: {err}"
            );
        }
    }

    // ---- Reconnector ----

    /// 默认退避序列 1,2,4,8,16,32 → 封顶 60s 保持不变。
    #[test]
    fn reconnector_default_backoff_sequence_caps_at_max() {
        let mut r = Reconnector::default();
        let expected = [1u64, 2, 4, 8, 16, 32, 60, 60, 60].map(Duration::from_secs);
        for (i, want) in expected.iter().enumerate() {
            assert_eq!(r.next_delay(), *want, "attempt #{i}");
        }
    }

    /// 自定义参数：initial 500ms、max 2s、multiplier 3 → 500ms, 1.5s, 2s, 2s。
    #[test]
    fn reconnector_custom_config_sequence() {
        let mut r = Reconnector::new(Duration::from_millis(500), Duration::from_secs(2), 3);
        assert_eq!(r.next_delay(), Duration::from_millis(500));
        assert_eq!(r.next_delay(), Duration::from_millis(1500));
        assert_eq!(r.next_delay(), Duration::from_secs(2), "capped at max");
        assert_eq!(r.next_delay(), Duration::from_secs(2), "stays capped");
    }

    /// 连接成功 reset() 后退避从 initial 重新开始。
    #[test]
    fn reconnector_reset_restores_initial() {
        let mut r = Reconnector::default();
        assert_eq!(r.next_delay(), Duration::from_secs(1));
        assert_eq!(r.next_delay(), Duration::from_secs(2));
        assert_eq!(r.next_delay(), Duration::from_secs(4));
        r.reset();
        assert_eq!(r.next_delay(), Duration::from_secs(1), "back to initial");
        assert_eq!(r.next_delay(), Duration::from_secs(2), "back to doubling");
    }

    /// 非法配置钳制：initial=0 → 1ms 忙重试下限；multiplier=0 → 1（固定间隔）；
    /// max < initial → max 抬升到 initial。
    #[test]
    fn reconnector_clamps_invalid_config() {
        let mut r = Reconnector::new(Duration::ZERO, Duration::from_millis(500), 0);
        assert_eq!(r.next_delay(), RECONNECT_MIN_DELAY, "zero initial floored");
        assert_eq!(
            r.next_delay(),
            RECONNECT_MIN_DELAY,
            "multiplier 0 floored to 1"
        );

        let mut r2 = Reconnector::new(Duration::from_secs(10), Duration::from_secs(5), 2);
        assert_eq!(r2.next_delay(), Duration::from_secs(10), "initial kept");
        assert_eq!(
            r2.next_delay(),
            Duration::from_secs(10),
            "max lifted to initial, so 20s caps at 10s"
        );
    }

    // ---- Driver trait：mock 实现验证 trait 可被实现（QA: trait compile PASS） ----

    /// 测试用 mock 驱动：无网络，状态机可断言。
    #[derive(Debug, Default)]
    struct MockDriver {
        connected: bool,
        last_write: Option<Vec<u8>>,
    }

    #[async_trait]
    impl Driver for MockDriver {
        async fn connect(&mut self) -> DaemonResult<()> {
            self.connected = true;
            Ok(())
        }

        async fn read(&mut self, points: &[ReadPoint]) -> DaemonResult<Vec<PointSample>> {
            if !self.connected {
                return Err(DaemonError::ProtocolError(
                    "mock driver: not connected".to_string(),
                ));
            }
            Ok(points
                .iter()
                .map(|p| PointSample {
                    address: p.address.clone(),
                    value: vec![0x5A; p.count as usize * 2],
                })
                .collect())
        }

        async fn write(&mut self, points: &[WritePoint]) -> DaemonResult<()> {
            if !self.connected {
                return Err(DaemonError::ProtocolError(
                    "mock driver: not connected".to_string(),
                ));
            }
            self.last_write = points.first().map(|p| p.value.clone());
            Ok(())
        }

        async fn disconnect(&mut self) -> DaemonResult<()> {
            self.connected = false;
            Ok(())
        }
    }

    /// Driver trait 可被实现且生命周期闭环可用（connect → read → write → disconnect）。
    #[tokio::test]
    async fn mock_driver_implements_trait_end_to_end() {
        let mut driver = MockDriver::default();
        driver.connect().await.expect("connect");

        let addr = PointAddressParser::parse("M100").expect("address");
        let read_points = [ReadPoint {
            address: addr.clone(),
            count: 2,
        }];
        let samples = driver.read(&read_points).await.expect("read");
        assert_eq!(samples.len(), read_points.len(), "1:1 with request");
        assert_eq!(samples[0].address, addr, "echoes request address");
        assert_eq!(samples[0].value, vec![0x5A; 4], "count=2 → 4 bytes");

        driver
            .write(&[WritePoint {
                address: addr.clone(),
                value: vec![0xAB],
            }])
            .await
            .expect("write");
        assert_eq!(driver.last_write.as_deref(), Some([0xAB].as_slice()));

        driver.disconnect().await.expect("disconnect");
        assert!(!driver.connected, "disconnected");
    }

    /// Driver trait 对象安全：可装箱为 `Box<dyn Driver>` 并经动态分发调用。
    #[tokio::test]
    async fn driver_is_object_safe_boxed() {
        let mut driver: Box<dyn Driver> = Box::new(MockDriver::default());
        driver.connect().await.expect("connect");
        let addr = PointAddressParser::parse("DB1.DBX0.0").expect("address");
        let samples = driver
            .read(&[ReadPoint {
                address: addr.clone(),
                count: 1,
            }])
            .await
            .expect("read via dyn");
        assert_eq!(samples[0].address, addr);
        driver.disconnect().await.expect("disconnect");
    }

    /// 未连接 read 返回 ProtocolError（错误码 1000）而非 panic。
    #[tokio::test]
    async fn mock_driver_read_requires_connection() {
        let mut driver = MockDriver::default();
        let addr = PointAddressParser::parse("D100").expect("address");
        let err = driver
            .read(&[ReadPoint {
                address: addr,
                count: 1,
            }])
            .await
            .expect_err("must fail while disconnected");
        assert!(matches!(err, DaemonError::ProtocolError(_)));
        assert_eq!(err.error_code(), ERR_PROTOCOL);
        assert!(err.to_string().contains("not connected"), "{err}");
    }
}

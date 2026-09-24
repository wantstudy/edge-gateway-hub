//! task 11（阶段 A）— 西门子 S7comm 协议纯函数层（自研实现，无任何 IO）。
//!
//! ## 范围（阶段 A，纯函数）
//! - 地址解析 [`parse_s7_address`]：`DB<n>.DBX<off>.<bit>`（位）/ `DBB/DBW/DBD`
//!   （字节/字/双字）、`M/I/Q` 位（`M1.2`）与字节/字/双字（`MB1`/`MW2`/`MD4`）。
//! - TPKT 封装 [`build_tpkt`] / 解析 [`parse_tpkt`]（版本 0x0300、总长 u16 大端）。
//! - COTP：连接请求 [`build_cotp_cr`]、连接确认解析 [`parse_cotp_cc`]、
//!   DT 数据头 [`build_cotp_dt_data`]、TSAP 推导 [`derive_tsap`]（S7-300/400 风格）。
//! - S7 通信：建立通信 [`build_setup_communication`] / [`parse_setup_ack`]、
//!   读变量 [`build_read_var`]（多 Item）、写变量 [`build_write_var`]、
//!   读确认解析 [`parse_read_ack`]（ReturnCode 错误映射）。
//! - 大端读辅助 [`read_u16_be`] / [`read_u32_be`]。
//!
//! ## 阶段 B2（会话层，本文件 `S7Session`）
//! TcpStream 连接管理（三步握手）、读/写路径、同 DB 相邻地址合并读、
//! 按协商 PDU 拆分多请求、对端关闭自动重连一次；测试内含 TcpListener mock。
//!
//! ## 与任务签名的小偏差（为满足「零 panic + 协议错误一律 Result」红线）
//! - `build_tpkt` / `build_read_var` / `build_write_var` 返回 `Result<Vec<u8>, S7Error>`：
//!   载荷超 TPKT u16 长度上限、Item 数 > 255、写数据超 u16 等属协议参数错误，
//!   静默截断会产出损坏帧，故以 `Err` 上报而非 panic。
//! - `build_setup_communication` / `build_cotp_cr` 无可失败路径，按任务签名返回 `Vec<u8>`。
//!
//! ## 编码约定
//! - 所有字段线上为大端；S7 三字节地址 = `字节偏移 × 8 + 位号`（位号 0-7），
//!   因此字节偏移上限 `0x1F_FFFF`（解析期强制校验，构造器可保持无 panic）。
//! - 传输尺寸（transport size）：Bit=0x01（长度单位=位），Byte/Word/DWord=0x04
//!   （长度单位=字节）。`S7ReadItem::count` 与 [`build_write_var`] 的 `data`
//!   均按该单位解释（Bit 区为位数，其余为字节数）。
//! - Item 数据区对齐、Password/DateTime 等扩展功能未覆盖（见交付报告
//!   「未覆盖清单」）。

use std::fmt;
use std::io::{Read, Write};
use std::net::{Shutdown, TcpStream};
use std::time::Duration;

// ---- 错误类型 ----

/// S7 协议错误（模块内统一错误；pub 供阶段 B 会话层直接匹配）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum S7Error {
    /// 帧结构错误：长度不符 / 截断 / 版本或类型字段非法。
    BadFrame(String),
    /// 地址解析失败（消息含原始输入）。
    BadAddress(String),
    /// 调用方参数越界（载荷过长 / Item 过多等）。
    BadParam(String),
    /// PLC 返回非 OK 的 ReturnCode（0xFF 之外），`detail` 为固定说明。
    ReturnCode { code: u8, detail: &'static str },
    /// 网络层故障：连接失败 / 读写超时 / 对端关闭（阶段 B 会话层专用）。
    NetworkError(String),
}

impl fmt::Display for S7Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            S7Error::BadFrame(d) => write!(f, "s7 bad frame: {d}"),
            S7Error::BadAddress(d) => write!(f, "s7 bad address: {d}"),
            S7Error::BadParam(d) => write!(f, "s7 bad param: {d}"),
            S7Error::ReturnCode { code, detail } => {
                write!(f, "s7 plc return code 0x{code:02X}: {detail}")
            }
            S7Error::NetworkError(d) => write!(f, "s7 network error: {d}"),
        }
    }
}

impl std::error::Error for S7Error {}

// ---- 地址模型 ----

/// S7 存储区（Item 的 area 字节，S7comm 区代码）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum S7Area {
    /// 数据块 DB（0x84，对照 s7comm Area 代码表：0x84=DB）。
    Db,
    /// 位存储区 M（0x83）。
    M,
    /// 过程输入 I / PE（0x81）。
    I,
    /// 过程输出 Q / PA（0x82）。
    Q,
}

impl S7Area {
    /// S7comm Any 指针的 area 字节。
    pub fn code(self) -> u8 {
        match self {
            S7Area::Db => 0x84,
            S7Area::M => 0x83,
            S7Area::I => 0x81,
            S7Area::Q => 0x82,
        }
    }
}

/// 数据尺寸（决定 Item 的 transport size 与长度单位）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum S7Size {
    /// 位（transport 0x01，长度单位=位）。
    Bit,
    /// 字节（transport 0x04，长度单位=字节）。
    Byte,
    /// 字（transport 0x04，长度单位=字节）。
    Word,
    /// 双字（transport 0x04，长度单位=字节）。
    DWord,
}

impl S7Size {
    /// Item transport size 字节（见模块文档「编码约定」）。
    pub fn transport(self) -> u8 {
        match self {
            S7Size::Bit => 0x01,
            S7Size::Byte | S7Size::Word | S7Size::DWord => 0x04,
        }
    }

    /// 单元宽度（字节；Bit 为 1 表示占 1 字节容器）。
    pub fn width(self) -> u16 {
        match self {
            S7Size::Bit | S7Size::Byte => 1,
            S7Size::Word => 2,
            S7Size::DWord => 4,
        }
    }
}

/// 解析后的 S7 地址（阶段 B 直接用于组 Item）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct S7Address {
    /// 存储区。
    pub area: S7Area,
    /// DB 号（非 DB 区恒为 0）。
    pub db: u16,
    /// 字节偏移（≤ 0x1F_FFFF，三字节地址字段的编码上限）。
    pub byte_offset: u32,
    /// 位号 0-7（仅 Bit 尺寸非 0）。
    pub bit_index: u8,
    /// 数据尺寸。
    pub size: S7Size,
}

/// 三字节地址字段的偏移上限：`(byte_offset << 3) | bit_index` 须落在 24 位内。
const MAX_BYTE_OFFSET: u32 = 0x1F_FFFF;

/// 解析 S7 点位地址字符串（匹配前 trim 首尾空白并转大写）。
///
/// 支持语法：
///
/// | 输入示例 | 结果 |
/// |---|---|
/// | `DB1.DBX0.0` | DB 区位访问（byte_offset=0, bit=0） |
/// | `DB2.DBB10` / `DB2.DBW20` / `DB2.DBD40` | DB 区字节/字/双字（记录字节偏移） |
/// | `M1.2` / `I0.0` / `Q2.7` | M/I/Q 位访问 |
/// | `MB5` / `MW6` / `MD8`（I/Q 同理 `IB/IW/ID`、`QB/QW/QD`） | M/I/Q 字节/字/双字 |
///
/// 注意：裸 `M1`（无 `.bit` 后缀）为非法——位与字节形式必须显式区分。
///
/// # Errors
/// 空串 / 格式错误 / 位号 > 7 / 偏移越界（> [`MAX_BYTE_OFFSET`]）返回
/// [`S7Error::BadAddress`]，消息含原始输入。
pub fn parse_s7_address(raw: &str) -> Result<S7Address, S7Error> {
    let s = raw.trim().to_ascii_uppercase();
    let bad = |detail: String| S7Error::BadAddress(format!("{raw:?}: {detail}"));
    if s.is_empty() {
        return Err(bad("empty address".to_string()));
    }

    if let Some(rest) = s.strip_prefix("DB") {
        let dot = rest
            .find('.')
            .ok_or_else(|| bad("missing .DBX/.DBB/.DBW/.DBD access spec".to_string()))?;
        let db: u16 = rest[..dot]
            .parse()
            .map_err(|_| bad(format!("invalid DB number {:?}", &rest[..dot])))?;
        let spec = rest[dot + 1..]
            .strip_prefix("DB")
            .ok_or_else(|| bad("expected .DBX/.DBB/.DBW/.DBD after DB number".to_string()))?;
        let (size, tail) = match spec.as_bytes().first() {
            Some(b'X') => (S7Size::Bit, &spec[1..]),
            Some(b'B') => (S7Size::Byte, &spec[1..]),
            Some(b'W') => (S7Size::Word, &spec[1..]),
            Some(b'D') => (S7Size::DWord, &spec[1..]),
            _ => {
                return Err(bad(format!(
                    "unknown access type {spec:?} (X/B/W/D expected)"
                )))
            }
        };
        if size == S7Size::Bit {
            let (off, bit_raw) = tail
                .split_once('.')
                .ok_or_else(|| bad("DBX requires <byte>.<bit> suffix (e.g. DB1.DBX0.0)".into()))?;
            let bit_index: u8 = bit_raw
                .parse()
                .map_err(|_| bad(format!("bit index must be 0-7, got {bit_raw:?}")))?;
            if bit_index > 7 {
                return Err(bad(format!("bit index must be 0-7, got {bit_index}")));
            }
            Ok(S7Address {
                area: S7Area::Db,
                db,
                byte_offset: parse_offset(off, &bad)?,
                bit_index,
                size,
            })
        } else {
            if tail.contains('.') {
                return Err(bad(
                    "byte/word/dword address must not have bit suffix".into()
                ));
            }
            Ok(S7Address {
                area: S7Area::Db,
                db,
                byte_offset: parse_offset(tail, &bad)?,
                bit_index: 0,
                size,
            })
        }
    } else {
        let area = match s.as_bytes()[0] {
            b'M' => S7Area::M,
            b'I' => S7Area::I,
            b'Q' => S7Area::Q,
            _ => return Err(bad("expected DB… / M… / I… / Q… address".to_string())),
        };
        let rest = &s[1..];
        // MB/MW/MD（及 IB/IW/ID、QB/QW/QD）字节/字/双字形式。
        let sized = [
            (b'B', S7Size::Byte),
            (b'W', S7Size::Word),
            (b'D', S7Size::DWord),
        ]
        .into_iter()
        .find(|(c, _)| rest.as_bytes().first() == Some(c));
        if let Some((_, size)) = sized {
            let tail = &rest[1..];
            if tail.contains('.') {
                return Err(bad(
                    "byte/word/dword address must not have bit suffix".into()
                ));
            }
            Ok(S7Address {
                area,
                db: 0,
                byte_offset: parse_offset(tail, &bad)?,
                bit_index: 0,
                size,
            })
        } else {
            // 位形式：<byte>.<bit>。
            let (off, bit_raw) = rest.split_once('.').ok_or_else(|| {
                bad("bit access requires <byte>.<bit> (e.g. M1.2) or MB/MW/MD".into())
            })?;
            let bit_index: u8 = bit_raw
                .parse()
                .map_err(|_| bad(format!("bit index must be 0-7, got {bit_raw:?}")))?;
            if bit_index > 7 {
                return Err(bad(format!("bit index must be 0-7, got {bit_index}")));
            }
            Ok(S7Address {
                area,
                db: 0,
                byte_offset: parse_offset(off, &bad)?,
                bit_index,
                size: S7Size::Bit,
            })
        }
    }
}

/// 解析十进制字节偏移并强制三字节地址上限。
fn parse_offset(raw: &str, bad: &impl Fn(String) -> S7Error) -> Result<u32, S7Error> {
    if raw.is_empty() {
        return Err(bad("missing byte offset".to_string()));
    }
    let off: u32 = raw
        .parse()
        .map_err(|_| bad(format!("invalid byte offset {raw:?}")))?;
    if off > MAX_BYTE_OFFSET {
        return Err(bad(format!(
            "byte offset {off} out of range (max {MAX_BYTE_OFFSET})"
        )));
    }
    Ok(off)
}

// ---- 大端读辅助 ----

/// 从缓冲区 `off` 处读 u16（大端）。缓冲区不足返回 [`S7Error::BadFrame`]。
fn u16_at(buf: &[u8], off: usize) -> Result<u16, S7Error> {
    let end = off
        .checked_add(2)
        .ok_or_else(|| S7Error::BadFrame("offset overflow".to_string()))?;
    if buf.len() < end {
        return Err(S7Error::BadFrame(format!(
            "need 2 bytes at offset {off}, buffer has {} bytes",
            buf.len()
        )));
    }
    Ok(u16::from_be_bytes([buf[off], buf[off + 1]]))
}

/// 从缓冲区 `off` 处读 u32（大端）。缓冲区不足返回 [`S7Error::BadFrame`]。
fn u32_at(buf: &[u8], off: usize) -> Result<u32, S7Error> {
    let end = off
        .checked_add(4)
        .ok_or_else(|| S7Error::BadFrame("offset overflow".to_string()))?;
    if buf.len() < end {
        return Err(S7Error::BadFrame(format!(
            "need 4 bytes at offset {off}, buffer has {} bytes",
            buf.len()
        )));
    }
    let mut b = [0u8; 4];
    b.copy_from_slice(&buf[off..end]);
    Ok(u32::from_be_bytes(b))
}

/// 读大端 u16（缓冲区至少 2 字节，否则 [`S7Error::BadFrame`]）。
pub fn read_u16_be(buf: &[u8]) -> Result<u16, S7Error> {
    u16_at(buf, 0)
}

/// 读大端 u32（缓冲区至少 4 字节，否则 [`S7Error::BadFrame`]）。
pub fn read_u32_be(buf: &[u8]) -> Result<u32, S7Error> {
    u32_at(buf, 0)
}

// ---- TPKT ----

/// TPKT 封装：`03 00` + 总长（u16 大端，含 4 字节头）+ 载荷。
///
/// # Errors
/// 载荷超过 u16 总长上限（> 65531 字节）返回 [`S7Error::BadParam`]。
pub fn build_tpkt(payload: &[u8]) -> Result<Vec<u8>, S7Error> {
    let total = u16::try_from(payload.len() + 4).map_err(|_| {
        S7Error::BadParam(format!(
            "payload {} bytes overflows TPKT u16 length",
            payload.len()
        ))
    })?;
    let mut out = Vec::with_capacity(total as usize);
    out.extend_from_slice(&[0x03, 0x00]); // 版本 3.0
    out.extend_from_slice(&total.to_be_bytes());
    out.extend_from_slice(payload);
    Ok(out)
}

/// TPKT 解析：校验版本与总长，返回去掉 4 字节头后的 COTP/S7 载荷切片。
///
/// 允许缓冲区尾部有多余字节（TCP 粘包场景由阶段 B 按总长切帧）。
///
/// # Errors
/// 头部不足 4 字节 / 版本非 0x0300 / 总长 < 4 / 报文截断 → [`S7Error::BadFrame`]。
pub fn parse_tpkt(buf: &[u8]) -> Result<&[u8], S7Error> {
    if buf.len() < 4 {
        return Err(S7Error::BadFrame(format!(
            "TPKT header truncated: {} < 4 bytes",
            buf.len()
        )));
    }
    if buf[0] != 0x03 || buf[1] != 0x00 {
        return Err(S7Error::BadFrame(format!(
            "TPKT version must be 0x0300, got {:02X}{:02X}",
            buf[0], buf[1]
        )));
    }
    let total = u16_at(buf, 2)? as usize;
    if total < 4 {
        return Err(S7Error::BadFrame(format!(
            "TPKT total length {total} < 4 (header size)"
        )));
    }
    if total > buf.len() {
        return Err(S7Error::BadFrame(format!(
            "TPKT truncated: total length {total} > {} buffered bytes",
            buf.len()
        )));
    }
    Ok(&buf[4..total])
}

// ---- COTP ----

/// 由机架号 / 槽位号推导远程 TSAP（S7-300/400 风格）。
///
/// 高字节 = `0x03`（连接类型：PG=0x01 / OP=0x02 / S7 基本通信=0x03，取 0x03）；
/// 低字节 = `(机架号 << 5) | 槽位号`。机架合法域 0-7、槽位 0-31，越界值钳制到
/// 合法域（避免移位溢出 panic；真实 PLC 也不接受越界机架/槽位）。
pub fn derive_tsap(rack: u8, slot: u8) -> [u8; 2] {
    [0x03, (rack.min(7) << 5) | slot.min(31)]
}

/// COTP 连接请求（CR TPDU），`remote_tsap` 为 [`derive_tsap`] 产物。
///
/// golden 样例（rack=0, slot=0，对照真实 S7comm 抓包样式，逐字段见行内注释）。
pub fn build_cotp_cr(remote_tsap: [u8; 2]) -> Vec<u8> {
    vec![
        0x11, // LI：LI 字节之后共 17 字节
        0xE0, // TPDU 类型：CR（Connection Request）
        0x00,
        0x01, // 目的引用（CR 阶段固定 0x0001）
        0x00,
        0x00, // 源引用（未分配，0x0000）
        0xF0, // 类字段：class 0（抓包样例值）
        0xC0,
        0x01,
        0x09, // TPDU 大小参数：2^9 = 512 字节
        0xC1,
        0x02,
        0x01,
        0x00, // 源 TSAP 参数（本地，固定 0x0100）
        0xC2,
        0x02,
        remote_tsap[0],
        remote_tsap[1], // 目的 TSAP 参数（远端，含机架/槽位）
    ]
}

/// COTP 连接确认（CC TPDU，类型 0xD0）解析：仅校验 LI 与类型，参数留待阶段 B。
///
/// # Errors
/// 截断 / 类型非 0xD0 → [`S7Error::BadFrame`]。
pub fn parse_cotp_cc(buf: &[u8]) -> Result<(), S7Error> {
    if buf.len() < 2 {
        return Err(S7Error::BadFrame(format!(
            "COTP truncated: {} < 2 bytes",
            buf.len()
        )));
    }
    let li = buf[0] as usize;
    if buf.len() < li + 1 {
        return Err(S7Error::BadFrame(format!(
            "COTP truncated: LI {li} + 1 > {} buffered bytes",
            buf.len()
        )));
    }
    if buf[1] != 0xD0 {
        return Err(S7Error::BadFrame(format!(
            "expected COTP CC TPDU (0xD0), got 0x{:02X}",
            buf[1]
        )));
    }
    Ok(())
}

/// COTP DT 数据头（`02 F0 80` = LI 2 + DT data + EOT=1），阶段 B 拼在 S7 PDU 前。
pub fn build_cotp_dt_data() -> [u8; 3] {
    [0x02, 0xF0, 0x80]
}

// ---- S7 头与通信 PDU ----

/// S7 请求头（ROSCTR=0x01 Job）：协议 ID、冗余保留、PDU 引用、参数/数据长度。
fn build_job_header(pdu_ref: u16, param_len: u16, data_len: u16) -> Vec<u8> {
    let mut out = Vec::with_capacity(10 + param_len as usize + data_len as usize);
    out.extend_from_slice(&[0x32, 0x01]); // 协议 ID 0x32、ROSCTR 0x01（Job）
    out.extend_from_slice(&[0x00, 0x00]); // 冗余标识（保留）
    out.extend_from_slice(&pdu_ref.to_be_bytes());
    out.extend_from_slice(&param_len.to_be_bytes());
    out.extend_from_slice(&data_len.to_be_bytes());
    out
}

/// 建立通信请求（功能码 0xF0）：并行作业数固定 1/1，`pdu_len` 为期望 PDU 长度
/// （常见 480 / 960）。返回裸 S7 PDU（阶段 B 前拼 COTP DT 头、外层套 TPKT）。
pub fn build_setup_communication(pdu_ref: u16, pdu_len: u16) -> Vec<u8> {
    let mut out = build_job_header(pdu_ref, 8, 0);
    out.extend_from_slice(&[
        0xF0, // 功能码：Setup communication
        0x00, // 保留
    ]);
    out.extend_from_slice(&1u16.to_be_bytes()); // Max AMQ Caller（并行作业数）
    out.extend_from_slice(&1u16.to_be_bytes()); // Max AMQ Callee
    out.extend_from_slice(&pdu_len.to_be_bytes()); // 请求的 PDU 长度
    out
}

/// 建立通信确认解析：校验协议 ID / ROSCTR=ACK_DATA(0x03) / 功能码 0xF0 /
/// 错误类与错误码为 0，返回对端 PDU 长度。
///
/// # Errors
/// 帧结构不符或 ack 报错 → [`S7Error`]。
pub fn parse_setup_ack(buf: &[u8]) -> Result<u16, S7Error> {
    check_ack_header(buf, 8, 0xF0, "setup communication ack")?;
    // 参数区：F0(0) 00(1) MaxAmqCaller(2..4) MaxAmqCallee(4..6) PduLength(6..8)。
    u16_at(buf, 12 + 6)
}

/// ACK_DATA（ROSCTR 0x03）公共头校验：协议 ID、ROSCTR、错误类/码、参数长、功能码。
fn check_ack_header(
    buf: &[u8],
    min_param_len: usize,
    function: u8,
    what: &'static str,
) -> Result<(), S7Error> {
    if buf.first() != Some(&0x32) {
        return Err(S7Error::BadFrame(format!(
            "{what}: not an S7 telegram (protocol id must be 0x32)"
        )));
    }
    if buf.get(1) != Some(&0x03) {
        return Err(S7Error::BadFrame(format!(
            "{what}: expected ROSCTR ACK_DATA (0x03), got {:?}",
            buf.get(1)
        )));
    }
    // 头部 10 字节 + 错误类(10) + 错误码(11)；两者非 0 视为 PLC 报错。
    if let Some(&class) = buf.get(10) {
        if class != 0 {
            return Err(S7Error::ReturnCode {
                code: class,
                detail: "non-zero error class in ack header",
            });
        }
    }
    let param_len = u16_at(buf, 6)? as usize;
    if param_len < min_param_len {
        return Err(S7Error::BadFrame(format!(
            "{what}: param length {param_len} < {min_param_len}"
        )));
    }
    if buf.get(12) != Some(&function) {
        return Err(S7Error::BadFrame(format!(
            "{what}: expected function 0x{function:02X}, got {:?}",
            buf.get(12)
        )));
    }
    Ok(())
}

// ---- Read / Write Var ----

/// 读请求单 Item：结构化地址 + 读取量（Bit 区单位=位，其余=字节）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct S7ReadItem {
    /// 目标地址（[`parse_s7_address`] 产物）。
    pub address: S7Address,
    /// 读取量（单位见模块文档「编码约定」）。
    pub count: u16,
}

/// 编码一个 Any 指针 Item（12 字节）追加到 `out`。
///
/// `length_units`：Bit 区为位数、其余为字节数；地址 3 字节大端 =
/// `字节偏移 × 8 + 位号`（偏移上限已在解析期强制）。
fn encode_item(address: &S7Address, length_units: u16, out: &mut Vec<u8>) {
    out.extend_from_slice(&[
        0x12, // Item 头（变量规格）
        0x0A, // 后续长度：10 字节（地址任意形式）
        0x10, // 语法 ID：ANY
        address.size.transport(),
    ]);
    out.extend_from_slice(&length_units.to_be_bytes()); // 长度
    out.extend_from_slice(&address.db.to_be_bytes()); // DB 号
    out.push(address.area.code()); // 存储区
    let addr24 = (address.byte_offset << 3) | u32::from(address.bit_index);
    out.extend_from_slice(&addr24.to_be_bytes()[1..4]); // 3 字节大端地址
}

/// 读变量请求（功能码 0x04，多 Item）。返回裸 S7 PDU。
///
/// # Errors
/// Item 数 > 255 / 参数区超 u16 → [`S7Error::BadParam`]。
pub fn build_read_var(pdu_ref: u16, items: &[S7ReadItem]) -> Result<Vec<u8>, S7Error> {
    let count = u8::try_from(items.len()).map_err(|_| {
        S7Error::BadParam(format!("too many read items: {} (max 255)", items.len()))
    })?;
    let param_len = u16::try_from(2 + items.len() * 12)
        .map_err(|_| S7Error::BadParam("read param length overflow".to_string()))?;
    let mut out = build_job_header(pdu_ref, param_len, 0);
    out.push(0x04); // 功能码：Read Var
    out.push(count); // Item 数
    for item in items {
        encode_item(&item.address, item.count, &mut out);
    }
    Ok(out)
}

/// 写变量请求（功能码 0x05，单 Item）。`data` 编码语义：Bit 区每字节 0x00/0x01
/// 且长度 ≤ 1 字节，其余按目标尺寸的大端字节序列。返回裸 S7 PDU。
///
/// # Errors
/// 数据超出 u16 数据区 → [`S7Error::BadParam`]。
pub fn build_write_var(pdu_ref: u16, address: &S7Address, data: &[u8]) -> Result<Vec<u8>, S7Error> {
    // 数据区 = 每项数据头 4 字节（返回码 + 传输尺寸 + 长度）+ 数据本体。
    let data_len = u16::try_from(data.len() + 4)
        .map_err(|_| S7Error::BadParam(format!("write data {} bytes overflows u16", data.len())))?;
    let units = u16::try_from(if address.size == S7Size::Bit {
        u32::from(data.len() as u16) * 8
    } else {
        u32::from(data.len() as u16)
    })
    .map_err(|_| S7Error::BadParam("write length units overflow u16".to_string()))?;
    let mut out = build_job_header(pdu_ref, 2 + 12, data_len);
    out.extend_from_slice(&[0x05, 0x01]); // 功能码：Write Var、Item 数 1
    encode_item(address, units, &mut out);
    out.push(0x00); // 返回码（请求侧保留）
    out.push(address.size.transport());
    out.extend_from_slice(&units.to_be_bytes());
    out.extend_from_slice(data);
    Ok(out)
}

/// PLC ReturnCode → [`S7Error`]（0xFF=OK 由调用方分支处理，不进此函数）。
fn return_code_error(code: u8) -> S7Error {
    let detail = match code {
        0x05 => "address out of range（地址越界）",
        0x0A => "object does not exist（对象不存在）",
        0x03 => "reserved / access not allowed",
        0x06 | 0x07 => "data type not supported / mismatch",
        0x08 | 0x09 | 0x0B | 0x0C => "hardware fault or partial access",
        _ => "unexpected return code",
    };
    S7Error::ReturnCode { code, detail }
}

/// 读确认（ACK_DATA，功能码 0x04）解析：按 Item 顺序取出数据字节。
///
/// 任一 Item ReturnCode 非 0xFF（OK）→ 整体 [`Err`]（与 Driver 契约「任一点失败
/// 整体返回 Err」一致）；0x05=地址越界、0x0A=对象不存在等经 [`return_code_error`]
/// 映射。Bit Item 的长度字段为位数，按 `(位长 + 7) / 8` 回推数据字节数。
///
/// # Errors
/// 帧结构不符 / ReturnCode 非 OK / 数据截断 → [`S7Error`]。
pub fn parse_read_ack(buf: &[u8]) -> Result<Vec<Vec<u8>>, S7Error> {
    check_ack_header(buf, 2, 0x04, "read ack")?;
    let param_len = u16_at(buf, 6)? as usize;
    let data_len = u16_at(buf, 8)? as usize;
    let item_count = u32::from(buf[13]) as usize;
    let data_start = 12 + param_len; // 10 字节头 + 错误类 + 错误码 之后为参数区
    let data_end = data_start
        .checked_add(data_len)
        .ok_or_else(|| S7Error::BadFrame("data length overflow".to_string()))?;
    if buf.len() < data_end {
        return Err(S7Error::BadFrame(format!(
            "read ack truncated: data region needs {data_len} bytes, got {}",
            buf.len().saturating_sub(data_start)
        )));
    }
    let mut out = Vec::with_capacity(item_count);
    let mut cursor = data_start;
    for _ in 0..item_count {
        // 每项数据：返回码(1) 传输尺寸(1) 长度(2) 数据。
        let rc = buf[cursor];
        if rc != 0xFF {
            return Err(return_code_error(rc));
        }
        let transport = buf[cursor + 1];
        let units = u16_at(buf, cursor + 2)? as usize;
        let nbytes = if transport == S7Size::Bit.transport() {
            units.div_ceil(8)
        } else {
            units
        };
        let val_end = cursor + 4 + nbytes;
        if val_end > buf.len() {
            return Err(S7Error::BadFrame(format!(
                "read ack item data truncated: need {nbytes} bytes at offset {}",
                cursor + 4
            )));
        }
        out.push(buf[cursor + 4..val_end].to_vec());
        cursor = val_end;
    }
    Ok(out)
}

/// 写确认（ACK_DATA，功能码 0x05）解析：校验 Item 返回码。
///
/// 写 ACK 的数据区每 Item 仅 1 字节返回码；非 0xFF（OK）→ 经
/// [`return_code_error`] 映射为 [`S7Error::ReturnCode`]（0x05=地址越界等）。
///
/// # Errors
/// 帧结构不符 / ReturnCode 非 OK / 缺 Item 返回码 → [`S7Error`]。
pub fn parse_write_ack(buf: &[u8]) -> Result<(), S7Error> {
    check_ack_header(buf, 2, 0x05, "write ack")?;
    let param_len = u16_at(buf, 6)? as usize;
    let data_start = 12usize.saturating_add(param_len);
    let rc = *buf.get(data_start).ok_or_else(|| {
        S7Error::BadFrame(format!(
            "write ack truncated: missing item return code at offset {data_start}"
        ))
    })?;
    if rc != 0xFF {
        return Err(return_code_error(rc));
    }
    Ok(())
}

// ---- 会话层（阶段 B 切片一：握手 + 读路径）----

/// connect / 读写默认超时（秒）。握手与数据阶段统一使用。
const SESSION_TIMEOUT: Duration = Duration::from_secs(2);

/// connect 时请求的 PDU 长度（常见 S7-300/400 值；实际以 PLC ACK 协商结果为准）。
pub const S7_REQUESTED_PDU: u16 = 480;

/// 组一个 Read Var 请求帧的固定开销（字节）：TPKT 头 4 + COTP DT 头 3 +
/// S7 头 10 + 参数头（功能码 + Item 数）2。协商 PDU 扣除该值后为 Item 预算。
const READ_FRAME_OVERHEAD: usize = 4 + 3 + 10 + 2;

/// 单个 Any 指针 Item 的编码长度（0x12 0x0A + 10 字节体）。
const ITEM_ENCODED_LEN: usize = 12;

/// 组一个 Write Var 请求帧的固定开销（字节）：TPKT 头 4 + COTP DT 头 3 +
/// S7 头 10 + 参数头（功能码 + Item 数）2 + Item 12 + 数据头（返回码 +
/// 传输尺寸 + 长度）4。协商 PDU 扣除该值后为单帧写入数据预算。
const WRITE_FRAME_OVERHEAD: usize = 4 + 3 + 10 + 2 + ITEM_ENCODED_LEN + 4;

/// io::Error → [`S7Error::NetworkError`] 统一映射。
fn io_err(ctx: &str, e: std::io::Error) -> S7Error {
    S7Error::NetworkError(format!("{ctx}: {e}"))
}

/// S7 会话：已握手的 TCP 连接 + 协商 PDU 长度 + 自增 PDU 引用 + 重连端点信息。
///
/// 生命周期内假设独占连接（&mut self 保证）；读/写遇对端关闭类网络错误时
/// 自动重连一次并重试（[`S7Session::is_connection_lost`]），二次失败上抛。
#[derive(Debug)]
pub struct S7Session {
    stream: TcpStream,
    negotiated_pdu: u16,
    next_pdu_ref: u16,
    /// 重连端点：connect 地址（重连时原样复用）。
    addr: String,
    /// 重连端点：远程 TSAP（重连时原样复用）。
    tsap: [u8; 2],
}

impl S7Session {
    /// 连接并完成三步握手（全部 2s 超时）：
    /// 1. TCP connect → 2. COTP CR/CC → 3. Setup Communication / ACK。
    ///
    /// `tsap` 为 [`derive_tsap`] 产物。成功返回会话，协商 PDU 经
    /// [`S7Session::negotiated_pdu`] 读取。
    ///
    /// # Errors
    /// 连接失败 / 超时 / 对端拒连 → [`S7Error::NetworkError`]；
    /// 握手帧不符 → [`S7Error::BadFrame`]。
    pub fn connect(addr: &str, tsap: [u8; 2]) -> Result<Self, S7Error> {
        let stream =
            TcpStream::connect(addr).map_err(|e| io_err(&format!("tcp connect {addr}"), e))?;
        let mut session = Self::handshake(stream, tsap)?;
        session.addr = addr.to_string();
        session.tsap = tsap;
        Ok(session)
    }

    /// 握手核心（`stream` 已连接）：设超时 → CR/CC → Setup/ACK。
    fn handshake(mut stream: TcpStream, tsap: [u8; 2]) -> Result<Self, S7Error> {
        stream
            .set_read_timeout(Some(SESSION_TIMEOUT))
            .map_err(|e| io_err("set read timeout", e))?;
        stream
            .set_write_timeout(Some(SESSION_TIMEOUT))
            .map_err(|e| io_err("set write timeout", e))?;

        // 1) COTP CR → CC。
        let cr = build_tpkt(&build_cotp_cr(tsap))?;
        stream
            .write_all(&cr)
            .map_err(|e| io_err("send cotp cr", e))?;
        let cc_frame = recv_frame(&mut stream)?;
        parse_cotp_cc(parse_tpkt(&cc_frame)?)?;

        // 2) Setup Communication（COTP DT 头 + S7 PDU）→ ACK（取协商 PDU）。
        let mut cotp_s7 = build_cotp_dt_data().to_vec();
        cotp_s7.extend_from_slice(&build_setup_communication(0x0000, S7_REQUESTED_PDU));
        let setup = build_tpkt(&cotp_s7)?;
        stream
            .write_all(&setup)
            .map_err(|e| io_err("send setup", e))?;
        let ack_frame = recv_frame(&mut stream)?;
        let negotiated_pdu = parse_setup_ack(parse_tpkt(&ack_frame)?)?;

        Ok(Self {
            stream,
            negotiated_pdu,
            next_pdu_ref: 0x0001,
            addr: String::new(),
            tsap,
        })
    }

    /// 协商后的 PDU 长度（PLC ACK 返回值）。
    pub fn negotiated_pdu(&self) -> u16 {
        self.negotiated_pdu
    }

    /// 读一批点位：同区同 DB 相邻地址合并为单 Item，按协商 PDU 拆分多请求，
    /// 结果按原地址顺序逐点返回（每点字节宽度由其尺寸决定）。
    ///
    /// 任一 Item ReturnCode 非 OK → 整体 `Err`（与 Driver 契约一致）。
    /// 遇对端关闭类网络错误自动重连一次并整体重试，二次失败上抛。
    ///
    /// # Errors
    /// 网络 / 帧结构 / ReturnCode 错误 → [`S7Error`]；空切片直接返回空。
    pub fn read_points(&mut self, addrs: &[S7Address]) -> Result<Vec<Vec<u8>>, S7Error> {
        match self.read_points_once(addrs) {
            Err(e) if Self::is_connection_lost(&e) => {
                self.reconnect()?;
                self.read_points_once(addrs)
            }
            other => other,
        }
    }

    /// 单次读尝试（无重连）：合并 → 按协商 PDU 分块 → 逐请求执行并汇总切分。
    fn read_points_once(&mut self, addrs: &[S7Address]) -> Result<Vec<Vec<u8>>, S7Error> {
        if addrs.is_empty() {
            return Ok(Vec::new());
        }
        let groups = merge_adjacent_groups(addrs);
        let max_items = self.max_items_per_request();
        let mut results: Vec<Vec<u8>> = Vec::with_capacity(addrs.len());
        for chunk in groups.chunks(max_items) {
            let items: Vec<S7ReadItem> = chunk.iter().map(|(item, _)| item.clone()).collect();
            let pdu_ref = self.next_pdu_ref;
            self.next_pdu_ref = self.next_pdu_ref.wrapping_add(1);

            let mut cotp_s7 = build_cotp_dt_data().to_vec();
            cotp_s7.extend_from_slice(&build_read_var(pdu_ref, &items)?);
            let frame = build_tpkt(&cotp_s7)?;
            self.stream
                .write_all(&frame)
                .map_err(|e| io_err("send read var", e))?;
            let resp_frame = recv_frame(&mut self.stream)?;
            let payloads = parse_read_ack(parse_tpkt(&resp_frame)?)?;
            if payloads.len() != items.len() {
                return Err(S7Error::BadFrame(format!(
                    "read ack item count {} != request item count {}",
                    payloads.len(),
                    items.len()
                )));
            }
            // 合并 Item 的连续载荷按原地址宽度切分回逐点结果。
            for ((_, widths), payload) in chunk.iter().zip(payloads) {
                let mut cursor = 0usize;
                for &w in widths {
                    let end = cursor + usize::from(w);
                    if end > payload.len() {
                        return Err(S7Error::BadFrame(format!(
                            "read ack item payload {} bytes < {end} bytes for merged addresses",
                            payload.len()
                        )));
                    }
                    results.push(payload[cursor..end].to_vec());
                    cursor = end;
                }
            }
        }
        Ok(results)
    }

    /// 单个 Read Var 请求可承载的最大 Item 数：协商 PDU 扣除帧固定开销
    /// （[`READ_FRAME_OVERHEAD`]）后按每 Item 12 字节取整；至少 1（防御
    /// 异常小的协商值，避免空请求）。
    fn max_items_per_request(&self) -> usize {
        let budget = (self.negotiated_pdu as usize).saturating_sub(READ_FRAME_OVERHEAD);
        (budget / ITEM_ENCODED_LEN).max(1)
    }

    /// 批量写入点位：同区同 DB、字节区间相邻衔接的**非位**写入合并为单 Item
    /// 的 Write Var 报文（一帧一个 Item，数据为各点载荷顺序拼接），按协商 PDU
    /// 的单帧数据预算再切分；位写入永不合并（逐点一帧）。结果整体成功或整体
    /// 失败（任一帧 ACK 非 OK → 整体 `Err`，与 Driver 契约一致）。
    /// 遇对端关闭类网络错误自动重连一次并整体重试，二次失败上抛。
    ///
    /// `data` 编码语义与 [`build_write_var`] 一致；每个载荷长度必须与对应地址
    /// 尺寸宽度一致（Bit=1 字节 0x00/0x01、Byte=1、Word=2、DWord=4）。
    ///
    /// # Errors
    /// 网络 / 帧结构 / ReturnCode / 载荷宽度与地址不符 → [`S7Error`]；空切片直接返回。
    pub fn write_points(&mut self, reqs: &[(S7Address, Vec<u8>)]) -> Result<(), S7Error> {
        match self.write_points_once(reqs) {
            Err(e) if Self::is_connection_lost(&e) => {
                self.reconnect()?;
                self.write_points_once(reqs)
            }
            other => other,
        }
    }

    /// 单次批量写尝试（无重连）：合并 → 组内按 PDU 数据预算切帧 → 逐帧执行。
    fn write_points_once(&mut self, reqs: &[(S7Address, Vec<u8>)]) -> Result<(), S7Error> {
        if reqs.is_empty() {
            return Ok(());
        }
        let addrs: Vec<S7Address> = reqs.iter().map(|(a, _)| a.clone()).collect();
        let groups = merge_adjacent_groups(&addrs);
        let max_data = self.max_write_data_bytes();
        let mut cursor = 0usize; // reqs 下标（与 addrs 同序同长）
        for (item, widths) in groups {
            let member_count = widths.len();
            let mut start = 0usize; // 组内成员下标
            let mut offset = item.address.byte_offset;
            while start < member_count {
                let mut data: Vec<u8> = Vec::new();
                let mut taken = 0usize;
                while start + taken < member_count {
                    let width = usize::from(widths[start + taken]);
                    let payload = &reqs[cursor + start + taken].1;
                    // 防御：载荷宽度须与地址尺寸一致（适配层已 fail-fast 校验）。
                    if payload.len() != width {
                        return Err(S7Error::BadParam(format!(
                            "write payload {} bytes != address width {width} for {:?}",
                            payload.len(),
                            reqs[cursor + start + taken].0
                        )));
                    }
                    // 首成员必入帧（与 max_items 的 max(1) 同思路）；后续成员按
                    // 单帧数据预算断开。组内地址连续，任意断开均合法。
                    if !data.is_empty() && data.len() + width > max_data {
                        break;
                    }
                    data.extend_from_slice(payload);
                    taken += 1;
                }
                let mut frame_addr = item.address.clone();
                frame_addr.byte_offset = offset;
                let pdu_ref = self.next_pdu_ref;
                self.next_pdu_ref = self.next_pdu_ref.wrapping_add(1);
                let mut cotp_s7 = build_cotp_dt_data().to_vec();
                cotp_s7.extend_from_slice(&build_write_var(pdu_ref, &frame_addr, &data)?);
                let frame = build_tpkt(&cotp_s7)?;
                self.stream
                    .write_all(&frame)
                    .map_err(|e| io_err("send write var", e))?;
                let resp_frame = recv_frame(&mut self.stream)?;
                parse_write_ack(parse_tpkt(&resp_frame)?)?;
                // 每个载荷长度 == 其宽度（上方防御校验），故累计字节数即宽度之和。
                offset += data.len() as u32;
                start += taken;
            }
            cursor += member_count;
        }
        Ok(())
    }

    /// 单个 Write Var 请求的数据区预算（字节）：协商 PDU 扣除帧固定开销
    /// [`WRITE_FRAME_OVERHEAD`]；至少 1（防御异常小的协商值）。
    fn max_write_data_bytes(&self) -> usize {
        (self.negotiated_pdu as usize)
            .saturating_sub(WRITE_FRAME_OVERHEAD)
            .max(1)
    }

    /// 写单个点位（一次 Write Var 请求）。
    ///
    /// `data` 编码语义与 [`build_write_var`] 一致（Bit 区 0x00/0x01，其余大端）。
    /// 遇对端关闭类网络错误自动重连一次并重试，二次失败上抛。
    ///
    /// # Errors
    /// 网络 / 帧结构 / ReturnCode（PLC 拒绝写入）→ [`S7Error`]。
    pub fn write_point(&mut self, addr: &S7Address, data: &[u8]) -> Result<(), S7Error> {
        match self.write_point_once(addr, data) {
            Err(e) if Self::is_connection_lost(&e) => {
                self.reconnect()?;
                self.write_point_once(addr, data)
            }
            other => other,
        }
    }

    /// 单次写尝试（无重连）。
    fn write_point_once(&mut self, addr: &S7Address, data: &[u8]) -> Result<(), S7Error> {
        let pdu_ref = self.next_pdu_ref;
        self.next_pdu_ref = self.next_pdu_ref.wrapping_add(1);
        let mut cotp_s7 = build_cotp_dt_data().to_vec();
        cotp_s7.extend_from_slice(&build_write_var(pdu_ref, addr, data)?);
        let frame = build_tpkt(&cotp_s7)?;
        self.stream
            .write_all(&frame)
            .map_err(|e| io_err("send write var", e))?;
        let resp_frame = recv_frame(&mut self.stream)?;
        parse_write_ack(parse_tpkt(&resp_frame)?)
    }

    /// 对端关闭类错误判定：读到 EOF/连接重置（"peer closed"）、写失败
    /// （"send …" 上下文）。读超时不算——可能是 PLC 忙，重连反而放大故障。
    fn is_connection_lost(err: &S7Error) -> bool {
        match err {
            S7Error::NetworkError(msg) => msg.contains("peer closed") || msg.starts_with("send "),
            _ => false,
        }
    }

    /// 重连一次：按 connect 时的地址与 TSAP 重建会话（完整三步握手），
    /// 成功则整体替换自身（旧连接随之关闭，协商 PDU 以新握手为准）。
    ///
    /// # Errors
    /// 重连或握手失败 → [`S7Error`]。
    fn reconnect(&mut self) -> Result<(), S7Error> {
        let addr = self.addr.clone();
        let session = Self::connect(&addr, self.tsap)?;
        *self = session;
        Ok(())
    }

    /// 关闭连接（幂等；错误忽略——关闭失败无恢复路径）。
    pub fn shutdown(&mut self) {
        let _ = self.stream.shutdown(Shutdown::Both);
    }
}

/// 按精确字节数读取（循环消化短读；对端关闭 → NetworkError）。
fn read_exact_s7(stream: &mut TcpStream, buf: &mut [u8]) -> Result<(), S7Error> {
    let mut filled = 0;
    while filled < buf.len() {
        let n = stream.read(&mut buf[filled..]).map_err(|e| {
            // 连接重置/中止/异常关闭归一为「peer closed」，供会话层判定重连。
            if matches!(
                e.kind(),
                std::io::ErrorKind::ConnectionReset
                    | std::io::ErrorKind::ConnectionAborted
                    | std::io::ErrorKind::UnexpectedEof
            ) {
                S7Error::NetworkError(format!("peer closed connection: {e}"))
            } else {
                io_err("recv", e)
            }
        })?;
        if n == 0 {
            return Err(S7Error::NetworkError("peer closed connection".to_string()));
        }
        filled += n;
    }
    Ok(())
}

/// 同区同 DB、字节区间相邻衔接的非位地址合并（按输入顺序连续段）。
///
/// 返回合并后的读 Item 及段内各原始地址的字节宽度（用于应答切分）。
/// 不合拢的情形：位地址、跨区/跨 DB、字节区间有间隙、合并会使 `count`
/// 超 u16 上限（此时断开新段，保证编码无溢出）。
fn merge_adjacent_groups(addrs: &[S7Address]) -> Vec<(S7ReadItem, Vec<u16>)> {
    let mut groups: Vec<(S7ReadItem, Vec<u16>)> = Vec::new();
    for a in addrs {
        let width = a.size.width();
        let can_merge = match groups.last() {
            Some((item, widths)) => {
                item.address.size != S7Size::Bit
                    && a.size != S7Size::Bit
                    && item.address.area == a.area
                    && item.address.db == a.db
                    && item.address.byte_offset + widths.iter().map(|&w| u32::from(w)).sum::<u32>()
                        == a.byte_offset
                    && u32::from(item.count) + u32::from(width) <= u32::from(u16::MAX)
            }
            None => false,
        };
        if can_merge {
            if let Some((item, widths)) = groups.last_mut() {
                item.count += width;
                widths.push(width);
                continue;
            }
        }
        groups.push((
            S7ReadItem {
                address: a.clone(),
                count: width,
            },
            vec![width],
        ));
    }
    groups
}

/// 接收一帧 TPKT（读 4 字节头取总长 → 再读余体），返回完整 TPKT 帧。
///
/// 精确按总长读取天然处理粘包：后续帧字节留在内核缓冲区，下次接收续取。
///
/// # Errors
/// 超时 / 对端关闭 → [`S7Error::NetworkError`]；版本或总长非法 → [`S7Error::BadFrame`]。
fn recv_frame(stream: &mut TcpStream) -> Result<Vec<u8>, S7Error> {
    let mut head = [0u8; 4];
    read_exact_s7(stream, &mut head)?;
    if head[0] != 0x03 || head[1] != 0x00 {
        return Err(S7Error::BadFrame(format!(
            "TPKT version must be 0x0300, got {:02X}{:02X}",
            head[0], head[1]
        )));
    }
    let total = u16::from_be_bytes([head[2], head[3]]) as usize;
    if total < 4 {
        return Err(S7Error::BadFrame(format!("TPKT total length {total} < 4")));
    }
    let mut body = vec![0u8; total - 4];
    read_exact_s7(stream, &mut body)?;
    let mut frame = Vec::with_capacity(total);
    frame.extend_from_slice(&head);
    frame.extend_from_slice(&body);
    Ok(frame)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// hex 字符串 → 字节向量（golden bytes 断言辅助；空格被忽略）。
    fn hex(s: &str) -> Vec<u8> {
        s.split(' ')
            .filter(|t| !t.is_empty())
            .map(|t| u8::from_str_radix(t, 16).expect("valid hex byte"))
            .collect()
    }

    // ---- 地址解析 ----

    /// QA Happy: DB 区四种访问形式 + M/I/Q 位与字节形式全部解析正确。
    #[test]
    fn parse_s7_address_valid() {
        let a = parse_s7_address("DB1.DBX0.0").expect("valid");
        assert_eq!(
            a,
            S7Address {
                area: S7Area::Db,
                db: 1,
                byte_offset: 0,
                bit_index: 0,
                size: S7Size::Bit
            }
        );
        // DBB/DBW/DBD 记录字节偏移，位号恒 0。
        assert_eq!(
            parse_s7_address(" db2.dbb10 ").expect("valid").byte_offset,
            10
        );
        let w = parse_s7_address("DB2.DBW20").expect("valid");
        assert_eq!((w.db, w.byte_offset, w.size), (2, 20, S7Size::Word));
        let d = parse_s7_address("DB2.DBD40").expect("valid");
        assert_eq!((d.byte_offset, d.size), (40, S7Size::DWord));
        // M/I/Q 位访问。
        let m = parse_s7_address("M1.2").expect("valid");
        assert_eq!((m.area, m.byte_offset, m.bit_index), (S7Area::M, 1, 2));
        let i = parse_s7_address("I0.0").expect("valid");
        assert_eq!(i.area, S7Area::I);
        let q = parse_s7_address("Q2.7").expect("valid");
        assert_eq!((q.area, q.bit_index), (S7Area::Q, 7));
        // M/I/Q 字节/字/双字。
        let mb = parse_s7_address("MB5").expect("valid");
        assert_eq!(
            (mb.area, mb.size, mb.byte_offset),
            (S7Area::M, S7Size::Byte, 5)
        );
        let mw = parse_s7_address("MW6").expect("valid");
        assert_eq!(mw.size, S7Size::Word);
        let md = parse_s7_address("MD8").expect("valid");
        assert_eq!(md.size, S7Size::DWord);
        let ib = parse_s7_address("IB0").expect("valid");
        assert_eq!((ib.area, ib.size), (S7Area::I, S7Size::Byte));
        let qd = parse_s7_address("QD12").expect("valid");
        assert_eq!((qd.area, qd.size), (S7Area::Q, S7Size::DWord));
        // 偏移上限内最大值可解析。
        let max = parse_s7_address("DB1.DBD2097151").expect("valid");
        assert_eq!(max.byte_offset, 0x1F_FFFF);
    }

    /// QA Error: 非法地址集合全部收敛为 BadAddress（消息含原始输入）。
    #[test]
    fn parse_s7_address_invalid() {
        let cases = [
            "",               // 空串
            "DB",             // 缺 DB 号
            "DB1.",           // 缺访问规格
            "DB1.DB",         // 缺访问类型
            "DB1.DBZ0",       // 未知访问类型
            "DB1.DBX0",       // DBX 缺位后缀
            "DB1.DBX0.8",     // 位号越界（>7）
            "DB1.DBX.1",      // 字节偏移为空
            "DB1.DBW0.1",     // 字访问不允许位后缀
            "DB1.DBB-1",      // 负偏移
            "DB1.DBD2097152", // 偏移超三字节地址上限
            "M",              // 缺偏移
            "M1",             // 裸 M1：位/字节形式必须显式区分
            "MB",             // 缺偏移
            "MB1.1",          // 字节形式不允许位后缀
            "Z1.2",           // 未支持区
        ];
        for raw in cases {
            let err = parse_s7_address(raw).expect_err(&format!("must reject {raw:?}"));
            assert!(
                matches!(err, S7Error::BadAddress(_)),
                "for {raw:?}: {err:?}"
            );
            assert!(
                err.to_string().contains(raw),
                "message keeps input for {raw:?}: {err}"
            );
        }
    }

    /// 三字节地址编码：DB12.DBX34.7 → (34<<3)|7 = 0x000117，Item 尾 3 字节为 00 01 17。
    #[test]
    fn bit_address_24bit_encoding() {
        let addr = parse_s7_address("DB12.DBX34.7").expect("valid");
        let frame = build_read_var(
            0,
            &[S7ReadItem {
                address: addr,
                count: 1,
            }],
        )
        .expect("build");
        // Item 起始于 10 头 + 2 参数头 = 12；地址 3 字节在 Item 尾部（偏移 21..24）。
        assert_eq!(frame[21..24], hex("00 01 17")[..], "byte_offset*8 + bit");
    }

    // ---- TPKT ----

    /// QA: build/parse round-trip + golden 头 + 截断/版本错误。
    #[test]
    fn tpkt_round_trip_and_parse_errors() {
        let payload = [0xAA, 0xBB, 0xCC];
        let frame = build_tpkt(&payload).expect("build");
        assert_eq!(frame, hex("03 00 00 07 AA BB CC"), "golden TPKT");
        assert_eq!(parse_tpkt(&frame).expect("parse"), &payload, "round-trip");

        // 截断：总长声明 7 但只给了 6 字节。
        let truncated = &frame[..frame.len() - 1];
        assert!(matches!(parse_tpkt(truncated), Err(S7Error::BadFrame(_))));
        // 版本非 0x0300。
        let mut bad_version = frame.clone();
        bad_version[0] = 0x04;
        assert!(matches!(
            parse_tpkt(&bad_version),
            Err(S7Error::BadFrame(_))
        ));
    }

    // ---- COTP ----

    /// QA: CR golden（对照真实 S7comm 抓包样式逐字段注释）+ CC 解析 + 类型错误。
    #[test]
    fn cotp_cr_golden_and_cc_parse() {
        assert_eq!(derive_tsap(0, 2), [0x03, 0x02], "rack<<5 | slot");
        assert_eq!(derive_tsap(0, 0), [0x03, 0x00]);
        // 越界钳制（rack 8 → 7，slot 32 → 31）。
        assert_eq!(derive_tsap(8, 32), [0x03, (7 << 5) | 31]);

        let cr = build_cotp_cr(derive_tsap(0, 0));
        // golden：LI(11) | CR(E0) | dst-ref(0001) | src-ref(0000) | class(F0)
        //       | TPDU-size(C0 01 09) | src-TSAP(C1 02 0100) | dst-TSAP(C2 02 0300)
        assert_eq!(
            cr,
            hex("11 E0 00 01 00 00 F0 C0 01 09 C1 02 01 00 C2 02 03 00")
        );

        // CC（0xD0）合法：LI=6，其后 6 字节。
        let cc = hex("06 D0 00 01 00 00 00");
        assert_eq!(parse_cotp_cc(&cc), Ok(()));
        // 类型错误：E0 是 CR 不是 CC。
        let not_cc = hex("11 E0 00 01 00 00 F0 C0 01 09 C1 02 01 00 C2 02 03 00");
        assert!(matches!(parse_cotp_cc(&not_cc), Err(S7Error::BadFrame(_))));
        // DT 数据头固定 02 F0 80。
        assert_eq!(build_cotp_dt_data(), [0x02, 0xF0, 0x80]);
    }

    // ---- S7 通信 ----

    /// QA: Setup golden（pdu_ref=0、PDU 480）+ ACK 解析取对端 PDU 长度 0x03C0=960。
    #[test]
    fn setup_comm_golden_and_ack_parse() {
        let req = build_setup_communication(0, 480);
        // golden：32 01(Job) 0000(冗余) 0000(ref) 0008(param) 0000(data)
        //       | F0 00 0100(caller) 0100(callee) 01E0(PDU=480)
        // 注：参数区共 8 字节（1+1+2+2+2），与头部 param len=0x0008 一致
        //（对照真实 S7comm 抓包：… 00 08 00 00 F0 00 00 01 00 01 01 E0）。
        assert_eq!(
            req,
            hex("32 01 00 00 00 00 00 08 00 00 F0 00 00 01 00 01 01 E0")
        );

        // ACK golden：对端回 PDU 960（0x03C0），错误类/码为 0，参数区 8 字节同构。
        let ack = hex("32 03 00 00 00 00 00 08 00 00 00 00 F0 00 00 01 00 01 03 C0");
        assert_eq!(parse_setup_ack(&ack).expect("parse ack"), 960);
        // 错误类非 0 → ReturnCode 错误。
        let mut err_ack = ack.clone();
        err_ack[10] = 0x81;
        assert!(matches!(
            parse_setup_ack(&err_ack),
            Err(S7Error::ReturnCode { .. })
        ));
    }

    /// QA: Read 请求 golden（单 Item，DB1.DBX2.1 × 1 bit）。
    #[test]
    fn read_var_single_item_golden() {
        let addr = parse_s7_address("DB1.DBX2.1").expect("valid");
        let frame = build_read_var(
            0x0100,
            &[S7ReadItem {
                address: addr,
                count: 1,
            }],
        )
        .expect("build");
        // golden：头 10B（ref=0100、param=000E、data=0000）+ 功能码 04、项数 01
        //       + Item：12 0A 10 01(bit) 0001(1 位) 0001(DB=1) 84(DB 区) 000011((2<<3)|1)
        assert_eq!(
            frame,
            hex("32 01 00 00 01 00 00 0E 00 00 04 01 12 0A 10 01 00 01 00 01 84 00 00 11")
        );
    }

    /// QA: Read 请求 golden（双 Item：DB1.DBW0 × 2 字节 + M3.5 × 1 位）。
    #[test]
    fn read_var_double_item_golden() {
        let w = parse_s7_address("DB1.DBW0").expect("valid");
        let b = parse_s7_address("M3.5").expect("valid");
        let frame = build_read_var(
            0,
            &[
                S7ReadItem {
                    address: w,
                    count: 2,
                },
                S7ReadItem {
                    address: b,
                    count: 1,
                },
            ],
        )
        .expect("build");
        // golden：param=001A（2 + 2×12）、项数 02；Item1 transport 04、2 字节；
        // Item2 transport 01、1 位、区 83(M)、地址 (3<<3)|5=0x1D。
        assert_eq!(
            frame,
            hex("32 01 00 00 00 00 00 1A 00 00 04 02 \
                 12 0A 10 04 00 02 00 01 84 00 00 00 \
                 12 0A 10 01 00 01 00 00 83 00 00 1D")
        );
    }

    /// QA: Write 请求 golden：DB1.DBW0 ← 0x1234（大端 12 34）。
    #[test]
    fn write_var_golden_bigendian_0x1234() {
        let addr = parse_s7_address("DB1.DBW0").expect("valid");
        let frame = build_write_var(0, &addr, &[0x12, 0x34]).expect("build");
        // golden：param=000E、data=0006（数据头 4B + 2B）；数据区 00(返回码) 04(transport)
        //       0002(2 字节) + 大端 12 34。
        assert_eq!(
            frame,
            hex("32 01 00 00 00 00 00 0E 00 06 05 01 \
                 12 0A 10 04 00 02 00 01 84 00 00 00 \
                 00 04 00 02 12 34")
        );
        // 大端语义二次断言：数据本体经 read_u16_be 还原 0x1234。
        let value = read_u16_be(&frame[frame.len() - 2..]).expect("read value");
        assert_eq!(value, 0x1234);
    }

    /// QA: 大端读辅助（0x1234 / 0x12345678）+ 长度不足报错。
    #[test]
    fn be_read_helpers() {
        assert_eq!(read_u16_be(&hex("12 34")).expect("u16"), 0x1234);
        assert_eq!(read_u32_be(&hex("12 34 56 78")).expect("u32"), 0x1234_5678);
        assert!(matches!(read_u16_be(&[0x12]), Err(S7Error::BadFrame(_))));
        assert!(matches!(
            read_u32_be(&[0x12, 0x34]),
            Err(S7Error::BadFrame(_))
        ));
    }

    /// QA: Read ACK 解析 OK（字 Item + 位 Item 位长回推字节）。
    #[test]
    fn parse_read_ack_ok() {
        // 两项：FF 04 0004 + 4 字节数据；FF 01 0001（1 位 → 1 字节）+ 01。
        let ack = hex("32 03 00 00 00 00 00 02 00 0C 00 00 04 02 \
             FF 04 00 04 12 34 56 78 FF 01 00 01 01");
        let values = parse_read_ack(&ack).expect("parse ok");
        assert_eq!(
            values,
            vec![hex("12 34 56 78"), vec![0x01]],
            "per-item payloads in order"
        );
    }

    /// QA: Write ACK 解析——OK（0xFF）/ ReturnCode 0x0A / 缺返回码截断。
    #[test]
    fn parse_write_ack_ok_and_return_code_error() {
        // golden：头 10B + 错误类/码 00 00 + 参数（05 01）+ 数据区（FF）。
        let ok = hex("32 03 00 00 00 00 00 02 00 01 00 00 05 01 FF");
        assert_eq!(parse_write_ack(&ok), Ok(()));
        // 返回码 0x0A（对象不存在）→ ReturnCode 错误。
        let bad = hex("32 03 00 00 00 00 00 02 00 01 00 00 05 01 0A");
        match parse_write_ack(&bad) {
            Err(S7Error::ReturnCode { code: 0x0A, detail }) => {
                assert!(detail.contains("does not exist"), "detail: {detail}");
            }
            other => panic!("expected ReturnCode, got {other:?}"),
        }
        // 截断：数据区缺 Item 返回码。
        assert!(matches!(
            parse_write_ack(&ok[..ok.len() - 1]),
            Err(S7Error::BadFrame(_))
        ));
    }

    /// QA Error: ReturnCode 0x05（地址越界）/ 0x0A（对象不存在）→ ReturnCode 错误。
    #[test]
    fn parse_read_ack_return_code_errors() {
        for (code, detail_key) in [(0x05u8, "out of range"), (0x0A, "does not exist")] {
            let ack = hex(&format!(
                "32 03 00 00 00 00 00 02 00 04 00 00 04 01 {code:02X} 01 00 00"
            ));
            let err = parse_read_ack(&ack).expect_err("non-OK return code");
            match &err {
                S7Error::ReturnCode { code: c, detail } => {
                    assert_eq!(*c, code);
                    assert!(detail.contains(detail_key), "detail: {detail}");
                }
                other => panic!("expected ReturnCode, got {other:?}"),
            }
            assert!(err.to_string().contains(detail_key), "{err}");
        }
    }
}

/// 阶段 B 切片一：会话握手 + 读路径（mock S7 服务器端到端）。
#[cfg(test)]
mod session_tests {
    use super::*;

    use std::net::{SocketAddr, TcpListener};
    use std::sync::{Arc, Mutex};
    use std::thread;

    /// mock 脚本类型：每连接一组应答（组内多帧拼接为一次 write，构造粘包）。
    type MockScript = Vec<Vec<Vec<u8>>>;

    /// 多连接记录型 mock：每个 accept 消耗一份脚本；`hold=false` 时脚本用尽
    /// 后**主动关闭该连接**（供重连用例模拟对端关闭），`hold=true` 时保持
    /// 连接直至客户端关闭（粘包等旧用例语义）。全部脚本用尽且未 hold 时关闭
    /// 监听退出（后续 connect 将被拒绝，供二次失败用例）。收到的每个完整
    /// TPKT 请求帧按序记录入共享 `received`。
    fn spawn_mock_multi_holding(
        scripts: Vec<MockScript>,
        hold: bool,
    ) -> (SocketAddr, Arc<Mutex<Vec<Vec<u8>>>>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("mock bind");
        let addr = listener.local_addr().expect("mock addr");
        let received = Arc::new(Mutex::new(Vec::<Vec<u8>>::new()));
        let rec = Arc::clone(&received);
        thread::spawn(move || {
            for script in scripts {
                let Ok((mut sock, _)) = listener.accept() else {
                    return;
                };
                let _ = sock.set_read_timeout(Some(Duration::from_secs(5)));
                for group in script {
                    // 读取一帧请求（4 字节头 + 余体），记录后按序应答。
                    let mut head = [0u8; 4];
                    if read_exact_s7(&mut sock, &mut head).is_err() {
                        return;
                    }
                    let total = u16::from_be_bytes([head[2], head[3]]) as usize;
                    if total < 4 {
                        return;
                    }
                    let mut body = vec![0u8; total - 4];
                    if read_exact_s7(&mut sock, &mut body).is_err() {
                        return;
                    }
                    let mut frame = head.to_vec();
                    frame.extend_from_slice(&body);
                    rec.lock().expect("mock record lock").push(frame);
                    // 组内多帧拼一次 write（粘包）；空组 = 只收不发。
                    let mut glued: Vec<u8> = Vec::new();
                    for resp in group {
                        glued.extend_from_slice(&resp);
                    }
                    if !glued.is_empty() && sock.write_all(&glued).is_err() {
                        return;
                    }
                }
                if hold {
                    // 保持连接直至客户端关闭（避免测试期间 RST 干扰读超时）。
                    let mut drain = [0u8; 64];
                    loop {
                        match sock.read(&mut drain) {
                            Ok(0) | Err(_) => break,
                            Ok(_) => {}
                        }
                    }
                }
                // 非 hold：该连接脚本用尽即主动关闭（模拟对端关闭）。
                drop(sock);
            }
        });
        (addr, received)
    }

    /// 多连接 mock（默认用完即关）：供写断言 / 重连 / 拆分用例。
    fn spawn_mock_multi(scripts: Vec<MockScript>) -> (SocketAddr, Arc<Mutex<Vec<Vec<u8>>>>) {
        spawn_mock_multi_holding(scripts, false)
    }

    /// 单连接 mock（旧用例兼容封装，保持连接语义）：返回监听地址。
    fn spawn_mock(script: MockScript) -> SocketAddr {
        spawn_mock_multi_holding(vec![script], true).0
    }

    /// 测试用 Setup ACK（协商 PDU = 480 = 0x01E0，错误类/码为 0）。
    fn setup_ack_frame() -> Vec<u8> {
        hex("32 03 00 00 00 00 00 08 00 00 00 00 F0 00 00 01 00 01 01 E0")
    }

    /// 测试用 COTP CC（LI=6，类型 0xD0）。
    fn cotp_cc_frame() -> Vec<u8> {
        hex("06 D0 00 01 00 00 00")
    }

    /// 多 Item 读 ACK：每项 `(transport, units, data)` 按序拼接数据区。
    ///
    /// `transport`：0x04=字节（units=字节数）、0x01=位（units=位数）。
    fn read_ack_frame_items(items: &[(u8, u16, Vec<u8>)]) -> Vec<u8> {
        let mut ack = hex("32 03 00 00 00 00 00 02 00 00 00 00 04 01");
        ack[13] = items.len() as u8; // 参数区 Item 数
        let data_len: u16 = items.iter().map(|(_, _, d)| (4 + d.len()) as u16).sum();
        ack[8..10].copy_from_slice(&data_len.to_be_bytes());
        for (transport, units, data) in items {
            ack.extend_from_slice(&[0xFF, *transport]);
            ack.extend_from_slice(&units.to_be_bytes());
            ack.extend_from_slice(data);
        }
        ack
    }

    /// 单 Item 读 ACK（旧用例兼容封装）：`data` 为 Item 数据本体。
    fn read_ack_frame(transport: u8, units: u16, data: &[u8]) -> Vec<u8> {
        read_ack_frame_items(&[(transport, units, data.to_vec())])
    }

    /// 写 ACK（单 Item 返回码 OK）。
    fn write_ack_frame() -> Vec<u8> {
        hex("32 03 00 00 00 00 00 02 00 01 00 00 05 01 FF")
    }

    /// hex 字符串 → 字节向量（与纯函数层测试同构的本地辅助）。
    fn hex(s: &str) -> Vec<u8> {
        s.split(' ')
            .filter(|t| !t.is_empty())
            .map(|t| u8::from_str_radix(t, 16).expect("valid hex byte"))
            .collect()
    }

    /// 标准握手脚本：CR→CC、Setup→ACK（两组应答）。
    fn handshake_script() -> Vec<Vec<Vec<u8>>> {
        vec![
            vec![build_tpkt(&cotp_cc_frame()).expect("tpkt")],
            vec![build_tpkt(&setup_ack_frame()).expect("tpkt")],
        ]
    }

    /// QA Happy: mock 服务器三步握手成功，协商 PDU = 480。
    #[test]
    fn mock_connect_handshake_ok() {
        let addr = spawn_mock(handshake_script());
        let mut s = S7Session::connect(&addr.to_string(), derive_tsap(0, 2)).expect("connect");
        assert_eq!(s.negotiated_pdu(), 480);
        s.shutdown();
    }

    /// QA Happy: 读 DB1.DBW0（Word）→ 大端 0x1234。
    #[test]
    fn mock_read_word_bigendian_0x1234() {
        let mut script = handshake_script();
        script.push(vec![
            build_tpkt(&read_ack_frame(0x04, 2, &[0x12, 0x34])).expect("tpkt")
        ]);
        let addr = spawn_mock(script);
        let mut s = S7Session::connect(&addr.to_string(), derive_tsap(0, 0)).expect("connect");
        let a = parse_s7_address("DB1.DBW0").expect("addr");
        let vals = s.read_points(&[a]).expect("read");
        assert_eq!(vals.len(), 1);
        assert_eq!(
            read_u16_be(&vals[0]).expect("u16"),
            0x1234,
            "big-endian word"
        );
        s.shutdown();
    }

    /// QA Happy: 读 M1.2（Bit）→ 1 字节 0x01。
    #[test]
    fn mock_read_bit() {
        let mut script = handshake_script();
        script.push(vec![
            build_tpkt(&read_ack_frame(0x01, 1, &[0x01])).expect("tpkt")
        ]);
        let addr = spawn_mock(script);
        let mut s = S7Session::connect(&addr.to_string(), derive_tsap(0, 0)).expect("connect");
        let a = parse_s7_address("M1.2").expect("addr");
        let vals = s.read_points(&[a]).expect("read");
        assert_eq!(vals, vec![vec![0x01]]);
        s.shutdown();
    }

    /// QA 粘包: 两次 read 的应答被 mock 拼成一次 write，客户端精确分帧各取所得。
    #[test]
    fn mock_sticky_two_frames_framed_correctly() {
        let ack1 = build_tpkt(&read_ack_frame(0x04, 2, &[0x12, 0x34])).expect("tpkt");
        let ack2 = build_tpkt(&read_ack_frame(0x04, 4, &[0x56, 0x78, 0x9A, 0xBC])).expect("tpkt");
        let mut script = handshake_script();
        script.push(vec![ack1, ack2]); // 两帧粘连为一次 write
        let addr = spawn_mock(script);
        let mut s = S7Session::connect(&addr.to_string(), derive_tsap(0, 0)).expect("connect");
        let w = parse_s7_address("DB1.DBW0").expect("addr");
        let d = parse_s7_address("DB1.DBD10").expect("addr");
        let v1 = s.read_points(&[w]).expect("read 1");
        let v2 = s.read_points(&[d]).expect("read 2");
        assert_eq!(v1, vec![vec![0x12, 0x34]], "first glued frame");
        assert_eq!(v2, vec![vec![0x56, 0x78, 0x9A, 0xBC]], "second glued frame");
        s.shutdown();
    }

    /// QA Error: 握手无应答（mock 只收 CR 不回包随即关闭）→ NetworkError。
    #[test]
    fn mock_connect_no_reply_is_network_error() {
        // 空脚本：mock 只收 CR 不回包（EOF / 对端关闭）。
        let addr = spawn_mock(vec![vec![]]);
        let err =
            S7Session::connect(&addr.to_string(), derive_tsap(0, 0)).expect_err("must time out");
        assert!(matches!(err, S7Error::NetworkError(_)), "got {err:?}");
    }

    /// QA Error: 连接拒绝（无人监听端口）→ NetworkError。
    #[test]
    fn connect_refused_is_network_error() {
        // 占住一个端口随即释放，向该端口发起连接（环回上通常立即 ECONNREFUSED）。
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr");
        drop(listener);
        let err =
            S7Session::connect(&addr.to_string(), derive_tsap(0, 0)).expect_err("must be refused");
        assert!(matches!(err, S7Error::NetworkError(_)), "got {err:?}");
    }

    // ---- B2：写路径 ----

    /// QA Happy: 写 DB1.DBW0 ← 0x1234（大端）；mock 校验收到的 Write Var 数据字段。
    #[test]
    fn mock_write_point_dbw0_0x1234() {
        let mut script = handshake_script();
        script.push(vec![build_tpkt(&write_ack_frame()).expect("tpkt")]);
        let (addr, received) = spawn_mock_multi(vec![script]);
        let mut s = S7Session::connect(&addr.to_string(), derive_tsap(0, 0)).expect("connect");
        let a = parse_s7_address("DB1.DBW0").expect("addr");
        s.write_point(&a, &[0x12, 0x34]).expect("write");
        s.shutdown();

        let rec = received.lock().expect("lock");
        assert_eq!(rec.len(), 3, "CR + Setup + Write Var");
        let payload = parse_tpkt(&rec[2]).expect("tpkt");
        assert_eq!(payload[13], 0x05, "function = Write Var");
        // COTP DT(3) + S7 头(10) + 功能码/项数(2) + Item(12) + 数据头(4) = 31。
        assert_eq!(&payload[31..33], &[0x12, 0x34], "write data big-endian");
    }

    /// QA Error: 写 ACK 返回码 0x05（地址越界）→ ReturnCode，且不触发重连重试。
    #[test]
    fn mock_write_point_return_code_error_no_retry() {
        let mut ack = write_ack_frame();
        let last = ack.len() - 1;
        ack[last] = 0x05;
        let mut script = handshake_script();
        script.push(vec![build_tpkt(&ack).expect("tpkt")]);
        let (addr, received) = spawn_mock_multi(vec![script]);
        let mut s = S7Session::connect(&addr.to_string(), derive_tsap(0, 0)).expect("connect");
        let a = parse_s7_address("DB1.DBW0").expect("addr");
        let err = s.write_point(&a, &[0x12, 0x34]).expect_err("must fail");
        assert!(
            matches!(err, S7Error::ReturnCode { code: 0x05, .. }),
            "got {err:?}"
        );
        assert_eq!(
            received.lock().expect("lock").len(),
            3,
            "no reconnect retry"
        );
        s.shutdown();
    }

    // ---- B2：同 DB 相邻地址合并读 ----

    /// QA Happy: 同 DB 相邻两 Word 合并为单 Item（count=4），结果按原地址切分。
    #[test]
    fn mock_read_merges_adjacent_words_into_single_item() {
        let mut script = handshake_script();
        script.push(vec![build_tpkt(&read_ack_frame(
            0x04,
            4,
            &[0x12, 0x34, 0x56, 0x78],
        ))
        .expect("tpkt")]);
        let (addr, received) = spawn_mock_multi(vec![script]);
        let mut s = S7Session::connect(&addr.to_string(), derive_tsap(0, 0)).expect("connect");
        let w0 = parse_s7_address("DB1.DBW0").expect("addr");
        let w2 = parse_s7_address("DB1.DBW2").expect("addr");
        let vals = s.read_points(&[w0, w2]).expect("read");
        s.shutdown();

        assert_eq!(
            vals,
            vec![hex("12 34"), hex("56 78")],
            "split back per address"
        );
        let rec = received.lock().expect("lock");
        let payload = parse_tpkt(&rec[2]).expect("tpkt");
        assert_eq!(payload[14], 1, "merged into a single item");
        // Item 长度字段位于 payload[19..21]（Item 头 4 字节之后）。
        assert_eq!(
            &payload[19..21],
            &4u16.to_be_bytes(),
            "merged count = 4 bytes"
        );
    }

    /// QA: 合并边界——有间隙不合拢、位地址不合拢、跨 DB 不合拢；异宽可链式合并。
    #[test]
    fn merge_adjacent_groups_boundaries() {
        let g = |addrs: &[&str]| -> Vec<(S7ReadItem, Vec<u16>)> {
            let parsed: Vec<S7Address> = addrs
                .iter()
                .map(|a| parse_s7_address(a).expect("addr"))
                .collect();
            merge_adjacent_groups(&parsed)
        };
        // 间隙 2 字节：不合拢。
        assert_eq!(g(&["DB1.DBW0", "DB1.DBW4"]).len(), 2);
        // 位地址永不合并。
        assert_eq!(g(&["DB1.DBX0.0", "DB1.DBX1.0"]).len(), 2);
        // 跨 DB 不合拢。
        assert_eq!(g(&["DB1.DBW0", "DB2.DBW2"]).len(), 2);
        // Word+DWord+Byte 链式合并为单 Item（count=7，宽度 2/4/1）。
        let merged = g(&["DB1.DBW0", "DB1.DBD2", "DB1.DBB6"]);
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].0.count, 7);
        assert_eq!(merged[0].1, vec![2, 4, 1]);
    }

    // ---- B2：PDU 拆分 ----

    /// QA Happy: 40 个不连续 Word 超协商 PDU（480 → 每请求 38 Item）拆为 2 个 Read Var。
    #[test]
    fn mock_read_pdu_split_into_multiple_requests() {
        // 步进 4 字节（字宽 2 + 间隙 2），相邻地址不合并 → 40 个独立 Item。
        let addrs: Vec<S7Address> = (0..40usize)
            .map(|i| parse_s7_address(&format!("DB1.DBW{}", i * 4)).expect("addr"))
            .collect();
        let mut script = handshake_script();
        // 请求 1 应答 38 项（值 100+j），请求 2 应答 2 项（值 200+j）。
        for (base, count) in [(100u16, 38usize), (200u16, 2usize)] {
            let items: Vec<(u8, u16, Vec<u8>)> = (0..count)
                .map(|j| (0x04u8, 2u16, (base + j as u16).to_be_bytes().to_vec()))
                .collect();
            script.push(vec![
                build_tpkt(&read_ack_frame_items(&items)).expect("tpkt")
            ]);
        }
        let (addr, received) = spawn_mock_multi(vec![script]);
        let mut s = S7Session::connect(&addr.to_string(), derive_tsap(0, 0)).expect("connect");
        let vals = s.read_points(&addrs).expect("read");
        s.shutdown();

        let rec = received.lock().expect("lock");
        assert_eq!(rec.len(), 4, "CR + Setup + Read#1 + Read#2");
        let p1 = parse_tpkt(&rec[2]).expect("tpkt");
        let p2 = parse_tpkt(&rec[3]).expect("tpkt");
        assert_eq!(p1[13], 0x04, "function = Read Var");
        assert_eq!(p1[14], 38, "first request carries 38 items");
        assert_eq!(p2[14], 2, "second request carries the rest");
        // 结果顺序与取值逐一正确（按请求内序号还原）。
        assert_eq!(vals.len(), 40);
        for (i, v) in vals.iter().enumerate() {
            let expect = if i < 38 {
                100 + i as u16
            } else {
                200 + (i - 38) as u16
            };
            assert_eq!(read_u16_be(v).expect("u16"), expect, "index {i}");
        }
    }

    // ---- B2：断线重连 ----

    /// QA Happy: 首连接握手后被对端关闭 → 自动重连到同一端点的新连接完成读。
    #[test]
    fn mock_reconnect_after_peer_close_succeeds() {
        let mut script2 = handshake_script();
        script2.push(vec![
            build_tpkt(&read_ack_frame(0x04, 2, &[0x12, 0x34])).expect("tpkt")
        ]);
        // 连接 1：仅握手（随后被 mock 关闭）；连接 2：握手 + 正常应答。
        let (addr, _rec) = spawn_mock_multi(vec![handshake_script(), script2]);
        let mut s = S7Session::connect(&addr.to_string(), derive_tsap(0, 0)).expect("connect");
        let a = parse_s7_address("DB1.DBW0").expect("addr");
        let vals = s.read_points(&[a]).expect("read after auto-reconnect");
        s.shutdown();
        assert_eq!(vals, vec![hex("12 34")]);
    }

    /// QA Error: 对端关闭后重连仍失败（监听已关闭）→ 二次失败上抛 NetworkError。
    #[test]
    fn mock_reconnect_second_failure_propagates() {
        let (addr, _rec) = spawn_mock_multi(vec![handshake_script()]);
        let mut s = S7Session::connect(&addr.to_string(), derive_tsap(0, 0)).expect("connect");
        // 让 mock 线程处理完连接 1 并释放监听端口（脚本耗尽后 mock 关闭退出）。
        thread::sleep(Duration::from_millis(300));
        let a = parse_s7_address("DB1.DBW0").expect("addr");
        let err = s
            .read_points(&[a])
            .expect_err("second failure must propagate");
        assert!(matches!(err, S7Error::NetworkError(_)), "got {err:?}");
    }
}

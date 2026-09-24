//! task 12 — 三菱 MC 协议驱动（3E 帧，TCP，自研实现）。
//!
//! ## 协议范围
//! - 仅实现 **3E 帧（QnA 兼容 3E，二进制，无和校验）** over TCP；
//!   **4E 帧（带和校验的 QnA 兼容 4E）留待后续版本**（帧结构差异仅在副头部与
//!   尾部和校验字段，届时可复用本模块的 `build_request` / `parse_response` 骨架）。
//! - 支持软元件：D（数据寄存器，字）/ M（内部继电器，位）/ X（输入，位）/
//!   Y（输出，位），见 [`McDeviceCode`] 软元件代码表。
//! - 读：批量读 0x0401；写：批量写 0x1401；子指令：字单位 0x0000 / 位单位 0x0100。
//!
//! ## 帧布局（请求，自左向右为线上字节序）
//! 副头部 0x5000(u16 LE) | 网络号 0x00 | PC 号 0xFF | 模块 IO 0x03FF(u16 LE) |
//! 站号 0x00 | 请求数据长(u16 LE) | CPU 监视定时器 0x0010(u16 LE) |
//! 指令(u16 LE) | 子指令(u16 LE) | 软元件代码(1B) | 头编号(u32 LE) | 点数(u16 LE) |
//! 写数据(仅批量写)。
//!
//! ⚠ 与官方手册的字节序差异备注：MELSEC 官方文档中 3E 二进制帧副头部线上
//! 字节序为 `50 00`（大端语义），本实现按任务规约「副头部 0x5000（u16 LE）」
//! 编码为 `00 50`（响应侧同序校验）。接真机前需以 PLC 实测确认；若需调整，
//! 仅 [`SUBHEADER`] 一个常量与 [`McDriver::parse_response`] 的校验值需要改动。
//! 其余字段（模块 IO、数据长、定时器、指令、头编号）线上均为小端，与规约一致。
//!
//! ## 地址解析
//! [`crate::driver::PointAddressParser`]（driver/mod.rs，task 8 范围）仅支持
//! MC `M`/`D` 两区；MC 驱动还需 `X`/`Y` 区（且后续或需十六进制元件号等 MC 专有
//! 语法），而公共解析器不允许本任务修改，故本模块提供 MC 专用解析
//! [`parse_mc_address`]。解析结果仍是公共 [`crate::driver::PointAddress`] 扁平
//! 结构（`area = Some('D'|'M'|'X'|'Y')`），驱动读写前会再次自校验区合法性，
//! 因此调用方也可用 `PointAddressParser` 解析 M/D 地址后直接传入。
//!
//! ## 批量合并
//! 读：同区连续（或重叠）地址合并为单帧批量读（字区单帧上限 960 点、位区 7168 点，
//! 超限拆帧）；写：同区且地址紧邻的字/位写合并为单帧批量写。
//! driver/modbus.rs 无现成合并辅助函数（其按点位逐个请求），故本模块自实现
//! `merge_read_batches` / `merge_write_batches` 并注释于此。
//!
//! ## 位数据编码
//! 3E 位单位读/写中，每 2 个点位打包进 1 字节（低半字节在前，ON=0x1、OFF=0x0）；
//! 本驱动对上层 [`crate::driver::PointSample`] 归一为**每点 1 字节 0x00/0x01**，
//! 字元件每点原样透传 2 字节（线上为每字高字节在前的大端序）。
//!
//! ## 错误与重连
//! - 拨号失败 / 传输中断 → [`DaemonError::NetworkError`]（ERR_NETWORK=6000）；
//! - 请求超时 / 异常结束代码 / 帧格式错误 → [`DaemonError::ProtocolError`]
//!   （ERR_PROTOCOL=1000）；
//! - 断线重连复用 [`Reconnector`]：请求遇连接丢失按指数退避等待后重连一次并
//!   重试，重试仍失败返回错误；`connect()` 成功重置退避，恢复重连不重置。

use std::net::SocketAddr;
use std::time::Duration;

use async_trait::async_trait;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::time::timeout;

use crate::driver::{Driver, PointAddress, PointSample, ReadPoint, Reconnector, WritePoint};
use crate::error::{DaemonError, DaemonResult};

// ---- 帧常量（3E 帧，二进制） ----

/// 副头部（3E 请求；响应侧按任务规约同值校验，见模块文档字节序备注）。
const SUBHEADER: u16 = 0x5000;
/// 网络号（单网络固定 0x00）。
const NETWORK_NO: u8 = 0x00;
/// PC 号（本站固定 0xFF）。
const PC_NO: u8 = 0xFF;
/// 请求目标模块 I/O 编号（0x03FF，固定值）。
const MODULE_IO: u16 = 0x03FF;
/// 请求目标模块站号（固定 0x00）。
const STATION_NO: u8 = 0x00;
/// CPU 监视定时器（0x0010 = 16000ms 上限档，任务规约固定值）。
const CPU_MONITOR_TIMER: u16 = 0x0010;
/// 指令：批量读。
const CMD_BATCH_READ: u16 = 0x0401;
/// 指令：批量写。
const CMD_BATCH_WRITE: u16 = 0x1401;
/// 子指令：字单位。
const SUBCMD_WORD: u16 = 0x0000;
/// 子指令：位单位。
const SUBCMD_BIT: u16 = 0x0100;

/// 字元件（D 区）单帧最大点数（3E 帧字单位批量读/写上限 960 点）。
const WORD_MAX_POINTS: u32 = 960;
/// 位元件（M/X/Y 区）单帧最大点数（3E 帧位单位上限 7168 点）。
const BIT_MAX_POINTS: u32 = 7168;

/// 定长响应头字节数：副头部(2) + 网络号(1) + PC 号(1) + 模块 IO(2) + 站号(1) + 响应数据长(2)。
const RESP_HEADER_LEN: usize = 9;

// ---- 软元件代码表 ----

/// MC 软元件代码（3E 帧二进制代码；常量表由 [`McDeviceCode::code`] 提供、单测校验）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum McDeviceCode {
    /// 数据寄存器 D（字元件）。
    D,
    /// 内部继电器 M（位元件）。
    M,
    /// 输入继电器 X（位元件；十进制元件号编址）。
    X,
    /// 输出继电器 Y（位元件；十进制元件号编址）。
    Y,
}

impl McDeviceCode {
    /// 软元件代码字节（QnA 兼容 3E 二进制）。
    pub fn code(self) -> u8 {
        match self {
            McDeviceCode::D => 0xA8,
            McDeviceCode::M => 0x90,
            McDeviceCode::X => 0x9C,
            McDeviceCode::Y => 0x9D,
        }
    }

    /// 区字符（与 [`PointAddress::area`] 对应）。
    pub fn area_char(self) -> char {
        match self {
            McDeviceCode::D => 'D',
            McDeviceCode::M => 'M',
            McDeviceCode::X => 'X',
            McDeviceCode::Y => 'Y',
        }
    }

    /// 由区字符反查软元件（未知区返回 `None`）。
    pub fn from_area(area: char) -> Option<Self> {
        match area {
            'D' => Some(McDeviceCode::D),
            'M' => Some(McDeviceCode::M),
            'X' => Some(McDeviceCode::X),
            'Y' => Some(McDeviceCode::Y),
            _ => None,
        }
    }

    /// 是否位元件（位访问）。
    pub fn is_bit(self) -> bool {
        !matches!(self, McDeviceCode::D)
    }

    /// 该区单帧最大点数（读/写共用上限）。
    pub fn max_points(self) -> u32 {
        if self.is_bit() {
            BIT_MAX_POINTS
        } else {
            WORD_MAX_POINTS
        }
    }

    /// 该区对应的子指令（字单位 0x0000 / 位单位 0x0100）。
    fn subcmd(self) -> u16 {
        if self.is_bit() {
            SUBCMD_BIT
        } else {
            SUBCMD_WORD
        }
    }
}

// ---- 地址解析（MC 专用） ----

/// MC 专用点位地址解析（原因见模块文档「地址解析」节）。
///
/// 语法：`<区字母><十进制元件号>`，如 `D100` / `M50` / `X1F`（此处按十进制 1F=31? 否，
/// 仅十进制：`X31`）；首尾空白 trim、大小写不敏感。X/Y 区编号按十进制处理
/// （部分 PLC 系列按八进制/十六进制编址，留待真机验证后扩展，见模块文档）。
///
/// # Errors
/// 空串 / 未知区 / 元件号缺失或非十进制 / 越界（超 u32）→ [`DaemonError::ProtocolError`]。
pub fn parse_mc_address(raw: &str) -> DaemonResult<PointAddress> {
    let upper = raw.trim().to_ascii_uppercase();
    let wrap =
        |detail: String| DaemonError::ProtocolError(format!("invalid mc address {raw:?}: {detail}"));

    let mut chars = upper.chars();
    let area = chars.next().ok_or_else(|| wrap("empty address".to_string()))?;
    let code = McDeviceCode::from_area(area)
        .ok_or_else(|| wrap(format!("unsupported mc area {area:?} (D/M/X/Y only)")))?;
    let number_raw = chars.as_str();
    if number_raw.is_empty() {
        return Err(wrap(format!("missing device number for area {area}")));
    }
    if !number_raw.chars().all(|c| c.is_ascii_digit()) {
        return Err(wrap(format!(
            "invalid device number {number_raw:?} (decimal digits only)"
        )));
    }
    let start: u32 = number_raw
        .parse()
        .map_err(|_| wrap(format!("device number out of range: {number_raw}")))?;
    Ok(PointAddress {
        db: 0,
        area: Some(area),
        start,
        bit: code.is_bit(),
        bit_index: 0,
    })
}

// ---- 驱动配置 ----

/// MC 驱动配置。
#[derive(Debug, Clone)]
pub struct McConfig {
    /// PLC 侧 MC 协议服务地址（常见默认口 1025，现场按 PLC 参数配置）。
    pub addr: SocketAddr,
    /// 单次请求超时（超时按连接丢失处理并触发重连）。
    pub timeout: Duration,
    /// 断线重连退避（`connect()` 成功重置；恢复重连不重置）。
    pub reconnector: Reconnector,
}

impl Default for McConfig {
    fn default() -> Self {
        Self {
            addr: SocketAddr::from(([127, 0, 0, 1], 1025)),
            timeout: Duration::from_secs(3),
            reconnector: Reconnector::default(),
        }
    }
}

// ---- 请求级错误分类 ----

/// 请求级错误：决定是否触发重连（与 driver/modbus.rs 同分类策略）。
#[derive(Debug)]
enum McError {
    /// 连接丢失（拨号 / 读写传输 / 超时）→ 按退避重连并重试一次。
    ConnectionLost(String),
    /// 协议语义错误（异常结束代码 / 帧格式）→ 直接返回，不重连。
    Terminal(String),
}

impl From<McError> for DaemonError {
    fn from(err: McError) -> Self {
        match err {
            McError::ConnectionLost(msg) => DaemonError::NetworkError(msg),
            McError::Terminal(msg) => DaemonError::ProtocolError(msg),
        }
    }
}

// ---- 批量合并结构 ----

/// 合并后的批量读请求。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ReadBatch {
    /// 软元件区。
    pub code: McDeviceCode,
    /// 批内起始元件号。
    pub start: u32,
    /// 批内覆盖的总点数（字区：字数；位区：位数）。
    pub count: u32,
    /// 覆盖的原始请求下标（按输入顺序记录，用于回填采样）。
    pub indices: Vec<usize>,
}

/// 合并后的批量写请求。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct WriteBatch {
    /// 软元件区。
    pub code: McDeviceCode,
    /// 批内起始元件号。
    pub start: u32,
    /// 批内总点数（字区：字数；位区：位数）。
    pub count: u32,
    /// 线上写数据（字区：每字 2 字节大端序拼接；位区：2 点/字节低半字节在前）。
    pub data: Vec<u8>,
    /// 位数据待并入的低位半字节（奇数点位时非 None；帧编码前需 finalize）。
    pending: Option<u8>,
}

impl WriteBatch {
    /// 追加一个位（0/1）：低半字节在前，2 点并 1 字节。
    fn push_bit(&mut self, bit: u8) {
        match self.pending.take() {
            Some(low) => self.data.push(low | (bit << 4)),
            None => self.pending = Some(bit),
        }
    }

    /// 帧编码前收尾：奇数点位的尾数补齐为独立字节（高半字节 0）。
    fn finalize_bits(&mut self) {
        if let Some(low) = self.pending.take() {
            self.data.push(low);
        }
    }
}

/// MC 驱动（3E 帧 over TCP，自研帧编解码）。
pub struct McDriver {
    config: McConfig,
    stream: Option<TcpStream>,
    reconnector: Reconnector,
}

impl McDriver {
    /// 创建驱动（`reconnector` 从配置克隆为工作状态）。
    pub fn new(config: McConfig) -> Self {
        let reconnector = config.reconnector.clone();
        Self {
            config,
            stream: None,
            reconnector,
        }
    }

    /// 当前重连退避状态（只读；测试断言用）。
    pub fn reconnector(&self) -> &Reconnector {
        &self.reconnector
    }

    // ---- 连接管理 ----

    /// 建立 TCP 连接。
    async fn open(&self) -> DaemonResult<TcpStream> {
        let addr = self.config.addr;
        TcpStream::connect(addr)
            .await
            .map_err(|e| DaemonError::NetworkError(format!("mc connect {addr}: {e}")))
    }

    /// 惰性连接：未连接时先 `connect()`。
    async fn ensure_connected(&mut self) -> DaemonResult<()> {
        if self.stream.is_none() {
            self.connect().await?;
        }
        Ok(())
    }

    // ---- 帧编解码（纯函数，供 golden bytes 单测直接断言） ----

    /// 构建 3E 请求帧（布局见模块文档）。
    fn build_request(
        cmd: u16,
        subcmd: u16,
        code: McDeviceCode,
        head: u32,
        count: u16,
        payload: &[u8],
    ) -> Vec<u8> {
        // 请求数据 = CPU 监视定时器 + 指令 + 子指令 + 软元件代码 + 头编号 + 点数 + 写数据
        let mut data = Vec::with_capacity(payload.len() + 11);
        data.extend_from_slice(&cmd.to_le_bytes());
        data.extend_from_slice(&subcmd.to_le_bytes());
        data.push(code.code());
        data.extend_from_slice(&head.to_le_bytes());
        data.extend_from_slice(&count.to_le_bytes());
        data.extend_from_slice(payload);

        let mut frame = Vec::with_capacity(data.len() + RESP_HEADER_LEN);
        frame.extend_from_slice(&SUBHEADER.to_le_bytes());
        frame.push(NETWORK_NO);
        frame.push(PC_NO);
        frame.extend_from_slice(&MODULE_IO.to_le_bytes());
        frame.push(STATION_NO);
        // 请求数据长 = 请求数据 + CPU 监视定时器（2 字节）
        let data_len = data.len() as u16 + 2;
        frame.extend_from_slice(&data_len.to_le_bytes());
        frame.extend_from_slice(&CPU_MONITOR_TIMER.to_le_bytes());
        frame.extend_from_slice(&data);
        frame
    }

    /// 构建批量读请求帧。
    fn build_read_frame(batch: &ReadBatch) -> Vec<u8> {
        Self::build_request(
            CMD_BATCH_READ,
            batch.code.subcmd(),
            batch.code,
            batch.start,
            batch.count as u16,
            &[],
        )
    }

    /// 构建批量写请求帧。
    fn build_write_frame(batch: &WriteBatch) -> Vec<u8> {
        let mut batch = batch.clone();
        batch.finalize_bits();
        Self::build_request(
            CMD_BATCH_WRITE,
            batch.code.subcmd(),
            batch.code,
            batch.start,
            batch.count as u16,
            &batch.data,
        )
    }

    /// 解析 3E 响应帧：校验副头部 / 数据长 / 结束代码，返回数据段（可为空）。
    fn parse_response(frame: &[u8]) -> Result<Vec<u8>, McError> {
        // 最短响应 = 定长头(9) + 结束代码(2)
        if frame.len() < RESP_HEADER_LEN + 2 {
            return Err(McError::Terminal(format!(
                "mc response too short: {} bytes (need >= {})",
                frame.len(),
                RESP_HEADER_LEN + 2
            )));
        }
        let subheader = u16::from_le_bytes([frame[0], frame[1]]);
        if subheader != SUBHEADER {
            return Err(McError::Terminal(format!(
                "mc response subheader 0x{subheader:04X} != expected 0x{SUBHEADER:04X}"
            )));
        }
        let data_len = u16::from_le_bytes([frame[7], frame[8]]) as usize;
        if data_len < 2 {
            return Err(McError::Terminal(
                "mc response data length < 2 (missing end code)".to_string(),
            ));
        }
        if frame.len() < RESP_HEADER_LEN + data_len {
            return Err(McError::Terminal(format!(
                "mc response truncated: got {} bytes, header declares {}",
                frame.len(),
                RESP_HEADER_LEN + data_len
            )));
        }
        let end_code = u16::from_le_bytes([frame[9], frame[10]]);
        if end_code != 0 {
            return Err(McError::Terminal(format!(
                "mc error end code 0x{end_code:04X}"
            )));
        }
        Ok(frame[RESP_HEADER_LEN + 2..RESP_HEADER_LEN + data_len].to_vec())
    }

    // ---- 请求执行（超时 + 重连一次重试） ----

    /// 在已连接流上收发一帧；传输层问题归为 ConnectionLost，语义问题归为 Terminal。
    async fn exchange(&mut self, request: &[u8]) -> Result<Vec<u8>, McError> {
        let stream = self
            .stream
            .as_mut()
            .expect("exchange requires an open stream");
        stream
            .write_all(request)
            .await
            .map_err(|e| McError::ConnectionLost(format!("mc send request: {e}")))?;
        // 响应头定长 9 字节，其中请求数据长（偏移 7..9）决定剩余字节数。
        let mut header = [0u8; RESP_HEADER_LEN];
        stream
            .read_exact(&mut header)
            .await
            .map_err(|e| McError::ConnectionLost(format!("mc read response header: {e}")))?;
        let data_len = u16::from_le_bytes([header[7], header[8]]) as usize;
        let mut rest = vec![0u8; data_len];
        stream
            .read_exact(&mut rest)
            .await
            .map_err(|e| McError::ConnectionLost(format!("mc read response body: {e}")))?;
        let mut frame = header.to_vec();
        frame.extend_from_slice(&rest);
        Self::parse_response(&frame)
    }

    /// 执行一次请求（含超时与「重连一次并重试」恢复，语义同 driver/modbus.rs）。
    async fn run_request(&mut self, request: &[u8]) -> DaemonResult<Vec<u8>> {
        self.ensure_connected().await?;
        let result = timeout(self.config.timeout, self.exchange(request)).await;
        match result {
            Ok(Ok(data)) => Ok(data),
            Ok(Err(McError::Terminal(msg))) => Err(DaemonError::ProtocolError(msg)),
            Ok(Err(McError::ConnectionLost(reason))) => self.recover(reason, request).await,
            Err(_) => {
                self.recover("mc request timeout".to_string(), request)
                    .await
            }
        }
    }

    /// 连接丢失恢复：按退避等待 → 重连 → 重试一次；重试仍失败则返回错误。
    async fn recover(&mut self, reason: String, request: &[u8]) -> DaemonResult<Vec<u8>> {
        let delay = self.reconnector.next_delay();
        tokio::time::sleep(delay).await;
        match self.open().await {
            Ok(stream) => {
                self.stream = Some(stream);
                match timeout(self.config.timeout, self.exchange(request)).await {
                    Ok(Ok(data)) => Ok(data),
                    Ok(Err(e)) => {
                        self.reconnector.next_delay();
                        Err(e.into())
                    }
                    Err(_) => {
                        self.reconnector.next_delay();
                        Err(DaemonError::ProtocolError(
                            "mc request timeout after reconnect".to_string(),
                        ))
                    }
                }
            }
            Err(e) => {
                self.reconnector.next_delay();
                let _ = reason; // 失败原因并入最终错误消息（保留可检索性）
                Err(DaemonError::NetworkError(format!("mc reconnect failed: {e}")))
            }
        }
    }

    // ---- 点位校验 ----

    /// 校验读取点位：返回（软元件, 起始元件号, 点数）。
    fn validate_read_point(p: &ReadPoint) -> DaemonResult<(McDeviceCode, u32, u32)> {
        let area = p.address.area.ok_or_else(|| {
            DaemonError::ProtocolError(format!(
                "mc read requires MC device area (D/M/X/Y), got {:?}",
                p.address
            ))
        })?;
        let code = McDeviceCode::from_area(area).ok_or_else(|| {
            DaemonError::ProtocolError(format!(
                "mc read unsupported device area {area:?} (D/M/X/Y only)"
            ))
        })?;
        if p.address.bit != code.is_bit() {
            return Err(DaemonError::ProtocolError(format!(
                "mc area {area} is {} access but address bit flag says {}",
                if code.is_bit() { "bit" } else { "word" },
                p.address.bit
            )));
        }
        let max = code.max_points();
        if p.count == 0 || p.count > max {
            return Err(DaemonError::ProtocolError(format!(
                "mc read count {} out of range 1..={max} for area {area}",
                p.count
            )));
        }
        // 头编号字段为 u32：起始 + 点数不得溢出。
        p.address
            .start
            .checked_add(p.count)
            .ok_or_else(|| DaemonError::ProtocolError("mc device range overflow".to_string()))?;
        Ok((code, p.address.start, p.count))
    }

    /// 校验写入点位：返回（软元件, 起始元件号, 点数）。
    fn validate_write_point(p: &WritePoint) -> DaemonResult<(McDeviceCode, u32, u32)> {
        let area = p.address.area.ok_or_else(|| {
            DaemonError::ProtocolError(format!(
                "mc write requires MC device area (D/M/X/Y), got {:?}",
                p.address
            ))
        })?;
        let code = McDeviceCode::from_area(area).ok_or_else(|| {
            DaemonError::ProtocolError(format!(
                "mc write unsupported device area {area:?} (D/M/X/Y only)"
            ))
        })?;
        if p.address.bit != code.is_bit() {
            return Err(DaemonError::ProtocolError(format!(
                "mc area {area} is {} access but address bit flag says {}",
                if code.is_bit() { "bit" } else { "word" },
                p.address.bit
            )));
        }
        p.address
            .start
            .checked_add(p.value.len() as u32)
            .ok_or_else(|| DaemonError::ProtocolError("mc device range overflow".to_string()))?;
        if code.is_bit() {
            // 位写：每点 1 字节，取值仅 0/1。
            for (i, &b) in p.value.iter().enumerate() {
                if b > 1 {
                    return Err(DaemonError::ProtocolError(format!(
                        "mc bit write value must be 0/1 per point, got {b} at index {i}"
                    )));
                }
            }
            let count = p.value.len() as u32;
            if count == 0 || count > BIT_MAX_POINTS {
                return Err(DaemonError::ProtocolError(format!(
                    "mc bit write count {count} out of range 1..={BIT_MAX_POINTS}"
                )));
            }
            Ok((code, p.address.start, count))
        } else {
            // 字写：每字 2 字节（大端序），数据长度必须为偶数。
            if p.value.is_empty() || p.value.len() % 2 != 0 {
                return Err(DaemonError::ProtocolError(format!(
                    "mc word write requires an even, non-zero byte length, got {} bytes",
                    p.value.len()
                )));
            }
            let count = p.value.len() as u32 / 2;
            if count > WORD_MAX_POINTS {
                return Err(DaemonError::ProtocolError(format!(
                    "mc word write count {count} out of range 1..={WORD_MAX_POINTS}"
                )));
            }
            Ok((code, p.address.start, count))
        }
    }

    // ---- 批量合并 ----

    /// 合并读取请求：同区连续/重叠地址并入单批；单批超出该区单帧上限则拆分。
    fn merge_read_batches(points: &[ReadPoint]) -> DaemonResult<Vec<ReadBatch>> {
        let mut entries: Vec<(McDeviceCode, u32, u32, usize)> =
            Vec::with_capacity(points.len());
        for (i, p) in points.iter().enumerate() {
            let (code, start, count) = Self::validate_read_point(p)?;
            entries.push((code, start, count, i));
        }
        // 按区、起始号排序后线性合并（同区中前一批的结束即衔接点）。
        entries.sort_by_key(|(code, start, _, _)| (code.area_char(), *start));

        let mut batches: Vec<ReadBatch> = Vec::new();
        for (code, start, count, idx) in entries {
            let end = start + count; // validate 阶段已保证不溢出
            let max = code.max_points();
            let need_new = match batches.last() {
                Some(b) => b.code != code || start > b.start + b.count,
                None => true,
            };
            if need_new {
                batches.push(ReadBatch {
                    code,
                    start,
                    count,
                    indices: vec![idx],
                });
                continue;
            }
            let batch = batches.last_mut().expect("need_new checked");
            let batch_end = batch.start + batch.count;
            if end > batch_end {
                // 合并会超出单帧上限：开新批（可能与前批部分重叠，属可接受冗余）。
                if end - batch.start > max {
                    batches.push(ReadBatch {
                        code,
                        start,
                        count,
                        indices: vec![idx],
                    });
                    continue;
                }
                batch.count = end - batch.start;
            }
            batch.indices.push(idx);
        }
        Ok(batches)
    }

    /// 合并写入请求：同区且地址紧邻（start == 前批结束）的写并入单批；超上限拆分。
    fn merge_write_batches(points: &[WritePoint]) -> DaemonResult<Vec<WriteBatch>> {
        let mut entries: Vec<(McDeviceCode, u32, u32, Vec<u8>)> = Vec::with_capacity(points.len());
        for p in points {
            let (code, start, count) = Self::validate_write_point(p)?;
            entries.push((code, start, count, p.value.clone()));
        }
        entries.sort_by_key(|(code, start, _, _)| (code.area_char(), *start));

        let mut batches: Vec<WriteBatch> = Vec::new();
        for (code, start, count, value) in entries {
            let max = code.max_points();
            let contiguous = match batches.last() {
                Some(b) => b.code == code && start == b.start + b.count,
                None => false,
            };
            if contiguous && batches.last().expect("contiguous checked").count + count <= max {
                let batch = batches.last_mut().expect("contiguous checked");
                batch.count += count;
                if code.is_bit() {
                    // 位数据打包：低半字节在前；紧邻位续排（含跨点位半字节 pending）。
                    for &bit in &value {
                        batch.push_bit(bit);
                    }
                } else {
                    batch.data.extend_from_slice(&value);
                }
            } else {
                let mut batch = WriteBatch {
                    code,
                    start,
                    count,
                    data: Vec::new(),
                    pending: None,
                };
                if code.is_bit() {
                    for &bit in &value {
                        batch.push_bit(bit);
                    }
                } else {
                    batch.data = value;
                }
                batches.push(batch);
            }
        }
        Ok(batches)
    }

}

#[async_trait]
impl Driver for McDriver {
    async fn connect(&mut self) -> DaemonResult<()> {
        match self.open().await {
            Ok(stream) => {
                self.stream = Some(stream);
                self.reconnector.reset();
                Ok(())
            }
            Err(e) => {
                self.reconnector.next_delay();
                Err(e)
            }
        }
    }

    async fn read(&mut self, points: &[ReadPoint]) -> DaemonResult<Vec<PointSample>> {
        if points.is_empty() {
            return Ok(Vec::new());
        }
        let batches = Self::merge_read_batches(points)?;
        let mut out: Vec<Option<PointSample>> = (0..points.len()).map(|_| None).collect();
        for batch in &batches {
            let frame = Self::build_read_frame(batch);
            let data = self.run_request(&frame).await?;
            // 响应数据长度校验：字区每点 2 字节；位区 2 点/字节。
            let expected = if batch.code.is_bit() {
                ((batch.count + 1) / 2) as usize
            } else {
                batch.count as usize * 2
            };
            if data.len() != expected {
                return Err(DaemonError::ProtocolError(format!(
                    "mc read response size mismatch for {}{}..: got {} bytes, expected {expected}",
                    batch.code.area_char(),
                    batch.start,
                    data.len()
                )));
            }
            for &i in &batch.indices {
                let p = &points[i];
                let offset = (p.address.start - batch.start) as usize;
                let value = if batch.code.is_bit() {
                    // 位采样归一：每点 1 字节 0x00/0x01（半字节非 0 即 ON）。
                    (0..p.count as usize)
                        .map(|j| {
                            let bitpos = offset + j;
                            let byte = data[bitpos / 2];
                            let nibble = if bitpos % 2 == 0 {
                                byte & 0x0F
                            } else {
                                byte >> 4
                            };
                            if nibble != 0 {
                                0x01u8
                            } else {
                                0x00u8
                            }
                        })
                        .collect()
                } else {
                    // 字采样：每字 2 字节大端序原样透传（类型解码交给数据处理链路）。
                    data[offset * 2..offset * 2 + p.count as usize * 2].to_vec()
                };
                out[i] = Some(PointSample {
                    address: p.address.clone(),
                    value,
                });
            }
        }
        Ok(out
            .into_iter()
            .map(|s| s.expect("every point index must be filled"))
            .collect())
    }

    async fn write(&mut self, points: &[WritePoint]) -> DaemonResult<()> {
        let batches = Self::merge_write_batches(points)?;
        for batch in &batches {
            let frame = Self::build_write_frame(batch);
            // 批量写响应仅含结束代码（数据段为空），run_request 内部已校验。
            self.run_request(&frame).await?;
        }
        Ok(())
    }

    async fn disconnect(&mut self) -> DaemonResult<()> {
        if let Some(mut stream) = self.stream.take() {
            // 对端可能已断开，shutdown 失败不升级为错误。
            let _ = stream.shutdown().await;
        }
        Ok(())
    }
}

// ---- 测试 ----

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    use crate::error::{ERR_NETWORK, ERR_PROTOCOL};

    /// hex 字符串 → 字节串（golden bytes 断言辅助；空格/下划线被忽略）。
    fn hex(s: &str) -> Vec<u8> {
        let clean: String = s
            .chars()
            .filter(|c| c.is_ascii_hexdigit())
            .collect();
        (0..clean.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&clean[i..i + 2], 16).expect("valid hex pair"))
            .collect()
    }

    /// 测试用配置：短超时 + 毫秒级退避（避免测试等待）。
    fn test_config(addr: SocketAddr) -> McConfig {
        McConfig {
            addr,
            timeout: Duration::from_secs(2),
            reconnector: Reconnector::new(Duration::from_millis(1), Duration::from_millis(2), 2),
        }
    }

    /// 测试用成功响应帧：副头部/网络/PC/IO/站 + 数据长 + 结束代码 0000 + 数据。
    fn build_ok_response(data: &[u8]) -> Vec<u8> {
        let mut frame = vec![0x00, 0x50, NETWORK_NO, PC_NO];
        frame.extend_from_slice(&MODULE_IO.to_le_bytes());
        frame.push(STATION_NO);
        frame.extend_from_slice(&((data.len() as u16 + 2).to_le_bytes()));
        frame.extend_from_slice(&0u16.to_le_bytes()); // 结束代码 0 = 成功
        frame.extend_from_slice(data);
        frame
    }

    // ---- mock 服务器（本机随机端口，禁真实网络） ----

    /// mock 服务器：对每个请求调用 handler 生成响应；可断言收到的请求字节。
    async fn spawn_mock_server<F>(handler: F) -> SocketAddr
    where
        F: Fn(Vec<u8>) -> Vec<u8> + Send + Sync + 'static,
    {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("local addr");
        let handler = Arc::new(handler);
        tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    return;
                };
                let handler = Arc::clone(&handler);
                tokio::spawn(async move {
                    let mut stream = stream;
                    loop {
                        // 读定长头 9 字节 → 按请求数据长补齐余量。
                        let mut header = [0u8; RESP_HEADER_LEN];
                        if stream.read_exact(&mut header).await.is_err() {
                            return;
                        }
                        let data_len = u16::from_le_bytes([header[7], header[8]]) as usize;
                        let mut rest = vec![0u8; data_len];
                        if stream.read_exact(&mut rest).await.is_err() {
                            return;
                        }
                        let mut request = header.to_vec();
                        request.extend_from_slice(&rest);
                        let response = handler(request);
                        if stream.write_all(&response).await.is_err() {
                            return;
                        }
                    }
                });
            }
        });
        addr
    }

    /// flaky 服务器：首个连接被接受后立即丢弃（模拟设备崩溃恢复），
    /// 之后所有连接正常服务。
    async fn spawn_flaky_server<F>(handler: F) -> SocketAddr
    where
        F: Fn(Vec<u8>) -> Vec<u8> + Send + Sync + 'static,
    {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("local addr");
        let handler = Arc::new(handler);
        tokio::spawn(async move {
            // 首连接：接受后立即断开。
            let Ok((stream, _)) = listener.accept().await else {
                return;
            };
            drop(stream);
            // 后续连接：正常回放。
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    return;
                };
                let handler = Arc::clone(&handler);
                tokio::spawn(async move {
                    let mut stream = stream;
                    let mut header = [0u8; RESP_HEADER_LEN];
                    if stream.read_exact(&mut header).await.is_err() {
                        return;
                    }
                    let data_len = u16::from_le_bytes([header[7], header[8]]) as usize;
                    let mut rest = vec![0u8; data_len];
                    if stream.read_exact(&mut rest).await.is_err() {
                        return;
                    }
                    let mut request = header.to_vec();
                    request.extend_from_slice(&rest);
                    let response = handler(request);
                    let _ = stream.write_all(&response).await;
                });
            }
        });
        addr
    }

    // ---- 软元件代码表（单测校验常量） ----

    /// QA: 软元件代码映射 D=0xA8、M=0x90、X=0x9C、Y=0x9D；区/位属性一致。
    #[test]
    fn device_code_table() {
        assert_eq!(McDeviceCode::D.code(), 0xA8);
        assert_eq!(McDeviceCode::M.code(), 0x90);
        assert_eq!(McDeviceCode::X.code(), 0x9C);
        assert_eq!(McDeviceCode::Y.code(), 0x9D);
        assert!(!McDeviceCode::D.is_bit(), "D is word device");
        for code in [McDeviceCode::M, McDeviceCode::X, McDeviceCode::Y] {
            assert!(code.is_bit(), "{code:?} is bit device");
            assert_eq!(
                McDeviceCode::from_area(code.area_char()),
                Some(code),
                "from_area/area_char round-trip"
            );
        }
        assert_eq!(McDeviceCode::from_area('Z'), None);
        assert_eq!(McDeviceCode::D.max_points(), WORD_MAX_POINTS);
        assert_eq!(McDeviceCode::M.max_points(), BIT_MAX_POINTS);
    }

    // ---- MC 地址解析 ----

    /// QA Happy: D/M/X/Y 四区 + trim/大小写归一。
    #[test]
    fn parse_mc_address_happy() {
        let d = parse_mc_address("D100").expect("D ok");
        assert_eq!(
            d,
            PointAddress {
                db: 0,
                area: Some('D'),
                start: 100,
                bit: false,
                bit_index: 0
            }
        );
        let m = parse_mc_address(" m50 ").expect("M ok");
        assert_eq!((m.area, m.start, m.bit), (Some('M'), 50, true));
        let x = parse_mc_address("X7").expect("X ok");
        assert_eq!((x.area, x.start, x.bit), (Some('X'), 7, true));
        let y = parse_mc_address("y40").expect("Y ok (case-insensitive)");
        assert_eq!((y.area, y.start, y.bit), (Some('Y'), 40, true));
    }

    /// QA Error: 空串 / 未知区 / 缺元件号 / 非十进制 / 越界 → ProtocolError。
    #[test]
    fn parse_mc_address_errors() {
        for raw in ["", "  ", "Z10", "D", "M-1", "D100.5", "X1F", "Y99999999999"] {
            let err = parse_mc_address(raw)
                .expect_err(&format!("must reject {raw:?}"));
            assert!(
                matches!(err, DaemonError::ProtocolError(_)),
                "for {raw:?}: {err:?}"
            );
            assert_eq!(err.error_code(), ERR_PROTOCOL, "for {raw:?}");
            assert!(err.to_string().contains(raw), "message keeps input: {err}");
        }
    }

    // ---- golden bytes：请求帧编码 ----

    /// QA: 批量读 D100 × 2（字）请求帧 golden bytes。
    #[test]
    fn golden_read_word_request_frame() {
        let frame = McDriver::build_request(CMD_BATCH_READ, SUBCMD_WORD, McDeviceCode::D, 100, 2, &[]);
        assert_eq!(
            frame,
            hex("00 50  00 FF  FF 03  00  0D 00  10 00  01 04  00 00  A8  64 00 00 00  02 00"),
            "3E request: subheader|net|pc|io|station|len=13|timer|cmd=0401|sub=0000|D|head=100|count=2"
        );
    }

    /// QA: 批量读 M50 × 3（位）请求帧 golden bytes。
    #[test]
    fn golden_read_bit_request_frame() {
        let frame = McDriver::build_request(CMD_BATCH_READ, SUBCMD_BIT, McDeviceCode::M, 50, 3, &[]);
        assert_eq!(
            frame,
            hex("00 50  00 FF  FF 03  00  0D 00  10 00  01 04  00 01  90  32 00 00 00  03 00"),
            "bit read: subcmd=0100|code=90|head=50(32h)|count=3"
        );
    }

    /// QA: 批量写 D100 ← 0x1234（字）请求帧 golden bytes。
    #[test]
    fn golden_write_word_request_frame() {
        let frame = McDriver::build_request(
            CMD_BATCH_WRITE,
            SUBCMD_WORD,
            McDeviceCode::D,
            100,
            1,
            &[0x12, 0x34],
        );
        assert_eq!(
            frame,
            hex("00 50  00 FF  FF 03  00  0F 00  10 00  01 14  00 00  A8  64 00 00 00  01 00  12 34"),
            "write: len=15|cmd=1401|payload 1234"
        );
    }

    // ---- golden bytes：响应帧解码 ----

    /// QA: 成功响应（结束代码 0000 + 数据 12 34 AB CD）解码出数据段。
    #[test]
    fn parse_response_success() {
        let frame = hex("00 50  00 FF  FF 03  00  06 00  00 00  12 34 AB CD");
        let data = McDriver::parse_response(&frame).expect("ok response");
        assert_eq!(data, vec![0x12, 0x34, 0xAB, 0xCD]);
    }

    /// QA: 异常结束代码（非 0）→ Terminal 错误，消息含结束代码。
    #[test]
    fn parse_response_error_end_code() {
        // 结束代码 0xC059（异常），数据长 = 2（仅结束代码）。
        let frame = hex("00 50  00 FF  FF 03  00  02 00  59 C0");
        let err = McDriver::parse_response(&frame).expect_err("must fail");
        let McError::Terminal(msg) = err else {
            panic!("end-code error must be terminal");
        };
        assert!(msg.contains("C059"), "message contains end code: {msg}");
    }

    /// QA Error: 帧过短 / 副头部不符 / 声明长度截断 → Terminal。
    #[test]
    fn parse_response_malformed_frames() {
        // 过短（无结束代码）。
        let short = hex("00 50  00 FF  FF 03  00  00 00");
        assert!(matches!(
            McDriver::parse_response(&short),
            Err(McError::Terminal(_))
        ));
        // 副头部不符。
        let bad_sub = hex("D0 00  00 FF  FF 03  00  02 00  00 00");
        assert!(matches!(
            McDriver::parse_response(&bad_sub),
            Err(McError::Terminal(_))
        ));
        // 声明数据长 6 但只给了结束代码。
        let truncated = hex("00 50  00 FF  FF 03  00  06 00  00 00");
        assert!(matches!(
            McDriver::parse_response(&truncated),
            Err(McError::Terminal(_))
        ));
    }

    // ---- 批量合并（纯函数） ----

    /// QA: 连续地址合并为单批；跨区不合并；不连续开新批。
    #[test]
    fn merge_read_batches_contiguous_and_split() {
        let pts = vec![
            ReadPoint {
                address: parse_mc_address("D100").expect("addr"),
                count: 2,
            },
            ReadPoint {
                address: parse_mc_address("D102").expect("addr"),
                count: 3,
            },
            ReadPoint {
                address: parse_mc_address("M50").expect("addr"),
                count: 2,
            },
            ReadPoint {
                address: parse_mc_address("D200").expect("addr"),
                count: 1,
            },
            ReadPoint {
                address: parse_mc_address("D103").expect("addr"), // 与 D100..D105 重叠
                count: 1,
            },
        ];
        let batches = McDriver::merge_read_batches(&pts).expect("merge ok");
        assert_eq!(batches.len(), 3, "D100-104 merged; D200 separate; M50 separate");
        assert_eq!(batches[0].code, McDeviceCode::D);
        assert_eq!((batches[0].start, batches[0].count), (100, 5),
            "D100+2 / D102+3 cover [100,105); D103+1 fully inside (no extension)");
        assert_eq!(batches[0].indices, vec![0, 1, 4], "input order preserved");
        // 批次按（区字母, 起始号）排序：D 区批次在前，采样回填按 indices 保证输入顺序。
        assert_eq!(
            (batches[1].code, batches[1].start, batches[1].count),
            (McDeviceCode::D, 200, 1)
        );
        assert_eq!(
            (batches[2].code, batches[2].start, batches[2].count),
            (McDeviceCode::M, 50, 2)
        );
    }

    /// QA: 合并超出单帧上限时拆批（960 字上限）。
    #[test]
    fn merge_read_batches_respects_frame_cap() {
        let pts = vec![
            ReadPoint {
                address: parse_mc_address("D0").expect("addr"),
                count: 960, // 顶满单帧
            },
            ReadPoint {
                address: parse_mc_address("D960").expect("addr"),
                count: 1, // 紧邻但并入会超限 → 拆批
            },
        ];
        let batches = McDriver::merge_read_batches(&pts).expect("merge ok");
        assert_eq!(batches.len(), 2, "cap split");
        assert_eq!(batches[0].count, 960);
        assert_eq!(batches[1].start, 960);
    }

    /// QA: 字写合并（数据拼接）与位写合并（半字节续排）。
    #[test]
    fn merge_write_batches_merges_contiguous_writes() {
        let word_writes = vec![
            WritePoint {
                address: parse_mc_address("D100").expect("addr"),
                value: vec![0x11, 0x11, 0x22, 0x22], // D100-D101
            },
            WritePoint {
                address: parse_mc_address("D102").expect("addr"),
                value: vec![0x33, 0x33], // D102 紧邻
            },
        ];
        let batches = McDriver::merge_write_batches(&word_writes).expect("merge ok");
        assert_eq!(batches.len(), 1, "contiguous word writes merged");
        assert_eq!((batches[0].start, batches[0].count), (100, 3));
        assert_eq!(batches[0].data, vec![0x11, 0x11, 0x22, 0x22, 0x33, 0x33]);

        let bit_writes = vec![
            WritePoint {
                address: parse_mc_address("M50").expect("addr"),
                value: vec![1, 0, 1], // M50/M51/M52
            },
            WritePoint {
                address: parse_mc_address("M53").expect("addr"),
                value: vec![1], // M53 紧邻 → 续排进同一批（4 位 → 2 字节）
            },
        ];
        let batches = McDriver::merge_write_batches(&bit_writes).expect("merge ok");
        assert_eq!(batches.len(), 1, "contiguous bit writes merged");
        assert_eq!((batches[0].start, batches[0].count), (50, 4));
        // 1,0 → 0x01；1,1 → 0x11（低半字节在前）。
        assert_eq!(batches[0].data, vec![0x01, 0x11]);
    }

    // ---- 校验错误路径（不触网） ----

    /// QA Error: 区不符 / 位标志不符 / 点数越界 / 写数据长度与取值非法。
    #[tokio::test]
    async fn validation_rejects_bad_points() {
        let mut driver = McDriver::new(test_config("127.0.0.1:1".parse().expect("addr")));

        // 未知区。
        let bad_area = ReadPoint {
            address: PointAddress {
                db: 0,
                area: Some('W'),
                start: 0,
                bit: false,
                bit_index: 0,
            },
            count: 1,
        };
        let err = driver.read(&[bad_area]).await.expect_err("unknown area");
        assert!(matches!(err, DaemonError::ProtocolError(_)), "{err:?}");
        assert_eq!(err.error_code(), ERR_PROTOCOL);

        // D 区（字）携带 bit=true 地址。
        let mut word_addr = parse_mc_address("D100").expect("addr");
        word_addr.bit = true;
        let err = driver
            .read(&[ReadPoint {
                address: word_addr,
                count: 1,
            }])
            .await
            .expect_err("bit flag mismatch");
        assert!(matches!(err, DaemonError::ProtocolError(_)));

        // 点数越界（字区 > 960）。
        let err = driver
            .read(&[ReadPoint {
                address: parse_mc_address("D0").expect("addr"),
                count: 961,
            }])
            .await
            .expect_err("count overflow");
        assert!(matches!(err, DaemonError::ProtocolError(_)));

        // 点数为 0。
        let err = driver
            .read(&[ReadPoint {
                address: parse_mc_address("M0").expect("addr"),
                count: 0,
            }])
            .await
            .expect_err("count 0");
        assert!(matches!(err, DaemonError::ProtocolError(_)));

        // 字写：奇数字节长度。
        let err = driver
            .write(&[WritePoint {
                address: parse_mc_address("D100").expect("addr"),
                value: vec![0x12],
            }])
            .await
            .expect_err("odd byte length");
        assert!(matches!(err, DaemonError::ProtocolError(_)));

        // 位写：取值非 0/1。
        let err = driver
            .write(&[WritePoint {
                address: parse_mc_address("M50").expect("addr"),
                value: vec![2],
            }])
            .await
            .expect_err("bit value must be 0/1");
        assert!(matches!(err, DaemonError::ProtocolError(_)));
    }

    /// 空请求直通：read/write 空切片返回空结果、不触网。
    #[tokio::test]
    async fn empty_requests_are_noop() {
        let mut driver = McDriver::new(test_config("127.0.0.1:1".parse().expect("addr")));
        assert!(driver.read(&[]).await.expect("empty read").is_empty());
        driver.write(&[]).await.expect("empty write");
    }

    // ---- TCP happy path ----

    /// QA Happy: 字读 round-trip（mock 服务器回放 canned 响应 + 请求字节断言）。
    #[tokio::test]
    async fn mock_read_words_roundtrip() {
        let requests: Arc<Mutex<Vec<Vec<u8>>>> = Arc::default();
        let req_log = Arc::clone(&requests);
        let addr = spawn_mock_server(move |request| {
            req_log.lock().expect("req lock").push(request);
            build_ok_response(&[0x12, 0x34, 0xAB, 0xCD]) // D100=0x1234, D101=0xABCD
        })
        .await;

        let mut driver = McDriver::new(test_config(addr));
        driver.connect().await.expect("connect");

        let point = ReadPoint {
            address: parse_mc_address("D100").expect("addr"),
            count: 2,
        };
        let samples = driver.read(&[point.clone()]).await.expect("read");
        assert_eq!(samples.len(), 1, "1:1 with request");
        assert_eq!(samples[0].address, point.address, "echoes request address");
        assert_eq!(samples[0].value, vec![0x12, 0x34, 0xAB, 0xCD], "word BE passthrough");

        // 请求帧与 golden bytes 完全一致。
        let logged = requests.lock().expect("req lock");
        assert_eq!(
            logged.as_slice(),
            &[McDriver::build_request(
                CMD_BATCH_READ,
                SUBCMD_WORD,
                McDeviceCode::D,
                100,
                2,
                &[]
            )],
            "server saw exact golden request frame"
        );
        driver.disconnect().await.expect("disconnect");
        assert!(driver.stream.is_none(), "stream released");
    }

    /// QA Happy: 位读 round-trip（2 点/字节低半字节在前 → 每点 1 字节 0/1）。
    #[tokio::test]
    async fn mock_read_bits_roundtrip() {
        let addr = spawn_mock_server(move |_request| {
            build_ok_response(&[0x11, 0x00]) // M50=ON, M51=ON, M52=OFF
        })
        .await;

        let mut driver = McDriver::new(test_config(addr));
        driver.connect().await.expect("connect");

        let point = ReadPoint {
            address: parse_mc_address("M50").expect("addr"),
            count: 3,
        };
        let samples = driver.read(&[point]).await.expect("read");
        assert_eq!(samples[0].value, vec![0x01, 0x01, 0x00], "bit samples normalized to 0/1");
        driver.disconnect().await.expect("disconnect");
    }

    /// QA Happy: 批量写 round-trip（服务器校验收到的写帧 golden bytes）。
    #[tokio::test]
    async fn mock_write_roundtrip() {
        let requests: Arc<Mutex<Vec<Vec<u8>>>> = Arc::default();
        let req_log = Arc::clone(&requests);
        let addr = spawn_mock_server(move |request| {
            req_log.lock().expect("req lock").push(request);
            build_ok_response(&[]) // 写响应仅含结束代码
        })
        .await;

        let mut driver = McDriver::new(test_config(addr));
        driver.connect().await.expect("connect");

        driver
            .write(&[WritePoint {
                address: parse_mc_address("D100").expect("addr"),
                value: vec![0x12, 0x34],
            }])
            .await
            .expect("write");

        let logged = requests.lock().expect("req lock");
        assert_eq!(logged.len(), 1, "single write frame");
        assert_eq!(
            logged[0],
            McDriver::build_request(CMD_BATCH_WRITE, SUBCMD_WORD, McDeviceCode::D, 100, 1, &[0x12, 0x34]),
            "server saw exact golden write frame"
        );
        driver.disconnect().await.expect("disconnect");
    }

    // ---- error path ----

    /// QA Error: 异常结束代码 → ProtocolError，且不推进退避（终端错误不重连）。
    #[tokio::test]
    async fn mock_error_end_code_maps_to_protocol_error() {
        let addr = spawn_mock_server(move |_request| {
            let mut frame = vec![0x00, 0x50, NETWORK_NO, PC_NO];
            frame.extend_from_slice(&MODULE_IO.to_le_bytes());
            frame.push(STATION_NO);
            frame.extend_from_slice(&2u16.to_le_bytes()); // 数据长 = 2（仅结束代码）
            frame.extend_from_slice(&0xC059u16.to_le_bytes()); // 异常结束代码
            frame
        })
        .await;

        let mut driver = McDriver::new(test_config(addr));
        driver.connect().await.expect("connect");

        let point = ReadPoint {
            address: parse_mc_address("D100").expect("addr"),
            count: 1,
        };
        let err = driver.read(&[point]).await.expect_err("must fail");
        assert!(matches!(err, DaemonError::ProtocolError(_)), "{err:?}");
        assert_eq!(err.error_code(), ERR_PROTOCOL);
        assert!(
            err.to_string().contains("C059"),
            "message contains end code: {err}"
        );
        assert_eq!(
            driver.reconnector().clone().next_delay(),
            Duration::from_millis(1),
            "terminal error must not advance backoff"
        );
        driver.disconnect().await.expect("disconnect");
    }

    /// QA Error: 不可达地址 connect → NetworkError，且推进退避。
    #[tokio::test]
    async fn unreachable_connect_returns_network_error() {
        let mut driver = McDriver::new(test_config("127.0.0.1:1".parse().expect("addr")));
        let err = driver.connect().await.expect_err("must fail");
        assert!(matches!(err, DaemonError::NetworkError(_)), "{err:?}");
        assert_eq!(err.error_code(), ERR_NETWORK);
        assert_eq!(
            driver.reconnector().clone().next_delay(),
            Duration::from_millis(2),
            "connect failure advances backoff"
        );
    }

    /// QA Error: 静默服务器（不响应）→ 请求超时归为 ProtocolError（重连重试后）。
    #[tokio::test]
    async fn silent_server_times_out_to_protocol_error() {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("local addr");
        tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    return;
                };
                // 保持连接打开但不响应（静默），直到客户端关闭。
                let _stream = stream;
                tokio::time::sleep(Duration::from_secs(30)).await;
            }
        });

        let mut driver = McDriver::new(McConfig {
            timeout: Duration::from_millis(100),
            ..test_config(addr)
        });
        driver.connect().await.expect("connect");

        let point = ReadPoint {
            address: parse_mc_address("D100").expect("addr"),
            count: 1,
        };
        let err = driver.read(&[point]).await.expect_err("must time out");
        assert!(matches!(err, DaemonError::ProtocolError(_)), "{err:?}");
        assert_eq!(err.error_code(), ERR_PROTOCOL);
        assert_eq!(
            driver.reconnector().clone().next_delay(),
            Duration::from_millis(2),
            "timeout advanced backoff (initial + reconnect failure)"
        );
    }

    // ---- 重连 ----

    /// QA: 连接丢失后按退避重连一次并重试成功（flaky 服务器首连接即断）。
    #[tokio::test]
    async fn reconnect_after_connection_loss() {
        let addr = spawn_flaky_server(move |_request| {
            build_ok_response(&[0xCA, 0xFE])
        })
        .await;

        let mut driver = McDriver::new(test_config(addr));
        driver.connect().await.expect("connect (conn1)");

        let point = ReadPoint {
            address: parse_mc_address("D100").expect("addr"),
            count: 1,
        };
        let samples = driver.read(&[point]).await.expect("read after reconnect");
        assert_eq!(samples[0].value, vec![0xCA, 0xFE], "retry succeeded on conn2");
        assert_eq!(
            driver.reconnector().clone().next_delay(),
            Duration::from_millis(2),
            "recovery reconnect must not reset backoff"
        );
        driver.disconnect().await.expect("disconnect");
    }
}

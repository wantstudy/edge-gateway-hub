//! task 11 收尾切片 — S7 [`Driver`] trait 适配层（薄壳）。
//!
//! 将会话层 [`S7Session`]（`s7.rs`，同步 API）挂接进 `driver/mod.rs` 的统一
//! [`Driver`] 契约，让 scheduler / pipeline 可与 Modbus 等驱动无差别调度：
//!
//! - **薄壳原则**：仅做类型转换（统一 [`PointAddress`] → S7 [`S7Address`]）与
//!   错误映射（[`S7Error`] → [`DaemonError`] 既有变体），**不复制**任何帧构造 /
//!   合并读 / PDU 拆分 / 重连业务逻辑——全部委托给 `S7Session` 既有实现。
//! - **错误映射**：`S7Error::NetworkError` → [`DaemonError::NetworkError`]
//!   （ERR_NETWORK=6000，对端关闭 / 连接失败 / 读写超时等传输层故障）；
//!   `BadFrame` / `BadAddress` / `BadParam` / `ReturnCode` →
//!   [`DaemonError::ProtocolError`]（ERR_PROTOCOL=1000，帧结构 / 地址 /
//!   参数越界 / PLC 拒绝等协议语义错误）。与 modbus 驱动的映射口径一致。
//! - **连接生命周期 / 断线重连（语义不回退声明）**：适配层**不做任何额外的
//!   重连或退避**，直接桥接 `S7Session` 内建的既有策略——
//!   `S7Session::read_points` / `write_points` 遇「对端关闭类」网络错误
//!   （peer closed / send 失败，见 `S7Session::is_connection_lost`）时自动
//!   **重连一次**并整体重试，二次失败上抛；**读超时不重连**（可能是 PLC 忙，
//!   重连反而放大故障）。该策略由会话层测试
//!   `s7::session_tests::mock_reconnect_after_peer_close_succeeds` /
//!   `mock_reconnect_second_failure_propagates` 固化，本层用
//!   [`S7Driver`] 重新走一遍对端关闭场景确认桥接后语义不变
//!   （见 `tests::dyn_driver_read_preserves_reconnect_semantics`）。
//! - **count 语义**：`S7Session::read_points` 每地址读取**一个元素**
//!   （位 / 字节 / 字 / 双字，见其合并逻辑 `count = size.width()`），
//!   故统一接口下 S7 驱动约定 `ReadPoint.count == 1`，其余值返回
//!   [`DaemonError::ProtocolError`]（与 modbus 驱动 `1..=125` 同为驱动自定义约束）。
//! - **阻塞桥接**：`S7Session` 为同步 IO（std TcpStream，内建 2s 超时），
//!   经 `tokio::task::spawn_blocking` 桥入 async [`Driver`] trait，
//!   不阻塞 tokio 运行时线程。
//!
//! 统一地址解析器（`PointAddressParser`）现支持 `DBn.DBX<off>.<bit>`、
//! `DBn.DBB/DBW/DBD<off>`（area 记 `B/W/D`）与 `I/Q` 位（`I0.1`）、MC 位
//! `M<off>`（bit_index 恒 0），本层逐一映射；MC 字区 `D<off>`（db=0）不属
//! S7，仍拒绝。批量写经 [`S7Session::write_points`] 透出会话层相邻合并能力
//! （相邻字节 → 单次 Write Var 报文）；Driver trait 本身已有批量写接口
//! `write(&[WritePoint])`，trait 级进一步扩展（如多 Item Write Var）留后续。

use async_trait::async_trait;
use tokio::task::spawn_blocking;

use crate::driver::s7::{
    derive_tsap, S7Address, S7Area, S7Error, S7Session, S7Size,
};
use crate::driver::{Driver, PointAddress, PointSample, ReadPoint, WritePoint};
use crate::error::{DaemonError, DaemonResult};

/// 三字节地址字段的偏移上限（与 `s7.rs::MAX_BYTE_OFFSET` 同值；
/// 该常量为 `s7.rs` 私有，适配层按解析器同规则显式校验，防止
/// `encode_item` 的 24 位地址截断）。
const S7_MAX_BYTE_OFFSET: u32 = 0x1F_FFFF;

/// 统一 [`PointAddress`] → S7 [`S7Address`] 转换（薄壳：纯类型映射）。
///
/// 支持范围（与统一解析器 `PointAddressParser` 对 S7 的产出对齐）：
/// - `area == None`（S7 DB 地址）：位访问（`bit == true`，即 `DBn.DBX<off>.<bit>`）；
/// - `area == Some('B' | 'W')`：DB 字节/字（`DBn.DBB/DBW<off>`，`db` 即 DB 号）；
/// - `area == Some('D')` 且 `db != 0`：DB 双字（`DBn.DBD<off>`）；`db == 0`
///   视为 MC 字区（与统一解析器的 MC `D` 编码区分，真实 PLC DB 号从 1 起）；
/// - `area == Some('I' | 'Q')`：过程映像位（`I<off>.<bit>` / `Q<off>.<bit>`）；
/// - `area == Some('M')`：位元件 `M<off>`（统一解析器固定 `bit_index = 0`）；
/// - 其余（Modbus 数字区、MC `D` 区）→ [`DaemonError::ProtocolError`]。
///
/// # Errors
/// 非位 DB / 未支持区 / DB 号超 u16 / 偏移超 24 位地址上限 / 位号 > 7。
fn point_to_s7_address(p: &PointAddress) -> DaemonResult<S7Address> {
    if p.bit_index > 7 {
        return Err(DaemonError::ProtocolError(format!(
            "s7 bit index must be 0-7, got {} for {p:?}",
            p.bit_index
        )));
    }
    if p.start > S7_MAX_BYTE_OFFSET {
        return Err(DaemonError::ProtocolError(format!(
            "s7 byte offset {} out of range (max {S7_MAX_BYTE_OFFSET}) for {p:?}",
            p.start
        )));
    }
    match p.area {
        // S7 DB 区位访问：DBn.DBX<off>.<bit>。
        None => {
            if !p.bit {
                return Err(DaemonError::ProtocolError(format!(
                    "s7 DB bit access required (DBn.DBX<off>.<bit>), got non-bit {p:?}"
                )));
            }
            let db = u16::try_from(p.db).map_err(|_| {
                DaemonError::ProtocolError(format!("s7 db number {} out of u16 range", p.db))
            })?;
            Ok(S7Address {
                area: S7Area::Db,
                db,
                byte_offset: p.start,
                bit_index: p.bit_index,
                size: S7Size::Bit,
            })
        }
        // DB 字节/字访问：DBn.DBB/DBW<off>（task 11 扩展）。
        Some('B') | Some('W') => {
            if p.bit {
                return Err(DaemonError::ProtocolError(format!(
                    "s7 DBB/DBW address must be byte/word (non-bit) access, got {p:?}"
                )));
            }
            let db = u16::try_from(p.db).map_err(|_| {
                DaemonError::ProtocolError(format!("s7 db number {} out of u16 range", p.db))
            })?;
            let size = if p.area == Some('B') {
                S7Size::Byte
            } else {
                S7Size::Word
            };
            Ok(S7Address {
                area: S7Area::Db,
                db,
                byte_offset: p.start,
                bit_index: 0,
                size,
            })
        }
        // DB 双字访问：DBn.DBD<off>（db=0 视为 MC 字区，交由兜底分支拒绝）。
        Some('D') if p.db != 0 => {
            if p.bit {
                return Err(DaemonError::ProtocolError(format!(
                    "s7 DBD address must be dword (non-bit) access, got {p:?}"
                )));
            }
            let db = u16::try_from(p.db).map_err(|_| {
                DaemonError::ProtocolError(format!("s7 db number {} out of u16 range", p.db))
            })?;
            Ok(S7Address {
                area: S7Area::Db,
                db,
                byte_offset: p.start,
                bit_index: 0,
                size: S7Size::DWord,
            })
        }
        // 过程映像位：I<off>.<bit> / Q<off>.<bit>（task 11 扩展）。
        Some('I') | Some('Q') => {
            if !p.bit {
                return Err(DaemonError::ProtocolError(format!(
                    "s7 I/Q area supports bit access only (e.g. I0.1), got {p:?}"
                )));
            }
            Ok(S7Address {
                area: if p.area == Some('I') {
                    S7Area::I
                } else {
                    S7Area::Q
                },
                db: 0,
                byte_offset: p.start,
                bit_index: p.bit_index,
                size: S7Size::Bit,
            })
        }
        // MC 位元件 M<off>（bit_index 恒 0，来自统一解析器）。
        Some('M') => {
            if !p.bit {
                return Err(DaemonError::ProtocolError(format!(
                    "s7 M area address must be bit access, got {p:?}"
                )));
            }
            Ok(S7Address {
                area: S7Area::M,
                db: 0,
                byte_offset: p.start,
                bit_index: p.bit_index,
                size: S7Size::Bit,
            })
        }
        Some(other) => Err(DaemonError::ProtocolError(format!(
            "s7 driver does not support address area {other:?} (expected DB…/B/W/D, I/Q bit or M… bit), got {p:?}"
        ))),
    }
}

/// [`S7Error`] → [`DaemonError`] 错误映射（薄壳：仅变体归并，不改写消息语义）。
///
/// - `NetworkError`（传输层故障）→ [`DaemonError::NetworkError`]（码 6000）；
/// - 其余（帧 / 地址 / 参数 / PLC ReturnCode 等协议语义）→
///   [`DaemonError::ProtocolError`]（码 1000），消息保留 `S7Error` 的
///   Display 全文（含原始输入 / ReturnCode 码与说明）。
fn map_s7_error(err: S7Error) -> DaemonError {
    match err {
        S7Error::NetworkError(msg) => DaemonError::NetworkError(msg),
        other => DaemonError::ProtocolError(other.to_string()),
    }
}

/// S7 驱动（统一 [`Driver`] 契约适配，包装 [`S7Session`]）。
///
/// 连接生命周期：`connect` 为「关闭旧会话 + 全新三步握手」（幂等可重入）；
/// `read` / `write` 惰性连接（未连接时先 connect）。已连接后的断线恢复
/// 完全由 `S7Session` 内建策略处理（见模块文档「语义不回退声明」）。
pub struct S7Driver {
    /// 对端地址（`ip:port`；重连由会话层按此原样复用）。
    addr: String,
    /// 远程 TSAP（[`derive_tsap`] 产物）。
    tsap: [u8; 2],
    /// 已握手会话（`None` = 未连接；连接独占由 `&mut self` 保证）。
    session: Option<S7Session>,
}

impl S7Driver {
    /// 创建驱动（未连接；首次 `connect` / `read` / `write` 时握手）。
    pub fn new(addr: String, tsap: [u8; 2]) -> Self {
        Self {
            addr,
            tsap,
            session: None,
        }
    }

    /// 按机架号 / 槽位号创建驱动（TSAP 经 [`derive_tsap`] 推导）。
    pub fn with_rack_slot(addr: String, rack: u8, slot: u8) -> Self {
        Self::new(addr, derive_tsap(rack, slot))
    }

    /// 惰性连接：未连接时执行三步握手（阻塞 IO 经 `spawn_blocking` 桥接）。
    async fn ensure_connected(&mut self) -> DaemonResult<()> {
        if self.session.is_some() {
            return Ok(());
        }
        let addr = self.addr.clone();
        let tsap = self.tsap;
        let session = spawn_blocking(move || S7Session::connect(&addr, tsap))
            .await
            .map_err(|e| DaemonError::NetworkError(format!("s7 connect task join failed: {e}")))?
            .map_err(map_s7_error)?;
        self.session = Some(session);
        Ok(())
    }
}

#[async_trait]
impl Driver for S7Driver {
    /// 建立连接：关闭旧会话（若有）后全新三步握手（幂等可重入）。
    async fn connect(&mut self) -> DaemonResult<()> {
        if let Some(mut old) = self.session.take() {
            old.shutdown();
        }
        self.ensure_connected().await
    }

    /// 批量读取点位：先全量做类型转换与参数校验（fail-fast，不产出部分结果），
    /// 再委托 [`S7Session::read_points`]（含合并读 / PDU 拆分 / 对端关闭
    /// 重连一次的既有策略）。返回与请求等长、按下标一一对应的采样。
    async fn read(&mut self, points: &[ReadPoint]) -> DaemonResult<Vec<PointSample>> {
        if points.is_empty() {
            return Ok(Vec::new());
        }
        let mut items: Vec<(PointAddress, S7Address)> = Vec::with_capacity(points.len());
        for p in points {
            let s7_addr = point_to_s7_address(&p.address)?;
            if p.count != 1 {
                return Err(DaemonError::ProtocolError(format!(
                    "s7 read requires count == 1 (one element per address: bit/byte/word/dword), got count {} for {p:?}",
                    p.count
                )));
            }
            items.push((p.address.clone(), s7_addr));
        }

        self.ensure_connected().await?;
        let mut session = self
            .session
            .take()
            .ok_or_else(|| DaemonError::NetworkError("s7 driver: session missing".to_string()))?;
        let s7_addrs: Vec<S7Address> = items.iter().map(|(_, a)| a.clone()).collect();
        let (session, result) = spawn_blocking(move || {
            let result = session.read_points(&s7_addrs);
            (session, result)
        })
        .await
        .map_err(|e| DaemonError::NetworkError(format!("s7 read task join failed: {e}")))?;
        self.session = Some(session);

        let values = result.map_err(map_s7_error)?;
        if values.len() != items.len() {
            // 会话层契约保证等长；防御性检查，不产出部分结果。
            return Err(DaemonError::ProtocolError(format!(
                "s7 read returned {} samples for {} points",
                values.len(),
                items.len()
            )));
        }
        Ok(items
            .into_iter()
            .zip(values)
            .map(|((address, _), value)| PointSample { address, value })
            .collect())
    }

    /// 批量写入点位：先全量做类型转换与参数校验（fail-fast，不产出部分结果），
    /// 再委托 [`S7Session::write_points`]（相邻非位写入合并为单 Item Write Var
    /// 报文 + 按 PDU 预算切分 + 对端关闭重连一次的既有策略，由会话层保持）。
    /// 写入宽度约束：位地址恰 1 字节且取值 0x00 / 0x01（`build_write_var` 的
    /// Bit 编码约定）；字节/字/双字地址载荷须与尺寸宽度严格一致（1/2/4 字节）。
    async fn write(&mut self, points: &[WritePoint]) -> DaemonResult<()> {
        if points.is_empty() {
            return Ok(());
        }
        let mut reqs: Vec<(S7Address, Vec<u8>)> = Vec::with_capacity(points.len());
        for p in points {
            let s7_addr = point_to_s7_address(&p.address)?;
            match s7_addr.size {
                S7Size::Bit => {
                    if p.value.len() != 1 || (p.value[0] != 0x00 && p.value[0] != 0x01) {
                        return Err(DaemonError::ProtocolError(format!(
                            "s7 bit write requires exactly 1 byte 0x00/0x01, got {} byte(s) {:?} for {p:?}",
                            p.value.len(),
                            p.value
                        )));
                    }
                }
                size => {
                    let width = usize::from(size.width());
                    if p.value.len() != width {
                        return Err(DaemonError::ProtocolError(format!(
                            "s7 {size:?} write requires exactly {width} byte(s), got {} for {p:?}",
                            p.value.len()
                        )));
                    }
                }
            }
            reqs.push((s7_addr, p.value.clone()));
        }

        self.ensure_connected().await?;
        let mut session = self
            .session
            .take()
            .ok_or_else(|| DaemonError::NetworkError("s7 driver: session missing".to_string()))?;
        let (session, result) = spawn_blocking(move || {
            let result = session.write_points(&reqs);
            (session, result)
        })
        .await
        .map_err(|e| DaemonError::NetworkError(format!("s7 write task join failed: {e}")))?;
        self.session = Some(session);
        result.map_err(map_s7_error)
    }

    /// 断开连接并释放会话（幂等；未连接时为 no-op）。
    /// 关闭失败无恢复路径，错误忽略（与 [`S7Session::shutdown`] 语义一致）。
    async fn disconnect(&mut self) -> DaemonResult<()> {
        if let Some(mut session) = self.session.take() {
            session.shutdown();
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::driver::PointAddressParser;
    use crate::error::{ERR_NETWORK, ERR_PROTOCOL};
    use std::io::{Read, Write as IoWrite};
    use std::net::{SocketAddr, TcpListener};
    use std::sync::{Arc, Mutex};
    use std::thread;
    use std::time::Duration;

    use crate::driver::s7::{build_tpkt, parse_s7_address, S7_REQUESTED_PDU};

    /// 确认会话层地址解析与统一解析器转换结果一致（防两套语义漂移）。
    fn assert_same_as_session_parser(raw: &str) {
        let unified =
            point_to_s7_address(&PointAddressParser::parse(raw).unwrap())
                .unwrap_or_else(|e| panic!("unified conversion must accept {raw:?}: {e:?}"));
        let session = parse_s7_address(raw)
            .unwrap_or_else(|e| panic!("session parser must accept {raw:?}: {e:?}"));
        assert_eq!(unified, session, "unified/session parser mismatch for {raw:?}");
    }

    // ---- 类型转换：往返一致性 ----

    /// QA Happy: 统一地址 → S7Address 与会话层 parse_s7_address 逐字段一致
    ///（DBX 位与 M 位元件，含非平凡取值）。
    ///
    /// 注：统一解析器接受裸 `M100`（等价 `M100.0` 位访问，bit_index 恒 0），
    /// 而会话层解析器要求位/字节形式显式区分（`M100.0` 或 `MB100`），故裸 M
    /// 形式单独对照 `M100.0`。
    #[test]
    fn conversion_roundtrip_matches_session_parser() {
        for raw in ["DB1.DBX0.0", "DB12.DBX34.7", "DB1.DBX2097151.7"] {
            assert_same_as_session_parser(raw);
        }
        // 裸 M 形式（统一解析器）≡ 会话层 <byte>.<bit> 位形式。
        let bare_m = point_to_s7_address(&PointAddressParser::parse("M100").unwrap())
            .expect("valid");
        let explicit_m =
            parse_s7_address("M100.0").expect("valid session-side equivalent");
        assert_eq!(bare_m, explicit_m, "bare M100 must equal M100.0");
        let a = point_to_s7_address(&PointAddressParser::parse("DB12.DBX34.7").unwrap())
            .expect("valid");
        assert_eq!((a.db, a.byte_offset, a.bit_index), (12, 34, 7));
        assert_eq!(a.area, S7Area::Db);
        assert_eq!(a.size, S7Size::Bit);
    }

    /// QA Error: 转换拒绝分支全覆盖——非位 DB、MC 字区、Modbus 区、
    /// DB 号超 u16、偏移超 24 位上限、位号 > 7，全部收敛为 ProtocolError。
    #[test]
    fn conversion_rejects_unsupported_addresses() {
        let cases = [
            PointAddress { db: 1, area: None, start: 0, bit: false, bit_index: 0 }, // DB 非位访问
            PointAddress { db: 0, area: Some('D'), start: 100, bit: false, bit_index: 0 }, // MC 字区
            PointAddress { db: 0, area: Some('4'), start: 1, bit: false, bit_index: 0 },   // Modbus 区
            PointAddress { db: u32::from(u16::MAX) + 1, area: None, start: 0, bit: true, bit_index: 0 }, // DB 超 u16
            PointAddress { db: 1, area: None, start: S7_MAX_BYTE_OFFSET + 1, bit: true, bit_index: 0 },  // 偏移越界
            PointAddress { db: 1, area: Some('M'), start: 5, bit: true, bit_index: 8 },  // 位号 > 7
            PointAddress { db: 1, area: Some('M'), start: 5, bit: false, bit_index: 0 }, // M 非位
        ];
        for p in &cases {
            let err = point_to_s7_address(p).expect_err("must reject");
            assert!(
                matches!(err, DaemonError::ProtocolError(_)),
                "for {p:?}: {err:?}"
            );
            assert_eq!(err.error_code(), ERR_PROTOCOL, "for {p:?}");
        }
    }

    // ---- 类型转换：DBB/DBW/DBD 与 I/Q 位（task 11 扩展） ----

    /// QA Happy: 统一地址 → S7Address 与会话层 parse_s7_address 逐字段一致
    ///（DBB/DBW/DBD 字节/字/双字，含三字节地址上限非平凡取值）。
    #[test]
    fn conversion_dbb_dbw_dbd_matches_session_parser() {
        for raw in ["DB2.DBB10", "DB2.DBW20", "DB2.DBD40", "DB1.DBD2097151"] {
            assert_same_as_session_parser(raw);
        }
        let w = point_to_s7_address(&PointAddressParser::parse("DB2.DBW20").unwrap())
            .expect("valid");
        assert_eq!(w.size, S7Size::Word);
        assert_eq!((w.db, w.byte_offset), (2, 20));
    }

    /// QA Happy: I/Q 位转换 → S7Area::I/Q + Bit 尺寸 + 真实位号；与会话层
    /// 解析器逐字段一致；MC 字区（db=0）仍拒绝（既有口径不回退）。
    #[test]
    fn conversion_iq_bit_addresses() {
        let i = point_to_s7_address(&PointAddressParser::parse("I0.1").unwrap()).expect("valid");
        assert_eq!(
            i,
            parse_s7_address("I0.1").expect("session parser"),
            "unified/session parser mismatch"
        );
        assert_eq!((i.area, i.byte_offset, i.bit_index, i.size),
                   (S7Area::I, 0, 1, S7Size::Bit));
        let q = point_to_s7_address(&PointAddressParser::parse("Q0.3").unwrap()).expect("valid");
        assert_eq!(q, parse_s7_address("Q0.3").expect("session parser"));
        assert_eq!(q.area, S7Area::Q);
        // MC 字区（db=0）不属 S7：仍收敛为 ProtocolError。
        let mc_d = PointAddress { db: 0, area: Some('D'), start: 100, bit: false, bit_index: 0 };
        let err = point_to_s7_address(&mc_d).expect_err("MC D must be rejected");
        assert_eq!(err.error_code(), ERR_PROTOCOL);
    }

    // ---- 错误映射 ----

    /// QA: S7Error 各分支 → DaemonError 既有变体（码域 1000 / 6000）。
    #[test]
    fn s7_error_mapping_all_branches() {
        // 协议语义类 → ProtocolError（码 1000）。
        let protocol_cases: Vec<(S7Error, &str)> = vec![
            (S7Error::BadFrame("truncated".to_string()), "s7 bad frame"),
            (S7Error::BadAddress("DB1.DBZ0".to_string()), "s7 bad address"),
            (S7Error::BadParam("too many items".to_string()), "s7 bad param"),
            (
                S7Error::ReturnCode { code: 0x05, detail: "address out of range（地址越界）" },
                "return code 0x05",
            ),
        ];
        for (err, expect_msg) in protocol_cases {
            let mapped = map_s7_error(err);
            assert!(
                matches!(mapped, DaemonError::ProtocolError(_)),
                "must be ProtocolError: {mapped:?}"
            );
            assert_eq!(mapped.error_code(), ERR_PROTOCOL);
            assert!(mapped.to_string().contains(expect_msg), "{mapped}");
        }
        // 网络类 → NetworkError（码 6000），消息透传。
        let mapped = map_s7_error(S7Error::NetworkError("peer closed connection".to_string()));
        assert!(matches!(mapped, DaemonError::NetworkError(_)), "{mapped:?}");
        assert_eq!(mapped.error_code(), ERR_NETWORK);
        assert!(mapped.to_string().contains("peer closed connection"), "{mapped}");
    }

    // ---- mock S7 服务器（与会话层 session_tests 同构；hold=false 用完即关）----

    /// mock 脚本类型：每连接一组应答（组内多帧拼接为一次 write，构造粘包）。
    type MockScript = Vec<Vec<Vec<u8>>>;

    /// 精确读取 `buf.len()` 字节（短读循环；EOF 报错）。
    fn read_exact_mock(sock: &mut std::net::TcpStream, buf: &mut [u8]) -> std::io::Result<()> {
        let mut filled = 0;
        while filled < buf.len() {
            let n = sock.read(&mut buf[filled..])?;
            if n == 0 {
                return Err(std::io::ErrorKind::UnexpectedEof.into());
            }
            filled += n;
        }
        Ok(())
    }

    /// 多连接记录型 mock：每个 accept 消耗一份脚本，脚本用尽即关闭该连接
    ///（模拟对端关闭，供重连用例）；收到的完整 TPKT 请求帧按序记录。
    fn spawn_mock_multi(scripts: Vec<MockScript>) -> (SocketAddr, Arc<Mutex<Vec<Vec<u8>>>>) {
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
                    let mut head = [0u8; 4];
                    if read_exact_mock(&mut sock, &mut head).is_err() {
                        return;
                    }
                    let total = u16::from_be_bytes([head[2], head[3]]) as usize;
                    if total < 4 {
                        return;
                    }
                    let mut body = vec![0u8; total - 4];
                    if read_exact_mock(&mut sock, &mut body).is_err() {
                        return;
                    }
                    let mut frame = head.to_vec();
                    frame.extend_from_slice(&body);
                    rec.lock().expect("mock record lock").push(frame);
                    let mut glued: Vec<u8> = Vec::new();
                    for resp in group {
                        glued.extend_from_slice(&resp);
                    }
                    if !glued.is_empty() && sock.write_all(&glued).is_err() {
                        return;
                    }
                }
                // 脚本用尽即主动关闭（模拟对端关闭）。
                drop(sock);
            }
        });
        (addr, received)
    }

    /// hex 字符串 → 字节向量（测试辅助）。
    fn hex(s: &str) -> Vec<u8> {
        s.split(' ')
            .filter(|t| !t.is_empty())
            .map(|t| u8::from_str_radix(t, 16).expect("valid hex byte"))
            .collect()
    }

    /// 测试用 Setup ACK（协商 PDU = 480 = 0x01E0，错误类/码为 0）。
    fn setup_ack_frame() -> Vec<u8> {
        hex("32 03 00 00 00 00 00 08 00 00 00 00 F0 00 00 01 00 01 01 E0")
    }

    /// 测试用 COTP CC（LI=6，类型 0xD0）。
    fn cotp_cc_frame() -> Vec<u8> {
        hex("06 D0 00 01 00 00 00")
    }

    /// 标准握手脚本：CR→CC、Setup→ACK（两组应答）。
    fn handshake_script() -> MockScript {
        vec![
            vec![build_tpkt(&cotp_cc_frame()).expect("tpkt")],
            vec![build_tpkt(&setup_ack_frame()).expect("tpkt")],
        ]
    }

    /// 多 Item 读 ACK：每项 `(transport, units, data)` 按序拼接数据区。
    fn read_ack_frame_items(items: &[(u8, u16, Vec<u8>)]) -> Vec<u8> {
        let mut ack = hex("32 03 00 00 00 00 00 02 00 00 00 00 04 01");
        ack[13] = items.len() as u8;
        let data_len: u16 = items.iter().map(|(_, _, d)| (4 + d.len()) as u16).sum();
        ack[8..10].copy_from_slice(&data_len.to_be_bytes());
        for (transport, units, data) in items {
            ack.extend_from_slice(&[0xFF, *transport]);
            ack.extend_from_slice(&units.to_be_bytes());
            ack.extend_from_slice(data);
        }
        ack
    }

    /// 写 ACK（单 Item 返回码 OK）。
    fn write_ack_frame() -> Vec<u8> {
        hex("32 03 00 00 00 00 00 02 00 01 00 00 05 01 FF")
    }

    // ---- Driver trait 端到端（mock 服务器）----

    /// QA Happy: 经 `Box<dyn Driver>` 对 mock 服务器跑一轮 read_points——
    /// 惰性连接 + 统一接口读取两点位（DBX 位 / M 位），采样与请求等长同序。
    #[tokio::test]
    async fn dyn_driver_read_mock_end_to_end() {
        let ack = read_ack_frame_items(&[
            (0x01, 1, vec![0xAB]), // DB1.DBX0.0 → 1 位 → 1 字节
            (0x01, 1, vec![0x01]), // M100.0    → 1 位 → 1 字节
        ]);
        let mut script = handshake_script();
        script.push(vec![build_tpkt(&ack).expect("tpkt")]);
        let (addr, _rec) = spawn_mock_multi(vec![script]);

        let mut driver: Box<dyn Driver> = Box::new(S7Driver::with_rack_slot(
            addr.to_string(),
            0,
            0,
        ));
        let p1 = PointAddressParser::parse("DB1.DBX0.0").expect("addr");
        let p2 = PointAddressParser::parse("M100").expect("addr");
        let points = [
            ReadPoint { address: p1.clone(), count: 1 },
            ReadPoint { address: p2.clone(), count: 1 },
        ];
        let samples = driver.read(&points).await.expect("read");
        assert_eq!(samples.len(), 2, "1:1 with request");
        assert_eq!(samples[0].address, p1, "echoes request address");
        assert_eq!(samples[0].value, vec![0xAB], "DBX bit sample");
        assert_eq!(samples[1].address, p2);
        assert_eq!(samples[1].value, vec![0x01], "M bit sample");
        driver.disconnect().await.expect("disconnect");
    }

    /// QA Happy: 经 Driver trait 写点位——位写 1 字节 0x01，mock 校验
    /// Write Var 帧的功能码与数据字段。
    #[tokio::test]
    async fn dyn_driver_write_mock_end_to_end() {
        let mut script = handshake_script();
        script.push(vec![build_tpkt(&write_ack_frame()).expect("tpkt")]);
        let (addr, received) = spawn_mock_multi(vec![script]);

        let mut driver: Box<dyn Driver> = Box::new(S7Driver::new(addr.to_string(), derive_tsap(0, 0)));
        driver
            .write(&[WritePoint {
                address: PointAddressParser::parse("DB1.DBX0.0").expect("addr"),
                value: vec![0x01],
            }])
            .await
            .expect("write");
        driver.disconnect().await.expect("disconnect");

        let rec = received.lock().expect("lock");
        assert_eq!(rec.len(), 3, "CR + Setup + Write Var");
        let tpkt = crate::driver::s7::parse_tpkt(&rec[2]).expect("tpkt");
        assert_eq!(tpkt[13], 0x05, "function = Write Var");
        // COTP DT(3) + S7 头(10) + 功能码/项数(2) + Item(12) + 数据头(4) = 31。
        assert_eq!(tpkt[31], 0x01, "bit write data 0x01");
        // Bit 写长度单位 = 字节数 × 8 = 8 位。
        assert_eq!(&tpkt[29..31], &8u16.to_be_bytes(), "units = 8 bits");
    }

    /// QA: 断线重连语义经适配层不回退——连接 1 握手后被 mock 关闭，
    /// 统一接口 read 触发会话层既有「peer closed 重连一次」策略，
    /// 在连接 2 上完成读取（读超时不重连的策略由会话层测试固化，此处桥接不改写）。
    #[tokio::test]
    async fn dyn_driver_read_preserves_reconnect_semantics() {
        let mut script2 = handshake_script();
        script2.push(vec![build_tpkt(&read_ack_frame_items(&[(0x01, 1, vec![0x12])])).expect("tpkt")]);
        // 连接 1：仅握手（随后被 mock 关闭）；连接 2：握手 + 正常应答。
        let (addr, _rec) = spawn_mock_multi(vec![handshake_script(), script2]);

        let mut driver: Box<dyn Driver> = Box::new(S7Driver::new(addr.to_string(), derive_tsap(0, 0)));
        driver.connect().await.expect("connect");
        let samples = driver
            .read(&[ReadPoint {
                address: PointAddressParser::parse("DB1.DBX0.0").expect("addr"),
                count: 1,
            }])
            .await
            .expect("read after auto-reconnect");
        assert_eq!(samples[0].value, vec![0x12], "reconnected and read ok");
        driver.disconnect().await.expect("disconnect");
    }

    /// QA Error: 连接拒绝（无人监听端口）经统一接口收敛为 NetworkError（码 6000）。
    #[tokio::test]
    async fn driver_connect_refused_is_network_error() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr");
        drop(listener);
        let mut driver = S7Driver::new(addr.to_string(), derive_tsap(0, 0));
        let err = driver.connect().await.expect_err("must be refused");
        assert!(matches!(err, DaemonError::NetworkError(_)), "got {err:?}");
        assert_eq!(err.error_code(), ERR_NETWORK);
    }

    /// QA Error: count ≠ 1 在发起 IO 前被拒（S7 会话每地址读一个元素，
    /// 见模块文档「count 语义」），错误码 1000。
    #[tokio::test]
    async fn driver_read_rejects_count_over_one() {
        let mut driver = S7Driver::new("127.0.0.1:1".to_string(), derive_tsap(0, 0));
        let err = driver
            .read(&[ReadPoint {
                address: PointAddressParser::parse("DB1.DBX0.0").expect("addr"),
                count: 2,
            }])
            .await
            .expect_err("must reject count != 1");
        assert!(matches!(err, DaemonError::ProtocolError(_)), "got {err:?}");
        assert_eq!(err.error_code(), ERR_PROTOCOL);
        assert!(err.to_string().contains("count == 1"), "{err}");
    }

    /// QA Happy: 经 Driver trait 对 mock 服务器端到端读 I/Q 位与 DBW 字
    ///（mock 过程映像覆盖 I/Q/DB）；DBW 大端 0x1234 断言沿用会话层既有口径；
    /// 请求帧 Item 的 area 字节逐一为 I=0x81 / Q=0x82 / DB=0x84。
    #[tokio::test]
    async fn dyn_driver_read_iq_dbw_mock_end_to_end() {
        let ack = read_ack_frame_items(&[
            (0x01, 1, vec![0x01]),       // I0.1 → 1 位 → 1 字节
            (0x01, 1, vec![0x00]),       // Q0.3 → 1 位 → 1 字节
            (0x04, 2, vec![0x12, 0x34]), // DB1.DBW0 → 大端 0x1234
        ]);
        let mut script = handshake_script();
        script.push(vec![build_tpkt(&ack).expect("tpkt")]);
        let (addr, received) = spawn_mock_multi(vec![script]);

        let mut driver: Box<dyn Driver> = Box::new(S7Driver::with_rack_slot(addr.to_string(), 0, 0));
        let pi = PointAddressParser::parse("I0.1").expect("addr");
        let pq = PointAddressParser::parse("Q0.3").expect("addr");
        let pw = PointAddressParser::parse("DB1.DBW0").expect("addr");
        let points = [
            ReadPoint { address: pi.clone(), count: 1 },
            ReadPoint { address: pq.clone(), count: 1 },
            ReadPoint { address: pw.clone(), count: 1 },
        ];
        let samples = driver.read(&points).await.expect("read");
        assert_eq!(samples.len(), 3, "1:1 with request");
        assert_eq!(samples[0].address, pi);
        assert_eq!(samples[0].value, vec![0x01], "I bit sample");
        assert_eq!(samples[1].address, pq);
        assert_eq!(samples[1].value, vec![0x00], "Q bit sample");
        assert_eq!(
            crate::driver::s7::read_u16_be(&samples[2].value).expect("u16"),
            0x1234,
            "DBW big-endian word"
        );
        driver.disconnect().await.expect("disconnect");

        // 请求帧 area 字节断言：Item 起始于 payload[15]，area 在 Item+8。
        let rec = received.lock().expect("lock");
        let payload = crate::driver::s7::parse_tpkt(&rec[2]).expect("tpkt");
        assert_eq!(payload[23], 0x81, "item#1 area = I (0x81)");
        assert_eq!(payload[35], 0x82, "item#2 area = Q (0x82)");
        assert_eq!(payload[47], 0x84, "item#3 area = DB (0x84)");
    }

    /// QA Happy: 经 Driver trait 写 I/Q 位（两帧，异区不合并），mock 校验
    /// Write Var 帧的 area 字节与数据字段。
    #[tokio::test]
    async fn dyn_driver_write_iq_mock_end_to_end() {
        let mut script = handshake_script();
        script.push(vec![build_tpkt(&write_ack_frame()).expect("tpkt")]);
        script.push(vec![build_tpkt(&write_ack_frame()).expect("tpkt")]);
        let (addr, received) = spawn_mock_multi(vec![script]);

        let mut driver: Box<dyn Driver> = Box::new(S7Driver::new(addr.to_string(), derive_tsap(0, 0)));
        driver
            .write(&[
                WritePoint { address: PointAddressParser::parse("Q0.3").expect("addr"), value: vec![0x01] },
                WritePoint { address: PointAddressParser::parse("I0.1").expect("addr"), value: vec![0x00] },
            ])
            .await
            .expect("write");
        driver.disconnect().await.expect("disconnect");

        let rec = received.lock().expect("lock");
        assert_eq!(rec.len(), 4, "CR + Setup + Write(Q) + Write(I)");
        let pq = crate::driver::s7::parse_tpkt(&rec[2]).expect("tpkt");
        assert_eq!(pq[13], 0x05, "function = Write Var");
        assert_eq!(pq[23], 0x82, "area = Q (0x82)");
        assert_eq!(pq[31], 0x01, "Q bit write data 0x01");
        let pi = crate::driver::s7::parse_tpkt(&rec[3]).expect("tpkt");
        assert_eq!(pi[23], 0x81, "area = I (0x81)");
        assert_eq!(pi[31], 0x00, "I bit write data 0x00");
    }

    /// QA 合并写: 相邻 3 字节（DBB0/1/2）合并为单 Item 单帧 Write Var，
    /// 有间隙的 DBB10 独立成帧——报文数断言 CR + Setup + 2 帧。
    #[tokio::test]
    async fn dyn_driver_write_merges_adjacent_bytes_single_frame() {
        let mut script = handshake_script();
        script.push(vec![build_tpkt(&write_ack_frame()).expect("tpkt")]);
        script.push(vec![build_tpkt(&write_ack_frame()).expect("tpkt")]);
        let (addr, received) = spawn_mock_multi(vec![script]);

        let mut driver: Box<dyn Driver> = Box::new(S7Driver::new(addr.to_string(), derive_tsap(0, 0)));
        let parse = |raw: &str| PointAddressParser::parse(raw).expect("addr");
        driver
            .write(&[
                WritePoint { address: parse("DB1.DBB0"), value: vec![0xAA] },
                WritePoint { address: parse("DB1.DBB1"), value: vec![0xBB] },
                WritePoint { address: parse("DB1.DBB2"), value: vec![0xCC] },
                WritePoint { address: parse("DB1.DBB10"), value: vec![0xDD] }, // 间隙 → 第二帧
            ])
            .await
            .expect("write");
        driver.disconnect().await.expect("disconnect");

        let rec = received.lock().expect("lock");
        assert_eq!(rec.len(), 4, "CR + Setup + merged Write#1 + Write#2(DBB10)");
        let p1 = crate::driver::s7::parse_tpkt(&rec[2]).expect("tpkt");
        assert_eq!(p1[14], 1, "3 adjacent bytes merged into a single item");
        assert_eq!(&p1[29..31], &3u16.to_be_bytes(), "units = 3 bytes");
        assert_eq!(&p1[31..34], &[0xAA, 0xBB, 0xCC], "merged payload in order");
        let p2 = crate::driver::s7::parse_tpkt(&rec[3]).expect("tpkt");
        assert_eq!(&p2[29..31], &1u16.to_be_bytes(), "gap point stays its own frame");
        assert_eq!(p2[31], 0xDD);
    }

    /// QA Error: 字节/字/双字写入宽度不符在发起 IO 前被拒（错误码 1000）。
    #[tokio::test]
    async fn driver_write_rejects_wrong_value_width() {
        let mut driver = S7Driver::new("127.0.0.1:1".to_string(), derive_tsap(0, 0));
        let cases = [
            ("DB1.DBW0", vec![0x12u8]),            // 字写 1 字节（须 2）
            ("DB1.DBB0", vec![0x01, 0x02]),        // 字节写 2 字节（须 1）
            ("DB1.DBD0", vec![0x01, 0x02, 0x03]),  // 双字写 3 字节（须 4）
        ];
        for (raw, value) in cases {
            let err = driver
                .write(&[WritePoint { address: PointAddressParser::parse(raw).expect("addr"), value }])
                .await
                .expect_err("must reject wrong width");
            assert!(
                matches!(err, DaemonError::ProtocolError(_)),
                "for {raw:?}: {err:?}"
            );
            assert_eq!(err.error_code(), ERR_PROTOCOL, "for {raw:?}");
            assert!(err.to_string().contains("byte(s)"), "for {raw:?}: {err}");
        }
    }

    /// QA: 常量健全性——请求 PDU 与会话层默认一致（防止适配层与协议层漂移）。
    #[test]
    fn requested_pdu_matches_session_constant() {
        assert_eq!(S7_REQUESTED_PDU, 480);
    }
}

//! Modbus TCP / RTU-over-TCP 驱动（plan task 9）。
//!
//! 基于 `tokio-modbus 0.17`（纯 Rust 栈）适配 [`Driver`] trait：
//! - 帧格式：Modbus TCP（MBAP）与 Modbus RTU（CRC16）经 TCP 传输（RTU-over-TCP）；
//!   真串口（tokio-serial）不在本任务范围，留待后续 wave。
//! - 地址：`4xxxx` 保持寄存器（FC03）/ `3xxxx` 输入寄存器（FC04），1 基寄存器号，
//!   协议地址 = 寄存器号 - 1；线圈 / 离散输入（FC01/FC02）不在本任务范围。
//! - 错误映射：拨号失败 / 传输中断 → [`DaemonError::NetworkError`]（ERR_NETWORK=6000）；
//!   请求超时 / 异常响应 / 帧校验失败 → [`DaemonError::ProtocolError`]（ERR_PROTOCOL=1000）。
//! - 断线重连：请求遇连接丢失（超时 / 传输错误）时按 [`Reconnector`] 指数退避等待，
//!   重连一次并重试该请求；重试仍失败则返回错误。`connect()` 成功会重置退避，
//!   恢复重连不重置（保持退避推进状态）。

use std::net::SocketAddr;
use std::time::Duration;

use async_trait::async_trait;
use tokio::net::TcpStream;
use tokio::time::timeout;
use tokio_modbus::client::{rtu, tcp, Client, Context, Reader, Writer};
use tokio_modbus::{Error, Slave};

use crate::driver::{Driver, PointSample, ReadPoint, Reconnector, WritePoint};
use crate::error::{DaemonError, DaemonResult};

/// Modbus 帧格式。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModbusFraming {
    /// Modbus TCP（MBAP 头；从站号默认 0xFF 广播）。
    Tcp,
    /// Modbus RTU 帧（CRC16）经 TCP 传输（RTU-over-TCP）。
    Rtu,
}

/// Modbus 驱动配置。
#[derive(Debug, Clone)]
pub struct ModbusConfig {
    /// 对端地址（TCP / RTU-over-TCP 均走 TCP 传输）。
    pub addr: SocketAddr,
    /// 从站号（TCP 默认 0xFF；RTU 1-247）。
    pub slave: u8,
    /// 帧格式。
    pub framing: ModbusFraming,
    /// 单次请求超时（超时按协议错误处理并触发重连）。
    pub timeout: Duration,
    /// 断线重连退避（`connect()` 成功重置；恢复重连不重置）。
    pub reconnector: Reconnector,
}

impl Default for ModbusConfig {
    fn default() -> Self {
        Self {
            addr: SocketAddr::from(([127, 0, 0, 1], 502)),
            slave: 0xFF,
            framing: ModbusFraming::Tcp,
            timeout: Duration::from_secs(3),
            reconnector: Reconnector::default(),
        }
    }
}

/// Modbus 驱动（TCP / RTU-over-TCP）。
///
/// `Context` 内部为 `Box<dyn Client>`（仅 `Send` 非 `Sync`），而 [`Driver`] trait
/// 要求 `Send + Sync`，故用 `tokio::sync::Mutex` 包裹以满足对象安全约束。
pub struct ModbusDriver {
    config: ModbusConfig,
    ctx: Option<tokio::sync::Mutex<Context>>,
    reconnector: Reconnector,
}

/// 请求级错误分类：决定是否触发重连。
enum RequestError {
    /// 连接丢失（超时 / 传输错误）→ 按退避重连并重试一次。
    ConnectionLost(String),
    /// 协议语义错误（异常响应 / 本地校验）→ 直接返回，不重连。
    Terminal(String),
}

/// 单次 Modbus 请求操作（`run_request` 内部执行；`Copy` 供重试复用）。
#[derive(Debug, Clone, Copy)]
enum RequestOp {
    /// FC03 读保持寄存器（协议地址, 数量）。
    ReadHolding(u16, u16),
    /// FC04 读输入寄存器（协议地址, 数量）。
    ReadInput(u16, u16),
    /// FC06 写单个保持寄存器（协议地址, 值）。
    WriteSingle(u16, u16),
}

/// 请求结果。
#[derive(Debug)]
enum RequestOutcome {
    /// 读到的寄存器字序列。
    Words(Vec<u16>),
    /// 写操作完成。
    Written,
}

/// 将 tokio-modbus 错误分类为请求级错误。
fn classify_request_error(err: Error) -> RequestError {
    match err {
        Error::Protocol(e) => RequestError::Terminal(format!("modbus protocol error: {e:?}")),
        Error::Transport(e) => RequestError::ConnectionLost(format!("modbus transport error: {e}")),
    }
}

/// 重试仍失败时的最终错误映射。
fn finalize_request_error(err: RequestError) -> DaemonError {
    match err {
        RequestError::Terminal(msg) => DaemonError::ProtocolError(msg),
        RequestError::ConnectionLost(msg) => DaemonError::NetworkError(msg),
    }
}

impl ModbusDriver {
    /// 创建驱动（`reconnector` 从配置克隆为工作状态）。
    pub fn new(config: ModbusConfig) -> Self {
        let reconnector = config.reconnector.clone();
        Self {
            config,
            ctx: None,
            reconnector,
        }
    }

    /// 当前重连退避状态（只读；测试断言用）。
    pub fn reconnector(&self) -> &Reconnector {
        &self.reconnector
    }

    /// 建立底层连接（TCP 拨号 / RTU-over-TCP 拨号）。
    async fn open(&self) -> DaemonResult<Context> {
        let addr = self.config.addr;
        let slave = Slave(self.config.slave);
        match self.config.framing {
            ModbusFraming::Tcp => tcp::connect_slave(addr, slave)
                .await
                .map_err(|e| DaemonError::NetworkError(format!("modbus tcp connect {addr}: {e}"))),
            ModbusFraming::Rtu => {
                let stream = TcpStream::connect(addr).await.map_err(|e| {
                    DaemonError::NetworkError(format!("modbus rtu connect {addr}: {e}"))
                })?;
                Ok(rtu::attach_slave(stream, slave))
            }
        }
    }

    /// 确保已连接（惰性连接：未连接时先 `connect()`）。
    async fn ensure_connected(&mut self) -> DaemonResult<()> {
        if self.ctx.is_none() {
            self.connect().await?;
        }
        Ok(())
    }

    /// 在已连接上下文上执行一次请求；连接丢失时按退避重连并重试一次。
    async fn run_request(&mut self, op: RequestOp) -> DaemonResult<RequestOutcome> {
        let timeout_dur = self.config.timeout;
        self.ensure_connected().await?;
        let result = {
            let ctx = self.ctx.as_ref().expect("connected");
            let mut guard = ctx.lock().await;
            timeout(timeout_dur, Self::execute(&mut guard, op)).await
        };
        match result {
            Ok(Ok(v)) => Ok(v),
            Ok(Err(e)) => self.recover(e, op).await,
            Err(_) => {
                self.recover(
                    RequestError::ConnectionLost("modbus request timeout".into()),
                    op,
                )
                .await
            }
        }
    }

    /// 处理请求失败：终端错误直接返回；连接丢失走退避 → 重连 → 重试一次。
    async fn recover(&mut self, err: RequestError, op: RequestOp) -> DaemonResult<RequestOutcome> {
        match err {
            RequestError::Terminal(msg) => Err(DaemonError::ProtocolError(msg)),
            RequestError::ConnectionLost(_) => {
                let delay = self.reconnector.next_delay();
                tokio::time::sleep(delay).await;
                match self.open().await {
                    Ok(ctx) => {
                        self.ctx = Some(tokio::sync::Mutex::new(ctx));
                        let timeout_dur = self.config.timeout;
                        let result = {
                            let mut guard = self.ctx.as_ref().expect("just set").lock().await;
                            timeout(timeout_dur, Self::execute(&mut guard, op)).await
                        };
                        match result {
                            Ok(Ok(v)) => Ok(v),
                            Ok(Err(e)) => {
                                self.reconnector.next_delay();
                                Err(finalize_request_error(e))
                            }
                            Err(_) => {
                                self.reconnector.next_delay();
                                Err(DaemonError::ProtocolError(
                                    "modbus request timeout after reconnect".to_string(),
                                ))
                            }
                        }
                    }
                    Err(e) => {
                        self.reconnector.next_delay();
                        Err(e)
                    }
                }
            }
        }
    }

    /// 在给定上下文上执行请求（无 IO 超时；超时由调用方 `timeout` 包装）。
    async fn execute(ctx: &mut Context, op: RequestOp) -> Result<RequestOutcome, RequestError> {
        match op {
            RequestOp::ReadHolding(reg, count) => {
                let result = ctx.read_holding_registers(reg, count).await;
                match result {
                    Ok(Ok(words)) => Ok(RequestOutcome::Words(words)),
                    Ok(Err(code)) => {
                        Err(RequestError::Terminal(format!("modbus exception {code:?}")))
                    }
                    Err(e) => Err(classify_request_error(e)),
                }
            }
            RequestOp::ReadInput(reg, count) => {
                let result = ctx.read_input_registers(reg, count).await;
                match result {
                    Ok(Ok(words)) => Ok(RequestOutcome::Words(words)),
                    Ok(Err(code)) => {
                        Err(RequestError::Terminal(format!("modbus exception {code:?}")))
                    }
                    Err(e) => Err(classify_request_error(e)),
                }
            }
            RequestOp::WriteSingle(reg, value) => {
                let result = ctx.write_single_register(reg, value).await;
                match result {
                    Ok(Ok(())) => Ok(RequestOutcome::Written),
                    Ok(Err(code)) => {
                        Err(RequestError::Terminal(format!("modbus exception {code:?}")))
                    }
                    Err(e) => Err(classify_request_error(e)),
                }
            }
        }
    }

    /// 校验读取点位：返回 (区, 协议地址, 数量)。
    fn validate_read_point(p: &ReadPoint) -> DaemonResult<(char, u16, u16)> {
        let area = match p.address.area {
            Some('4') => '4',
            Some('3') => '3',
            _ => {
                return Err(DaemonError::ProtocolError(format!(
                    "modbus read requires 4xxxx/3xxxx register address, got {:?}",
                    p.address
                )))
            }
        };
        if p.address.bit {
            return Err(DaemonError::ProtocolError(
                "modbus register read does not support bit access".to_string(),
            ));
        }
        let reg = p.address.start.checked_sub(1).ok_or_else(|| {
            DaemonError::ProtocolError("modbus register number must be >= 1".to_string())
        })?;
        if reg > u32::from(u16::MAX) {
            return Err(DaemonError::ProtocolError(format!(
                "modbus register {} out of range",
                p.address.start
            )));
        }
        if p.count == 0 || p.count > 125 {
            return Err(DaemonError::ProtocolError(format!(
                "modbus read count {} out of range 1..=125",
                p.count
            )));
        }
        Ok((area, reg as u16, p.count as u16))
    }

    /// 校验写入点位：返回 (协议地址, 寄存器值)。
    fn validate_write_point(p: &WritePoint) -> DaemonResult<(u16, u16)> {
        if p.address.area != Some('4') {
            return Err(DaemonError::ProtocolError(format!(
                "modbus write requires 4xxxx holding register address, got {:?}",
                p.address
            )));
        }
        if p.address.bit {
            return Err(DaemonError::ProtocolError(
                "modbus register write does not support bit access".to_string(),
            ));
        }
        let reg = p.address.start.checked_sub(1).ok_or_else(|| {
            DaemonError::ProtocolError("modbus register number must be >= 1".to_string())
        })?;
        if reg > u32::from(u16::MAX) {
            return Err(DaemonError::ProtocolError(format!(
                "modbus register {} out of range",
                p.address.start
            )));
        }
        if p.value.len() != 2 {
            return Err(DaemonError::ProtocolError(format!(
                "modbus write requires exactly 2 bytes (one register), got {} bytes",
                p.value.len()
            )));
        }
        let value = u16::from_be_bytes([p.value[0], p.value[1]]);
        Ok((reg as u16, value))
    }
}

#[async_trait]
impl Driver for ModbusDriver {
    async fn connect(&mut self) -> DaemonResult<()> {
        match self.open().await {
            Ok(ctx) => {
                self.ctx = Some(tokio::sync::Mutex::new(ctx));
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
        let mut samples = Vec::with_capacity(points.len());
        for p in points {
            let (area, reg, count) = Self::validate_read_point(p)?;
            let op = match area {
                '4' => RequestOp::ReadHolding(reg, count),
                '3' => RequestOp::ReadInput(reg, count),
                _ => unreachable!("validated area"),
            };
            let words = match self.run_request(op).await? {
                RequestOutcome::Words(words) => words,
                RequestOutcome::Written => unreachable!("read op cannot write"),
            };
            let mut value = Vec::with_capacity(words.len() * 2);
            for w in words {
                value.extend_from_slice(&w.to_be_bytes());
            }
            samples.push(PointSample {
                address: p.address.clone(),
                value,
            });
        }
        Ok(samples)
    }

    async fn write(&mut self, points: &[WritePoint]) -> DaemonResult<()> {
        for p in points {
            let (reg, value) = Self::validate_write_point(p)?;
            match self.run_request(RequestOp::WriteSingle(reg, value)).await? {
                RequestOutcome::Written => {}
                RequestOutcome::Words(_) => unreachable!("write op cannot read"),
            }
        }
        Ok(())
    }

    async fn disconnect(&mut self) -> DaemonResult<()> {
        if let Some(ctx) = self.ctx.take() {
            let mut guard = ctx.lock().await;
            guard
                .disconnect()
                .await
                .map_err(|e| DaemonError::NetworkError(format!("modbus disconnect: {e}")))?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::future;
    use std::sync::{Arc, Mutex};

    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::{TcpListener, TcpStream};
    use tokio_modbus::server::tcp::{accept_tcp_connection, Server};
    use tokio_modbus::server::Service;
    use tokio_modbus::{ExceptionCode, Request, Response};

    use crate::driver::PointAddressParser;
    use crate::error::{ERR_NETWORK, ERR_PROTOCOL};

    // ---- mock 服务器 ----

    /// mock 服务器共享状态。
    #[derive(Debug, Default)]
    struct MockState {
        holding: Vec<u16>,
        input: Vec<u16>,
        writes: Vec<(u16, u16)>,
        requests: Vec<Request<'static>>,
    }

    /// 基于 tokio-modbus server 的 mock 服务（共享状态，可断言请求日志）。
    #[derive(Clone)]
    struct MockService {
        state: Arc<Mutex<MockState>>,
    }

    impl Service for MockService {
        type Request = Request<'static>;
        type Response = Response;
        type Exception = ExceptionCode;
        type Future = future::Ready<std::result::Result<Response, ExceptionCode>>;

        fn call(&self, req: Self::Request) -> Self::Future {
            let mut state = self.state.lock().expect("mock state lock");
            state.requests.push(req.clone());
            let resp = match req {
                Request::ReadHoldingRegisters(addr, cnt) => {
                    let end = addr as usize + cnt as usize;
                    if end > state.holding.len() {
                        return future::ready(Err(ExceptionCode::IllegalDataAddress));
                    }
                    Response::ReadHoldingRegisters(state.holding[addr as usize..end].to_vec())
                }
                Request::ReadInputRegisters(addr, cnt) => {
                    let end = addr as usize + cnt as usize;
                    if end > state.input.len() {
                        return future::ready(Err(ExceptionCode::IllegalDataAddress));
                    }
                    Response::ReadInputRegisters(state.input[addr as usize..end].to_vec())
                }
                Request::WriteSingleRegister(addr, val) => {
                    if addr as usize >= state.holding.len() {
                        return future::ready(Err(ExceptionCode::IllegalDataAddress));
                    }
                    state.holding[addr as usize] = val;
                    state.writes.push((addr, val));
                    Response::WriteSingleRegister(addr, val)
                }
                _ => return future::ready(Err(ExceptionCode::IllegalFunction)),
            };
            future::ready(Ok(resp))
        }
    }

    /// 启动共享状态 mock 服务器，返回监听地址。
    async fn spawn_mock_server(state: Arc<Mutex<MockState>>) -> SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("local addr");
        let new_service = move |_socket_addr: SocketAddr| {
            Ok(Some(MockService {
                state: Arc::clone(&state),
            }))
        };
        let on_connected = move |stream: TcpStream, socket_addr: SocketAddr| {
            let new_service = new_service.clone();
            async move { accept_tcp_connection(stream, socket_addr, new_service) }
        };
        let server = Server::new(listener);
        tokio::spawn(async move {
            server
                .serve(&on_connected, |err| eprintln!("mock modbus server: {err}"))
                .await
                .expect("serve");
        });
        addr
    }

    /// 启动「首连接即断」的 flaky 服务器：连接 1 被接受后立即丢弃，
    /// 连接 2 起正常服务（模拟设备崩溃后恢复）。
    async fn spawn_flaky_server(state: Arc<Mutex<MockState>>) -> SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("local addr");
        tokio::spawn(async move {
            let (_stream, _peer) = listener.accept().await.expect("accept conn 1");
            let new_service = move |_socket_addr: SocketAddr| {
                Ok(Some(MockService {
                    state: Arc::clone(&state),
                }))
            };
            let on_connected = move |stream: TcpStream, socket_addr: SocketAddr| {
                let new_service = new_service.clone();
                async move { accept_tcp_connection(stream, socket_addr, new_service) }
            };
            let server = Server::new(listener);
            server
                .serve(&on_connected, |err| eprintln!("mock modbus server: {err}"))
                .await
                .expect("serve");
        });
        addr
    }

    /// 测试用驱动配置：短超时 + 毫秒级退避（避免测试等待）。
    fn test_config(addr: SocketAddr, framing: ModbusFraming) -> ModbusConfig {
        ModbusConfig {
            addr,
            slave: 0xFF,
            framing,
            timeout: Duration::from_secs(2),
            reconnector: Reconnector::new(Duration::from_millis(1), Duration::from_millis(2), 2),
        }
    }

    // ---- TCP happy path ----

    #[tokio::test]
    async fn modbus_tcp_read_holding_registers() {
        let state = Arc::new(Mutex::new(MockState {
            holding: vec![0x1234, 0xABCD, 0x0001],
            input: vec![0x1111, 0x2222],
            ..Default::default()
        }));
        let addr = spawn_mock_server(Arc::clone(&state)).await;

        let mut driver = ModbusDriver::new(test_config(addr, ModbusFraming::Tcp));
        driver.connect().await.expect("connect");

        let point = ReadPoint {
            address: PointAddressParser::parse("40001").expect("address"),
            count: 2,
        };
        let samples = driver
            .read(std::slice::from_ref(&point))
            .await
            .expect("read");
        assert_eq!(samples.len(), 1, "1:1 with request");
        assert_eq!(samples[0].address, point.address, "echoes request address");
        assert_eq!(
            samples[0].value,
            vec![0x12, 0x34, 0xAB, 0xCD],
            "registers big-endian concatenated"
        );

        {
            let state = state.lock().expect("lock");
            assert_eq!(
                state.requests,
                vec![Request::ReadHoldingRegisters(0, 2)],
                "server saw FC03 addr=0 count=2"
            );
        }
        driver.disconnect().await.expect("disconnect");
    }

    #[tokio::test]
    async fn modbus_tcp_read_input_registers() {
        let state = Arc::new(Mutex::new(MockState {
            holding: vec![0x1234],
            input: vec![0x1111, 0x2222],
            ..Default::default()
        }));
        let addr = spawn_mock_server(Arc::clone(&state)).await;

        let mut driver = ModbusDriver::new(test_config(addr, ModbusFraming::Tcp));
        driver.connect().await.expect("connect");

        let point = ReadPoint {
            address: PointAddressParser::parse("30001").expect("address"),
            count: 2,
        };
        let samples = driver.read(&[point]).await.expect("read");
        assert_eq!(samples[0].value, vec![0x11, 0x11, 0x22, 0x22]);

        {
            let state = state.lock().expect("lock");
            assert_eq!(state.requests, vec![Request::ReadInputRegisters(0, 2)]);
        }
        driver.disconnect().await.expect("disconnect");
    }

    #[tokio::test]
    async fn modbus_tcp_write_single_register() {
        let state = Arc::new(Mutex::new(MockState {
            holding: vec![0x0000, 0x0000],
            ..Default::default()
        }));
        let addr = spawn_mock_server(Arc::clone(&state)).await;

        let mut driver = ModbusDriver::new(test_config(addr, ModbusFraming::Tcp));
        driver.connect().await.expect("connect");

        let point = WritePoint {
            address: PointAddressParser::parse("40002").expect("address"),
            value: vec![0xBE, 0xEF],
        };
        driver.write(&[point]).await.expect("write");

        {
            let state = state.lock().expect("lock");
            assert_eq!(state.holding[1], 0xBEEF, "register updated");
            assert_eq!(state.writes, vec![(1, 0xBEEF)], "write log");
            assert_eq!(
                state.requests,
                vec![Request::WriteSingleRegister(1, 0xBEEF)],
                "server saw FC06 addr=1"
            );
        }
        driver.disconnect().await.expect("disconnect");
    }

    // ---- error path ----

    #[tokio::test]
    async fn modbus_exception_maps_to_protocol_error_without_reconnect() {
        let state = Arc::new(Mutex::new(MockState {
            holding: vec![0x0001], // 仅 1 个寄存器
            ..Default::default()
        }));
        let addr = spawn_mock_server(Arc::clone(&state)).await;

        let mut driver = ModbusDriver::new(test_config(addr, ModbusFraming::Tcp));
        driver.connect().await.expect("connect");

        let point = ReadPoint {
            address: PointAddressParser::parse("40001").expect("address"),
            count: 2, // 越界 → IllegalDataAddress
        };
        let err = driver.read(&[point]).await.expect_err("must fail");
        assert!(matches!(err, DaemonError::ProtocolError(_)), "{err:?}");
        assert_eq!(err.error_code(), ERR_PROTOCOL);
        assert_eq!(
            driver.reconnector().clone().next_delay(),
            Duration::from_millis(1),
            "terminal error must not advance backoff"
        );
        driver.disconnect().await.expect("disconnect");
    }

    #[tokio::test]
    async fn modbus_unreachable_connect_returns_network_error() {
        let mut driver = ModbusDriver::new(test_config(
            "127.0.0.1:1".parse().expect("addr"),
            ModbusFraming::Tcp,
        ));
        let err = driver.connect().await.expect_err("must fail");
        assert!(matches!(err, DaemonError::NetworkError(_)), "{err:?}");
        assert_eq!(err.error_code(), ERR_NETWORK);
        assert_eq!(
            driver.reconnector().clone().next_delay(),
            Duration::from_millis(2),
            "connect failure advances backoff"
        );
    }

    #[tokio::test]
    async fn modbus_silent_server_times_out_to_protocol_error() {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("local addr");
        tokio::spawn(async move {
            // 接受所有连接，保持打开但不响应（静默），直到客户端关闭。
            loop {
                let (stream, _peer) = listener.accept().await.expect("accept");
                tokio::spawn(async move {
                    let _stream = stream; // 保持连接打开
                    tokio::time::sleep(Duration::from_secs(30)).await;
                });
            }
        });

        let mut driver = ModbusDriver::new(ModbusConfig {
            timeout: Duration::from_millis(100),
            ..test_config(addr, ModbusFraming::Tcp)
        });
        driver.connect().await.expect("connect");

        let point = ReadPoint {
            address: PointAddressParser::parse("40001").expect("address"),
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

    // ---- reconnect ----

    #[tokio::test]
    async fn modbus_reconnect_after_connection_loss() {
        let state = Arc::new(Mutex::new(MockState {
            holding: vec![0xCAFE],
            ..Default::default()
        }));
        let addr = spawn_flaky_server(Arc::clone(&state)).await;

        let mut driver = ModbusDriver::new(test_config(addr, ModbusFraming::Tcp));
        driver.connect().await.expect("connect");

        let point = ReadPoint {
            address: PointAddressParser::parse("40001").expect("address"),
            count: 1,
        };
        let samples = driver.read(&[point]).await.expect("read after reconnect");
        assert_eq!(samples[0].value, vec![0xCA, 0xFE]);
        assert_eq!(
            driver.reconnector().clone().next_delay(),
            Duration::from_millis(2),
            "recovery reconnect must not reset backoff"
        );
        driver.disconnect().await.expect("disconnect");
    }

    // ---- RTU-over-TCP ----

    #[tokio::test]
    async fn modbus_rtu_over_tcp_framing() {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("local addr");
        let (tx, rx) = tokio::sync::oneshot::channel();
        tokio::spawn(async move {
            let result = async {
                let (mut stream, _peer) = listener.accept().await.map_err(|e| e.to_string())?;
                let mut buf = [0u8; 8];
                stream
                    .read_exact(&mut buf)
                    .await
                    .map_err(|e| e.to_string())?;
                assert_eq!(buf[0], 0x11, "slave id");
                assert_eq!(buf[1], 0x03, "FC03 read holding");
                assert_eq!(&buf[2..4], &[0x00, 0x00], "register addr 0");
                assert_eq!(&buf[4..6], &[0x00, 0x02], "count 2");
                let crc = crc16_modbus(&buf[..6]);
                assert_eq!(buf[6], (crc & 0xFF) as u8, "crc low byte first");
                assert_eq!(buf[7], (crc >> 8) as u8, "crc high byte");
                let data = [0x12u8, 0x34, 0xAB, 0xCD];
                let mut resp = vec![0x11, 0x03, 4];
                resp.extend_from_slice(&data);
                let crc = crc16_modbus(&resp);
                resp.push((crc & 0xFF) as u8);
                resp.push((crc >> 8) as u8);
                stream.write_all(&resp).await.map_err(|e| e.to_string())?;
                Ok::<(), String>(())
            }
            .await;
            let _ = tx.send(result);
        });

        let mut driver = ModbusDriver::new(ModbusConfig {
            slave: 0x11,
            framing: ModbusFraming::Rtu,
            ..test_config(addr, ModbusFraming::Rtu)
        });
        driver.connect().await.expect("connect");
        let point = ReadPoint {
            address: PointAddressParser::parse("40001").expect("address"),
            count: 2,
        };
        let samples = driver.read(&[point]).await.expect("read");
        assert_eq!(samples[0].value, vec![0x12, 0x34, 0xAB, 0xCD]);
        driver.disconnect().await.expect("disconnect");

        rx.await
            .expect("server task alive")
            .expect("server assertions passed");
    }

    // ---- 本地校验（不触网） ----

    #[tokio::test]
    async fn modbus_validation_rejects_bad_points() {
        let mut driver = ModbusDriver::new(test_config(
            "127.0.0.1:1".parse().expect("addr"),
            ModbusFraming::Tcp,
        ));

        let err = driver
            .write(&[WritePoint {
                address: PointAddressParser::parse("30001").expect("input register"),
                value: vec![0x00, 0x01],
            }])
            .await
            .expect_err("input register is read-only");
        assert!(matches!(err, DaemonError::ProtocolError(_)));
        assert_eq!(err.error_code(), ERR_PROTOCOL);

        let err = driver
            .write(&[WritePoint {
                address: PointAddressParser::parse("40001").expect("holding"),
                value: vec![0x00],
            }])
            .await
            .expect_err("write value must be exactly 2 bytes");
        assert!(matches!(err, DaemonError::ProtocolError(_)));

        let err = driver
            .read(&[ReadPoint {
                address: PointAddressParser::parse("40001").expect("holding"),
                count: 0,
            }])
            .await
            .expect_err("count must be >= 1");
        assert!(matches!(err, DaemonError::ProtocolError(_)));

        let err = driver
            .read(&[ReadPoint {
                address: PointAddressParser::parse("40001").expect("holding"),
                count: 126,
            }])
            .await
            .expect_err("count must be <= 125");
        assert!(matches!(err, DaemonError::ProtocolError(_)));
    }

    /// CRC16-Modbus（poly 0xA001，init 0xFFFF，低字节先发）。
    fn crc16_modbus(data: &[u8]) -> u16 {
        let mut crc: u16 = 0xFFFF;
        for &byte in data {
            crc ^= u16::from(byte);
            for _ in 0..8 {
                if crc & 1 != 0 {
                    crc = (crc >> 1) ^ 0xA001;
                } else {
                    crc >>= 1;
                }
            }
        }
        crc
    }
}

//! 南向采集装配（task-61 验收 D-12 修复）：把 `GroupScheduler` 的
//! [`PollHandler`] 接到真实南向驱动。
//!
//! # 背景
//! 修复前 `BootstrapBuilder::with_poll_handler` 全仓**没有生产调用方**（仅测试），
//! 生产形态恒走 `warn!("no poll handler configured; scheduler not started (V1)")`
//! —— 采集调度器在生产永不启动（容器实测：点位注册成功但 mock 从站 0 连接）。
//! bin 装配入口（`src/bin/iot-daq-daemon.rs`）现按本模块构造 [`DevicePollHandler`]
//! 并 `with_poll_handler(...)` 注入 bootstrap，生产路径调度器真正驱动南向读。
//!
//! # 职责边界
//! - **做**：按配置点位表把 `poll(group, point_ids)` 翻译为「该设备一次南向驱动
//!   批量读」（惰性建连 + 断线自愈走 [`crate::driver::Reconnector`]）；返回成功
//!   采集样本数供调度器统计。
//! - **不做**：北向转发 / 离线队列（bootstrap 的 north 装配链路负责）；授权 /
//!   降级状态的采集限制（Degraded 档位配额闸门在
//!   `LicenseRuntime::enforce_free_limits` 与热重载准入闸门——本模块不重复实现、
//!   也绝不绕过）。
//!
//! # V1 限制（随代码注释，勿静默扩面）
//! - 支持协议：`modbus-tcp` / `modbus-rtu`（RTU-over-TCP；从站号固定默认 0xFF，
//!   从站号配置字段留待点位 schema 扩展）。其余协议该组轮询显式 `ConfigError`
//!   （错误隔离，不影响其余组），绝不猜测意图。
//! - 点位标识（`point_id`）即南向寄存器地址（Modbus 5 位编址，如 `40001`）——
//!   当前点位 schema 无独立寄存器地址列；不可解析为南向地址的点位跳过并 `warn!`。
//! - 设备级协议 / 接入地址取该设备**首个点位行**（schema 内设备地址随行冗余）。

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use tracing::warn;

use crate::config::GatewayConfig;
use crate::driver::modbus::{ModbusConfig, ModbusDriver, ModbusFraming};
use crate::driver::{Driver, PointAddressParser, ReadPoint, Reconnector};
use crate::error::{DaemonError, DaemonResult};
use crate::scheduler::PollHandler;

/// Modbus 单次请求超时（与 `ModbusConfig::default` 一致；显式声明便于运维口径统一）。
const SOUTHBOUND_REQUEST_TIMEOUT: Duration = Duration::from_secs(3);

/// 缺省南向端口（设备接入地址未写端口时使用，Modbus 标准端口）。
const DEFAULT_SOUTHBOUND_PORT: u16 = 502;

/// 每设备驱动连接：互斥锁内 `Box<dyn Driver>`（组任务串行使用本设备连接）。
type DeviceConn = Arc<tokio::sync::Mutex<Box<dyn Driver>>>;

/// 单设备的轮询计划：协议 + 接入地址 + 该设备全部点位标识。
#[derive(Debug, Clone)]
struct DevicePlan {
    /// 南向协议字面量（点位行 `protocol`，如 `modbus-tcp`）。
    protocol: String,
    /// 设备接入地址（点位行 `address`，如 `192.168.1.10:502`）。
    address: String,
    /// 该设备点位标识列表（首次出现顺序、去重）。
    point_ids: Vec<String>,
}

/// 生产轮询动作：把调度器的组轮询翻译为南向驱动批量读（一组一设备一连接）。
///
/// 由 bin 装配入口构造并经 `BootstrapBuilder::with_poll_handler` 注入；多组共享
/// 本实例（内部 `Arc` 连接表），组间互不阻塞（每设备独立互斥锁）。
pub struct DevicePollHandler {
    /// 设备轮询计划（`device_id -> DevicePlan`；组名 = `device_id`）。
    devices: HashMap<String, DevicePlan>,
    /// 设备驱动连接表（惰性建连；`poll` 期间按设备锁串行，组间并行）。
    conns: tokio::sync::Mutex<HashMap<String, DeviceConn>>,
}

impl DevicePollHandler {
    /// 从配置点位表构建轮询计划（一组一设备；点位按首次出现顺序去重）。
    ///
    /// 设备级协议 / 接入地址取该设备首个点位行；纯内存构建、不失败——协议 /
    /// 地址非法性延迟到 `poll` 时按组显式报错（错误隔离，不阻断启动）。
    #[must_use]
    pub fn from_config(config: &GatewayConfig) -> Self {
        let mut order: Vec<String> = Vec::new();
        let mut devices: HashMap<String, DevicePlan> = HashMap::new();
        for point in &config.points {
            let plan = devices.entry(point.device_id.clone()).or_insert_with(|| {
                order.push(point.device_id.clone());
                DevicePlan {
                    protocol: point.protocol.clone(),
                    address: point.address.clone(),
                    point_ids: Vec::new(),
                }
            });
            if !plan.point_ids.contains(&point.point_id) {
                plan.point_ids.push(point.point_id.clone());
            }
        }
        Self {
            devices,
            conns: tokio::sync::Mutex::new(HashMap::new()),
        }
    }

    /// 配置是否声明了任何点位（bin 装配判据：无点位不注入 handler，避免噪音告警）。
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.devices.is_empty()
    }

    /// V1 支持的南向协议 → Modbus 帧格式（大小写不敏感；未知协议 `None`）。
    fn framing_for(protocol: &str) -> Option<ModbusFraming> {
        match protocol.trim().to_ascii_lowercase().as_str() {
            "modbus-tcp" => Some(ModbusFraming::Tcp),
            "modbus-rtu" => Some(ModbusFraming::Rtu),
            _ => None,
        }
    }

    /// 解析设备接入地址（`host[:port]`；缺省端口 [`DEFAULT_SOUTHBOUND_PORT`]；
    /// 主机名走系统解析，取首个解析结果）。
    async fn resolve_addr(raw: &str) -> DaemonResult<SocketAddr> {
        let trimmed = raw.trim();
        let with_port = if trimmed.contains(':') {
            trimmed.to_string()
        } else {
            format!("{trimmed}:{DEFAULT_SOUTHBOUND_PORT}")
        };
        // 先绑定中间值再收尾（尾表达式借用局部变量的临时生命周期问题）。
        let resolved = tokio::net::lookup_host(&with_port)
            .await
            .map_err(|err| {
                DaemonError::NetworkError(format!(
                    "southbound address {raw:?} resolve failed: {err}"
                ))
            })?
            .next();
        resolved.ok_or_else(|| {
            DaemonError::NetworkError(format!(
                "southbound address {raw:?} resolved to no socket address"
            ))
        })
    }

    /// 取该组设备连接；缺席则按计划建连并登记（惰性，首拍或断线卸载后触发）。
    async fn get_or_connect(&self, group: &str, plan: &DevicePlan) -> DaemonResult<DeviceConn> {
        let mut conns = self.conns.lock().await;
        if let Some(conn) = conns.get(group) {
            return Ok(Arc::clone(conn));
        }
        let framing = Self::framing_for(&plan.protocol).ok_or_else(|| {
            DaemonError::ConfigError(format!(
                "southbound poll: device {group:?} protocol {:?} is not wired into the poll \
                 handler (V1 supports modbus-tcp / modbus-rtu)",
                plan.protocol
            ))
        })?;
        let addr = Self::resolve_addr(&plan.address).await?;
        let mut driver = ModbusDriver::new(ModbusConfig {
            addr,
            framing,
            timeout: SOUTHBOUND_REQUEST_TIMEOUT,
            reconnector: Reconnector::default(),
            ..ModbusConfig::default()
        });
        driver.connect().await?;
        let conn: DeviceConn = Arc::new(tokio::sync::Mutex::new(Box::new(driver)));
        conns.insert(group.to_string(), Arc::clone(&conn));
        Ok(conn)
    }
}

#[async_trait]
impl PollHandler for DevicePollHandler {
    async fn poll(&self, group: &str, point_ids: &[String]) -> DaemonResult<usize> {
        let plan = self.devices.get(group).ok_or_else(|| {
            DaemonError::ConfigError(format!("southbound poll: unknown device group {group:?}"))
        })?;
        if plan.point_ids.is_empty() {
            return Ok(0);
        }

        // 惰性建连（连接表只在取/插时短持锁；读期间按设备锁串行，组间并行）。
        let conn = self.get_or_connect(group, plan).await?;
        let mut driver = conn.lock().await;

        // 组内批量读：一次驱动请求承载全部可读点位（PollHandler 契约）。
        // 未知点位 / 不可解析为南向地址的点位跳过并告警（不整体失败）。
        let mut read_points: Vec<ReadPoint> = Vec::with_capacity(point_ids.len());
        for point_id in point_ids {
            if !plan.point_ids.contains(point_id) {
                warn!(
                    group = %group,
                    point_id = %point_id,
                    "southbound poll: point id missing from the device plan; skipped"
                );
                continue;
            }
            match PointAddressParser::parse(point_id) {
                Ok(address) => read_points.push(ReadPoint { address, count: 1 }),
                Err(err) => warn!(
                    group = %group,
                    point_id = %point_id,
                    error = %err,
                    "southbound poll: point id is not a parseable southbound address; skipped"
                ),
            }
        }
        if read_points.is_empty() {
            return Ok(0);
        }
        let samples = driver.read(&read_points).await?;
        Ok(samples.len())
    }
}

// ---------------------------------------------------------------------------
// 单元 / 集成测试（mock 从站真实链路 + 生产 bootstrap 路径接线证明）
// ---------------------------------------------------------------------------
#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;

    use tokio_modbus::server::tcp::{accept_tcp_connection, Server};
    use tokio_modbus::server::Service;
    use tokio_modbus::{ExceptionCode, Request, Response};

    use crate::bootstrap::{BootstrapBuilder, DaemonShared, LifecycleState};
    use crate::scheduler::{GroupConfig, GroupScheduler, PollHandler};

    // ---- mock Modbus 从站（与 driver::modbus 测试同款最小实现） ----

    /// mock 从站共享状态（寄存器 + 请求日志）。
    #[derive(Debug, Default)]
    struct MockInner {
        holding: Vec<u16>,
        requests: Vec<Request<'static>>,
    }

    /// mock 从站服务（FC03 读保持寄存器；其余异常码拒绝）。
    #[derive(Clone)]
    struct MockService {
        state: Arc<Mutex<MockInner>>,
    }

    impl Service for MockService {
        type Request = Request<'static>;
        type Response = Response;
        type Exception = ExceptionCode;
        type Future = std::future::Ready<std::result::Result<Response, ExceptionCode>>;

        fn call(&self, req: Self::Request) -> Self::Future {
            let mut state = self.state.lock().expect("mock state lock");
            let resp = match req {
                Request::ReadHoldingRegisters(addr, cnt) => {
                    state
                        .requests
                        .push(Request::ReadHoldingRegisters(addr, cnt));
                    let end = addr as usize + cnt as usize;
                    if end > state.holding.len() {
                        return std::future::ready(Err(ExceptionCode::IllegalDataAddress));
                    }
                    Response::ReadHoldingRegisters(state.holding[addr as usize..end].to_vec())
                }
                _ => return std::future::ready(Err(ExceptionCode::IllegalFunction)),
            };
            std::future::ready(Ok(resp))
        }
    }

    /// 启动 mock 从站：返回监听地址（`connections` 累计被接受的连接数）。
    async fn spawn_mock_server(
        state: Arc<Mutex<MockInner>>,
        connections: Arc<AtomicUsize>,
    ) -> SocketAddr {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind mock slave");
        let addr = listener.local_addr().expect("local addr");
        tokio::spawn(async move {
            let server = Server::new(listener);
            let on_connected = move |stream: tokio::net::TcpStream, socket_addr: SocketAddr| {
                connections.fetch_add(1, Ordering::SeqCst);
                let state = Arc::clone(&state);
                async move {
                    accept_tcp_connection(stream, socket_addr, move |_| {
                        Ok(Some(MockService {
                            state: Arc::clone(&state),
                        }))
                    })
                }
            };
            let _ = server
                .serve(&on_connected, |err| {
                    eprintln!("mock modbus server: {err}");
                })
                .await;
        });
        addr
    }

    /// 构造单设备单点位的测试配置 TOML。
    fn one_point_toml(addr: SocketAddr, protocol: &str, point_id: &str) -> String {
        format!(
            "[[points]]\ndevice_id = \"dev-01\"\npoint_id = {point_id:?}\n\
             protocol = {protocol:?}\naddress = \"{addr}\"\nfrequency_ms = 100\n"
        )
    }

    // ---- 计划构建 ----

    /// from_config：一组一设备、点位去重、协议/地址取首行；无点位 → is_empty。
    #[test]
    fn from_config_groups_points_by_device_and_dedups() {
        let config = GatewayConfig::parse(
            "[[points]]\ndevice_id = \"dev-a\"\npoint_id = \"40001\"\n\
             protocol = \"modbus-tcp\"\naddress = \"10.0.0.1:502\"\n\n\
             [[points]]\ndevice_id = \"dev-a\"\npoint_id = \"40001\"\n\
             protocol = \"modbus-tcp\"\naddress = \"10.0.0.1:502\"\n\n\
             [[points]]\ndevice_id = \"dev-b\"\npoint_id = \"D100\"\n\
             protocol = \"mc\"\naddress = \"10.0.0.2:6000\"\n",
        )
        .expect("parse");
        let handler = DevicePollHandler::from_config(&config);
        assert!(!handler.is_empty());
        assert_eq!(handler.devices.len(), 2, "one plan per device");
        let a = &handler.devices["dev-a"];
        assert_eq!(a.protocol, "modbus-tcp");
        assert_eq!(a.address, "10.0.0.1:502");
        assert_eq!(a.point_ids, vec!["40001".to_string()], "deduped");

        let empty = DevicePollHandler::from_config(&GatewayConfig::default());
        assert!(empty.is_empty(), "no points → empty plan");
    }

    /// 未知组轮询 → ConfigError（可解释）。
    #[tokio::test]
    async fn poll_unknown_group_is_config_error() {
        let handler = DevicePollHandler::from_config(&GatewayConfig::default());
        let err = handler
            .poll("no-such-device", &["40001".to_string()])
            .await
            .expect_err("unknown group must fail");
        assert!(matches!(err, DaemonError::ConfigError(_)), "{err:?}");
        assert!(err.to_string().contains("no-such-device"), "{err}");
    }

    /// 未接线协议 → ConfigError 且消息点明 V1 支持面（绝不猜测意图）。
    #[tokio::test]
    async fn poll_unsupported_protocol_is_config_error() {
        let config = GatewayConfig::parse(&one_point_toml(
            "127.0.0.1:1".parse().expect("addr"),
            "opcua",
            "ns=2;s=Demo",
        ))
        .expect("parse");
        let handler = DevicePollHandler::from_config(&config);
        let err = handler
            .poll("dev-01", &["ns=2;s=Demo".to_string()])
            .await
            .expect_err("unsupported protocol must fail");
        assert!(matches!(err, DaemonError::ConfigError(_)), "{err:?}");
        let msg = err.to_string();
        assert!(msg.contains("not wired"), "{msg}");
        assert!(msg.contains("modbus-tcp"), "{msg}");
    }

    /// 真实链路：经调度器 `poll_group` 对 mock 从站发起 FC03 批量读 → 样本数 1，
    /// 从站收到 `ReadHoldingRegisters(0, 1)`。
    #[tokio::test]
    async fn handler_reads_mock_modbus_server_via_scheduler() {
        let state = Arc::new(Mutex::new(MockInner {
            holding: vec![0x1234],
            requests: Vec::new(),
        }));
        let connections = Arc::new(AtomicUsize::new(0));
        let addr = spawn_mock_server(Arc::clone(&state), Arc::clone(&connections)).await;

        let config =
            GatewayConfig::parse(&one_point_toml(addr, "modbus-tcp", "40001")).expect("parse");
        let handler = DevicePollHandler::from_config(&config);
        let group = GroupConfig::new("dev-01", Duration::from_secs(1), vec!["40001".to_string()])
            .expect("group");
        let scheduler = GroupScheduler::new(handler, vec![group]).expect("scheduler");

        let samples = scheduler.poll_group("dev-01").await.expect("poll");
        assert_eq!(samples, 1, "one readable point → one sample");
        assert_eq!(
            state.lock().expect("mock state").requests,
            vec![Request::ReadHoldingRegisters(0, 1)],
            "mock slave saw FC03 addr=0 count=1"
        );

        // 连接复用：第二轮不再新建连接。
        scheduler.poll_group("dev-01").await.expect("poll 2");
        assert_eq!(
            connections.load(Ordering::SeqCst),
            1,
            "second poll must reuse the device connection"
        );
    }

    /// 不可解析为南向地址的点位跳过（全跳过 → Ok(0)，不整体失败）。
    #[tokio::test]
    async fn poll_skips_unparseable_point_ids() {
        let state = Arc::new(Mutex::new(MockInner::default()));
        let connections = Arc::new(AtomicUsize::new(0));
        let addr = spawn_mock_server(state, connections).await;

        let config =
            GatewayConfig::parse(&one_point_toml(addr, "modbus-tcp", "p_temp")).expect("parse");
        let handler = DevicePollHandler::from_config(&config);
        let samples = handler
            .poll("dev-01", &["p_temp".to_string()])
            .await
            .expect("poll must not fail");
        assert_eq!(samples, 0, "unparseable point skipped → zero samples");
    }

    // ---- 生产 bootstrap 路径接线证明（D-12 核心：摘掉接线即失败） ----

    /// 生产 bootstrap 路径注入真实 handler 后：daemon 到达 Running，调度器按点位
    /// 周期对 mock 从站发起真实南向读（连接数 > 0），停机正常。修复前该路径恒
    /// `warn!("no poll handler configured")` → 从站 0 连接。
    #[tokio::test]
    async fn bootstrap_production_path_polls_mock_modbus_device() {
        let state = Arc::new(Mutex::new(MockInner {
            holding: vec![0x0001],
            requests: Vec::new(),
        }));
        let connections = Arc::new(AtomicUsize::new(0));
        let addr = spawn_mock_server(Arc::clone(&state), Arc::clone(&connections)).await;

        let dir = tempfile::tempdir().expect("tempdir");
        let config_path = dir.path().join("config.toml");
        std::fs::write(
            &config_path,
            format!(
                "[gateway]\ngateway_id = \"gw-poll\"\n\n[[points]]\ndevice_id = \"dev-01\"\n\
                 point_id = \"40001\"\nprotocol = \"modbus-tcp\"\naddress = \"{addr}\"\n\
                 frequency_ms = 50\n"
            ),
        )
        .expect("write config");

        let config = crate::config::GatewayConfig::load(&config_path).expect("load config");
        assert!(!config.points.is_empty(), "config declares one point");
        let shared = DaemonShared::new();
        let handler: Arc<dyn PollHandler> = Arc::new(DevicePollHandler::from_config(&config));
        let builder = BootstrapBuilder::new(&config_path)
            .with_shared(shared.clone())
            .without_signal_handlers()
            .with_poll_handler(handler);

        let run_handle = tokio::spawn(builder.run());

        // 等 Running（真实时间；装配毫秒级）。
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        while shared.state() != LifecycleState::Running {
            assert!(
                tokio::time::Instant::now() < deadline,
                "bootstrap did not reach Running"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }

        // 接线证明：调度器对 mock 从站发起真实连接与读取。
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while connections.load(Ordering::SeqCst) == 0 {
            assert!(
                tokio::time::Instant::now() < deadline,
                "scheduler never connected to the mock slave (poll handler not wired?)"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }

        shared.request_shutdown();
        let result = run_handle.await.expect("run task joins").expect("run ok");
        assert_eq!(result.state(), LifecycleState::Stopped);
    }
}

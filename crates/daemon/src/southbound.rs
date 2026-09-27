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
//!   批量读」（惰性建连 + 断线自愈走 [`crate::driver::Reconnector`]）；把回读的
//!   寄存器字节解码为带质量码 / 时间戳的样本（D-14 数据面接线：`poll` 返回
//!   **样本**而非计数，供 [`crate::dataplane::NorthDataPlane`] 消费）。
//! - **不做**：北向转发 / 离线队列（`dataplane` + bootstrap 的 north 装配链路
//!   负责）；公式求值（pipeline / formula 负责）；授权 / 降级状态的采集限制
//!   （Degraded 档位配额闸门在 `LicenseRuntime::enforce_free_limits` 与热重载
//!   准入闸门——本模块不重复实现、也绝不绕过）。
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
use std::sync::{Arc, RwLock as StdRwLock};
use std::time::Duration;

use async_trait::async_trait;
use protocol_proto::Quality;
use tracing::{info, warn};

use crate::codec::{DecodeSpec, ValueDecoder};
use crate::config::GatewayConfig;
use crate::driver::modbus::{ModbusConfig, ModbusDriver, ModbusFraming};
use crate::driver::{Driver, PointAddressParser, ReadPoint, Reconnector};
use crate::error::{DaemonError, DaemonResult};
use crate::pipeline::RawSample;
use crate::scheduler::PollHandler;
use crate::sim;

/// Modbus 单次请求超时（与 `ModbusConfig::default` 一致；显式声明便于运维口径统一）。
const SOUTHBOUND_REQUEST_TIMEOUT: Duration = Duration::from_secs(3);

/// 缺省南向端口（设备接入地址未写端口时使用，Modbus 标准端口）。
const DEFAULT_SOUTHBOUND_PORT: u16 = 502;

/// 每设备驱动连接：互斥锁内 `Box<dyn Driver>`（组任务串行使用本设备连接）。
type DeviceConn = Arc<tokio::sync::Mutex<Box<dyn Driver>>>;

/// 单设备的轮询计划：协议 + 接入地址 + 该设备全部点位标识 + 仿真规格。
#[derive(Debug, Clone)]
struct DevicePlan {
    /// 南向协议字面量（点位行 `protocol`，如 `modbus-tcp`）。
    protocol: String,
    /// 设备接入地址（点位行 `address`，如 `192.168.1.10:502`）。
    address: String,
    /// 该设备点位标识列表（首次出现顺序、去重）。
    point_ids: Vec<String>,
    /// 开启仿真的点位（`point_id -> 规格`）。**只有出现在此表的点位走仿真**，
    /// 其余点位仍走真实南向读 —— 仿真粒度是点位级，不是设备级。
    sims: HashMap<String, SimPlan>,
}

/// 点位仿真计划：已校验规格 / **必须显式报错的非法配置**。
///
/// 非法配置不做静默降级：`poll` 遇到 `Invalid` 即报 `ConfigError`（该组失败并
/// 告警），绝不偷偷退回真实读——否则「开着仿真却去连真设备」正是要消灭的假能力。
#[derive(Debug, Clone)]
enum SimPlan {
    /// 已校验规格。
    Ready(sim::SimSpec),
    /// 非法配置的真实原因（原样上报，含字段与约束）。
    Invalid(String),
}

/// 从配置点位表推导设备计划表（一组一设备；点位按首次出现顺序去重）。
///
/// 设备级协议 / 接入地址取该设备**首个**点位行。纯内存推导、不失败——协议 /
/// 地址非法性延迟到 `poll` 时按组显式报错（错误隔离，不阻断启动）。
/// 启动期 [`DevicePollHandler::from_config`] 与热重载
/// [`DevicePollHandler::refresh_devices`] 共用这一份推导，避免两条路径口径漂移。
fn derive_plans(config: &GatewayConfig) -> HashMap<String, DevicePlan> {
    // 设备级端点（让端点有唯一归属）：`device_id -> 设备登记段声明的 endpoint`。
    let device_endpoint: HashMap<&str, Option<&str>> = config
        .devices
        .iter()
        .map(|d| (d.device_id.as_str(), d.endpoint.as_deref()))
        .collect();
    let mut devices: HashMap<String, DevicePlan> = HashMap::new();
    for point in &config.points {
        // 端点归属链：点位级 `endpoint` → 设备级 `endpoint` → 点位 `address`（事实源）。
        let dev_ep = device_endpoint
            .get(point.device_id.as_str())
            .copied()
            .flatten();
        let ep = point
            .endpoint
            .as_deref()
            .filter(|s| !s.trim().is_empty())
            .or(dev_ep)
            .unwrap_or(point.address.as_str());
        let plan = devices
            .entry(point.device_id.clone())
            .or_insert_with(|| DevicePlan {
                protocol: point.protocol.clone(),
                address: ep.to_string(),
                point_ids: Vec::new(),
                sims: HashMap::new(),
            });
        if !plan.point_ids.contains(&point.point_id) {
            plan.point_ids.push(point.point_id.clone());
        }
        // 仿真点位登记（`sim_enabled = true` 才登记；未开启的点位不进此表 →
        // 走真实南向读，语义与配置开关严格一致）。
        if point.sim_enabled {
            let plan_sim = match sim::SimSpec::from_fields(
                point.sim_mode.as_deref(),
                point.sim_min,
                point.sim_max,
                point.sim_dec,
            ) {
                Ok(spec) => SimPlan::Ready(spec),
                Err(reason) => SimPlan::Invalid(reason),
            };
            plan.sims.insert(point.point_id.clone(), plan_sim);
        }
    }
    devices
}

/// 生产轮询动作：把调度器的组轮询翻译为南向驱动批量读（一组一设备一连接）。
///
/// 由 bin 装配入口构造并经 `BootstrapBuilder::with_poll_handler` 注入；多组共享
/// 本实例（内部 `Arc` 连接表），组间互不阻塞（每设备独立互斥锁）。
pub struct DevicePollHandler {
    /// 设备轮询计划（`device_id -> DevicePlan`；组名 = `device_id`）。
    ///
    /// 内部上锁而非构造期定死：配置热重载必须能整体换掉这张表，否则重载后
    /// 调度器认得新起的组、本表不认，新组每拍都 `unknown device group`。
    /// 锁内数据只在 `poll` 开头短持一次并拷出使用，**不跨 `await`**。
    devices: StdRwLock<HashMap<String, DevicePlan>>,
    /// 设备驱动连接表（惰性建连；`poll` 期间按设备锁串行，组间并行）。
    conns: tokio::sync::Mutex<HashMap<String, DeviceEndpoint>>,
    /// 仿真拍号（每次 `poll` 自增；`sim::SimSpec::value_at` 据此取确定性波形）。
    sim_tick: std::sync::atomic::AtomicU64,
}

/// 一条设备连接的登记信息：连接本体 + 建连时的接入快照。
///
/// 记下快照是为了热重载时能判断**既有连接是否已失效**：接入地址 / 协议变了就
/// 必须丢掉旧连接，否则下一拍会继续读旧端点（改造期静默读到错设备的数据）。
struct DeviceEndpoint {
    /// 建连时的协议（与 [`DevicePlan::protocol`] 同口径）。
    protocol: String,
    /// 建连时的接入地址（与 [`DevicePlan::address`] 同口径）。
    address: String,
    /// 驱动连接本体（按设备互斥，组内串行）。
    conn: DeviceConn,
}

impl DevicePollHandler {
    /// 从配置点位表构建轮询计划（一组一设备；点位按首次出现顺序去重）。
    ///
    /// 设备级协议 / 接入地址取该设备首个点位行；纯内存构建、不失败——协议 /
    /// 地址非法性延迟到 `poll` 时按组显式报错（错误隔离，不阻断启动）。
    #[must_use]
    pub fn from_config(config: &GatewayConfig) -> Self {
        Self {
            devices: StdRwLock::new(derive_plans(config)),
            conns: tokio::sync::Mutex::new(HashMap::new()),
            sim_tick: std::sync::atomic::AtomicU64::new(0),
        }
    }

    /// 配置是否声明了任何点位（bin 装配判据：无点位不注入 handler，避免噪音告警）。
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.devices
            .read()
            .map(|plans| plans.is_empty())
            .unwrap_or(true)
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
        if let Some(endpoint) = conns.get(group) {
            return Ok(Arc::clone(&endpoint.conn));
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
        conns.insert(
            group.to_string(),
            DeviceEndpoint {
                protocol: plan.protocol.clone(),
                address: plan.address.clone(),
                conn: Arc::clone(&conn),
            },
        );
        Ok(conn)
    }
}

#[async_trait]
impl PollHandler for DevicePollHandler {
    async fn refresh_devices(&self, config: &GatewayConfig) {
        // 计划表：整体换成新配置推导结果（只有这张表持有「哪些设备 / 哪些点位」的
        // 派生状态，重载不换它，新组就会一直 `unknown device group`）。
        let plans = derive_plans(config);
        match self.devices.write() {
            Ok(mut guard) => *guard = plans.clone(),
            Err(poisoned) => *poisoned.into_inner() = plans.clone(),
        }

        // 连接表：摘掉僵尸与失效连接，保留既有连接（见 `refresh_devices` 契约）。
        let mut conns = self.conns.lock().await;
        let dropped: Vec<String> = conns
            .iter()
            .filter(|(device_id, endpoint)| match plans.get(*device_id) {
                // 设备没了 → 僵尸连接；地址 / 协议变了 → 旧连接指向旧端点，按新端点重连。
                Some(plan) => {
                    plan.protocol != endpoint.protocol || plan.address != endpoint.address
                }
                None => true,
            })
            .map(|(device_id, _)| device_id.clone())
            .collect();
        for device_id in dropped {
            conns.remove(&device_id);
            info!(
                device_id = %device_id,
                "southbound: device connection dropped (device removed or re-addressed by reload)"
            );
        }
    }

    async fn poll(&self, group: &str, point_ids: &[String]) -> DaemonResult<Vec<RawSample>> {
        // 计划表短持读锁 → 拷出本拍所需字段即释放（锁绝不跨 `await`，热路径无阻塞）。
        let (protocol, address, plan_point_ids, sims): (
            String,
            String,
            Vec<String>,
            HashMap<String, SimPlan>,
        ) = {
            let guard = match self.devices.read() {
                Ok(guard) => guard,
                Err(poisoned) => poisoned.into_inner(),
            };
            match guard.get(group) {
                Some(plan) => (
                    plan.protocol.clone(),
                    plan.address.clone(),
                    plan.point_ids.clone(),
                    plan.sims.clone(),
                ),
                None => {
                    return Err(DaemonError::ConfigError(format!(
                        "southbound poll: unknown device group {group:?}"
                    )))
                }
            }
        };
        if plan_point_ids.is_empty() {
            return Ok(Vec::new());
        }

        // ---- 点位分区：仿真点（不碰南向） vs 真实点（走驱动批量读）----
        // 未知点位跳过并告警（与原语义一致）；非法仿真配置**显式失败**不降级。
        let mut sim_hits: Vec<String> = Vec::new();
        let mut real_hits: Vec<String> = Vec::new();
        for point_id in point_ids {
            if !plan_point_ids.contains(point_id) {
                warn!(
                    group = %group,
                    point_id = %point_id,
                    "southbound poll: point id missing from the device plan; skipped"
                );
                continue;
            }
            match sims.get(point_id) {
                Some(SimPlan::Ready(_)) => sim_hits.push(point_id.clone()),
                Some(SimPlan::Invalid(reason)) => {
                    return Err(DaemonError::ConfigError(format!(
                        "southbound poll: point {point_id:?} has invalid simulation config: {reason}"
                    )))
                }
                None => real_hits.push(point_id.clone()),
            }
        }

        // ---- 仿真点：本拍由引擎合成（质量码 GOOD / 无设备时间戳，与真实读同口径）----
        let tick = self
            .sim_tick
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let mut out: Vec<RawSample> = Vec::with_capacity(sim_hits.len() + real_hits.len());
        for point_id in &sim_hits {
            if let Some(SimPlan::Ready(spec)) = sims.get(point_id) {
                out.push(RawSample {
                    source_id: point_id.clone(),
                    value: spec.value_at(sim::seed_for(point_id), tick),
                    quality: Quality::Good,
                    device_ts_ns: None,
                });
            }
        }

        // 全部为仿真点 → **不建连、不发起任何南向请求**（仿真的意义即在此）。
        if real_hits.is_empty() {
            return Ok(out);
        }

        let plan = DevicePlan {
            protocol,
            address,
            point_ids: plan_point_ids,
            sims,
        };

        // 惰性建连（连接表只在取/插时短持锁；读期间按设备锁串行，组间并行）。
        let conn = self.get_or_connect(group, &plan).await?;
        let mut driver = conn.lock().await;

        // 组内批量读：一次驱动请求承载全部可读点位（PollHandler 契约）。
        // 未知点位 / 不可解析为南向地址的点位跳过并告警（不整体失败）。
        // 记录点位标识与请求的下标对应关系，回读后逐一解码。
        let mut read_points: Vec<ReadPoint> = Vec::with_capacity(real_hits.len());
        let mut read_ids: Vec<String> = Vec::with_capacity(real_hits.len());
        for point_id in &real_hits {
            match PointAddressParser::parse(point_id) {
                Ok(address) => {
                    read_points.push(ReadPoint { address, count: 1 });
                    read_ids.push(point_id.clone());
                }
                Err(err) => warn!(
                    group = %group,
                    point_id = %point_id,
                    error = %err,
                    "southbound poll: point id is not a parseable southbound address; skipped"
                ),
            }
        }
        if read_points.is_empty() {
            // 真实点全部不可解析 → 至少把已合成的仿真样本交出去（不整体失败）。
            return Ok(out);
        }
        let samples = driver.read(&read_points).await?;

        // 字节 → 数值解码（D-14）：保持寄存器负载按缺省解码规格（uint16 / ABCD
        // 大端，与驱动 `to_be_bytes` 装载一致）解码为工程量值；成功读到的样本
        // 质量码 = GOOD（读取失败时整个 `read` 报错，无部分结果语义）；
        // 设备不提供时间戳 → `device_ts_ns = None`（统一由采集时刻承载）。
        // 单个点位解码失败只跳过该点（结构性配置错误须可观测，不静默吞值）。
        let decoder = ValueDecoder::new(DecodeSpec::default())?;
        for (sample, point_id) in samples.into_iter().zip(read_ids) {
            match decoder.decode(&sample.value) {
                Ok(decoded) => out.push(RawSample {
                    source_id: point_id,
                    value: decoded.scaled,
                    quality: Quality::Good,
                    device_ts_ns: None,
                }),
                Err(err) => warn!(
                    group = %group,
                    point_id = %point_id,
                    error = %err,
                    "southbound poll: sample decode failed; point skipped"
                ),
            }
        }
        Ok(out)
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
        let plans = handler.devices.read().expect("plan table lock").clone();
        assert_eq!(plans.len(), 2, "one plan per device");
        let a = &plans["dev-a"];
        assert_eq!(a.protocol, "modbus-tcp");
        assert_eq!(a.address, "10.0.0.1:502");
        assert_eq!(a.point_ids, vec!["40001".to_string()], "deduped");

        let empty = DevicePollHandler::from_config(&GatewayConfig::default());
        assert!(empty.is_empty(), "no points → empty plan");
    }

    /// 热重载 `refresh_devices`：新设备进表、被删设备出表、点位增减生效。
    ///
    /// 这是「重载后 `unknown device group` 必现」的直接修复点：调度器侧重建了组，
    /// 南向计划表也必须同步，否则新组每拍都撞同一个错。
    #[tokio::test]
    async fn refresh_devices_replaces_plan_table_on_hot_reload() {
        let old = GatewayConfig::parse(
            "[[points]]\ndevice_id = \"dev-a\"\npoint_id = \"40001\"\n\
             protocol = \"modbus-tcp\"\naddress = \"10.0.0.1:502\"\n\n\
             [[points]]\ndevice_id = \"dev-b\"\npoint_id = \"40002\"\n\
             protocol = \"modbus-tcp\"\naddress = \"10.0.0.2:502\"\n",
        )
        .expect("parse");
        let handler = DevicePollHandler::from_config(&old);

        // 新配置：dev-b 删除，dev-a 多挂一个点位，dev-c 全新出现。
        let next = GatewayConfig::parse(
            "[[points]]\ndevice_id = \"dev-a\"\npoint_id = \"40001\"\n\
             protocol = \"modbus-tcp\"\naddress = \"10.0.0.1:502\"\n\n\
             [[points]]\ndevice_id = \"dev-a\"\npoint_id = \"40003\"\n\
             protocol = \"modbus-tcp\"\naddress = \"10.0.0.1:502\"\n\n\
             [[points]]\ndevice_id = \"dev-c\"\npoint_id = \"40005\"\n\
             protocol = \"modbus-tcp\"\naddress = \"10.0.0.3:502\"\n",
        )
        .expect("parse");
        handler.refresh_devices(&next).await;

        let plans = handler.devices.read().expect("plan table lock").clone();
        assert!(
            !plans.contains_key("dev-b"),
            "deleted device must leave the plan table (else zombie polling)"
        );
        assert!(plans.contains_key("dev-c"), "new device must appear");
        assert_eq!(
            plans["dev-a"].point_ids,
            vec!["40001".to_string(), "40003".to_string()],
            "added point must be picked up"
        );

        // 新设备立即可轮询（不再 `unknown device group`）。
        let err = handler
            .poll("dev-c", &["40005".to_string()])
            .await
            .expect_err("mock-less device fails to connect");
        assert!(
            !err.to_string().contains("unknown device group"),
            "plan table must already know dev-c, got: {err}"
        );
    }

    /// `refresh_devices` 保留同名设备的既有连接，只摘掉失效连接。
    ///
    /// 断言口径走 `conns` 的可观测副作用：被删设备的连接条目必须消失（僵尸南向
    /// 连接），同名设备的条目原样保留（热重载不重建连接 = 不断流）。
    #[tokio::test]
    async fn refresh_devices_keeps_connections_of_unchanged_devices() {
        let state = Arc::new(Mutex::new(MockInner {
            holding: vec![0x1234, 0x5678],
            requests: Vec::new(),
        }));
        let connections = Arc::new(AtomicUsize::new(0));
        let addr = spawn_mock_server(Arc::clone(&state), Arc::clone(&connections)).await;

        let two_devices = |addr: SocketAddr| {
            format!(
                "[[points]]\ndevice_id = \"dev-a\"\npoint_id = \"40001\"\n\
                 protocol = \"modbus-tcp\"\naddress = \"{addr}\"\nfrequency_ms = 100\n\n\
                 [[points]]\ndevice_id = \"dev-b\"\npoint_id = \"40002\"\n\
                 protocol = \"modbus-tcp\"\naddress = \"{addr}\"\nfrequency_ms = 100\n"
            )
        };
        let handler = DevicePollHandler::from_config(
            &GatewayConfig::parse(&two_devices(addr)).expect("parse"),
        );
        // 两台设备各建一条真实连接（mock 从站可答）。
        handler
            .poll("dev-a", &["40001".to_string()])
            .await
            .expect("poll dev-a");
        handler
            .poll("dev-b", &["40002".to_string()])
            .await
            .expect("poll dev-b");
        assert_eq!(
            connections.load(Ordering::SeqCst),
            2,
            "both devices must have dialed once"
        );

        // dev-a 完全不变 → 连接原样保留；dev-b 接入地址改了 → 旧连接失效，按新端点重连。
        let re_addressed = GatewayConfig::parse(&format!(
            "[[points]]\ndevice_id = \"dev-a\"\npoint_id = \"40001\"\n\
             protocol = \"modbus-tcp\"\naddress = \"{addr}\"\nfrequency_ms = 100\n\n\
             [[points]]\ndevice_id = \"dev-b\"\npoint_id = \"40002\"\n\
             protocol = \"modbus-tcp\"\naddress = \"127.0.0.1:2\"\nfrequency_ms = 100\n"
        ))
        .expect("parse");
        handler.refresh_devices(&re_addressed).await;

        let conns = handler.conns.lock().await;
        assert!(
            conns.contains_key("dev-a"),
            "unchanged device keeps its live connection (no re-dial on reload)"
        );
        assert!(
            !conns.contains_key("dev-b"),
            "re-addressed device drops the stale connection and re-dials"
        );
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
    /// 从站收到 `ReadHoldingRegisters(0, 1)`；直连 `poll` 断言样本携带解码值
    /// （寄存器 0x1234 → 4660.0，uint16 / 大端）与 GOOD 质量码（D-14：poll 产样本）。
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

        // 直连 poll：样本本体携带解码值与质量码（不只是计数）。
        let raw = handler
            .poll("dev-01", &["40001".to_string()])
            .await
            .expect("poll");
        assert_eq!(raw.len(), 1, "one decoded sample");
        let first = &raw[0];
        assert_eq!(first.source_id, "40001");
        assert!((first.value - 4660.0).abs() < 1e-9, "0x1234 → 4660.0");
        assert_eq!(first.quality, Quality::Good);
        assert_eq!(
            first.device_ts_ns, None,
            "modbus carries no device timestamp"
        );

        let group = GroupConfig::new("dev-01", Duration::from_secs(1), vec!["40001".to_string()])
            .expect("group");
        let scheduler = GroupScheduler::new(handler, vec![group]).expect("scheduler");
        let samples = scheduler.poll_group("dev-01").await.expect("poll");
        assert_eq!(samples, 1, "one readable point → one sample");
        assert_eq!(
            state.lock().expect("mock state").requests.first(),
            Some(&Request::ReadHoldingRegisters(0, 1)),
            "mock slave saw FC03 addr=0 count=1"
        );

        // 连接复用：直连 poll 已建连，调度器路径不再新建连接。
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
        assert_eq!(samples.len(), 0, "unparseable point skipped → zero samples");
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

    // ---- 仿真源（sim_* 执行体）----

    /// 单设备单仿真点位的配置 TOML（地址故意指向不可达端口：仿真点**绝不应建连**）。
    fn sim_point_toml(extra: &str) -> String {
        format!(
            "[[points]]\ndevice_id = \"simdev\"\npoint_id = \"40001\"\n\
             protocol = \"modbus-tcp\"\naddress = \"127.0.0.1:1\"\nfrequency_ms = 100\n\
             sim_enabled = true\n{extra}"
        )
    }

    /// 仿真点位：合成样本、**不发起任何南向连接**（地址不可达也照样出数）。
    #[tokio::test]
    async fn sim_point_polls_without_touching_southbound() {
        let config = GatewayConfig::parse(&sim_point_toml(
            "sim_mode = \"random\"\nsim_min = 0.0\nsim_max = 100.0\nsim_dec = 2\n",
        ))
        .expect("parse");
        let handler = DevicePollHandler::from_config(&config);

        let samples = handler
            .poll("simdev", &["40001".to_string()])
            .await
            .expect("sim poll must succeed without any device");
        assert_eq!(samples.len(), 1, "one sim point → one sample");
        assert_eq!(samples[0].source_id, "40001");
        assert_eq!(samples[0].quality, Quality::Good);
        assert!(samples[0].device_ts_ns.is_none(), "sim has no device timestamp");
        assert!(
            (0.0..=100.0).contains(&samples[0].value),
            "value {} outside [sim_min, sim_max]",
            samples[0].value
        );

        // 关键断言：仿真路径**没有**留下任何南向连接（地址不可达，一旦建连必失败）。
        assert!(
            handler.conns.lock().await.is_empty(),
            "simulation must not dial the southbound device"
        );
    }

    /// `fixed` 波形恒为 `sim_min`，且小数位按 `sim_dec` 收敛。
    #[tokio::test]
    async fn sim_fixed_mode_is_constant_min() {
        let config = GatewayConfig::parse(&sim_point_toml(
            "sim_mode = \"fixed\"\nsim_min = 12.345\nsim_max = 99.0\nsim_dec = 2\n",
        ))
        .expect("parse");
        let handler = DevicePollHandler::from_config(&config);
        for _ in 0..3 {
            let samples = handler
                .poll("simdev", &["40001".to_string()])
                .await
                .expect("sim poll");
            assert_eq!(samples[0].value, 12.35);
        }
    }

    /// 拍号自增：`random` 波形逐拍变化（不是恒定值冒充随机）。
    #[tokio::test]
    async fn sim_random_advances_every_tick() {
        let config = GatewayConfig::parse(&sim_point_toml(
            "sim_mode = \"random\"\nsim_min = 0.0\nsim_max = 1000.0\nsim_dec = 3\n",
        ))
        .expect("parse");
        let handler = DevicePollHandler::from_config(&config);
        let mut values = Vec::new();
        for _ in 0..6 {
            let samples = handler
                .poll("simdev", &["40001".to_string()])
                .await
                .expect("sim poll");
            values.push(samples[0].value);
        }
        assert!(
            values.windows(2).any(|w| w[0] != w[1]),
            "random sim must advance per tick, got {values:?}"
        );
    }

    /// 非法波形 → 该组 `ConfigError`（**绝不静默退回真实读**）。
    #[tokio::test]
    async fn sim_unknown_mode_fails_closed() {
        let config = GatewayConfig::parse(&sim_point_toml("sim_mode = \"sine\"\n"))
            .expect("parse");
        let handler = DevicePollHandler::from_config(&config);
        let err = handler
            .poll("simdev", &["40001".to_string()])
            .await
            .expect_err("unknown sim_mode must fail");
        assert!(matches!(err, DaemonError::ConfigError(_)), "{err:?}");
        let msg = err.to_string();
        assert!(msg.contains("invalid simulation config"), "{msg}");
        assert!(msg.contains("sine"), "{msg}");
        assert!(msg.contains("random"), "{msg}");
        // 失败即失败：不建连、不产出样本。
        assert!(handler.conns.lock().await.is_empty());
    }

    /// 区间倒置 / 非有限值 → `ConfigError`（同样 fail-closed）。
    #[tokio::test]
    async fn sim_inverted_range_fails_closed() {
        let config = GatewayConfig::parse(&sim_point_toml(
            "sim_mode = \"random\"\nsim_min = 10.0\nsim_max = 1.0\n",
        ))
        .expect("parse");
        let handler = DevicePollHandler::from_config(&config);
        let err = handler
            .poll("simdev", &["40001".to_string()])
            .await
            .expect_err("inverted range must fail");
        assert!(matches!(err, DaemonError::ConfigError(_)), "{err:?}");
        assert!(err.to_string().contains("sim_min"), "{err}");
    }

    /// 同一设备内「仿真点 + 真实点」共存：仿真点走合成、真实点走 mock 从站读，
    /// 两路样本一并返回（仿真粒度是点位级，不是设备级）。
    #[tokio::test]
    async fn sim_and_real_points_coexist() {
        let state = Arc::new(Mutex::new(MockInner {
            // `40001` → 保持寄存器偏移 0；`40002` → 偏移 1（5 位 Modbus 编址）。
            holding: vec![0x1234, 0x5678],
            requests: Vec::new(),
        }));
        let connections = Arc::new(AtomicUsize::new(0));
        let addr = spawn_mock_server(Arc::clone(&state), Arc::clone(&connections)).await;

        let config = GatewayConfig::parse(&format!(
            "[[points]]\ndevice_id = \"mixed\"\npoint_id = \"40001\"\n\
             protocol = \"modbus-tcp\"\naddress = \"{addr}\"\nfrequency_ms = 100\n\
             sim_enabled = true\nsim_mode = \"fixed\"\nsim_min = 7.0\nsim_max = 7.0\n\n\
             [[points]]\ndevice_id = \"mixed\"\npoint_id = \"40002\"\n\
             protocol = \"modbus-tcp\"\naddress = \"{addr}\"\nfrequency_ms = 100\n"
        ))
        .expect("parse");
        let handler = DevicePollHandler::from_config(&config);

        let samples = handler
            .poll("mixed", &["40001".to_string(), "40002".to_string()])
            .await
            .expect("mixed poll");
        assert_eq!(samples.len(), 2, "both points must yield a sample: {samples:?}");
        let sim = samples
            .iter()
            .find(|s| s.source_id == "40001")
            .expect("sim sample");
        assert_eq!(sim.value, 7.0, "sim point uses the simulator");
        let real = samples
            .iter()
            .find(|s| s.source_id == "40002")
            .expect("real sample");
        assert_eq!(real.value, 22136.0, "real point still reads the slave (0x5678)");
        assert_eq!(
            connections.load(Ordering::SeqCst),
            1,
            "exactly one dial (driven by the real point only)"
        );
    }

    /// 计划表：只有 `sim_enabled = true` 的点位进仿真表（其余点位保持真实读）。
    #[test]
    fn plan_table_records_only_sim_enabled_points() {
        let config = GatewayConfig::parse(
            "[[points]]\ndevice_id = \"dev-a\"\npoint_id = \"40001\"\n\
             protocol = \"modbus-tcp\"\naddress = \"10.0.0.1:502\"\n\n\
             [[points]]\ndevice_id = \"dev-a\"\npoint_id = \"40002\"\n\
             protocol = \"modbus-tcp\"\naddress = \"10.0.0.1:502\"\n\
             sim_enabled = true\nsim_mode = \"fixed\"\n",
        )
        .expect("parse");
        let handler = DevicePollHandler::from_config(&config);
        let plans = handler.devices.read().expect("plan lock").clone();
        let plan = &plans["dev-a"];
        assert_eq!(plan.point_ids.len(), 2);
        assert_eq!(plan.sims.len(), 1, "only the sim-enabled point is registered");
        assert!(matches!(plan.sims.get("40002"), Some(SimPlan::Ready(_))));
        assert!(!plan.sims.contains_key("40001"));
    }
}

//! D-14 数据面接线**端到端验收**：mock Modbus 从站（写入已知值）→ 生产装配路径
//! （`assemble_north_runtime` + `BootstrapBuilder::with_poll_handler` +
//! `with_north_runtime`，与 bin `iot-daq-daemon.rs` main 完全同构）→ mock MQTT
//! broker 断言收到 PUBLISH，且 payload 能解码出已知值。
//!
//! 数据流被验的每一跳：
//! 1. bootstrap 把 [`DevicePollHandler`] 包进 [`daemon::dataplane::NorthDataPlane`]
//!    （生产装配在 bootstrap 内部完成——摘掉接线本测试即失败）；
//! 2. 调度器按点位周期驱动南向 FC03 批量读 → 样本（解码值 4660.0 + GOOD）；
//! 3. 管线（质量码归一 / 死区直通）→ `TelemetryBatch` → 按出口 `encoding` 编码；
//! 4. `NorthRuntime::submit` → 发送队列 → 驱动任务 pump → 真实 TCP PUBLISH。
//!
//! 覆盖场景：
//! 1. **Happy（JSON 出口）**：broker 真实收到 QoS1 PUBLISH，payload 可解码出
//!    `device_id/point_id/value=4660.0/quality=GOOD`（语义字段断言）；
//! 2. **授权装配失败（Degraded 闸门）**：样本照常采集并投递发送队列
//!    （`admitted`/`spilled` 增长、水位有界），驱动拍全部被 `NorthForwardGate`
//!    拒绝（`gated_cycles` 增长、**0 PUBLISH**）——证明数据面在 gate 下游、
//!    Degraded「无北向转发」语义零改动；
//! 3. **无 `[[outlets]]`**：北向不启动、不建 queue.db，采集照常（既有行为）。
//!
//! **计时纪律**：真实 TCP 往返使用有界 deadline 轮询，不比较绝对耗时。

use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

use daemon::bootstrap::{
    assemble_north_runtime, offline_flush_hook, BootstrapBuilder, DaemonShared, LifecycleState,
};
use daemon::config::GatewayConfig;
use daemon::error::DaemonResult;
use daemon::north::encoder::{decode_batch, Encoding};
use daemon::scheduler::PollHandler;
use daemon::southbound::DevicePollHandler;

// ============================================================================
// mock Modbus 从站（FC03 保持寄存器；与 southbound 单测同款最小实现）
// ============================================================================

#[derive(Debug, Default)]
struct ModbusInner {
    holding: Vec<u16>,
}

#[derive(Clone)]
struct ModbusService {
    state: Arc<Mutex<ModbusInner>>,
}

impl tokio_modbus::server::Service for ModbusService {
    type Request = tokio_modbus::Request<'static>;
    type Response = tokio_modbus::Response;
    type Exception = tokio_modbus::ExceptionCode;
    type Future = std::future::Ready<
        std::result::Result<tokio_modbus::Response, tokio_modbus::ExceptionCode>,
    >;

    fn call(&self, req: Self::Request) -> Self::Future {
        let state = self.state.lock().expect("modbus mock state lock");
        let resp = match req {
            tokio_modbus::Request::ReadHoldingRegisters(addr, cnt) => {
                let end = addr as usize + cnt as usize;
                if end > state.holding.len() {
                    return std::future::ready(Err(
                        tokio_modbus::ExceptionCode::IllegalDataAddress,
                    ));
                }
                tokio_modbus::Response::ReadHoldingRegisters(
                    state.holding[addr as usize..end].to_vec(),
                )
            }
            _ => return std::future::ready(Err(tokio_modbus::ExceptionCode::IllegalFunction)),
        };
        std::future::ready(Ok(resp))
    }
}

/// 启动 mock 从站：返回监听地址（`connections` 累计被接受的连接数）。
async fn spawn_mock_modbus(holding: Vec<u16>, connections: Arc<AtomicUsize>) -> SocketAddr {
    let state = Arc::new(Mutex::new(ModbusInner { holding }));
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind modbus mock");
    let addr = listener.local_addr().expect("modbus local addr");
    tokio::spawn(async move {
        let server = tokio_modbus::server::tcp::Server::new(listener);
        let on_connected = move |stream: tokio::net::TcpStream, socket_addr: SocketAddr| {
            connections.fetch_add(1, Ordering::SeqCst);
            let state = Arc::clone(&state);
            async move {
                tokio_modbus::server::tcp::accept_tcp_connection(stream, socket_addr, move |_| {
                    Ok(Some(ModbusService {
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

// ============================================================================
// mock MQTT broker（localhost 真实 TCP；捕获 QoS ≥ 1 PUBLISH 的 payload 字节）
// ============================================================================

/// mock broker 句柄：地址 + QoS1 PUBLISH 计数 + payload 捕获 + 恢复开关。
struct MockBroker {
    addr: SocketAddr,
    publishes: Arc<AtomicUsize>,
    payloads: Arc<Mutex<Vec<Vec<u8>>>>,
    resume: Arc<AtomicBool>,
}

impl MockBroker {
    fn resume(&self) {
        self.resume.store(true, Ordering::SeqCst);
    }

    fn publish_count(&self) -> usize {
        self.publishes.load(Ordering::SeqCst)
    }

    /// 已捕获的 payload 快照。
    fn captured(&self) -> Vec<Vec<u8>> {
        self.payloads.lock().expect("payload lock").clone()
    }
}

/// 读一个 MQTT 定长头包（支持变长 remaining length）。
async fn read_mqtt_packet<R: tokio::io::AsyncRead + Unpin>(
    stream: &mut R,
) -> Option<(u8, Vec<u8>)> {
    let mut header = [0u8; 1];
    stream.read_exact(&mut header).await.ok()?;
    let opcode = header[0];
    let mut len: usize = 0;
    let mut mult: usize = 1;
    loop {
        let mut byte = [0u8; 1];
        stream.read_exact(&mut byte).await.ok()?;
        len += usize::from(byte[0] & 0x7F) * mult;
        if byte[0] & 0x80 == 0 {
            break;
        }
        mult *= 128;
    }
    let mut body = vec![0u8; len];
    if len > 0 {
        stream.read_exact(&mut body).await.ok()?;
    }
    Some((opcode, body))
}

/// 起一个本地明文 mock broker（accept 循环；每连接一个任务）。
///
/// 暂停期接受连接但不应答（模拟断连）；恢复后应答 CONNECT / PUBLISH / PINGREQ，
/// 并每 100ms 注入一条 QoS0 心跳（保持客户端事件循环流动，与 north_wiring 同口径）。
async fn spawn_mock_broker() -> MockBroker {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind broker mock");
    let addr = listener.local_addr().expect("broker local addr");
    let publishes = Arc::new(AtomicUsize::new(0));
    let payloads = Arc::new(Mutex::new(Vec::new()));
    let resume = Arc::new(AtomicBool::new(false));
    let pubs = Arc::clone(&publishes);
    let captured = Arc::clone(&payloads);
    let resume_flag = Arc::clone(&resume);
    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                break;
            };
            let pubs = Arc::clone(&pubs);
            let captured = Arc::clone(&captured);
            let resume = Arc::clone(&resume_flag);
            tokio::spawn(async move {
                let (mut rd, wr) = tokio::io::split(stream);
                let wr = Arc::new(tokio::sync::Mutex::new(wr));

                // 暂停期：等待恢复开关（连接已建立、CONNECT 已缓冲）。
                while !resume.load(Ordering::SeqCst) {
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }

                // 心跳注入：每 100ms 一条 QoS0 PUBLISH（不计入 publishes）。
                {
                    let wr = Arc::clone(&wr);
                    tokio::spawn(async move {
                        let mut seq = 0u8;
                        loop {
                            tokio::time::sleep(Duration::from_millis(100)).await;
                            let pkt = [0x30u8, 0x04, 0x00, 0x01, b't', seq];
                            let mut guard = wr.lock().await;
                            if guard.write_all(&pkt).await.is_err() {
                                return;
                            }
                            seq = seq.wrapping_add(1);
                        }
                    });
                }

                loop {
                    let Some((opcode, body)) = read_mqtt_packet(&mut rd).await else {
                        return;
                    };
                    match opcode & 0xF0 {
                        0x10 => {
                            // CONNECT → CONNACK（session present = 0, rc = 0）。
                            let mut guard = wr.lock().await;
                            let _ = guard.write_all(&[0x20, 0x02, 0x00, 0x00]).await;
                        }
                        0x30 => {
                            let qos = (opcode >> 1) & 0x03;
                            if qos >= 1 && body.len() >= 4 {
                                let topic_len = u16::from_be_bytes([body[0], body[1]]) as usize;
                                if body.len() >= topic_len + 4 {
                                    // 捕获 payload（QoS ≥ 1 才计数：客户端真实批次）。
                                    captured
                                        .lock()
                                        .expect("payload lock")
                                        .push(body[4 + topic_len..].to_vec());
                                    pubs.fetch_add(1, Ordering::SeqCst);
                                    let pid = [body[2 + topic_len], body[3 + topic_len]];
                                    let mut guard = wr.lock().await;
                                    let _ = guard.write_all(&[0x40, 0x02, pid[0], pid[1]]).await;
                                }
                            }
                        }
                        0xC0 => {
                            let mut guard = wr.lock().await;
                            let _ = guard.write_all(&[0xD0, 0x00]).await;
                        }
                        0xE0 => return, // DISCONNECT
                        _ => {}
                    }
                }
            });
        }
    });
    MockBroker {
        addr,
        publishes,
        payloads,
        resume,
    }
}

// ============================================================================
// 装配夹具（与 bin main ②-b → ④ 完全同构：poll handler + north runtime）
// ============================================================================

/// 测试配置：一个 modbus 点位 + 一个 JSON 编码出口（`with_outlet` 可去掉出口段）。
fn config_toml(
    broker: Option<SocketAddr>,
    modbus_addr: SocketAddr,
    data_dir: &std::path::Path,
) -> String {
    let outlet = match broker {
        Some(addr) => format!(
            "\n[[outlets]]\nname = \"north-1\"\nbroker = \"mqtt://{addr}\"\nqos = 1\n\
             encoding = \"json\"\n"
        ),
        None => String::new(),
    };
    format!(
        "[gateway]\n\
         gateway_id = \"gw-data-plane\"\n\
         data_dir = \"{}\"\n\
         {outlet}\n\
         [[points]]\n\
         device_id = \"dev-01\"\n\
         point_id = \"40001\"\n\
         protocol = \"modbus-tcp\"\n\
         address = \"{modbus_addr}\"\n\
         frequency_ms = 50\n",
        // TOML 基本字符串：Windows 路径反斜杠转正斜杠（Windows 原生接受）。
        data_dir.display().to_string().replace('\\', "/")
    )
}

/// 按生产装配路径启动 daemon（与 bin main 一一对应；驱动拍 50ms 加速收敛）。
///
/// `assembly_failed_reason`：`Some` 时注入 `with_license_assembly_failed`
/// （fail-closed 分支：北向出口挂恒拒绝闸门，模拟 Degraded 无北向转发）。
async fn start_daemon(
    toml_text: &str,
    dir: &std::path::Path,
    assembly_failed_reason: Option<&str>,
) -> (
    DaemonShared,
    tokio::task::JoinHandle<DaemonResult<DaemonShared>>,
    Option<Arc<daemon::offline_queue::OfflineQueue>>,
) {
    let config = GatewayConfig::parse(toml_text).expect("parse config");
    let assembly = assemble_north_runtime(&config).expect("north assembly must not fail");
    let config_path = dir.join("config.toml");
    std::fs::write(&config_path, toml_text).expect("write config");

    let shared = DaemonShared::new();
    // 生产装配：配置声明了点位 → 构造真实南向轮询动作（bin ②-b 同构）。
    let handler: Option<Arc<dyn PollHandler>> = if config.points.is_empty() {
        None
    } else {
        Some(Arc::new(DevicePollHandler::from_config(&config)))
    };
    let mut builder = BootstrapBuilder::new(&config_path)
        .with_shared(shared.clone())
        .without_signal_handlers();
    if let Some(handler) = handler {
        builder = builder.with_poll_handler(handler);
    }
    if let Some(assembly) = assembly {
        builder = builder
            .with_north_runtime(assembly.runtime_config.with_tick(Duration::from_millis(50)))
            .with_offline_flusher(offline_flush_hook(Arc::clone(&assembly.queue)));
        builder = match assembly_failed_reason {
            Some(reason) => builder.with_license_assembly_failed(reason),
            None => builder,
        };
        let queue = Arc::clone(&assembly.queue);
        let handle = tokio::spawn(builder.run());
        wait_until_running(&shared).await;
        return (shared, handle, Some(queue));
    }
    let handle = tokio::spawn(builder.run());
    wait_until_running(&shared).await;
    (shared, handle, None)
}

/// 有界等待共享态到达 Running。
async fn wait_until_running(shared: &DaemonShared) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while shared.state() != LifecycleState::Running {
        assert!(Instant::now() < deadline, "daemon never reached Running");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// 有界轮询直到谓词成立（真实 TCP 往返口径）。
async fn wait_until(what: &str, timeout: Duration, mut predicate: impl FnMut() -> bool) {
    let deadline = Instant::now() + timeout;
    while !predicate() {
        assert!(
            Instant::now() < deadline,
            "condition not met within {timeout:?}: {what}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

// ============================================================================
// 场景 1（Happy）：mock 从站已知值 → 生产装配 → broker PUBLISH → JSON 解码断言
// ============================================================================

#[tokio::test]
async fn collected_samples_flow_to_broker_and_decode_to_known_value() {
    // 从站保持寄存器 0 = 0x1234（uint16 大端解码 → 4660.0）。
    let modbus_conns = Arc::new(AtomicUsize::new(0));
    let modbus_addr = spawn_mock_modbus(vec![0x1234], Arc::clone(&modbus_conns)).await;
    let broker = spawn_mock_broker().await;
    broker.resume();

    let dir = tempfile::tempdir().expect("tempdir");
    let toml = config_toml(Some(broker.addr), modbus_addr, dir.path());
    let (shared, handle, _queue) = start_daemon(&toml, dir.path(), None).await;

    let runtime = shared
        .north_runtime()
        .expect("north runtime must be wired by the production assembly path");
    assert_eq!(runtime.outlet_count(), 1, "one outlet from [[outlets]]");

    // 端到端：南向采集（调度器真实驱动 FC03 读）→ 数据面 → 北向 PUBLISH。
    wait_until(
        "broker receives at least one QoS1 PUBLISH",
        Duration::from_secs(20),
        || broker.publish_count() > 0,
    )
    .await;
    assert!(
        modbus_conns.load(Ordering::SeqCst) > 0,
        "scheduler must have polled the mock modbus slave"
    );
    assert!(
        runtime.stats().pumps > 0,
        "north driver pumps must have run"
    );
    assert!(
        dir.path().join("queue.db").is_file(),
        "queue.db must land under the configured data_dir"
    );

    // payload 语义断言：JSON 批次解码出已知值（device / point / value / quality）。
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut matched = false;
    while !matched {
        assert!(
            Instant::now() < deadline,
            "no payload decoded to the known value; captured={} stats={:?}",
            broker.captured().len(),
            runtime.stats()
        );
        for payload in broker.captured() {
            let Ok(batch) = decode_batch(Encoding::Json, &payload) else {
                continue;
            };
            for point in &batch.points {
                if point.device_id == "dev-01"
                    && point.point_id == "40001"
                    && point.value.len() == 8
                    && (f64::from_le_bytes(point.value.as_slice().try_into().expect("8 bytes"))
                        - 4660.0)
                        .abs()
                        < 1e-9
                    && point.quality == protocol_proto::Quality::Good as i32
                {
                    matched = true;
                }
            }
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    shared.request_shutdown();
    let result = handle.await.expect("run task joins").expect("run ok");
    assert_eq!(result.state(), LifecycleState::Stopped);
    assert_eq!(
        runtime.running_tasks(),
        0,
        "shutdown must abort all north driver tasks"
    );
}

// ============================================================================
// 场景 2：授权装配失败（Degraded 闸门）——样本照常采集 / 投递发送队列，
// 驱动拍全部被 NorthForwardGate 拒绝：0 PUBLISH、水位有界（落盘降级）
// ============================================================================

#[tokio::test]
async fn degraded_gate_blocks_publishing_while_capture_and_queueing_continue() {
    let modbus_conns = Arc::new(AtomicUsize::new(0));
    let modbus_addr = spawn_mock_modbus(vec![0x0042], Arc::clone(&modbus_conns)).await;
    let broker = spawn_mock_broker().await;
    broker.resume(); // broker 正常可达：不发报文的唯一原因是闸门。

    let dir = tempfile::tempdir().expect("tempdir");
    let toml = config_toml(Some(broker.addr), modbus_addr, dir.path());
    let (shared, handle, queue) =
        start_daemon(&toml, dir.path(), Some("test: license assembly failed")).await;
    let runtime = shared.north_runtime().expect("north runtime wired");

    // 闸门生效：驱动拍计入 gated_cycles（Degraded 语义零改动，gate 在每拍拒绝）。
    wait_until(
        "gated_cycles grows (license gate denies every driver cycle)",
        Duration::from_secs(20),
        || runtime.stats().gated_cycles > 0,
    )
    .await;

    // 采集照常：调度器对 mock 从站发起真实读（样本产生不依赖授权状态）。
    wait_until(
        "scheduler polls the mock modbus slave",
        Duration::from_secs(10),
        || modbus_conns.load(Ordering::SeqCst) > 0,
    )
    .await;

    // 数据面照常投递发送队列：admitted / spilled 增长（超水位落盘降级 = 有界）。
    wait_until(
        "send queue spills beyond the high-water mark (bounded memory)",
        Duration::from_secs(20),
        || runtime.stats().spilled > 0,
    )
    .await;
    assert!(
        runtime.stats().admitted > 0,
        "dataplane must keep submitting while gated; stats={:?}",
        runtime.stats()
    );
    let disk = queue
        .as_ref()
        .expect("queue present")
        .pending_disk()
        .expect("disk stats");
    assert!(disk > 0, "spilled batches must persist in queue.db");

    // 北向零转发：闸门期间绝不 PUBLISH。
    let published_during_gate = broker.publish_count();
    assert_eq!(
        published_during_gate, 0,
        "license gate must block all publishing while degraded"
    );

    shared.request_shutdown();
    let result = handle.await.expect("run task joins").expect("run ok");
    assert_eq!(result.state(), LifecycleState::Stopped);
}

// ============================================================================
// 场景 3：无 [[outlets]]（但声明了点位）——北向不启动、不建 queue.db，采集照常
// ============================================================================

#[tokio::test]
async fn points_without_outlets_keep_existing_behavior() {
    let modbus_conns = Arc::new(AtomicUsize::new(0));
    let modbus_addr = spawn_mock_modbus(vec![0x0001], Arc::clone(&modbus_conns)).await;

    let dir = tempfile::tempdir().expect("tempdir");
    let toml = config_toml(None, modbus_addr, dir.path());
    let (shared, handle, queue) = start_daemon(&toml, dir.path(), None).await;

    assert!(
        shared.north_runtime().is_none(),
        "north runtime must stay unwired without outlets"
    );
    assert!(queue.is_none(), "no north assembly without outlets");
    assert!(
        !dir.path().join("queue.db").exists(),
        "queue.db must not be created without outlets"
    );

    // 采集照常：南向轮询真实发生（数据面桥接对空泳道是无操作）。
    wait_until(
        "scheduler polls the mock modbus slave",
        Duration::from_secs(10),
        || modbus_conns.load(Ordering::SeqCst) > 0,
    )
    .await;

    shared.request_shutdown();
    let result = handle.await.expect("run task joins").expect("run ok");
    assert_eq!(result.state(), LifecycleState::Stopped);
}

//! 北向运行期 + 离线队列**生产装配路径**集成测试（queue.db 生产落盘接线）。
//!
//! 与 bin `iot-daq-daemon.rs` main 的装配完全同构：
//! `assemble_north_runtime(&config)`（OfflineQueue @ `<data_dir>/queue.db` +
//! `NorthRuntimeConfig`）→ `BootstrapBuilder::with_north_runtime` +
//! `with_offline_flusher(offline_flush_hook(queue))`。
//!
//! 覆盖场景：
//! 1. Happy：生产装配后 broker **真实收到 PUBLISH**（localhost TCP mock broker，
//!    复用 `license_gating.rs` 的真实 IO 手法）；`queue.db` 落在 data_dir；
//! 2. 断连：broker 暂停应答期间数据溢写落盘（queue.db 未 ack 行数 > 0）；
//!    恢复后落盘批次被补发（`replayed > 0`），且**无重复行、无丢发**
//!    （最终 QoS1 PUBLISH 计数 == 投递总数）；
//! 3. 未声明 `[[outlets]]`：装配返回 `None`、北向不启动、不创建 queue.db
//!    （保持既有行为）。
//!
//! **计时纪律**：真实 TCP 往返使用有界的 deadline 轮询（与 TLS 握手测试同口径），
//! 不比较两次绝对耗时。

use std::net::SocketAddr;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

use daemon::backpressure::PushOutcome;
use daemon::bootstrap::{
    assemble_north_runtime, offline_flush_hook, BootstrapBuilder, DaemonShared, LifecycleState,
};
use daemon::config::GatewayConfig;
use daemon::error::DaemonResult;

// ============================================================================
// mock broker（localhost 真实 TCP；支持「暂停应答」模拟断连）
// ============================================================================

/// mock broker 句柄：地址 + QoS1 PUBLISH 计数 + 恢复开关。
struct MockBroker {
    addr: SocketAddr,
    /// 收到的 **QoS ≥ 1** PUBLISH 条数（心跳注入是 QoS0，不计入 → 计数恒等于
    /// 客户端真实上报批次，可直接断言「无重复、无丢发」）。
    publishes: Arc<AtomicUsize>,
    /// `false` = 暂停应答（已建立的连接不回 CONNACK，模拟断连）；
    /// `true` = 正常应答。
    resume: Arc<AtomicBool>,
}

impl MockBroker {
    fn resume(&self) {
        self.resume.store(true, Ordering::SeqCst);
    }

    fn publish_count(&self) -> usize {
        self.publishes.load(Ordering::SeqCst)
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
/// 暂停期（`resume == false`）接受连接但**不应答**：客户端 CONNECT 挂起等待
/// CONNACK，驱动任务停在 `poll_event`，泵不推进——即「断连」的确定性模拟
/// （端口始终可达，无需换端口重建 listener）。
///
/// 恢复后：应答 CONNECT / PUBLISH / PINGREQ，并每 100ms 注入一条 QoS0
/// PUBLISH（保持客户端事件循环流动，与 `license_gating.rs` 同口径）。
async fn spawn_mock_broker() -> MockBroker {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("local addr");
    let publishes = Arc::new(AtomicUsize::new(0));
    let resume = Arc::new(AtomicBool::new(false));
    let pubs = Arc::clone(&publishes);
    let resume_flag = Arc::clone(&resume);
    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                break;
            };
            let pubs = Arc::clone(&pubs);
            let resume = Arc::clone(&resume_flag);
            tokio::spawn(async move {
                let (mut rd, wr) = tokio::io::split(stream);
                let wr = Arc::new(tokio::sync::Mutex::new(wr));

                // 暂停期：等待恢复开关（连接已建立、CONNECT 已缓冲）。
                while !resume.load(Ordering::SeqCst) {
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }

                // 心跳注入任务：每 100ms 一条 QoS0 PUBLISH（不计入 publishes）。
                {
                    let wr = Arc::clone(&wr);
                    tokio::spawn(async move {
                        let mut seq = 0u8;
                        loop {
                            tokio::time::sleep(Duration::from_millis(100)).await;
                            // fixed 0x30 (PUBLISH QoS0) + rem-len 4 + topic len 1 "t" + payload。
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
                            if qos >= 1 {
                                // QoS ≥ 1 PUBLISH：计数（客户端真实批次）并回 PUBACK。
                                pubs.fetch_add(1, Ordering::SeqCst);
                                if body.len() >= 4 {
                                    let topic_len = u16::from_be_bytes([body[0], body[1]]) as usize;
                                    if body.len() >= topic_len + 4 {
                                        let pid = [body[2 + topic_len], body[3 + topic_len]];
                                        let mut guard = wr.lock().await;
                                        let _ =
                                            guard.write_all(&[0x40, 0x02, pid[0], pid[1]]).await;
                                    }
                                }
                            }
                            // QoS0（心跳注入）不计数、不回包。
                        }
                        0xC0 => {
                            // PINGREQ → PINGRESP。
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
        resume,
    }
}

// ============================================================================
// 装配夹具（与 bin main 完全同构）
// ============================================================================

/// 测试配置（一个指向 mock broker 的出口；data_dir 落临时目录）。
fn config_toml(broker: SocketAddr, data_dir: &Path) -> String {
    format!(
        "[gateway]\n\
         gateway_id = \"gw-north-wiring\"\n\
         data_dir = \"{}\"\n\
         \n\
         [[outlets]]\n\
         name = \"north-1\"\n\
         broker = \"mqtt://{broker}\"\n\
         qos = 1\n",
        // TOML 基本字符串：Windows 路径反斜杠转正斜杠（Windows 原生接受）。
        data_dir.display().to_string().replace('\\', "/")
    )
}

/// 按生产装配路径启动 daemon（Running 后返回共享态 / run 句柄 / 离线队列）。
///
/// 与 `iot-daq-daemon.rs` main ②-d → ④ 的接线一一对应；仅驱动拍改用 50ms
/// （加速真实 TCP 往返下的补发收敛；生产默认 200ms 不受影响）。
async fn start_daemon(
    toml_text: &str,
    dir: &Path,
) -> (
    DaemonShared,
    tokio::task::JoinHandle<DaemonResult<DaemonShared>>,
    Arc<daemon::offline_queue::OfflineQueue>,
) {
    let config = GatewayConfig::parse(toml_text).expect("parse config");
    let assembly = assemble_north_runtime(&config)
        .expect("north assembly must not fail for a valid config")
        .expect("[[outlets]] declared → assembly must be Some");
    let config_path = dir.join("config.toml");
    std::fs::write(&config_path, toml_text).expect("write config");

    let shared = DaemonShared::new();
    let builder = BootstrapBuilder::new(&config_path)
        .with_shared(shared.clone())
        .without_signal_handlers()
        .with_north_runtime(assembly.runtime_config.with_tick(Duration::from_millis(50)))
        .with_offline_flusher(offline_flush_hook(Arc::clone(&assembly.queue)));
    let handle = tokio::spawn(builder.run());
    wait_until_running(&shared).await;
    (shared, handle, assembly.queue)
}

/// 有界等待共享态到达 Running。
async fn wait_until_running(shared: &DaemonShared) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while shared.state() != LifecycleState::Running {
        assert!(Instant::now() < deadline, "daemon never reached Running");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

// ============================================================================
// 场景 1（Happy）：生产装配 → broker 真实收到 PUBLISH；queue.db 落 data_dir
// ============================================================================

#[tokio::test]
async fn production_assembly_delivers_publish_to_broker() {
    let broker = spawn_mock_broker().await;
    broker.resume(); // 正常场景：连接建立即应答。
    let dir = tempfile::tempdir().expect("tempdir");
    let toml = config_toml(broker.addr, dir.path());
    let (shared, handle, _queue) = start_daemon(&toml, dir.path()).await;

    let runtime = shared
        .north_runtime()
        .expect("north runtime must be wired by the production assembly path");
    assert_eq!(runtime.outlet_count(), 1, "one outlet from [[outlets]]");

    // 采集侧投递入口（同步、不阻塞）；PUBACK 门控确认在驱动循环内完成。
    assert!(matches!(
        runtime.submit("north-1", 1, vec![7u8; 32]),
        Ok(PushOutcome::Admitted)
    ));

    // 真实 TCP 往返：有界等待 broker 收到 QoS1 PUBLISH。
    let deadline = Instant::now() + Duration::from_secs(15);
    while broker.publish_count() == 0 {
        assert!(
            Instant::now() < deadline,
            "broker never received PUBLISH; stats={:?}",
            runtime.stats()
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(runtime.stats().pumps > 0, "driver pumps must have run");
    assert!(
        dir.path().join("queue.db").is_file(),
        "queue.db must land under the configured data_dir"
    );

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
// 场景 2：断连 → 溢写落盘 queue.db → 恢复补发（无重复、无丢发）
// ============================================================================

#[tokio::test]
async fn disconnected_batches_persist_in_queue_db_and_replay_after_recovery() {
    let broker = spawn_mock_broker().await; // 暂停应答 = 断连模拟。
    let dir = tempfile::tempdir().expect("tempdir");
    let toml = config_toml(broker.addr, dir.path());
    let (shared, handle, queue) = start_daemon(&toml, dir.path()).await;
    let runtime = shared.north_runtime().expect("north runtime wired");

    // 断连期投递：驱动任务停在 CONNACK 等待，发送队列在途增长 → 超高水位溢写落盘
    //（发送水位 32 / 硬上限 64；64 次投递必有溢写，且硬上限拒绝不会出现）。
    const TOTAL: usize = 64;
    let mut spilled = 0usize;
    for seq in 1..=TOTAL as u64 {
        match runtime.submit("north-1", seq, vec![seq as u8; 64]) {
            Ok(PushOutcome::Admitted) => {}
            Ok(PushOutcome::Spilled { .. }) => spilled += 1,
            other => panic!("unexpected submit outcome: {other:?}"),
        }
    }
    assert!(
        spilled > 0,
        "pushes beyond the send high-water must spill to the offline queue"
    );

    // 落盘断言：queue.db 未 ack 行数 > 0（降级批次已持久化；spill 内部已强制 flush）。
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let disk = queue.pending_disk().expect("disk stats");
        if disk > 0 {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "degraded batches never reached queue.db"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(
        broker.publish_count(),
        0,
        "paused broker must not receive anything while disconnected"
    );

    // 恢复：broker 开始应答 → CONNACK → 泵恢复 → 内存 + 落盘批次全部发出。
    broker.resume();
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let stats = runtime.stats();
        if stats.replayed > 0 && broker.publish_count() > 0 {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "no replay after broker recovery; stats={stats:?}"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(
        runtime.stats().replayed > 0,
        "persisted batches must be replayed from queue.db"
    );

    // 收敛断言 ①：全部批次恰好各发一次（无重复行、无丢发）——
    // 最终 QoS1 PUBLISH 计数 == 投递总数。以 broker 实收计数为准（发送队列与
    // 离线队列的「已清空」不含 rumqttc 请求通道内的在途报文，不能作为收敛依据）。
    let deadline = Instant::now() + Duration::from_secs(60);
    while broker.publish_count() < TOTAL {
        assert!(
            Instant::now() < deadline,
            "broker never received all {TOTAL} publishes; got {} stats={:?}",
            broker.publish_count(),
            runtime.stats()
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert_eq!(
        broker.publish_count(),
        TOTAL,
        "each submitted batch must be published exactly once \
         (idempotent replay, no duplicate rows)"
    );

    // 收敛断言 ②：全链路排空——发送队列（内存水位）与离线队列（内存 + 磁盘）
    // 均清零；已 ack 批次从 queue.db 删除。
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let drained = queue.pending().expect("queue stats") == 0
            && runtime
                .outlet("north-1")
                .is_none_or(|outlet| outlet.send().pending() == 0);
        if drained {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "queue never drained; stats={:?}",
            runtime.stats()
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert_eq!(
        queue.pending_disk().expect("disk stats"),
        0,
        "acked batches must be deleted from queue.db"
    );
    assert_eq!(
        queue.pending_disk().expect("disk stats"),
        0,
        "acked batches must be deleted from queue.db"
    );

    shared.request_shutdown();
    let result = handle.await.expect("run task joins").expect("run ok");
    assert_eq!(result.state(), LifecycleState::Stopped);
}

// ============================================================================
// 场景 3：未声明 [[outlets]] → 保持既有行为（北向不启动、queue.db 不创建）
// ============================================================================

#[tokio::test]
async fn no_outlets_keeps_existing_behavior() {
    let dir = tempfile::tempdir().expect("tempdir");
    let toml = format!(
        "[gateway]\n\
         gateway_id = \"gw-north-wiring\"\n\
         data_dir = \"{}\"\n",
        dir.path().display().to_string().replace('\\', "/")
    );
    let config = GatewayConfig::parse(&toml).expect("parse");
    assert!(
        assemble_north_runtime(&config)
            .expect("assemble must not fail")
            .is_none(),
        "no [[outlets]] → no north assembly (existing behavior)"
    );

    // 与生产 bin 同构的注入条件（None → 不注入）下 bootstrap 照常 Running。
    let config_path = dir.path().join("config.toml");
    std::fs::write(&config_path, &toml).expect("write config");
    let shared = DaemonShared::new();
    let builder = BootstrapBuilder::new(&config_path)
        .with_shared(shared.clone())
        .without_signal_handlers();
    let handle = tokio::spawn(builder.run());
    wait_until_running(&shared).await;
    assert!(
        shared.north_runtime().is_none(),
        "north runtime must stay unwired without outlets"
    );
    assert!(
        !dir.path().join("queue.db").exists(),
        "queue.db must not be created without outlets"
    );
    shared.request_shutdown();
    let result = handle.await.expect("run task joins").expect("run ok");
    assert_eq!(result.state(), LifecycleState::Stopped);
}

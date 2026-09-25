//! **授权状态门控接线**集成测试（免费版降级 / 北向转发门控 / 配额闸门）。
//!
//! 覆盖场景（fail-closed、可解释、「降级 ≠ 停用」）：
//! 1. `Degraded` 时北向驱动**跳过发送**（`gated_cycles` 增长、pump 恒 0），
//!    而采集侧 `submit` 照常受理（本地采集不停）；
//! 2. `Degraded`（免费版）配额闸门：设备数 > 8 → 拒绝（含字段名 + 恢复路径）；
//! 3. 非 Modbus 协议 → 拒绝；Modbus 两种写法放行；
//! 4. 采集间隔 < 1s → 拒绝；恰好 1s 放行；试用（Trial）不受配额限制；
//! 5. **状态跃迁传播**：Licensed → Degraded 后 broker 收不到 PUBLISH；
//!    Degraded → Licensed（激活恢复）后 broker **真实收到 PUBLISH**（localhost
//!    TCP mock broker，复用 `tls_mtls_handshake.rs` 的真实 IO 手法）。
//!
//! **纪律**：授权状态推进全部用注入虚拟时钟（无真实 sleep 参与**授权语义**）；
//! 仅测试 5 的真实 TCP 往返使用有界的 `tokio::time::timeout` / 等待阈值轮询
//!（与既有 TLS 握手测试同口径）。

use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use base64::engine::general_purpose::STANDARD as B64;
use base64::engine::general_purpose::STANDARD_NO_PAD as B64NP;
use base64::Engine as _;
use ed25519_dalek::{Signer, SigningKey};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::watch;

use daemon::auth::client::{
    render_response_signing_message, LicenseState, LicenseTransport, LicensingClient,
    LicensingClientConfig, RESPONSE_SIG_DOMAIN_ACTIVATION,
};
use daemon::auth::machine_id::{FingerprintKey, MachineIdentity, StaticAnchor};
use daemon::auth::signing::{AuthSigner, LicenseGate, StaticKeyProvider};
use daemon::backpressure::{BackpressureAudit, PushOutcome};
use daemon::config::{GatewayConfig, GatewaySection, OutletConfig, OutletEncoding, PointConfig};
use daemon::error::DaemonError;
use daemon::license::{LicenseRuntime, LicenseRuntimeConfig};
use daemon::north::mqtt::AuditSink;
use daemon::north::runtime::{NorthRuntime, NorthRuntimeConfig};
use daemon::offline_queue::{OfflineQueue, QueueConfig, SystemClock};

// ============================================================================
// 测试常量（**仅测试，禁止真实部署**）
// ============================================================================

/// **TEST_ONLY_** 设备私钥种子（32 字节；禁止真实部署）。
const TEST_ONLY_SEED: [u8; 32] = *b"iotdaq-license-gating-test-seed!";
/// **TEST_ONLY_** 服务端响应签名私钥种子（TOFU；**仅测试，禁止真实部署**）。
const TEST_ONLY_SERVER_SEED: [u8; 32] = [0x6bu8; 32];
/// 测试指纹 HMAC key（派 mid 用，**仅测试**）。
const TEST_ONLY_FP_KEY: &[u8] = b"TEST_ONLY_license_gating_fp_key";
/// 租约失效时刻（UTC 秒，2100-01-01）。
const VALID_UNTIL: i64 = 4_102_444_800;
/// 租约签发时刻（UTC 秒）。
const ISSUED_AT: i64 = VALID_UNTIL - 365 * 86_400;
/// 虚拟时间基准（Unix 毫秒）。
const T0_MS: u64 = 1_700_000_000_000;
/// 一天毫秒数。
const DAY_MS: u64 = 86_400_000;
/// 心跳周期（秒；短周期便于虚拟时间内触发）。
const HEARTBEAT_SECS: u64 = 60;

// ============================================================================
// 夹具：mock 传输 + 虚拟时钟（与 tests/license_runtime.rs 同手法）
// ============================================================================

/// 恒开放行的测试闸门。
struct AlwaysGate;
impl LicenseGate for AlwaysGate {
    fn can_sign(&self) -> bool {
        true
    }
}

/// 经 task 3 指纹模块派生 mid 与逐锚点哈希集。
fn test_identity() -> (String, Vec<String>) {
    let identity = MachineIdentity::new(
        vec![Box::new(StaticAnchor::new(
            "gating-anchor",
            Some("gw-gating-001"),
        ))],
        1,
        FingerprintKey::from_bytes(TEST_ONLY_FP_KEY.to_vec()).expect("test-only fp key"),
    );
    let mid = identity
        .get_machine_fingerprint()
        .expect("quorum ok: 1 usable anchor");
    let anchors = identity
        .get_anchor_hashes()
        .expect("quorum ok: 1 usable anchor");
    (mid, anchors)
}

/// 追加长度前缀字段（与服务端 lease 签名域同口径）。
fn push_len_field(out: &mut String, name: &str, value: &str) {
    out.push('|');
    out.push_str(name);
    out.push('=');
    out.push_str(&value.len().to_string());
    out.push(':');
    out.push_str(value);
}

/// 渲染 Lease Token 签名域串（与 `client.rs::render_signing_message` 一致）。
fn render_lease_signing_message(kid: &str, claims: &serde_json::Value) -> Vec<u8> {
    let s = |k: &str| claims[k].as_str().unwrap_or_default().to_string();
    let n = |k: &str| claims[k].as_i64().unwrap_or_default().to_string();
    let mut out = String::from("iotdaq.lease.v1");
    push_len_field(&mut out, "kid", kid);
    push_len_field(&mut out, "lease_id", &s("lease_id"));
    push_len_field(&mut out, "device_id", &s("device_id"));
    push_len_field(&mut out, "mid", &s("mid"));
    push_len_field(&mut out, "tier", &s("tier"));
    push_len_field(&mut out, "verify_mode", &s("verify_mode"));
    push_len_field(&mut out, "issued_at", &n("issued_at"));
    push_len_field(&mut out, "valid_until", &n("valid_until"));
    out.into_bytes()
}

/// 构造合法三段式 Lease Token。
fn build_lease_token(seed: &[u8; 32], kid: &str, mid: &str, verify_mode: &str) -> String {
    let claims = serde_json::json!({
        "lease_id": "lease-0001",
        "device_id": "dev-0001",
        "mid": mid,
        "tier": "standard",
        "verify_mode": verify_mode,
        "issued_at": ISSUED_AT,
        "valid_until": VALID_UNTIL,
    });
    let message = render_lease_signing_message(kid, &claims);
    let key = SigningKey::from_bytes(seed);
    let sig = B64.encode(key.sign(&message).to_bytes());
    let payload = serde_json::to_vec(&claims).expect("claims serialize");
    format!(
        "{}.{}.{}",
        B64NP.encode(kid.as_bytes()),
        B64NP.encode(payload),
        sig
    )
}

/// mock 动作。
#[derive(Default)]
enum MockAction {
    /// 激活成功：请求到达时**动态构造**带 TOFU 服务端签名的响应
    /// （响应 `sig` 域串绑定请求 nonce，无法离线预制）。
    ActivationOk { token: String },
    /// 网络错误（断网）。
    NetFail,
    /// 未编程（默认按网络错误处理，fail-closed）。
    #[default]
    Unset,
}

#[derive(Default)]
struct MockInner {
    action: MockAction,
    calls: Vec<String>,
}

/// 测试用网络传输假实现。
#[derive(Default)]
struct MockTransport {
    inner: Mutex<MockInner>,
}

impl MockTransport {
    /// 编程：下一次激活请求返回**带 TOFU 服务端签名**的成功响应
    /// （`server_pubkey` + `sig`，sig 域串 `activation|lease-0001|<nonce>|<server_time>`）。
    fn respond_with_activation(&self, token: String) {
        self.inner.lock().expect("mock lock").action = MockAction::ActivationOk { token };
    }

    fn fail_network(&self) {
        self.inner.lock().expect("mock lock").action = MockAction::NetFail;
    }
}

impl LicenseTransport for MockTransport {
    fn post_json(
        &self,
        url: &str,
        _body: &serde_json::Value,
    ) -> Result<serde_json::Value, DaemonError> {
        let mut inner = self.inner.lock().expect("mock lock");
        inner.calls.push(url.to_string());
        match &inner.action {
            MockAction::ActivationOk { token } => {
                // 动态构造 TOFU 签名的激活响应（服务端 `ActivationResponse` 形状，
                // lease_id / tier / valid_until 与 `build_lease_token` 的默认租约一致）。
                let nonce = _body
                    .get("nonce")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .to_string();
                let server_time = ISSUED_AT;
                let message = render_response_signing_message(
                    RESPONSE_SIG_DOMAIN_ACTIVATION,
                    "lease-0001",
                    &nonce,
                    server_time,
                );
                let server_key = SigningKey::from_bytes(&TEST_ONLY_SERVER_SEED);
                Ok(serde_json::json!({
                    "lease_id": "lease-0001",
                    "lease_token": token,
                    "verify_mode": "B",
                    "tier": "standard",
                    "valid_until": VALID_UNTIL.to_string(),
                    "heartbeat_hours": 24,
                    "server_time": server_time.to_string(),
                    "nonce": nonce,
                    "server_pubkey": B64.encode(server_key.verifying_key().to_bytes()),
                    "sig": B64.encode(server_key.sign(message.as_bytes()).to_bytes()),
                }))
            }
            MockAction::NetFail => Err(DaemonError::NetworkError("mock: link down".to_string())),
            MockAction::Unset => Err(DaemonError::NetworkError(
                "mock: not programmed".to_string(),
            )),
        }
    }
}

/// 构造授权客户端（mock 传输 + kid-a 公钥 + 设备签名器）。
fn build_client(
    transport: Arc<MockTransport>,
    mid: &str,
    anchors: Vec<String>,
) -> Arc<LicensingClient> {
    let signer = Arc::new(
        AuthSigner::new(
            Arc::new(StaticKeyProvider::new(TEST_ONLY_SEED)),
            Arc::new(AlwaysGate),
            mid.to_string(),
        )
        .expect("mid is 64-hex"),
    );
    let cfg =
        LicensingClientConfig::new("https://licensing.test/v1", 24, 10, 30).expect("non-empty url");
    let raw_transport: Arc<dyn LicenseTransport> =
        Arc::clone(&transport) as Arc<dyn LicenseTransport>;
    let mut client = LicensingClient::with_transport(cfg, signer, mid.to_string(), raw_transport)
        .with_receipt_signer(Arc::new(StaticKeyProvider::new(TEST_ONLY_SEED)))
        .with_anchor_hashes(anchors);
    client
        .register_public_key(
            "kid-a",
            &SigningKey::from_bytes(&TEST_ONLY_SEED)
                .verifying_key()
                .to_bytes(),
        )
        .expect("register kid-a");
    Arc::new(client)
}

/// 简单夹具：mock + client + 独立 data_dir + 虚拟时钟。
struct Fixture {
    mock: Arc<MockTransport>,
    mid: String,
    client: Arc<LicensingClient>,
    data_dir: std::path::PathBuf,
    now: Arc<AtomicU64>,
    _dir: tempfile::TempDir,
}

impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        let mock = Arc::new(MockTransport::default());
        let (mid, anchors) = test_identity();
        let client = build_client(Arc::clone(&mock), &mid, anchors);
        Self {
            mock,
            mid,
            client,
            data_dir: dir.path().join("data"),
            now: Arc::new(AtomicU64::new(T0_MS)),
            _dir: dir,
        }
    }

    fn now_closure(&self) -> Arc<dyn Fn() -> u64 + Send + Sync> {
        let store = Arc::clone(&self.now);
        Arc::new(move || store.load(Ordering::SeqCst))
    }

    /// 纯本地 C 档运行时（无 cloud_url / 激活码）。
    fn local(&self) -> LicenseRuntime {
        LicenseRuntime::new(
            LicenseRuntimeConfig::new(Arc::clone(&self.client), self.data_dir.clone())
                .with_now_ms(self.now_closure()),
        )
    }

    /// 联网运行时（cloud_url + 激活码 + 短心跳周期）。
    fn cloud(&self) -> LicenseRuntime {
        LicenseRuntime::new(
            LicenseRuntimeConfig::new(Arc::clone(&self.client), self.data_dir.clone())
                .with_cloud_url("https://licensing.test/v1")
                .with_activation_code("ACT-TEST-0001")
                .with_heartbeat_interval(Duration::from_secs(HEARTBEAT_SECS))
                .with_now_ms(self.now_closure()),
        )
    }

    fn advance(&self, delta_ms: u64) {
        self.now.fetch_add(delta_ms, Ordering::SeqCst);
    }

    fn token(&self) -> String {
        build_lease_token(&TEST_ONLY_SEED, "kid-a", &self.mid, "B")
    }
}

// ============================================================================
// 夹具：北向运行期 + 真实 TCP mock broker
// ============================================================================

/// 丢弃型审计出口。
struct DropSink;

impl AuditSink for DropSink {
    fn emit(&self, _events: Vec<BackpressureAudit>) {}
}

/// 临时目录打开真实 [`OfflineQueue`]。
fn temp_queue(dir: &std::path::Path) -> Arc<OfflineQueue> {
    let cfg = QueueConfig::new(dir.join("queue.db"), "gw-gating").expect("queue cfg");
    Arc::new(OfflineQueue::open(cfg, Arc::new(SystemClock)).expect("open queue"))
}

/// 最小 MQTT 3.1.1 broker（localhost 真实 TCP）：CONNACK + PUBLISH 计数 + PUBACK。
struct MockBroker {
    addr: SocketAddr,
    publishes: Arc<AtomicUsize>,
}

impl MockBroker {
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
/// 除应答 CONNECT / PUBLISH / PINGREQ 外，每 100ms 向客户端主动发一条 QoS0
/// PUBLISH（topic `t`）：让客户端事件循环持续有事件可读 —— 否则空闲连接上
/// `poll_event` 会阻塞到 keep-alive（60s），门控拍将无从推进。
async fn spawn_mock_broker() -> MockBroker {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("local addr");
    let publishes = Arc::new(AtomicUsize::new(0));
    let pubs = Arc::clone(&publishes);
    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                break;
            };
            let pubs = Arc::clone(&pubs);
            tokio::spawn(async move {
                let (mut rd, wr) = tokio::io::split(stream);
                let wr = Arc::new(tokio::sync::Mutex::new(wr));

                // 心跳注入任务：每 100ms 一条 QoS0 PUBLISH（保持客户端事件流动）。
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
                            // PUBLISH：计数；QoS ≥ 1 回 PUBACK（保客户端在途窗口健康）。
                            pubs.fetch_add(1, Ordering::SeqCst);
                            let qos = (opcode >> 1) & 0x03;
                            if qos >= 1 && body.len() >= 4 {
                                let topic_len = u16::from_be_bytes([body[0], body[1]]) as usize;
                                if body.len() >= topic_len + 4 {
                                    let pid = [body[2 + topic_len], body[3 + topic_len]];
                                    let mut guard = wr.lock().await;
                                    let _ = guard.write_all(&[0x40, 0x02, pid[0], pid[1]]).await;
                                }
                            }
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
    MockBroker { addr, publishes }
}

/// 构造单出口配置（指向给定地址）。
fn outlet_to(addr: SocketAddr) -> OutletConfig {
    OutletConfig {
        name: "north-1".to_string(),
        broker: format!("mqtt://{addr}"),
        topic_prefix: "telemetry".to_string(),
        qos: 1,
        tls: false,
        ca_cert_path: None,
        client_cert_path: None,
        client_key_path: None,
        server_name: None,
        alpn: Vec::new(),
        encoding: OutletEncoding::Protobuf,
    }
}

/// 构造带 N 台设备（每台 1 点位、指定协议 / 间隔）的配置。
fn config_with_devices(device_count: usize, protocol: &str, frequency_ms: u64) -> GatewayConfig {
    let points = (0..device_count)
        .map(|i| PointConfig {
            device_id: format!("dev-{i:03}"),
            point_id: format!("p_{i:03}"),
            protocol: protocol.to_string(),
            address: format!("192.168.1.{i}:502"),
            frequency_ms,
        })
        .collect();
    GatewayConfig {
        gateway: GatewaySection::default(),
        outlets: Vec::new(),
        points,
        devices: Vec::new(),
        mgmt_auth: None,
    }
}

/// 有界等待谓词成立（真实 IO 场景；阈值驱动而非固定时序）。
async fn wait_until(mut cond: impl FnMut() -> bool, max: Duration) -> bool {
    let deadline = std::time::Instant::now() + max;
    while std::time::Instant::now() < deadline {
        if cond() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    cond()
}

// ============================================================================
// 1. Degraded：北向跳过发送，采集入队继续
// ============================================================================

#[tokio::test(start_paused = true)]
async fn degraded_north_gate_skips_forward_but_submit_continues() {
    let fx = Fixture::new();
    let rt = Arc::new(fx.local());
    rt.step().await.expect("step ok");
    assert!(matches!(rt.state(), LicenseState::Trial { .. }));

    // 推进 3 天零 1 小时 → 试用到期 → Degraded。
    fx.advance(3 * DAY_MS + 3_600_000);
    rt.step().await.expect("step ok");
    assert!(matches!(rt.state(), LicenseState::Degraded { .. }));

    // 门控 = 授权状态真相源（LicenseRuntime 实现 NorthForwardGate）。
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let runtime = NorthRuntime::start(
        "gw-gating",
        &[outlet_to("127.0.0.1:1883".parse().expect("addr"))],
        NorthRuntimeConfig::new(
            temp_queue(fx._dir.path()),
            Arc::new(SystemClock),
            Arc::new(DropSink),
        )
        .with_tick(Duration::from_millis(20))
        .with_gate(Arc::clone(&rt) as Arc<dyn daemon::north::runtime::NorthForwardGate>),
        shutdown_rx,
    );

    // 采集侧入队继续：Degraded 下 submit 仍受理（降级 ≠ 停用）。
    let outcome = runtime
        .submit("north-1", 1, vec![1, 2, 3])
        .expect("submit ok");
    assert!(
        matches!(outcome, PushOutcome::Admitted),
        "capture-side ingest must keep working while degraded: {outcome:?}"
    );

    // 推进多个驱动拍（poll 失败触发指数退避，驱动拍稀疏出现 → 按谓词推进）：
    // 门控生效 —— gated_cycles 增长、pump 恒 0（发送被跳过）。
    let mut gated_cycles = 0u64;
    for _ in 0..400 {
        tokio::time::advance(Duration::from_millis(100)).await;
        tokio::task::yield_now().await;
        gated_cycles = runtime.stats().gated_cycles;
        if gated_cycles >= 5 {
            break;
        }
    }
    let stats = runtime.stats();
    assert!(
        gated_cycles >= 5,
        "denied driver cycles must be counted: {stats:?}"
    );
    assert_eq!(stats.pumps, 0, "pump must be skipped while degraded");

    // 本地采集判据恒真。
    assert!(rt.state().allows_local_capture());

    shutdown_tx.send_replace(true);
}

// ============================================================================
// 2-4. 免费版配额闸门（Degraded 生效；Trial 不限制；边界放行）
// ============================================================================

#[tokio::test]
async fn free_quota_rejects_too_many_devices_when_degraded() {
    let fx = Fixture::new();
    let rt = fx.local();
    rt.step().await.expect("step ok");

    // 试用享受完整配额：20 台也不限制。
    let big = config_with_devices(20, "modbus-tcp", 1_000);
    assert!(
        rt.enforce_free_limits(&big).is_ok(),
        "trial must not be quota-limited"
    );

    // 降到 Degraded：9 台拒绝（消息含实际数 / 字段名 / 恢复路径）；8 台边界放行。
    fx.advance(3 * DAY_MS + 3_600_000);
    rt.step().await.expect("step ok");
    assert!(matches!(rt.state(), LicenseState::Degraded { .. }));

    let err = rt
        .enforce_free_limits(&config_with_devices(9, "modbus-tcp", 1_000))
        .expect_err("9 devices must be rejected in free edition");
    let msg = err.to_string();
    assert!(msg.contains("free-edition"), "reason prefix: {msg}");
    assert!(msg.contains('9'), "actual count: {msg}");
    assert!(msg.contains("device_id"), "field name: {msg}");
    assert!(msg.contains("activate"), "recovery path: {msg}");

    assert!(
        rt.enforce_free_limits(&config_with_devices(8, "modbus-tcp", 1_000))
            .is_ok(),
        "exactly 8 devices is the free-edition boundary"
    );
}

#[tokio::test]
async fn free_quota_rejects_non_modbus_protocol_when_degraded() {
    let fx = Fixture::new();
    let rt = fx.local();
    rt.step().await.expect("step ok");
    // Trial：非 Modbus 不限制。
    assert!(rt
        .enforce_free_limits(&config_with_devices(1, "opcua", 1_000))
        .is_ok());

    fx.advance(3 * DAY_MS + 3_600_000);
    rt.step().await.expect("step ok");

    let err = rt
        .enforce_free_limits(&config_with_devices(1, "opcua", 1_000))
        .expect_err("opcua must be rejected in free edition");
    let msg = err.to_string();
    assert!(msg.contains("protocol"), "field name: {msg}");
    assert!(msg.contains("opcua"), "offending value: {msg}");
    assert!(msg.contains("modbus"), "allowed protocols: {msg}");
    assert!(msg.contains("activate"), "recovery path: {msg}");

    // Modbus 两种写法（含大小写）放行。
    assert!(rt
        .enforce_free_limits(&config_with_devices(1, "modbus-tcp", 1_000))
        .is_ok());
    assert!(rt
        .enforce_free_limits(&config_with_devices(1, "modbus-rtu", 1_000))
        .is_ok());
    assert!(rt
        .enforce_free_limits(&config_with_devices(1, "MODBUS-TCP", 1_000))
        .is_ok());
}

#[tokio::test]
async fn free_quota_rejects_fast_interval_when_degraded() {
    let fx = Fixture::new();
    let rt = fx.local();
    rt.step().await.expect("step ok");
    // Trial：500ms 不限制。
    assert!(rt
        .enforce_free_limits(&config_with_devices(1, "modbus-tcp", 500))
        .is_ok());

    fx.advance(3 * DAY_MS + 3_600_000);
    rt.step().await.expect("step ok");

    let err = rt
        .enforce_free_limits(&config_with_devices(1, "modbus-tcp", 500))
        .expect_err("500ms must be rejected in free edition");
    let msg = err.to_string();
    assert!(msg.contains("frequency_ms"), "field name: {msg}");
    assert!(msg.contains("500"), "offending value: {msg}");
    assert!(msg.contains("1000"), "floor value: {msg}");
    assert!(msg.contains("activate"), "recovery path: {msg}");

    // 边界：恰好 1000ms 放行。
    assert!(rt
        .enforce_free_limits(&config_with_devices(1, "modbus-tcp", 1_000))
        .is_ok());
}

// ============================================================================
// 5. 状态跃迁传播：Degraded 停转发 → 恢复 Licensed 后真实转发到 broker
// ============================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn license_transition_stops_and_resumes_real_forwarding() {
    let fx = Fixture::new();
    let token = fx.token();
    fx.mock.respond_with_activation(token.clone());
    let rt = Arc::new(fx.cloud());
    rt.step().await.expect("activate ok");
    assert!(matches!(rt.state(), LicenseState::Licensed { .. }));

    // 本地 mock broker（真实 TCP）。
    let broker = spawn_mock_broker().await;

    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let runtime = NorthRuntime::start(
        "gw-gating",
        &[outlet_to(broker.addr)],
        NorthRuntimeConfig::new(
            temp_queue(fx._dir.path()),
            Arc::new(SystemClock),
            Arc::new(DropSink),
        )
        .with_tick(Duration::from_millis(50))
        .with_gate(Arc::clone(&rt) as Arc<dyn daemon::north::runtime::NorthForwardGate>),
        shutdown_rx,
    );

    // 降级：断网 8 天 → 宽限耗尽 → Degraded（ Licensed → Degraded 跃迁）。
    fx.mock.fail_network();
    fx.advance(8 * DAY_MS);
    rt.step().await.expect("step ok");
    assert!(matches!(rt.state(), LicenseState::Degraded { .. }));

    // 采集侧照常入队（降级不停采集）；批次应滞留在发送队列（门控跳过发送）。
    runtime
        .submit("north-1", 1, vec![7, 8, 9])
        .expect("submit ok");

    // Degraded 期间：门控生效（gated_cycles 增长），broker 收不到任何 PUBLISH。
    let gated = wait_until(
        || runtime.stats().gated_cycles >= 3,
        Duration::from_secs(25),
    )
    .await;
    assert!(gated, "driver must observe gating while degraded");
    assert_eq!(
        broker.publish_count(),
        0,
        "Degraded must stop northbound forwarding (no PUBLISH on the wire)"
    );

    // 恢复：重新联网 → 激活成功 → Licensed（Grace→Licensed / Degraded→Licensed 恢复路）。
    fx.mock.respond_with_activation(token);
    fx.advance(HEARTBEAT_SECS * 1000 + 1_000);
    rt.step().await.expect("step ok");
    assert!(
        matches!(rt.state(), LicenseState::Licensed { .. }),
        "recovery must return to Licensed, got {}",
        rt.state().name()
    );

    // 恢复后：门控解除，队列中的批次被真实发出 —— broker 收到 PUBLISH。
    let resumed = wait_until(
        || broker.publish_count() >= 1 && runtime.stats().pumps > 0,
        Duration::from_secs(15),
    )
    .await;
    assert!(
        resumed,
        "forwarding must resume after license recovery (publishes={}, pumps={})",
        broker.publish_count(),
        runtime.stats().pumps
    );

    shutdown_tx.send_replace(true);
}

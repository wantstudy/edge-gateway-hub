//! **bootstrap 授权生产装配集成测试**（`auth::assembly` → `BootstrapBuilder` 上岗链路）。
//!
//! 覆盖主理人交付要求的四组场景（mock transport + 虚拟时钟 + 临时 data_dir）：
//! 1. **配置了激活码** → 运行期经 mock 完成激活（状态 `Unlicensed` → `Licensed`），
//!    且**锚点哈希确实随请求发送**（断言 mock 收到的请求体 `anchor_hashes` 字段）；
//!    设备签名密钥持久化在 `data_dir/license/` 约定路径；
//! 2. **装配失败注入** → daemon 照常启动 + 可解释原因落入共享态 +
//!    北向闸门保持关闭（`gated_cycles` 增长、pump 不执行）；
//! 3. **未配置授权** → 行为与旧版完全一致（授权运行期缺席、北向无闸门恒放行）；
//! 4. **生产 HTTPS transport 真实联调**：loopback 明文 HTTP 上真实收发 JSON
//!    （`http://127.0.0.1`，axum echo 服务）+ 非 2xx 错误码透出；
//!    另验证试用到期 → 审计 `TrialExpired` 事件恰好记录一次（task 23 接线）。
//!
//! **纪律**：不真实 sleep、不联网（mock transport / loopback 127.0.0.1）；
//! 时间推进全部用注入虚拟时钟。

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use base64::engine::general_purpose::STANDARD as B64;
use base64::engine::general_purpose::STANDARD_NO_PAD as B64NP;
use base64::Engine as _;
use ed25519_dalek::{Signer, SigningKey};

use daemon::audit::{AuditLogger, AuditQuery};
use daemon::auth::assembly::{
    assemble, AssemblyInput, AssemblyOutcome, HttpsTransport, DEVICE_KEY_FILE, DEVICE_KEY_SUBDIR,
};
use daemon::auth::client::{
    render_response_signing_message, LicenseState, LicenseTransport, RESPONSE_SIG_DOMAIN_ACTIVATION,
};
use daemon::auth::machine_id::{FingerprintKey, MachineIdentity, StaticAnchor};
use daemon::bootstrap::{BootstrapBuilder, DaemonShared, LifecycleState};
use daemon::config::LicensingSection;
use daemon::error::DaemonError;
use daemon::license::LicenseRuntimeConfig;
use daemon::north::mqtt::AuditSink;
use daemon::north::runtime::NorthRuntimeConfig;
use daemon::offline_queue::{OfflineQueue, QueueConfig, SystemClock};

// ============================================================================
// 测试常量（**仅测试，禁止真实部署**）
// ============================================================================

/// **TEST_ONLY_** 设备私钥种子（32 字节；禁止真实部署）。
const TEST_ONLY_SEED: [u8; 32] = *b"iotdaq-bootstrap-assembly-seed01";
/// **TEST_ONLY_** 服务端响应签名私钥种子（TOFU；**仅测试，禁止真实部署**）。
const TEST_ONLY_SERVER_SEED: [u8; 32] = [0x6du8; 32];
/// 测试指纹 HMAC key（派 mid 用，**仅测试**）。
const TEST_ONLY_FP_KEY: &[u8] = b"TEST_ONLY_bootstrap_assembly_fp";
/// 租约失效时刻（UTC 秒，2100-01-01）。
const VALID_UNTIL: i64 = 4_102_444_800;
/// 租约签发时刻（UTC 秒）。
const ISSUED_AT: i64 = VALID_UNTIL - 365 * 86_400;
/// 虚拟时间基准（Unix 毫秒）。
const T0_MS: u64 = 1_700_000_000_000;
/// 一天毫秒数。
const DAY_MS: u64 = 86_400_000;

// ============================================================================
// 夹具：身份 + lease token + 签名响应
// ============================================================================

/// 经 task 3 指纹模块派生 mid 与逐锚点哈希集（两个静态锚点，满足 quorum=2）。
fn test_identity() -> (String, Vec<String>) {
    let identity = MachineIdentity::new(
        vec![
            Box::new(StaticAnchor::new("bootstrap-anchor-a", Some("value-a"))),
            Box::new(StaticAnchor::new("bootstrap-anchor-b", Some("value-b"))),
        ],
        2,
        FingerprintKey::from_bytes(TEST_ONLY_FP_KEY.to_vec()).expect("test-only fp key non-empty"),
    );
    let mid = identity
        .get_machine_fingerprint()
        .expect("quorum ok: 2 usable anchors");
    let anchors = identity
        .get_anchor_hashes()
        .expect("quorum ok: 2 usable anchors");
    (mid, anchors)
}

/// 追加一个长度前缀字段（与服务端 lease 签名域同口径）。
fn push_len_field(out: &mut String, name: &str, value: &str) {
    out.push('|');
    out.push_str(name);
    out.push('=');
    out.push_str(&value.len().to_string());
    out.push(':');
    out.push_str(value);
}

/// 渲染 Lease Token 签名域串（与 `client.rs::render_signing_message` 逐字节一致）。
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

/// 构造合法三段式 Lease Token（`kid.payload.sig`）。
fn build_lease_token(kid: &str, mid: &str) -> String {
    let claims = serde_json::json!({
        "lease_id": "lease-0001",
        "device_id": "dev-0001",
        "mid": mid,
        "tier": "standard",
        "verify_mode": "B",
        "issued_at": ISSUED_AT,
        "valid_until": VALID_UNTIL,
    });
    let message = render_lease_signing_message(kid, &claims);
    let key = SigningKey::from_bytes(&TEST_ONLY_SEED);
    let sig = B64.encode(key.sign(&message).to_bytes());
    let payload = serde_json::to_vec(&claims).expect("claims serialize");
    format!(
        "{}.{}.{}",
        B64NP.encode(kid.as_bytes()),
        B64NP.encode(payload),
        sig
    )
}

/// 动态构造**带 TOFU 服务端签名**的激活成功响应（绑定请求 nonce）。
fn signed_activation_ok(req: &serde_json::Value, token: &str) -> serde_json::Value {
    let nonce = req
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
    serde_json::json!({
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
    })
}

// ============================================================================
// 夹具：bootstrap 装配
// ============================================================================

/// 最小可加载配置（ConfigHotReloader 要求真实文件；无 [gateway.licensing] 段；
/// 声明一个北向出口供装配失败 / 未配置场景的门控观测）。
const TEST_TOML: &str = r#"
[gateway]
gateway_id = "gw-asm"

[[outlets]]
name = "north-1"
broker = "mqtt://127.0.0.1:1883"

[[points]]
device_id = "dev-01"
point_id = "p1"
protocol = "modbus-tcp"
address = "127.0.0.1:502"
frequency_ms = 1000
"#;

/// 把测试配置写入临时目录并返回文件路径。
fn write_test_config(dir: &std::path::Path) -> PathBuf {
    let path = dir.join("config.toml");
    std::fs::write(&path, TEST_TOML).expect("write test config");
    path
}

/// 轮询等待共享态到达指定状态（current_thread 运行时，yield 让 run 任务推进）。
async fn wait_for_state(
    handle: tokio::task::JoinHandle<daemon::error::DaemonResult<DaemonShared>>,
    shared: &DaemonShared,
    want: LifecycleState,
) -> tokio::task::JoinHandle<daemon::error::DaemonResult<DaemonShared>> {
    for _ in 0..2000 {
        if shared.state() == want {
            return handle;
        }
        tokio::task::yield_now().await;
    }
    panic!(
        "state did not reach {want:?} (current {:?})",
        shared.state()
    );
}

/// 虚拟时间源（毫秒）。
fn now_arc() -> Arc<AtomicU64> {
    Arc::new(AtomicU64::new(T0_MS))
}

/// 把 `Arc<AtomicU64>` 包成注入用的时间闭包。
fn now_closure(now: &Arc<AtomicU64>) -> Arc<dyn Fn() -> u64 + Send + Sync> {
    let store = Arc::clone(now);
    Arc::new(move || store.load(Ordering::SeqCst))
}

/// 组装测试用 `AssemblyInput`（注入身份 / transport / lease 公钥；密钥落默认路径）。
fn test_assembly_input<'a>(
    licensing: &'a LicensingSection,
    data_dir: &'a std::path::Path,
    transport: Arc<dyn LicenseTransport>,
) -> AssemblyInput<'a> {
    AssemblyInput {
        licensing,
        data_dir,
        transport: Some(transport),
        anchor_providers: Some(vec![
            Box::new(StaticAnchor::new("bootstrap-anchor-a", Some("value-a"))),
            Box::new(StaticAnchor::new("bootstrap-anchor-b", Some("value-b"))),
        ]),
        fingerprint_key: Some(
            FingerprintKey::from_bytes(TEST_ONLY_FP_KEY.to_vec()).expect("non-empty fp key"),
        ),
        device_key_path: None,
        lease_public_keys: Some(vec![(
            "kid-a".to_string(),
            SigningKey::from_bytes(&TEST_ONLY_SEED)
                .verifying_key()
                .to_bytes()
                .to_vec(),
        )]),
    }
}

// ============================================================================
// 1. 配置了激活码 → 经 mock 激活成功 + 锚点哈希随请求发送
// ============================================================================

#[tokio::test(start_paused = true)]
async fn configured_activation_completes_and_sends_anchor_hashes() {
    let dir = tempfile::tempdir().expect("tempdir");
    let config_path = write_test_config(dir.path());
    let data_dir = dir.path().join("data");

    let (mid, expected_anchors) = test_identity();
    let token = build_lease_token("kid-a", &mid);

    // TOFU 响应签名绑定请求 nonce → mock 必须按请求体**动态**构造响应。
    // 同时记录每次调用（`(url, body)`）供锚点哈希断言用。
    struct DynamicMock(Arc<Mutex<Vec<(String, serde_json::Value)>>>, String);
    impl LicenseTransport for DynamicMock {
        fn post_json(
            &self,
            url: &str,
            body: &serde_json::Value,
        ) -> Result<serde_json::Value, DaemonError> {
            self.0
                .lock()
                .expect("calls lock")
                .push((url.to_string(), body.clone()));
            Ok(signed_activation_ok(body, &self.1))
        }
    }
    let calls: Arc<Mutex<Vec<(String, serde_json::Value)>>> = Arc::new(Mutex::new(Vec::new()));
    let transport = Arc::new(DynamicMock(Arc::clone(&calls), token));

    let licensing = LicensingSection {
        cloud_url: Some("https://licensing.test/v1".to_string()),
        heartbeat_interval_secs: 3_600,
        activation_code: Some("ACT-TEST-0001".to_string()),
        ..LicensingSection::default()
    };
    let outcome = assemble(test_assembly_input(&licensing, &data_dir, transport));
    let AssemblyOutcome::Assembled(rt_cfg) = outcome else {
        panic!("assembly must succeed");
    };
    // 测试接管：tick 缩短 + 虚拟时钟。
    let now = now_arc();
    let rt_cfg = rt_cfg
        .with_tick(Duration::from_millis(20))
        .with_now_ms(now_closure(&now));

    let shared = DaemonShared::new();
    let builder = BootstrapBuilder::new(config_path)
        .with_shared(shared.clone())
        .without_signal_handlers()
        .with_license_runtime(rt_cfg);

    let handle = wait_for_state(
        tokio::spawn(builder.run()),
        &shared,
        LifecycleState::Running,
    )
    .await;

    // 授权运行期已上岗且激活完成：Unlicensed → Licensed。
    let runtime = shared
        .license_runtime()
        .expect("license runtime must be wired");
    for _ in 0..2000 {
        if matches!(runtime.state(), LicenseState::Licensed { .. }) {
            break;
        }
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_millis(1)).await;
    }
    match runtime.state() {
        LicenseState::Licensed { lease } => {
            assert_eq!(lease.lease_id, "lease-0001");
        }
        other => panic!("expected Licensed, got {other:?}"),
    }
    assert!(
        runtime.north_forward_allowed(),
        "licensed ⇒ forward allowed"
    );

    // 断言激活请求确实携带锚点哈希（形状 + 值与本地指纹模块一致）。
    let calls = calls.lock().expect("calls lock").clone();
    let activation = calls
        .iter()
        .find(|(url, _)| url.ends_with("/activate"))
        .expect("activation must hit /activate");
    assert_eq!(
        activation.1["anchor_hashes"],
        serde_json::to_value(&expected_anchors).expect("anchors json"),
        "anchor_hashes must be sent with the activation request"
    );
    // 机器码指纹同样随请求发送。
    assert_eq!(activation.1["machine_code"], mid);

    // 设备签名密钥已持久化在持久卷约定路径（data_dir/license/device-ed25519.key）。
    let key_path = data_dir.join(DEVICE_KEY_SUBDIR).join(DEVICE_KEY_FILE);
    assert!(
        key_path.is_file(),
        "device key must persist: {}",
        key_path.display()
    );

    shared.request_shutdown();
    handle.await.expect("run task joins").expect("run ok");
}

// ============================================================================
// 2. 装配失败 → daemon 照常启动 + 可解释原因 + 北向闸门关闭
// ============================================================================

/// 记录型审计出口（北向运行期夹具；接线证明用）。
#[derive(Default)]
struct CountingAuditSink {
    emitted: Mutex<usize>,
}

impl AuditSink for CountingAuditSink {
    fn emit(&self, events: Vec<daemon::backpressure::BackpressureAudit>) {
        if let Ok(mut guard) = self.emitted.lock() {
            *guard = guard.saturating_add(events.len());
        }
    }
}

/// 在临时目录打开一个真实 [`OfflineQueue`]。
fn temp_queue(dir: &std::path::Path) -> Arc<OfflineQueue> {
    let cfg = QueueConfig::new(dir.join("queue.db"), "gw-asm").expect("queue cfg");
    Arc::new(OfflineQueue::open(cfg, Arc::new(SystemClock)).expect("open queue"))
}

#[tokio::test(start_paused = true)]
async fn assembly_failure_keeps_daemon_running_and_closes_north() {
    let dir = tempfile::tempdir().expect("tempdir");
    let config_path = write_test_config(dir.path());
    let queue = temp_queue(dir.path());

    let shared = DaemonShared::new();
    let builder = BootstrapBuilder::new(config_path)
        .with_shared(shared.clone())
        .without_signal_handlers()
        .with_license_assembly_failed(
            "machine identity failed (anchor quorum failed: collected 1 usable of 2 \
             required); recovery: mount host anchors and restart",
        )
        .with_north_runtime(NorthRuntimeConfig::new(
            queue,
            Arc::new(SystemClock),
            Arc::new(CountingAuditSink::default()),
        ));

    let handle = wait_for_state(
        tokio::spawn(builder.run()),
        &shared,
        LifecycleState::Running,
    )
    .await;

    // ① daemon 照常启动（本地采集不受影响）：state == Running、授权运行期缺席。
    assert!(
        shared.license_runtime().is_none(),
        "assembly failure ⇒ no license runtime"
    );
    // ② 可解释原因落入共享态（含 quorum 数字与恢复路径）。
    let reason = shared
        .license_assembly_error()
        .expect("assembly failure must be observable on DaemonShared");
    assert!(reason.contains("quorum"), "explainable reason: {reason}");
    assert!(reason.contains("recovery"), "recovery path: {reason}");

    // ③ 北向闸门保持关闭：gated_cycles 增长、pump 不执行（fail-closed）。
    let north = shared.north_runtime().expect("north runtime must be wired");
    let mut gated = 0u64;
    for _ in 0..200 {
        tokio::time::advance(Duration::from_millis(50)).await;
        tokio::task::yield_now().await;
        gated = north.stats().gated_cycles;
        if gated > 0 {
            break;
        }
    }
    assert!(gated > 0, "northbound must be gated (assembly failure)");
    assert_eq!(north.stats().pumps, 0, "no pump while gated");

    shared.request_shutdown();
    handle.await.expect("run task joins").expect("run ok");
}

// ============================================================================
// 3. 未配置授权 → 行为与旧版一致（授权缺席、北向无闸门恒放行）
// ============================================================================

#[tokio::test(start_paused = true)]
async fn licensing_not_configured_keeps_legacy_ungated_behavior() {
    let dir = tempfile::tempdir().expect("tempdir");
    let config_path = write_test_config(dir.path());
    let queue = temp_queue(dir.path());

    let shared = DaemonShared::new();
    let builder = BootstrapBuilder::new(config_path)
        .with_shared(shared.clone())
        .without_signal_handlers()
        .with_north_runtime(NorthRuntimeConfig::new(
            queue,
            Arc::new(SystemClock),
            Arc::new(CountingAuditSink::default()),
        ));

    let handle = wait_for_state(
        tokio::spawn(builder.run()),
        &shared,
        LifecycleState::Running,
    )
    .await;

    assert!(
        shared.license_runtime().is_none(),
        "not configured ⇒ no runtime"
    );
    assert!(
        shared.license_assembly_error().is_none(),
        "not configured is not an error"
    );

    // 无闸门 ⇒ 恒放行：多个驱动拍后 gated_cycles 恒 0。
    for _ in 0..10 {
        tokio::time::advance(Duration::from_millis(50)).await;
        tokio::task::yield_now().await;
    }
    assert_eq!(
        shared
            .north_runtime()
            .expect("north wired")
            .stats()
            .gated_cycles,
        0,
        "no gate ⇒ no gating ever (legacy parity)"
    );

    shared.request_shutdown();
    handle.await.expect("run task joins").expect("run ok");
}

// ============================================================================
// 4. 生产 HTTPS transport：loopback 真实 HTTP 联调（127.0.0.1，axum echo）
// ============================================================================

// 注意：post_json 可能阻塞调用线程（低频路径、超时有限）——这两个测试用
// 多线程 runtime，保证 loopback 服务的任务在其它 worker 上照常推进。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn https_transport_posts_json_over_real_loopback_http() {
    use axum::Json;

    async fn echo(Json(body): Json<serde_json::Value>) -> Json<serde_json::Value> {
        Json(serde_json::json!({ "echo": body, "ok": true }))
    }

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind loopback");
    let addr = listener.local_addr().expect("local addr");
    tokio::spawn(async move {
        let app = axum::Router::new().route("/v1/activate", axum::routing::post(echo));
        axum::serve(listener, app).await.expect("serve");
    });

    let transport = HttpsTransport::new(Duration::from_secs(5), Duration::from_secs(5));
    let body = serde_json::json!({
        "machine_code": "a".repeat(64),
        "anchor_hashes": ["h0", "h1"],
    });
    let response = transport
        .post_json(&format!("http://{addr}/v1/activate"), &body)
        .expect("post over real loopback http");
    assert_eq!(response["ok"], true, "response: {response}");
    assert_eq!(response["echo"]["machine_code"], "a".repeat(64));
    assert_eq!(response["echo"]["anchor_hashes"][1], "h1");
}

/// 非 2xx：服务端明确拒绝 → `AuthError` 且错误提示透出服务端错误码。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn https_transport_maps_server_rejection_to_auth_error() {
    use axum::Json;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind loopback");
    let addr = listener.local_addr().expect("local addr");
    tokio::spawn(async move {
        let app = axum::Router::new().route(
            "/v1/activate",
            axum::routing::post(|| async {
                (
                    axum::http::StatusCode::FORBIDDEN,
                    Json(serde_json::json!({"error": "code_revoked"})),
                )
            }),
        );
        axum::serve(listener, app).await.expect("serve");
    });

    let transport = HttpsTransport::new(Duration::from_secs(5), Duration::from_secs(5));
    let err = transport
        .post_json(
            &format!("http://{addr}/v1/activate"),
            &serde_json::json!({}),
        )
        .expect_err("4xx must be an error");
    match &err {
        DaemonError::AuthError(message) => {
            assert!(message.contains("403"), "status in message: {message}");
            assert!(
                message.contains("code_revoked"),
                "server error code surfaced: {message}"
            );
        }
        other => panic!("expected AuthError, got {other:?}"),
    }
}

// ============================================================================
// 5. 试用到期 → 审计 TrialExpired 恰好记录一次（task 23 接线）
// ============================================================================

#[tokio::test]
async fn trial_expiry_records_audit_event_exactly_once() {
    let dir = tempfile::tempdir().expect("tempdir");
    let logger =
        Arc::new(AuditLogger::open(&dir.path().join("audit.db"), None).expect("open audit db"));

    let (mid, _anchors) = test_identity();
    let signer = Arc::new(
        daemon::auth::signing::AuthSigner::new(
            Arc::new(daemon::auth::signing::StaticKeyProvider::new(
                TEST_ONLY_SEED,
            )),
            Arc::new(AlwaysGate),
            mid,
        )
        .expect("mid is 64-hex"),
    );
    let client_cfg =
        daemon::auth::client::LicensingClientConfig::new("https://licensing.test/v1", 24, 10, 30)
            .expect("url ok");
    let client = Arc::new(daemon::auth::client::LicensingClient::new(
        client_cfg,
        signer,
        "0".repeat(64),
    ));

    let now = now_arc();
    let mut rt_cfg =
        LicenseRuntimeConfig::new(client, dir.path().join("data")).with_now_ms(now_closure(&now));
    // 挂审计（bootstrap 生产路径在装配期完成同样的挂载）。
    rt_cfg.audit = Some(Arc::clone(&logger));
    let rt = daemon::license::LicenseRuntime::new(rt_cfg);

    rt.step().await.expect("step ok");
    assert!(matches!(rt.state(), LicenseState::Trial { .. }));

    // 推进 4 天 → 试用到期 → Degraded + TrialExpired 审计。
    now.fetch_add(4 * DAY_MS, Ordering::SeqCst);
    rt.step().await.expect("step ok");
    assert!(matches!(rt.state(), LicenseState::Degraded { .. }));

    let rows = logger.query(&AuditQuery::new()).expect("query audit");
    let expired: Vec<_> = rows
        .iter()
        .filter(|row| row.event == "trial_expired")
        .collect();
    assert_eq!(
        expired.len(),
        1,
        "exactly one trial_expired event: {rows:?}"
    );
    assert_eq!(expired[0].outcome, "degraded");
    assert!(
        expired[0].detail.contains("trial"),
        "detail: {}",
        expired[0].detail
    );

    // 再步进：状态机已在 Degraded，**不得**重复记录（防审计刷屏）。
    rt.step().await.expect("step ok");
    let rows = logger.query(&AuditQuery::new()).expect("query audit");
    assert_eq!(
        rows.iter()
            .filter(|row| row.event == "trial_expired")
            .count(),
        1,
        "audit must not repeat after the transition"
    );
}

/// 恒开放行的测试闸门（`AuthSigner` 构造需要）。
struct AlwaysGate;
impl daemon::auth::signing::LicenseGate for AlwaysGate {
    fn can_sign(&self) -> bool {
        true
    }
}

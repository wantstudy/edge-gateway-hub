//! **授权运行期端到端测试**（task 22/23/24 接线层 `daemon::license::LicenseRuntime`）。
//!
//! 覆盖门禁要求的 9 组场景：
//! 1. 首次启动 → `Trial { days_left: 3 }`；
//! 2. 3 天推进（虚拟时间）→ `Degraded` 且不可转发，reason 可读；
//! 3. 有激活码 + 合法 `lease_token` → `Licensed`；
//! 4. `Licensed` 后断网 → `Grace` 且**仍可转发**（B 档红线）；
//! 5. 断网 6 天仍 `Grace`；断网 8 天 → `Degraded`；
//! 6. 宽限内恢复联网 → 回到 `Licensed`；
//! 7. 试用标记被篡改 → `Degraded`（fail-closed），**不得**重置为 3 天；
//! 8. `north_forward_allowed()` 状态矩阵（在 `license.rs` 单测覆盖）；
//! 9. bootstrap 缺省注入（在 `bootstrap.rs` 单测覆盖）。
//!
//! **纪律**：不真实 sleep、不联网——mock [`LicenseTransport`] 返回构造好的 JSON，
//! 注入虚拟 `now_ms` 驱动时间。lease token 由测试侧按**公开契约**（服务端形状）签发，
//! 与 `licensing_contract_conformance.rs` 的跨端口径一致。

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use base64::engine::general_purpose::STANDARD as B64;
use base64::engine::general_purpose::STANDARD_NO_PAD as B64NP;
use base64::Engine as _;
use ed25519_dalek::{Signer, SigningKey};

use daemon::auth::client::{
    LicenseState, LicenseTransport, LicensingClient, LicensingClientConfig,
};
use daemon::auth::machine_id::{FingerprintKey, MachineIdentity, StaticAnchor};
use daemon::auth::signing::{AuthSigner, LicenseGate, StaticKeyProvider};
use daemon::auth::trial::TrialMarker;
use daemon::error::DaemonError;
use daemon::license::{LicenseRuntime, LicenseRuntimeConfig};

// ============================================================================
// 测试常量（**仅测试，禁止真实部署**）
// ============================================================================

/// **TEST_ONLY_** 设备私钥种子（32 字节；禁止真实部署）。
const TEST_ONLY_SEED: [u8; 32] = *b"iotdaq-license-runtime-test-seed";
/// 测试指纹 HMAC key（派 mid 用，**仅测试**）。
const TEST_ONLY_FP_KEY: &[u8] = b"TEST_ONLY_license_runtime_fp_key";
/// 租约失效时刻（UTC 秒，2100-01-01）——远未来，保证相对真实系统时钟不过期。
const VALID_UNTIL: i64 = 4_102_444_800;
/// 租约签发时刻（UTC 秒；VALID_UNTIL 前 365 天）。
const ISSUED_AT: i64 = VALID_UNTIL - 365 * 86_400;
/// 虚拟时间基准（Unix 毫秒，2023-11-14）——仅用于试用 / 宽限的相对推进。
const T0_MS: u64 = 1_700_000_000_000;
/// 一天毫秒数。
const DAY_MS: u64 = 86_400_000;
/// 心跳周期（秒；测试用短周期以在虚拟时间内触发多拍）。
const HEARTBEAT_SECS: u64 = 60;

// ============================================================================
// 测试夹具
// ============================================================================

/// 恒开放行的测试闸门（`AuthSigner` 需要；本测试不签名 AuthBlock）。
struct AlwaysGate;
impl LicenseGate for AlwaysGate {
    fn can_sign(&self) -> bool {
        true
    }
}

/// 经 task 3 指纹模块派生 mid（64 hex）与逐锚点哈希集。
fn test_identity() -> (String, Vec<String>) {
    let identity = MachineIdentity::new(
        vec![Box::new(StaticAnchor::new(
            "license-anchor",
            Some("gw-license-001"),
        ))],
        1,
        FingerprintKey::from_bytes(TEST_ONLY_FP_KEY.to_vec()).expect("test-only fp key non-empty"),
    );
    let mid = identity
        .get_machine_fingerprint()
        .expect("quorum ok: 1 usable anchor");
    let anchors = identity
        .get_anchor_hashes()
        .expect("quorum ok: 1 usable anchor");
    (mid, anchors)
}

/// 追加一个长度前缀字段：`|name=<字节长度>:<value>`（与服务端 lease 签名域同口径）。
fn push_len_field(out: &mut String, name: &str, value: &str) {
    out.push('|');
    out.push_str(name);
    out.push('=');
    out.push_str(&value.len().to_string());
    out.push(':');
    out.push_str(value);
}

/// 渲染 Lease Token 签名域串（公开契约；与 `client.rs::render_signing_message` 逐字节一致）。
fn render_lease_signing_message(kid: &str, claims: &serde_json::Value) -> Vec<u8> {
    let s = |k: &str| claims[k].as_str().unwrap_or_default().to_string();
    let n = |k: &str| claims[k].as_i64().unwrap_or_default().to_string();
    let mut out = String::new();
    out.push_str("iotdaq.lease.v1");
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

/// 构造一个合法三段式 Lease Token（`kid.payload.sig`）。
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

/// 构造激活成功响应体。
fn activate_ok(token: &str) -> serde_json::Value {
    serde_json::json!({ "lease_token": token })
}

/// 构造心跳成功响应体（服务端 `HeartbeatResponse` 形状，**不含新 Token**）。
fn heartbeat_ok() -> serde_json::Value {
    serde_json::json!({
        "server_time": (VALID_UNTIL - 86_400).to_string(),
        "next_deadline": VALID_UNTIL.to_string(),
        "valid_until": VALID_UNTIL.to_string(),
        "verify_mode": "B",
        "tier": "standard",
        "sig": "server-response-sig",
    })
}

// ---- 可编程 mock 传输 ----

/// mock 动作。
#[derive(Default)]
enum MockAction {
    /// 返回成功响应体。
    Ok(serde_json::Value),
    /// 返回网络错误（模拟断网）。
    NetFail,
    /// 未编程（默认）——按网络错误处理，避免误放行。
    #[default]
    Unset,
}

/// 内部可变态。
#[derive(Default)]
struct MockInner {
    action: MockAction,
    calls: Vec<String>,
}

/// 测试用网络传输假实现（记录调用并返回编程好的响应）。
#[derive(Default)]
struct MockTransport {
    inner: Mutex<MockInner>,
}

impl MockTransport {
    /// 编程：下一次请求返回给定响应体。
    fn respond_with(&self, value: serde_json::Value) {
        self.inner.lock().expect("mock lock").action = MockAction::Ok(value);
    }

    /// 编程：下一次请求返回网络错误（断网）。
    fn fail_network(&self) {
        self.inner.lock().expect("mock lock").action = MockAction::NetFail;
    }

    /// 调用过的 URL 列表。
    fn calls(&self) -> Vec<String> {
        self.inner.lock().expect("mock lock").calls.clone()
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
            MockAction::Ok(value) => Ok(value.clone()),
            MockAction::NetFail => Err(DaemonError::NetworkError("mock: link down".to_string())),
            MockAction::Unset => Err(DaemonError::NetworkError(
                "mock: not programmed".to_string(),
            )),
        }
    }
}

// ---- 构建器 ----

/// 构造授权客户端（注入 mock 传输 + kid-a 公钥 + 设备签名器 + 逐锚点哈希集）。
fn build_client(
    transport: Arc<MockTransport>,
    mid: &str,
    anchor_hashes: Vec<String>,
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
        .with_anchor_hashes(anchor_hashes);
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

/// 虚拟时间源（毫秒）。
fn now_arc() -> Arc<AtomicU64> {
    Arc::new(AtomicU64::new(T0_MS))
}

/// 把 `Arc<AtomicU64>` 包成注入用的时间闭包。
fn now_closure(now: &Arc<AtomicU64>) -> Arc<dyn Fn() -> u64 + Send + Sync> {
    let store = Arc::clone(now);
    Arc::new(move || store.load(Ordering::SeqCst))
}

/// 纯本地 C 档运行时（无 cloud_url、无激活码）。
fn local_runtime(
    client: Arc<LicensingClient>,
    data_dir: PathBuf,
    now: &Arc<AtomicU64>,
) -> LicenseRuntime {
    LicenseRuntime::new(LicenseRuntimeConfig::new(client, data_dir).with_now_ms(now_closure(now)))
}

/// 联网运行时（有 cloud_url + 激活码 + 短心跳周期）。
fn cloud_runtime(
    client: Arc<LicensingClient>,
    data_dir: PathBuf,
    now: &Arc<AtomicU64>,
) -> LicenseRuntime {
    LicenseRuntime::new(
        LicenseRuntimeConfig::new(client, data_dir)
            .with_cloud_url("https://licensing.test/v1")
            .with_activation_code("ACT-TEST-0001")
            .with_heartbeat_interval(Duration::from_secs(HEARTBEAT_SECS))
            .with_now_ms(now_closure(now)),
    )
}

/// 简单夹具：mock + mid + client + 独立 data_dir。
struct Fixture {
    mock: Arc<MockTransport>,
    mid: String,
    client: Arc<LicensingClient>,
    data_dir: PathBuf,
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
            now: now_arc(),
            _dir: dir,
        }
    }

    fn local(&self) -> LicenseRuntime {
        local_runtime(Arc::clone(&self.client), self.data_dir.clone(), &self.now)
    }

    fn cloud(&self) -> LicenseRuntime {
        cloud_runtime(Arc::clone(&self.client), self.data_dir.clone(), &self.now)
    }

    fn advance(&self, delta_ms: u64) {
        self.now.fetch_add(delta_ms, Ordering::SeqCst);
    }

    fn token(&self) -> String {
        build_lease_token(&TEST_ONLY_SEED, "kid-a", &self.mid, "B")
    }
}

// ============================================================================
// 1. 首次启动 → Trial { days_left: 3 }
// ============================================================================

#[tokio::test]
async fn first_start_is_trial_three_days() {
    let fx = Fixture::new();
    let rt = fx.local();
    rt.step().await.expect("step ok");

    match rt.state() {
        LicenseState::Trial { days_left } => assert_eq!(days_left, 3, "fresh trial is 3 days"),
        other => panic!("expected Trial, got {other:?}"),
    }
    assert!(
        rt.north_forward_allowed(),
        "trial allows northbound forward"
    );
    assert!(
        rt.trial_marker_path().exists(),
        "trial marker must be persisted under data_dir"
    );
    // 无联网配置 ⇒ 绝不发网络请求。
    assert!(
        fx.mock.calls().is_empty(),
        "local mode must not call network"
    );
}

// ============================================================================
// 2. 3 天推进 → Degraded（不可转发，reason 可读）
// ============================================================================

#[tokio::test]
async fn trial_expiry_degrades_after_three_days() {
    let fx = Fixture::new();
    let rt = fx.local();
    rt.step().await.expect("step ok");
    assert!(matches!(rt.state(), LicenseState::Trial { .. }));

    // 推进到首启之后 3 天零 1 小时（越过 72h 窗口）。
    fx.advance(3 * DAY_MS + 3_600_000);
    rt.step().await.expect("step ok");

    match rt.state() {
        LicenseState::Degraded { reason } => {
            assert!(
                reason.to_lowercase().contains("trial"),
                "readable reason: {reason}"
            );
        }
        other => panic!("expected Degraded after trial expiry, got {other:?}"),
    }
    assert!(
        !rt.north_forward_allowed(),
        "degraded must stop northbound forward"
    );
    // 降级 ≠ 停用：本地采集判据仍为真。
    assert!(rt.state().allows_local_capture());
}

// ============================================================================
// 3. 有激活码 + 合法 lease_token → Licensed
// ============================================================================

#[tokio::test]
async fn activation_with_valid_token_licenses() {
    let fx = Fixture::new();
    let token = fx.token();
    fx.mock.respond_with(activate_ok(&token));

    let rt = fx.cloud();
    rt.step().await.expect("step ok");

    match rt.state() {
        LicenseState::Licensed { lease } => {
            assert_eq!(lease.lease_id, "lease-0001");
            assert_eq!(lease.kid, "kid-a");
        }
        other => panic!("expected Licensed, got {other:?}"),
    }
    assert!(rt.north_forward_allowed());
    assert!(
        fx.mock.calls().iter().any(|u| u.ends_with("/activate")),
        "must hit the /activate endpoint: {:?}",
        fx.mock.calls()
    );
}

// ============================================================================
// 4. Licensed 后断网 → Grace，且仍可转发（B 档红线）
// ============================================================================

#[tokio::test]
async fn network_failure_after_license_enters_grace_and_still_forwards() {
    let fx = Fixture::new();
    let token = fx.token();
    fx.mock.respond_with(activate_ok(&token));
    let rt = fx.cloud();
    rt.step().await.expect("activate ok");
    assert!(matches!(rt.state(), LicenseState::Licensed { .. }));

    // 断网 + 越过一个心跳周期 → 心跳失败 → Grace（不得立即降级）。
    fx.mock.fail_network();
    fx.advance(HEARTBEAT_SECS * 1000 + 1_000);
    rt.step().await.expect("step ok");

    match rt.state() {
        LicenseState::Grace { days_left, .. } => {
            assert!(days_left > 0, "fresh grace has days left: {days_left}");
        }
        other => panic!("expected Grace after offline heartbeat, got {other:?}"),
    }
    assert!(
        rt.north_forward_allowed(),
        "B 档红线：断网宽限内必须继续转发"
    );
    assert!(
        fx.mock.calls().iter().any(|u| u.ends_with("/heartbeat")),
        "must hit the /heartbeat endpoint: {:?}",
        fx.mock.calls()
    );
}

// ============================================================================
// 5. 断网 6 天仍 Grace；断网 8 天 → Degraded
// ============================================================================

#[tokio::test]
async fn grace_survives_six_days_and_degrades_by_eight() {
    let fx = Fixture::new();
    let token = fx.token();
    fx.mock.respond_with(activate_ok(&token));
    let rt = fx.cloud();
    rt.step().await.expect("activate ok"); // last_online = T0_MS
    assert!(matches!(rt.state(), LicenseState::Licensed { .. }));

    // 断网 6 天：仍宽限、可转发。
    fx.mock.fail_network();
    fx.advance(6 * DAY_MS);
    rt.step().await.expect("step ok");
    match rt.state() {
        LicenseState::Grace { days_left, .. } => {
            assert_eq!(days_left, 1, "6 天断网 ⇒ 宽限剩 1 天");
        }
        other => panic!("expected Grace at 6 days offline, got {other:?}"),
    }
    assert!(rt.north_forward_allowed());

    // 断网 8 天：宽限耗尽 → Degraded、不可转发。
    fx.advance(2 * DAY_MS);
    rt.step().await.expect("step ok");
    match rt.state() {
        LicenseState::Degraded { reason } => {
            assert!(
                reason.to_lowercase().contains("grace"),
                "readable reason: {reason}"
            );
        }
        other => panic!("expected Degraded at 8 days offline, got {other:?}"),
    }
    assert!(!rt.north_forward_allowed());
}

// ============================================================================
// 6. 宽限内恢复联网 → 回到 Licensed
// ============================================================================

#[tokio::test]
async fn recovery_within_grace_returns_to_licensed() {
    let fx = Fixture::new();
    let token = fx.token();
    fx.mock.respond_with(activate_ok(&token));
    let rt = fx.cloud();
    rt.step().await.expect("activate ok");

    // 断网 2 天 → Grace。
    fx.mock.fail_network();
    fx.advance(2 * DAY_MS);
    rt.step().await.expect("step ok");
    assert!(matches!(rt.state(), LicenseState::Grace { .. }));

    // 恢复联网 → 下一次心跳成功 → Licensed。
    fx.mock.respond_with(heartbeat_ok());
    fx.advance(HEARTBEAT_SECS * 1000 + 1_000);
    rt.step().await.expect("step ok");

    assert!(
        matches!(rt.state(), LicenseState::Licensed { .. }),
        "recovery must return to Licensed, got {}",
        rt.state().name()
    );
    assert!(rt.north_forward_allowed());
}

// ============================================================================
// 7. 试用标记被篡改 → Degraded（fail-closed，不得重置为 3 天）
// ============================================================================

#[tokio::test]
async fn tampered_trial_marker_fails_closed() {
    let fx = Fixture::new();
    let rt = fx.local();
    rt.step().await.expect("step ok");
    assert!(matches!(rt.state(), LicenseState::Trial { days_left: 3 }));

    // 篡改标记文件的 1 个字节（sig 十六进制区）。
    let path = rt.trial_marker_path();
    let mut bytes = std::fs::read(&path).expect("marker readable");
    let last = bytes.len();
    assert!(last >= 3, "marker json is non-trivial");
    bytes[last - 3] ^= 0x01;
    std::fs::write(&path, &bytes).expect("write tampered marker");

    // 篡改后步进（时间未到 72h）：必须 fail-closed，绝不重置为 3 天。
    rt.step().await.expect("step ok");
    match rt.state() {
        LicenseState::Degraded { reason } => {
            assert!(!reason.is_empty(), "degraded reason must be readable");
        }
        LicenseState::Trial { days_left } => {
            panic!("tampered marker must NOT reset trial (got days_left={days_left})")
        }
        other => panic!("expected Degraded on tamper, got {other:?}"),
    }
    assert!(!rt.north_forward_allowed());
    // 落盘标记仍是篡改后的原样（未被「修复 / 重签」）。
    assert_eq!(
        std::fs::read(&path).expect("read back"),
        bytes,
        "fail-closed: tampered marker must not be silently rewritten"
    );
}

// ============================================================================
// 附加：篡改后的标记内容仍可被解析（证明判定来自 HMAC，而非解析失败）
// ============================================================================

#[tokio::test]
async fn tampered_marker_still_parses_but_signature_fails() {
    let fx = Fixture::new();
    let rt = fx.local();
    rt.step().await.expect("step ok");

    let path = rt.trial_marker_path();
    let raw = std::fs::read_to_string(&path).expect("marker readable");
    let mut marker: TrialMarker = serde_json::from_str(&raw).expect("marker parses");
    marker.sig = "0".repeat(64); // 合法 hex 但内容错
    std::fs::write(&path, serde_json::to_string(&marker).expect("serialize")).expect("write");

    rt.step().await.expect("step ok");
    assert!(
        matches!(rt.state(), LicenseState::Degraded { .. }),
        "bad-hmac marker must degrade, got {}",
        rt.state().name()
    );
}

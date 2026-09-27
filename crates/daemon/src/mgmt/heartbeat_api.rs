//! 设备心跳上报端点（wave2 由 `mgmt/mod.rs` 挂载，函数签名已就绪）。
//!
//! # 路由接线
//! 模块已在 `lib.rs` 经 `#[path = "mgmt/heartbeat_api.rs"]` 注册（`pub mod heartbeat_api`），
//! 本文件**不修改** `mgmt/mod.rs`，仅提供就绪函数。wave2 接线示例：
//! ```ignore
//! use crate::heartbeat_api::heartbeat;
//! Router::new().route("/api/devices/:id/heartbeat", post(heartbeat))
//! ```
//!
//! # 语义（诚实红线）
//! - 成功上报 200：回显 `last_beat_at`（epoch 毫秒，**字符串**，大数红线）。
//! - 设备不存在 / 参数非法 / 存储写入失败 → 结构化错误（`{error, message, code}`），
//!   **绝不返回 200 伪造成功**。
//! - 心跳写入是低危安全操作，**不落审计**（与危险写操作 / 配置写区分；需审计者
//!   见 `writeapi` / `audit_api` 范式）。
//!
//! # 与既有轮询三态的关系
//! 心跳是**主动探活**，轮询三态（`health::status_for`）是**被动推导**——二者为**并列
//! 独立维度**，本端点只回显 `beat_status`（`online` / `stale` / `unknown`），**绝不覆盖**
//! `/api/devices` 由 `health::status_for` 计算的三态 `status`。呈现层若需合并，由
//! `mgmt::mod.rs`（wave2）在现有设备行**追加** `last_beat_at` / `beat_status` 字段完成。

use axum::body::to_bytes as body_to_bytes;
use axum::extract::{Path as AxumPath, Request, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Json, Response};
use serde_json::{json, Value};

use crate::config::GatewayConfig;
use crate::error::ERR_DEVICE_STATE;
use crate::mgmt::rbac::{AuthedRole, Permission};
use crate::mgmt::MgmtState;

/// 请求体最大字节数（心跳只携带一个 `t` 时间戳，上限很小）。
const MAX_BEAT_BODY_BYTES: usize = 4_096;

/// `POST /api/devices/:id/heartbeat` → 设备主动探活上报。
///
/// 成功 200 回显 `last_beat_at`（字符串毫秒）+ `beat_status`；任何失败（设备不存在 /
/// 参数非法 / 持久化失败 / 持久后端不可用）均返回非 200 结构化错误。
pub async fn heartbeat(
    State(state): State<MgmtState>,
    authed: AuthedRole,
    AxumPath(id): AxumPath<String>,
    request: Request,
) -> Response {
    // ① RBAC：已认证且具备设备侧可见性（`device.view`）即可上报；心跳为低危安全
    //    操作，不在此处要求 `device.write`。
    if let Err(rejection) = authed.ensure(Permission::DeviceView) {
        return rejection.into_response();
    }

    // ② 参数解析（400 先于任何设备判定 / 写）。
    let at_ms = match parse_beat_payload(request).await {
        Ok(maybe) => match maybe {
            Some(ts) => ts,
            None => crate::mgmt::health::now_ms(),
        },
        Err((reason, code)) => return bad_request(&reason, code),
    };

    // ③ 设备存在性（404，诚实拒绝；绝不 200）。
    let config = state.config();
    if !device_exists(&config, &id) {
        return not_found(&format!(
            "device {id:?} is not registered and owns no point; heartbeat rejected"
        ));
    }

    // ④ 写心跳（持久化失败 / 后端不可用 → 如实 500 / 503，严禁 200 伪造成功）。
    let registry = state.daemon().health_registry();
    let stored = match registry.record_beat(&id, at_ms) {
        Ok(ts) => ts,
        Err(err) => {
            tracing::warn!(error = %err, device = %id, "heartbeat: persist failed");
            return internal(&format!("heartbeat persist failed: {err}"), err.error_code());
        }
    };
    // 持久后端未能挂载（惰性打开失败）→ 本次心跳仅存内存、重启即失：如实 503，
    // 不冒充 200 成功。
    if !registry.beat_store_mounted() {
        return service_unavailable(
            "heartbeat persistent store could not be opened; beat kept in-memory only \
             and will not survive a restart",
        );
    }

    let now = crate::mgmt::health::now_ms();
    let status = match registry.beat_status(&id, now) {
        Ok(s) => s,
        Err(err) => {
            return internal(&format!("heartbeat read failed: {err}"), err.error_code());
        }
    };

    Json(json!({
        "id": id,
        // 大数红线：epoch 毫秒一律字符串。
        "last_beat_at": stored.to_string(),
        "beat_status": status.as_str(),
        "beat_window_ms": crate::mgmt::health::BEAT_FRESH_WINDOW_MS.to_string(),
    }))
    .into_response()
}

/// 解析可选 JSON 体中的 `t`（epoch 毫秒**字符串**）；返回上报时刻或 `None`（服务端取当前）。
///
/// `None` 用于「无体 / 空体 / 无 `t` 字段」——均视为「由服务端取当前时刻」，合法。
async fn parse_beat_payload(request: Request) -> Result<Option<u64>, (String, u16)> {
    let bytes = match body_to_bytes(request.into_body(), MAX_BEAT_BODY_BYTES).await {
        Ok(b) => b,
        Err(e) => {
            return Err((format!("request body unreadable or too large: {e}"), 400));
        }
    };
    if bytes.is_empty() {
        return Ok(None);
    }
    let text = match String::from_utf8(bytes.to_vec()) {
        Ok(t) => t,
        Err(_) => return Err(("request body is not valid UTF-8".to_string(), 400)),
    };
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }
    let value: Value = match serde_json::from_str(trimmed) {
        Ok(v) => v,
        Err(e) => return Err((format!("body must be a JSON object: {e}"), 400)),
    };
    // 体存在但无 `t` 字段 → 服务端取当前时刻（合法）。
    // 若 `t` 字段存在但类型不对（非字符串）→ 拒绝：必须为十进制毫秒字符串（大数红线）。
    let Some(raw_value) = value.get("t") else {
        return Ok(None);
    };
    let Some(raw) = raw_value.as_str() else {
        return Err((
            "field `t` must be a decimal epoch-millisecond STRING (bignum red line)".to_string(),
            400,
        ));
    };
    // 大数红线：`t` 必须是十进制毫秒**字符串**，绝不允许裸 number 越界。
    let ts: u64 = raw.parse().map_err(|_| {
        (
            format!(
                "field `t` must be a decimal epoch-millisecond STRING (bignum red line), got {raw:?}"
            ),
            400,
        )
    })?;
    let now = crate::mgmt::health::now_ms();
    if ts > now.saturating_add(crate::mgmt::health::MAX_FUTURE_SKEW_MS) {
        return Err((
            format!("field `t` is in the future beyond allowed skew; got {ts} (now≈{now})"),
            400,
        ));
    }
    if ts < crate::mgmt::health::MIN_PLAUSIBLE_BEAT_MS {
        return Err((
            format!("field `t` is not a plausible epoch-millisecond timestamp; got {ts}"),
            400,
        ));
    }
    Ok(Some(ts))
}

/// 设备是否在配置中真实存在（登记段 ∪ 点位行）。
fn device_exists(config: &GatewayConfig, id: &str) -> bool {
    config
        .devices
        .iter()
        .any(|d| d.device_id == id)
        || config.points.iter().any(|p| p.device_id == id)
}

// ---- 响应辅助（与 audit_api / writeapi 同风格） ----

/// 400 + 结构化错误体（含数字错误码）。
fn bad_request(message: &str, code: u16) -> Response {
    (
        StatusCode::BAD_REQUEST,
        Json(json!({ "error": "bad_request", "message": message, "code": code })),
    )
        .into_response()
}

/// 404 + 结构化错误体（设备不存在）。
fn not_found(message: &str) -> Response {
    (
        StatusCode::NOT_FOUND,
        Json(json!({ "error": "device_not_found", "message": message, "code": ERR_DEVICE_STATE })),
    )
        .into_response()
}

/// 500 + 结构化错误体（持久化 / 读取失败；不泄露内部细节之外的内容）。
fn internal(message: &str, code: u16) -> Response {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(json!({ "error": "internal", "message": message, "code": code })),
    )
        .into_response()
}

/// 503 + 结构化错误体（持久后端不可用；fail-closed，绝不冒充成功）。
fn service_unavailable(message: &str) -> Response {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        Json(json!({ "error": "heartbeat_unavailable", "message": message, "code": ERR_DEVICE_STATE })),
    )
        .into_response()
}

// ---------------------------------------------------------------------------
// 测试（自起最小 axum 子路由 + 手写 HTTP；路由接线由 wave2 统一做，这里只验
// handler + 鉴权 + 状态链。沿用 `test_support` 的 HTTP 助手与种子配置。）
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bootstrap::DaemonShared;
    use crate::config::{ConfigShared, GatewayConfig};
    use crate::mgmt::health::BeatStatus;
    use crate::mgmt::rbac::Role;
    use crate::mgmt::test_support::{parse_response, post_json, token_for};
    use axum::routing::post;
    use axum::Router;
    use std::sync::Arc;
    use std::time::Duration;
    use tokio::net::TcpListener;

    /// 构造挂载了心跳态的 MgmtState（独立实例，防并行污染）。
    ///
    /// 不触碰 `bootstrap` / `mod.rs`：注册表经 `DeviceHealthRegistry::new()` 的**惰性
    /// 默认库打开**挂载（首报 / 查询时按 `IOT_DAQ_DATA_DIR` 解析；未设则 `./`）——
    /// 测试间用**独立设备 id** 避免并行共享库的行冲突（`PRAGMA busy_timeout` 已防
    /// 表级 BUSY）。
    fn make_heartbeat_state(device_id: &str) -> MgmtState {
        let toml = format!(
            r#"
[gateway]
gateway_id = "gw-test"

[[outlets]]
name = "north-1"
broker = "mqtt://127.0.0.1:18831"

[[devices]]
device_id = "{device_id}"
name = "设备一"
protocol = "modbus-tcp"
"#
        );
        let dir = std::env::temp_dir().join(format!("iot-daq-hb-{device_id}"));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("config.toml");
        std::fs::write(&path, toml).expect("seed config");
        let config = Arc::new(GatewayConfig::load(&path).expect("load"));
        let daemon = DaemonShared::new();
        daemon.set_config(Arc::new(ConfigShared::new((*config).clone())));
        MgmtState::new(daemon, config)
    }

    /// 在 127.0.0.1 随机端口启动仅含心跳路由的最小服务（不与 mod.rs 路由接线耦合）。
    async fn spawn_heartbeat_server(state: MgmtState) -> u16 {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let port = listener.local_addr().expect("addr").port();
        let app = Router::new()
            .route("/api/devices/:id/heartbeat", post(heartbeat))
            .with_state(state);
        tokio::spawn(async move {
            axum::serve(listener, app).await.expect("serve error");
        });
        port
    }

    /// QA: 成功上报 → 200，回显 `last_beat_at` 为**非空字符串**且可解析为 u64。
    #[tokio::test]
    async fn heartbeat_returns_last_beat_at_as_non_empty_string() {
        let state = make_heartbeat_state("dev-hb-str");
        let sys = token_for(&state, Role::System);
        let port = spawn_heartbeat_server(state).await;

        let (status, _, body) = post_json(
            port,
            "/api/devices/dev-hb-str/heartbeat",
            "{}",
            &sys,
        )
        .await;
        assert_eq!(status, 200, "successful beat must be 200: {body}");
        let value: Value = serde_json::from_str(&body).expect("json");
        // 大数红线：必须是字符串且非空。
        let last_beat = value.get("last_beat_at").expect("field present");
        assert!(last_beat.is_string(), "last_beat_at must be a string (bignum red line)");
        let s = last_beat.as_str().expect("string");
        assert!(!s.is_empty(), "last_beat_at must not be empty");
        assert!(s.parse::<u64>().is_ok(), "must parse to u64 epoch ms");
        assert_eq!(value["beat_status"], "online");
    }

    /// QA（诚实红线）: 未知设备被拒 → 非 200 结构化错误（含错误码），绝不 200。
    #[tokio::test]
    async fn heartbeat_rejects_unknown_device() {
        let state = make_heartbeat_state("dev-hb-unknown");
        let sys = token_for(&state, Role::System);
        let port = spawn_heartbeat_server(state).await;

        let (status, _, body) = post_json(
            port,
            "/api/devices/ghost/heartbeat",
            "{}",
            &sys,
        )
        .await;
        assert_ne!(status, 200, "unknown device must NOT be 200: {body}");
        let value: Value = serde_json::from_str(&body).expect("json");
        assert_eq!(value["error"], "device_not_found");
        assert_eq!(value["code"], ERR_DEVICE_STATE);
        assert!(value["message"].as_str().is_some_and(|m| !m.is_empty()));
    }

    /// QA: 参数非法（`t` 非字符串 / 越界 number）→ 400 结构化错误，绝不 200 伪造成功。
    #[tokio::test]
    async fn heartbeat_rejects_invalid_timestamp() {
        let state = make_heartbeat_state("dev-hb-bad");
        let sys = token_for(&state, Role::System);
        let port = spawn_heartbeat_server(state).await;

        for (payload, why) in [
            (r#"{"t":"not-a-number"}"#, "t must be numeric string"),
            (r#"{"t":1700000000123}"#, "t as number (bignum red line)"),
        ] {
            let (status, _, body) = post_json(
                port,
                "/api/devices/dev-hb-bad/heartbeat",
                payload,
                &sys,
            )
            .await;
            assert_eq!(status, 400, "{why}: {body}");
            let value: Value = serde_json::from_str(&body).expect("json");
            assert_eq!(value["error"], "bad_request");
            assert!(value["message"].as_str().is_some_and(|m| !m.is_empty()));
        }
    }

    /// QA（诚实红线）: 缺 token → 401（JWT 鉴权由 extractor 承担）。
    #[tokio::test]
    async fn heartbeat_requires_auth() {
        let state = make_heartbeat_state("dev-hb-auth");
        let port = spawn_heartbeat_server(state).await;

        let (status, _, _) = tokio::time::timeout(Duration::from_secs(3), async {
            let mut stream = tokio::net::TcpStream::connect(("127.0.0.1", port))
                .await
                .expect("connect");
            let req = "POST /api/devices/dev-hb-auth/heartbeat HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Type: application/json\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}";
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            stream.write_all(req.as_bytes()).await.expect("write");
            stream.flush().await.expect("flush");
            let mut buf = Vec::new();
            stream.read_to_end(&mut buf).await.expect("read");
            parse_response(&String::from_utf8(buf).expect("utf8"))
        })
        .await
        .expect("timeout");
        assert_eq!(status, 401, "missing token must be 401");
    }

    /// QA: 成功上报后写入的 `last_beat_at` 与读路径一致（单设备端到端，落库确认）。
    #[tokio::test]
    async fn heartbeat_written_value_survives_readback() {
        let state = make_heartbeat_state("dev-hb-read");
        let sys = token_for(&state, Role::System);
        let port = spawn_heartbeat_server(state.clone()).await;

        let stamped = 1_700_000_000_123u64;
        let (status, _, body) = post_json(
            port,
            "/api/devices/dev-hb-read/heartbeat",
            &format!(r#"{{"t":"{stamped}"}}"#),
            &sys,
        )
        .await;
        assert_eq!(status, 200, "{body}");
        let value: Value = serde_json::from_str(&body).expect("json");
        assert_eq!(value["last_beat_at"], stamped.to_string());

        // 经注册表读回，确认确实落库（不只是回显）。
        let reg = state.daemon().health_registry();
        assert_eq!(
            reg.last_beat_ms("dev-hb-read").expect("read"),
            Some(stamped)
        );
        assert_eq!(
            reg.beat_status("dev-hb-read", stamped).expect("status"),
            BeatStatus::Online
        );
    }
}

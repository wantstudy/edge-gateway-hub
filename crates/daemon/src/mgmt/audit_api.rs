//! task 26 — 安全审计日志远程拉取端点（mgmt 管理面扩展，只读 + RBAC 门控）。
//!
//! ## 职责边界
//! - **做**：两个只读端点。`GET /api/audit`：在线查询（分页 + 时间窗 + 事件
//!   类型过滤），RBAC = [`Permission::AuditView`]（risk / system）；
//!   `GET /api/audit/export`：导出（同一查询条件 + 链完整性报告），RBAC =
//!   [`Permission::AuditExport`]（**仅 system**——数据出境动作收敛单一角色，
//!   对齐 rbac.rs 契约注释）。
//!   查询 / 导出行为自身落持久审计（`audit_read` / `audit_export` 事件入链，
//!   可追溯「谁看过审计」；本次请求的记录在**本次返回集之外**，属诚实语义）。
//! - **不做**：审计日志前端（task 30）；写路径（审计只增，库触发器强制追加写）。
//!
//! ## 安全契约
//! 1. **门控在 extractor + ensure 双层**：`AuthedRole`（401）+
//!    `authed.ensure(permission)`（403），与 writeapi 同型；
//! 2. **fail-closed**：审计库未装配（bootstrap 挂载失败 / 未注入）→ 503，
//!    绝不静默返回空集冒充「无审计事件」；
//! 3. **大数红线**：`seq` / `ts_ns` / `count` / `limit` / `offset` 一律字符串编码。
//!
//! ## 数据源
//! `state.daemon().audit_logger()`（bootstrap task 26 最小接线挂载到
//! [`DaemonShared`]）；本模块不持有连接、不做侧表注册。

use std::collections::HashMap;

use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Json, Response};
use serde_json::{json, Value};

use super::rbac::{AuthedRole, Permission};
use super::MgmtState;
use crate::audit::{AuditEventType, AuditQuery, ChainVerifyReport, MAX_QUERY_LIMIT};

// ---- REST 处理器 ----

/// GET /api/audit?limit=&amp;offset=&amp;since=&amp;until=&amp;event= → 在线查询（`audit.view`）。
///
/// `since` / `until` 为 UTC **纳秒**时间戳字符串（闭区间；大数红线：字符串编码）；
/// `event` 为 [`AuditEventType::as_str`] 字面量；`limit` 缺省 100、上限 1000。
pub async fn list(
    State(state): State<MgmtState>,
    authed: AuthedRole,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    query_impl(state, authed, params, Permission::AuditView, false).await
}

/// GET /api/audit/export?… → 导出（`audit.export`，仅 system）+ 链完整性报告。
///
/// 响应额外携带 `chain` 字段：`{ok, total, verified, first_broken_seq}`——
/// 导出方（数据出境面）应当同时拿到「这份导出是否完整可信」的证明。
pub async fn export(
    State(state): State<MgmtState>,
    authed: AuthedRole,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    query_impl(state, authed, params, Permission::AuditExport, true).await
}

/// 共同实现：门控 → 取 logger → 参数解析 → 查询 → （导出附链报告）→ 审计自身。
async fn query_impl(
    state: MgmtState,
    authed: AuthedRole,
    params: HashMap<String, String>,
    permission: Permission,
    include_chain: bool,
) -> Response {
    // ① RBAC（403；401 由 extractor 承担）。
    if let Err(rejection) = authed.ensure(permission) {
        return rejection.into_response();
    }

    // ② fail-closed：审计库未装配 → 503（绝不冒充空集）。
    let Some(logger) = state.daemon().audit_logger() else {
        return service_unavailable("audit logger not mounted");
    };

    // ③ 参数解析（400 先于任何查询）。
    let query = match parse_query(&params) {
        Ok(query) => query,
        Err(message) => return bad_request(&message),
    };
    // ③′ 呈现口径「最新在前」：审计日志对用户应是「 newest 在最前 」，否则
    // `limit` 永远只从最旧处起算，本次进程产生的事件被截断在末尾，读起来像
    // 拿到了 stale 数据-seq。`offset` 随之为「从最新往后翻页」。
    // （审计语义 / 哈希链 / outcome·event 判定均不受影响，仅排列方向。）
    let query = query.with_order_desc(true);

    // ④ 查询。
    let rows = match logger.query(&query) {
        Ok(rows) => rows,
        Err(err) => return internal(&format!("audit query failed: {err}")),
    };
    let row_json: Vec<Value> = rows
        .iter()
        .map(crate::audit::AuditRecord::to_json)
        .collect();

    // ⑤ 导出附链完整性报告（数据出境面职责）。
    let chain_json: Option<Value> = if include_chain {
        match logger.verify_chain() {
            Ok(report) => Some(chain_to_json(&report)),
            Err(err) => return internal(&format!("audit chain verify failed: {err}")),
        }
    } else {
        None
    };

    // ⑥ 查询行为自身入链（actor = JWT sub；本次记录不在本次返回集内）。
    let event = if include_chain {
        AuditEventType::AuditExport
    } else {
        AuditEventType::AuditRead
    };
    if let Err(err) = logger.record(
        &authed.claims.sub,
        event,
        crate::audit::OUTCOME_ACCEPTED,
        &format!(
            "returned {} rows (limit={}, offset={})",
            rows.len(),
            query.limit,
            query.offset
        ),
    ) {
        tracing::warn!(error = %err, "audit_api: failed to persist self-audit record");
    }

    let mut body = json!({
        "rows": row_json,
        "count": rows.len().to_string(),
        "limit": query.limit.to_string(),
        "offset": query.offset.to_string(),
    });
    if let Some(chain) = chain_json {
        body["chain"] = chain;
    }
    Json(body).into_response()
}

// ---- 参数解析 ----

/// 解析查询参数（缺省 / 空串 = 不过滤；非法数值 → Err）。
fn parse_query(params: &HashMap<String, String>) -> Result<AuditQuery, String> {
    let mut query = AuditQuery::new();

    if let Some(raw) = non_empty(params.get("limit")) {
        let limit: u32 = raw
            .parse()
            .map_err(|_| format!("limit must be a u32 (1..={MAX_QUERY_LIMIT}), got {raw:?}"))?;
        if limit == 0 {
            return Err(format!("limit must be >= 1, got {raw:?}"));
        }
        query = query.with_limit(limit);
    }
    if let Some(raw) = non_empty(params.get("offset")) {
        let offset: u64 = raw
            .parse()
            .map_err(|_| format!("offset must be a u64, got {raw:?}"))?;
        query = query.with_offset(offset);
    }
    if let Some(raw) = non_empty(params.get("since")) {
        let since_ns: u64 = raw
            .parse()
            .map_err(|_| format!("since must be a u64 nanosecond timestamp string, got {raw:?}"))?;
        query = query.with_since_ns(since_ns);
    }
    if let Some(raw) = non_empty(params.get("until")) {
        let until_ns: u64 = raw
            .parse()
            .map_err(|_| format!("until must be a u64 nanosecond timestamp string, got {raw:?}"))?;
        query = query.with_until_ns(until_ns);
    }
    if let (Some(since), Some(until)) = (query.since_ns, query.until_ns) {
        if until < since {
            return Err(format!(
                "until ({until}) must not be earlier than since ({since})"
            ));
        }
    }
    if let Some(raw) = non_empty(params.get("event")) {
        // 事件类型白名单校验（未知值 400，不做静默通配）。
        let event = AuditEventType::parse(raw)
            .ok_or_else(|| format!("unknown event type {raw:?}; see AuditEventType::as_str"))?;
        query = query.with_event(event.as_str());
    }
    Ok(query)
}

/// 取非空参数（缺省 / 空串 → None）。
fn non_empty(raw: Option<&String>) -> Option<&str> {
    raw.map(String::as_str).filter(|s| !s.is_empty())
}

/// 链完整性报告 → JSON（数值一律字符串，大数红线）。
fn chain_to_json(report: &ChainVerifyReport) -> Value {
    json!({
        "ok": report.ok,
        "total": report.total.to_string(),
        "verified": report.verified.to_string(),
        "first_broken_seq": report
            .first_broken_seq
            .map(|seq| seq.to_string()),
    })
}

// ---- 响应辅助（与 writeapi 同风格） ----

/// 400 + JSON 错误体。
fn bad_request(message: &str) -> Response {
    (
        StatusCode::BAD_REQUEST,
        Json(json!({ "error": "bad_request", "message": message })),
    )
        .into_response()
}

/// 503 + JSON 错误体（审计库未装配；fail-closed：绝不冒充空集）。
fn service_unavailable(message: &str) -> Response {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        Json(json!({ "error": "audit_unavailable", "message": message })),
    )
        .into_response()
}

/// 500 + JSON 错误体（不泄露内部细节之外的内容）。
fn internal(message: &str) -> Response {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(json!({ "error": "internal", "message": message })),
    )
        .into_response()
}

// ---------------------------------------------------------------------------
// 测试（axum 回环端口 + 手写 HTTP，与 mgmt 既有测试手法一致）
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audit::{AuditEventType, AuditLogger, OUTCOME_ACCEPTED, OUTCOME_DENIED};
    use crate::bootstrap::DaemonShared;
    use crate::config::{ConfigShared, GatewayConfig};
    use crate::mgmt::auth_jwt::{now_unix_secs, sign, Claims};
    use crate::mgmt::rbac::Role;
    use std::sync::Arc;
    use std::time::Duration;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::{TcpListener, TcpStream};

    /// 测试 IKM（显式注入，不读进程环境变量）。
    const TEST_IKM: &[u8] = b"audit-api-test-ikm";

    /// 测试配置（最小合法段）。
    const TEST_TOML: &str = r#"
[gateway]
gateway_id = "gw-test"

[[outlets]]
name = "north-1"
broker = "mqtt://127.0.0.1:1883"

[[points]]
device_id = "dev-01"
point_id = "p1"
protocol = "modbus-tcp"
address = "127.0.0.1:502"
frequency_ms = 100
"#;

    /// 构造挂载了审计库的 MgmtState（独立 DaemonShared，不污染其他测试）。
    fn make_state(dir: &std::path::Path) -> MgmtState {
        let config = Arc::new(GatewayConfig::parse(TEST_TOML).expect("parse"));
        let daemon = DaemonShared::new();
        daemon.set_config(Arc::new(ConfigShared::new((*config).clone())));
        let logger =
            AuditLogger::open(&dir.join("audit.db"), Some(TEST_IKM)).expect("open audit db");
        daemon.set_audit_logger(Arc::new(logger));
        MgmtState::new(daemon, config)
    }

    /// task 57：以 state 的签名密钥签发测试 token（sub = "ops-admin"）。
    fn token_for(state: &MgmtState, role: Role) -> String {
        let now = now_unix_secs();
        let claims = Claims {
            sub: "ops-admin".to_string(),
            role: role.as_str().to_string(),
            perms: None,
            exp: now + 600,
            iat: now,
            nbf: None,
            jti: "test-jti".to_string(),
        };
        sign(&claims, state.auth().key()).expect("sign test token")
    }

    /// 在 127.0.0.1 随机端口启动 axum 服务。
    async fn spawn_server(state: MgmtState) -> u16 {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let port = listener.local_addr().expect("addr").port();
        tokio::spawn(async move {
            axum::serve(listener, super::super::router(state))
                .await
                .expect("serve error");
        });
        port
    }

    /// 解析原始 HTTP 响应 → (状态码, 头部文本, body)。
    fn parse_response(raw: &str) -> (u16, String, String) {
        let (head, body) = raw
            .split_once("\r\n\r\n")
            .expect("response must contain header/body separator");
        let status_line = head.lines().next().expect("status line");
        let status: u16 = status_line
            .split_whitespace()
            .nth(1)
            .and_then(|s| s.parse().ok())
            .expect("status code");
        (status, head.to_string(), body.to_string())
    }

    /// 手写 HTTP GET（带可选 Bearer），整体 3s 超时防挂死。
    async fn http_get_bearer(port: u16, path: &str, token: Option<&str>) -> (u16, String, String) {
        tokio::time::timeout(Duration::from_secs(3), async move {
            let mut stream = TcpStream::connect(("127.0.0.1", port))
                .await
                .expect("connect");
            let auth = token
                .map(|t| format!("Authorization: Bearer {t}\r\n"))
                .unwrap_or_default();
            let request = format!(
                "GET {path} HTTP/1.1\r\nHost: 127.0.0.1\r\n{auth}Connection: close\r\n\r\n"
            );
            stream.write_all(request.as_bytes()).await.expect("write");
            stream.flush().await.expect("flush");
            let mut buf = Vec::new();
            stream.read_to_end(&mut buf).await.expect("read");
            parse_response(&String::from_utf8(buf).expect("utf8"))
        })
        .await
        .expect("http_get_bearer timed out")
    }

    /// 预置三条审计事件（不同类型 / 时间）。
    fn seed_events(logger: &AuditLogger) {
        logger
            .record("alice", AuditEventType::Login, OUTCOME_ACCEPTED, "login ok")
            .expect("seed 1");
        logger
            .record(
                "bob",
                AuditEventType::LoginFailed,
                OUTCOME_DENIED,
                "bad password",
            )
            .expect("seed 2");
        logger
            .record(
                "carol",
                AuditEventType::ConfigChange,
                OUTCOME_ACCEPTED,
                "device_create",
            )
            .expect("seed 3");
    }

    // ---- RBAC 门控 ----

    /// QA 安全: 缺 token → 401；ops（不持 audit.view）→ 403；risk → 200。
    #[tokio::test]
    async fn list_is_rbac_gated() {
        let dir = tempfile::tempdir().expect("tempdir");
        let state = make_state(dir.path());
        let logger = state.daemon().audit_logger();
        seed_events(logger.as_ref().expect("logger"));
        let ops = token_for(&state, Role::Ops);
        let risk = token_for(&state, Role::Risk);
        let port = spawn_server(state).await;

        // 缺 token → 401。
        let (status, _, _) = http_get_bearer(port, "/api/audit", None).await;
        assert_eq!(status, 401, "missing token must be 401");

        // ops 不持 audit.view → 403。
        let (status, _, body) = http_get_bearer(port, "/api/audit", Some(&ops)).await;
        assert_eq!(status, 403, "ops must not view audit: {body}");

        // risk 持 audit.view → 200。
        let (status, _, body) = http_get_bearer(port, "/api/audit", Some(&risk)).await;
        assert_eq!(status, 200, "{body}");
    }

    /// QA: 正常查询 → 行集 + 分页 / 事件过滤生效；seq / ts_ns 字符串（大数红线）。
    #[tokio::test]
    async fn list_returns_rows_with_filters_and_string_bignums() {
        let dir = tempfile::tempdir().expect("tempdir");
        let state = make_state(dir.path());
        let logger = state.daemon().audit_logger();
        seed_events(logger.as_ref().expect("logger"));
        let risk = token_for(&state, Role::Risk);
        let port = spawn_server(state).await;

        // 全量。
        let (status, _, body) = http_get_bearer(port, "/api/audit", Some(&risk)).await;
        assert_eq!(status, 200, "{body}");
        let value: Value = serde_json::from_str(&body).expect("json");
        assert_eq!(value["count"], "3");
        assert!(
            value["limit"].is_string() && value["offset"].is_string(),
            "大数红线"
        );
        let rows = value["rows"].as_array().expect("rows");
        assert_eq!(rows.len(), 3);
        for row in rows {
            assert!(row["seq"].is_string(), "seq must be string: {row}");
            assert!(row["ts_ns"].is_string(), "ts_ns must be string: {row}");
        }
        // 呈现口径「最新在前」：rows[0] 为最新事件，rows[2] 为最旧。
        assert_eq!(rows[0]["event"], "config_change");
        assert_eq!(rows[2]["event"], "login");

        // 事件过滤：login_failed → 1 条。
        let (status, _, body) =
            http_get_bearer(port, "/api/audit?event=login_failed", Some(&risk)).await;
        assert_eq!(status, 200, "{body}");
        let value: Value = serde_json::from_str(&body).expect("json");
        assert_eq!(value["count"], "1");
        assert_eq!(value["rows"][0]["actor"], "bob");

        // 分页：呈现口径「最新在前」，limit=2&offset=1 → 跳过最新一条后为
        // 首次查询的 audit_read（ops-admin）与 carol。
        let (status, _, body) =
            http_get_bearer(port, "/api/audit?limit=2&offset=1", Some(&risk)).await;
        assert_eq!(status, 200, "{body}");
        let value: Value = serde_json::from_str(&body).expect("json");
        assert_eq!(value["count"], "2");
        assert_eq!(value["rows"][0]["actor"], "ops-admin");
        assert_eq!(value["rows"][1]["actor"], "carol");
    }

    /// QA Error: 非法参数（limit=abc / limit=0 / event 未知 / until<since）→ 400。
    #[tokio::test]
    async fn list_bad_params_are_400() {
        let dir = tempfile::tempdir().expect("tempdir");
        let state = make_state(dir.path());
        let logger = state.daemon().audit_logger();
        seed_events(logger.as_ref().expect("logger"));
        let risk = token_for(&state, Role::Risk);
        let port = spawn_server(state).await;

        for (path, why) in [
            ("/api/audit?limit=abc", "limit not a number"),
            ("/api/audit?limit=0", "limit zero"),
            ("/api/audit?offset=-1", "negative offset"),
            ("/api/audit?since=x1", "since not a number"),
            ("/api/audit?event=nope", "unknown event"),
            ("/api/audit?since=200&until=100", "until < since"),
        ] {
            let (status, _, body) = http_get_bearer(port, path, Some(&risk)).await;
            assert_eq!(status, 400, "{why}: {path} → {body}");
        }
    }

    /// QA 安全: 审计库未装配 → 503（fail-closed，不冒充空集）。
    #[tokio::test]
    async fn list_without_mounted_logger_is_503() {
        let config = Arc::new(GatewayConfig::parse(TEST_TOML).expect("parse"));
        let daemon = DaemonShared::new();
        daemon.set_config(Arc::new(ConfigShared::new((*config).clone())));
        // 不挂载 audit logger。
        let state = MgmtState::new(daemon, config);
        let risk = token_for(&state, Role::Risk);
        let port = spawn_server(state).await;

        let (status, _, body) = http_get_bearer(port, "/api/audit", Some(&risk)).await;
        assert_eq!(status, 503, "fail-closed expected, got: {body}");
    }

    /// QA: 查询行为自身入链——查询后再查能看到 audit_read 记录（actor = JWT sub）。
    #[tokio::test]
    async fn list_self_audits_the_query() {
        let dir = tempfile::tempdir().expect("tempdir");
        let state = make_state(dir.path());
        let logger = state.daemon().audit_logger();
        seed_events(logger.as_ref().expect("logger"));
        let risk = token_for(&state, Role::Risk);
        let port = spawn_server(state).await;

        let _ = http_get_bearer(port, "/api/audit", Some(&risk)).await;

        let rows = logger
            .as_ref()
            .expect("logger")
            .query(&AuditQuery::new().with_event("audit_read"))
            .expect("query");
        assert_eq!(rows.len(), 1, "the query itself must be audited");
        assert_eq!(rows[0].actor, "ops-admin");
        assert_eq!(rows[0].outcome, OUTCOME_ACCEPTED);
        // 整链依旧完好（自审计记录已正确入链）。
        assert!(
            logger
                .as_ref()
                .expect("logger")
                .verify_chain()
                .expect("verify")
                .ok
        );
    }

    /// QA: 在线查询默认「最新在前」——本进程新产生的事件必须出现在首屏。
    ///
    /// 回归背景：排序缺省为 `seq ASC` 时，`?limit=2` 永远命中**最旧**的两条，
    /// 本次进程产生的事件被截断在末尾，管理面看起来像「读取侧拿到 stale 数据」。
    #[tokio::test]
    async fn list_returns_newest_first() {
        let dir = tempfile::tempdir().expect("tempdir");
        let state = make_state(dir.path());
        let logger = state.daemon().audit_logger();
        let seed = logger.as_ref().expect("logger");
        seed_events(seed);
        let risk = token_for(&state, Role::Risk);
        let port = spawn_server(state).await;

        let (status, _, body) = http_get_bearer(port, "/api/audit?limit=2", Some(&risk)).await;
        assert_eq!(status, 200, "body: {body}");

        let value = serde_json::from_str::<Value>(&body).expect("json body");
        let rows = value["rows"].as_array().expect("rows array");
        assert_eq!(rows.len(), 2, "body: {body}");
        let seqs: Vec<String> = rows
            .iter()
            .map(|r| r["seq"].as_str().expect("seq 大数红线：字符串").to_string())
            .collect();
        assert_eq!(seqs, vec!["3".to_string(), "2".to_string()], "默认须最新在前: {seqs:?}");
        assert_eq!(rows[0]["actor"], "carol");
        assert_eq!(rows[0]["detail"], "device_create");
        assert_eq!(rows[1]["actor"], "bob");

        // 再查一次：本次「查询行为自身」入链的 audit_read 应排在最前。
        let (status, _, body) = http_get_bearer(port, "/api/audit?limit=1", Some(&risk)).await;
        assert_eq!(status, 200, "body: {body}");
        let value = serde_json::from_str::<Value>(&body).expect("json body");
        let rows = value["rows"].as_array().expect("rows array");
        assert_eq!(rows.len(), 1);
        assert_eq!(
            rows[0]["event"], "audit_read",
            "本次进程的查询记录应在首行: {body}"
        );
        assert_eq!(rows[0]["actor"], "ops-admin");
    }

    /// QA 安全: export 门控——risk（持 audit.view 但不持 audit.export）→ 403；
    /// system → 200 且携带链完整性报告（ok = true）。
    #[tokio::test]
    async fn export_requires_audit_export_and_reports_chain() {
        let dir = tempfile::tempdir().expect("tempdir");
        let state = make_state(dir.path());
        let logger = state.daemon().audit_logger();
        seed_events(logger.as_ref().expect("logger"));
        let risk = token_for(&state, Role::Risk);
        let system = token_for(&state, Role::System);
        let port = spawn_server(state).await;

        // risk → 403（数据出境动作收敛 system）。
        let (status, _, body) = http_get_bearer(port, "/api/audit/export", Some(&risk)).await;
        assert_eq!(status, 403, "risk must not export audit: {body}");

        // system → 200 + chain.ok。
        let (status, _, body) = http_get_bearer(port, "/api/audit/export", Some(&system)).await;
        assert_eq!(status, 200, "{body}");
        let value: Value = serde_json::from_str(&body).expect("json");
        assert_eq!(value["chain"]["ok"], true);
        assert!(value["chain"]["total"].is_string(), "大数红线");
        assert_eq!(value["chain"]["total"], "3");

        // 导出行为自身入链（audit_export）。
        let rows = logger
            .as_ref()
            .expect("logger")
            .query(&AuditQuery::new().with_event("audit_export"))
            .expect("query");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].actor, "ops-admin");
    }

    /// QA（防篡改联动）: 链被篡改后 export 的链报告必须 ok=false 并定位首断点。
    #[tokio::test]
    async fn export_reports_broken_chain() {
        let dir = tempfile::tempdir().expect("tempdir");
        let state = make_state(dir.path());
        let logger = state.daemon().audit_logger();
        seed_events(logger.as_ref().expect("logger"));
        // 模拟攻击者：摘触发器 + 改第 2 行。
        let tamper = rusqlite::Connection::open(dir.path().join("audit.db")).expect("tamper conn");
        tamper
            .execute("DROP TRIGGER audit_log_no_update", [])
            .expect("drop trigger");
        tamper
            .execute("UPDATE audit_log SET detail = 'forged' WHERE seq = 2", [])
            .expect("tamper");

        let system = token_for(&state, Role::System);
        let port = spawn_server(state).await;

        let (status, _, body) = http_get_bearer(port, "/api/audit/export", Some(&system)).await;
        assert_eq!(status, 200, "{body}");
        let value: Value = serde_json::from_str(&body).expect("json");
        assert_eq!(
            value["chain"]["ok"], false,
            "forged chain must be reported: {body}"
        );
        assert_eq!(value["chain"]["first_broken_seq"], "2");
        assert!(value["chain"]["first_broken_seq"].is_string(), "大数红线");
    }
}

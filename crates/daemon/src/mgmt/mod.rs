//! task 52 — 后端管理 API 层 + 实时数据通道（axum REST + SSE + 状态聚合）。
//!
//! ## 职责边界
//! - **做**：只读聚合 REST（health / status / devices / points / outlets）+
//!   实时事件通道（`/api/events` SSE，MgmtEvent 由 broadcast + 历史环驱动）+
//!   手写静态文件服务（web-console 前端资源）。
//! - **不做**：**授权判定不在此层**（由网关前置层 / task 25/31 执行）；写操作
//!   （配置修改 / 下发控制）不在此层；北向连接运行态细粒度查询（V1 只聚合配置）。
//!
//! ## 实现红线
//! - axum 0.7.9（已在 Cargo.lock，禁止升级/换框架）；SSE 依赖 `futures-core = "0.3"`、
//!   `tokio-stream = "0.1"`（均已在 Cargo.lock，主理人登记为 daemon 直接依赖，零新增下载）。
//! - **JSON 大数红线**：所有 u64/i64（纳秒时间戳 / uptime / 计数 / interval / qos /
//!   事件 seq）一律**字符串编码**（手工构造 `serde_json::json!`，不走 derive 序列化）。
//! - 静态文件：Cargo.lock 无 tower-http，按决议**手写**最小 static handler；
//!   路径穿越双重防护（分量白名单 + canonicalize 前缀校验）。
//! - web-console 使用 **hash 路由**（`#/devices`），不存在 history 路由回退需求：
//!   404 一律直接 404，不回退 index.html（见 `serve_file` 注释）。
//!
//! ## /api/events SSE 语义
//! - `GET /api/events?seq=<last_seq>`：`seq = 0` 首连（历史环回放，空则发状态快照帧，
//!   保证首事件必达）；`seq > 0` 回放该序号之后的增量，然后进入实时推送；
//! - 实时源三路合流：管理事件 broadcast（Lagged 时从历史环补拉）+ 生命周期 watch +
//!   配置版本 watch；心跳注释行由 `KeepAlive`（15s）承担；
//! - 断线重连：客户端带回上次最大 `seq` 重新 GET 即可，历史环（容量 256）保证不丢。

// task 57 切片 2：管理面 JWT 鉴权 + RBAC 原语（服务端判定，红线：授权判定只在
// Rust 侧，WebView/JS 只做展示）。
//
// task 57 全量接线（本轮）：JWT/RBAC 原语接入 daemon 管理面，本地端到端可测。
// - **登录**：`POST /api/auth/login`（`auth_login` 模块；凭证生产路 = config
//   `[mgmt_auth]`，开发路 = `IOT_DAQ_DEV_ADMIN_PASS` dev 管理员，两路皆空 fail-closed）；
// - **密钥**：`IssuerKey` 来自 `IOT_DAQ_JWT_SECRET`（64 hex），未提供回退 dev
//   常量并 warn（仅本地），装配见 `auth_login::build`；
// - **守卫**：`RbacAuth` 挂入 [`MgmtState`]（`FromRef` 分发给
//   `rbac::AuthedRole` extractor），`/api/ops/*` 由 `from_fn` 中间件
//   [`ops_guard`] 包裹——AuthedRole（401）+ `permission_for_ops_action`（403），
//   被拒动作入 ops 审计环；**读接口保持开放**（web-console 16 页契约不变）；
// - **whoami**：`GET /api/auth/whoami` 返回 sub/role/exp（exp 字符串编码）。
pub mod audit_api;
pub mod auth_jwt;
pub mod auth_login;
pub mod rbac;

pub mod remote_ops;
pub mod writeapi;

use std::collections::{HashMap, HashSet, VecDeque};
use std::convert::Infallible;
use std::path::{Component, Path, PathBuf};
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::Duration;

use axum::extract::{FromRef, FromRequestParts, Path as AxumPath, Query, Request, State};
use axum::http::{header, StatusCode};
use axum::middleware::{from_fn_with_state, Next};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Json, Response};
use axum::routing::get;
use axum::Router;
use futures_core::Stream;
use serde_json::{json, Value};
use tokio::sync::{broadcast, mpsc};

use crate::bootstrap::{DaemonShared, LifecycleState};
use crate::config::GatewayConfig;

/// 事件历史环容量（断线重连回放窗口；超出按环形淘汰）。
pub const EVENT_HISTORY_CAPACITY: usize = 256;
/// 事件广播通道容量（并发订阅者各自的积压上限；超限按 Lagged 丢弃并回退到历史环）。
const EVENT_CHANNEL_CAPACITY: usize = 64;
/// web 静态资源根目录环境变量。
pub const WEB_DIST_ENV: &str = "IOT_DAQ_WEB_DIST";
/// web 静态资源默认目录。
pub const WEB_DIST_DEFAULT: &str = "./web-dist";

// ---- 事件 ----

/// 管理事件（实时通道推送载荷；broadcast + 历史环双驱动）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MgmtEvent {
    /// 配置热重载成功（携带新版本号）。
    ConfigReloaded {
        /// 重载后的配置版本号（JSON 编码为字符串，大数红线）。
        version: u64,
    },
    /// 生命周期状态变化。
    LifecycleChanged {
        /// 新状态。
        state: LifecycleState,
    },
    /// 设备配置变化（增删改点位行）。
    DeviceChanged {
        /// 受影响设备标识。
        device_id: String,
    },
}

impl MgmtEvent {
    /// 事件类型名（`type` 字段 / 前端事件分发约定，小写下划线）。
    pub fn type_name(&self) -> &'static str {
        match self {
            MgmtEvent::ConfigReloaded { .. } => "config_reloaded",
            MgmtEvent::LifecycleChanged { .. } => "lifecycle_changed",
            MgmtEvent::DeviceChanged { .. } => "device_changed",
        }
    }

    /// 序列化为单行 JSON（**手工构造**：u64 → 字符串，落实大数红线）。
    pub fn to_json(&self) -> String {
        match self {
            MgmtEvent::ConfigReloaded { version } => json!({
                "type": "config_reloaded",
                "version": version.to_string(),
            }),
            MgmtEvent::LifecycleChanged { state } => json!({
                "type": "lifecycle_changed",
                "state": state.as_str(),
            }),
            MgmtEvent::DeviceChanged { device_id } => json!({
                "type": "device_changed",
                "device_id": device_id,
            }),
        }
        .to_string()
    }
}

/// 带全局递增序号的事件信封（长轮询增量拉取 / 回放的基本单位）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EventEnvelope {
    /// 全局单调递增序号（JSON 编码为字符串，大数红线）。
    pub seq: u64,
    /// 事件载荷。
    pub event: MgmtEvent,
}

impl EventEnvelope {
    /// 序列化为 JSON（`seq` 为字符串 + 事件载荷字段平铺）。
    pub fn to_json(&self) -> Value {
        // 事件 JSON 由本模块自身生成，解析失败理论不可达；兜底为 unknown 事件。
        let mut value: Value = serde_json::from_str(&self.event.to_json())
            .unwrap_or_else(|_| json!({ "type": "unknown" }));
        value["seq"] = Value::String(self.seq.to_string());
        value
    }
}

// ---- 共享状态 ----

/// [`MgmtState`] 内部态。
struct MgmtStateInner {
    /// daemon 共享态（生命周期 / 心跳 / 配置 watch 源；配置读侧经
    /// `daemon.config_snapshot()` 取**热重载感知**的最新快照）。
    daemon: DaemonShared,
    /// 管理事件广播（并发订阅者即时扇出）。
    events: broadcast::Sender<EventEnvelope>,
    /// 事件历史环（断线重连 / 轮询间隙的事件回放）。
    history: Mutex<VecDeque<EventEnvelope>>,
    /// 下一个事件序号（全局单调递增）。
    next_seq: AtomicU64,
    /// 测试 / 嵌入式场景的静态根目录覆盖（`None` = 读环境变量 / 默认值）。
    web_dist: Option<PathBuf>,
    /// task 57：JWT/RBAC 鉴权上下文（`FromRef<MgmtState>` 分发给 extractor；
    /// 密钥来自 `IOT_DAQ_JWT_SECRET` / dev 兜底，见 `auth_login::build`）。
    auth: rbac::RbacAuth,
    /// task 57：登录凭证 + token 签发器（生产路 / 开发路两路 fail-closed）。
    login: Arc<auth_login::MgmtAuth>,
    /// 配置写路径（`writeapi` 落盘目标；与热重载监听同一文件。`None` = 未
    /// 装配，写接口 fail-closed 拒绝）。经 `with_config_path` 装配。
    config_path: Option<PathBuf>,
}

/// 管理 API 共享状态（axum `State`；`Clone` 廉价，内部 `Arc`）。
#[derive(Clone)]
pub struct MgmtState {
    inner: Arc<MgmtStateInner>,
}

impl MgmtState {
    /// 创建管理状态：绑定 daemon 共享态与启动配置快照。
    ///
    /// task 57：鉴权上下文在此一并装配（`auth_login::build` 读环境变量与
    /// 配置内的 `[mgmt_auth]` 可选段；测试可用 [`Self::with_auth`] 覆盖，
    /// 避免测试间环境变量竞争）。
    /// `startup_config` 仅用于装配期鉴权凭证解析；REST 读侧统一走
    /// `daemon.config_snapshot()`（热重载感知，见 [`Self::config`]）。
    pub fn new(daemon: DaemonShared, startup_config: Arc<GatewayConfig>) -> Self {
        let (auth, login) = auth_login::build(&startup_config, &|key| std::env::var(key).ok());
        let (events, _) = broadcast::channel(EVENT_CHANNEL_CAPACITY);
        Self {
            inner: Arc::new(MgmtStateInner {
                daemon,
                events,
                history: Mutex::new(VecDeque::with_capacity(EVENT_HISTORY_CAPACITY)),
                next_seq: AtomicU64::new(0),
                web_dist: None,
                auth,
                login: Arc::new(login),
                config_path: None,
            }),
        }
    }

    /// 覆盖静态资源根目录（测试注入临时目录用；生产走 `IOT_DAQ_WEB_DIST`）。
    pub fn with_web_dist(mut self, dir: impl Into<PathBuf>) -> Self {
        let inner = Arc::get_mut(&mut self.inner)
            .expect("with_web_dist must be called before cloning/sharing");
        inner.web_dist = Some(dir.into());
        self
    }

    /// 覆盖鉴权装配（测试注入受控密钥 / 凭证用；生产走 `new` 的
    /// `auth_login::build` 环境变量装配）。与 [`Self::with_web_dist`] 同约束：
    /// 必须在 clone / 共享之前调用。
    pub fn with_auth(mut self, auth: rbac::RbacAuth, login: auth_login::MgmtAuth) -> Self {
        let inner =
            Arc::get_mut(&mut self.inner).expect("with_auth must be called before cloning/sharing");
        inner.auth = auth;
        inner.login = Arc::new(login);
        self
    }

    /// 鉴权上下文（JWT 签名密钥 / leeway）。
    pub fn auth(&self) -> &rbac::RbacAuth {
        &self.inner.auth
    }

    /// 登录凭证与 token 签发器。
    pub fn login_auth(&self) -> &auth_login::MgmtAuth {
        &self.inner.login
    }

    /// daemon 共享态句柄。
    pub fn daemon(&self) -> &DaemonShared {
        &self.inner.daemon
    }

    /// 当前配置快照（**热重载感知**：读 daemon 共享句柄的最新快照——写接口
    /// 落盘后经 `ConfigShared::replace` 即时可见；HTTP 响应形状不变）。
    pub fn config(&self) -> Arc<GatewayConfig> {
        self.inner.daemon.config_snapshot()
    }

    /// 配置文件路径（写接口落盘目标；`None` = 未装配，写接口 fail-closed）。
    pub fn config_path(&self) -> Option<PathBuf> {
        self.inner.config_path.clone()
    }

    /// 绑定配置文件路径（写接口落盘目标；须与热重载监听同一文件）。
    ///
    /// 必须在 clone / 共享之前调用（与 [`Self::with_web_dist`] 同约束）；
    /// 实例已被共享时无法写入，记 warn 后忽略——写接口将 fail-closed 拒绝，
    /// 绝不 panic。
    pub fn with_config_path(mut self, path: impl Into<PathBuf>) -> Self {
        match Arc::get_mut(&mut self.inner) {
            Some(inner) => inner.config_path = Some(path.into()),
            None => tracing::warn!(
                "with_config_path must be called before cloning/sharing; write path not bound"
            ),
        }
        self
    }

    /// 发布管理事件：分配序号 → 写历史环 → broadcast 扇出，返回事件序号。
    pub fn publish(&self, event: MgmtEvent) -> u64 {
        self.publish_envelope(EventEnvelope {
            seq: self.allocate_seq(),
            event,
        })
    }

    /// 发布一批当前状态快照事件（首次连接保证首事件必达），返回信封列表。
    pub fn publish_snapshot(&self) -> Vec<EventEnvelope> {
        let envelopes = vec![
            EventEnvelope {
                seq: self.allocate_seq(),
                event: MgmtEvent::LifecycleChanged {
                    state: self.inner.daemon.state(),
                },
            },
            EventEnvelope {
                seq: self.allocate_seq(),
                event: MgmtEvent::ConfigReloaded {
                    version: self.inner.daemon.config_version(),
                },
            },
        ];
        for envelope in &envelopes {
            self.publish_envelope(envelope.clone());
        }
        envelopes
    }

    /// 当前已分配的最大事件序号（客户端增量拉取的起点语义）。
    pub fn current_seq(&self) -> u64 {
        self.inner.next_seq.load(Ordering::Relaxed)
    }

    /// 回放历史环中 `seq > last_seq` 的事件（保序）。
    pub fn history_since(&self, last_seq: u64) -> Vec<EventEnvelope> {
        self.inner
            .history
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .iter()
            .filter(|envelope| envelope.seq > last_seq)
            .cloned()
            .collect()
    }

    /// 订阅事件广播（即时扇出通道；历史回放走 [`Self::history_since`]）。
    pub fn subscribe(&self) -> broadcast::Receiver<EventEnvelope> {
        self.inner.events.subscribe()
    }

    /// 分配下一个事件序号。
    fn allocate_seq(&self) -> u64 {
        self.inner.next_seq.fetch_add(1, Ordering::Relaxed) + 1
    }

    /// 信封入历史环（环形淘汰最旧）并 broadcast 扇出，返回事件序号。
    fn publish_envelope(&self, envelope: EventEnvelope) -> u64 {
        {
            let mut history = self
                .inner
                .history
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if history.len() >= EVENT_HISTORY_CAPACITY {
                history.pop_front();
            }
            history.push_back(envelope.clone());
        }
        // 无订阅者时 send 失败属正常（长轮询方走历史环）。
        let _ = self.inner.events.send(envelope.clone());
        envelope.seq
    }
}

// ---- 局部错误类型 ----

/// 本模块局部 API 错误 → `IntoResponse`（状态码 + 简洁 JSON body）。
///
/// 授权类错误不在本层产生（红线：授权判定不在此层）。
/// `BadRequest` / `Internal` 为后续处理器扩展预留（当前路由无对应错误路径）。
#[derive(Debug)]
#[allow(dead_code)]
enum ApiError {
    /// 请求参数非法。
    BadRequest(String),
    /// 资源不存在。
    NotFound(String),
    /// 路径穿越等被拒绝的访问（不泄露细节，仅复述被拒路径）。
    Forbidden(String),
    /// 服务端内部错误。
    Internal(String),
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let (status, message) = match self {
            ApiError::BadRequest(message) => (StatusCode::BAD_REQUEST, message),
            ApiError::NotFound(message) => (StatusCode::NOT_FOUND, message),
            ApiError::Forbidden(message) => (StatusCode::FORBIDDEN, message),
            ApiError::Internal(message) => (StatusCode::INTERNAL_SERVER_ERROR, message),
        };
        (status, Json(json!({ "error": message }))).into_response()
    }
}

// ---- 路由 ----

/// task 57：`RbacAuth` 从 [`MgmtState`] 分发（`rbac::AuthedRole` extractor 的
/// 约束 `RbacAuth: FromRef<S>` 由此满足）。
impl FromRef<MgmtState> for rbac::RbacAuth {
    fn from_ref(state: &MgmtState) -> rbac::RbacAuth {
        state.inner.auth.clone()
    }
}

/// 构建管理 API 路由。
///
/// task 57 守卫布局：
/// - **开放（不变）**：读聚合 `/api/health|status|devices|points|outlets|events`
///   + 静态资源 + `/api/auth/login`（凭密码建立身份）；
/// - **守卫**：`/api/ops/*` 三路由挂在独立子路由并由 [`ops_guard`] 中间件
///   包裹（AuthedRole 401 + `permission_for_ops_action` 403）；
///   `/api/auth/whoami` 靠 `rbac::AuthedRole` extractor 自行 401。
pub fn router(state: MgmtState) -> Router {
    // /api/ops/* 子路由：仅这三条路由被 ops_guard 包裹（route_layer 只作用于
    // 本子路由已注册路由）。
    let ops = Router::new()
        .route("/api/ops/restart", axum::routing::post(remote_ops::restart))
        .route(
            "/api/ops/collectors",
            axum::routing::post(remote_ops::collectors),
        )
        .route("/api/ops/logs", get(remote_ops::logs))
        .route_layer(from_fn_with_state(state.clone(), ops_guard));

    Router::new()
        .route("/api/health", get(health))
        // task-61 D-01：`/healthz` 别名（与 `/api/health` 完全等价）——部署文档
        // 与 install.sh 的健康判据统一走 `/healthz`，两个路径共享同一 handler。
        .route("/healthz", get(health))
        .route("/api/status", get(status))
        // 读接口形状不变；写接口（writeapi）自带 AuthedRole extractor（401）
        // + ensure()（403）+ 审计，不经过 ops_guard（动作映射仅覆盖 /api/ops/*）。
        .route("/api/devices", get(devices).post(writeapi::device_create))
        .route(
            "/api/devices/:id",
            axum::routing::put(writeapi::device_update).delete(writeapi::device_delete),
        )
        .route("/api/points", get(points).post(writeapi::point_create))
        .route(
            "/api/points/:device_id/:point_id",
            axum::routing::put(writeapi::point_update).delete(writeapi::point_delete),
        )
        .route("/api/outlets", get(outlets))
        .route("/api/events", get(events))
        .route("/api/auth/login", axum::routing::post(auth_login::login))
        .route("/api/auth/whoami", get(auth_login::whoami))
        // task 26：安全审计远程拉取（只读；handler 自带 AuthedRole extractor +
        // ensure 门控——list=audit.view(risk/system)，export=audit.export(仅 system)）。
        .route("/api/audit", get(audit_api::list))
        .route("/api/audit/export", get(audit_api::export))
        .merge(ops)
        .route("/", get(serve_root))
        .route("/assets/*path", get(serve_asset))
        .with_state(state)
}

/// `/api/ops/*` 守卫中间件（task 57 全量接线）：
///
/// 1. **方法+路径 → 动作字面量**：`POST /api/ops/restart → "restart"`、
///    `POST /api/ops/collectors → "collectors_pause"`（pause/resume 共享同一
///    权限，且中间件层不可重放读取 body，按最严口径统一映射）、
///    `GET /api/ops/logs → "logs_read"`；
/// 2. **AuthedRole**：校验 `Authorization: Bearer` JWT（签名/时间窗/角色，
///    全 Rust 侧）→ 失败 401（未知角色 403，见 `rbac::AuthRejection`）；
/// 3. **permission_for_ops_action**：`authed.ensure(perm)` → 权限不足 403，
///    且被拒动作**入 ops 审计环**（actor 取 JWT sub；审计不因中间件层拒绝而缺痕）；
/// 4. 未注册的方法/路径组合（如 GET restart）不在动作映射内 → 透传给路由器
///    （404/405）。⚠️ **给 `/api/ops/*` 新增路由时必须同步扩充本映射**，
///    否则新路由将不经守卫（见 router() 注释）。
///
/// 注意：`remote_ops` 三个 handler 内还保留 `authed.ensure(perm)` 二次校验
///（防御纵深）；`DenyAllOpsAuthorizer` 保留为「无可信角色源」的 fail-closed
/// 兜底（未装配 runtime 的实例 / 非 extractor 路径），不再参与本守卫链判定。
async fn ops_guard(State(state): State<MgmtState>, req: Request, next: Next) -> Response {
    let (mut parts, body) = req.into_parts();

    // ① 动作映射（未知组合透传给路由器，由其产生 404/405）。
    let action: &str = match (parts.method.as_str(), parts.uri.path()) {
        ("POST", "/api/ops/restart") => "restart",
        ("POST", "/api/ops/collectors") => "collectors_pause",
        ("GET", "/api/ops/logs") => "logs_read",
        _ => "",
    };
    let Some(permission) = rbac::permission_for_ops_action(action) else {
        return next.run(Request::from_parts(parts, body)).await;
    };

    // ② JWT 鉴权（401；签名验真走恒时比较）。
    let authed = match rbac::AuthedRole::from_request_parts(&mut parts, &state).await {
        Ok(authed) => authed,
        Err(rejection) => return rejection.into_response(),
    };

    // ③ RBAC 判定（403 + 审计；fail-closed：映射外动作在上面已透传/拒绝）。
    if let Err(rejection) = authed.ensure(permission) {
        let action_enum = match action {
            "restart" => remote_ops::OpsAction::Restart,
            "collectors_pause" => remote_ops::OpsAction::CollectorsPause,
            _ => remote_ops::OpsAction::LogsRead,
        };
        remote_ops::runtime_for(&state).record_audit(
            &authed.claims.sub,
            action_enum,
            false,
            remote_ops::OUTCOME_DENIED,
            &rejection.to_string(),
        );
        return rejection.into_response();
    }

    next.run(Request::from_parts(parts, body)).await
}

// ---- REST 处理器（全部只读聚合） ----

/// GET /api/health → 存活探针（固定 200，供 liveness 探测，不做聚合计算）。
async fn health() -> Json<Value> {
    Json(json!({
        "status": "ok",
        "version": env!("CARGO_PKG_VERSION"),
    }))
}

/// GET /api/status → 生命周期 + 运行时聚合（**整数字段一律字符串**，大数红线）。
async fn status(State(state): State<MgmtState>) -> Response {
    let daemon = state.daemon();
    let config = state.config();
    let device_count = config
        .points
        .iter()
        .map(|p| p.device_id.as_str())
        .collect::<HashSet<_>>()
        .len();
    Json(json!({
        "state": daemon.state().as_str(),
        "uptime_secs": daemon.uptime_secs().to_string(),
        "last_heartbeat_ns": daemon.last_heartbeat_ns().to_string(),
        "device_count": device_count.to_string(),
        "outlet_count": config.outlets.len().to_string(),
        "config_version": daemon.config_version().to_string(),
        "version": env!("CARGO_PKG_VERSION"),
    }))
    .into_response()
}

/// GET /api/devices → 设备摘要数组（id/name/protocol/enabled/poll_interval_ms 字符串）。
///
/// 设备列表 = 点位平铺行去重推导（保首次出现顺序）∪ `[[devices]]` 登记段
/// （仅无点位设备追加，保持登记顺序）；`poll_interval_ms` 取该设备最小采集
/// 频率（与 bootstrap `build_groups` 口径一致，无点位设备为 `"0"`）；
/// `name` / `enabled` 优先取登记段覆盖，`protocol` 优先按点位行推导。
/// **响应形状不变**：旧配置（无登记段）输出与既有口径逐字节一致。
async fn devices(State(state): State<MgmtState>) -> Json<Value> {
    let config = state.config();
    let mut order: Vec<String> = Vec::new();
    let mut agg: HashMap<String, (String, u64)> = HashMap::new();
    for point in &config.points {
        match agg.get_mut(&point.device_id) {
            Some(entry) => entry.1 = entry.1.min(point.frequency_ms.max(1)),
            None => {
                order.push(point.device_id.clone());
                agg.insert(
                    point.device_id.clone(),
                    (point.protocol.clone(), point.frequency_ms.max(1)),
                );
            }
        }
    }
    // 登记段并入：仅追加点位聚合中不存在的设备（无点位设备的登记行）。
    for device in &config.devices {
        if !order.iter().any(|id| id == &device.device_id) {
            order.push(device.device_id.clone());
        }
    }
    let rows: Vec<Value> = order
        .iter()
        .map(|device_id| {
            let entry = config.devices.iter().find(|d| &d.device_id == device_id);
            // 协议：点位行推导优先，回退登记段默认协议，再回退空串。
            let protocol = agg
                .get(device_id)
                .map(|(p, _)| p.clone())
                .or_else(|| entry.and_then(|d| d.protocol.clone()));
            let name = entry
                .and_then(|d| d.name.clone())
                .unwrap_or_else(|| device_id.clone());
            let enabled = entry.map(|d| d.enabled).unwrap_or(true);
            let freq_ms = agg.get(device_id).map(|(_, f)| *f).unwrap_or(0);
            json!({
                "id": device_id,
                "name": name,
                "protocol": protocol,
                "enabled": enabled,
                "poll_interval_ms": freq_ms.to_string(),
            })
        })
        .collect();
    Json(Value::Array(rows))
}

/// GET /api/points?device_id=xxx → 该设备点位列表（没有则空数组）。
async fn points(
    State(state): State<MgmtState>,
    Query(params): Query<HashMap<String, String>>,
) -> Json<Value> {
    let device_id = params.get("device_id").map(String::as_str).unwrap_or("");
    let rows: Vec<Value> = state
        .config()
        .points
        .iter()
        .filter(|point| point.device_id == device_id)
        .map(|point| {
            json!({
                "device_id": point.device_id,
                "point_id": point.point_id,
                "protocol": point.protocol,
                "address": point.address,
                "frequency_ms": point.frequency_ms.to_string(),
            })
        })
        .collect();
    Json(Value::Array(rows))
}

/// GET /api/outlets → 北向出口列表（`target` 用对外标识 = broker 地址）。
async fn outlets(State(state): State<MgmtState>) -> Json<Value> {
    let rows: Vec<Value> = state
        .config()
        .outlets
        .iter()
        .map(|outlet| {
            // 配置层 OutletEncoding 无 as_str，这里单一映射点（与 north::Encoding 对齐）。
            let encoding = match outlet.encoding {
                crate::config::OutletEncoding::Protobuf => "protobuf",
                crate::config::OutletEncoding::Json => "json",
            };
            json!({
                "name": outlet.name,
                "target": outlet.broker,
                "topic_prefix": outlet.topic_prefix,
                "qos": outlet.qos.to_string(),
                "tls": outlet.tls,
                "encoding": encoding,
            })
        })
        .collect();
    Json(Value::Array(rows))
}

// ---- 实时事件通道（SSE：历史回放 + broadcast/watch 三源合流） ----

/// GET /api/events?seq=&lt;last_seq&gt; → SSE 实时事件流（`text/event-stream`）。
///
/// - `seq = 0`（首次连接）：回放历史环；为空则先发一帧状态快照（首事件必达）；
/// - `seq > 0`：回放历史环中 `seq` 之后的事件，然后进入实时推送；
/// - 心跳注释行由 `KeepAlive`（15s）承担；客户端断线重连带回上次最大 `seq`。
async fn events(
    State(state): State<MgmtState>,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    let last_seq: u64 = params
        .get("seq")
        .and_then(|raw| raw.parse().ok())
        .unwrap_or(0);

    // 合并任务在后台把「历史回放 + 三源实时事件」推入通道；本 handler 立即返回 SSE 流。
    let (tx, rx) = mpsc::channel(EVENT_CHANNEL_CAPACITY);
    tokio::spawn(merge_events(state.clone(), last_seq, tx));

    // [对照实验] 临时去掉 keep_alive。
    Sse::new(MgmtEventStream { rx })
        .keep_alive(KeepAlive::new().interval(Duration::from_secs(15)))
        .into_response()
}

/// 信封 → SSE 帧（`event:` 为类型名，`data:` 为完整 JSON，seq 为字符串——大数红线）。
fn envelope_to_event(envelope: &EventEnvelope) -> Event {
    Event::default()
        .event(envelope.event.type_name())
        .data(envelope.to_json().to_string())
}

/// 事件合并任务：先回放前奏（历史环 / 首连快照），再 select! 合流三路实时源。
///
/// - 订阅先于前奏收集（中间窗口发布的事件由 cursor 去重，不丢不重）；
/// - broadcast `Lagged`（积压超限）时从历史环补拉——历史环是唯一完整副本；
/// - 通道关闭（SSE 客户端断开）即退出；发送端全部销毁（daemon 关停）亦退出。
async fn merge_events(state: MgmtState, last_seq: u64, tx: mpsc::Sender<EventEnvelope>) {
    // 订阅先行，避免「收集前奏 ↔ 订阅」窗口丢事件；重复由 cursor 去重。
    let mut mgmt_rx = state.subscribe();
    let mut state_rx = state.daemon().subscribe_state();
    let mut reload_rx = state.daemon().subscribe_config_reload();

    let mut cursor = last_seq;

    // ① 前奏：历史环回放；首连（seq=0）且无历史 → 发布并回放状态快照（首事件必达）。
    let mut prelude: VecDeque<EventEnvelope> = state.history_since(cursor).into();
    if cursor == 0 && prelude.is_empty() {
        prelude.extend(state.publish_snapshot());
    }
    for envelope in prelude {
        cursor = cursor.max(envelope.seq);
        if tx.send(envelope).await.is_err() {
            return;
        }
    }

    // ② 实时三源合流。
    loop {
        tokio::select! {
            res = mgmt_rx.recv() => match res {
                Ok(envelope) => {
                    if envelope.seq <= cursor {
                        continue; // 前奏已回放过，去重。
                    }
                    cursor = envelope.seq;
                    if tx.send(envelope).await.is_err() {
                        return;
                    }
                }
                // 积压超限被广播丢弃 → 从历史环补拉（环也淘汰则放弃缺失段）。
                Err(broadcast::error::RecvError::Lagged(_)) => {
                    for envelope in state.history_since(cursor) {
                        cursor = cursor.max(envelope.seq);
                        if tx.send(envelope).await.is_err() {
                            return;
                        }
                    }
                }
                // 发送端全部销毁（daemon 关停）→ 结束。
                Err(broadcast::error::RecvError::Closed) => return,
            },
            res = state_rx.changed() => match res {
                Ok(()) => {
                    // watch 源无序号语义（seq=0）；前端以 type 分发。
                    let envelope = EventEnvelope {
                        seq: 0,
                        event: MgmtEvent::LifecycleChanged { state: *state_rx.borrow() },
                    };
                    if tx.send(envelope).await.is_err() {
                        return;
                    }
                }
                Err(_) => return,
            },
            res = reload_rx.changed() => match res {
                Ok(()) => {
                    let envelope = EventEnvelope {
                        seq: 0,
                        event: MgmtEvent::ConfigReloaded { version: *reload_rx.borrow() },
                    };
                    if tx.send(envelope).await.is_err() {
                        return;
                    }
                }
                Err(_) => return,
            },
        }
    }
}

/// `/api/events` 的 SSE 事件流：包装合并任务的通道接收端。
///
/// `mpsc::Receiver::poll_recv` 是 tokio 公开的同步轮询 API，与
/// `futures_core::Stream` 单向对接（Item 恒为 `Ok`，错误经由流结束表达）。
struct MgmtEventStream {
    rx: mpsc::Receiver<EventEnvelope>,
}

impl Stream for MgmtEventStream {
    type Item = Result<Event, Infallible>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        match self.rx.poll_recv(cx) {
            Poll::Ready(Some(envelope)) => Poll::Ready(Some(Ok(envelope_to_event(&envelope)))),
            Poll::Ready(None) => Poll::Ready(None),
            Poll::Pending => Poll::Pending,
        }
    }
}

// ---- 静态文件服务（手写，无 tower-http） ----

/// GET / → index.html。
async fn serve_root(State(state): State<MgmtState>) -> Response {
    serve_file(&state, "index.html").await
}

/// GET /assets/*path → 静态资源。
///
/// 通配符只捕获 `/assets/` 之后的部分（如 `app.js`），统一映射到
/// `{web_dist}/assets/{path}`（与 web-console 的资源布局一致）。
async fn serve_asset(State(state): State<MgmtState>, AxumPath(rel): AxumPath<String>) -> Response {
    serve_file(&state, &format!("assets/{rel}")).await
}

/// 解析静态根目录：注入覆盖 → `IOT_DAQ_WEB_DIST` 环境变量 → 默认 `./web-dist`。
fn resolve_web_root(state: &MgmtState) -> PathBuf {
    if let Some(dir) = &state.inner.web_dist {
        return dir.clone();
    }
    std::env::var(WEB_DIST_ENV)
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from(WEB_DIST_DEFAULT))
}

/// 相对路径安全校验（穿越防护第一层）：仅接受普通分量与 `.` 分量。
///
/// 拒绝：绝对路径、`..`（ParentDir）、Windows 盘符 / 根前缀（Prefix / RootDir）。
/// 返回 `None` 表示判定为路径穿越攻击。
fn safe_rel_path(raw: &str) -> Option<PathBuf> {
    if raw.is_empty() {
        return None;
    }
    let path = Path::new(raw);
    if path.is_absolute() {
        return None;
    }
    for component in path.components() {
        match component {
            Component::Normal(_) | Component::CurDir => {}
            _ => return None,
        }
    }
    Some(path.to_path_buf())
}

/// 读取静态文件并按扩展名回 Content-Type。
///
/// 穿越防护第二层：canonicalize 后必须仍位于根目录内（防符号链接 / 大小写绕过）。
/// 404 处理：web-console 是 **hash 路由**（`#/devices`），资源缺失无需回退
/// index.html（history 路由才需要 fallback），故直接 404。
///
/// 注：tokio 的 `fs` feature 未在 daemon 依赖声明中启用（红线不许改 Cargo.toml），
/// 故用 `spawn_blocking` + `std::fs`（管理面低频小文件，阻塞线程池开销可忽略）。
async fn serve_file(state: &MgmtState, rel: &str) -> Response {
    let root = resolve_web_root(state);
    let Some(rel_path) = safe_rel_path(rel) else {
        return ApiError::Forbidden(format!("rejected path {rel:?}")).into_response();
    };
    let full = root.join(rel_path);
    let outcome =
        tokio::task::spawn_blocking(move || -> Result<(PathBuf, PathBuf, Vec<u8>), bool> {
            // Ok = (规范化文件路径, 规范化根路径, 字节)；Err(true) = 穿越被拒，Err(false) = 不存在。
            let (canonical, root_canonical) = match (full.canonicalize(), root.canonicalize()) {
                (Ok(file), Ok(root_dir)) => (file, root_dir),
                // 根目录不存在 / 文件不存在均按 404 处理。
                _ => return Err(false),
            };
            if !canonical.starts_with(&root_canonical) {
                return Err(true);
            }
            match std::fs::read(&canonical) {
                Ok(bytes) => Ok((canonical, root_canonical, bytes)),
                Err(_) => Err(false),
            }
        })
        .await;

    match outcome {
        Ok(Ok((canonical, _, bytes))) => (
            [(header::CONTENT_TYPE, content_type_for(&canonical))],
            bytes,
        )
            .into_response(),
        Ok(Err(true)) => {
            ApiError::Forbidden(format!("path escapes web root: {rel}")).into_response()
        }
        // spawn_blocking join 失败（运行时关停）与文件不存在统一按 404 处理。
        _ => ApiError::NotFound(format!("no such file: {rel}")).into_response(),
    }
}

/// 按扩展名给 Content-Type（规格集合：html/js/css/svg/png/ico/json/map/woff2）。
fn content_type_for(path: &Path) -> &'static str {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    match ext.as_str() {
        "html" | "htm" => "text/html; charset=utf-8",
        "js" => "application/javascript; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "ico" => "image/x-icon",
        "json" | "map" => "application/json",
        "woff2" => "font/woff2",
        _ => "application/octet-stream",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::{TcpListener, TcpStream};

    /// 测试配置：2 个出口 + 单设备 2 点位（频率 100/500 → 摘要取 100）。
    const TEST_TOML: &str = r#"
[gateway]
gateway_id = "gw-test"

[[outlets]]
name = "north-1"
broker = "mqtts://broker.local:8883"
topic_prefix = "telemetry"
qos = 1
tls = true
encoding = "protobuf"

[[outlets]]
name = "north-2"
broker = "mqtt://backup.local:1883"
encoding = "json"

[[points]]
device_id = "dev-01"
point_id = "p_temp"
protocol = "modbus-tcp"
address = "192.168.1.10:502"
frequency_ms = 100

[[points]]
device_id = "dev-01"
point_id = "p_press"
protocol = "modbus-tcp"
address = "192.168.1.10:502"
frequency_ms = 500
"#;

    /// 构造绑定测试配置的 MgmtState（共享态 + 启动快照）。
    fn test_state() -> MgmtState {
        let config = Arc::new(GatewayConfig::parse(TEST_TOML).expect("parse"));
        let daemon = DaemonShared::new();
        daemon.set_config(Arc::new(crate::config::ConfigShared::new(
            (*config).clone(),
        )));
        MgmtState::new(daemon, config)
    }

    /// 在 127.0.0.1 随机端口启动 axum 服务（本机回环，不依赖外网）。
    async fn spawn_server(state: MgmtState) -> u16 {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let port = listener.local_addr().expect("addr").port();
        tokio::spawn(async move {
            axum::serve(listener, router(state))
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

    /// 手写 HTTP GET（Connection: close，读至 EOF），整体 3s 超时防挂死。
    ///
    /// 说明：tower 未成为 daemon 直接依赖，无法 `tower::ServiceExt::oneshot`；
    /// reqwest 亦不在依赖树——按任务决议采用「axum::serve 随机回环端口 +
    /// 手写 TcpStream HTTP GET 解析响应」。
    async fn http_get(port: u16, path: &str) -> (u16, String, String) {
        tokio::time::timeout(Duration::from_secs(3), http_get_inner(port, path))
            .await
            .expect("http_get timed out")
    }

    async fn http_get_inner(port: u16, path: &str) -> (u16, String, String) {
        let mut stream = TcpStream::connect(("127.0.0.1", port))
            .await
            .expect("connect");
        let request =
            format!("GET {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n");
        stream.write_all(request.as_bytes()).await.expect("write");
        stream.flush().await.expect("flush");
        let mut buf = Vec::new();
        stream.read_to_end(&mut buf).await.expect("read");
        parse_response(&String::from_utf8(buf).expect("utf8"))
    }

    /// QA Happy: /api/health → 200 + {"status":"ok"} + 版本号。
    #[tokio::test]
    async fn health_returns_ok_with_version() {
        let port = spawn_server(test_state()).await;
        let (status, _, body) = http_get(port, "/api/health").await;
        assert_eq!(status, 200);
        let value: Value = serde_json::from_str(&body).expect("json body");
        assert_eq!(value["status"], "ok");
        assert_eq!(value["version"], env!("CARGO_PKG_VERSION"));
    }

    /// QA（task-61 D-01）：`/healthz` 别名与 `/api/health` 完全等价
    /// （200 + 同 payload）——部署文档 / install.sh 的健康判据。
    #[tokio::test]
    async fn healthz_alias_matches_api_health() {
        let port = spawn_server(test_state()).await;
        let (status, _, body) = http_get(port, "/healthz").await;
        assert_eq!(status, 200, "/healthz alias must answer 200");
        let value: Value = serde_json::from_str(&body).expect("json body");
        assert_eq!(value["status"], "ok");
        assert_eq!(value["version"], env!("CARGO_PKG_VERSION"));
    }

    /// QA 红线: /api/status 全部整数语义字段必须为字符串编码（uptime / 纳秒 / 计数）。
    #[tokio::test]
    async fn status_integer_fields_are_strings() {
        let state = test_state();
        // 注入一个 u64::MAX 量级的纳秒心跳，验证大数不被 JSON number 精度截断。
        state.daemon().touch_heartbeat(u64::MAX);
        let port = spawn_server(state).await;

        let (status, _, body) = http_get(port, "/api/status").await;
        assert_eq!(status, 200);
        let value: Value = serde_json::from_str(&body).expect("json body");

        for field in [
            "uptime_secs",
            "last_heartbeat_ns",
            "device_count",
            "outlet_count",
            "config_version",
        ] {
            assert!(
                value[field].is_string(),
                "{field} must be a JSON string (大数红线), got: {}",
                value[field]
            );
        }
        // u64::MAX 纳秒以字符串无损往返。
        assert_eq!(
            value["last_heartbeat_ns"],
            u64::MAX.to_string(),
            "large heartbeat must round-trip losslessly"
        );
        assert_eq!(value["state"], "starting");
        assert_eq!(value["device_count"], "1");
        assert_eq!(value["outlet_count"], "2");
    }

    /// QA: /api/devices 摘要（去重、poll_interval_ms 字符串、enabled=true）；
    /// /api/points 按设备过滤 + 未知设备空数组；/api/outlets target 对外标识。
    #[tokio::test]
    async fn devices_points_outlets_aggregations() {
        let port = spawn_server(test_state()).await;

        // devices
        let (status, _, body) = http_get(port, "/api/devices").await;
        assert_eq!(status, 200);
        let devices: Vec<Value> = serde_json::from_str(&body).expect("array");
        assert_eq!(devices.len(), 1, "one deduped device");
        assert_eq!(devices[0]["id"], "dev-01");
        assert_eq!(devices[0]["protocol"], "modbus-tcp");
        assert_eq!(devices[0]["enabled"], true);
        assert!(
            devices[0]["poll_interval_ms"].is_string(),
            "poll_interval_ms must be string, got {}",
            devices[0]["poll_interval_ms"]
        );
        assert_eq!(devices[0]["poll_interval_ms"], "100", "min frequency");

        // points（命中设备）
        let (status, _, body) = http_get(port, "/api/points?device_id=dev-01").await;
        assert_eq!(status, 200);
        let points: Vec<Value> = serde_json::from_str(&body).expect("array");
        assert_eq!(points.len(), 2);
        assert_eq!(points[0]["point_id"], "p_temp");
        assert!(points[0]["frequency_ms"].is_string(), "大数红线");
        assert_eq!(points[0]["frequency_ms"], "100");

        // points（未知设备 → 空数组，不报错）
        let (status, _, body) = http_get(port, "/api/points?device_id=nope").await;
        assert_eq!(status, 200);
        let points: Vec<Value> = serde_json::from_str(&body).expect("array");
        assert!(points.is_empty(), "unknown device → empty array");

        // outlets
        let (status, _, body) = http_get(port, "/api/outlets").await;
        assert_eq!(status, 200);
        let outlets: Vec<Value> = serde_json::from_str(&body).expect("array");
        assert_eq!(outlets.len(), 2);
        assert_eq!(outlets[0]["name"], "north-1");
        assert_eq!(outlets[0]["target"], "mqtts://broker.local:8883");
        assert!(outlets[0]["qos"].is_string(), "qos 大数红线");
        assert_eq!(outlets[0]["qos"], "1");
        assert_eq!(outlets[0]["encoding"], "protobuf");
        assert_eq!(outlets[1]["encoding"], "json");
    }

    /// 建立 SSE 连接（不设 Connection: close，流保持打开）。
    async fn sse_connect(port: u16, path: &str) -> TcpStream {
        let mut sock = TcpStream::connect(("127.0.0.1", port))
            .await
            .expect("connect");
        let req =
            format!("GET {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nAccept: text/event-stream\r\n\r\n");
        sock.write_all(req.as_bytes()).await.expect("write request");
        sock
    }

    /// 从 SSE 流缓冲中取出一帧完整 data: JSON（帧以空行结束）。
    fn try_take_frame(acc: &mut String) -> Option<Value> {
        let end = acc.find("\n\n")?;
        let frame: String = acc.drain(..end + 2).collect();
        frame.lines().find_map(|line| {
            line.strip_prefix("data: ")
                .and_then(|rest| serde_json::from_str(rest).ok())
        })
    }

    /// 阻塞读取下一帧 data JSON（30s 兜底超时）。
    ///
    /// `acc` 由调用方持有并在多次调用间复用：一次 `read()` 可能带回多帧，
    /// 取出第一帧后剩余字节必须留在缓冲里，否则会被静默丢弃。
    async fn sse_next_data(sock: &mut TcpStream, acc: &mut String) -> Value {
        let mut buf = vec![0u8; 4096];
        let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
        loop {
            if let Some(value) = try_take_frame(acc) {
                return value;
            }
            let remain = deadline.saturating_duration_since(tokio::time::Instant::now());
            assert!(!remain.is_zero(), "sse read timeout: buffered={acc}");
            let n = match tokio::time::timeout(remain, sock.read(&mut buf)).await {
                Ok(r) => r.expect("io error"),
                Err(_elapsed) => panic!("sse read timeout: buffered={acc}"),
            };
            assert!(n > 0, "sse connection closed unexpectedly: {acc}");
            acc.push_str(&String::from_utf8_lossy(&buf[..n]));
        }
    }

    /// QA: SSE 事件流——首连快照必达（seq 大数红线）、seq 断线重连回放、实时推送。
    #[tokio::test]
    async fn events_sse_first_frame_replay_and_live() {
        let state = test_state();
        let port = spawn_server(state.clone()).await;

        // ① 首连（seq=0）：首帧必须是 lifecycle_changed 快照，seq 为字符串（大数红线）。
        let mut sock = sse_connect(port, "/api/events").await;
        let mut acc_first = String::new();
        let first = sse_next_data(&mut sock, &mut acc_first).await;
        assert_eq!(first["type"], "lifecycle_changed");
        assert_eq!(first["state"], "starting");
        assert!(first["seq"].is_string(), "seq must be string: {first}");

        // ② 断线重连回放：先发布一个事件，再带 seq=<首帧序号> 重连，
        //    回放序列应为 [config_reloaded, device_changed]——读到目标帧为止。
        let _ = state.publish(MgmtEvent::DeviceChanged {
            device_id: "dev-01".to_string(),
        });
        let cursor = first["seq"].as_str().expect("seq string").to_string();
        let mut replay = sse_connect(port, &format!("/api/events?seq={cursor}")).await;
        let mut acc_replay = String::new();
        let replayed = sse_next_data(&mut replay, &mut acc_replay).await;
        assert_eq!(replayed["type"], "config_reloaded", "replay: {replayed}");
        let replayed = sse_next_data(&mut replay, &mut acc_replay).await;
        assert_eq!(replayed["type"], "device_changed", "replay: {replayed}");
        assert_eq!(replayed["device_id"], "dev-01", "replay: {replayed}");

        // ③ 实时推送：发布于连接建立之后的事件，在同一条流上送达。
        let state2 = state.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(100)).await;
            state2.publish(MgmtEvent::DeviceChanged {
                device_id: "dev-02".to_string(),
            });
        });
        // 连接建立于 dev-02 发布之前——流上可能出现前奏帧，循环直到见到 dev-02。
        let mut live = sse_connect(port, &format!("/api/events?seq={cursor}")).await;
        let mut acc_live = String::new();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        loop {
            assert!(
                tokio::time::Instant::now() < deadline,
                "live event dev-02 not delivered in time"
            );
            let frame = tokio::time::timeout(
                Duration::from_secs(5),
                sse_next_data(&mut live, &mut acc_live),
            )
            .await
            .expect("frame future");
            if frame["type"] == "device_changed" && frame["device_id"] == "dev-02" {
                break;
            }
        }
    }

    /// QA: 静态文件 200（/ 与 /assets、Content-Type 按扩展名）。
    #[tokio::test]
    async fn static_files_served_with_content_types() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("index.html"), "<html>iot-daq</html>").expect("write index");
        std::fs::create_dir(dir.path().join("assets")).expect("mkdir");
        std::fs::write(dir.path().join("assets/app.js"), "console.log(1)").expect("write js");
        std::fs::write(dir.path().join("assets/logo.svg"), "<svg/>").expect("write svg");

        let port = spawn_server(test_state().with_web_dist(dir.path())).await;

        // GET / → index.html，text/html。
        let (status, headers, body) = http_get(port, "/").await;
        assert_eq!(status, 200);
        assert!(headers.contains("text/html"), "headers: {headers}");
        assert_eq!(body, "<html>iot-daq</html>");

        // GET /assets/app.js → application/javascript。
        let (status, headers, body) = http_get(port, "/assets/app.js").await;
        assert_eq!(status, 200);
        assert!(headers.contains("javascript"), "headers: {headers}");
        assert_eq!(body, "console.log(1)");

        // GET /assets/logo.svg → image/svg+xml。
        let (status, headers, _) = http_get(port, "/assets/logo.svg").await;
        assert_eq!(status, 200);
        assert!(headers.contains("image/svg+xml"), "headers: {headers}");
    }

    /// QA Error: 静态资源缺失 → 404（hash 路由无需 index.html 回退，注释见 serve_file）；
    /// 路径穿越（`..` 与百分号编码）→ 403 或 404，绝不泄露根外文件。
    #[tokio::test]
    async fn static_missing_is_404_and_traversal_is_rejected() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("index.html"), "<html>ok</html>").expect("write index");
        std::fs::write(dir.path().join("secret.txt"), "TOP_SECRET").expect("write secret");

        let port = spawn_server(test_state().with_web_dist(dir.path())).await;

        // 缺失资源 → 404（不回退 index.html）。
        let (status, _, _) = http_get(port, "/assets/missing.js").await;
        assert_eq!(status, 404);

        // 字面 `..` 穿越 → 403 或 404（两防线任一拦截均可），body 不得含密文。
        let (status, _, body) = http_get(port, "/assets/../secret.txt").await;
        assert!(
            status == 403 || status == 404,
            "traversal must be rejected, got {status}"
        );
        assert!(!body.contains("TOP_SECRET"), "must not leak: {body}");

        // 百分号编码的 `..`（%2e%2e）同样被拒。
        let (status, _, body) = http_get(port, "/assets/%2e%2e/secret.txt").await;
        assert!(
            status == 403 || status == 404,
            "encoded traversal must be rejected, got {status}"
        );
        assert!(!body.contains("TOP_SECRET"), "must not leak: {body}");

        // 绝对路径注入同样被拒。
        let (status, _, _) = http_get(port, "/assets//etc/passwd").await;
        assert!(status == 403 || status == 404, "got {status}");
    }

    /// QA 红线: MgmtEvent JSON 编码——u64 版本号为字符串、类型名小写下划线。
    #[test]
    fn mgmt_event_json_encodes_u64_as_string() {
        let reloaded = MgmtEvent::ConfigReloaded { version: u64::MAX };
        assert_eq!(reloaded.type_name(), "config_reloaded");
        let json = reloaded.to_json();
        assert!(
            json.contains(&format!("\"version\":\"{}\"", u64::MAX)),
            "version must be a JSON string (大数红线): {json}"
        );

        let lifecycle = MgmtEvent::LifecycleChanged {
            state: LifecycleState::Running,
        };
        assert_eq!(lifecycle.type_name(), "lifecycle_changed");
        assert!(lifecycle.to_json().contains("\"state\":\"running\""));

        let device = MgmtEvent::DeviceChanged {
            device_id: "dev-9".to_string(),
        };
        assert_eq!(device.type_name(), "device_changed");
        assert!(device.to_json().contains("\"device_id\":\"dev-9\""));
    }

    /// QA: 事件序号单调递增；历史环容量上限（环形淘汰最旧）。
    #[test]
    fn event_seq_monotonic_and_history_ring_evicts_oldest() {
        let state = test_state();
        assert_eq!(state.current_seq(), 0);
        assert_eq!(
            state.publish(MgmtEvent::DeviceChanged {
                device_id: "a".to_string()
            }),
            1
        );
        assert_eq!(
            state.publish(MgmtEvent::DeviceChanged {
                device_id: "b".to_string()
            }),
            2
        );
        assert_eq!(state.current_seq(), 2);

        // 回放完整。
        let replay = state.history_since(0);
        assert_eq!(replay.len(), 2);
        assert_eq!(replay[0].seq, 1);
        assert_eq!(replay[1].seq, 2);
        // 增量语义：seq=1 之后只有第二条。
        let replay = state.history_since(1);
        assert_eq!(replay.len(), 1);
        assert_eq!(
            replay[0].event,
            MgmtEvent::DeviceChanged {
                device_id: "b".to_string()
            }
        );

        // 历史环满 → 淘汰最旧，序号不回退。
        for i in 0..(EVENT_HISTORY_CAPACITY + 8) {
            state.publish(MgmtEvent::DeviceChanged {
                device_id: format!("dev-{i}"),
            });
        }
        let replay = state.history_since(0);
        assert_eq!(replay.len(), EVENT_HISTORY_CAPACITY, "ring capped");
        assert_eq!(
            replay[0].event,
            MgmtEvent::DeviceChanged {
                device_id: format!("dev-{}", 8),
            },
            "oldest evicted first"
        );
    }

    /// QA: 相对路径安全校验——普通/当前目录分量放行，其余（..、绝对、盘符）拒绝。
    #[test]
    fn safe_rel_path_component_whitelist() {
        assert!(safe_rel_path("index.html").is_some());
        assert!(safe_rel_path("assets/app.js").is_some());
        assert!(safe_rel_path("assets/./app.js").is_some(), "CurDir ok");
        assert!(safe_rel_path("").is_none(), "empty rejected");
        assert!(safe_rel_path("../secret").is_none(), "ParentDir rejected");
        assert!(safe_rel_path("a/../../b").is_none(), "nested .. rejected");
        assert!(safe_rel_path("/etc/passwd").is_none(), "absolute rejected");
        #[cfg(windows)]
        assert!(safe_rel_path("C:/boot").is_none(), "drive prefix rejected");
    }
}

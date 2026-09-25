//! task 36 — 远程运维端点（mgmt 管理面扩展）：`/api/ops/*` + 审计 + 鉴权注入点。
//!
//! ## 职责边界
//! - **做**：三个运维端点（restart / collectors / logs）、[`OpsAuthorizer`]
//!   鉴权 trait + 注入点（默认 **拒绝一切**，fail-closed）、[`OpsAuditEvent`]
//!   审计环（每个动作必记，含被拒动作）、mgmt 事件历史环的带时间戳捕获
//!   （`/api/ops/logs` 数据源）。
//! - **不做**：~~统一鉴权中间件（task 57）~~ → task 57 全量接线已完成：`/api/ops/*`
//!   由 `mod.rs::ops_guard` 中间件守卫（JWT 401 + RBAC 403），三个 handler 内
//!   保留 `ensure_action` 二次校验（防御纵深）；`install()` 仍是运维运行时
//!   （审计环 / 事件捕获）的装配点，`OpsAuthorizer` 降级为「无可信角色源」的
//!   fail-closed 兜底（见 `DenyAllOpsAuthorizer` 注释）；
//!   文件系统日志聚合（task 18/57 范畴，见 `logs` 注释）；运行期采集器启停能力
//!   （task 37 调度器拓扑，见 `collectors` 诚实降级说明）。
//!
//! ## 安全契约
//! 1. **所有运维动作必须鉴权 + 审计**：默认 authorizer 拒绝一切（fail-closed）；
//!    未安装 runtime 的实例在首个请求到达时自动落到默认 runtime（同样全拒）；
//!    拒绝动作同样写入审计环。
//! 2. **危险动作二次确认**：restart 请求体必须携带 `confirm: "<gateway_id>"`
//!    （回显对象名），缺失或不匹配 → 400 + 审计。
//! 3. **远程重启不直接杀进程**：动作 = [`crate::bootstrap::DaemonShared::request_shutdown`]
//!    请求优雅停机，由调用方 Supervisor / 服务管理器（Windows 服务 / systemd）
//!    负责拉起；响应体 `note` 字段向运维方说明该语义。
//!
//! ## 实现红线
//! - 零 panic：锁中毒一律 `into_inner` 恢复；JSON 解析失败走 400 路径。
//! - **大数红线**：JSON 里时间戳（`ts_ns` / `ts_ms`）/ `seq` / 计数一律**字符串编码**
//!   （手工 `serde_json::json!`，不走 derive 序列化）。
//! - 状态注入点：`MgmtState` 本体不可改（task 52 红线），故 runtime 以
//!   「实例指针 → runtime」侧表注册（[`runtime_for`]），`install()` 在启动 /
//!   测试装配时调用一次。缺省自动落 fail-closed runtime，绝不 panic。

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::{SystemTime, UNIX_EPOCH};

use axum::body::Bytes;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Json, Response};
use serde::Deserialize;
use serde_json::{json, Value};

use super::rbac::{permission_for_ops_action, AuthRejection, AuthedRole};
use super::{MgmtEvent, MgmtState, MgmtStateInner};

/// 审计环容量（超出按环形淘汰最旧）。
pub const AUDIT_RING_CAPACITY: usize = 1024;
/// 运维事件捕获环容量（`/api/ops/logs` 回放窗口）。
pub const OPS_LOG_CAPACITY: usize = 1024;

// ---- 审计结果常量 ----

/// 动作被受理（已生效或已受理待执行）。
pub const OUTCOME_ACCEPTED: &str = "accepted";
/// 被鉴权拒绝。
pub const OUTCOME_DENIED: &str = "denied";
/// 请求参数非法（未进入授权判定或授权后参数校验失败）。
pub const OUTCOME_BAD_REQUEST: &str = "bad_request";
/// 能力未实现（诚实降级，管线已跑通）。
pub const OUTCOME_NOT_IMPLEMENTED: &str = "not_implemented";

// ---- 动作枚举 ----

/// 运维动作枚举（审计 `action` 字段的取值域）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpsAction {
    /// 远程重启（优雅停机语义）。
    Restart,
    /// 采集器运行期暂停。
    CollectorsPause,
    /// 采集器运行期恢复。
    CollectorsResume,
    /// 运维日志查询。
    LogsRead,
    // —— 设备/点位配置写（mgmt writeapi；写动作含被拒均入审计环）——
    /// 设备登记（`POST /api/devices`）。
    DeviceCreate,
    /// 设备改名/启停（`PUT /api/devices/{id}`）。
    DeviceUpdate,
    /// 设备删除（`DELETE /api/devices/{id}`）。
    DeviceDelete,
    /// 点位新增（`POST /api/points`）。
    PointCreate,
    /// 点位修改（`PUT /api/points/{device_id}/{point_id}`）。
    PointUpdate,
    /// 点位删除（`DELETE /api/points/{device_id}/{point_id}`）。
    PointDelete,
    // —— 页面级补齐端点（mgmt pages；写/探测动作含被拒均入审计环）——
    /// 点位批量导入（`POST /api/points/import`）。
    PointImport,
    /// 配置回滚（`POST /api/settings/rollback`）。
    SettingsRollback,
    /// 设备连通性探测（`POST /api/devices/test`）。
    DeviceTest,
    /// 北向出口登记（`POST /api/forwarders`；诚实 501 占位动作）。
    ForwarderCreate,
    /// 北向出口连通性探测（`POST /api/forwarders/{id}/test`）。
    ForwarderTest,
    /// 告警规则写（`PUT /api/alerts/rules`；诚实 501 占位动作）。
    AlarmRulesWrite,
    /// 设置持久化写（`PUT /api/settings`；mgmt settings 页面真实落盘）。
    SettingsWrite,
    /// 远程停机（`POST /api/ops/stop`；优雅停机，不承诺拉起——比 restart 更保守）。
    Stop,
}

impl OpsAction {
    /// 动作字面量（小写下划线；鉴权策略与审计共用）。
    pub fn as_str(self) -> &'static str {
        match self {
            OpsAction::Restart => "restart",
            OpsAction::CollectorsPause => "collectors_pause",
            OpsAction::CollectorsResume => "collectors_resume",
            OpsAction::LogsRead => "logs_read",
            OpsAction::DeviceCreate => "device_create",
            OpsAction::DeviceUpdate => "device_update",
            OpsAction::DeviceDelete => "device_delete",
            OpsAction::PointCreate => "point_create",
            OpsAction::PointUpdate => "point_update",
            OpsAction::PointDelete => "point_delete",
            OpsAction::PointImport => "point_import",
            OpsAction::SettingsRollback => "settings_rollback",
            OpsAction::DeviceTest => "device_test",
            OpsAction::ForwarderCreate => "forwarder_create",
            OpsAction::ForwarderTest => "forwarder_test",
            OpsAction::AlarmRulesWrite => "alarm_rules_write",
            OpsAction::SettingsWrite => "settings_write",
            OpsAction::Stop => "stop",
        }
    }
}

// ---- 鉴权 ----

/// 运维动作鉴权 trait（task 57 统一鉴权中间件的接线点）。
///
/// 默认实现 [`DenyAllOpsAuthorizer`] = 拒绝一切（fail-closed）；
/// 生产策略在 `install()` 时注入，测试注入 allow 版本。
pub trait OpsAuthorizer: Send + Sync + 'static {
    /// 判定 `actor` 是否可执行 `action`（动作字面量见 [`OpsAction::as_str`]）。
    fn authorize(&self, actor: &str, action: &str) -> bool;
}

/// 默认鉴权策略：拒绝一切（fail-closed）。
///
/// task 57 全量接线后语义：三个 handler 的授权判定已切换为「可信 JWT role →
/// `permission_for_ops_action` → `ensure`」链（守卫中间件见 `mod.rs::ops_guard`），
/// 本策略**不再参与该链**，保留为「无可信角色源」路径的 fail-closed 兜底
///（未装配 runtime 的实例自动落 [`runtime_for`] 默认值；未来任何绕过
/// extractor 的新代码路径默认全拒）。
pub struct DenyAllOpsAuthorizer;

impl OpsAuthorizer for DenyAllOpsAuthorizer {
    fn authorize(&self, _actor: &str, _action: &str) -> bool {
        false
    }
}

/// 测试 / 开发用全放行策略（**禁止在生产装配中使用**）。
pub struct AllowAllOpsAuthorizer;

impl OpsAuthorizer for AllowAllOpsAuthorizer {
    fn authorize(&self, _actor: &str, _action: &str) -> bool {
        true
    }
}

// ---- 时钟 ----

/// 时间戳注入闭包（返回 UTC 纳秒）；测试注入受控时钟保证确定性。
pub type OpsClock = Arc<dyn Fn() -> u64 + Send + Sync>;

/// 当前 UTC 纳秒（Unix 纪元起；时钟早于纪元按 0 处理，不 panic）。
fn system_clock_ns() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0)
}

/// 默认墙钟。
pub fn default_clock() -> OpsClock {
    Arc::new(system_clock_ns)
}

// ---- 审计 ----

/// 单条运维审计记录（每个动作必记，**被拒动作同样记录**）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpsAuditEvent {
    /// 审计环内序号（单调递增；JSON 编码为字符串，大数红线）。
    pub seq: u64,
    /// 记录时刻（UTC 纳秒，来自注入时钟；JSON 编码为字符串）。
    pub ts_ns: u64,
    /// 操作者（请求体 `actor`；缺失时记 `<missing>` / `<unknown>`）。
    pub actor: String,
    /// 动作字面量（[`OpsAction::as_str`]）。
    pub action: String,
    /// 鉴权是否放行（`false` = 被拒，同样入环）。
    pub allowed: bool,
    /// 结果字面量（accepted / denied / bad_request / not_implemented）。
    pub outcome: String,
    /// 原因 / 细节（审计人读）。
    pub reason: String,
}

impl OpsAuditEvent {
    /// 序列化为 JSON（`seq` / `ts_ns` 一律字符串，大数红线）。
    pub fn to_json(&self) -> Value {
        json!({
            "seq": self.seq.to_string(),
            "ts_ns": self.ts_ns.to_string(),
            "actor": self.actor,
            "action": self.action,
            "allowed": self.allowed,
            "outcome": self.outcome,
            "reason": self.reason,
        })
    }
}

// ---- 运维事件捕获 ----

/// 带毫秒时间戳的 mgmt 事件捕获条目（`/api/ops/logs` 的数据源单元）。
///
/// 来源：`install()` 时订阅 MgmtState 事件广播，逐条打时间戳入环。
/// **诚实限制**：install 之前发布的事件没有时间戳、不入环（不回溯伪造时间）；
/// 广播积压超限（容量 64）丢段时按 warn 记录（历史环仍是完整副本，但无时间戳）。
#[derive(Debug, Clone)]
pub struct OpsLogEntry {
    /// 捕获时刻（UTC 毫秒，来自注入时钟；JSON 编码为字符串）。
    pub ts_ms: u64,
    /// 事件序号（MgmtState 全局单调递增；JSON 编码为字符串）。
    pub seq: u64,
    /// 事件载荷。
    pub event: MgmtEvent,
}

// ---- 运行时（鉴权 + 审计 + 捕获） ----

/// 单个 MgmtState 实例的运维运行时：authorizer + 时钟 + 审计环 + 事件捕获环。
pub struct OpsRuntime {
    authorizer: Arc<dyn OpsAuthorizer>,
    clock: OpsClock,
    audit: Mutex<VecDeque<OpsAuditEvent>>,
    log: Mutex<VecDeque<OpsLogEntry>>,
    audit_seq: AtomicU64,
}

impl OpsRuntime {
    fn new(authorizer: Arc<dyn OpsAuthorizer>, clock: OpsClock) -> Self {
        Self {
            authorizer,
            clock,
            audit: Mutex::new(VecDeque::with_capacity(16)),
            log: Mutex::new(VecDeque::with_capacity(16)),
            audit_seq: AtomicU64::new(0),
        }
    }

    /// 委托 authorizer 判定。
    pub fn authorize(&self, actor: &str, action: &str) -> bool {
        self.authorizer.authorize(actor, action)
    }

    /// 追加一条审计（环形淘汰最旧；锁中毒恢复，零 panic）。
    pub fn record_audit(
        &self,
        actor: &str,
        action: OpsAction,
        allowed: bool,
        outcome: &str,
        reason: &str,
    ) {
        let event = OpsAuditEvent {
            seq: self.audit_seq.fetch_add(1, Ordering::Relaxed) + 1,
            ts_ns: (self.clock)(),
            actor: actor.to_string(),
            action: action.as_str().to_string(),
            allowed,
            outcome: outcome.to_string(),
            reason: reason.to_string(),
        };
        let mut ring = self.audit.lock().unwrap_or_else(|p| p.into_inner());
        if ring.len() >= AUDIT_RING_CAPACITY {
            ring.pop_front();
        }
        ring.push_back(event);
    }

    /// 审计环快照（保序；供测试与后续审计查询端点使用）。
    pub fn audit_snapshot(&self) -> Vec<OpsAuditEvent> {
        self.audit
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .iter()
            .cloned()
            .collect()
    }

    /// 事件捕获环快照（保序）。
    pub fn log_snapshot(&self) -> Vec<OpsLogEntry> {
        self.log
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .iter()
            .cloned()
            .collect()
    }

    /// 事件入捕获环（环形淘汰最旧）。
    fn push_log(&self, entry: OpsLogEntry) {
        let mut ring = self.log.lock().unwrap_or_else(|p| p.into_inner());
        if ring.len() >= OPS_LOG_CAPACITY {
            ring.pop_front();
        }
        ring.push_back(entry);
    }
}

// ---- 实例侧表（MgmtState 不可改，按实例指针身份注册 runtime） ----

/// 实例指针 → runtime 侧表。
///
/// 条目 = `(键, 弱引用, runtime)`：键 = `Arc<MgmtStateInner>` 堆地址；弱引用 =
/// 指向同一 `MgmtStateInner` 的 [`Weak`]，用于**驱逐已销毁实例的残留条目**。
/// 每实例一条、随进程存活（生产仅 1 条；测试每用例 1 条，量级可忽略，
/// 故用 `Vec` 线性查找，避免非常量 `HashMap::new`）。
///
/// ## 为什么必须持有 `Weak`（ABA / 堆地址复用缺陷修复）
/// 键是**堆地址**。测试用例（生产：配置热重载 / 重启装配路径若重建
/// `MgmtState`）会新建实例、用完即 `drop`，其 `MgmtStateInner` 的堆地址会被
/// 下一次分配**复用**。若条目从不移除，复用同一地址的新实例会在 [`runtime_for`]
/// 中**命中上一已销毁实例残留的条目**，从而静默继承其
/// 1) **审计环**（新实例首次查询即看到旧实例的审计痕）；
/// 2) **已注入的授权策略**（[`install_with_clock`] 写入的 authorizer）。
///
/// 若旧实例装的是**宽松策略**，新实例即 **fail-open**——这是**安全相关**缺陷，
/// 非纯测试问题。
///
/// ## 驱逐语义（弱引用失效即驱逐）
/// 每次取锁后、匹配前先 [`prune_dead_instances`]：
/// `retain(|(_, weak, _)| weak.strong_count() > 0)`。已销毁实例
/// （`strong_count() == 0`）的条目被清掉，被复用的地址不再命中残留条目。
/// `strong_count()` 是瞬时值，但 `retain` 与随后的匹配在**同一把锁内**完成，
/// 故两次观测一致。
///
/// **前提（并发正确性）**：实例的 `MgmtState` 必须在其存活期间**持有 `Arc`**
/// （`state.inner`），弱引用才有升级目标；只要 `MgmtState` 存活，其条目就不会被
/// 误驱逐。反之，`MgmtState` 一旦销毁，其条目最迟在下一次 [`runtime_for`] /
/// [`install_with_clock`] 取锁时被清除。
/// 侧表条目类型别名（消除 `clippy::type_complexity`）：
/// `(键 = MgmtStateInner 堆地址, 该实例的弱引用, 该实例的 runtime)`。
type OpsRegistryEntry = (usize, Weak<MgmtStateInner>, Arc<OpsRuntime>);

static OPS_RUNTIMES: Mutex<Vec<OpsRegistryEntry>> = Mutex::new(Vec::new());

fn runtime_key(state: &MgmtState) -> usize {
    Arc::as_ptr(&state.inner) as usize
}

/// 侧表驱逐：移除已销毁实例（弱引用失效）的条目。
///
/// 必须在**持有 `OPS_RUNTIMES` 锁的临界区内**调用，与随后的键匹配同属一个
/// 临界区，从而保证「驱逐 → 匹配」之间没有新条目被并发插入/移除
/// （`strong_count()` 的瞬时值在该临界区内一致）。
fn prune_dead_instances(registry: &mut Vec<OpsRegistryEntry>) {
    registry.retain(|(_, weak, _)| weak.strong_count() > 0);
}

/// 取实例 runtime；未安装时自动落 **fail-closed 默认 runtime**（拒绝一切 + 墙钟），
/// 保证未装配实例的运维请求同样被拒且被审计（绝不 panic、绝不放行）。
pub fn runtime_for(state: &MgmtState) -> Arc<OpsRuntime> {
    let key = runtime_key(state);
    let mut registry = OPS_RUNTIMES.lock().unwrap_or_else(|p| p.into_inner());
    // 先驱逐已销毁实例的残留条目：避免被复用的堆地址命中上一实例的条目
    //（否则会静默继承其审计环与授权策略，见 OPS_RUNTIMES 注释）。
    prune_dead_instances(&mut registry);
    match registry.iter().position(|(k, _, _)| *k == key) {
        Some(idx) => registry[idx].2.clone(),
        None => {
            let runtime = Arc::new(OpsRuntime::new(
                Arc::new(DenyAllOpsAuthorizer),
                default_clock(),
            ));
            registry.push((key, Arc::downgrade(&state.inner), runtime.clone()));
            runtime
        }
    }
}

/// [`runtime_for`] 的语义别名：审计 / 事件日志查询入口（供测试与后续审计端点）。
pub fn ops_runtime(state: &MgmtState) -> Arc<OpsRuntime> {
    runtime_for(state)
}

/// 为实例安装运维运行时（鉴权策略注入点；默认墙钟）。
///
/// 同时启动事件捕获任务（订阅 MgmtState 事件广播）；当前无 tokio 运行时上下文
/// 时跳过捕获（`/api/ops/logs` 返回空数组，不 panic）——仅诊断 warn。
pub fn install(state: &MgmtState, authorizer: Arc<dyn OpsAuthorizer>) {
    install_with_clock(state, authorizer, default_clock());
}

/// [`install`] 的带时钟版本（测试注入受控时钟，保证时间过滤确定性）。
pub fn install_with_clock(state: &MgmtState, authorizer: Arc<dyn OpsAuthorizer>, clock: OpsClock) {
    let runtime = Arc::new(OpsRuntime::new(authorizer, clock));
    {
        let key = runtime_key(state);
        let mut registry = OPS_RUNTIMES.lock().unwrap_or_else(|p| p.into_inner());
        // 同上：先驱逐已销毁实例的残留条目，再命中 / 新建。
        prune_dead_instances(&mut registry);
        // 当前实例的弱引用：命中（原地替换）与未命中（新建）都必须写入最新 weak，
        // 否则 retain 会把刚写入的条目误判为「已销毁」而立即删除。
        let weak = Arc::downgrade(&state.inner);
        match registry.iter().position(|(k, _, _)| *k == key) {
            Some(idx) => {
                registry[idx].1 = weak;
                registry[idx].2 = runtime.clone();
            }
            None => registry.push((key, weak, runtime.clone())),
        }
    }
    spawn_capture(state, runtime);
}

/// 测试钩子：读取侧表当前条目数（**仅测试构建**暴露，生产不可见）。
///
/// 注意：全局长度会被**并行运行的其他测试**扰动（本仓 `mgmt::remote_ops` /
/// `mgmt::writeapi` 用例会并发注册实例），故断言侧表「不累积」时应优先使用
/// 以 runtime 指针身份匹配的 [`registry_count_for_runtime`]（不受并发扰动）。
#[cfg(test)]
pub(crate) fn registry_len() -> usize {
    OPS_RUNTIMES.lock().unwrap_or_else(|p| p.into_inner()).len()
}

/// 测试钩子：统计侧表中持有**指定 runtime** 的条目数（**仅测试构建**暴露）。
///
/// 以 `Arc::as_ptr` 身份匹配：其他并发用例的条目 runtime 指针不同，故该计数
/// 只反映「本用例注册的 runtime 是否仍在侧表」，在并行测试下**确定性可靠**。
#[cfg(test)]
pub(crate) fn registry_count_for_runtime(runtime: &Arc<OpsRuntime>) -> usize {
    let target = Arc::as_ptr(runtime);
    OPS_RUNTIMES
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .iter()
        .filter(|(_, _, rt)| Arc::as_ptr(rt) == target)
        .count()
}

/// 事件捕获任务：订阅广播 → 逐条打毫秒时间戳入捕获环。
fn spawn_capture(state: &MgmtState, runtime: Arc<OpsRuntime>) {
    if tokio::runtime::Handle::try_current().is_err() {
        tracing::warn!("remote_ops: no tokio runtime at install; event capture disabled");
        return;
    }
    let mut rx = state.subscribe();
    tokio::spawn(async move {
        loop {
            match rx.recv().await {
                Ok(envelope) => {
                    let ts_ms = (runtime.clock)() / 1_000_000;
                    runtime.push_log(OpsLogEntry {
                        ts_ms,
                        seq: envelope.seq,
                        event: envelope.event,
                    });
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(dropped)) => {
                    tracing::warn!(dropped, "remote_ops: event capture lagged; gap in ops log");
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
            }
        }
    });
}

// ---- 局部响应辅助 ----

/// 400 + JSON 错误体。
fn bad_request(code: &str, message: &str) -> Response {
    (
        StatusCode::BAD_REQUEST,
        Json(json!({ "error": code, "message": message })),
    )
        .into_response()
}

/// 403 + JSON 错误体（不泄露策略细节）。
fn forbidden(message: &str) -> Response {
    (
        StatusCode::FORBIDDEN,
        Json(json!({ "error": "forbidden", "message": message })),
    )
        .into_response()
}

// ---- 请求体 ----

/// task 57：可信角色 → RBAC 判定（授权判定唯一入口 = 权限矩阵，fail-closed：
/// 映射外动作一律拒绝）。守卫中间件（`mod.rs::ops_guard`）已做过同等判定，
/// handler 内保留二次校验为**防御纵深**。
fn ensure_action(authed: &AuthedRole, action: OpsAction) -> Result<(), AuthRejection> {
    match permission_for_ops_action(action.as_str()) {
        Some(permission) => authed.ensure(permission),
        None => Err(AuthRejection::Forbidden(format!(
            "unknown ops action {:?}",
            action.as_str()
        ))),
    }
}

/// POST /api/ops/restart 请求体（字段缺失按空串校验，避免 serde 硬失败路径分叉）。
#[derive(Debug, Default, Deserialize)]
struct RestartBody {
    #[serde(default)]
    actor: String,
    #[serde(default)]
    confirm: String,
    #[serde(default)]
    reason: String,
}

/// POST /api/ops/collectors 请求体。
#[derive(Debug, Default, Deserialize)]
struct CollectorsBody {
    #[serde(default)]
    actor: String,
    #[serde(default)]
    action: String,
}

// ---- REST 处理器 ----

/// POST /api/ops/restart → 请求优雅停机（**不直接杀进程**）。
///
/// 管线：解析 → actor 校验 → 鉴权（task 57：可信 JWT role 经
/// [`ensure_action`] 判定；守卫中间件已先行 401/403，此处为二次校验）→
/// 二次确认（`confirm` 必须回显当前 `gateway_id`，缺失/不匹配 → 400 + 审计）→
/// [`DaemonShared::request_shutdown`]。通过 → 200 `{accepted, mode: "graceful", note}`；
/// note 说明停机由 Supervisor / 服务管理器拉起（Windows 服务 / systemd 形态）。
///
/// **诚实限制**：本端点只保证「优雅停机请求已受理」；重启是否成功取决于
/// 外部 Supervisor 的拉起策略（与看门狗/task 51 的单次重启钩子同一契约）。
pub async fn restart(State(state): State<MgmtState>, authed: AuthedRole, body: Bytes) -> Response {
    let runtime = runtime_for(&state);

    let req: RestartBody = match serde_json::from_slice(&body) {
        Ok(req) => req,
        Err(err) => {
            runtime.record_audit(
                "<unknown>",
                OpsAction::Restart,
                false,
                OUTCOME_BAD_REQUEST,
                &format!("malformed json body: {err}"),
            );
            return bad_request(
                "bad_request",
                "body must be a JSON object {actor, confirm, reason}",
            );
        }
    };

    if req.actor.trim().is_empty() {
        runtime.record_audit(
            "<missing>",
            OpsAction::Restart,
            false,
            OUTCOME_BAD_REQUEST,
            "actor is required",
        );
        return bad_request("bad_request", "actor is required");
    }

    if let Err(rejection) = ensure_action(&authed, OpsAction::Restart) {
        runtime.record_audit(
            &req.actor,
            OpsAction::Restart,
            false,
            OUTCOME_DENIED,
            &rejection.to_string(),
        );
        return forbidden("restart denied by authorizer");
    }

    // 二次确认：confirm 必须回显当前 gateway_id（从运行期配置读，热重载感知）。
    let gateway_id = state.daemon().config_snapshot().gateway.gateway_id.clone();
    if req.confirm.trim().is_empty() {
        runtime.record_audit(
            &req.actor,
            OpsAction::Restart,
            true,
            OUTCOME_BAD_REQUEST,
            "confirm missing (echo gateway id required)",
        );
        return bad_request(
            "confirm_required",
            &format!("confirm must echo the gateway id {gateway_id:?}"),
        );
    }
    if req.confirm != gateway_id {
        runtime.record_audit(
            &req.actor,
            OpsAction::Restart,
            true,
            OUTCOME_BAD_REQUEST,
            "confirm mismatch",
        );
        return bad_request(
            "confirm_mismatch",
            &format!("confirm does not match gateway id {gateway_id:?}"),
        );
    }

    // 远程重启 = 请求优雅停机；拉起由 Supervisor / 服务管理器负责。
    state.daemon().request_shutdown();
    runtime.record_audit(
        &req.actor,
        OpsAction::Restart,
        true,
        OUTCOME_ACCEPTED,
        &format!("graceful shutdown requested; reason={:?}", req.reason),
    );
    tracing::info!(
        actor = %req.actor,
        "remote_ops: graceful shutdown requested via /api/ops/restart"
    );
    Json(json!({
        "accepted": true,
        "mode": "graceful",
        "note": "shutdown requested via DaemonShared::request_shutdown; the supervisor / service manager (Windows service, systemd) is responsible for relaunching the daemon",
    }))
    .into_response()
}

/// POST /api/ops/stop → 请求优雅停机（**不承诺拉起**，契约与 restart 对齐）。
///
/// ## 安全停机裁决（为何可实现而非 501）
/// 既有 [`crate::bootstrap::DaemonShared::request_shutdown`] 的语义是**置位
/// shutdown watch 标志后立即返回**：主循环在下一轮 tick 才消费该标志执行优雅
/// 停机（先完成在途工作）。因此 handler 可以**先构造并送达 HTTP 响应、再置位
/// 停机标志**，响应不会因进程先死而丢失——与 `/api/ops/restart` 同一已验证的
/// 机制（见该端点测试 `restart_accepted_triggers_graceful_shutdown`），故本端点
/// 可安全实现，无须诚实 501。
///
/// 与 restart 的差异：stop **不假设任何拉起方**（响应 `note` 明确说明进程将
/// 退出、是否拉起由 Supervisor / 服务管理器自行决定），restart 的语义是
/// 「停机后由 Supervisor 拉起」。审计动作字面量独立为 `stop`（便于区分运维
/// 意图），权限与 restart 共享 `ops.restart`（仅 system）。
pub async fn stop(State(state): State<MgmtState>, authed: AuthedRole, body: Bytes) -> Response {
    let runtime = runtime_for(&state);

    let req: RestartBody = match serde_json::from_slice(&body) {
        Ok(req) => req,
        Err(err) => {
            runtime.record_audit(
                "<unknown>",
                OpsAction::Stop,
                false,
                OUTCOME_BAD_REQUEST,
                &format!("malformed json body: {err}"),
            );
            return bad_request(
                "bad_request",
                "body must be a JSON object {actor, confirm, reason}",
            );
        }
    };

    if req.actor.trim().is_empty() {
        runtime.record_audit(
            "<missing>",
            OpsAction::Stop,
            false,
            OUTCOME_BAD_REQUEST,
            "actor is required",
        );
        return bad_request("bad_request", "actor is required");
    }

    if let Err(rejection) = ensure_action(&authed, OpsAction::Stop) {
        runtime.record_audit(
            &req.actor,
            OpsAction::Stop,
            false,
            OUTCOME_DENIED,
            &rejection.to_string(),
        );
        return forbidden("stop denied by authorizer");
    }

    // 二次确认：与 restart 同契约（confirm 必须回显当前 gateway_id）。
    let gateway_id = state.daemon().config_snapshot().gateway.gateway_id.clone();
    if req.confirm.trim().is_empty() {
        runtime.record_audit(
            &req.actor,
            OpsAction::Stop,
            true,
            OUTCOME_BAD_REQUEST,
            "confirm missing (echo gateway id required)",
        );
        return bad_request(
            "confirm_required",
            &format!("confirm must echo the gateway id {gateway_id:?}"),
        );
    }
    if req.confirm != gateway_id {
        runtime.record_audit(
            &req.actor,
            OpsAction::Stop,
            true,
            OUTCOME_BAD_REQUEST,
            "confirm mismatch",
        );
        return bad_request(
            "confirm_mismatch",
            &format!("confirm does not match gateway id {gateway_id:?}"),
        );
    }

    state.daemon().request_shutdown();
    runtime.record_audit(
        &req.actor,
        OpsAction::Stop,
        true,
        OUTCOME_ACCEPTED,
        &format!("graceful stop requested; reason={:?}", req.reason),
    );
    tracing::info!(
        actor = %req.actor,
        "remote_ops: graceful stop requested via /api/ops/stop"
    );
    Json(json!({
        "accepted": true,
        "mode": "graceful_stop",
        "note": "graceful shutdown requested via DaemonShared::request_shutdown; the daemon will exit after in-flight work completes — relaunching (if desired) is the supervisor / service manager's decision",
    }))
    .into_response()
}

/// POST /api/ops/collectors → 采集器运行期启停（**诚实降级：501**）。
///
/// 鉴权 / actor / action 校验 / 审计管线**完整实现**；当前 daemon 无运行期
/// 采集开关（启停需调度器拓扑开关，task 37），故授权通过后返回
/// 501 `{error: "not_implemented", planned, reason}` + 审计（not_implemented）。
/// 「管线先于能力」：能力落地时仅替换 501 分支，安全管线零改动。
pub async fn collectors(
    State(state): State<MgmtState>,
    authed: AuthedRole,
    body: Bytes,
) -> Response {
    let runtime = runtime_for(&state);

    let req: CollectorsBody = match serde_json::from_slice(&body) {
        Ok(req) => req,
        Err(err) => {
            runtime.record_audit(
                "<unknown>",
                OpsAction::CollectorsPause,
                false,
                OUTCOME_BAD_REQUEST,
                &format!("malformed json body: {err}"),
            );
            return bad_request(
                "bad_request",
                "body must be a JSON object {actor, action: \"pause\"|\"resume\"}",
            );
        }
    };

    if req.actor.trim().is_empty() {
        runtime.record_audit(
            "<missing>",
            OpsAction::CollectorsPause,
            false,
            OUTCOME_BAD_REQUEST,
            "actor is required",
        );
        return bad_request("bad_request", "actor is required");
    }

    let action = match req.action.as_str() {
        "pause" => OpsAction::CollectorsPause,
        "resume" => OpsAction::CollectorsResume,
        other => {
            runtime.record_audit(
                &req.actor,
                OpsAction::CollectorsPause,
                false,
                OUTCOME_BAD_REQUEST,
                &format!("invalid action {other:?} (pause|resume)"),
            );
            return bad_request("bad_request", "action must be \"pause\" or \"resume\"");
        }
    };

    if let Err(rejection) = ensure_action(&authed, action) {
        runtime.record_audit(
            &req.actor,
            action,
            false,
            OUTCOME_DENIED,
            &rejection.to_string(),
        );
        return forbidden("collectors action denied by authorizer");
    }

    // 授权通过但能力未实现：诚实降级 501，审计照记。
    runtime.record_audit(
        &req.actor,
        action,
        true,
        OUTCOME_NOT_IMPLEMENTED,
        "runtime collector switch requires scheduler topology (task 37)",
    );
    (
        StatusCode::NOT_IMPLEMENTED,
        Json(json!({
            "error": "not_implemented",
            "planned": "runtime collector pause/resume via scheduler switch (task 37)",
            "reason": "运行期启停需调度器开关（task 37 拓扑）；鉴权与审计管线已就绪",
        })),
    )
        .into_response()
}

/// GET /api/ops/logs?actor=&amp;since=&amp;until=&amp;level= → 运维事件日志查询。
///
/// - 数据源 = mgmt 事件历史环的**带时间戳捕获**（install 后订阅广播逐条打点；
///   `since`/`until` 为毫秒时间戳字符串（大数红线），闭区间 `[since, until]`，
///   缺省任一侧 = 不限）；范围参数解析失败 → 400 + 审计；
/// - `level` 按事件类型过滤：`lifecycle|config|device`（或完整类型名）；
///   未知名 → 400；缺省 = 全类型；
/// - 返回 JSON 数组，`seq` / `ts_ms` 一律字符串（大数红线）。
///
/// **诚实限制（不读文件系统日志）**：文件日志聚合属 task 18/57 范畴；本端点
/// 只回放 install 之后发生的 mgmt 管理事件（lifecycle/config/device），
/// 不含采集数据与文件日志，不回溯伪造 install 前事件的时间戳。
/// 查询本身也是运维动作：actor 必填、鉴权、审计（logs_read）。
pub async fn logs(
    State(state): State<MgmtState>,
    authed: AuthedRole,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    let runtime = runtime_for(&state);

    let actor = params.get("actor").map(String::as_str).unwrap_or("");
    if actor.trim().is_empty() {
        runtime.record_audit(
            "<missing>",
            OpsAction::LogsRead,
            false,
            OUTCOME_BAD_REQUEST,
            "actor query parameter is required",
        );
        return bad_request("bad_request", "actor query parameter is required");
    }

    if let Err(rejection) = ensure_action(&authed, OpsAction::LogsRead) {
        runtime.record_audit(
            actor,
            OpsAction::LogsRead,
            false,
            OUTCOME_DENIED,
            &rejection.to_string(),
        );
        return forbidden("logs_read denied by authorizer");
    }

    let since = match parse_opt_ms(params.get("since"), "since") {
        Ok(v) => v,
        Err(message) => {
            runtime.record_audit(
                actor,
                OpsAction::LogsRead,
                true,
                OUTCOME_BAD_REQUEST,
                &message,
            );
            return bad_request("bad_request", &message);
        }
    };
    let until = match parse_opt_ms(params.get("until"), "until") {
        Ok(v) => v,
        Err(message) => {
            runtime.record_audit(
                actor,
                OpsAction::LogsRead,
                true,
                OUTCOME_BAD_REQUEST,
                &message,
            );
            return bad_request("bad_request", &message);
        }
    };
    if let (Some(s), Some(u)) = (since, until) {
        if u < s {
            let message = format!("until ({u}) must not be earlier than since ({s})");
            runtime.record_audit(
                actor,
                OpsAction::LogsRead,
                true,
                OUTCOME_BAD_REQUEST,
                &message,
            );
            return bad_request("bad_request", &message);
        }
    }

    let level = match params.get("level").map(String::as_str) {
        None | Some("") => None,
        Some(raw) => match normalize_level(raw) {
            Some(type_name) => Some(type_name),
            None => {
                let message = format!(
                    "unknown level {raw:?}; expected lifecycle|config|device (or full type name)"
                );
                runtime.record_audit(
                    actor,
                    OpsAction::LogsRead,
                    true,
                    OUTCOME_BAD_REQUEST,
                    &message,
                );
                return bad_request("bad_request", &message);
            }
        },
    };

    let rows: Vec<Value> = runtime
        .log_snapshot()
        .iter()
        .filter(|entry| level.is_none_or(|name| entry.event.type_name() == name))
        .filter(|entry| since.is_none_or(|s| entry.ts_ms >= s))
        .filter(|entry| until.is_none_or(|u| entry.ts_ms <= u))
        .map(log_entry_to_json)
        .collect();

    runtime.record_audit(
        actor,
        OpsAction::LogsRead,
        true,
        OUTCOME_ACCEPTED,
        &format!("returned {} entries", rows.len()),
    );
    Json(Value::Array(rows)).into_response()
}

/// 解析可选毫秒时间戳参数（缺省 / 空串 = None；非 u64 → Err）。
fn parse_opt_ms(raw: Option<&String>, name: &str) -> Result<Option<u64>, String> {
    let Some(v) = raw else {
        return Ok(None);
    };
    if v.is_empty() {
        return Ok(None);
    }
    v.parse::<u64>()
        .map(Some)
        .map_err(|_| format!("{name} must be a u64 millisecond timestamp string, got {v:?}"))
}

/// `level` 参数 → 事件类型名（接受短名与完整名）。
fn normalize_level(raw: &str) -> Option<&'static str> {
    match raw {
        "lifecycle" | "lifecycle_changed" => Some("lifecycle_changed"),
        "config" | "config_reloaded" => Some("config_reloaded"),
        "device" | "device_changed" => Some("device_changed"),
        _ => None,
    }
}

/// 捕获条目 → JSON（复用 `MgmtEvent::to_json` 的手工编码，seq/ts 补字符串）。
fn log_entry_to_json(entry: &OpsLogEntry) -> Value {
    // 事件 JSON 由 MgmtEvent 自身生成，解析失败理论不可达；兜底 unknown 不 panic。
    let mut value: Value = serde_json::from_str(&entry.event.to_json())
        .unwrap_or_else(|_| json!({ "type": "unknown" }));
    value["seq"] = Value::String(entry.seq.to_string());
    value["ts_ms"] = Value::String(entry.ts_ms.to_string());
    value
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bootstrap::{DaemonShared, LifecycleState};
    use crate::config::{ConfigShared, GatewayConfig};
    use crate::mgmt::auth_jwt::{now_unix_secs, sign, Claims};
    use crate::mgmt::rbac::Role;
    use std::time::Duration;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::{TcpListener, TcpStream};

    /// 测试配置：gateway_id = "gw-test"（restart confirm 回显目标）。
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

    /// 受控时钟句柄（测试注入，纳秒可写）。
    #[derive(Clone)]
    struct TestClock(Arc<AtomicU64>);

    impl TestClock {
        fn at(ns: u64) -> Self {
            Self(Arc::new(AtomicU64::new(ns)))
        }
        fn set(&self, ns: u64) {
            self.0.store(ns, Ordering::Relaxed);
        }
        fn clock(&self) -> OpsClock {
            let inner = self.0.clone();
            Arc::new(move || inner.load(Ordering::Relaxed))
        }
    }

    /// 构造绑定测试配置 + 指定鉴权/时钟的 MgmtState（独立实例，不污染其他测试）。
    fn make_state(authorizer: Arc<dyn OpsAuthorizer>, clock: Option<TestClock>) -> MgmtState {
        let config = Arc::new(GatewayConfig::parse(TEST_TOML).expect("parse"));
        let daemon = DaemonShared::new();
        daemon.set_config(Arc::new(ConfigShared::new((*config).clone())));
        let state = MgmtState::new(daemon, config);
        match clock {
            Some(clock) => install_with_clock(&state, authorizer, clock.clock()),
            None => install(&state, authorizer),
        }
        state
    }

    /// 构造**未安装** runtime 的裸 `MgmtState`（独立实例，不污染其他测试）：
    /// 用于验证「未装配实例自动落 fail-closed 默认 runtime」与侧表驱逐语义
    /// （裸实例在首次 `runtime_for` 时才注册默认 runtime）。
    fn bare_state() -> MgmtState {
        let config = Arc::new(GatewayConfig::parse(TEST_TOML).expect("parse"));
        let daemon = DaemonShared::new();
        daemon.set_config(Arc::new(ConfigShared::new((*config).clone())));
        MgmtState::new(daemon, config)
    }

    /// task 57：以 state 的实际签名密钥签发测试 token（默认 dev 兜底密钥，
    /// 与 `MgmtState::new` 的装配一致；sub 取 "ops-admin" 对齐审计断言）。
    fn token_for(state: &MgmtState, role: Role) -> String {
        let now = now_unix_secs();
        let claims = Claims {
            sub: "ops-admin".to_string(),
            role,
            exp: now + 600,
            iat: now,
            nbf: None,
            jti: "test-jti".to_string(),
        };
        sign(&claims, state.auth().key()).expect("sign test token")
    }

    /// 在 127.0.0.1 随机端口启动 axum 服务（本机回环）。
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

    /// 手写 HTTP 请求（method/path/body/token），整体 3s 超时防挂死。
    ///
    /// task 57：`/api/ops/*` 由 ops_guard 中间件守卫——需带 Bearer token 的
    /// 用例使用 `http_get_bearer` / `http_post_bearer`。
    async fn http_request(
        port: u16,
        method: &str,
        path: &str,
        body: Option<&str>,
        token: Option<&str>,
    ) -> (u16, String, String) {
        tokio::time::timeout(Duration::from_secs(3), async move {
            let mut stream = TcpStream::connect(("127.0.0.1", port)).await.expect("connect");
            let body = body.unwrap_or("");
            let auth = token
                .map(|t| format!("Authorization: Bearer {t}\r\n"))
                .unwrap_or_default();
            let request = if method == "POST" {
                format!(
                    "POST {path} HTTP/1.1\r\nHost: 127.0.0.1\r\n{auth}Content-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                )
            } else {
                format!("GET {path} HTTP/1.1\r\nHost: 127.0.0.1\r\n{auth}Connection: close\r\n\r\n")
            };
            stream.write_all(request.as_bytes()).await.expect("write");
            stream.flush().await.expect("flush");
            let mut buf = Vec::new();
            stream.read_to_end(&mut buf).await.expect("read");
            parse_response(&String::from_utf8(buf).expect("utf8"))
        })
        .await
        .expect("http_request timed out")
    }

    async fn http_get(port: u16, path: &str) -> (u16, String, String) {
        http_request(port, "GET", path, None, None).await
    }

    async fn http_post(port: u16, path: &str, body: &str) -> (u16, String, String) {
        http_request(port, "POST", path, Some(body), None).await
    }

    async fn http_get_bearer(port: u16, path: &str, token: &str) -> (u16, String, String) {
        http_request(port, "GET", path, None, Some(token)).await
    }

    async fn http_post_bearer(
        port: u16,
        path: &str,
        body: &str,
        token: &str,
    ) -> (u16, String, String) {
        http_request(port, "POST", path, Some(body), Some(token)).await
    }

    /// 等待事件捕获环达到指定条数（捕获任务是异步的，轮询 + 兜底超时）。
    async fn wait_for_log_len(state: &MgmtState, want: usize) {
        for _ in 0..500 {
            if ops_runtime(state).log_snapshot().len() >= want {
                return;
            }
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
        panic!(
            "capture did not reach {want} entries (got {})",
            ops_runtime(state).log_snapshot().len()
        );
    }

    // ---- 单元测试：动作枚举与默认 authorizer ----

    /// QA: OpsAction 字面量映射稳定（鉴权策略与审计共用，改了会破坏策略契约）。
    #[test]
    fn ops_action_str_mapping_is_stable() {
        assert_eq!(OpsAction::Restart.as_str(), "restart");
        assert_eq!(OpsAction::CollectorsPause.as_str(), "collectors_pause");
        assert_eq!(OpsAction::CollectorsResume.as_str(), "collectors_resume");
        assert_eq!(OpsAction::LogsRead.as_str(), "logs_read");
    }

    /// QA: 默认 authorizer 全拒（fail-closed），allow 版本全放行。
    #[test]
    fn authorizer_defaults_deny_and_allow() {
        let deny = DenyAllOpsAuthorizer;
        assert!(!deny.authorize("ops-admin", "restart"));
        assert!(!deny.authorize("root", "logs_read"));

        let allow = AllowAllOpsAuthorizer;
        assert!(allow.authorize("ops-admin", "restart"));
        assert!(allow.authorize("anyone", "collectors_pause"));
    }

    // ---- 默认 fail-closed ----

    /// QA 安全: 未带 token 的运维请求 → 401（ops_guard 中间件在 ops runtime
    /// 之前拒绝；身份未建立，不留审计痕——运行时不可能被匿名请求触达）。
    #[tokio::test]
    async fn restart_fail_closed_even_without_install() {
        // 不走 make_state：裸 MgmtState（无 runtime 注入）。
        let config = Arc::new(GatewayConfig::parse(TEST_TOML).expect("parse"));
        let daemon = DaemonShared::new();
        daemon.set_config(Arc::new(ConfigShared::new((*config).clone())));
        let state = MgmtState::new(daemon, config);
        let port = spawn_server(state.clone()).await;

        let (status, _, body) = http_post(
            port,
            "/api/ops/restart",
            r#"{"actor":"ops-admin","confirm":"gw-test"}"#,
        )
        .await;
        assert_eq!(
            status, 401,
            "unauthenticated ops request must be 401: {body}"
        );
        assert!(
            ops_runtime(&state).audit_snapshot().is_empty(),
            "request rejected before the ops runtime must not forge audit entries"
        );
    }

    // ---- ABA（堆地址复用）侧表驱逐 ----

    /// QA ABA 修复（**确定性**）: 侧表驱逐已销毁实例的条目——被复用的堆地址上
    /// 新建的实例绝不继承上一实例的**审计痕**，且旧条目被清除而非累积。
    ///
    /// 断言一律**以 runtime 指针身份**判定（[`registry_count_for_runtime`]）：
    /// 既不依赖分配器是否真的复用地址（ABA 触发与否都成立），也不受并行用例
    /// 对全局侧表长度的扰动（故本测试是**确定性**的，不会假红）。
    #[test]
    fn ops_registry_prunes_dead_instances() {
        // 实例 A：安装可辨识 authorizer（AllowAll）并触发一次审计，留下审计痕。
        let state_a = bare_state();
        install(&state_a, Arc::new(AllowAllOpsAuthorizer));
        let rt_a = ops_runtime(&state_a);
        rt_a.record_audit(
            "ops-admin",
            OpsAction::Restart,
            true,
            OUTCOME_ACCEPTED,
            "aba-marker",
        );
        assert!(
            !rt_a.audit_snapshot().is_empty(),
            "A must carry an audit entry before being dropped"
        );
        assert_eq!(
            registry_count_for_runtime(&rt_a),
            1,
            "A's runtime must be registered exactly once"
        );

        // 销毁 A：其 MgmtStateInner 堆地址进入可复用状态（下一步新建实例极可能复用）。
        drop(state_a);

        // 实例 B：新建并取 runtime。修复前若地址被复用会命中 A 的残留条目。
        let state_b = bare_state();
        let rt_b = ops_runtime(&state_b);

        // ① B 不得继承 A 的审计环（核心行为断言）。
        assert!(
            rt_b.audit_snapshot().is_empty(),
            "fresh instance B must NOT inherit A's audit entries"
        );
        // ② B 拿到的是全新 runtime，而非 A 的残留条目（指针身份不同）。
        assert_ne!(
            Arc::as_ptr(&rt_b),
            Arc::as_ptr(&rt_a),
            "B must get a fresh runtime, not A's recycled entry"
        );
        // ③ A 的条目已被驱逐（未累积）；B 的条目恰好在册。
        assert_eq!(
            registry_count_for_runtime(&rt_a),
            0,
            "A's registry entry must be evicted once A is dropped"
        );
        assert_eq!(
            registry_count_for_runtime(&rt_b),
            1,
            "B's runtime must be registered exactly once"
        );
        // 注：此处**不**断言侧表「全局长度 = 基线 + 1」——`mgmt` 下多个用例在
        // 同一测试二进制内并行运行，会并发注册/销毁实例，全局长度在并行测试下
        // 非确定性（实测 baseline=0 → after=19）。「A 的条目被驱逐、未累积」已由
        // ③ 以 runtime 指针身份**确定性**证明；「全局不累积」由
        // `ops_registry_does_not_accumulate_dead_instances` 覆盖。
    }

    /// QA ABA 修复（**抗累积**）: 反复「新建 → 注册 → 销毁」实例，侧表不得线性累积。
    ///
    /// 若不驱逐已销毁条目，每轮至少 +1，`ROUNDS` 轮后长度将增长 `ROUNDS` 量级
    /// （实测无驱逐时会 +256 以上）；驱逐生效时长度基本持平。断言阈值取
    /// `ROUNDS / 2`（远小于无驱逐的增长、远大于并行用例对全局长度的扰动），
    /// 故对「不累积」是**稳健**回归（修复前必红、修复后必绿）。
    #[test]
    fn ops_registry_does_not_accumulate_dead_instances() {
        const ROUNDS: usize = 256;
        let before = registry_len();
        for _ in 0..ROUNDS {
            let a = bare_state();
            let _ = ops_runtime(&a); // 注册 A（fail-closed 默认 runtime）
            drop(a); // A 销毁 → 其条目成为可驱逐的死条目
            let b = bare_state();
            let _ = ops_runtime(&b); // 取锁时驱逐 A，再注册 B
            drop(b);
        }
        let after = registry_len();
        assert!(
            after < before + ROUNDS / 2,
            "registry must be pruned, not accumulate: before={before}, after={after}, rounds={ROUNDS}"
        );
    }

    /// QA 安全回归: A 装**宽松** authorizer（AllowAll）→ drop → 新建 B：
    /// B 的运维请求仍须 **fail-closed 拒绝**，证明授权策略不跨实例串台
    ///（修复前若堆地址被复用，B 会静默继承 A 的 AllowAll → **fail-open**）。
    #[test]
    fn fresh_state_never_inherits_previous_authorizer() {
        let state_a = bare_state();
        install(&state_a, Arc::new(AllowAllOpsAuthorizer));
        let rt_a = ops_runtime(&state_a);
        assert!(
            rt_a.authorize("ops-admin", OpsAction::Restart.as_str()),
            "A's injected AllowAll policy must allow restart (sanity)"
        );
        drop(state_a);

        let state_b = bare_state();
        let rt_b = ops_runtime(&state_b);
        assert!(
            !rt_b.authorize("ops-admin", OpsAction::Restart.as_str()),
            "fresh instance B must stay fail-closed, NOT inherit A's AllowAll policy"
        );
        assert!(
            rt_b.audit_snapshot().is_empty(),
            "fresh instance B must not inherit A's audit entries"
        );
    }

    // ---- restart ----

    /// QA 安全: ops 角色 token（不持 ops.restart）→ 守卫中间件 403 + 审计
    ///（被拒动作入审计环；DenyAll 兜底策略不参与该判定链，见模块注释）。
    #[tokio::test]
    async fn restart_denied_by_default_authorizer() {
        let state = make_state(Arc::new(DenyAllOpsAuthorizer), None);
        let token = token_for(&state, Role::Ops);
        let port = spawn_server(state.clone()).await;

        let (status, _, body) = http_post_bearer(
            port,
            "/api/ops/restart",
            r#"{"actor":"ops-admin","confirm":"gw-test","reason":"maintenance"}"#,
            &token,
        )
        .await;
        assert_eq!(status, 403, "{body}");
        assert!(!state.daemon().shutdown_requested(), "must NOT shutdown");

        let audit = ops_runtime(&state).audit_snapshot();
        assert!(audit.iter().any(|e| e.action == "restart" && !e.allowed));
    }

    /// QA: 守卫中间件 403 的审计痕——actor 取 JWT sub、action/outcome 齐备。
    #[tokio::test]
    async fn guard_denied_action_is_audited_with_jwt_sub() {
        let state = make_state(Arc::new(DenyAllOpsAuthorizer), None);
        let token = token_for(&state, Role::Ops);
        let port = spawn_server(state.clone()).await;

        let _ = http_post_bearer(
            port,
            "/api/ops/restart",
            r#"{"actor":"body-actor","confirm":"gw-test"}"#,
            &token,
        )
        .await;

        let audit = ops_runtime(&state).audit_snapshot();
        assert_eq!(audit.len(), 1);
        // 中间件层拿不到 body（不可重放读取），actor 以可信 JWT sub 为准。
        assert_eq!(audit[0].actor, "ops-admin");
        assert_eq!(audit[0].action, "restart");
        assert!(!audit[0].allowed);
        assert_eq!(audit[0].outcome, OUTCOME_DENIED);
    }

    /// QA（task 57 语义变更锚定）: 已装配 DenyAll 兜底策略的实例，携带**可信
    /// system 角色** token 的请求照常放行——授权判定 = RBAC 权限矩阵，
    /// authorizer 仅兜底「无可信角色源」路径。
    #[tokio::test]
    async fn restart_accepted_with_denyall_when_role_trusted() {
        let state = make_state(Arc::new(DenyAllOpsAuthorizer), None);
        let token = token_for(&state, Role::System);
        let port = spawn_server(state.clone()).await;

        let (status, _, body) = http_post_bearer(
            port,
            "/api/ops/restart",
            r#"{"actor":"ops-admin","confirm":"gw-test"}"#,
            &token,
        )
        .await;
        assert_eq!(status, 200, "{body}");
    }

    /// QA: system 角色守卫通过后，confirm 缺失 → 400（二次确认语义）+ 审计。
    #[tokio::test]
    async fn restart_missing_confirm_is_400() {
        let state = make_state(Arc::new(AllowAllOpsAuthorizer), None);
        let token = token_for(&state, Role::System);
        let port = spawn_server(state.clone()).await;

        let (status, _, body) =
            http_post_bearer(port, "/api/ops/restart", r#"{"actor":"ops-admin"}"#, &token).await;
        assert_eq!(status, 400, "{body}");
        let value: Value = serde_json::from_str(&body).expect("json");
        assert_eq!(value["error"], "confirm_required");
        assert!(!state.daemon().shutdown_requested(), "must NOT shutdown");

        let audit = ops_runtime(&state).audit_snapshot();
        assert!(audit
            .iter()
            .any(|e| e.action == "restart" && e.outcome == OUTCOME_BAD_REQUEST));
    }

    /// QA: confirm 与 gateway_id 不匹配 → 400 + 审计。
    #[tokio::test]
    async fn restart_confirm_mismatch_is_400() {
        let state = make_state(Arc::new(AllowAllOpsAuthorizer), None);
        let token = token_for(&state, Role::System);
        let port = spawn_server(state.clone()).await;

        let (status, _, body) = http_post_bearer(
            port,
            "/api/ops/restart",
            r#"{"actor":"ops-admin","confirm":"gw-wrong","reason":"x"}"#,
            &token,
        )
        .await;
        assert_eq!(status, 400, "{body}");
        let value: Value = serde_json::from_str(&body).expect("json");
        assert_eq!(value["error"], "confirm_mismatch");
        assert!(!state.daemon().shutdown_requested(), "must NOT shutdown");
    }

    /// QA Happy: confirm 匹配 → 200 {accepted, mode: graceful} + note 说明
    /// supervisor 拉起语义 + shutdown watch 观测到停机请求 + 审计 accepted。
    ///
    /// 独立 MgmtState/DaemonShared 实例，停机只影响本用例。
    #[tokio::test]
    async fn restart_accepted_triggers_graceful_shutdown() {
        let state = make_state(Arc::new(AllowAllOpsAuthorizer), None);
        let token = token_for(&state, Role::System);
        let shutdown_rx = state.daemon().subscribe_shutdown();
        assert!(!*shutdown_rx.borrow());
        let port = spawn_server(state.clone()).await;

        let (status, _, body) = http_post_bearer(
            port,
            "/api/ops/restart",
            r#"{"actor":"ops-admin","confirm":"gw-test","reason":"scheduled maintenance"}"#,
            &token,
        )
        .await;
        assert_eq!(status, 200, "{body}");
        let value: Value = serde_json::from_str(&body).expect("json");
        assert_eq!(value["accepted"], true);
        assert_eq!(value["mode"], "graceful");
        let note = value["note"].as_str().expect("note string");
        assert!(
            note.contains("supervisor"),
            "response must explain supervisor relaunch semantics: {note}"
        );

        // 订阅 shutdown watch 断言 request_shutdown 真正生效。
        for _ in 0..100 {
            if *shutdown_rx.borrow() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
        assert!(*shutdown_rx.borrow(), "shutdown watch must observe request");

        let audit = ops_runtime(&state).audit_snapshot();
        let accepted = audit
            .iter()
            .find(|e| e.action == "restart" && e.allowed && e.outcome == OUTCOME_ACCEPTED)
            .expect("accepted audit entry");
        assert_eq!(accepted.actor, "ops-admin");
        assert!(accepted.reason.contains("scheduled maintenance"));
    }

    /// QA Error: 恶意 JSON body → 400 + 审计，不 panic。
    #[tokio::test]
    async fn restart_malformed_json_is_400() {
        let state = make_state(Arc::new(AllowAllOpsAuthorizer), None);
        let token = token_for(&state, Role::System);
        let port = spawn_server(state.clone()).await;

        let (status, _, _) =
            http_post_bearer(port, "/api/ops/restart", "not-json{{{", &token).await;
        assert_eq!(status, 400);
        assert!(!state.daemon().shutdown_requested());
        assert!(ops_runtime(&state)
            .audit_snapshot()
            .iter()
            .any(|e| e.action == "restart" && e.outcome == OUTCOME_BAD_REQUEST));
    }

    /// QA Error: actor 缺失 → 400（鉴权主体不可为空）。
    #[tokio::test]
    async fn restart_missing_actor_is_400() {
        let state = make_state(Arc::new(AllowAllOpsAuthorizer), None);
        let token = token_for(&state, Role::System);
        let port = spawn_server(state.clone()).await;

        let (status, _, _) =
            http_post_bearer(port, "/api/ops/restart", r#"{"confirm":"gw-test"}"#, &token).await;
        assert_eq!(status, 400);
        assert!(!state.daemon().shutdown_requested());
    }

    // ---- stop ----

    /// QA Happy: system 角色 + confirm 匹配 → 200 {accepted, mode:
    /// graceful_stop}；shutdown watch 观测到停机请求（先响应后停机的安全机制
    /// 与 restart 同源）；审计动作字面量 = stop（accepted）。
    #[tokio::test]
    async fn stop_accepted_triggers_graceful_shutdown() {
        let state = make_state(Arc::new(AllowAllOpsAuthorizer), None);
        let token = token_for(&state, Role::System);
        let shutdown_rx = state.daemon().subscribe_shutdown();
        assert!(!*shutdown_rx.borrow());
        let port = spawn_server(state.clone()).await;

        let (status, _, body) = http_post_bearer(
            port,
            "/api/ops/stop",
            r#"{"actor":"ops-admin","confirm":"gw-test","reason":"decommission"}"#,
            &token,
        )
        .await;
        assert_eq!(status, 200, "{body}");
        let value: Value = serde_json::from_str(&body).expect("json");
        assert_eq!(value["accepted"], true);
        assert_eq!(value["mode"], "graceful_stop");
        let note = value["note"].as_str().expect("note string");
        assert!(
            note.contains("supervisor"),
            "note must state relaunch is the supervisor's decision: {note}"
        );

        for _ in 0..100 {
            if *shutdown_rx.borrow() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
        assert!(
            *shutdown_rx.borrow(),
            "shutdown watch must observe stop request"
        );

        let audit = ops_runtime(&state).audit_snapshot();
        let accepted = audit
            .iter()
            .find(|e| e.action == "stop" && e.allowed && e.outcome == OUTCOME_ACCEPTED)
            .expect("accepted stop audit entry");
        assert_eq!(accepted.actor, "ops-admin");
        assert!(accepted.reason.contains("decommission"));
    }

    /// QA: confirm 不匹配 / 缺失 → 400（与 restart 同一二次确认契约），
    /// 且不触发停机。
    #[tokio::test]
    async fn stop_confirm_validation_is_400() {
        let state = make_state(Arc::new(AllowAllOpsAuthorizer), None);
        let token = token_for(&state, Role::System);
        let port = spawn_server(state.clone()).await;

        // 缺 confirm。
        let (status, _, body) =
            http_post_bearer(port, "/api/ops/stop", r#"{"actor":"ops-admin"}"#, &token).await;
        assert_eq!(status, 400, "{body}");
        let value: Value = serde_json::from_str(&body).expect("json");
        assert_eq!(value["error"], "confirm_required");

        // confirm 与 gateway_id 不匹配。
        let (status, _, body) = http_post_bearer(
            port,
            "/api/ops/stop",
            r#"{"actor":"ops-admin","confirm":"gw-wrong"}"#,
            &token,
        )
        .await;
        assert_eq!(status, 400, "{body}");
        let value: Value = serde_json::from_str(&body).expect("json");
        assert_eq!(value["error"], "confirm_mismatch");

        assert!(!state.daemon().shutdown_requested(), "must NOT shutdown");
        assert!(ops_runtime(&state)
            .audit_snapshot()
            .iter()
            .all(|e| e.outcome == OUTCOME_BAD_REQUEST));
    }

    /// QA 安全: ops 角色（不持 ops.restart——stop 共享该权限）→ 403 + 审计，
    /// 不触发停机。
    #[tokio::test]
    async fn stop_denied_for_role_without_permission_is_403() {
        let state = make_state(Arc::new(AllowAllOpsAuthorizer), None);
        let token = token_for(&state, Role::Ops);
        let port = spawn_server(state.clone()).await;

        let (status, _, body) = http_post_bearer(
            port,
            "/api/ops/stop",
            r#"{"actor":"ops-admin","confirm":"gw-test"}"#,
            &token,
        )
        .await;
        assert_eq!(status, 403, "{body}");
        assert!(!state.daemon().shutdown_requested(), "must NOT shutdown");
        let audit = ops_runtime(&state).audit_snapshot();
        assert!(audit.iter().any(|e| e.action == "stop" && !e.allowed));
    }

    /// QA Error: 恶意 JSON / 缺 actor → 400 + 审计，不 panic、不停机。
    #[tokio::test]
    async fn stop_bad_requests_are_400_and_audited() {
        let state = make_state(Arc::new(AllowAllOpsAuthorizer), None);
        let token = token_for(&state, Role::System);
        let port = spawn_server(state.clone()).await;

        let (status, _, _) = http_post_bearer(port, "/api/ops/stop", "not-json{{{", &token).await;
        assert_eq!(status, 400);
        let (status, _, _) =
            http_post_bearer(port, "/api/ops/stop", r#"{"confirm":"gw-test"}"#, &token).await;
        assert_eq!(status, 400, "missing actor must be 400");

        assert!(!state.daemon().shutdown_requested());
        assert_eq!(
            ops_runtime(&state)
                .audit_snapshot()
                .iter()
                .filter(|e| e.action == "stop" && e.outcome == OUTCOME_BAD_REQUEST)
                .count(),
            2,
            "both bad requests audited"
        );
    }

    /// QA: GET /api/ops/stop → 405（路径已注册、方法不匹配；守卫对未映射组合透传）。
    #[tokio::test]
    async fn stop_route_is_registered() {
        let state = make_state(Arc::new(AllowAllOpsAuthorizer), None);
        let port = spawn_server(state).await;
        let (status, _, _) = http_get(port, "/api/ops/stop").await;
        assert_eq!(status, 405, "POST-only route must be registered");
    }

    // ---- collectors ----

    /// QA 安全: ops 角色（不持 ops.collectors）→ 403 + 审计（即使能力未实现也先鉴权）。
    #[tokio::test]
    async fn collectors_denied_by_default_authorizer() {
        let state = make_state(Arc::new(DenyAllOpsAuthorizer), None);
        let token = token_for(&state, Role::Ops);
        let port = spawn_server(state.clone()).await;

        let (status, _, body) = http_post_bearer(
            port,
            "/api/ops/collectors",
            r#"{"actor":"ops-admin","action":"pause"}"#,
            &token,
        )
        .await;
        assert_eq!(status, 403, "{body}");
        let audit = ops_runtime(&state).audit_snapshot();
        assert!(audit
            .iter()
            .any(|e| e.action == "collectors_pause" && !e.allowed));
    }

    /// QA Error: 非法 action（非 pause/resume）→ 400 + 审计。
    #[tokio::test]
    async fn collectors_invalid_action_is_400() {
        let state = make_state(Arc::new(AllowAllOpsAuthorizer), None);
        let token = token_for(&state, Role::System);
        let port = spawn_server(state.clone()).await;

        let (status, _, body) = http_post_bearer(
            port,
            "/api/ops/collectors",
            r#"{"actor":"ops-admin","action":"nuke"}"#,
            &token,
        )
        .await;
        assert_eq!(status, 400, "{body}");
        assert!(ops_runtime(&state)
            .audit_snapshot()
            .iter()
            .any(|e| e.action == "collectors_pause" && e.outcome == OUTCOME_BAD_REQUEST));
    }

    /// QA: 授权通过 → 501 诚实降级（能力未实现）但审计已记（allowed=true）；
    /// pause 与 resume 同语义。
    #[tokio::test]
    async fn collectors_not_implemented_501_but_audited() {
        let state = make_state(Arc::new(AllowAllOpsAuthorizer), None);
        let token = token_for(&state, Role::System);
        let port = spawn_server(state.clone()).await;

        for action in ["pause", "resume"] {
            let body = format!(r#"{{"actor":"ops-admin","action":"{action}"}}"#);
            let (status, _, body) =
                http_post_bearer(port, "/api/ops/collectors", &body, &token).await;
            assert_eq!(status, 501, "{body}");
            let value: Value = serde_json::from_str(&body).expect("json");
            assert_eq!(value["error"], "not_implemented");
            assert!(value["planned"]
                .as_str()
                .expect("planned")
                .contains("task 37"));
            assert!(value["reason"].as_str().expect("reason").contains("调度器"));
        }

        let audit = ops_runtime(&state).audit_snapshot();
        assert!(audit.iter().any(|e| e.action == "collectors_pause"
            && e.allowed
            && e.outcome == OUTCOME_NOT_IMPLEMENTED));
        assert!(audit.iter().any(|e| e.action == "collectors_resume"
            && e.allowed
            && e.outcome == OUTCOME_NOT_IMPLEMENTED));
    }

    // ---- logs ----

    /// QA 安全: ops 角色（不持 ops.logs_read）→ 403 + 审计。
    #[tokio::test]
    async fn logs_denied_by_default_authorizer() {
        let state = make_state(Arc::new(DenyAllOpsAuthorizer), None);
        let token = token_for(&state, Role::Ops);
        let port = spawn_server(state.clone()).await;

        let (status, _, body) =
            http_get_bearer(port, "/api/ops/logs?actor=ops-admin", &token).await;
        assert_eq!(status, 403, "{body}");
        let audit = ops_runtime(&state).audit_snapshot();
        assert!(audit.iter().any(|e| e.action == "logs_read" && !e.allowed));
    }

    /// QA Error: 缺 actor / 坏 since / 坏 until / until<since → 均 400 + 审计。
    #[tokio::test]
    async fn logs_bad_params_are_400() {
        let state = make_state(Arc::new(AllowAllOpsAuthorizer), None);
        let token = token_for(&state, Role::System);
        let port = spawn_server(state.clone()).await;

        // 缺 actor。
        let (status, _, _) = http_get_bearer(port, "/api/ops/logs", &token).await;
        assert_eq!(status, 400, "missing actor must be 400");

        // since 非数字。
        let (status, _, _) = http_get_bearer(port, "/api/ops/logs?actor=a&since=abc", &token).await;
        assert_eq!(status, 400, "bad since must be 400");

        // until 非数字。
        let (status, _, _) = http_get_bearer(port, "/api/ops/logs?actor=a&until=-1", &token).await;
        assert_eq!(status, 400, "bad until must be 400");

        // until < since。
        let (status, _, _) =
            http_get_bearer(port, "/api/ops/logs?actor=a&since=100&until=50", &token).await;
        assert_eq!(status, 400, "until<since must be 400");

        // 未知 level。
        let (status, _, _) =
            http_get_bearer(port, "/api/ops/logs?actor=a&level=debug", &token).await;
        assert_eq!(status, 400, "unknown level must be 400");

        // 全部入审计（bad_request）。
        let audit = ops_runtime(&state).audit_snapshot();
        assert_eq!(
            audit
                .iter()
                .filter(|e| e.outcome == OUTCOME_BAD_REQUEST)
                .count(),
            5,
            "every rejected query must be audited"
        );
    }

    /// QA Happy: 时间范围过滤正确——受控时钟打点两条事件，
    /// since=15000 只回 T2 事件、until=15000 只回 T1 事件、全范围回两条。
    #[tokio::test]
    async fn logs_time_range_filter_is_correct() {
        let clock = TestClock::at(10_000_000_000); // T1 = 10000ms
        let state = make_state(Arc::new(AllowAllOpsAuthorizer), Some(clock.clone()));
        let token = token_for(&state, Role::System);
        let port = spawn_server(state.clone()).await;

        // T1 发布第一条；先等捕获落盘（时间戳 = 捕获时刻时钟）再推进时钟，
        // 保证 T1/T2 时间戳确定分离。
        state.publish(MgmtEvent::DeviceChanged {
            device_id: "dev-a".to_string(),
        });
        wait_for_log_len(&state, 1).await;
        clock.set(20_000_000_000); // T2 = 20000ms
        state.publish(MgmtEvent::DeviceChanged {
            device_id: "dev-b".to_string(),
        });
        wait_for_log_len(&state, 2).await;

        // 全范围（无 since/until）→ 两条。
        let (status, _, body) =
            http_get_bearer(port, "/api/ops/logs?actor=ops-admin", &token).await;
        assert_eq!(status, 200, "{body}");
        let rows: Vec<Value> = serde_json::from_str(&body).expect("array");
        assert_eq!(rows.len(), 2);

        // since=15000 → 只有 T2（dev-b）。
        let (status, _, body) =
            http_get_bearer(port, "/api/ops/logs?actor=ops-admin&since=15000", &token).await;
        assert_eq!(status, 200, "{body}");
        let rows: Vec<Value> = serde_json::from_str(&body).expect("array");
        assert_eq!(rows.len(), 1, "since filter: {rows:?}");
        assert_eq!(rows[0]["device_id"], "dev-b");
        assert_eq!(rows[0]["ts_ms"], "20000");

        // until=15000 → 只有 T1（dev-a）。
        let (status, _, body) =
            http_get_bearer(port, "/api/ops/logs?actor=ops-admin&until=15000", &token).await;
        assert_eq!(status, 200, "{body}");
        let rows: Vec<Value> = serde_json::from_str(&body).expect("array");
        assert_eq!(rows.len(), 1, "until filter: {rows:?}");
        assert_eq!(rows[0]["device_id"], "dev-a");

        // 闭区间端点命中：since=10000 → 两条都在。
        let (status, _, body) =
            http_get_bearer(port, "/api/ops/logs?actor=ops-admin&since=10000", &token).await;
        assert_eq!(status, 200, "{body}");
        let rows: Vec<Value> = serde_json::from_str(&body).expect("array");
        assert_eq!(rows.len(), 2, "since is inclusive");

        // 查询本身被审计。
        assert!(ops_runtime(&state)
            .audit_snapshot()
            .iter()
            .any(|e| e.action == "logs_read" && e.allowed && e.outcome == OUTCOME_ACCEPTED));
    }

    /// QA: level 按事件类型过滤（device 只回 device_changed，lifecycle 不混入）。
    #[tokio::test]
    async fn logs_level_filter_by_event_type() {
        let clock = TestClock::at(1_000_000_000);
        let state = make_state(Arc::new(AllowAllOpsAuthorizer), Some(clock.clone()));
        let token = token_for(&state, Role::System);
        let port = spawn_server(state.clone()).await;

        state.publish(MgmtEvent::LifecycleChanged {
            state: LifecycleState::Running,
        });
        state.publish(MgmtEvent::DeviceChanged {
            device_id: "dev-01".to_string(),
        });
        wait_for_log_len(&state, 2).await;

        // level=device → 只 device_changed。
        let (status, _, body) =
            http_get_bearer(port, "/api/ops/logs?actor=ops-admin&level=device", &token).await;
        assert_eq!(status, 200, "{body}");
        let rows: Vec<Value> = serde_json::from_str(&body).expect("array");
        assert_eq!(rows.len(), 1, "{rows:?}");
        assert_eq!(rows[0]["type"], "device_changed");
        assert_eq!(rows[0]["device_id"], "dev-01");

        // level=lifecycle → 只 lifecycle_changed。
        let (status, _, body) = http_get_bearer(
            port,
            "/api/ops/logs?actor=ops-admin&level=lifecycle",
            &token,
        )
        .await;
        assert_eq!(status, 200, "{body}");
        let rows: Vec<Value> = serde_json::from_str(&body).expect("array");
        assert_eq!(rows.len(), 1, "{rows:?}");
        assert_eq!(rows[0]["type"], "lifecycle_changed");
        assert_eq!(rows[0]["state"], "running");

        // 完整类型名同样接受。
        let (status, _, body) = http_get_bearer(
            port,
            "/api/ops/logs?actor=ops-admin&level=device_changed",
            &token,
        )
        .await;
        assert_eq!(status, 200, "{body}");
        let rows: Vec<Value> = serde_json::from_str(&body).expect("array");
        assert_eq!(rows.len(), 1);
    }

    /// QA 红线: 日志条目 seq / ts_ms 一律字符串编码（大数不丢精度）。
    #[tokio::test]
    async fn logs_bignumber_fields_are_strings() {
        // 注入 u64 量级纳秒时钟 → ts_ms = 1.7e15（超 JS 安全整数）。
        let clock = TestClock::at(1_700_000_000_000_000_000);
        let state = make_state(Arc::new(AllowAllOpsAuthorizer), Some(clock));
        let token = token_for(&state, Role::System);
        let port = spawn_server(state.clone()).await;

        state.publish(MgmtEvent::ConfigReloaded { version: u64::MAX });
        wait_for_log_len(&state, 1).await;

        let (status, _, body) =
            http_get_bearer(port, "/api/ops/logs?actor=ops-admin", &token).await;
        assert_eq!(status, 200, "{body}");
        let rows: Vec<Value> = serde_json::from_str(&body).expect("array");
        assert_eq!(rows.len(), 1, "{rows:?}");
        assert!(rows[0]["seq"].is_string(), "seq must be string: {rows:?}");
        assert!(
            rows[0]["ts_ms"].is_string(),
            "ts_ms must be string: {rows:?}"
        );
        assert!(
            rows[0]["version"].is_string(),
            "version must be string: {rows:?}"
        );
        assert_eq!(rows[0]["seq"], "1");
        assert_eq!(
            rows[0]["ts_ms"], "1700000000000",
            "1.7e18 ns / 1e6 = 1.7e12 ms"
        );
        assert_eq!(rows[0]["version"], u64::MAX.to_string());
    }

    // ---- 路由注册与审计完整性 ----

    /// QA: 三条 ops 路由均已注册（方法不匹配 → 405 而非 404——方法/路径组合
    /// 不在守卫动作映射内，透传给路由器产生 405）；
    /// 且正确方法在携带 token 时可达（非 404）。
    #[tokio::test]
    async fn ops_routes_are_registered() {
        let state = make_state(Arc::new(AllowAllOpsAuthorizer), None);
        let system = token_for(&state, Role::System);
        let ops = token_for(&state, Role::Ops);
        let port = spawn_server(state).await;

        // 方法不匹配 → 405（证明路径已注册；守卫对未映射组合透传）。
        let (status, _, _) = http_get(port, "/api/ops/restart").await;
        assert_eq!(status, 405, "GET /api/ops/restart must be 405");
        let (status, _, _) = http_get(port, "/api/ops/collectors").await;
        assert_eq!(status, 405, "GET /api/ops/collectors must be 405");
        let (status, _, _) = http_post(port, "/api/ops/logs", "").await;
        assert_eq!(status, 405, "POST /api/ops/logs must be 405");

        // 正确方法可达（非 404）：GET logs（system）→ 200；POST restart（ops，
        // 故意用会被 403 的角色，避免真实触发停机）→ 403。
        let (status, _, _) = http_get_bearer(port, "/api/ops/logs?actor=ops-admin", &system).await;
        assert_ne!(status, 404, "GET /api/ops/logs must be registered");
        let (status, _, _) = http_post_bearer(
            port,
            "/api/ops/restart",
            r#"{"actor":"a","confirm":"gw-test"}"#,
            &ops,
        )
        .await;
        assert_ne!(status, 404, "POST /api/ops/restart must be registered");
    }

    /// QA 安全: 审计记录完整性——被拒动作与放行动作都有记录，seq 单调、
    /// 时间戳非零、actor/action/outcome/reason 字段齐备。
    #[tokio::test]
    async fn audit_records_denied_and_accepted_completely() {
        let state = make_state(Arc::new(DenyAllOpsAuthorizer), None);
        let ops = token_for(&state, Role::Ops);
        let port = spawn_server(state.clone()).await;

        // 三个被拒动作（ops 角色：三条 ops 权限都不持 → 守卫层 403 + 审计）。
        let _ = http_post_bearer(
            port,
            "/api/ops/restart",
            r#"{"actor":"ops-admin","confirm":"gw-test"}"#,
            &ops,
        )
        .await;
        let _ = http_post_bearer(
            port,
            "/api/ops/collectors",
            r#"{"actor":"ops-admin","action":"pause"}"#,
            &ops,
        )
        .await;
        let _ = http_get_bearer(port, "/api/ops/logs?actor=ops-admin", &ops).await;

        let audit = ops_runtime(&state).audit_snapshot();
        assert_eq!(audit.len(), 3, "all three denied actions must be audited");
        for (idx, entry) in audit.iter().enumerate() {
            assert_eq!(entry.seq, (idx + 1) as u64, "audit seq monotonic");
            assert!(entry.ts_ns > 0, "ts_ns must be recorded");
            assert!(!entry.allowed, "all denied");
            assert_eq!(entry.outcome, OUTCOME_DENIED);
            assert!(!entry.reason.is_empty(), "reason must be present");
        }
        let actions: Vec<&str> = audit.iter().map(|e| e.action.as_str()).collect();
        assert_eq!(actions, vec!["restart", "collectors_pause", "logs_read"]);
        // 守卫层拒绝的 actor = JWT sub（token_for 统一取 "ops-admin"）。
        assert!(audit.iter().all(|e| e.actor == "ops-admin"));

        // JSON 编码大数红线（审计环虽无 HTTP 端点，编码契约同源）。
        let json = audit[0].to_json();
        assert!(json["seq"].is_string());
        assert!(json["ts_ns"].is_string());
    }
}

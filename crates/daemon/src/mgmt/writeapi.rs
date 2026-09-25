//! 管理面配置写接口（task：web-console real 模式设备/点位写真正落盘）。
//!
//! ## 端点（全部要求 `Authorization: Bearer` JWT；RBAC 权限见 rbac.rs
//! `Permission::DeviceWrite` / `PointWrite`——高危配置动作，仅 `system` 可授）
//! - `POST   /api/devices`                            设备登记（`[[devices]]` 段）
//! - `PUT    /api/devices/:id`                        改名 / 启停 / 默认协议（upsert 登记行）
//! - `DELETE /api/devices/:id?cascade=true|false`     删除（有点位时默认 400，`cascade=true` 连同点位级联删除）
//! - `POST   /api/points`                             新增点位行
//! - `PUT    /api/points/:device_id/:point_id`        修改协议 / 地址 / 频率
//! - `DELETE /api/points/:device_id/:point_id`        删除点位行
//!
//! ## 写路径语义（顺序即契约）
//! 1. **鉴权**：`AuthedRole` extractor（401：身份未建立，不留审计痕）→
//!    `ensure(perm)`（403：权限不足，**入审计环**，actor = JWT sub）；
//! 2. **全进程串行**：所有写操作经 static Mutex（读-改-写全程持锁），
//!    防并发写撕裂 / 丢更新；
//! 3. **校验**：协议 ∈ 既有协议域；modbus / s7 / mc 地址走
//!    `driver::PointAddressParser`（只调用不修改）；opcua / http / mqtt 地址为
//!    endpoint/URL 语义，仅非空校验（诚实限制：无对应结构化解析器）；
//!    `frequency_ms` ≥ 100ms（计划指标）；
//! 4. **落盘**：基于当前热快照克隆变更 → `GatewayConfig::save`（先
//!    `backup_before_rewrite` 写前原子备份，再临时文件 + fsync + rename 原子落盘）；
//! 5. **生效**：`ConfigShared::replace` 即时推送新快照（读侧立即可见，无需等
//!    notify 防抖窗口）；notify watch 对磁盘的重读为同内容幂等重放（无害）；
//!    发布 `DeviceChanged` 管理事件（SSE 即时扇出 + 历史环回放）；
//! 6. **审计**：每个写动作（含被拒 / 参数非法 / 落盘失败）入 ops 审计环
//!    （actor = JWT sub，动作 / 对象 / 结果齐备）。
//!
//! ## 错误口径（对齐点位批量导入「行号 + 原因 + 允许值」）
//! 400 校验失败统一 `{error: "validation_failed", field, reason, allowed}`；
//! 设备删除含点位未级联 → `{error: "device_has_points", ..., hint}`；
//! 404 `{error: "not_found", message}`；401/403 由 rbac 层统一编码；
//! 500 `{error: "internal", message}`（落盘失败，fail-closed 不半写）。
//!
//! ## 大数红线
//! `config_version`（u64）一律字符串编码；`frequency_ms` 请求体接受
//! number 或 string（string 为推荐形态，与读接口回显一致）。
//!
//! ## 诚实限制
//! - `enabled` 仅登记层语义（设备摘要 `enabled` 字段与展示）；运行期采集启停
//!   需调度器拓扑开关（task 37），本接口不做运行期暂停；
//! - `config_path` 未装配（`with_config_path` 未调用）时写接口 fail-closed
//!   返回 500 并审计（绝不写未知文件）。

use std::collections::HashMap;
use std::sync::{Mutex, MutexGuard};

use axum::body::Bytes;
use axum::extract::{Path as AxumPath, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Json, Response};
use serde::Deserialize;
use serde_json::{json, Value};

use super::rbac::{AuthedRole, Permission};
use super::remote_ops::{self, OpsAction, OUTCOME_ACCEPTED, OUTCOME_BAD_REQUEST, OUTCOME_DENIED};
use super::{MgmtEvent, MgmtState};
use crate::config::{DeviceConfig, PointConfig};
use crate::driver::PointAddressParser;

/// 配置写全局互斥：**全进程串行**（读-改-写全程持锁），防并发写导致
/// TOML 撕裂或丢失更新；锁中毒时 `into_inner` 恢复（零 panic）。
static CONFIG_WRITE_LOCK: Mutex<()> = Mutex::new(());

/// 取写锁（毒锁恢复语义，对齐项目「锁中毒不 panic」纪律）。
/// `pub(crate)`：页面级写端点（mgmt::pages 的导入 / 回滚）共用同一全进程写锁，
/// 防跨模块并发写撕裂。
pub(crate) fn write_guard() -> MutexGuard<'static, ()> {
    CONFIG_WRITE_LOCK.lock().unwrap_or_else(|p| p.into_inner())
}

/// 协议取值域（与 config.rs `PointConfig::protocol` 注释一致）。
/// `pub(crate)`：点位批量导入（mgmt::pages）复用同一协议域，防两套取值漂移。
pub(crate) const PROTOCOLS: &[&str] = &[
    "modbus-tcp",
    "modbus-rtu",
    "opcua",
    "s7",
    "mc",
    "http",
    "mqtt",
];

/// 地址需过结构化解析器校验的协议；其余（opcua / http / mqtt）地址为
/// endpoint / URL 语义，仅做非空校验（诚实限制，见模块注释）。
const PARSER_PROTOCOLS: &[&str] = &["modbus-tcp", "modbus-rtu", "s7", "mc"];

/// 计划指标：采集频率下限（毫秒）。`pub(crate)`：批量导入共用同一计划指标。
pub(crate) const MIN_FREQUENCY_MS: u64 = 100;
/// 新增点位的缺省采集频率（毫秒；与 config.rs `default_frequency_ms` 一致）。
pub(crate) const DEFAULT_FREQUENCY_MS: u64 = 1000;

/// 落盘失败（审计结果字面量；accepted / denied / bad_request 之外的失败路径）。
pub(crate) const OUTCOME_FAILED: &str = "failed";

// ---- 审计 ----

/// 记审计（actor = JWT sub；被拒 / 非法 / 失败 / 成功全记）。
///
/// task 26：内存环（remote_ops）之外同步落**持久安全审计**（防篡改哈希链）——
/// 放行的配置写 = `config_change`，被拒 = `authz_failed`；持久写失败仅告警
/// 不阻塞主流程（内存环仍是兜底轨迹）。
pub(crate) fn audit(
    state: &MgmtState,
    actor: &str,
    action: OpsAction,
    allowed: bool,
    outcome: &str,
    reason: &str,
) {
    remote_ops::runtime_for(state).record_audit(actor, action, allowed, outcome, reason);
    if let Some(logger) = state.daemon().audit_logger() {
        let event = if allowed {
            crate::audit::AuditEventType::ConfigChange
        } else {
            crate::audit::AuditEventType::AuthzFailed
        };
        if let Err(err) = logger.record(
            actor,
            event,
            outcome,
            &format!("{}: {reason}", action.as_str()),
        ) {
            tracing::warn!(error = %err, "writeapi: persistent audit record failed");
        }
    }
}

// ---- 响应辅助 ----

/// 400 + 「字段 + 原因 + 允许值」错误体（对齐点位批量导入错误口径）。
pub(crate) fn validation_error(field: &str, reason: &str, allowed: &str) -> Response {
    (
        StatusCode::BAD_REQUEST,
        Json(json!({
            "error": "validation_failed",
            "field": field,
            "reason": reason,
            "allowed": allowed,
        })),
    )
        .into_response()
}

/// 404 + JSON 错误体。
pub(crate) fn not_found(message: &str) -> Response {
    (
        StatusCode::NOT_FOUND,
        Json(json!({ "error": "not_found", "message": message })),
    )
        .into_response()
}

/// 500 + JSON 错误体（落盘失败等内部错误；message 只含原因，不泄露路径细节以外内容）。
pub(crate) fn internal(message: &str) -> Response {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(json!({ "error": "internal", "message": message })),
    )
        .into_response()
}

/// 200 受理响应（`config_version` 字符串编码——大数红线）。
fn accepted(device_id: &str, point_id: Option<&str>, version: u64) -> Response {
    let mut body = json!({
        "accepted": true,
        "device_id": device_id,
        "config_version": version.to_string(),
    });
    if let Some(pid) = point_id {
        body["point_id"] = json!(pid);
    }
    Json(body).into_response()
}

// ---- 校验 ----

/// 协议取值域校验。
#[allow(clippy::result_large_err)]
fn validate_protocol(protocol: &str) -> Result<(), Response> {
    if PROTOCOLS.contains(&protocol) {
        Ok(())
    } else {
        Err(validation_error(
            "protocol",
            &format!("unknown protocol {protocol:?}"),
            &PROTOCOLS.join(" | "),
        ))
    }
}

/// 地址合法性：modbus / s7 / mc 走统一解析器（复用 driver，只调用不修改）；
/// 其余协议仅非空校验（诚实限制）。
#[allow(clippy::result_large_err)]
fn validate_address(protocol: &str, address: &str) -> Result<(), Response> {
    if address.trim().is_empty() {
        return Err(validation_error(
            "address",
            "address must not be empty",
            "non-empty endpoint or point address",
        ));
    }
    if PARSER_PROTOCOLS.contains(&protocol) {
        if let Err(err) = PointAddressParser::parse(address) {
            return Err(validation_error(
                "address",
                &err.to_string(),
                &format!(
                    "{protocol} address syntax accepted by driver::PointAddressParser \
                     (e.g. modbus \"40001\", s7 \"DB1.DBX0.0\", mc \"D100\")"
                ),
            ));
        }
    }
    Ok(())
}

/// `frequency_ms` 校验：接受 JSON number 或 string（大数红线），解析 + 下限
/// （计划指标 ≥100ms）。缺省 = 1000ms。
#[allow(clippy::result_large_err)]
fn validate_frequency(raw: Option<&Value>) -> Result<u64, Response> {
    let Some(raw) = raw else {
        return Ok(DEFAULT_FREQUENCY_MS);
    };
    let parse_err = |detail: String| {
        validation_error(
            "frequency_ms",
            &detail,
            &format!("integer >= {MIN_FREQUENCY_MS} (ms); number or string"),
        )
    };
    let ms = match raw {
        Value::Number(n) => match n.as_u64() {
            Some(v) => v,
            None => return Err(parse_err(format!("not a valid u64: {raw}"))),
        },
        Value::String(s) => match s.trim().parse::<u64>() {
            Ok(v) => v,
            Err(_) => return Err(parse_err(format!("not a valid u64: {s:?}"))),
        },
        other => return Err(parse_err(format!("must be number or string, got {other}"))),
    };
    if ms < MIN_FREQUENCY_MS {
        return Err(validation_error(
            "frequency_ms",
            &format!("{ms}ms is below the minimum collection frequency"),
            &format!(">= {MIN_FREQUENCY_MS} (ms)"),
        ));
    }
    Ok(ms)
}

// ---- 请求体 ----

/// `POST /api/devices` 请求体（字段缺失按缺省校验，避免 serde 硬失败分叉）。
#[derive(Debug, Default, Deserialize)]
struct DeviceCreateBody {
    #[serde(default)]
    id: String,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    protocol: Option<String>,
    #[serde(default)]
    enabled: Option<bool>,
}

/// `PUT /api/devices/:id` 请求体（全部可选；只更新出现的字段）。
#[derive(Debug, Default, Deserialize)]
struct DeviceUpdateBody {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    protocol: Option<String>,
    #[serde(default)]
    enabled: Option<bool>,
}

/// `POST /api/points` 请求体。
#[derive(Debug, Default, Deserialize)]
struct PointCreateBody {
    #[serde(default)]
    device_id: String,
    #[serde(default)]
    point_id: String,
    #[serde(default)]
    protocol: String,
    #[serde(default)]
    address: String,
    #[serde(default)]
    frequency_ms: Option<Value>,
}

/// `PUT /api/points/:device_id/:point_id` 请求体（身份字段取自路径，body 只更新
/// 协议 / 地址 / 频率；不支持移动点位归属）。
#[derive(Debug, Default, Deserialize)]
struct PointUpdateBody {
    #[serde(default)]
    protocol: Option<String>,
    #[serde(default)]
    address: Option<String>,
    #[serde(default)]
    frequency_ms: Option<Value>,
}

// ---- 配置辅助 ----

/// 设备是否存在（登记段或点位聚合任一命中）。
pub(crate) fn device_exists(config: &crate::config::GatewayConfig, device_id: &str) -> bool {
    config.devices.iter().any(|d| d.device_id == device_id)
        || config.points.iter().any(|p| p.device_id == device_id)
}

/// 设备下某点位的行下标。
fn point_index(
    config: &crate::config::GatewayConfig,
    device_id: &str,
    point_id: &str,
) -> Option<usize> {
    config
        .points
        .iter()
        .position(|p| p.device_id == device_id && p.point_id == point_id)
}

/// 规整可选文本字段：trim 后非空才取值。
fn trimmed_or_none(raw: &Option<String>) -> Option<String> {
    raw.as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(ToString::to_string)
}

/// 构造点位行（调用方须先完成协议 / 地址 / 频率校验）。
fn make_point_row(
    device_id: &str,
    point_id: &str,
    protocol: &str,
    address: &str,
    frequency_ms: u64,
) -> PointConfig {
    PointConfig {
        device_id: device_id.to_string(),
        point_id: point_id.to_string(),
        protocol: protocol.to_string(),
        address: address.to_string(),
        frequency_ms,
    }
}

/// 落盘 + 即时推送 + 管理事件 + 审计（调用方必须已持写锁并完成全部校验）。
fn persist(
    state: &MgmtState,
    new_config: crate::config::GatewayConfig,
    actor: &str,
    action: OpsAction,
    detail: &str,
    device_id: &str,
    point_id: Option<&str>,
) -> Response {
    // 授权配额闸门（免费版 fail-closed，可解释）：Degraded 期间设备数 / 协议 /
    // 采集间隔超限的管理面写操作一律拒绝（400），与启动装配期 / 热重载同口径。
    if let Some(license) = state.daemon().license_runtime() {
        if let Err(err) = license.enforce_free_limits(&new_config) {
            audit(
                state,
                actor,
                action,
                false,
                OUTCOME_BAD_REQUEST,
                &format!("{detail}: rejected by free-edition quota gate: {err}"),
            );
            return validation_error(
                "config",
                &format!("free-edition quota rejected: {err}"),
                "free edition: ≤8 devices, modbus-tcp/modbus-rtu only, \
                 frequency_ms ≥ 1000 (activate a license to lift the limits)",
            );
        }
    }
    let Some(path) = state.config_path() else {
        audit(
            state,
            actor,
            action,
            true,
            OUTCOME_FAILED,
            "config file path not configured (with_config_path wiring missing); write refused",
        );
        return internal("config file path not configured; write refused (fail-closed)");
    };
    if let Err(err) = new_config.save(&path) {
        audit(
            state,
            actor,
            action,
            true,
            OUTCOME_FAILED,
            &format!("{detail}: save failed: {err}"),
        );
        return internal(&format!("config save failed: {err}"));
    }
    // 即时推送新快照（读侧立即可见）；notify watch 随后的磁盘重读为幂等重放。
    let shared = state.daemon().config_shared();
    let version = shared.replace(new_config);
    state.publish(MgmtEvent::DeviceChanged {
        device_id: device_id.to_string(),
    });
    audit(
        state,
        actor,
        action,
        true,
        OUTCOME_ACCEPTED,
        &format!("{detail}; config_version={version}"),
    );
    accepted(device_id, point_id, version)
}

// ---- 设备写处理器 ----

/// `POST /api/devices` → 设备登记（写入可选 `[[devices]]` 段）。
///
/// body：`{id, name?, protocol, enabled?}`（`protocol` 必填——登记设备的展示
/// 协议；`name` 缺省回退 `id`；`enabled` 缺省 true）。重复 id（登记段或点位
/// 聚合任一命中）→ 400。
pub async fn device_create(
    State(state): State<MgmtState>,
    authed: AuthedRole,
    body: Bytes,
) -> Response {
    if let Err(rejection) = authed.ensure(Permission::DeviceWrite) {
        audit(
            &state,
            &authed.claims.sub,
            OpsAction::DeviceCreate,
            false,
            OUTCOME_DENIED,
            &rejection.to_string(),
        );
        return rejection.into_response();
    }
    let actor = authed.claims.sub.clone();
    let req: DeviceCreateBody = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(err) => {
            audit(
                &state,
                &actor,
                OpsAction::DeviceCreate,
                true,
                OUTCOME_BAD_REQUEST,
                &format!("malformed json body: {err}"),
            );
            return validation_error(
                "body",
                &format!("malformed JSON: {err}"),
                "object {id, name?, protocol, enabled?}",
            );
        }
    };
    let id = req.id.trim().to_string();
    if id.is_empty() {
        audit(
            &state,
            &actor,
            OpsAction::DeviceCreate,
            true,
            OUTCOME_BAD_REQUEST,
            "device id is required",
        );
        return validation_error(
            "id",
            "device id is required",
            "non-empty unique device identifier",
        );
    }
    let Some(protocol) = trimmed_or_none(&req.protocol) else {
        audit(
            &state,
            &actor,
            OpsAction::DeviceCreate,
            true,
            OUTCOME_BAD_REQUEST,
            "protocol is required for device registration",
        );
        return validation_error(
            "protocol",
            "protocol is required for device registration",
            &PROTOCOLS.join(" | "),
        );
    };
    if let Err(resp) = validate_protocol(&protocol) {
        audit(
            &state,
            &actor,
            OpsAction::DeviceCreate,
            true,
            OUTCOME_BAD_REQUEST,
            &format!("invalid protocol {protocol:?}"),
        );
        return resp;
    }

    let _guard = write_guard();
    let mut config = (*state.config()).clone();
    if device_exists(&config, &id) {
        audit(
            &state,
            &actor,
            OpsAction::DeviceCreate,
            true,
            OUTCOME_BAD_REQUEST,
            &format!("duplicate device id {id:?}"),
        );
        return validation_error(
            "id",
            &format!("device {id:?} already exists"),
            "unique device id",
        );
    }
    let name = trimmed_or_none(&req.name).unwrap_or_else(|| id.clone());
    config.devices.push(DeviceConfig {
        device_id: id.clone(),
        name: Some(name),
        enabled: req.enabled.unwrap_or(true),
        protocol: Some(protocol.clone()),
    });
    persist(
        &state,
        config,
        &actor,
        OpsAction::DeviceCreate,
        &format!("create device {id:?} (protocol={protocol})"),
        &id,
        None,
    )
}

/// `PUT /api/devices/:id` → 改名 / 启停 / 默认协议。
///
/// upsert 语义：点位聚合出的设备首次改名 / 启停时落一行登记
/// （见 config.rs `DeviceConfig` 取舍说明）。设备不存在 → 404。
pub async fn device_update(
    State(state): State<MgmtState>,
    AxumPath(device_id): AxumPath<String>,
    authed: AuthedRole,
    body: Bytes,
) -> Response {
    if let Err(rejection) = authed.ensure(Permission::DeviceWrite) {
        audit(
            &state,
            &authed.claims.sub,
            OpsAction::DeviceUpdate,
            false,
            OUTCOME_DENIED,
            &rejection.to_string(),
        );
        return rejection.into_response();
    }
    let actor = authed.claims.sub.clone();
    let req: DeviceUpdateBody = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(err) => {
            audit(
                &state,
                &actor,
                OpsAction::DeviceUpdate,
                true,
                OUTCOME_BAD_REQUEST,
                &format!("malformed json body: {err}"),
            );
            return validation_error(
                "body",
                &format!("malformed JSON: {err}"),
                "object {name?, protocol?, enabled?}",
            );
        }
    };
    let device_id = device_id.trim().to_string();
    let _guard = write_guard();
    let mut config = (*state.config()).clone();
    if !device_exists(&config, &device_id) {
        audit(
            &state,
            &actor,
            OpsAction::DeviceUpdate,
            true,
            OUTCOME_BAD_REQUEST,
            &format!("unknown device {device_id:?}"),
        );
        return not_found(&format!("device {device_id:?} not found"));
    }
    if let Some(protocol) = trimmed_or_none(&req.protocol) {
        if let Err(resp) = validate_protocol(&protocol) {
            audit(
                &state,
                &actor,
                OpsAction::DeviceUpdate,
                true,
                OUTCOME_BAD_REQUEST,
                &format!("invalid protocol {protocol:?}"),
            );
            return resp;
        }
    }
    // upsert 登记行。
    match config.devices.iter_mut().find(|d| d.device_id == device_id) {
        Some(entry) => {
            if let Some(name) = trimmed_or_none(&req.name) {
                entry.name = Some(name);
            }
            if let Some(enabled) = req.enabled {
                entry.enabled = enabled;
            }
            if let Some(protocol) = trimmed_or_none(&req.protocol) {
                entry.protocol = Some(protocol);
            }
        }
        None => {
            config.devices.push(DeviceConfig {
                device_id: device_id.clone(),
                name: trimmed_or_none(&req.name),
                enabled: req.enabled.unwrap_or(true),
                protocol: trimmed_or_none(&req.protocol),
            });
        }
    }
    persist(
        &state,
        config,
        &actor,
        OpsAction::DeviceUpdate,
        &format!("update device {device_id:?}"),
        &device_id,
        None,
    )
}

/// `DELETE /api/devices/:id?cascade=true|false` → 删除设备。
///
/// **级联语义（显式 opt-in）**：设备仍有点位时默认 400（`device_has_points`，
/// fail-closed 防误删）；`cascade=true` 时连同该设备全部点位行一并删除（与
/// web-console mock 语义一致）。仅登记设备（无点位）直接删除。
pub async fn device_delete(
    State(state): State<MgmtState>,
    AxumPath(device_id): AxumPath<String>,
    authed: AuthedRole,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    if let Err(rejection) = authed.ensure(Permission::DeviceWrite) {
        audit(
            &state,
            &authed.claims.sub,
            OpsAction::DeviceDelete,
            false,
            OUTCOME_DENIED,
            &rejection.to_string(),
        );
        return rejection.into_response();
    }
    let actor = authed.claims.sub.clone();
    let device_id = device_id.trim().to_string();
    let cascade = params.get("cascade").map(|v| v == "true").unwrap_or(false);
    let _guard = write_guard();
    let mut config = (*state.config()).clone();
    if !device_exists(&config, &device_id) {
        audit(
            &state,
            &actor,
            OpsAction::DeviceDelete,
            true,
            OUTCOME_BAD_REQUEST,
            &format!("unknown device {device_id:?}"),
        );
        return not_found(&format!("device {device_id:?} not found"));
    }
    let point_rows = config
        .points
        .iter()
        .filter(|p| p.device_id == device_id)
        .count();
    if point_rows > 0 && !cascade {
        audit(
            &state,
            &actor,
            OpsAction::DeviceDelete,
            true,
            OUTCOME_BAD_REQUEST,
            &format!("device {device_id:?} still has {point_rows} point(s); cascade not confirmed"),
        );
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({
                "error": "device_has_points",
                "message": format!(
                    "device {device_id:?} still has {point_rows} point(s); deletion refused (fail-closed)"
                ),
                "field": "cascade",
                "allowed": "true | false (default false)",
                "hint": "pass ?cascade=true to delete the device together with all its points",
            })),
        )
            .into_response();
    }
    config.points.retain(|p| p.device_id != device_id);
    config.devices.retain(|d| d.device_id != device_id);
    persist(
        &state,
        config,
        &actor,
        OpsAction::DeviceDelete,
        &format!("delete device {device_id:?} (cascade={cascade}, removed {point_rows} point(s))"),
        &device_id,
        None,
    )
}

// ---- 点位写处理器 ----

/// `POST /api/points` → 新增点位行。
///
/// body：`{device_id, point_id, protocol, address, frequency_ms?}`。
/// 设备必须已存在（登记段或点位聚合）；`(device_id, point_id)` 唯一；
/// 地址按协议过结构化解析器。
pub async fn point_create(
    State(state): State<MgmtState>,
    authed: AuthedRole,
    body: Bytes,
) -> Response {
    if let Err(rejection) = authed.ensure(Permission::PointWrite) {
        audit(
            &state,
            &authed.claims.sub,
            OpsAction::PointCreate,
            false,
            OUTCOME_DENIED,
            &rejection.to_string(),
        );
        return rejection.into_response();
    }
    let actor = authed.claims.sub.clone();
    let req: PointCreateBody = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(err) => {
            audit(
                &state,
                &actor,
                OpsAction::PointCreate,
                true,
                OUTCOME_BAD_REQUEST,
                &format!("malformed json body: {err}"),
            );
            return validation_error(
                "body",
                &format!("malformed JSON: {err}"),
                "object {device_id, point_id, protocol, address, frequency_ms?}",
            );
        }
    };
    let device_id = req.device_id.trim().to_string();
    let point_id = req.point_id.trim().to_string();
    if device_id.is_empty() {
        audit(
            &state,
            &actor,
            OpsAction::PointCreate,
            true,
            OUTCOME_BAD_REQUEST,
            "device_id is required",
        );
        return validation_error(
            "device_id",
            "device_id is required",
            "identifier of an existing device",
        );
    }
    if point_id.is_empty() {
        audit(
            &state,
            &actor,
            OpsAction::PointCreate,
            true,
            OUTCOME_BAD_REQUEST,
            "point_id is required",
        );
        return validation_error(
            "point_id",
            "point_id is required",
            "non-empty unique point identifier within the device",
        );
    }
    let _guard = write_guard();
    let mut config = (*state.config()).clone();
    if !device_exists(&config, &device_id) {
        audit(
            &state,
            &actor,
            OpsAction::PointCreate,
            true,
            OUTCOME_BAD_REQUEST,
            &format!("unknown device {device_id:?}"),
        );
        return not_found(&format!("device {device_id:?} not found"));
    }
    if let Err(resp) = validate_protocol(&req.protocol) {
        audit(
            &state,
            &actor,
            OpsAction::PointCreate,
            true,
            OUTCOME_BAD_REQUEST,
            &format!("invalid protocol {:?}", req.protocol),
        );
        return resp;
    }
    if let Err(resp) = validate_address(&req.protocol, &req.address) {
        audit(
            &state,
            &actor,
            OpsAction::PointCreate,
            true,
            OUTCOME_BAD_REQUEST,
            &format!("invalid address {:?}", req.address),
        );
        return resp;
    }
    let frequency_ms = match validate_frequency(req.frequency_ms.as_ref()) {
        Ok(v) => v,
        Err(resp) => {
            audit(
                &state,
                &actor,
                OpsAction::PointCreate,
                true,
                OUTCOME_BAD_REQUEST,
                "invalid frequency_ms",
            );
            return resp;
        }
    };
    if point_index(&config, &device_id, &point_id).is_some() {
        audit(
            &state,
            &actor,
            OpsAction::PointCreate,
            true,
            OUTCOME_BAD_REQUEST,
            &format!("duplicate point {device_id:?}/{point_id:?}"),
        );
        return validation_error(
            "point_id",
            &format!("point {point_id:?} already exists on device {device_id:?}"),
            "unique point id within the device",
        );
    }
    config.points.push(make_point_row(
        &device_id,
        &point_id,
        &req.protocol,
        req.address.trim(),
        frequency_ms,
    ));
    persist(
        &state,
        config,
        &actor,
        OpsAction::PointCreate,
        &format!(
            "create point {device_id:?}/{point_id:?} (protocol={}, address={:?}, frequency_ms={frequency_ms})",
            req.protocol, req.address
        ),
        &device_id,
        Some(&point_id),
    )
}

/// `PUT /api/points/:device_id/:point_id` → 修改协议 / 地址 / 频率
/// （身份字段取自路径；点位不存在 → 404）。
pub async fn point_update(
    State(state): State<MgmtState>,
    AxumPath((device_id, point_id)): AxumPath<(String, String)>,
    authed: AuthedRole,
    body: Bytes,
) -> Response {
    if let Err(rejection) = authed.ensure(Permission::PointWrite) {
        audit(
            &state,
            &authed.claims.sub,
            OpsAction::PointUpdate,
            false,
            OUTCOME_DENIED,
            &rejection.to_string(),
        );
        return rejection.into_response();
    }
    let actor = authed.claims.sub.clone();
    let req: PointUpdateBody = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(err) => {
            audit(
                &state,
                &actor,
                OpsAction::PointUpdate,
                true,
                OUTCOME_BAD_REQUEST,
                &format!("malformed json body: {err}"),
            );
            return validation_error(
                "body",
                &format!("malformed JSON: {err}"),
                "object {protocol?, address?, frequency_ms?}",
            );
        }
    };
    let device_id = device_id.trim().to_string();
    let point_id = point_id.trim().to_string();
    let _guard = write_guard();
    let mut config = (*state.config()).clone();
    let Some(idx) = point_index(&config, &device_id, &point_id) else {
        audit(
            &state,
            &actor,
            OpsAction::PointUpdate,
            true,
            OUTCOME_BAD_REQUEST,
            &format!("unknown point {device_id:?}/{point_id:?}"),
        );
        return not_found(&format!(
            "point {point_id:?} not found on device {device_id:?}"
        ));
    };
    let (cur_protocol, cur_address) = (
        config.points[idx].protocol.clone(),
        config.points[idx].address.clone(),
    );
    let protocol = trimmed_or_none(&req.protocol).unwrap_or(cur_protocol);
    if let Err(resp) = validate_protocol(&protocol) {
        audit(
            &state,
            &actor,
            OpsAction::PointUpdate,
            true,
            OUTCOME_BAD_REQUEST,
            &format!("invalid protocol {protocol:?}"),
        );
        return resp;
    }
    let address = trimmed_or_none(&req.address).unwrap_or(cur_address);
    if let Err(resp) = validate_address(&protocol, &address) {
        audit(
            &state,
            &actor,
            OpsAction::PointUpdate,
            true,
            OUTCOME_BAD_REQUEST,
            &format!("invalid address {address:?}"),
        );
        return resp;
    }
    let frequency_ms = match validate_frequency(req.frequency_ms.as_ref()) {
        Ok(v) => v,
        Err(resp) => {
            audit(
                &state,
                &actor,
                OpsAction::PointUpdate,
                true,
                OUTCOME_BAD_REQUEST,
                "invalid frequency_ms",
            );
            return resp;
        }
    };
    let row = &mut config.points[idx];
    row.protocol = protocol.clone();
    row.address = address.clone();
    row.frequency_ms = frequency_ms;
    persist(
        &state,
        config,
        &actor,
        OpsAction::PointUpdate,
        &format!(
            "update point {device_id:?}/{point_id:?} (protocol={protocol}, address={address:?}, frequency_ms={frequency_ms})"
        ),
        &device_id,
        Some(&point_id),
    )
}

/// `DELETE /api/points/:device_id/:point_id` → 删除点位行（不存在 → 404）。
pub async fn point_delete(
    State(state): State<MgmtState>,
    AxumPath((device_id, point_id)): AxumPath<(String, String)>,
    authed: AuthedRole,
) -> Response {
    if let Err(rejection) = authed.ensure(Permission::PointWrite) {
        audit(
            &state,
            &authed.claims.sub,
            OpsAction::PointDelete,
            false,
            OUTCOME_DENIED,
            &rejection.to_string(),
        );
        return rejection.into_response();
    }
    let actor = authed.claims.sub.clone();
    let device_id = device_id.trim().to_string();
    let point_id = point_id.trim().to_string();
    let _guard = write_guard();
    let mut config = (*state.config()).clone();
    let Some(idx) = point_index(&config, &device_id, &point_id) else {
        audit(
            &state,
            &actor,
            OpsAction::PointDelete,
            true,
            OUTCOME_BAD_REQUEST,
            &format!("unknown point {device_id:?}/{point_id:?}"),
        );
        return not_found(&format!(
            "point {point_id:?} not found on device {device_id:?}"
        ));
    };
    config.points.remove(idx);
    persist(
        &state,
        config,
        &actor,
        OpsAction::PointDelete,
        &format!("delete point {device_id:?}/{point_id:?}"),
        &device_id,
        Some(&point_id),
    )
}

// ---- 测试 ----

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audit::AuditQuery;
    use crate::bootstrap::DaemonShared;
    use crate::config::{ConfigShared, GatewayConfig};
    use crate::mgmt::auth_jwt::{now_unix_secs, sign, Claims};
    use crate::mgmt::rbac::Role;
    use crate::mgmt::remote_ops::runtime_for;
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::time::Duration;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::{TcpListener, TcpStream};

    /// 测试种子配置：1 个登记设备（无点位）+ 1 个点位设备。
    const SEED_TOML: &str = r#"
[gateway]
gateway_id = "gw-test"

[[devices]]
device_id = "dev-empty"
name = "空设备"
protocol = "opcua"

[[points]]
device_id = "dev-01"
point_id = "p_temp"
protocol = "modbus-tcp"
address = "192.168.1.10:502"
frequency_ms = 100
"#;

    /// 构造绑定临时配置文件 + 指定角色的 MgmtState（独立实例）。
    ///
    /// 显式 `install` 一个全新 ops runtime：ops runtime 侧表按实例指针地址
    /// 注册，测试并行时可能发生地址复用（ABA），残留旧审计环会污染本用例
    /// 断言；install 以全新空环覆盖（DenyAll 兜底策略不参与 JWT/RBAC 判定链，
    /// 行为不变，见 remote_ops 模块注释）。
    fn make_state(dir: &tempfile::TempDir) -> (MgmtState, PathBuf) {
        let path = dir.path().join("config.toml");
        std::fs::write(&path, SEED_TOML).expect("seed config");
        let config = Arc::new(GatewayConfig::load(&path).expect("load"));
        let daemon = DaemonShared::new();
        daemon.set_config(Arc::new(ConfigShared::new((*config).clone())));
        let state = MgmtState::new(daemon, config).with_config_path(&path);
        remote_ops::install(&state, Arc::new(remote_ops::DenyAllOpsAuthorizer));
        (state, path)
    }

    /// 以 state 的实际签名密钥签发测试 token（对齐 remote_ops 测试装配）。
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

    /// 手写 HTTP 请求（GET/POST/PUT/DELETE + Bearer + body），3s 超时防挂死
    /// （tower 非 daemon 直接依赖，无法 oneshot；对齐 mgmt 既有测试口径）。
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
            let (request, method) = if method == "GET" {
                (
                    format!("GET {path} HTTP/1.1\r\nHost: 127.0.0.1\r\n{auth}Connection: close\r\n\r\n"),
                    "GET",
                )
            } else {
                (
                    format!(
                        "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1\r\n{auth}Content-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    ),
                    method,
                )
            };
            let _ = method;
            stream.write_all(request.as_bytes()).await.expect("write");
            stream.flush().await.expect("flush");
            let mut buf = Vec::new();
            stream.read_to_end(&mut buf).await.expect("read");
            parse_response(&String::from_utf8(buf).expect("utf8"))
        })
        .await
        .expect("http_request timed out")
    }

    async fn http_post_bearer(
        port: u16,
        path: &str,
        body: &str,
        token: &str,
    ) -> (u16, String, String) {
        http_request(port, "POST", path, Some(body), Some(token)).await
    }

    async fn http_put_bearer(
        port: u16,
        path: &str,
        body: &str,
        token: &str,
    ) -> (u16, String, String) {
        http_request(port, "PUT", path, Some(body), Some(token)).await
    }

    async fn http_delete_bearer(port: u16, path: &str, token: &str) -> (u16, String, String) {
        http_request(port, "DELETE", path, None, Some(token)).await
    }

    async fn http_get(port: u16, path: &str) -> (u16, String, String) {
        http_request(port, "GET", path, None, None).await
    }

    /// 从落盘文件重读配置（往返断言用）。
    fn load_config(path: &std::path::Path) -> GatewayConfig {
        GatewayConfig::load(path).expect("reload saved config")
    }

    /// QA Happy: POST /api/devices 落盘往返——响应 accepted + config_version
    /// 字符串（大数红线）；文件重新解析出登记行；GET /api/devices 立即可见。
    #[tokio::test]
    async fn device_create_persists_and_is_readable() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (state, path) = make_state(&dir);
        let token = token_for(&state, Role::System);
        let port = spawn_server(state.clone()).await;

        let (status, _, body) = http_post_bearer(
            port,
            "/api/devices",
            r#"{"id":"dev-02","name":"二号设备","protocol":"s7","enabled":false}"#,
            &token,
        )
        .await;
        assert_eq!(status, 200, "{body}");
        let value: Value = serde_json::from_str(&body).expect("json");
        assert_eq!(value["accepted"], true);
        assert_eq!(value["device_id"], "dev-02");
        assert!(
            value["config_version"].is_string(),
            "config_version must be string (大数红线): {value}"
        );

        // 落盘往返：重新解析出登记行（含启停 / 协议 / 名称）。
        let config = load_config(&path);
        let entry = config
            .devices
            .iter()
            .find(|d| d.device_id == "dev-02")
            .expect("device row persisted");
        assert_eq!(entry.protocol.as_deref(), Some("s7"));
        assert!(!entry.enabled);
        assert_eq!(entry.name.as_deref(), Some("二号设备"));
        assert_eq!(config.points.len(), 1, "points untouched");

        // 读接口立即可见（热快照即时推送；无点位设备 poll_interval_ms="0"）。
        let (status, _, body) = http_get(port, "/api/devices").await;
        assert_eq!(status, 200);
        let devices: Vec<Value> = serde_json::from_str(&body).expect("array");
        let row = devices
            .iter()
            .find(|d| d["id"] == "dev-02")
            .expect("new device visible");
        assert_eq!(row["name"], "二号设备");
        assert_eq!(row["enabled"], false);
        assert_eq!(row["poll_interval_ms"], "0");
    }

    /// QA Error: 重复设备 id / 缺 protocol / 未知协议 → 400「字段+原因+允许值」。
    #[tokio::test]
    async fn device_create_validation_errors_are_400() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (state, path) = make_state(&dir);
        let token = token_for(&state, Role::System);
        let port = spawn_server(state.clone()).await;

        // 重复 id（点位聚合命中 dev-01）。
        let (status, _, body) = http_post_bearer(
            port,
            "/api/devices",
            r#"{"id":"dev-01","protocol":"s7"}"#,
            &token,
        )
        .await;
        assert_eq!(status, 400, "{body}");
        let value: Value = serde_json::from_str(&body).expect("json");
        assert_eq!(value["error"], "validation_failed");
        assert_eq!(value["field"], "id");
        assert_eq!(value["reason"], "device \"dev-01\" already exists");

        // 重复 id（登记段命中 dev-empty）。
        let (status, _, body) = http_post_bearer(
            port,
            "/api/devices",
            r#"{"id":"dev-empty","protocol":"s7"}"#,
            &token,
        )
        .await;
        assert_eq!(status, 400, "{body}");

        // 缺 protocol。
        let (status, _, body) =
            http_post_bearer(port, "/api/devices", r#"{"id":"dev-03"}"#, &token).await;
        assert_eq!(status, 400, "{body}");
        let value: Value = serde_json::from_str(&body).expect("json");
        assert_eq!(value["field"], "protocol");
        assert!(
            value["allowed"]
                .as_str()
                .expect("allowed")
                .contains("modbus-tcp"),
            "allowed must enumerate protocols: {value}"
        );

        // 未知协议。
        let (status, _, body) = http_post_bearer(
            port,
            "/api/devices",
            r#"{"id":"dev-03","protocol":"magic-bus"}"#,
            &token,
        )
        .await;
        assert_eq!(status, 400, "{body}");
        let value: Value = serde_json::from_str(&body).expect("json");
        assert_eq!(value["field"], "protocol");

        // 全部失败路径均不落盘。
        assert_eq!(load_config(&path).devices.len(), 1, "seed device only");
    }

    /// QA: PUT /api/devices/:id 改名 + 启停落盘；upsert 语义（点位聚合设备
    /// 首次更新落登记行）；未知设备 404。
    #[tokio::test]
    async fn device_update_renames_toggles_and_upserts() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (state, path) = make_state(&dir);
        let token = token_for(&state, Role::System);
        let port = spawn_server(state.clone()).await;

        // 点位聚合设备 dev-01（无登记行）→ upsert 落登记行。
        let (status, _, body) = http_put_bearer(
            port,
            "/api/devices/dev-01",
            r#"{"name":"一号改名","enabled":false}"#,
            &token,
        )
        .await;
        assert_eq!(status, 200, "{body}");
        let config = load_config(&path);
        let entry = config
            .devices
            .iter()
            .find(|d| d.device_id == "dev-01")
            .expect("upserted entry persisted");
        assert_eq!(entry.name.as_deref(), Some("一号改名"));
        assert!(!entry.enabled);

        // 读接口反映覆盖：name 来自登记段，protocol 仍按点位行推导。
        let (status, _, body) = http_get(port, "/api/devices").await;
        assert_eq!(status, 200);
        let devices: Vec<Value> = serde_json::from_str(&body).expect("array");
        let row = devices.iter().find(|d| d["id"] == "dev-01").expect("row");
        assert_eq!(row["name"], "一号改名");
        assert_eq!(row["protocol"], "modbus-tcp");
        assert_eq!(row["enabled"], false);

        // 未知设备 → 404。
        let (status, _, _) =
            http_put_bearer(port, "/api/devices/nope", r#"{"enabled":true}"#, &token).await;
        assert_eq!(status, 404);
    }

    /// QA: 设备删除级联语义——有点位默认 400（fail-closed），cascade=true
    /// 连同点位一并删除；仅登记设备直接删除；重复删除 404。
    #[tokio::test]
    async fn device_delete_cascade_semantics() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (state, path) = make_state(&dir);
        let token = token_for(&state, Role::System);
        let port = spawn_server(state.clone()).await;

        // dev-01 有点位：默认拒绝（fail-closed）。
        let (status, _, body) = http_delete_bearer(port, "/api/devices/dev-01", &token).await;
        assert_eq!(status, 400, "{body}");
        let value: Value = serde_json::from_str(&body).expect("json");
        assert_eq!(value["error"], "device_has_points");
        assert_eq!(value["field"], "cascade");
        assert!(load_config(&path).points.len() == 1, "points untouched");

        // cascade=true → 设备 + 点位一并删除。
        let (status, _, body) =
            http_delete_bearer(port, "/api/devices/dev-01?cascade=true", &token).await;
        assert_eq!(status, 200, "{body}");
        let config = load_config(&path);
        assert!(
            config.points.iter().all(|p| p.device_id != "dev-01"),
            "cascade removes points"
        );
        assert!(
            !config.devices.iter().any(|d| d.device_id == "dev-01"),
            "cascade removes registry entry if any"
        );

        // 仅登记设备（无点位）直接删除。
        let (status, _, _) = http_delete_bearer(port, "/api/devices/dev-empty", &token).await;
        assert_eq!(status, 200);
        assert!(load_config(&path).devices.is_empty(), "dev-empty removed");

        // 重复删除 → 404。
        let (status, _, _) = http_delete_bearer(port, "/api/devices/dev-empty", &token).await;
        assert_eq!(status, 404);
    }

    /// QA Happy: 点位新增落盘往返（frequency_ms 字符串形态）+ 重复 400；
    /// GET /api/points 立即可见。
    #[tokio::test]
    async fn point_create_roundtrip_and_duplicate_is_400() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (state, path) = make_state(&dir);
        let token = token_for(&state, Role::System);
        let port = spawn_server(state.clone()).await;

        let (status, _, body) = http_post_bearer(
            port,
            "/api/points",
            r#"{"device_id":"dev-01","point_id":"p_press","protocol":"modbus-tcp","address":"40010","frequency_ms":"250"}"#,
            &token,
        )
        .await;
        assert_eq!(status, 200, "{body}");
        let value: Value = serde_json::from_str(&body).expect("json");
        assert_eq!(value["point_id"], "p_press");
        assert_eq!(value["device_id"], "dev-01");
        assert!(value["config_version"].is_string(), "大数红线: {value}");

        let config = load_config(&path);
        let row = config
            .points
            .iter()
            .find(|p| p.point_id == "p_press")
            .expect("point row persisted");
        assert_eq!(row.address, "40010");
        assert_eq!(row.frequency_ms, 250);

        // 重复 (device, point) → 400。
        let (status, _, body) = http_post_bearer(
            port,
            "/api/points",
            r#"{"device_id":"dev-01","point_id":"p_press","protocol":"modbus-tcp","address":"40011"}"#,
            &token,
        )
        .await;
        assert_eq!(status, 400, "{body}");
        let value: Value = serde_json::from_str(&body).expect("json");
        assert_eq!(value["field"], "point_id");

        // 读接口立即可见。
        let (status, _, body) = http_get(port, "/api/points?device_id=dev-01").await;
        assert_eq!(status, 200);
        let points: Vec<Value> = serde_json::from_str(&body).expect("array");
        assert_eq!(points.len(), 2);
    }

    /// QA Error: 非法地址 400（modbus / s7 走 PointAddressParser 校验——
    /// 复用 driver 解析器，只调用不修改）；合法 s7 地址通过。
    #[tokio::test]
    async fn point_address_validation_rejects_invalid() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (state, path) = make_state(&dir);
        let token = token_for(&state, Role::System);
        let port = spawn_server(state.clone()).await;

        // modbus 非法地址。
        let (status, _, body) = http_post_bearer(
            port,
            "/api/points",
            r#"{"device_id":"dev-01","point_id":"p_bad","protocol":"modbus-tcp","address":"INVALID"}"#,
            &token,
        )
        .await;
        assert_eq!(status, 400, "{body}");
        let value: Value = serde_json::from_str(&body).expect("json");
        assert_eq!(value["error"], "validation_failed");
        assert_eq!(value["field"], "address");
        assert!(
            value["reason"]
                .as_str()
                .expect("reason")
                .contains("invalid point address"),
            "reason must carry parser detail: {value}"
        );

        // s7 位号越界（bit 必须 0-7）。
        let (status, _, body) = http_post_bearer(
            port,
            "/api/points",
            r#"{"device_id":"dev-01","point_id":"p_bad","protocol":"s7","address":"DB1.DBX0.9"}"#,
            &token,
        )
        .await;
        assert_eq!(status, 400, "{body}");

        // s7 合法地址通过（统一解析器）。
        let (status, _, body) = http_post_bearer(
            port,
            "/api/points",
            r#"{"device_id":"dev-01","point_id":"p_s7","protocol":"s7","address":"DB2.DBW20"}"#,
            &token,
        )
        .await;
        assert_eq!(status, 200, "{body}");
        assert!(load_config(&path)
            .points
            .iter()
            .any(|p| p.point_id == "p_s7" && p.protocol == "s7"));

        // 空地址 400。
        let (status, _, _) = http_post_bearer(
            port,
            "/api/points",
            r#"{"device_id":"dev-01","point_id":"p_bad","protocol":"modbus-tcp","address":"  "}"#,
            &token,
        )
        .await;
        assert_eq!(status, 400);
    }

    /// QA: frequency_ms 校验——低于下限 400（计划指标 ≥100ms）、非数字 400、
    /// 缺省 1000ms、数字形态可用。
    #[tokio::test]
    async fn point_frequency_validation() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (state, path) = make_state(&dir);
        let token = token_for(&state, Role::System);
        let port = spawn_server(state.clone()).await;

        // 低于下限。
        let (status, _, body) = http_post_bearer(
            port,
            "/api/points",
            r#"{"device_id":"dev-01","point_id":"p_fast","protocol":"modbus-tcp","address":"40001","frequency_ms":50}"#,
            &token,
        )
        .await;
        assert_eq!(status, 400, "{body}");
        let value: Value = serde_json::from_str(&body).expect("json");
        assert_eq!(value["field"], "frequency_ms");
        assert_eq!(value["allowed"], ">= 100 (ms)");

        // 非数字。
        let (status, _, _) = http_post_bearer(
            port,
            "/api/points",
            r#"{"device_id":"dev-01","point_id":"p_fast","protocol":"modbus-tcp","address":"40001","frequency_ms":"abc"}"#,
            &token,
        )
        .await;
        assert_eq!(status, 400);

        // 缺省 1000ms。
        let (status, _, _) = http_post_bearer(
            port,
            "/api/points",
            r#"{"device_id":"dev-01","point_id":"p_dflt","protocol":"modbus-tcp","address":"40002"}"#,
            &token,
        )
        .await;
        assert_eq!(status, 200);
        assert_eq!(
            load_config(&path)
                .points
                .iter()
                .find(|p| p.point_id == "p_dflt")
                .expect("row")
                .frequency_ms,
            1000
        );
    }

    /// QA: 点位修改 + 删除落盘往返；未知点位 404；未知设备新增点位 404。
    #[tokio::test]
    async fn point_update_delete_roundtrip_and_404() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (state, path) = make_state(&dir);
        let token = token_for(&state, Role::System);
        let port = spawn_server(state.clone()).await;

        // 修改地址 + 频率（数字形态）。
        let (status, _, body) = http_put_bearer(
            port,
            "/api/points/dev-01/p_temp",
            r#"{"address":"40001","frequency_ms":500}"#,
            &token,
        )
        .await;
        assert_eq!(status, 200, "{body}");
        let row = load_config(&path)
            .points
            .into_iter()
            .find(|p| p.point_id == "p_temp")
            .expect("row");
        assert_eq!(row.address, "40001");
        assert_eq!(row.frequency_ms, 500);

        // 未知点位 → 404。
        let (status, _, _) = http_put_bearer(
            port,
            "/api/points/dev-01/nope",
            r#"{"address":"40002"}"#,
            &token,
        )
        .await;
        assert_eq!(status, 404);

        // 未知设备上新增点位 → 404。
        let (status, _, _) = http_post_bearer(
            port,
            "/api/points",
            r#"{"device_id":"nope","point_id":"p1","protocol":"mc","address":"D100"}"#,
            &token,
        )
        .await;
        assert_eq!(status, 404);

        // 删除 → 文件中消失；再删 → 404。
        let (status, _, _) = http_delete_bearer(port, "/api/points/dev-01/p_temp", &token).await;
        assert_eq!(status, 200);
        assert!(
            !load_config(&path)
                .points
                .iter()
                .any(|p| p.point_id == "p_temp"),
            "point row removed from disk"
        );
        let (status, _, _) = http_delete_bearer(port, "/api/points/dev-01/p_temp", &token).await;
        assert_eq!(status, 404);
    }

    /// QA RBAC: ops 角色（不持 device.write / point.write）→ 403 + 审计
    ///（被拒动作入审计环，actor = JWT sub）。
    #[tokio::test]
    async fn write_denied_for_role_without_permission_is_403_and_audited() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (state, path) = make_state(&dir);
        let ops = token_for(&state, Role::Ops);
        let port = spawn_server(state.clone()).await;

        let (status, _, body) = http_post_bearer(
            port,
            "/api/devices",
            r#"{"id":"dev-9","protocol":"s7"}"#,
            &ops,
        )
        .await;
        assert_eq!(status, 403, "{body}");
        let value: Value = serde_json::from_str(&body).expect("json");
        assert_eq!(value["error"], "forbidden");

        let (status, _, _) = http_post_bearer(
            port,
            "/api/points",
            r#"{"device_id":"dev-01","point_id":"p9","protocol":"s7","address":"DB1.DBW2"}"#,
            &ops,
        )
        .await;
        assert_eq!(status, 403);

        // 审计：两条被拒（device_create / point_create）。
        let denied: Vec<_> = runtime_for(&state)
            .audit_snapshot()
            .into_iter()
            .filter(|e| !e.allowed && e.outcome == OUTCOME_DENIED)
            .collect();
        let actions: Vec<&str> = denied.iter().map(|e| e.action.as_str()).collect();
        assert_eq!(actions, vec!["device_create", "point_create"]);
        assert!(denied.iter().all(|e| e.actor == "ops-admin"));

        // 不落盘。
        let config = load_config(&path);
        assert_eq!(config.devices.len(), 1);
        assert_eq!(config.points.len(), 1);
    }

    /// QA 安全: 未携带 token → 401（身份未建立；不留审计痕——对齐 ops 语义）。
    #[tokio::test]
    async fn unauthenticated_write_is_401_without_audit() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (state, path) = make_state(&dir);
        let port = spawn_server(state.clone()).await;

        let (status, _, _) = http_request(
            port,
            "POST",
            "/api/devices",
            Some(r#"{"id":"x","protocol":"s7"}"#),
            None,
        )
        .await;
        assert_eq!(status, 401);

        let (status, _, _) = http_delete_bearer(port, "/api/points/dev-01/p_temp", "").await;
        assert_eq!(status, 401, "invalid token must be 401");

        assert!(
            runtime_for(&state).audit_snapshot().is_empty(),
            "identity never established → no forged audit entries"
        );
        let config = load_config(&path);
        assert_eq!(config.devices.len(), 1);
        assert_eq!(config.points.len(), 1);
    }

    /// QA: 并发写串行化——6 个并发设备创建全部 200，最终文件可解析且
    /// 含全部设备（无撕裂 / 丢更新）。
    #[tokio::test]
    async fn concurrent_device_creates_serialize_and_persist() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (state, path) = make_state(&dir);
        let token = token_for(&state, Role::System);
        let port = spawn_server(state.clone()).await;

        let mut tasks = Vec::new();
        for i in 0..6 {
            let token = token.clone();
            tasks.push(tokio::spawn(async move {
                let body = format!(r#"{{"id":"dev-c{i}","protocol":"modbus-tcp"}}"#);
                http_post_bearer(port, "/api/devices", &body, &token).await
            }));
        }
        for task in tasks {
            let (status, _, body) = task.await.expect("join");
            assert_eq!(status, 200, "{body}");
        }

        // 最终落盘：全部 6 个设备 + 种子登记设备，且文件整体可解析（无撕裂）。
        let config = load_config(&path);
        assert_eq!(config.devices.len(), 7, "6 created + 1 seed");
        for i in 0..6 {
            assert!(
                config
                    .devices
                    .iter()
                    .any(|d| d.device_id == format!("dev-c{i}")),
                "dev-c{i} must be persisted"
            );
        }
        assert_eq!(config.points.len(), 1, "points untouched");
    }

    /// QA: 写后备份——首次写操作产生 `config.toml.bak-*`，内容为写前快照。
    #[tokio::test]
    async fn write_creates_backup_of_pre_write_snapshot() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (state, path) = make_state(&dir);
        let before_raw = std::fs::read_to_string(&path).expect("read pre-write");
        let token = token_for(&state, Role::System);
        let port = spawn_server(state.clone()).await;

        let (status, _, _) = http_post_bearer(
            port,
            "/api/devices",
            r#"{"id":"dev-9","protocol":"mc"}"#,
            &token,
        )
        .await;
        assert_eq!(status, 200);

        let mut backups: Vec<PathBuf> = std::fs::read_dir(dir.path())
            .expect("read_dir")
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| {
                p.file_name()
                    .and_then(|n| n.to_str())
                    .map(|n| n.starts_with("config.toml.bak-") && !n.contains(".tmp"))
                    .unwrap_or(false)
            })
            .collect();
        assert_eq!(
            backups.len(),
            1,
            "exactly one backup after one write: {backups:?}"
        );
        let backup_raw = std::fs::read_to_string(backups.remove(0)).expect("read backup");
        assert_eq!(
            backup_raw, before_raw,
            "backup must be the pre-write snapshot"
        );
    }

    /// QA: 写路径快照与落盘一致性——写后 `ConfigShared::replace` 即时生效，
    /// 读接口（/api/status 的 device_count 等聚合）反映新点位数。
    #[tokio::test]
    async fn status_aggregation_reflects_writes_immediately() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (state, _path) = make_state(&dir);
        let token = token_for(&state, Role::System);
        let port = spawn_server(state.clone()).await;

        // 新增第二个设备（经点位聚合影响 device_count）。
        let (status, _, _) = http_post_bearer(
            port,
            "/api/devices",
            r#"{"id":"dev-02","protocol":"modbus-tcp"}"#,
            &token,
        )
        .await;
        assert_eq!(status, 200);
        let (status, _, _) = http_post_bearer(
            port,
            "/api/points",
            r#"{"device_id":"dev-02","point_id":"p1","protocol":"modbus-tcp","address":"40001"}"#,
            &token,
        )
        .await;
        assert_eq!(status, 200);

        let (status, _, body) = http_get(port, "/api/status").await;
        assert_eq!(status, 200);
        let value: Value = serde_json::from_str(&body).expect("json");
        assert_eq!(
            value["device_count"], "2",
            "two devices with points: {value}"
        );
    }

    /// QA（task 26）: 配置写受理 → 持久安全审计落 `config_change`（accepted），
    /// RBAC 拒绝 → `authz_failed`（denied）；事件进入防篡改哈希链且整链校验通过。
    #[tokio::test]
    async fn config_write_lands_in_persistent_audit_chain() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (state, _path) = make_state(&dir);
        // 挂载持久审计库（make_state 本身不挂载；生产由 bootstrap 装配）。
        let audit_dir = tempfile::tempdir().expect("audit tempdir");
        let logger = Arc::new(
            crate::audit::AuditLogger::open(&audit_dir.path().join("audit.db"), Some(b"ikm"))
                .expect("open audit db"),
        );
        state.daemon().set_audit_logger(Arc::clone(&logger));

        let system = token_for(&state, Role::System);
        let ops = token_for(&state, Role::Ops);
        let port = spawn_server(state.clone()).await;

        // ① system（持 device.write）新增设备 → 200 + config_change(accepted)。
        let (status, _, body) = http_post_bearer(
            port,
            "/api/devices",
            r#"{"id":"dev-audit","protocol":"modbus-tcp"}"#,
            &system,
        )
        .await;
        assert_eq!(status, 200, "{body}");

        // ② ops（不持 device.write）新增设备 → 403 + authz_failed(denied)。
        let (status, _, _) = http_post_bearer(
            port,
            "/api/devices",
            r#"{"id":"dev-denied","protocol":"modbus-tcp"}"#,
            &ops,
        )
        .await;
        assert_eq!(status, 403);

        // ③ 持久审计两事件齐备 + 链校验通过。
        let rows = logger.query(&AuditQuery::new()).expect("query audit");
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].event, "config_change");
        assert_eq!(rows[0].actor, "ops-admin");
        assert_eq!(rows[0].outcome, OUTCOME_ACCEPTED);
        assert_eq!(rows[1].event, "authz_failed");
        assert_eq!(rows[1].outcome, OUTCOME_DENIED);
        assert!(rows.iter().all(|r| r.detail.contains("device_create")));
        assert!(logger.verify_chain().expect("verify").ok);
    }
}

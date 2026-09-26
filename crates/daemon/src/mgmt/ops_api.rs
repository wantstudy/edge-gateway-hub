//! 运维端点四类（E 项收口；`GET /api/updates/check` + `GET /api/service/autostart`
//! + `POST /api/settings/backups` + `GET /api/diagnostics/selfcheck`）
//! + 备份策略（B-2 收口：`GET|PUT /api/settings/backup-policy` + 周期备份 + retention）。
//!
//! ## 诚实性红线
//! 每个能力：实现了就真实现；没实现就**结构化返回「未实现 + 原因」**，严禁假成功。
//! 更新检查未接升级源 → `check_supported:false` + 原因；自启注册读写在 Windows 下
//! 真实现（`reg query` / `reg add|delete` HKCU Run 键），非 Windows 如实 501。
//!
//! ## 备份策略语义（B-2）
//! - `auto_before_write`：写路径落盘前自动备份（既有行为显式化，闸门在
//!   `GatewayConfig::save`，全写路径统一生效）；
//! - `retention_count`：`{config}.bak-*` 最大保留份数（超出删最旧；**只清本服务
//!   自产的该前缀文件**——`config.rs`/`migrations.rs` 同一命名模式；`0` = 不清理）；
//! - `interval_min`：周期备份间隔（分钟，`0` = 关闭）。本模块 30s tick 轮询实现，
//!   每拍读热快照——PUT 热重载后下一拍即按新间隔生效。
//!
//! ## 消费方
//! - `UpdatePage.vue`（当前只读 `/api/overview` 的 version；本端点为其预留诚实数据源）；
//! - `StartupPage.vue`（自启状态）；
//! - `SettingsPage.vue`（手动备份动作；清单读既有 `GET /api/settings/backups`）；
//! - `DiagnosePage.vue`（自检清单，当前只调 `/api/health`；本端点提供逐项判定）。

use std::sync::atomic::{AtomicU64, Ordering};

use axum::body::Bytes;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Json, Response};
use serde_json::{json, Value};
use tokio::time::MissedTickBehavior;

use super::rbac::{AuthedRole, Permission};
use super::remote_ops::{OpsAction, OUTCOME_ACCEPTED, OUTCOME_BAD_REQUEST, OUTCOME_DENIED};
use super::writeapi::{audit, validation_error, write_guard, OUTCOME_FAILED};
use super::MgmtState;

/// 当前 Unix 毫秒（时钟回退为 0，不阻塞）。
fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

// ---- 更新检查 ----

/// `GET /api/updates/check` → 更新检查（**当前未接升级源，诚实返回**）。
///
/// 未配置升级源时：`check_supported:false` + 原因说明；绝不伪造
/// 「已是最新版本」之类的假成功。接入升级源后本端点改为真实比对。
pub async fn updates_check(State(state): State<MgmtState>, authed: AuthedRole) -> Response {
    if let Err(resp) = authed.ensure(Permission::OpsLogsRead) {
        return resp.into_response();
    }
    let current = env!("CARGO_PKG_VERSION");
    let _ = state;
    Json(json!({
        "check_supported": false,
        "current_version": current,
        "update_available": false,
        "available_version": Value::Null,
        "source": "unconfigured",
        "reason": "no update source configured (OTA source wiring pending); \
                   nothing was downloaded or applied",
    }))
    .into_response()
}

// ---- 启动与自启 ----

/// Windows：读取 `HKCU\Software\Microsoft\Windows\CurrentVersion\Run` 的
/// `iot-daq` 值（`reg query`，本机只读操作；按 **exit code** 判定——0 = 注册表
/// 命中，1 = 未注册，其他 = 查询失败。不解析 stderr 文案，避免本地化差异）。
/// GET 与 PUT（写后回读）共用该形状。
#[cfg(target_os = "windows")]
fn autostart_status_body() -> Value {
    let output = std::process::Command::new("reg")
        .args([
            "query",
            r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run",
            "/v",
            "iot-daq",
        ])
        .output();
    let (registered, command, query_error) = match output {
        Ok(out) if out.status.success() => {
            let stdout = String::from_utf8_lossy(&out.stdout).to_string();
            let command = stdout
                .lines()
                .find_map(|line| {
                    let idx = line.find("REG_SZ")?;
                    Some(line[idx + "REG_SZ".len()..].trim().to_string())
                })
                .unwrap_or_default();
            (json!(true), json!(command), Value::Null)
        }
        Ok(out) if out.status.code() == Some(1) => (json!(false), Value::Null, Value::Null),
        Ok(out) => (
            Value::Null,
            Value::Null,
            json!(format!("reg query exited {:?}", out.status.code())),
        ),
        Err(err) => (Value::Null, Value::Null, json!(err.to_string())),
    };
    json!({
        "supported": true,
        "registered": registered,
        "command": command,
        "source": "registry-hkcu-run",
        "query_error": query_error,
    })
}

/// `GET /api/service/autostart` → 自启注册状态。
pub async fn service_autostart(State(state): State<MgmtState>, authed: AuthedRole) -> Response {
    if let Err(resp) = authed.ensure(Permission::OpsLogsRead) {
        return resp.into_response();
    }
    let _ = state;
    #[cfg(target_os = "windows")]
    {
        let mut body = autostart_status_body();
        body["write_supported"] = json!(true);
        body["write_reason"] = Value::Null;
        Json(body).into_response()
    }
    #[cfg(not(target_os = "windows"))]
    {
        Json(json!({
            "supported": false,
            "registered": Value::Null,
            "source": "unimplemented",
            "reason": "autostart status is only implemented for Windows (registry HKCU Run)",
            "write_supported": false,
            "write_reason": "autostart registration is only implemented for Windows",
        }))
        .into_response()
    }
}

/// `PUT /api/service/autostart` → 注册 / 注销 HKCU Run 自启项（team-lead 追加项）。
///
/// - body：`{"enable": bool}`（必填；可选 `reason` 进审计）；
/// - `enable=true`：`reg add HKCU\...\Run /v iot-daq /t REG_SZ /d "<current_exe>" /f`
///   （值带引号——Run 键惯例，含空格路径也能解析）；
/// - `enable=false`：`reg delete HKCU\...\Run /v iot-daq /f`；exit 1 = 本就未注册，
///   **幂等成功**；
/// - 成功后回读注册表真实状态（与 GET 同形状 + `accepted:true`）；
/// - 写失败 → 500 `autostart_write_failed`；非 Windows → 501 `not_supported`；
/// - 守卫：`OpsCollectors`（服务运行期控制，仅 system），写动作含被拒入审计环。
#[cfg(target_os = "windows")]
pub async fn service_autostart_put(
    State(state): State<MgmtState>,
    authed: AuthedRole,
    body: Bytes,
) -> Response {
    let actor = authed.claims.sub.clone();
    if let Err(rejection) = authed.ensure(Permission::OpsCollectors) {
        audit(
            &state,
            &actor,
            OpsAction::AutostartWrite,
            false,
            OUTCOME_DENIED,
            &rejection.to_string(),
        );
        return rejection.into_response();
    }
    let req: Value = match serde_json::from_slice::<Value>(&body) {
        Ok(value) if value.is_object() => value,
        _ => {
            return autostart_audit_bad_request(&state, &actor, "body must be a JSON object");
        }
    };
    let enable = match req.get("enable") {
        Some(Value::Bool(flag)) => *flag,
        Some(_) => {
            return autostart_audit_bad_request(&state, &actor, "\"enable\" must be a boolean");
        }
        None => {
            return autostart_audit_bad_request(
                &state,
                &actor,
                "missing required field \"enable\"",
            );
        }
    };
    let reason = req
        .get("reason")
        .and_then(Value::as_str)
        .unwrap_or("<unspecified>");

    let _guard = write_guard();
    let run_key = r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run";
    let result = if enable {
        let exe = match std::env::current_exe() {
            Ok(path) => path,
            Err(err) => {
                audit(
                    &state,
                    &actor,
                    OpsAction::AutostartWrite,
                    false,
                    OUTCOME_FAILED,
                    &format!("autostart enable failed: current_exe unavailable: {err}"),
                );
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({
                        "error": "autostart_write_failed",
                        "message": format!("current exe path unavailable: {err}"),
                    })),
                )
                    .into_response();
            }
        };
        let cmd_value = format!("\"{}\"", exe.display());
        std::process::Command::new("reg")
            .args([
                "add", run_key, "/v", "iot-daq", "/t", "REG_SZ", "/d", &cmd_value, "/f",
            ])
            .output()
    } else {
        std::process::Command::new("reg")
            .args(["delete", run_key, "/v", "iot-daq", "/f"])
            .output()
    };
    match result {
        // 0 = 成功；enable=false 且 exit 1 = 本就未注册（幂等成功）。
        Ok(out) if out.status.success() || (!enable && out.status.code() == Some(1)) => {
            audit(
                &state,
                &actor,
                OpsAction::AutostartWrite,
                true,
                OUTCOME_ACCEPTED,
                &format!(
                    "autostart {} (reason: {reason})",
                    if enable { "registered" } else { "unregistered" }
                ),
            );
            let mut body = autostart_status_body();
            body["accepted"] = json!(true);
            body["write_supported"] = json!(true);
            Json(body).into_response()
        }
        Ok(out) => {
            let detail = format!("reg exited {:?} (enable={enable})", out.status.code());
            audit(
                &state,
                &actor,
                OpsAction::AutostartWrite,
                false,
                OUTCOME_FAILED,
                &detail,
            );
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({
                    "error": "autostart_write_failed",
                    "message": detail,
                })),
            )
                .into_response()
        }
        Err(err) => {
            let detail = format!("reg spawn failed: {err}");
            audit(
                &state,
                &actor,
                OpsAction::AutostartWrite,
                false,
                OUTCOME_FAILED,
                &detail,
            );
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({
                    "error": "autostart_write_failed",
                    "message": detail,
                })),
            )
                .into_response()
        }
    }
}

/// 非 Windows：PUT 无实现 → 501（诚实占位；读取见 GET 的 `supported:false`）。
#[cfg(not(target_os = "windows"))]
pub async fn service_autostart_put(
    State(state): State<MgmtState>,
    authed: AuthedRole,
    body: Bytes,
) -> Response {
    let actor = authed.claims.sub.clone();
    if let Err(rejection) = authed.ensure(Permission::OpsCollectors) {
        audit(
            &state,
            &actor,
            OpsAction::AutostartWrite,
            false,
            OUTCOME_DENIED,
            &rejection.to_string(),
        );
        return rejection.into_response();
    }
    audit(
        &state,
        &actor,
        OpsAction::AutostartWrite,
        false,
        OUTCOME_BAD_REQUEST,
        "autostart write is only implemented for Windows (registry HKCU Run)",
    );
    (
        StatusCode::NOT_IMPLEMENTED,
        Json(json!({
            "error": "not_supported",
            "message": "autostart registration is only implemented for Windows (registry HKCU Run)",
        })),
    )
        .into_response()
}

/// 自启 PUT 校验失败：入审计（bad_request）后返回 400。
fn autostart_audit_bad_request(state: &MgmtState, actor: &str, detail: &str) -> Response {
    audit(
        state,
        actor,
        OpsAction::AutostartWrite,
        false,
        OUTCOME_BAD_REQUEST,
        detail,
    );
    (
        StatusCode::BAD_REQUEST,
        Json(json!({ "error": "validation_failed", "message": detail })),
    )
        .into_response()
}

// ---- 手动备份 ----

/// `POST /api/settings/backups` → 手动创建一份配置备份。
///
/// 复用既有备份命名空间：`config.toml.bak-manual-<epoch_ms>`（`GET
/// /api/settings/backups` 的清单过滤 `*.bak-*` 前缀，手动备份自动可见）。
/// 复制当前 config.toml 原文（字节级快照，不做序列化重写）。
pub async fn create_backup(State(state): State<MgmtState>, authed: AuthedRole) -> Response {
    let actor = authed.claims.sub.clone();
    if let Err(rejection) = authed.ensure(Permission::DeviceWrite) {
        audit(
            &state,
            &actor,
            OpsAction::SettingsWrite,
            false,
            OUTCOME_DENIED,
            &rejection.to_string(),
        );
        return rejection.into_response();
    }
    let _guard = write_guard();
    let Some(path) = state.config_path() else {
        audit(
            &state,
            &actor,
            OpsAction::SettingsWrite,
            false,
            OUTCOME_BAD_REQUEST,
            "config path not configured; manual backup refused",
        );
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({
                "error": "internal",
                "message": "config file path not configured; backup refused (fail-closed)",
            })),
        )
            .into_response();
    };
    let dir = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .map_or_else(std::path::PathBuf::new, std::borrow::ToOwned::to_owned);
    let file_name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("config.toml")
        .to_string();
    let target = dir.join(format!("{file_name}.bak-manual-{}", now_ms()));
    let result = std::fs::copy(&path, &target);
    match result {
        Ok(bytes) => {
            let backup_name = target
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or_default()
                .to_string();
            // retention 清理（与写前备份同一策略；0 = 不清理）。
            let policy = state.config().settings.backup_policy.clone();
            let pruned = state
                .config_path()
                .map(|p| crate::migrations::enforce_backup_retention(&p, policy.retention_count))
                .unwrap_or(0);
            audit(
                &state,
                &actor,
                OpsAction::SettingsWrite,
                true,
                OUTCOME_ACCEPTED,
                &format!(
                    "manual backup created {backup_name:?} ({bytes} bytes); retention pruned {pruned}"
                ),
            );
            Json(json!({
                "accepted": true,
                "backup": backup_name,
                "bytes": bytes.to_string(),
                "pruned": pruned.to_string(),
            }))
            .into_response()
        }
        Err(err) => {
            audit(
                &state,
                &actor,
                OpsAction::SettingsWrite,
                false,
                OUTCOME_BAD_REQUEST,
                &format!("manual backup failed: {err}"),
            );
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({
                    "error": "internal",
                    "message": format!("manual backup failed: {err}"),
                })),
            )
                .into_response()
        }
    }
}

// ---- 备份策略（B-2） ----

/// `GET /api/settings/backup-policy` → 备份策略只读视图（读，开放——与
/// `/api/settings` 同口径）。
///
/// 数字一律字符串（大数红线）；`source:"config"` 表示值来自配置热快照
/// （未做过 PUT 时即默认策略）。
pub async fn backup_policy_get(State(state): State<MgmtState>) -> Response {
    let policy = &state.config().settings.backup_policy;
    Json(json!({
        "auto_before_write": policy.auto_before_write,
        "retention_count": policy.retention_count.to_string(),
        "interval_min": policy.interval_min.to_string(),
        "source": "config",
    }))
    .into_response()
}

/// `PUT /api/settings/backup-policy` → 备份策略持久化（`device.write`，仅 system）。
///
/// Body：`{"auto_before_write": bool, "retention_count": string, "interval_min":
/// string, "reason": string, "note": string}`（`retention_count` / `interval_min`
/// 数字一律字符串——大数红线；`auto_before_write` / `retention_count` /
/// `interval_min` 漏传 = 保持现值；`reason` **必填**进审计，`note` 可选）。
///
/// 非危险操作：不强制 confirm 三要素；写走 persist_config 既有链路（写锁 →
/// 写前备份（若开启）→ 原子落盘 → 热替换）→ `ConfigReloaded` 广播 →
/// **落盘后按新 retention 触发一次清理**（即使 `auto_before_write=false`，
/// 手动/周期备份也受 retention 约束，PUT 即验证策略可达）。
pub async fn backup_policy_put(
    State(state): State<MgmtState>,
    authed: AuthedRole,
    body: Bytes,
) -> Response {
    const ALLOWED: &[&str] = &[
        "auto_before_write",
        "retention_count",
        "interval_min",
        "reason",
        "note",
    ];
    let actor = authed.claims.sub.clone();
    if let Err(rejection) = authed.ensure(Permission::DeviceWrite) {
        audit(
            &state,
            &actor,
            OpsAction::BackupPolicyWrite,
            false,
            OUTCOME_DENIED,
            &rejection.to_string(),
        );
        return rejection.into_response();
    }
    let req: Value = match serde_json::from_slice::<Value>(&body) {
        Ok(value) if value.is_object() => value,
        Ok(_) | Err(_) => {
            return audit_policy_bad_request(
                &state,
                &actor,
                "malformed json body",
                validation_error(
                    "body",
                    "body must be a JSON object",
                    "object {auto_before_write?, retention_count?, interval_min?, reason, note?}",
                ),
            );
        }
    };
    let Some(obj) = req.as_object() else {
        return audit_policy_bad_request(
            &state,
            &actor,
            "unreachable: body verified as JSON object",
            validation_error("body", "body must be a JSON object", "object"),
        );
    };
    for key in obj.keys() {
        if !ALLOWED.contains(&key.as_str()) {
            return audit_policy_bad_request(
                &state,
                &actor,
                "body carries unknown fields",
                validation_error(
                    &format!("body.{key}"),
                    &format!("unknown field {key:?}"),
                    &format!("allowed fields: {}", ALLOWED.join(" | ")),
                ),
            );
        }
    }
    // reason 必填（进审计 detail；非危险操作不强制 confirm 三要素）。
    let reason = obj
        .get("reason")
        .and_then(Value::as_str)
        .map(str::trim)
        .unwrap_or("");
    if reason.is_empty() {
        return audit_policy_bad_request(
            &state,
            &actor,
            "reason is required",
            validation_error(
                "reason",
                "reason must not be empty",
                "non-empty change reason",
            ),
        );
    }
    let note = obj
        .get("note")
        .and_then(Value::as_str)
        .map(str::trim)
        .unwrap_or("");
    let current = state.config().settings.backup_policy.clone();
    let mut next = current.clone();
    if let Some(raw) = obj.get("auto_before_write") {
        match raw.as_bool() {
            Some(value) => next.auto_before_write = value,
            None => {
                return audit_policy_bad_request(
                    &state,
                    &actor,
                    "auto_before_write is not a boolean",
                    validation_error("auto_before_write", "must be a boolean", "true | false"),
                );
            }
        }
    }
    if let Some(raw) = obj.get("retention_count") {
        match parse_u32_string(raw) {
            Ok(value) => next.retention_count = value,
            Err(resp) => {
                return audit_policy_bad_request(
                    &state,
                    &actor,
                    "retention_count is not a valid u32",
                    resp,
                );
            }
        }
    }
    if let Some(raw) = obj.get("interval_min") {
        match parse_u32_string(raw) {
            Ok(value) => next.interval_min = value,
            Err(resp) => {
                return audit_policy_bad_request(
                    &state,
                    &actor,
                    "interval_min is not a valid u32",
                    resp,
                );
            }
        }
    }

    let _guard = write_guard();
    let mut config = (*state.config()).clone();
    config.settings.backup_policy = next.clone();
    let detail = if note.is_empty() {
        format!("persist backup policy (reason: {reason})")
    } else {
        format!("persist backup policy (reason: {reason}; note: {note})")
    };
    // pruned 口径：本次 PUT 触发的清理净删除数（写前备份 +1、清理后现存 -1
    // 全部计入；persist 失败路径不返回该字段）。
    let backup_delta = if next.auto_before_write { 1usize } else { 0 };
    let bak_before = state
        .config_path()
        .map(|p| crate::migrations::count_backup_files(&p))
        .unwrap_or(0);
    match super::pages::persist_config(
        &state,
        config,
        &actor,
        OpsAction::BackupPolicyWrite,
        &detail,
    ) {
        Ok(version) => {
            state.publish(super::MgmtEvent::ConfigReloaded { version });
            // 落盘后按新策略立即触发一次 retention（0 = 不清理）。
            let pruned = state
                .config_path()
                .map(|p| {
                    let after = crate::migrations::count_backup_files(&p);
                    bak_before
                        .saturating_add(backup_delta)
                        .saturating_sub(after)
                })
                .unwrap_or(0);
            Json(json!({
                "accepted": true,
                "config_version": version.to_string(),
                "backup_policy": {
                    "auto_before_write": next.auto_before_write,
                    "retention_count": next.retention_count.to_string(),
                    "interval_min": next.interval_min.to_string(),
                },
                "pruned": pruned.to_string(),
            }))
            .into_response()
        }
        Err(resp) => resp,
    }
}

/// 备份策略 PUT 校验失败：入审计（bad_request）后返回 400。
fn audit_policy_bad_request(
    state: &MgmtState,
    actor: &str,
    detail: &str,
    resp: Response,
) -> Response {
    audit(
        state,
        actor,
        OpsAction::BackupPolicyWrite,
        true,
        OUTCOME_BAD_REQUEST,
        detail,
    );
    resp
}

/// u32 字段解析（接受 string——大数红线推荐形态——或 JSON number）。
#[allow(clippy::result_large_err)]
fn parse_u32_string(raw: &Value) -> Result<u32, Response> {
    let parsed = match raw {
        Value::Number(number) => number.as_u64(),
        Value::String(value) => value.trim().parse::<u64>().ok(),
        _ => None,
    };
    match parsed {
        Some(value) if value <= u32::MAX as u64 => Ok(value as u32),
        _ => Err(validation_error(
            "value",
            &format!("not a valid u32: {raw}"),
            "integer >= 0 (u32); string or number",
        )),
    }
}

// ---- 周期备份（interval_min > 0 时生效） ----

/// 上次周期备份时刻（Unix 毫秒；进程内状态，重启后重置为启动时刻——
/// 重启后的第一次到期备份最早发生在 `interval_min` 之后，不立即补打）。
static LAST_PERIODIC_BACKUP_MS: AtomicU64 = AtomicU64::new(0);

/// 生产入口：启动周期备份循环（30s tick 轮询；`interval_min == 0` 时每拍空转）。
///
/// 每拍读配置热快照——PUT 备份策略热重载后下一拍即按新间隔/新 retention 生效。
/// 测试服务不挂本循环（生产装配 `bin/iot-daq-daemon.rs` 独占调用）。
pub fn spawn_periodic_backup(state: &MgmtState) {
    let state = state.clone();
    LAST_PERIODIC_BACKUP_MS.store(now_ms(), Ordering::Relaxed);
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(std::time::Duration::from_secs(30));
        ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
        loop {
            ticker.tick().await;
            let policy = state.config().settings.backup_policy.clone();
            if policy.interval_min == 0 {
                continue;
            }
            let interval_ms = u64::from(policy.interval_min).saturating_mul(60_000);
            let last = LAST_PERIODIC_BACKUP_MS.load(Ordering::Relaxed);
            if now_ms().saturating_sub(last) < interval_ms {
                continue;
            }
            LAST_PERIODIC_BACKUP_MS.store(now_ms(), Ordering::Relaxed);
            periodic_backup_once(&state, policy.retention_count);
        }
    });
}

/// 执行一次周期备份（`config.toml.bak-periodic-<epoch_ms>` 字节级快照 +
/// retention 清理；失败仅告警——周期任务绝不打断主流程，也不假成功）。
fn periodic_backup_once(state: &MgmtState, retention: u32) {
    let Some(path) = state.config_path() else {
        tracing::warn!("periodic backup: config path not bound; skipped");
        return;
    };
    let dir = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .map_or_else(
            || std::path::PathBuf::from("."),
            std::borrow::ToOwned::to_owned,
        );
    let file_name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("config.toml");
    let target = dir.join(format!("{file_name}.bak-periodic-{}", now_ms()));
    match std::fs::copy(&path, &target) {
        Ok(bytes) => {
            let pruned = crate::migrations::enforce_backup_retention(&path, retention);
            let backup_name = target
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or_default()
                .to_string();
            tracing::info!(
                backup = %backup_name,
                bytes,
                pruned,
                "periodic backup created"
            );
            audit(
                state,
                "system-periodic",
                OpsAction::SettingsWrite,
                true,
                OUTCOME_ACCEPTED,
                &format!(
                    "periodic backup created {backup_name:?} ({bytes} bytes); retention pruned {pruned}"
                ),
            );
        }
        Err(err) => {
            tracing::warn!(error = %err, "periodic backup failed; will retry next interval");
            audit(
                state,
                "system-periodic",
                OpsAction::SettingsWrite,
                true,
                OUTCOME_FAILED,
                &format!("periodic backup failed: {err}"),
            );
        }
    }
}

// ---- 自检清单 ----

/// 自检单项。
struct CheckItem {
    name: &'static str,
    ok: bool,
    detail: Value,
}

/// `GET /api/diagnostics/selfcheck` → 自检清单（DiagnosePage 消费）。
///
/// 每项都是**真实判定**（读当前运行态，不缓存、不伪造）：
/// - `config_writable`：config 目录可写（临时文件探针，写后即删）；
/// - `scheduler`：调度器是否在跑 + 各组累计样本 / 错误；
/// - `alarm_engine`：告警规则数 + 存储记录数；
/// - `license`：授权状态与北向闸门；
/// - `audit_logger`：持久审计是否挂载；
/// - `machine_code`：机器码来源（license 指纹 / 占位）；
/// - `clock`：系统时钟可用性（UNIX_EPOCH 之后）。
pub async fn selfcheck(State(state): State<MgmtState>, authed: AuthedRole) -> Response {
    if let Err(resp) = authed.ensure(Permission::OpsLogsRead) {
        return resp.into_response();
    }
    let daemon = state.daemon();
    let config = state.config();
    let mut items: Vec<CheckItem> = Vec::new();

    // ① config 目录可写探针（临时文件写后即删；不产生备份、不碰配置本体）。
    // 相对路径（如 `config.toml`）的 parent 为空 → 用 `.`（与 rollback 同口径）。
    let config_ok = state.config_path().is_some();
    let write_probe = state.config_path().map(|path| {
        let dir = path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .map_or_else(
                || std::path::PathBuf::from("."),
                std::borrow::ToOwned::to_owned,
            );
        let probe = dir.join(format!(".selfcheck-probe-{}", now_ms()));
        let write = std::fs::write(&probe, b"ok").is_ok();
        if write {
            let _ = std::fs::remove_file(&probe);
        }
        write
    });
    items.push(CheckItem {
        name: "config_writable",
        ok: write_probe.unwrap_or(false),
        detail: json!({
            "config_path_bound": config_ok,
            "probe": write_probe,
        }),
    });

    // ② 调度器：在跑的组 + 累计计数。
    let (scheduler_running, group_stats) = {
        let scheduler = daemon.scheduler();
        match scheduler.as_ref() {
            Some(s) => {
                let stats: Vec<Value> = s
                    .group_names()
                    .iter()
                    .filter_map(|name| {
                        s.stats(name).map(|g| {
                            json!({
                                "group": name,
                                "samples": g.samples(),
                                "errors": g.errors(),
                            })
                        })
                    })
                    .collect();
                (true, stats)
            }
            None => (false, Vec::new()),
        }
    };
    items.push(CheckItem {
        name: "scheduler",
        ok: scheduler_running,
        detail: json!({ "running": scheduler_running, "groups": group_stats }),
    });

    // ③ 告警引擎：规则数 + 记录数（读侧真实统计）。
    let alarm_rules = config.alarms.as_ref().map(|a| a.rules.len()).unwrap_or(0);
    let alarm_records = daemon.alarms_store().len();
    items.push(CheckItem {
        name: "alarm_engine",
        ok: true, // 引擎随数据面挂载；有无规则都是合法态
        detail: json!({ "rules": alarm_rules, "records": alarm_records }),
    });

    // ④ 授权状态（runtime 缺席 = 未配置授权，如实 degrade 信息）。
    let license = daemon.license_runtime();
    let (license_state, forward_allowed, assembly_error) = match &license {
        Some(rt) => {
            use crate::auth::client::LicenseState;
            let label = match rt.state() {
                LicenseState::Unlicensed => "unlicensed",
                LicenseState::Trial { .. } => "trial",
                LicenseState::Licensed { .. } => "licensed",
                LicenseState::Grace { .. } => "grace",
                LicenseState::Degraded { .. } => "degraded",
            };
            (label.to_string(), rt.north_forward_allowed(), Value::Null)
        }
        None => (
            "absent".to_string(),
            false,
            json!(daemon
                .license_assembly_error()
                .unwrap_or_else(|| { "licensing not configured".to_string() })),
        ),
    };
    items.push(CheckItem {
        name: "license",
        ok: forward_allowed || license.is_none(),
        detail: json!({
            "state": license_state,
            "north_forward_allowed": forward_allowed,
            "assembly_error": assembly_error,
        }),
    });

    // ⑤ 持久审计挂载。
    let audit_mounted = daemon.audit_logger().is_some();
    items.push(CheckItem {
        name: "audit_logger",
        ok: audit_mounted,
        detail: json!({ "mounted": audit_mounted }),
    });

    // ⑥ 机器码来源（与 overview 同口径）。
    let machine_code_source = match &license {
        Some(_rt) => "license-fingerprint",
        None => "placeholder-unlicensed",
    };
    items.push(CheckItem {
        name: "machine_code",
        ok: true,
        detail: json!({ "source": machine_code_source }),
    });

    // ⑦ 时钟可用性。
    let clock_ok = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .is_ok();
    items.push(CheckItem {
        name: "clock",
        ok: clock_ok,
        detail: json!({ "now_ms": now_ms().to_string() }),
    });

    let all_ok = items.iter().all(|i| i.ok);
    let rows: Vec<Value> = items
        .into_iter()
        .map(|i| json!({ "name": i.name, "ok": i.ok, "detail": i.detail }))
        .collect();
    Json(json!({
        "ok": all_ok,
        "checks": rows,
        "checked_at": now_ms().to_string(),
    }))
    .into_response()
}

// ---- 测试 ----

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bootstrap::DaemonShared;
    use crate::config::{ConfigShared, GatewayConfig};
    use crate::mgmt::auth_jwt::{now_unix_secs, sign, Claims};
    use crate::mgmt::rbac::Role;
    use crate::mgmt::remote_ops;
    use std::sync::Arc;

    /// 测试种子配置（[gateway] 最小段）。
    const SEED_TOML: &str = r#"
[gateway]
gateway_id = "gw-ops-test"
data_dir = "./data"
"#;

    /// 构造绑定临时配置文件的 MgmtState（备份策略端点共用装配口径）。
    fn make_state(dir: &tempfile::TempDir) -> (MgmtState, std::path::PathBuf) {
        let path = dir.path().join("config.toml");
        std::fs::write(&path, SEED_TOML).expect("seed config");
        let config = Arc::new(GatewayConfig::load(&path).expect("load"));
        let daemon = DaemonShared::new();
        daemon.set_config(Arc::new(ConfigShared::new((*config).clone())));
        let state = MgmtState::new(daemon, config).with_config_path(&path);
        remote_ops::install(&state, Arc::new(remote_ops::DenyAllOpsAuthorizer));
        (state, path)
    }

    /// 以 state 的实际签名密钥签发测试 token。
    fn token_for(state: &MgmtState, role: Role) -> String {
        let now = now_unix_secs();
        let claims = Claims {
            sub: "ops-admin".to_string(),
            role: role.as_str().to_string(),
            perms: None,
            exp: now + 600,
            iat: now,
            nbf: None,
            jti: "test-jti-ops".to_string(),
        };
        sign(&claims, state.auth().key()).expect("sign test token")
    }

    /// 在 127.0.0.1 随机端口启动 axum 服务（本机回环）。
    async fn spawn_server(state: MgmtState) -> u16 {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let port = listener.local_addr().expect("addr").port();
        tokio::spawn(async move {
            axum::serve(listener, crate::mgmt::router(state))
                .await
                .expect("serve error");
        });
        port
    }

    /// 解析原始 HTTP 响应 → (状态码, body)。
    fn parse_response(raw: &str) -> (u16, String) {
        let (head, body) = raw.split_once("\r\n\r\n").expect("header/body separator");
        let status: u16 = head
            .lines()
            .next()
            .and_then(|l| l.split_whitespace().nth(1))
            .and_then(|s| s.parse().ok())
            .expect("status code");
        (status, body.to_string())
    }

    /// 手写 HTTP 请求（3s 超时防挂死）。
    async fn http(
        port: u16,
        method: &str,
        path: &str,
        body: Option<&str>,
        token: Option<&str>,
    ) -> (u16, String) {
        tokio::time::timeout(std::time::Duration::from_secs(3), async move {
            let mut stream = tokio::net::TcpStream::connect(("127.0.0.1", port))
                .await
                .expect("connect");
            let body = body.unwrap_or("");
            let auth = token
                .map(|t| format!("Authorization: Bearer {t}\r\n"))
                .unwrap_or_default();
            let request = if method == "GET" {
                format!("GET {path} HTTP/1.1\r\nHost: 127.0.0.1\r\n{auth}Connection: close\r\n\r\n")
            } else {
                format!(
                    "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1\r\n{auth}Content-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                )
            };
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            stream.write_all(request.as_bytes()).await.expect("write");
            stream.flush().await.expect("flush");
            let mut buf = Vec::new();
            stream.read_to_end(&mut buf).await.expect("read");
            parse_response(&String::from_utf8(buf).expect("utf8"))
        })
        .await
        .expect("http timed out")
    }

    /// QA Happy: GET 默认策略（读开放 + 数字字符串红线 + source=config）；
    /// PUT 落盘往返 + GET 立即反映。
    #[tokio::test]
    async fn backup_policy_get_defaults_and_put_roundtrip() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (state, path) = make_state(&dir);
        let token = token_for(&state, Role::System);
        let port = spawn_server(state.clone()).await;

        // 默认策略（旧配置无 [settings] 段）。
        let (status, body) = http(port, "GET", "/api/settings/backup-policy", None, None).await;
        assert_eq!(status, 200, "{body}");
        let value: Value = serde_json::from_str(&body).expect("json");
        assert_eq!(value["auto_before_write"], true);
        assert_eq!(value["retention_count"], "20", "大数红线：字符串编码");
        assert_eq!(value["interval_min"], "0");
        assert_eq!(value["source"], "config");

        // PUT：显式覆盖（reason 必填）。
        let (status, body) = http(
            port,
            "PUT",
            "/api/settings/backup-policy",
            Some(
                r#"{"auto_before_write":true,"retention_count":"5","interval_min":"30","reason":"tighten retention","note":"ops request"}"#,
            ),
            Some(&token),
        )
        .await;
        assert_eq!(status, 200, "{body}");
        let value: Value = serde_json::from_str(&body).expect("json");
        assert_eq!(value["accepted"], true);
        assert!(value["config_version"].is_string(), "大数红线");
        assert_eq!(value["backup_policy"]["retention_count"], "5");
        assert_eq!(value["backup_policy"]["interval_min"], "30");

        // 落盘往返：重读配置文件语义一致。
        let reloaded = GatewayConfig::load(&path).expect("reload");
        assert_eq!(reloaded.settings.backup_policy.retention_count, 5);
        assert_eq!(reloaded.settings.backup_policy.interval_min, 30);
        assert!(reloaded.settings.backup_policy.auto_before_write);

        // GET 立即反映热快照。
        let (status, body) = http(port, "GET", "/api/settings/backup-policy", None, None).await;
        assert_eq!(status, 200);
        let value: Value = serde_json::from_str(&body).expect("json");
        assert_eq!(value["retention_count"], "5");
    }

    /// QA Error: 未知字段 / 缺 reason / 非法数字 → 400；ops 角色 → 403；
    /// 未携带 token → 401；全部失败路径零落盘。
    #[tokio::test]
    async fn backup_policy_rejects_invalid_input_and_unauthorized() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (state, path) = make_state(&dir);
        let system = token_for(&state, Role::System);
        let ops = token_for(&state, Role::Ops);
        let port = spawn_server(state.clone()).await;

        // 未知字段。
        let (status, body) = http(
            port,
            "PUT",
            "/api/settings/backup-policy",
            Some(r#"{"reason":"x","hacker":"true"}"#),
            Some(&system),
        )
        .await;
        assert_eq!(status, 400, "{body}");

        // reason 缺失 / 空白 → 400（必填进审计）。
        for body_raw in [r#"{"retention_count":"5"}"#, r#"{"reason":"   "}"#] {
            let (status, body) = http(
                port,
                "PUT",
                "/api/settings/backup-policy",
                Some(body_raw),
                Some(&system),
            )
            .await;
            assert_eq!(status, 400, "{body_raw}: {body}");
        }

        // 非法数字 / 超界 / 类型错 → 400。
        for body_raw in [
            r#"{"reason":"x","retention_count":"abc"}"#,
            r#"{"reason":"x","retention_count":"99999999999"}"#,
            r#"{"reason":"x","interval_min":true}"#,
            r#"{"reason":"x","auto_before_write":"yes"}"#,
        ] {
            let (status, _) = http(
                port,
                "PUT",
                "/api/settings/backup-policy",
                Some(body_raw),
                Some(&system),
            )
            .await;
            assert_eq!(status, 400, "{body_raw}");
        }

        // ops（不持 device.write）→ 403。
        let (status, body) = http(
            port,
            "PUT",
            "/api/settings/backup-policy",
            Some(r#"{"reason":"x"}"#),
            Some(&ops),
        )
        .await;
        assert_eq!(status, 403, "{body}");

        // 无 token → 401。
        let (status, _) = http(
            port,
            "PUT",
            "/api/settings/backup-policy",
            Some(r#"{"reason":"x"}"#),
            None,
        )
        .await;
        assert_eq!(status, 401);

        // 全部失败路径零落盘。
        let reloaded = GatewayConfig::load(&path).expect("reload");
        assert_eq!(
            reloaded.settings.backup_policy.retention_count, 20,
            "no disk change"
        );
    }

    /// QA（autostart PUT 本地分支）：401 无 token / 403 ops 角色无
    /// OpsCollectors / 400 缺 enable / 400 非布尔 / 400 非 JSON。
    /// **只测不触注册表的分支**（enable 合法路径由真机 curl 验收并清理现场）。
    #[tokio::test]
    async fn autostart_put_local_branches() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (state, _path) = make_state(&dir);
        let system = token_for(&state, Role::System);
        let ops = token_for(&state, Role::Ops);
        let port = spawn_server(state.clone()).await;

        // 401：无 token。
        let (status, _) = http(
            port,
            "PUT",
            "/api/service/autostart",
            Some(r#"{"enable":true}"#),
            None,
        )
        .await;
        assert_eq!(status, 401);

        // 403：ops 角色无 OpsCollectors（仅 system）。
        let (status, body) = http(
            port,
            "PUT",
            "/api/service/autostart",
            Some(r#"{"enable":true}"#),
            Some(&ops),
        )
        .await;
        assert_eq!(status, 403, "{body}");

        // 400：缺 enable。
        let (status, body) = http(
            port,
            "PUT",
            "/api/service/autostart",
            Some(r#"{"reason":"x"}"#),
            Some(&system),
        )
        .await;
        assert_eq!(status, 400, "{body}");
        assert!(body.contains("missing required field"));

        // 400：enable 非布尔。
        let (status, body) = http(
            port,
            "PUT",
            "/api/service/autostart",
            Some(r#"{"enable":"yes"}"#),
            Some(&system),
        )
        .await;
        assert_eq!(status, 400, "{body}");
        assert!(body.contains("must be a boolean"));

        // 400：非 JSON body。
        let (status, _) = http(
            port,
            "PUT",
            "/api/service/autostart",
            Some("not-json"),
            Some(&system),
        )
        .await;
        assert_eq!(status, 400);

        // GET 形状守卫：write_supported 已翻转为 true（Windows 下）。
        let (status, body) = http(port, "GET", "/api/service/autostart", None, Some(&system)).await;
        assert_eq!(status, 200, "{body}");
        let value: Value = serde_json::from_str(&body).expect("json");
        assert_eq!(value["supported"], true);
        assert_eq!(value["write_supported"], true);
    }

    /// QA（retention 实证）: 造 25 个假 `.bak-*`（自产前缀）→ PUT retention=20
    /// → 响应 pruned="6"（25 旧 + 1 新写前备份 = 26 → 删 6 留 20）；
    /// 用户自建前缀文件不动。
    #[tokio::test]
    async fn backup_policy_put_enforces_retention() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (state, _path) = make_state(&dir);
        for i in 0..25 {
            std::fs::write(dir.path().join(format!("config.toml.bak-{i:020}")), b"old")
                .expect("seed bak");
        }
        std::fs::write(dir.path().join("config.toml.userbak"), b"keep").expect("seed user file");
        let token = token_for(&state, Role::System);
        let port = spawn_server(state.clone()).await;

        let (status, body) = http(
            port,
            "PUT",
            "/api/settings/backup-policy",
            Some(r#"{"reason":"enforce retention","retention_count":"20"}"#),
            Some(&token),
        )
        .await;
        assert_eq!(status, 200, "{body}");
        let value: Value = serde_json::from_str(&body).expect("json");
        assert_eq!(
            value["pruned"], "6",
            "25 old + 1 fresh write-backup = 26 → prune 6 (大数红线: 字符串)"
        );

        let remaining: Vec<_> = std::fs::read_dir(dir.path())
            .expect("read_dir")
            .filter_map(|e| e.ok())
            .filter_map(|e| e.file_name().to_str().map(ToString::to_string))
            .filter(|n| n.starts_with("config.toml.bak-"))
            .collect();
        assert_eq!(remaining.len(), 20, "exactly 20 backups remain");
        assert!(
            dir.path().join("config.toml.userbak").exists(),
            "user files never touched"
        );
    }
}

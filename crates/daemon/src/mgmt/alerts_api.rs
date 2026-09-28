//! 告警与告警规则端点（`GET /api/alerts`、`GET /api/alerts/rules`、
//! `PUT /api/alerts/rules`、`PUT/DELETE /api/alerts/rules/:id`、
//! `POST /api/alerts/:id/ack`）——web-console「告警中心」页（`/alarms`）的
//! 冻结契约。
//!
//! ## 端点
//! | 方法 · 路径 | handler | 语义 |
//! | --- | --- | --- |
//! | `GET    /api/alerts`          | [`list`]         | 真实告警记录（引擎产出；无告警即空数组） |
//! | `GET    /api/alerts/rules`    | [`rules_list`]   | 读回 `[alarms].rules` 全量行 |
//! | `PUT    /api/alerts/rules`    | [`rules_put`]    | **整体保存**（`rules` 即全量，非增量合并） |
//! | `PUT    /api/alerts/rules/:id`| [`rule_update`]  | 可选子集部分更新（含启停 `enabled`） |
//! | `DELETE /api/alerts/rules/:id`| [`rule_remove`]  | 删除（`reason` 须为枚举原文） |
//! | `POST   /api/alerts/:id/ack`  | [`ack`]          | 处置（ `open` → `acking` / `resolved`） |
//!
//! ## 前端实际会打到后端的只有一条写接口
//!
//! `AlarmsPage.vue` 里 `toggleRule`（`:923`）与 `submitDelete`（`:950`）**不发
//! HTTP**（页面自己回一句「后端未提供接口」）。唯一真正发出去的写请求是
//! `saveRule`（`:901`）——`PUT /api/alerts/rules`，按钮是普通按钮，**不走
//! `DangerConfirmModal`，不带 `reason` / `note` / `confirm`**。
//!
//! 由此产生一处**刻意的不对称**（已如实上报 team-lead，可裁决）：
//!
//! - `PUT /api/alerts/rules`：四要素**可选但一旦携带即严格校验**。强制必填会让
//!   前端每一次「保存规则」都 400——这正是本 ticket 要消掉的用户可见故障；
//! - `PUT/DELETE /api/alerts/rules/:id` 与 `POST /api/alerts/:id/ack`：前端当前
//!   **没有**调用方（按钮不发请求），因此在这里**强制**四要素，不留口子。
//!
//! ## 落盘前的语义校验（与转发规则同一条红线）
//!
//! 告警规则必须在**落盘的那一份**上验过「能被引擎求值」——解析不出阈值 / 运算
//! 符 / 数值的规则，是一条把样本喂给引擎也永远不会响的死规则，让它落盘只会让
//! 用户在页面上看到「规则已保存」而告警中心永远空着（比直接报错更坏）。
//! 校验失败一律 400，**不落盘**。
//!
//! ## 诚实限制
//! - 告警记录来自 [`crate::alarm::AlarmStore`]（**进程内 historian**）：进程
//!   重启即清空。这里如实标注，不假装持久化；
//! - 规则行上的 `hit_count` 恒 0：规则是配置，命中记账在告警记录上
//!   （`AlarmRecord::count`），不另造第二个口径。

use axum::body::Bytes;
use axum::extract::{Path as AxumPath, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Json, Response};
use serde::Deserialize;
use serde_json::{json, Value};

use super::rbac::{AuthedRole, Permission};
use super::remote_ops::{OpsAction, OUTCOME_ACCEPTED, OUTCOME_BAD_REQUEST, OUTCOME_DENIED};
use super::writeapi::{audit, not_found, validation_error, write_guard};
use super::{pages::persist_config, MgmtEvent, MgmtState};
use crate::alarm::{record_to_wire, ALARM_LEVELS};
use crate::config::{AlarmRuleConfig, AlarmsSection, GatewayConfig};

/// 删除原因取值域（与 `AlarmsPage.vue:366-371` 的 `DELETE_REASONS` **逐字一致**）。
const DELETE_REASONS: &[&str] = &[
    "规则条件已过时（阈值/点位变更）",
    "与其它规则重复",
    "误配置",
    "业务调整，不再需要",
];

/// `note` 最小字数（与 `DangerConfirmModal` 的 `min-note-length` 一致）。
const MIN_NOTE_LEN: usize = 10;

/// 规则名最大长度（展示 + 二次确认回显长度）。
const MAX_RULE_NAME_LEN: usize = 128;

/// 单条规则 id 最大长度。
const MAX_RULE_ID_LEN: usize = 128;

/// 自动补齐 id 的前缀（`AlarmsPage` 保存时不带 `id`，后端负责生成稳定主键）。
const AUTO_ID_PREFIX: &str = "alarm";

// ---- 请求体 ----

/// 单条告警规则的可写字段（`PUT /api/alerts/rules` 的 `rules[]` 元素）。
///
/// 字段命名**照抄前端 `AlarmsPage.vue:903-912` 的下发形状**（`condition` /
/// `duration_sec` / `suppress_min` / `target`），后端只做单位换算与语义补全；
/// 结构化写法（`op` + `threshold` + `point_id`）同时兼容，供配置直接编辑。
#[derive(Debug, Default, Deserialize)]
struct AlarmRuleUpsert {
    /// 规则主键（省略 = 后端生成稳定 id）。
    #[serde(default)]
    id: String,
    /// 规则名称（省略 = 回退 `id`）。
    #[serde(default)]
    name: String,
    /// 条件原文（前端主字段；如 `[T_Barrel1] > 240`）。
    #[serde(default)]
    condition: String,
    /// 告警级别（省略 = `minor`）。
    #[serde(default)]
    level: String,
    /// 适用设备（`AlarmsPage` 的 `target`；空 = 全部设备）。
    #[serde(default)]
    target: String,
    /// 适用点位（结构化写法；省略时从 `condition` 的 `[...]` 里取）。
    #[serde(default)]
    point_id: String,
    /// 比较运算符（结构化写法；省略时从 `condition` 里解析）。
    #[serde(default)]
    op: String,
    /// 阈值（结构化写法）。
    #[serde(default)]
    threshold: Option<f64>,
    /// 持续时间（**秒**；前端口径）。
    #[serde(default)]
    duration_sec: Option<f64>,
    /// 抑制窗口（**分钟**；前端口径）。
    #[serde(default)]
    suppress_min: Option<f64>,
    /// 是否启用（省略 = 不改）。
    #[serde(default)]
    enabled: Option<bool>,
}

/// `PUT /api/alerts/rules` 请求体（**整体保存**：`rules` 即期望的全量集合）。
#[derive(Debug, Deserialize)]
struct RulesPutBody {
    /// 期望的全量规则（空数组 = 清空全部告警规则）。
    #[serde(default)]
    rules: Vec<AlarmRuleUpsert>,
    /// 危险操作三要素（可选，携带即严格校验；见模块文档）。
    #[serde(default)]
    reason: String,
    #[serde(default)]
    note: String,
    #[serde(default)]
    confirm: String,
}

/// `PUT /api/alerts/rules/:id` 请求体（**可选子集**部分更新）。
#[derive(Debug, Default, Deserialize)]
struct RuleUpdateBody {
    #[serde(default)]
    name: String,
    #[serde(default)]
    condition: String,
    #[serde(default)]
    level: String,
    #[serde(default)]
    target: String,
    #[serde(default)]
    point_id: String,
    #[serde(default)]
    op: String,
    #[serde(default)]
    threshold: Option<f64>,
    #[serde(default)]
    duration_sec: Option<f64>,
    #[serde(default)]
    suppress_min: Option<f64>,
    #[serde(default)]
    enabled: Option<bool>,
    #[serde(default)]
    reason: String,
    #[serde(default)]
    note: String,
    #[serde(default)]
    confirm: String,
}

/// `POST /api/alerts/:id/ack` 请求体（告警处置）。
#[derive(Debug, Default, Deserialize)]
struct AckBody {
    /// 目标状态（`acking` / `resolved`）。
    #[serde(default)]
    state: String,
    /// 处置说明（写入 `AlarmRecord::note`）。
    #[serde(default)]
    note: String,
    #[serde(default)]
    reason: String,
    #[serde(default)]
    confirm: String,
}

/// 危险操作三要素（`reason` / `note` / `confirm` **独立字段**，禁止拼接）。
#[derive(Debug, Default, Deserialize)]
struct DangerTrio {
    #[serde(default)]
    reason: String,
    #[serde(default)]
    note: String,
    #[serde(default)]
    confirm: String,
}

// ---- 四要素校验 ----

/// `reason`：必填；`enum_only` = 必须在 [`DELETE_REASONS`] 内取原文。
#[allow(clippy::result_large_err)]
fn check_reason(reason: &str, enum_only: bool) -> Result<(), Response> {
    let text = reason.trim();
    if text.is_empty() {
        return Err(validation_error(
            "reason",
            "reason is required",
            "non-empty change reason",
        ));
    }
    if enum_only && !DELETE_REASONS.contains(&text) {
        return Err(validation_error(
            "reason",
            &format!("unknown reason {reason:?}"),
            &DELETE_REASONS.join(" | "),
        ));
    }
    Ok(())
}

/// `note`：独立补充说明（**绝不与 `reason` 拼接**）；非空则 ≥ [`MIN_NOTE_LEN`] 字。
#[allow(clippy::result_large_err)]
fn check_note(note: &str) -> Result<(), Response> {
    let text = note.trim();
    if text.is_empty() {
        return Ok(());
    }
    let len = text.chars().count();
    if len < MIN_NOTE_LEN {
        return Err(validation_error(
            "note",
            &format!("note must be at least {MIN_NOTE_LEN} characters ({len} given)"),
            &format!(
                "≥{MIN_NOTE_LEN} characters, kept independent from `reason` (never append it to `reason`)"
            ),
        ));
    }
    Ok(())
}

/// `confirm`：必填（对象全名二次确认的输入侧）。
#[allow(clippy::result_large_err)]
fn check_confirm_present(confirm: &str) -> Result<(), Response> {
    if confirm.trim().is_empty() {
        return Err(validation_error(
            "confirm",
            "confirm is required (echo the full object name)",
            "the full object name",
        ));
    }
    Ok(())
}

/// `confirm` 回显值不匹配：`confirm` 必须逐字等于对象**全名**。
fn confirm_mismatch(expected: &str) -> Response {
    (
        StatusCode::BAD_REQUEST,
        Json(json!({
            "error": "confirm_mismatch",
            "field": "confirm",
            "reason": format!("confirm does not match {expected:?}"),
            "allowed": expected,
        })),
    )
        .into_response()
}

/// 三要素的完整校验（**先于** id 定位 / 落盘等一切业务分支）。
#[allow(clippy::result_large_err)]
fn check_trio(danger: &DangerTrio, enum_only: bool, confirm_target: &str) -> Result<(), Response> {
    check_reason(&danger.reason, enum_only)?;
    check_note(&danger.note)?;
    check_confirm_present(&danger.confirm)?;
    // 大数 / 大小写口径：`trim()` + 大小写不敏感精确匹配（前端 gateway_id / 对象名
    // 可能带空格或大小写差异，只做 `==` 会让正常回显被误判为 mismatch）。
    if !danger.confirm.trim().eq_ignore_ascii_case(confirm_target) {
        return Err(confirm_mismatch(confirm_target));
    }
    Ok(())
}

/// 从已解析的「整体保存」请求体里取出三要素（同一份 body 不二次解析，
/// 三个字段各归各位，**严禁拼进 `reason`**）。
fn trio_of(body: &RulesPutBody) -> DangerTrio {
    DangerTrio {
        reason: body.reason.trim().to_string(),
        note: body.note.trim().to_string(),
        confirm: body.confirm.trim().to_string(),
    }
}

// ---- 规则视图 ----

/// 配置行 → wire（配置侧 `enabled` 可能是 false，如实回显）。
fn config_to_wire(rule: &AlarmRuleConfig) -> Value {
    let condition = rule
        .condition
        .clone()
        .filter(|text| !text.trim().is_empty())
        .unwrap_or_else(|| rule.id.clone());
    json!({
        "id": rule.id,
        "name": rule.name.clone().unwrap_or_else(|| rule.id.clone()),
        "condition": condition,
        "level": rule.level.clone().unwrap_or_else(|| "minor".to_string()),
        "source_type": rule.source_type.clone().unwrap_or_else(|| "device".to_string()),
        "target": rule.device_id,
        "point_id": rule.point_id,
        "op": rule.op.clone().unwrap_or_default(),
        "threshold": rule.threshold,
        "hit_count": 0,
        "duration_sec": rule.duration_ms.unwrap_or(0) as f64 / 1000.0,
        "duration_ms": rule.duration_ms.unwrap_or(0),
        "suppress_min": rule.suppress_ms.unwrap_or(0) as f64 / 60_000.0,
        "suppress_ms": rule.suppress_ms.unwrap_or(0),
        "enabled": rule.enabled,
    })
}

// ---- 规则装配（写路径的唯一真相构造处） ----

/// 单条 upsert → 配置行；返回 `Err` 时 **400 且不落盘**。
#[allow(clippy::result_large_err)]
fn alarm_rule_from_upsert(
    raw: &AlarmRuleUpsert,
    index: usize,
    existing_ids: &[String],
) -> Result<AlarmRuleConfig, Response> {
    let label = format!("rules[{index}]");
    let id = raw.id.trim().to_string();
    let id = if id.is_empty() {
        next_auto_id(existing_ids)
    } else {
        if id.len() > MAX_RULE_ID_LEN {
            return Err(validation_error(
                "id",
                &format!("{label} id exceeds {MAX_RULE_ID_LEN} characters"),
                "a short, stable rule id",
            ));
        }
        if existing_ids.iter().any(|known| known == &id) {
            return Err(validation_error(
                "id",
                &format!("{label} {id:?} already exists"),
                "unique rule id",
            ));
        }
        id
    };
    // 语义校验：条件必须能被引擎求值（解析不出阈值 / 运算符 / 数值 → 死规则）。
    let parsed = match crate::alarm::parse_condition(&raw.condition) {
        Ok(parsed) => Some(parsed),
        Err(err) if raw.op.trim().is_empty() && raw.threshold.is_none() => {
            return Err(validation_error(
                "condition",
                &format!("{label}: {err}"),
                "[POINT] OP NUMBER，例如 [T_Barrel1] > 240（或显式下发 op + threshold）",
            ));
        }
        Err(_) => None,
    };
    let op = crate::alarm::parse_op(&raw.op)
        .or_else(|| parsed.as_ref().map(|parsed| parsed.op))
        .ok_or_else(|| {
            validation_error(
                "op",
                &format!("{label}: unknown comparison operator {:#?}", raw.op),
                "gt | ge | lt | le | eq | ne",
            )
        })?;
    let threshold = raw.threshold.filter(|value| value.is_finite()).or_else(|| {
        parsed
            .as_ref()
            .map(|parsed| parsed.threshold)
            .filter(|value| value.is_finite())
    });
    let Some(threshold) = threshold else {
        return Err(validation_error(
            "threshold",
            &format!("{label}: threshold is required and must be numeric"),
            "a finite number, e.g. 240",
        ));
    };
    let level = raw.level.trim().to_string();
    if !level.is_empty() && !ALARM_LEVELS.contains(&level.as_str()) {
        return Err(validation_error(
            "level",
            &format!("{label}: unknown level {level:?}"),
            &ALARM_LEVELS.join(" | "),
        ));
    }
    let (duration_ms, suppress_ms) = durations(raw)?.unwrap_or((0, 0));
    let name = raw.name.trim().to_string();
    if name.len() > MAX_RULE_NAME_LEN {
        return Err(validation_error(
            "name",
            &format!("{label} name exceeds {MAX_RULE_NAME_LEN} characters"),
            "a short rule name (used as the danger-confirm echo)",
        ));
    }
    Ok(AlarmRuleConfig {
        id,
        name: if name.is_empty() { None } else { Some(name) },
        level: if level.is_empty() { None } else { Some(level) },
        source_type: None,
        condition: if raw.condition.trim().is_empty() {
            None
        } else {
            Some(raw.condition.trim().to_string())
        },
        threshold: Some(threshold),
        device_id: Some(raw.target.trim().to_string())
            .filter(|value| !value.trim().is_empty())
            .map(|value| value.trim().to_string()),
        point_id: Some(raw.point_id.trim().to_string())
            .filter(|value| !value.trim().is_empty())
            .map(|value| value.trim().to_string()),
        op: Some(op.as_str().to_string()),
        duration_ms: Some(duration_ms),
        suppress_ms: Some(suppress_ms),
        enabled: raw.enabled.unwrap_or(true),
    })
}

/// 秒 / 分钟 → 毫秒；负数一律 400（负时长不是「立即」，是无意义值）。
#[allow(clippy::result_large_err)]
fn durations(raw: &AlarmRuleUpsert) -> Result<Option<(u64, u64)>, Response> {
    let secs = raw.duration_sec.unwrap_or(0.0);
    let mins = raw.suppress_min.unwrap_or(0.0);
    if !secs.is_finite() || secs < 0.0 {
        return Err(validation_error(
            "duration_sec",
            &format!("duration_sec must be a non-negative number ({secs})"),
            "seconds, e.g. 0 for immediate firing",
        ));
    }
    if !mins.is_finite() || mins < 0.0 {
        return Err(validation_error(
            "suppress_min",
            &format!("suppress_min must be a non-negative number ({mins})"),
            "minutes, e.g. 0 for no suppression",
        ));
    }
    Ok(Some((
        (secs * 1000.0).round() as u64,
        (mins * 60_000.0).round() as u64,
    )))
}

/// 自动生成下一个稳定 id（`alarm-1` / `alarm-2` …，避开已占用的）。
fn next_auto_id(existing_ids: &[String]) -> String {
    let mut seq = existing_ids.len() + 1;
    while existing_ids
        .iter()
        .any(|known| known == &format!("{AUTO_ID_PREFIX}-{seq}"))
    {
        seq += 1;
    }
    format!("{AUTO_ID_PREFIX}-{seq}")
}

/// 校验一份「告警规则集」能被引擎装配（不落盘，只做可用性断言）。
#[allow(clippy::result_large_err)]
fn validate_alarm_rule_set(rules: &[AlarmRuleConfig]) -> Result<(), Response> {
    for rule in rules {
        if let Err(err) = crate::alarm::rule_from_config(rule) {
            return Err(validation_error(
                "rules",
                &format!("rule {}: {}", rule.id, err),
                "condition must be evaluable, e.g. [POINT] > 240 (or explicit op + threshold)",
            ));
        }
    }
    Ok(())
}

// ---- 读 ----

/// `GET /api/alerts` → 真实告警记录（引擎产出）。
///
/// 数据源 = [`crate::alarm::AlarmStore`]（数据面每拍写入的真实记录）。**没有任何
/// 「拿不到就回退假数据」的分支**：没配告警规则 / 无样本触发 / 进程刚重启，就是
/// `items: []` + `source: "ok"`（前端据此呈现「后端暂无告警数据」，不歪曲成
/// 「告警不可用」）。
pub async fn list(State(state): State<MgmtState>, authed: AuthedRole) -> Response {
    if let Err(rejection) = authed.ensure(Permission::DeviceView) {
        return rejection.into_response();
    }
    let rows = state.daemon().alarms_store().snapshot();
    let items: Vec<Value> = rows.iter().map(record_to_wire).collect();
    Json(json!({
        "items": items,
        // 大数红线：计数类一律字符串。
        "total": items.len().to_string(),
        "source": "ok",
        "reason": "",
    }))
    .into_response()
}

/// `GET /api/alerts/rules` → 告警规则清单（配置真相源 `[alarms].rules`）。
pub async fn rules_list(State(state): State<MgmtState>, authed: AuthedRole) -> Response {
    if let Err(rejection) = authed.ensure(Permission::DeviceView) {
        return rejection.into_response();
    }
    let config = state.config();
    let items: Vec<Value> = match config.alarms.as_ref() {
        Some(section) => section.rules.iter().map(config_to_wire).collect(),
        None => Vec::new(),
    };
    Json(json!({
        "items": items,
        "total": items.len().to_string(),
        "source": "ok",
    }))
    .into_response()
}

// ---- 写 ----

/// `PUT /api/alerts/rules` → **整体保存**（`rules` 即期望的全量集合）。
///
/// - `rules: []` = 清空全部告警规则（合法；`alerts` 段随之无规则可评估）；
/// - 四要素：可选但一旦携带即严格校验（见模块文档「刻意的不对称」）；
/// - 任何一条不可用 → **整批拒绝，零落盘**（半写的规则集比没有规则更坏：页面
///   显示「已保存」而告警中心永远空着）。
pub async fn rules_put(
    State(state): State<MgmtState>,
    authed: AuthedRole,
    body: Bytes,
) -> Response {
    if let Err(rejection) = authed.ensure(Permission::DeviceWrite) {
        audit(
            &state,
            &authed.claims.sub,
            OpsAction::AlarmRulesWrite,
            false,
            OUTCOME_DENIED,
            &rejection.to_string(),
        );
        return rejection.into_response();
    }
    let actor = authed.claims.sub.clone();
    let req: RulesPutBody = match serde_json::from_slice::<RulesPutBody>(&body) {
        Ok(v) => v,
        Err(err) => {
            audit(
                &state,
                &actor,
                OpsAction::AlarmRulesWrite,
                true,
                OUTCOME_BAD_REQUEST,
                &format!("malformed json body: {err}"),
            );
            return validation_error(
                "body",
                &format!("malformed JSON: {err}"),
                "object {rules: [{name?, condition, level?, target?, point_id?, op?, threshold?, duration_sec?, suppress_min?, enabled?}], reason?, note?, confirm?}",
            );
        }
    };
    // 三要素（携带即校验）：不强制必填的原因见模块文档——前端「保存规则」按钮
    // 是普通按钮，不走 DangerConfirmModal，强制必填会让每次保存都 400。
    let trio = trio_of(&req);
    if !trio.reason.is_empty() || !trio.note.is_empty() || !trio.confirm.is_empty() {
        if let Err(resp) = check_reason(&trio.reason, false) {
            return resp;
        }
        if let Err(resp) = check_note(&trio.note) {
            return resp;
        }
        if let Err(resp) = check_confirm_present(&trio.confirm) {
            return resp;
        }
        // `confirm` 的回显目标 = 本次将被**覆盖**的全部规则名（整体保存 = 破坏性）。
        // 既存表为空时**没有可回显的对象**：若仍死盯空表，写入侧会永远
        // `confirm_mismatch`，首次启用告警时一条规则都建不起来（自锁）。此时
        // 把回显目标退化为本次提交的规则名——确认语义仍是「我清楚这批规则会
        // 整体覆盖」。
        let expected: Vec<String> = state
            .config()
            .alarms
            .as_ref()
            .map(|section| {
                section
                    .rules
                    .iter()
                    .map(|rule| rule.name.clone().unwrap_or_else(|| rule.id.clone()))
                    .collect()
            })
            .unwrap_or_default();
        let targets: Vec<String> = if expected.is_empty() {
            req.rules
                .iter()
                .map(|rule| rule.name.trim().to_string())
                .filter(|name| !name.is_empty())
                .collect()
        } else {
            expected
        };
        let text = trio.confirm.trim();
        if !targets
            .iter()
            .any(|name| name.trim().eq_ignore_ascii_case(text))
        {
            return confirm_mismatch(&targets.join(" / "));
        }
    }

    let existing_ids: Vec<String> = state
        .config()
        .alarms
        .as_ref()
        .map(|section| section.rules.iter().map(|rule| rule.id.clone()).collect())
        .unwrap_or_default();
    let mut rules: Vec<AlarmRuleConfig> = Vec::with_capacity(req.rules.len());
    for (index, raw) in req.rules.iter().enumerate() {
        match alarm_rule_from_upsert(raw, index, &existing_ids) {
            Ok(rule) => rules.push(rule),
            Err(resp) => return resp,
        }
    }
    if !rules.is_empty() {
        if let Err(resp) = validate_alarm_rule_set(&rules) {
            return resp;
        }
    }

    let _guard = write_guard();
    let config = (*state.config()).clone();
    let mut next = config.clone();
    next.alarms = Some(AlarmsSection {
        // 至少保存过一条启用规则即视为「用户要告警」；清空规则时不擅自把总开关
        // 关掉（否则下次加规则还得先手动打开 `[alarms].enabled`）。
        enabled: config
            .alarms
            .as_ref()
            .is_some_and(|section| section.enabled)
            || rules.iter().any(|rule| rule.enabled),
        rules: rules.clone(),
    });

    match persist_config(
        &state,
        next,
        &actor,
        OpsAction::AlarmRulesWrite,
        &format!(
            "replace alarm rule set ({} rule(s), reason={:?})",
            rules.len(),
            trio.reason.trim()
        ),
    ) {
        Ok(version) => {
            state.publish(MgmtEvent::ConfigReloaded { version });
            (
                StatusCode::OK,
                Json(json!({
                    "ok": true,
                    "replaced": rules.len(),
                    "items": rules.iter().map(config_to_wire).collect::<Vec<Value>>(),
                    "config_version": version.to_string(),
                })),
            )
                .into_response()
        }
        Err(resp) => resp,
    }
}

/// `PUT /api/alerts/rules/:id` → 可选子集部分更新（含启停 `enabled`）。
///
/// 只下发 `{enabled, reason, note, confirm}` = 改启用状态；未知 id → 404；
/// `confirm` 必须回显规则全名。空更新（只有三要素、无可更新字段）如实返回
/// `{"ok":true,"updated":false}`，不假装改了。
pub async fn rule_update(
    State(state): State<MgmtState>,
    AxumPath(id): AxumPath<String>,
    authed: AuthedRole,
    body: Bytes,
) -> Response {
    if let Err(rejection) = authed.ensure(Permission::DeviceWrite) {
        audit(
            &state,
            &authed.claims.sub,
            OpsAction::AlarmRuleUpdate,
            false,
            OUTCOME_DENIED,
            &rejection.to_string(),
        );
        return rejection.into_response();
    }
    let actor = authed.claims.sub.clone();
    let req: RuleUpdateBody = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(err) => {
            audit(
                &state,
                &actor,
                OpsAction::AlarmRuleUpdate,
                true,
                OUTCOME_BAD_REQUEST,
                &format!("malformed json body: {err}"),
            );
            return validation_error(
                "body",
                &format!("malformed JSON: {err}"),
                "object {name?, condition?, level?, target?, point_id?, op?, threshold?, duration_sec?, suppress_min?, enabled?, reason, note, confirm}",
            );
        }
    };
    let trio = DangerTrio {
        reason: req.reason.trim().to_string(),
        note: req.note.trim().to_string(),
        confirm: req.confirm.trim().to_string(),
    };
    // 四要素**强制**（前端无调用方，留口子没意义）；回显目标 = 本条规则全名。
    let echo = state
        .config()
        .alarms
        .as_ref()
        .and_then(|section| section.rules.iter().find(|rule| rule.id == id))
        .map(|rule| rule.name.clone().unwrap_or_else(|| rule.id.clone()))
        .unwrap_or_default();
    if let Err(resp) = check_trio(&trio, false, &echo) {
        return resp;
    }
    let _guard = write_guard();
    let config = (*state.config()).clone();
    let Some(section) = config.alarms.clone() else {
        audit(
            &state,
            &actor,
            OpsAction::AlarmRuleUpdate,
            true,
            OUTCOME_BAD_REQUEST,
            "no [alarms] section configured",
        );
        return not_found(&format!("alarm rule {id:?} not found"));
    };
    let index = section.rules.iter().position(|rule| rule.id == id);
    let Some(index) = index else {
        audit(
            &state,
            &actor,
            OpsAction::AlarmRuleUpdate,
            true,
            OUTCOME_BAD_REQUEST,
            &format!("unknown alarm rule {id:?}"),
        );
        return not_found(&format!("alarm rule {id:?} not found"));
    };
    let mut current = section.rules[index].clone();
    // 空串 / 缺省 = 不改动（与 `writeapi` 的点位更新同口径）。
    if let Some(name) = resolved(&req.name) {
        current.name = Some(name);
    }
    if let Some(level) = resolved(&req.level) {
        current.level = Some(level);
    }
    if let Some(condition) = resolved(&req.condition) {
        current.condition = Some(condition.clone());
        // 条件换了新的，判据就得跟着换：配置里残留的旧 `threshold` / `op` 若留在
        // 原地，回显就成了「`condition` 说 90000、`threshold` 说 40000」——页面
        // 显示与引擎判据（[`crate::alarm::rule_from_config`] 的条件原文优先）互
        // 相打脸。**本次显式下发过的 `op` / `threshold` 不动**（那是用户最新意图）。
        if req.op.trim().is_empty() {
            if let Ok(parsed) = crate::alarm::parse_condition(&condition) {
                current.op = Some(parsed.op.as_str().to_string());
                current.threshold = Some(parsed.threshold);
            }
        }
    }
    if let Some(threshold) = req.threshold.filter(|value| value.is_finite()) {
        current.threshold = Some(threshold);
    }
    if let Some(target) = resolved_opt(&req.target) {
        current.device_id = Some(target);
    }
    if let Some(point) = resolved_opt(&req.point_id) {
        current.point_id = Some(point);
    }
    if let Some(op) = resolved(&req.op) {
        current.op = Some(op);
    }
    if let Some(ms) = resolved_u64(req.duration_sec) {
        current.duration_ms = Some(ms);
    }
    if let Some(ms) = resolved_u64(req.suppress_min) {
        current.suppress_ms = Some(ms);
    }
    if let Some(enabled) = req.enabled {
        current.enabled = enabled;
    }
    let updated = current;
    if let Err(resp) = validate_alarm_rule_set(std::slice::from_ref(&updated)) {
        return resp;
    }
    let mut next_section = section;
    next_section.rules[index] = updated.clone();
    let mut next = config.clone();
    next.alarms = Some(next_section);

    match persist_config(
        &state,
        next,
        &actor,
        OpsAction::AlarmRuleUpdate,
        &format!(
            "update alarm rule {} (enabled={}, reason={:?})",
            id,
            updated.enabled,
            trio.reason.trim()
        ),
    ) {
        Ok(version) => {
            state.publish(MgmtEvent::ConfigReloaded { version });
            (
                StatusCode::OK,
                Json(json!({
                    "ok": true,
                    "updated": true,
                    "rule": config_to_wire(&updated),
                    "config_version": version.to_string(),
                })),
            )
                .into_response()
        }
        Err(resp) => resp,
    }
}

/// `DELETE /api/alerts/rules/:id` → 删除告警规则（`reason` 须为枚举原文）。
pub async fn rule_remove(
    State(state): State<MgmtState>,
    AxumPath(id): AxumPath<String>,
    authed: AuthedRole,
    body: Bytes,
) -> Response {
    if let Err(rejection) = authed.ensure(Permission::DeviceWrite) {
        audit(
            &state,
            &authed.claims.sub,
            OpsAction::AlarmRuleDelete,
            false,
            OUTCOME_DENIED,
            &rejection.to_string(),
        );
        return rejection.into_response();
    }
    let actor = authed.claims.sub.clone();
    let trio: DangerTrio = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(err) => {
            audit(
                &state,
                &actor,
                OpsAction::AlarmRuleDelete,
                true,
                OUTCOME_BAD_REQUEST,
                &format!("malformed json body: {err}"),
            );
            return validation_error(
                "body",
                &format!("malformed JSON: {err}"),
                "object {reason, note, confirm}",
            );
        }
    };
    let _guard = write_guard();
    let config = (*state.config()).clone();
    let Some(section) = config.alarms.clone() else {
        audit(
            &state,
            &actor,
            OpsAction::AlarmRuleDelete,
            true,
            OUTCOME_BAD_REQUEST,
            "no [alarms] section configured",
        );
        return not_found(&format!("alarm rule {id:?} not found"));
    };
    let Some(current) = section.rules.iter().find(|rule| rule.id == id).cloned() else {
        audit(
            &state,
            &actor,
            OpsAction::AlarmRuleDelete,
            true,
            OUTCOME_BAD_REQUEST,
            &format!("unknown alarm rule {id:?}"),
        );
        return not_found(&format!("alarm rule {id:?} not found"));
    };
    let expected = current.name.clone().unwrap_or_else(|| current.id.clone());
    // 四要素**强制**且 `reason` 取枚举原文（删除走 `DangerConfirmModal`，
    // 四要素的回显目标就是这条规则的全名）。
    if let Err(resp) = check_trio(&trio, true, &expected) {
        return resp;
    }
    let mut next_section = section;
    next_section.rules.retain(|rule| rule.id != id);
    let mut next = config;
    next.alarms = Some(next_section);

    match persist_config(
        &state,
        next,
        &actor,
        OpsAction::AlarmRuleDelete,
        &format!("delete alarm rule {id:?} (reason={:?})", trio.reason.trim()),
    ) {
        Ok(version) => {
            state.publish(MgmtEvent::ConfigReloaded { version });
            (
                StatusCode::OK,
                Json(json!({
                    "ok": true,
                    "deleted": id,
                    "config_version": version.to_string(),
                })),
            )
                .into_response()
        }
        Err(resp) => resp,
    }
}

/// `POST /api/alerts/:id/ack` → 告警处置（`open` → `acking` / `resolved`）。
///
/// 只改内存态记录（[`crate::alarm::AlarmStore`]），不落配置——告警处置结论
/// 属于运维留痕，不是网关配置。写进审计环，便于事后追责。
pub async fn ack(
    State(state): State<MgmtState>,
    AxumPath(id): AxumPath<String>,
    authed: AuthedRole,
    body: Bytes,
) -> Response {
    if let Err(rejection) = authed.ensure(Permission::DeviceWrite) {
        audit(
            &state,
            &authed.claims.sub,
            OpsAction::AlarmAck,
            false,
            OUTCOME_DENIED,
            &rejection.to_string(),
        );
        return rejection.into_response();
    }
    let actor = authed.claims.sub.clone();
    let req: AckBody = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(err) => {
            audit(
                &state,
                &actor,
                OpsAction::AlarmAck,
                true,
                OUTCOME_BAD_REQUEST,
                &format!("malformed json body: {err}"),
            );
            return validation_error(
                "body",
                &format!("malformed JSON: {err}"),
                "object {state: \"acking\" | \"resolved\", note, reason?, confirm?}",
            );
        }
    };
    let target = req.state.trim().to_string();
    if target != "acking" && target != "resolved" {
        audit(
            &state,
            &actor,
            OpsAction::AlarmAck,
            true,
            OUTCOME_BAD_REQUEST,
            &format!("unknown alarm state {target:?}"),
        );
        return validation_error(
            "state",
            &format!("unknown alarm state {target:?}"),
            "\"acking\" | \"resolved\"",
        );
    }
    let trio = DangerTrio {
        reason: req.reason.trim().to_string(),
        note: req.note.trim().to_string(),
        confirm: req.confirm.trim().to_string(),
    };
    // 告警处置是危险操作：四要素**强制**，不留口子（见模块文档「刻意的不对称」）。
    //
    // confirm 的判定与 `:271` 的 `check_trio` **完全同一套**（`trim()` + 大小写
    // 不敏感精确匹配，不由 `check_trio` 代调的原因是其 `check_confirm_present`
    // 会把空串归为 `validation_failed`，而本端点的契约是空串同样算不匹配、一律
    // `confirm_mismatch`）。上一版写作 `!confirm.is_empty() && confirm.trim() != id`，
    // `!is_empty()` 的短路让空 confirm 直接放行——危险操作的闸门被空值整个拆掉。
    if !trio.confirm.trim().eq_ignore_ascii_case(&id) {
        return confirm_mismatch(&id);
    }
    let store = state.daemon().alarms_store();
    if store.find(&id).is_none() {
        audit(
            &state,
            &actor,
            OpsAction::AlarmAck,
            true,
            OUTCOME_BAD_REQUEST,
            &format!("unknown alarm {id:?}"),
        );
        return not_found(&format!("alarm {id:?} not found"));
    }
    let note = trio.note.trim().to_string();
    match store.set_state(&id, &target, &actor, &note) {
        Some(updated) => {
            audit(
                &state,
                &actor,
                OpsAction::AlarmAck,
                true,
                OUTCOME_ACCEPTED,
                &format!(
                    "alarm {} -> {} (note={:?})",
                    id,
                    target,
                    note.chars().take(32).collect::<String>()
                ),
            );
            (
                StatusCode::OK,
                Json(json!({
                    "ok": true,
                    "alarm": record_to_wire(&updated),
                })),
            )
                .into_response()
        }
        None => {
            audit(
                &state,
                &actor,
                OpsAction::AlarmAck,
                true,
                OUTCOME_BAD_REQUEST,
                &format!("alarm {id:?} vanished before the state change"),
            );
            not_found(&format!("alarm {id:?} not found"))
        }
    }
}

// ---- 小工具 ----

/// 非空字符串 → `Some`（用于「空串 = 不改动」的部分更新语义）。
fn resolved(text: &str) -> Option<String> {
    let text = text.trim();
    if text.is_empty() {
        None
    } else {
        Some(text.to_string())
    }
}

/// 同上，但返回空 `String` 也算「不改动」（设备 / 点位可显式清空）。
fn resolved_opt(text: &str) -> Option<String> {
    Some(text.trim().to_string())
}

/// 秒 / 分钟（可为空）→ 毫秒（`Some` 表示「字段确实下发过」）。
fn resolved_u64(value: Option<f64>) -> Option<u64> {
    match value {
        Some(secs) if secs.is_finite() && secs >= 0.0 => Some((secs * 1000.0).round() as u64),
        _ => None,
    }
}

/// 供测试与文档引用：告警配置段是否被装配（避免 `unused` 警告）。
#[allow(dead_code)]
fn section_is_configured(config: &GatewayConfig) -> bool {
    config.alarms.is_some()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::alarm::AlarmRecord;
    use crate::config::ConfigShared;
    use crate::dataplane::NorthDataPlane;
    use crate::error::DaemonResult;
    use crate::mgmt::health::DeviceHealthRegistry;
    use crate::mgmt::rbac::Role;
    use crate::mgmt::test_support::{
        http_get, http_request, make_state, post_json, put_json, spawn_server, token_for,
    };
    use crate::pipeline::RawSample;
    use crate::scheduler::PollHandler;
    use async_trait::async_trait;
    use serde_json::Value as Json;
    use std::sync::Arc;

    /// 固定产出一个值的采集源：替身 southbound 的解码结果（模拟「这一拍采到 250.0」）。
    struct FixedSource {
        value: f64,
    }

    #[async_trait]
    impl PollHandler for FixedSource {
        async fn poll(&self, _group: &str, point_ids: &[String]) -> DaemonResult<Vec<RawSample>> {
            Ok(point_ids
                .iter()
                .map(|point_id| RawSample {
                    source_id: point_id.clone(),
                    value: self.value,
                    quality: protocol_proto::Quality::Good,
                    device_ts_ns: None,
                })
                .collect())
        }
    }

    /// 用给定配置装配一台数据面（采集源固定产出 `value`）。
    fn plane_with(
        config: &GatewayConfig,
        value: f64,
        store: Arc<crate::alarm::AlarmStore>,
    ) -> NorthDataPlane {
        let (live_tx, _live_rx) = tokio::sync::broadcast::channel(8);
        NorthDataPlane::new(
            Arc::new(FixedSource { value }),
            config,
            Arc::new(ConfigShared::new(config.clone())),
            live_tx,
            Arc::new(DeviceHealthRegistry::new()),
            store,
        )
    }

    /// 「页面建规则 → 采集到点 → 数据面求值 → 告警列表看得到」整条链。
    ///
    /// 这条断言针对的是**接线**而不是判定逻辑：`alarm.rs` 里的单测证明的是
    /// 「样本 + 规则 → 该不该报警」，这里证明的是「HTTP 建出来的规则真的进了
    /// 数据面的求值循环，并且产出能在 `GET /api/alerts` 上被看到」——也就是
    /// 「引擎不是空跑」的那一段。末尾的反向对照（同一份样本、规则还没建时
    /// 采集）证明记录确实是这条规则带来的，而不是数据面的通用产物。
    #[tokio::test]
    async fn rule_created_over_http_fires_on_captured_samples() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (state, _path) = make_state(&dir, 1);
        let port = spawn_server(state.clone()).await;
        let token = token_for(&state, Role::System);
        let store = state.daemon().alarms_store();

        // 建规则前先留一份配置快照（反向对照用）。
        let before = (*state.config()).clone();
        assert!(before.alarms.is_none(), "种子配置里不该有告警规则");

        // ① 建规则（前端 `AlarmsPage` 的实际下发形状：`condition` + `target`）。
        let body = r#"{"rules":[{"name":"锅炉超温","condition":"[40001] > 100","level":"major","target":"dev-01","duration_sec":0,"suppress_min":0}]}"#;
        let (status, _, raw) = put_json(port, "/api/alerts/rules", body, &token).await;
        assert_eq!(status, 200, "建规则应 200，实际 {raw}");
        let created: Json = serde_json::from_str(&raw).expect("json");
        let rule = created["items"][0].clone();
        let rule_id = rule["id"].as_str().expect("rule id").to_string();
        assert_eq!(rule["condition"].as_str(), Some("[40001] > 100"));
        assert_eq!(
            rule["threshold"].as_f64(),
            Some(100.0),
            "回显的阈值必须与条件原文一致"
        );

        // ② 反向对照：规则还没建时采集同一份样本 → 不该有任何记录。
        let control = plane_with(&before, 250.0, Arc::clone(&store));
        control
            .poll("dev-01", &["40001".to_string()])
            .await
            .expect("control poll");
        assert_eq!(store.len(), 0, "无规则时不该产生告警记录");

        // ③ 采集到点：调度器调 `poll` 取数 → 数据面求值（真实样本、真实记录）。
        let config = (*state.config()).clone();
        assert!(config.alarms.is_some(), "规则必须已进配置");
        let plane = plane_with(&config, 250.0, Arc::clone(&store));
        let samples = plane
            .poll("dev-01", &["40001".to_string()])
            .await
            .expect("poll");
        assert_eq!(samples.len(), 1, "采集必须拿到样本");
        assert_eq!(
            plane.stats().alarms_evaluated,
            1,
            "这一拍必须真的进了告警求值阶段（不要求值时计数器不动）"
        );
        assert_eq!(plane.stats().alarms_fired, 1, "250 > 100 → 恰好触发一次");

        // ④ 列表先过鉴权：无 token 一律 401（读接口也不裸奔）。
        let (status, _, raw) = http_get(port, "/api/alerts").await;
        assert_eq!(status, 401, "无 token 应 401，实际 {raw}");

        // ⑤ 带 token 看得到这条真实记录（wire 契约：计数与时间戳均为字符串）。
        let (status, _, raw) =
            http_request(port, "GET", "/api/alerts", None, Some(&token), "text/plain").await;
        assert_eq!(status, 200, "告警列表应 200");
        let listed: Json = serde_json::from_str(&raw).expect("json");
        assert_eq!(listed["total"].as_str(), Some("1"), "一条真实记录");
        let item = &listed["items"][0];
        let expected_id = format!("{rule_id}|dev-01|40001");
        assert_eq!(item["id"].as_str(), Some(expected_id.as_str()));
        // 记录必须来自刚才那条 API 建出来的规则（`rule_id` 不在冻结的 wire 契约里，
        // 用标题 —— 取的就是规则名 —— 佐证归属）。
        assert_eq!(item["title"].as_str(), Some("锅炉超温"));
        assert_eq!(item["state"].as_str(), Some("open"));
        // `count` 按冻结的前端契约（AlarmsPage 用 `String(a.count)` 渲染）落成数值；
        // 走数值型的是时间戳（见下方毫秒字符串断言）。
        assert_eq!(
            item["count"].as_u64(),
            Some(1),
            "count 如实记 1，实际 {}",
            item["count"]
        );
        assert!(
            item["detail"].as_str().expect("detail").contains("250"),
            "detail 必须带真实采集值：{}",
            item["detail"]
        );
        assert_eq!(
            item["first_seen_at"].as_str().map(str::len),
            Some(13),
            "毫秒 epoch 字符串（前端 formatEpochText 只认 10/13 位）"
        );
    }

    /// 告警段从零起建：既存规则表为空时，`confirm` 必须能回显到**本次提交的**
    /// 规则名。
    ///
    /// 这条用例守的是一个自锁坑：`confirm` 的回显目标原本只从既存规则表里取，
    /// 于是在「一条规则都还没有」的时刻，无论填什么 `confirm` 都不匹配既存空
    /// 表 → 首次启用告警的建规则请求被自己的防误操作拦门打死（`confirm_mismatch`
    /// 400），告警规则一条都建不起来。退路是空表时改以提交内容为准。
    #[tokio::test]
    async fn first_rule_can_be_created_when_table_is_empty() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (state, _path) = make_state(&dir, 1);
        let port = spawn_server(state.clone()).await;
        let token = token_for(&state, Role::System);

        let body = r#"{"rules":[{"name":"锅炉超温","condition":"[40001] > 100","level":"major","target":"dev-01"}],"reason":"新建规则","note":"首次启用告警，从零建第一条规则","confirm":"锅炉超温"}"#;

        // 空表 + 正确回显 → 200（修好前这里是 400 confirm_mismatch）。
        let (status, _, raw) = put_json(port, "/api/alerts/rules", body, &token).await;
        assert_eq!(status, 200, "空表建第一条规则应 200，实际 {raw}");

        // 空表 + 张冠李戴的回显 → 仍然 400（防误操作没被绕过去）。
        let (status, _, raw) = put_json(
            port,
            "/api/alerts/rules",
            &body.replace("锅炉超温\"}", "不存在的规则\"}"),
            &token,
        )
        .await;
        assert_eq!(status, 400, "回显不符应 400，实际 {raw}");
    }

    /// 回归：告警处置 `POST /api/alerts/:id/ack` 的 `confirm` 二次确认必须 fail-closed。
    ///
    /// 上一版实现是 `!confirm.is_empty() && confirm.trim() != id`——`!is_empty()` 的
    /// 短路让**空 `confirm` 直接放行**，「危险操作二次确认」这条闸门被空值整个拆掉；
    /// 同时区分大小写，与本文件 `:271` 那一路的口径不一致。本用例锁死三件事：
    /// 空 → 400、错 → 400 `confirm_mismatch`、前后空格 + 大小写变化 → 放行。
    #[tokio::test]
    async fn ack_confirm_is_fail_closed_and_trim_case_insensitive() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (state, _path) = make_state(&dir, 1);
        let port = spawn_server(state.clone()).await;
        let token = token_for(&state, Role::System);

        let alarm_id = "al-ack-1";
        state.daemon().alarms_store().upsert(&[AlarmRecord {
            id: alarm_id.to_string(),
            rule_id: "r1".to_string(),
            device_id: "dev-01".to_string(),
            point_id: "40001".to_string(),
            level: "major".to_string(),
            title: "锅炉超温".to_string(),
            detail: String::new(),
            source_type: "device".to_string(),
            source_label: "dev-01".to_string(),
            state: "open".to_string(),
            first_seen_at: "1700000000000".to_string(),
            last_seen_at: "1700000000000".to_string(),
            count: 1,
            acked_by: String::new(),
            note: String::new(),
        }]);
        let path = format!("/api/alerts/{alarm_id}/ack");
        // note 取 ≥ MIN_NOTE_LEN(10)：确保下面断言命中的是 confirm，而不是 note 太短。
        let note = "现场已确认并上报值班人员";

        // ① 空 confirm：必须 400（上一版这里会被放行）。
        let (status, _, raw) = post_json(
            port,
            &path,
            &format!("{{\"state\":\"acking\",\"note\":\"{note}\"}}"),
            &token,
        )
        .await;
        assert_eq!(status, 400, "空 confirm 必须被拒，实际 {status} {raw}");

        // ② 张冠李戴的 confirm → 400 confirm_mismatch。
        let (status, _, raw) = post_json(
            port,
            &path,
            &format!("{{\"state\":\"acking\",\"note\":\"{note}\",\"confirm\":\"别的告警\"}}"),
            &token,
        )
        .await;
        assert_eq!(status, 400, "错误 confirm 应 400，实际 {status} {raw}");
        assert_eq!(
            serde_json::from_str::<Json>(&raw).expect("json")["error"],
            "confirm_mismatch"
        );

        // ③ 前后空格 + 全大写 → 放行（`trim()` + 大小写不敏感精确匹配）。
        let (status, _, raw) = post_json(
            port,
            &path,
            &format!(
                "{{\"state\":\"acking\",\"note\":\"{note}\",\"confirm\":\"  {} \"}}",
                alarm_id.to_uppercase()
            ),
            &token,
        )
        .await;
        assert_eq!(
            status, 200,
            "trim + 大小写不敏感应放行，实际 {status} {raw}"
        );
    }
}

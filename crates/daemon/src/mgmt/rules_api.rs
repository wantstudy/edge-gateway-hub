//! 转发规则 CRUD（`GET /api/rules`、`POST /api/rules`、`PUT /api/rules/:id`、
//! `DELETE /api/rules/:id`）——web-console「转发规则」页（`/rules`）的冻结契约。
//!
//! ## 端点
//! | 方法 · 路径 | handler | 语义 |
//! | --- | --- | --- |
//! | `GET    /api/rules`      | [`list`]     | 读回配置 `[[rules]]` 全量行 |
//! | `POST   /api/rules`      | [`create`]   | 新增（结构化 `when` / `actions`） |
//! | `PUT    /api/rules/:id`  | [`update`]   | 可选子集部分更新（含启停 `enabled`） |
//! | `DELETE /api/rules/:id`  | [`remove`]   | 删除（`reason` / `note` / `confirm` 三独立字段） |
//!
//! 启停与编辑走**同一 handler**（`PUT /api/rules/:id`）：前端 `repo.rules
//! .setEnabled` 只下发 `{enabled, reason, note, confirm}`，与「可选子集更新」
//! 是同一语义，不另开端点。
//!
//! ## 写路径语义（与 `writeapi` 的设备 / 点位写同范式）
//! 1. **鉴权**：`AuthedRole` extractor（401）→
//!    `ensure(Permission::DeviceWrite)`（403，入审计；与 `/api/alerts/rules`
//!    同一档配置写权限——转发规则会改变北向投递语义，仅 `system` 可授）；
//! 2. **危险操作四要素**（写接口**任何提前 return 之前**校验）：
//!    `reason`（必填；删除须为枚举内原文）、`note`（独立字段，≥ 10 字，绝不与
//!    `reason` 拼接）、`confirm`（回显规则**全名**原文；不匹配 400
//!    `confirm_mismatch`）；
//! 3. **语义校验**：落盘前用 [`crate::rules::RuleEngine`] 的构造期校验过一遍
//!    「整体规则集」——JSONPath 白名单、publish topic 非空、where 字段类型 /
//!    比较符匹配、`depends_on` DAG（环 / 未知 id 一律拒绝）。**不合法绝不落盘**
//!    （半写的规则会在数据面上全量转发，是最危险的一类事故）；
//! 4. **落盘**：全进程写锁 → 基于当前热快照克隆变更 → `persist_config`
//!    （写前备份 + 原子落盘 + 即时推送新快照，读侧立即可见）；
//! 5. **审计**：新增 / 改 / 删（含被拒 / 非法 / 落盘失败）全入审计环，
//!    `reason` / `note` / `confirm` 三者原样落痕（便于对 `reason` 做枚举统计）。
//!
//! ## 结构化契约（禁止退化为描述串）
//! `when` 必须是 [`crate::rules::Condition`] 的 kind 标签 JSON 形态
//! （`{"kind":"cmp",...}` / `{"kind":"and","conditions":[...]}` …），`actions`
//! 必须是 [`crate::rules::Action`] 形态；**字符串条件一律 400**（`Condition` /
//! `Action` 的 `Deserialize` 手写实现会给出带上下文的报错）。`GET` 回显同一结构。
//!
//! ## 诚实限制（不伪造）
//! - `hit_count` 恒 0：[`crate::rules::RuleEngine`] 求值路径（
//!   [`crate::rules::RuleEngine::route`]）**不做命中记账**，这里不另起一套计数
//!   假装真实（避免「命中次数」成为不可信数字）；
//! - `depends_on` 的跨规则 DAG 校验在「整体规则集」上执行（见上文 3），
//!   单条规则本身不含全局拓扑信息。

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
use crate::config::{GatewayConfig, RuleConfig};
use crate::rules::{Action, CmpOp, Condition, ConditionValue, RuleEngine};

/// 删除原因取值域（与前端 `RulesPage.vue::DELETE_REASONS` **逐字一致**）。
///
/// ⚠️ 只用于 `DELETE`：新增 / 编辑 / 启停的前端「变更原因」是**自由文本**
/// （`RulesPage.vue` 用 `UiInput` 而非枚举下拉），后端在那里强制枚举会让
/// 正常的新增流程直接 400。删除走 `DangerConfirmModal`（枚举下拉 +
/// `min-note-length=10` + 全名二次确认）。
const DELETE_REASONS: &[&str] = &["规则不再需要", "条件配置错误", "路由目标变更", "调试清理"];

/// `note` 最小字数（与 `DangerConfirmModal` 的 `min-note-length` 一致）。
const MIN_NOTE_LEN: usize = 10;

/// 规则 id 最大长度（防超长 id 写塌 TOML 行）。
const MAX_RULE_ID_LEN: usize = 128;
/// 规则名称最大长度（展示 + 二次确认回显长度）。
const MAX_RULE_NAME_LEN: usize = 128;

// ---- 请求体 ----

/// `POST /api/rules` / `PUT /api/rules/:id` 请求体（**可选子集**部分更新语义）。
///
/// 字段一律 `Option` / `Vec` 缺省语义：`Vec` 缺省 = 不改动（不是「清空」），
/// 与 `writeapi` 的点位更新同口径；新增时 `actions` 为空会被业务校验拒绝
/// （一条产不出消息的规则对数据面毫无意义）。
#[derive(Debug, Default, Deserialize)]
struct RuleUpsertBody {
    /// 规则主键（新增时前端生成；编辑时即路径参数）。
    #[serde(default)]
    id: String,
    /// 规则名称（必填，非空）。
    #[serde(default)]
    name: String,
    /// 生效出口 id（**空串 = 未绑定 / 不限定出口**）。
    #[serde(default)]
    forwarder_id: String,
    /// 是否启用（缺省 = 不改）。
    #[serde(default)]
    enabled: Option<bool>,
    /// 优先级（缺省 = 不改）。
    #[serde(default)]
    priority: Option<i64>,
    /// WHERE 条件树（`None` = 恒真通配；缺省 = 不改）。
    #[serde(default)]
    when: Option<Condition>,
    /// DO 动作列表（缺省 = 不改）。
    #[serde(default)]
    actions: Vec<Action>,
    /// SELECT 字段白名单（缺省 = 不改）。
    #[serde(default)]
    select: Vec<String>,
    /// 依赖规则 id（缺省 = 不改）。
    #[serde(default)]
    depends_on: Vec<String>,
}

/// 危险操作三要素（`reason` / `note` / `confirm` **独立字段**，禁止拼接）。
#[derive(Debug, Default, Deserialize)]
struct DangerBody {
    #[serde(default)]
    reason: String,
    #[serde(default)]
    note: String,
    #[serde(default)]
    confirm: String,
}

// ---- 危险操作四要素校验（先于一切业务分支 / 提前 return） ----

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

/// `note`：独立补充说明。空 = 未填（前端表单为可选项；此处**不**把它连同
/// `reason` 一起伪造），非空则 ≥ [`MIN_NOTE_LEN`] 字。
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
            "confirm is required (echo the full rule name)",
            "the full rule name",
        ));
    }
    Ok(())
}

/// `confirm` 回显值不匹配：`confirm` 必须逐字等于规则**全名**。
fn confirm_mismatch(expected: &str) -> Response {
    (
        StatusCode::BAD_REQUEST,
        Json(json!({
            "error": "confirm_mismatch",
            "field": "confirm",
            "reason": format!("confirm does not match the rule name {expected:?}"),
            "allowed": expected,
        })),
    )
        .into_response()
}

// ---- 规则视图（读回 / 回显） ----

/// 规则行 → `GET /api/rules` 的 JSON 视图（`when` / `actions` **结构化**回显，
/// 绝不退化为描述串；`condition` / `action` 仅为列表展示用的人读描述）。
fn rule_to_wire(rule: &RuleConfig) -> Value {
    json!({
        "id": rule.id,
        "name": rule.name,
        "forwarder_id": rule.forwarder_id,
        "condition": describe_condition(&rule.when),
        "action": describe_actions(&rule.actions),
        // 引擎不记账命中次数（见模块注释「诚实限制」）：恒 0，不伪造统计。
        "hit_count": 0,
        "priority": rule.priority,
        "enabled": rule.enabled,
        "last_hit_at": "",
        "when": rule.when,
        "actions": rule.actions,
        "select": rule.select,
        "depends_on": rule.depends_on,
    })
}

/// WHERE 条件树 → 人读描述（`None` = 恒真通配规则）。
fn describe_condition(when: &Option<Condition>) -> String {
    match when {
        None => "全部数据（无条件过滤）".to_string(),
        Some(condition) => describe_node(condition),
    }
}

/// 条件节点 → 人读描述（递归渲染 `and` / `or` / `not` 嵌套）。
fn describe_node(condition: &Condition) -> String {
    match condition {
        Condition::Cmp { field, op, value } => {
            format!("{field} {} {}", op_text(op), value_text(value))
        }
        Condition::And(conditions) => join_conditions(conditions, "且"),
        Condition::Or(conditions) => join_conditions(conditions, "或"),
        Condition::Not(inner) => format!("非（{}）", describe_node(inner)),
    }
}

fn join_conditions(conditions: &[Condition], glue: &str) -> String {
    let rendered: Vec<String> = conditions.iter().map(describe_node).collect();
    rendered.join(&format!(" {glue} "))
}

/// 比较值 → 人读文本（`Num` / `Str` 两种 wire 形态）。
fn value_text(value: &ConditionValue) -> String {
    match value {
        ConditionValue::Num(num) => num.to_string(),
        ConditionValue::Str(text) => text.clone(),
    }
}

fn op_text(op: &CmpOp) -> &'static str {
    match op {
        CmpOp::Gt => ">",
        CmpOp::Ge => ">=",
        CmpOp::Lt => "<",
        CmpOp::Le => "<=",
        CmpOp::Eq => "=",
        CmpOp::Ne => "≠",
    }
}

/// DO 动作列表 → 人读描述。
fn describe_actions(actions: &[Action]) -> String {
    if actions.is_empty() {
        return "无动作".to_string();
    }
    actions
        .iter()
        .map(|action| match action {
            Action::Publish { topic } => format!("转发至 {topic}"),
            Action::Remap { fields } => {
                let pairs: Vec<String> = fields
                    .iter()
                    .map(|(key, path)| format!("{key}←{path}"))
                    .collect();
                format!("重映射（{}）", pairs.join("，"))
            }
        })
        .collect::<Vec<String>>()
        .join("；")
}

// ---- 业务校验 / 引擎校验 ----

/// 校验规则 id（非空 + 长度 + 字符集：与分组 id 同口径的安全字符集）。
#[allow(clippy::result_large_err)]
fn validate_rule_id(id: &str) -> Result<(), Response> {
    if id.is_empty() {
        return Err(validation_error(
            "id",
            "rule id is required",
            "non-empty unique rule identifier",
        ));
    }
    if id.len() > MAX_RULE_ID_LEN {
        return Err(validation_error(
            "id",
            &format!("rule id exceeds {MAX_RULE_ID_LEN} characters"),
            &format!("≤{MAX_RULE_ID_LEN} characters"),
        ));
    }
    if id.chars().any(|ch| ch.is_whitespace() || ch.is_control()) {
        return Err(validation_error(
            "id",
            "rule id must not contain whitespace or control characters",
            "printable characters only",
        ));
    }
    Ok(())
}

/// 校验规则名称（非空 + 长度）。
#[allow(clippy::result_large_err)]
fn validate_rule_name(name: &str) -> Result<(), Response> {
    let text = name.trim();
    if text.is_empty() {
        return Err(validation_error(
            "name",
            "rule name is required",
            "non-empty rule name (also the `confirm` echo target)",
        ));
    }
    if text.chars().count() > MAX_RULE_NAME_LEN {
        return Err(validation_error(
            "name",
            &format!("rule name exceeds {MAX_RULE_NAME_LEN} characters"),
            &format!("≤{MAX_RULE_NAME_LEN} characters"),
        ));
    }
    Ok(())
}

/// `forwarder_id` 必须命中既有出口（`[[outlets]].name`）。
#[allow(clippy::result_large_err)]
fn validate_forwarder(config: &GatewayConfig, forwarder_id: &str) -> Result<(), Response> {
    let Some(id) = trimmed_or_none(forwarder_id) else {
        return Ok(()); // 未绑定 = 不限定出口
    };
    if config.outlets.iter().any(|o| o.name == id) {
        Ok(())
    } else {
        Err(validation_error(
            "forwarder_id",
            &format!("unknown forwarder {id:?}"),
            &format!(
                "one of [[outlets]].name (configured: {:?})",
                outlet_names(config)
            ),
        ))
    }
}

fn trimmed_or_none(raw: &str) -> Option<String> {
    let text = raw.trim();
    if text.is_empty() {
        None
    } else {
        Some(text.to_string())
    }
}

fn outlet_names(config: &GatewayConfig) -> Vec<String> {
    config.outlets.iter().map(|o| o.name.clone()).collect()
}

/// 落盘前的**整体规则集**引擎校验（语义 + DAG）。
///
/// 单条规则不足以保证全局合法（`depends_on` 指向的规则可能在本规则之后），
/// 因此这里拿「变更后的完整集合」过一遍 [`RuleEngine::from_rules`]——任何
/// 构造期错误都不落盘（半写规则会在数据面上把全部样本转发出去）。
#[allow(clippy::result_large_err)]
fn validate_rule_set(config: &GatewayConfig) -> Result<(), Response> {
    let rules: Vec<crate::rules::Rule> = config.rules.iter().map(RuleConfig::to_rule).collect();
    if rules.is_empty() {
        return Ok(());
    }
    match RuleEngine::from_rules(rules) {
        Ok(_) => Ok(()),
        Err(err) => Err(validation_error(
            "rule",
            &format!("rule engine rejected the rule set: {err}"),
            "structured `when` / `actions` (see crate::rules::Condition / Action)",
        )),
    }
}

// ---- 读 ----

/// `GET /api/rules` → 转发规则清单（结构化 `when` / `actions` 原样回显）。
///
/// 数据源 = 热配置快照（**真实行**；没有任何「拿不到就回退假数据」的分支——
/// 配置快照恒可得，空列表 = 用户真的还没配规则）。
pub async fn list(State(state): State<MgmtState>, authed: AuthedRole) -> Response {
    if let Err(rejection) = authed.ensure(Permission::DeviceView) {
        audit(
            &state,
            &authed.claims.sub,
            OpsAction::RuleUpdate,
            false,
            OUTCOME_DENIED,
            &rejection.to_string(),
        );
        return rejection.into_response();
    }
    let config = state.config();
    let items: Vec<Value> = config.rules.iter().map(rule_to_wire).collect();
    Json(json!({
        "items": items,
        // 大数红线：计数类字段一律字符串。
        "total": items.len().to_string(),
        "source": "ok",
    }))
    .into_response()
}

// ---- 写 ----

/// `POST /api/rules` → 新增转发规则（结构化 `when` + `actions`），返回 201。
pub async fn create(State(state): State<MgmtState>, authed: AuthedRole, body: Bytes) -> Response {
    if let Err(rejection) = authed.ensure(Permission::DeviceWrite) {
        audit(
            &state,
            &authed.claims.sub,
            OpsAction::RuleCreate,
            false,
            OUTCOME_DENIED,
            &rejection.to_string(),
        );
        return rejection.into_response();
    }
    let actor = authed.claims.sub.clone();
    let req: RuleUpsertBody = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(err) => {
            audit(
                &state,
                &actor,
                OpsAction::RuleCreate,
                true,
                OUTCOME_BAD_REQUEST,
                &format!("malformed json body: {err}"),
            );
            return validation_error(
                "body",
                &format!("malformed JSON: {err}"),
                "object {id, name, forwarder_id?, enabled?, priority?, when?, actions?, select?, depends_on?}",
            );
        }
    };

    // 危险操作三要素：**先于** id 去重 / 落盘等一切业务分支。
    let danger: DangerBody = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(err) => {
            audit(
                &state,
                &actor,
                OpsAction::RuleCreate,
                true,
                OUTCOME_BAD_REQUEST,
                &format!("malformed danger body: {err}"),
            );
            return validation_error(
                "body",
                &format!("malformed JSON: {err}"),
                "object {..., reason?, note?, confirm?}",
            );
        }
    };
    if let Err(resp) = check_reason(&danger.reason, false) {
        return resp;
    }
    if let Err(resp) = check_note(&danger.note) {
        return resp;
    }
    if let Err(resp) = check_confirm_present(&danger.confirm) {
        return resp;
    }

    let id = req.id.trim().to_string();
    if let Err(resp) = validate_rule_id(&id) {
        audit(
            &state,
            &actor,
            OpsAction::RuleCreate,
            true,
            OUTCOME_BAD_REQUEST,
            "invalid rule id",
        );
        return resp;
    }
    let name = req.name.trim().to_string();
    if let Err(resp) = validate_rule_name(&name) {
        audit(
            &state,
            &actor,
            OpsAction::RuleCreate,
            true,
            OUTCOME_BAD_REQUEST,
            "invalid rule name",
        );
        return resp;
    }
    if req.actions.is_empty() {
        audit(
            &state,
            &actor,
            OpsAction::RuleCreate,
            true,
            OUTCOME_BAD_REQUEST,
            "rule without actions",
        );
        return validation_error(
            "actions",
            "at least one action is required (a rule without actions emits nothing)",
            "[{\"kind\":\"publish\",\"topic\":\"...\"}] | [{\"kind\":\"remap\",\"fields\":{...}}]",
        );
    }
    let forwarder_id = req.forwarder_id.clone();
    let select = req.select.clone();
    let depends_on = req.depends_on.clone();

    let _guard = write_guard();
    let config = (*state.config()).clone();
    if let Err(resp) = validate_forwarder(&config, &forwarder_id) {
        audit(
            &state,
            &actor,
            OpsAction::RuleCreate,
            true,
            OUTCOME_BAD_REQUEST,
            &format!("invalid forwarder_id {forwarder_id:?}"),
        );
        return resp;
    }
    if config.rules.iter().any(|rule| rule.id == id) {
        audit(
            &state,
            &actor,
            OpsAction::RuleCreate,
            true,
            OUTCOME_BAD_REQUEST,
            &format!("duplicate rule id {id:?}"),
        );
        return validation_error(
            "id",
            &format!("rule {id:?} already exists"),
            "unique rule id",
        );
    }
    let rule = RuleConfig {
        id,
        name: name.clone(),
        forwarder_id: trimmed_or_none(&forwarder_id),
        when: req.when.clone(),
        actions: req.actions.clone(),
        select,
        depends_on,
        priority: req.priority.unwrap_or(0),
        enabled: req.enabled.unwrap_or(true),
    };
    // 语义校验必须在**落盘的那一份**上做：含刚新增的这条（否则新规则的
    // JSONPath / topic / DAG 错误会一路落盘，数据面下一拍就把全量样本转发出去）。
    let mut next = config.clone();
    next.rules.push(rule.clone());
    if let Err(resp) = validate_rule_set(&next) {
        return resp;
    }

    match persist_config(
        &state,
        next,
        &actor,
        OpsAction::RuleCreate,
        &format!(
            "create rule {name:?} (enabled={}, actions={})",
            rule.enabled,
            rule.actions.len()
        ),
    ) {
        Ok(version) => {
            state.publish(MgmtEvent::ConfigReloaded { version });
            (
                StatusCode::CREATED,
                Json(json!({
                    "ok": true,
                    "rule": rule_to_wire(&rule),
                    "config_version": version.to_string(),
                })),
            )
                .into_response()
        }
        Err(resp) => resp,
    }
}

/// `PUT /api/rules/:id` → 可选子集部分更新（含启停 `enabled`）。
///
/// - 只下发 `{enabled, reason, note, confirm}`（仅启停）= 改启用状态；
/// - 只下发审计三要素、无任何可更新字段 = **`updated: false` 的空更新**，
///   如实返回当前规则（不做「假装改了」）；
/// - 未知 id → 404；`confirm` 必须回显（提交的 `name` 优先，否则当前名称）。
pub async fn update(
    State(state): State<MgmtState>,
    AxumPath(id): AxumPath<String>,
    authed: AuthedRole,
    body: Bytes,
) -> Response {
    if let Err(rejection) = authed.ensure(Permission::DeviceWrite) {
        audit(
            &state,
            &authed.claims.sub,
            OpsAction::RuleUpdate,
            false,
            OUTCOME_DENIED,
            &rejection.to_string(),
        );
        return rejection.into_response();
    }
    let actor = authed.claims.sub.clone();
    let req: RuleUpsertBody = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(err) => {
            audit(
                &state,
                &actor,
                OpsAction::RuleUpdate,
                true,
                OUTCOME_BAD_REQUEST,
                &format!("malformed json body: {err}"),
            );
            return validation_error(
                "body",
                &format!("malformed JSON: {err}"),
                "object {name?, forwarder_id?, enabled?, priority?, when?, actions?, select?, depends_on?}",
            );
        }
    };
    let danger: DangerBody = match serde_json::from_slice::<DangerBody>(&body) {
        Ok(v) => v,
        Err(err) => {
            audit(
                &state,
                &actor,
                OpsAction::RuleUpdate,
                true,
                OUTCOME_BAD_REQUEST,
                &format!("malformed danger body: {err}"),
            );
            return validation_error(
                "body",
                &format!("malformed JSON: {err}"),
                "object {..., reason?, note?, confirm?}",
            );
        }
    };
    // 四要素校验先于「规则是否存在」等一切业务分支。
    if let Err(resp) = check_reason(&danger.reason, false) {
        return resp;
    }
    if let Err(resp) = check_note(&danger.note) {
        return resp;
    }
    if let Err(resp) = check_confirm_present(&danger.confirm) {
        return resp;
    }

    let rule_id = id.trim().to_string();
    let _guard = write_guard();
    let mut config = (*state.config()).clone();
    let position = config.rules.iter().position(|rule| rule.id == rule_id);
    let Some(position) = position else {
        audit(
            &state,
            &actor,
            OpsAction::RuleUpdate,
            true,
            OUTCOME_BAD_REQUEST,
            &format!("unknown rule {rule_id:?}"),
        );
        return not_found(&format!("rule {rule_id:?} not found"));
    };
    let current = config.rules[position].clone();
    // `confirm` 回显：提交的 `name` 优先（改名场景），否则当前名称。
    let expected_confirm = match trimmed_or_none(&req.name) {
        Some(name) => name,
        None => current.name.clone(),
    };
    // 大数 / 大小写口径：`trim()` + 大小写不敏感精确匹配（`confirm` 须回显对象全名）。
    if !danger
        .confirm
        .trim()
        .eq_ignore_ascii_case(&expected_confirm)
    {
        audit(
            &state,
            &actor,
            OpsAction::RuleUpdate,
            true,
            OUTCOME_BAD_REQUEST,
            &format!("confirm mismatch for rule {rule_id:?}"),
        );
        return confirm_mismatch(&expected_confirm);
    }

    let mut updated = current.clone();
    if let Some(name) = trimmed_or_none(&req.name) {
        if let Err(resp) = validate_rule_name(&name) {
            return resp;
        }
        updated.name = name;
    }
    if let Err(resp) = validate_forwarder(&config, &req.forwarder_id) {
        return resp;
    }
    updated.forwarder_id = trimmed_or_none(&req.forwarder_id);
    if let Some(enabled) = req.enabled {
        updated.enabled = enabled;
    }
    if let Some(priority) = req.priority {
        updated.priority = priority;
    }
    if req.when.is_some() {
        updated.when = req.when.clone();
    }
    if !req.actions.is_empty() {
        updated.actions = req.actions.clone();
    }
    if !req.select.is_empty() {
        updated.select = req.select.clone();
    }
    if !req.depends_on.is_empty() {
        updated.depends_on = req.depends_on.clone();
    }
    if updated.actions.is_empty() {
        return validation_error(
            "actions",
            "at least one action is required (a rule without actions emits nothing)",
            "[{\"kind\":\"publish\",\"topic\":\"...\"}] | [{\"kind\":\"remap\",\"fields\":{...}}]",
        );
    }
    let touched = updated != current;
    config.rules[position] = updated.clone();
    if let Err(resp) = validate_rule_set(&config) {
        return resp;
    }
    if !touched {
        // 空更新：如实说明「未改动」，不伪造成功。
        audit(
            &state,
            &actor,
            OpsAction::RuleUpdate,
            true,
            OUTCOME_ACCEPTED,
            &format!("empty partial update for rule {rule_id:?}; nothing changed"),
        );
        return Json(json!({
            "ok": true,
            "updated": false,
            "rule": rule_to_wire(&current),
            "note": "no updatable field received; rule unchanged",
        }))
        .into_response();
    }

    match persist_config(
        &state,
        config,
        &actor,
        OpsAction::RuleUpdate,
        &format!(
            "update rule {rule_id:?} (enabled={}, actions={})",
            updated.enabled,
            updated.actions.len()
        ),
    ) {
        Ok(version) => {
            state.publish(MgmtEvent::ConfigReloaded { version });
            Json(json!({
                "ok": true,
                "updated": true,
                "rule": rule_to_wire(&updated),
                "config_version": version.to_string(),
            }))
            .into_response()
        }
        Err(resp) => resp,
    }
}

/// `DELETE /api/rules/:id` → 删除转发规则（三独立字段二次确认）。
///
/// `reason` 必须取 [`DELETE_REASONS`] 内原文，`note` ≥ 10 字，`confirm` 必须
/// 逐字回显规则名。任一不满足 → 400（含审计），**绝不做部分删除**。
pub async fn remove(
    State(state): State<MgmtState>,
    AxumPath(id): AxumPath<String>,
    authed: AuthedRole,
    body: Bytes,
) -> Response {
    if let Err(rejection) = authed.ensure(Permission::DeviceWrite) {
        audit(
            &state,
            &authed.claims.sub,
            OpsAction::RuleDelete,
            false,
            OUTCOME_DENIED,
            &rejection.to_string(),
        );
        return rejection.into_response();
    }
    let actor = authed.claims.sub.clone();
    let danger: DangerBody = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(err) => {
            audit(
                &state,
                &actor,
                OpsAction::RuleDelete,
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
    // 删除是四要素最严口径：枚举原因 + 独立备注 + 全名回显。
    if let Err(resp) = check_reason(&danger.reason, true) {
        return resp;
    }
    if let Err(resp) = check_note(&danger.note) {
        return resp;
    }
    if let Err(resp) = check_confirm_present(&danger.confirm) {
        return resp;
    }

    let rule_id = id.trim().to_string();
    let _guard = write_guard();
    let mut config = (*state.config()).clone();
    let Some(position) = config.rules.iter().position(|rule| rule.id == rule_id) else {
        audit(
            &state,
            &actor,
            OpsAction::RuleDelete,
            true,
            OUTCOME_BAD_REQUEST,
            &format!("unknown rule {rule_id:?}"),
        );
        return not_found(&format!("rule {rule_id:?} not found"));
    };
    let removed = config.rules.remove(position);
    // 大数 / 大小写口径：`trim()` + 大小写不敏感精确匹配（confirm 须回显对象全名）。
    if !danger.confirm.trim().eq_ignore_ascii_case(&removed.name) {
        audit(
            &state,
            &actor,
            OpsAction::RuleDelete,
            true,
            OUTCOME_BAD_REQUEST,
            &format!("confirm mismatch for rule {rule_id:?}"),
        );
        // 修正：把已摘除的行放回去（失败路径绝不留下半改状态）。
        config.rules.insert(position, removed.clone());
        return confirm_mismatch(&removed.name);
    }

    match persist_config(
        &state,
        config,
        &actor,
        OpsAction::RuleDelete,
        &format!(
            "delete rule {rule_id:?} (reason={:?} note={:?})",
            danger.reason, danger.note
        ),
    ) {
        Ok(version) => {
            state.publish(MgmtEvent::ConfigReloaded { version });
            Json(json!({
                "ok": true,
                "deleted": true,
                "id": rule_id,
                "config_version": version.to_string(),
            }))
            .into_response()
        }
        Err(resp) => resp,
    }
}

// ---- 单元测试 ----

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rules::{CmpOp, ConditionValue};

    fn danger(reason: &str, note: &str, confirm: &str) -> DangerBody {
        DangerBody {
            reason: reason.to_string(),
            note: note.to_string(),
            confirm: confirm.to_string(),
        }
    }

    /// 四要素：`reason` 必填（枚举口径仅用于删除）。
    #[test]
    fn reason_is_required_and_must_be_enum_member_for_delete() {
        assert!(check_reason("", false).is_err(), "empty rejected");
        assert!(
            check_reason("质量过滤需求落地", false).is_ok(),
            "free text ok"
        );
        assert!(
            check_reason("质量过滤需求落地", true).is_err(),
            "delete must use an enum member"
        );
        for allowed in DELETE_REASONS {
            assert!(check_reason(allowed, true).is_ok(), "{allowed} accepted");
        }
        assert!(
            check_reason("规则不再需要（误删）", true).is_err(),
            "appending text to an enum member is rejected"
        );
    }

    /// `note` 独立字段：不足 10 字 → 400；`reason` 绝不参与判定。
    #[test]
    fn note_is_independent_and_enforced() {
        assert!(check_reason("调试清理", false).is_ok());
        assert!(check_note("太短").is_err(), "3 chars rejected");
        assert!(
            check_note("补充说明至少十个字").is_err(),
            "9 chars rejected（边界）"
        );
        assert!(
            check_note("补充说明至少十个字符").is_ok(),
            "10 chars accepted（边界）"
        );
        assert!(check_note("").is_ok(), "absent note allowed (UI 可选项)");
    }

    /// 删除请求的三要素组合：枚举 `reason` + 独立 `note` + 全名 `confirm`。
    #[test]
    fn delete_request_validates_the_whole_trio() {
        let ok = danger("规则不再需要", "现场已无此路测点需求", "高温转发");
        assert!(check_reason(&ok.reason, true).is_ok());
        assert!(check_note(&ok.note).is_ok());
        assert!(check_confirm_present(&ok.confirm).is_ok());
        assert_eq!(ok.reason, "规则不再需要", "reason 原样保留，不被 note 拼接");

        let missing_confirm = danger("调试清理", "临时清理一下不再需要的规则", "");
        assert!(check_confirm_present(&missing_confirm.confirm).is_err());

        let short_note = danger("调试清理", "短备注", "高温转发");
        assert!(check_note(&short_note.note).is_err());
    }

    /// `confirm` 必填 + 不匹配 → `confirm_mismatch` 400。
    #[test]
    fn confirm_is_required_and_mismatched_response_is_400() {
        assert!(check_confirm_present("").is_err(), "missing → 400");
        assert!(check_confirm_present("高温告警").is_ok());
        let resp = confirm_mismatch("高温告警");
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    /// 结构化 `when` / `actions` 不被退化为描述串（读回契约的核心）。
    #[test]
    fn wire_carries_structured_when_and_actions() {
        let rule = RuleConfig {
            id: "r-1".to_string(),
            name: "高温转发".to_string(),
            forwarder_id: Some("out-a".to_string()),
            when: Some(Condition::Cmp {
                field: "value".to_string(),
                op: CmpOp::Gt,
                value: ConditionValue::Num(30.0),
            }),
            actions: vec![Action::Publish {
                topic: "gw/hot".to_string(),
            }],
            select: vec!["value".to_string()],
            depends_on: Vec::new(),
            priority: 1,
            enabled: true,
        };
        let wire = rule_to_wire(&rule);
        assert_eq!(wire["when"]["kind"], json!("cmp"));
        assert_eq!(wire["when"]["field"], json!("value"));
        assert_eq!(wire["when"]["op"], json!("gt"));
        assert_eq!(wire["when"]["value"], json!(30.0));
        assert_eq!(wire["actions"][0]["kind"], json!("publish"));
        assert_eq!(wire["actions"][0]["topic"], json!("gw/hot"));
        assert_eq!(wire["select"][0], json!("value"));
        assert_eq!(wire["hit_count"], json!(0), "引擎不记账命中数，不伪造");
    }

    /// `and` / `or` / `not` 嵌套条件原样回显。
    #[test]
    fn nested_condition_tree_survives_the_wire() {
        let rule = RuleConfig {
            id: "r-2".to_string(),
            name: "复合过滤".to_string(),
            when: Some(Condition::And(vec![
                Condition::Cmp {
                    field: "value".to_string(),
                    op: CmpOp::Ge,
                    value: ConditionValue::Num(10.0),
                },
                Condition::Or(vec![Condition::Not(Box::new(Condition::Cmp {
                    field: "quality".to_string(),
                    op: CmpOp::Eq,
                    value: ConditionValue::Str("bad".to_string()),
                }))]),
            ])),
            actions: vec![
                Action::Publish {
                    topic: "a".to_string(),
                },
                Action::Remap {
                    fields: [("t".to_string(), "$.value".to_string())]
                        .into_iter()
                        .collect(),
                },
            ],
            ..RuleConfig::default()
        };
        let wire = rule_to_wire(&rule);
        assert_eq!(wire["when"]["kind"], json!("and"));
        let conditions = wire["when"]["conditions"].as_array().expect("array");
        assert_eq!(conditions.len(), 2);
        assert_eq!(conditions[1]["kind"], json!("or"));
        assert_eq!(conditions[1]["conditions"][0]["kind"], json!("not"));
        assert_eq!(wire["actions"][1]["kind"], json!("remap"));
    }

    /// id / name / forwarder 校验：空、超长、空白字符、未知出口。
    #[test]
    fn field_validation_rejects_unusable_input() {
        assert!(validate_rule_id("").is_err());
        assert!(validate_rule_id(&"r".repeat(MAX_RULE_ID_LEN + 1)).is_err());
        assert!(validate_rule_id("rule a").is_err(), "whitespace rejected");
        assert!(validate_rule_id("rule-a").is_ok());

        assert!(validate_rule_name("").is_err());
        assert!(validate_rule_name("   ").is_err());
        assert!(validate_rule_name("高温转发").is_ok());

        let config = GatewayConfig::parse(
            "[[outlets]]\nname = \"out-a\"\nbroker = \"mqtt://127.0.0.1:1883\"\n",
        )
        .expect("parse");
        assert!(validate_forwarder(&config, "out-a").is_ok());
        assert!(
            validate_forwarder(&config, "no-such-outlet").is_err(),
            "未知出口必须拒绝（否则规则永远不生效）"
        );
        assert!(
            validate_forwarder(&config, "").is_ok(),
            "空串 = 未绑定，允许"
        );
    }

    /// 落盘前引擎校验：非法 JSONPath / 空 topic / 未知依赖 → 拒绝（绝不半写）。
    #[test]
    fn rule_set_validation_rejects_semantically_invalid_rules() {
        let mut config = GatewayConfig::parse("").expect("empty");
        config.rules.push(RuleConfig {
            id: "ok".to_string(),
            name: "ok".to_string(),
            when: Some(Condition::Cmp {
                field: "value".to_string(),
                op: CmpOp::Gt,
                value: ConditionValue::Num(1.0),
            }),
            actions: vec![Action::Publish {
                topic: "gw/ok".to_string(),
            }],
            ..RuleConfig::default()
        });
        assert!(validate_rule_set(&config).is_ok(), "合法规则集通过");

        let mut bad = config.clone();
        bad.rules.push(RuleConfig {
            id: "bad-path".to_string(),
            name: "bad-path".to_string(),
            when: Some(Condition::Cmp {
                field: "no.such.field.deep".to_string(),
                op: CmpOp::Gt,
                value: ConditionValue::Num(1.0),
            }),
            actions: vec![Action::Publish {
                topic: "gw/bad".to_string(),
            }],
            ..RuleConfig::default()
        });
        assert!(validate_rule_set(&bad).is_err(), "未知字段路径被拒");

        let mut no_topic = config.clone();
        no_topic.rules.push(RuleConfig {
            id: "no-topic".to_string(),
            name: "no-topic".to_string(),
            actions: vec![Action::Publish {
                topic: String::new(),
            }],
            ..RuleConfig::default()
        });
        assert!(validate_rule_set(&no_topic).is_err(), "空 topic 被拒");

        let mut cyclic = config.clone();
        cyclic.rules.push(RuleConfig {
            id: "dup".to_string(),
            name: "dup".to_string(),
            actions: vec![Action::Publish {
                topic: "gw/dup".to_string(),
            }],
            depends_on: vec!["self".to_string()],
            ..RuleConfig::default()
        });
        assert!(validate_rule_set(&cyclic).is_err(), "重复 id 被拒");
    }

    /// `to_rule()` 把 `select` / `depends_on` 一并交给引擎（不静默丢弃）。
    #[test]
    fn to_rule_preserves_engine_fields() {
        let rule = RuleConfig {
            id: "r".to_string(),
            name: "r".to_string(),
            select: vec!["value".to_string()],
            depends_on: vec!["other".to_string()],
            ..RuleConfig::default()
        };
        let engine_rule = rule.to_rule();
        assert_eq!(engine_rule.select, vec!["value".to_string()]);
        assert_eq!(engine_rule.depends_on, vec!["other".to_string()]);
    }
}

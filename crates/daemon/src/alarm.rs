//! 告警引擎（`[alarms]` 段驱动）——web-console「告警中心」页（`/alarms`）的
//! 真实数据源。
//!
//! ## 契约
//!
//! 前端（`web-console/src/pages/AlarmsPage.vue`）的告警规则行契约是
//! `{id, name, condition, duration_sec, suppress_min, hit_count, enabled}`，
//! 其中 **`condition` 是用户唯一填写的条件原文**（形如 `[T_Barrel1] > 240`）。
//! 因此本模块的规则视图同时承认两种写法：
//!
//! - **表达式写法**（前端在用的）：`condition = "[T_Barrel1] > 240"`，由
//!   [`parse_condition`] 解析出点位 / 运算符 / 阈值；
//! - **结构化写法**（配置与 API 的显式字段）：`op` + `threshold` + `point_id`，
//!   解析失败时以结构化字段为准。
//!
//! 两者给出同一个求值器（[`AlarmRule::condition_matches`]）。
//!
//! ## 状态机（每条规则 × 每个点位一条轨道）
//!
//! ```text
//!                 条件首次满足 ──(duration_ms 持续)──▶ armed ──▶ active(open)
//!   空闲 idle ◀──────────────────────────────────────────────────────┘
//!      │                                                            │
//!      │ 条件不再满足（且已 active）→ 置 resolved                      │
//!      └────────────────────────────────────────────────────────────┘
//! ```
//!
//! - `duration_ms`：条件**连续**满足达该时长才真正触发（`0` = 立即触发）。
//!   中途条件回落的未成形窗口直接丢弃，不触发（避免毛刺刷屏）；
//! - `suppress_ms`：一次触发后的冷却窗口，窗口内**不重复触发**（`count` 不增），
//!   但活跃记录的 `last_seen_at` 照常续期（真实在线状态，不伪造停止触发）；
//! - 抑制只压制「新触发」，**不压制条件回落导致的 resolved**——否则告警永远
//!   恢复不了。
//!
//! ## 诚实限制（不伪造）
//!
//! - 告警记录**只能由真实样本产生**：引擎没有数据源时 [`AlarmEngine::evaluate`]
//!   返回空，`GET /api/alerts` 就是空列表 + 如实原因，绝不本地拼装假告警；
//! - 记录保存在**进程内**（[`AlarmStore`]）：进程重启即清空。这是内存态 historian
//!   的有意取舍——持久化告警历史需要一张新表与回滚路径，不在此 ticket 范围内，
//!   在 [`AlarmStore`] 文档里已如实标注；
//! - `[alarms]` 未配置 / `enabled = false` / 段缺省时引擎为**空装配**，数据面
//!   每拍零开销（不构建、不求值）；
//! - 规则语义非法（无法解析出可求值条件）的告警规则在装配期被**跳过并 warn**，
//!   绝不静默落进引擎。
//!
//! ## 时间口径
//!
//! 引擎内部一律用**毫秒**（与配置 `duration_ms` / `suppress_ms` 同量纲）；
//! 记录里的 `first_seen_at` / `last_seen_at` 也是**毫秒 epoch 字符串**——前端
//! `formatEpochText`（`repo.ts:137`）只认 10 位秒 / 13 位毫秒，纳秒串会被原样
//! 回显成不可读数字。守大数红线：时间戳**一律字符串**，绝不发 number。

use serde::Serialize;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::{Arc, Mutex as StdMutex, MutexGuard};

use crate::config::{AlarmRuleConfig, AlarmsSection, GatewayConfig};
use crate::pipeline::ProcessedSample;

/// 告警级别取值域（与前端 `ALARM_LEVEL_LABELS` 的键一致）。
pub const ALARM_LEVELS: &[&str] = &["critical", "major", "minor", "warning"];

/// 告警状态取值域（与前端 `ALARM_STATE_LABELS` 的键一致）。
pub const ALARM_STATES: &[&str] = &["open", "acking", "resolved"];

/// 告警记录条数上限（进程内存态 historian 的硬边界，防采样长期运行撑爆内存）。
///
/// 超限时从**最旧**的开始丢弃——活跃告警永远比历史新，丢新会让「当前有几条
/// 待处理」失真，丢旧不会。
const RECORD_LIMIT: usize = 500;

/// 锁中毒取回内部数据（零 panic 口径，与 `dataplane::lock_or_recover` 一致）。
fn lock_or_recover<T>(mutex: &StdMutex<T>) -> MutexGuard<'_, T> {
    match mutex.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

// ---- 比较运算符 ----

/// 告警比较运算符（`ALARM_RULE_OPS` 的求值侧）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Compare {
    /// `>`。
    Gt,
    /// `>=`。
    Ge,
    /// `<`。
    Lt,
    /// `<=`。
    Le,
    /// `==`。
    Eq,
    /// `!=`。
    Ne,
}

impl Compare {
    /// 运算符的 snake_case wire 名（与 `config::ALARM_RULE_OPS` 同值域）。
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Gt => "gt",
            Self::Ge => "ge",
            Self::Lt => "lt",
            Self::Le => "le",
            Self::Eq => "eq",
            Self::Ne => "ne",
        }
    }

    /// 运算符的人读名（告警详情用）。
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Gt => ">",
            Self::Ge => ">=",
            Self::Lt => "<",
            Self::Le => "<=",
            Self::Eq => "==",
            Self::Ne => "!=",
        }
    }

    /// `actual OP threshold` 是否满足。
    ///
    /// `NaN` 一律判为**不满足**：质量无效 / 计算失败的样本不得触发告警（那是
    /// 质量语义的活，不是阈值语义的活）。
    #[must_use]
    pub fn matches(self, actual: f64, threshold: f64) -> bool {
        if actual.is_nan() || threshold.is_nan() {
            return false;
        }
        match self {
            Self::Gt => actual > threshold,
            Self::Ge => actual >= threshold,
            Self::Lt => actual < threshold,
            Self::Le => actual <= threshold,
            Self::Eq => actual == threshold,
            Self::Ne => actual != threshold,
        }
    }
}

/// 由配置里的 op 字符串取运算符；不在 [`crate::config::ALARM_RULE_OPS`] 值域内
/// 返回 `None`（调用方据此跳过该规则，绝不回退成「恒满足」）。
#[must_use]
pub fn parse_op(raw: &str) -> Option<Compare> {
    match raw.trim() {
        "gt" => Some(Compare::Gt),
        "ge" => Some(Compare::Ge),
        "lt" => Some(Compare::Lt),
        "le" => Some(Compare::Le),
        "eq" => Some(Compare::Eq),
        "ne" => Some(Compare::Ne),
        _ => None,
    }
}

// ---- 条件表达式解析 ----

/// 条件表达式解析结果（`[POINT] OP NUMBER` → 可求值三元组）。
#[derive(Debug, Clone)]
pub struct ConditionSpec {
    /// 绑定点位 id（`None` = 未限定，任意点位参与求值）。
    pub point_id: Option<String>,
    /// 运算符。
    pub op: Compare,
    /// 阈值。
    pub threshold: f64,
}

/// 解析 `[T_Barrel1] > 240` / `> 240` / `T_Barrel1 >= 3.5` 这类条件原文。
///
/// 前端表单（`AlarmsPage.vue:790-797`）只允许「含比较运算符 + 一个数值阈值」，
/// 所以这里只认这一种形状，其余原样返回错误描述（由调用方决定跳过还是 400）。
pub fn parse_condition(raw: &str) -> Result<ConditionSpec, String> {
    let text = raw.trim();
    let mut rest = text;
    let point_id = if let Some(inner) = text.strip_prefix('[') {
        let Some((point, tail)) = inner.split_once(']') else {
            return Err(format!("condition {text:?} has an unclosed \"[\""));
        };
        let point = point.trim();
        if point.is_empty() {
            return Err(format!("condition {text:?} has an empty point bracket"));
        }
        rest = tail.trim();
        Some(point.to_string())
    } else {
        None
    };
    if rest.is_empty() {
        return Err(format!("condition {text:?} has no comparison"));
    }
    // 运算符取**最左**一个，优先按双字符理解（`>=` / `<=` / `==` / `!=`），
    // 否则 `>=` 会被当成 `>` 后面跟一个无意义的 `=`。
    let mut found: Option<(Compare, usize, usize)> = None;
    for (idx, ch) in rest.char_indices() {
        if !matches!(ch, '>' | '<' | '=' | '!') {
            continue;
        }
        // 取最左一个运算符字符，但**按双字符优先理解**：`>=` / `<=` / `==` /
        // `!=` 不能被拆成 `>` + 一个无意义的 `=`（那样 `>= 3.5` 会变成「> 3.5」，
        // 阈值边界上的样本告警错一边）。
        let next = rest[idx..].chars().nth(1);
        let (op, width) = match (ch, next) {
            ('>', Some('=')) => (Compare::Ge, 2),
            ('>', _) => (Compare::Gt, 1),
            ('<', Some('=')) => (Compare::Le, 2),
            ('<', _) => (Compare::Lt, 1),
            ('!', Some('=')) => (Compare::Ne, 2),
            ('=', Some('=')) => (Compare::Eq, 2),
            ('=', _) => (Compare::Eq, 1),
            _ => (Compare::Ne, 1),
        };
        found = Some((op, idx, width));
        break;
    }
    let Some((op, idx, op_width)) = found else {
        return Err(format!(
            "condition {text:?} has no comparison operator (>, >=, <, <=, =, !=)"
        ));
    };
    let threshold_text = rest[idx + op_width..].trim();
    let threshold = threshold_text
        .parse::<f64>()
        .map_err(|_| format!("condition {text:?} threshold {threshold_text:?} is not a number"))?;
    Ok(ConditionSpec {
        point_id,
        op,
        threshold,
    })
}

// ---- 运行时规则 ----

/// 求值期告警规则（由 [`AlarmRuleConfig`] 装配，语义非法者在装配期被丢弃）。
#[derive(Debug, Clone)]
pub struct AlarmRule {
    /// 规则主键。
    pub id: String,
    /// 规则名称（展示 + 二次确认回显）。
    pub name: String,
    /// 告警级别（缺省 `minor`）。
    pub level: String,
    /// 来源类型（缺省 `device`）。
    pub source_type: String,
    /// 条件原文（配置 / 列表回显用）。
    pub condition: String,
    /// 绑定设备 id（`None` = 不限定设备）。
    pub device_id: Option<String>,
    /// 绑定点位 id（`None` = 不限定点位）。
    pub point_id: Option<String>,
    /// 比较运算符（与 [`Self::threshold`] 成对；恒真规则不存在）。
    pub op: Compare,
    /// 阈值。
    pub threshold: f64,
    /// 持续满足时长（毫秒）。
    pub duration_ms: u64,
    /// 触发后抑制窗口（毫秒）。
    pub suppress_ms: u64,
}

impl AlarmRule {
    /// 该样本是否落在本规则的绑定范围内（设备 / 点位可各自缺省）。
    #[must_use]
    pub fn applies_to(&self, sample: &ProcessedSample) -> bool {
        if let Some(device_id) = &self.device_id {
            if device_id != &sample.device_id {
                return false;
            }
        }
        if let Some(point_id) = &self.point_id {
            if point_id != &sample.point_id {
                return false;
            }
        }
        true
    }

    /// 样本值是否满足本规则的条件。
    #[must_use]
    pub fn condition_matches(&self, value: f64) -> bool {
        self.op.matches(value, self.threshold)
    }

    /// 人读条件（无 `condition` 原文时按结构化字段拼一份，不退化成占位串）。
    #[must_use]
    pub fn describe(&self) -> String {
        if self.condition.trim().is_empty() {
            return format!(
                "{} {} {}",
                self.point_id.as_deref().unwrap_or("*"),
                self.op.label(),
                self.threshold
            );
        }
        self.condition.clone()
    }
}

/// [`AlarmRuleConfig`] → 求值期规则；返回 `Err` 表示**这条规则不可用**（调用方
/// 负责跳过 + 记录，绝不静默塞进引擎）。
pub fn rule_from_config(raw: &AlarmRuleConfig) -> Result<AlarmRule, String> {
    let id = raw.id.trim().to_string();
    if id.is_empty() {
        return Err("alarm rule without id".to_string());
    }
    let name = raw
        .name
        .clone()
        .filter(|text| !text.trim().is_empty())
        .unwrap_or_else(|| id.clone());
    let parsed = raw
        .condition
        .as_deref()
        .filter(|text| !text.trim().is_empty())
        .map(parse_condition)
        .transpose()
        .map_err(|err| format!("alarm rule {id:?}: {err}"))?;
    // **条件原文优先于结构化字段**：`condition` 是用户在页面上唯一填写、也是列表
    // 回显的那一份（`AlarmsPage.vue` 只发 `condition`）。若让结构化的 `threshold`
    // 压过它，就会出现「页面显示 `> 40000`、引擎按 `-1` 判」的自相矛盾记录。
    // 结构化字段只在没有 `condition` 时兜底（纯 TOML 手写配置）。
    let op = parsed
        .as_ref()
        .map(|parsed| parsed.op)
        .or_else(|| raw.op.as_deref().and_then(parse_op));
    let Some(op) = op else {
        return Err(format!(
            "alarm rule {id:?}: unknown comparison operator (op must be one of gt/ge/lt/le/eq/ne)"
        ));
    };
    let threshold = parsed
        .as_ref()
        .map(|parsed| parsed.threshold)
        .filter(|value| value.is_finite())
        .or_else(|| raw.threshold.filter(|value| value.is_finite()));
    let Some(threshold) = threshold else {
        return Err(format!(
            "alarm rule {id:?}: threshold is required (numeric `threshold` or a `condition` with a number)"
        ));
    };
    let point_id = parse_point_id(raw, parsed.as_ref());
    let level = raw
        .level
        .as_deref()
        .filter(|level| ALARM_LEVELS.contains(level))
        .unwrap_or("minor")
        .to_string();
    Ok(AlarmRule {
        id,
        name,
        level,
        source_type: raw
            .source_type
            .as_deref()
            .filter(|value| !value.is_empty())
            .unwrap_or("device")
            .to_string(),
        condition: raw.condition.clone().unwrap_or_default(),
        device_id: raw.device_id.clone().filter(|value| !value.is_empty()),
        point_id,
        op,
        threshold,
        duration_ms: raw.duration_ms.unwrap_or(0),
        suppress_ms: raw.suppress_ms.unwrap_or(0),
    })
}

/// 点位绑定：显式 `point_id` 优先，其次是条件表达式里的 `[...]`。
fn parse_point_id(raw: &AlarmRuleConfig, parsed: Option<&ConditionSpec>) -> Option<String> {
    if let Some(point_id) = raw.point_id.as_deref().filter(|p| !p.is_empty()) {
        return Some(point_id.to_string());
    }
    parsed
        .and_then(|parsed| parsed.point_id.as_deref())
        .map(str::to_string)
}

// ---- 引擎 ----

/// 毫秒（`u64` 配置值）→ 内部时间轴（`i64`，与纳秒换算后的毫秒同量纲）。
///
/// 配置里的 `duration_ms` / `suppress_ms` 是 `u64`；内部一律用 `i64` 与
/// 时间戳做减法（`saturating_sub` 防下溢饱和）。超大值按 `i64::MAX` 饱和，
/// 不截断也不 panic（配置侧的上限校验在 `mgmt::alerts_api`）。
fn ms_to_i64(ms: u64) -> i64 {
    if ms > i64::MAX as u64 {
        i64::MAX
    } else {
        ms as i64
    }
}

/// 单条规则 × 单条点位轨道的运行态。
#[derive(Debug, Default)]
struct RuleTrack {
    /// 条件**首次**满足的时刻（毫秒；去抖起点；条件回落即清空）。
    armed_since_ms: Option<i64>,
    /// 抑制窗口截止（毫秒）。
    suppress_until_ms: i64,
    /// 当前活跃的告警记录（`None` = 无活跃告警）。
    active: Option<AlarmRecord>,
}

impl RuleTrack {
    /// 推进一条轨道一个采样点，返回**应当落库的记录**（无变化则 `None`）。
    #[allow(clippy::too_many_arguments)]
    fn step(
        &mut self,
        rule: &AlarmRule,
        sample: &ProcessedSample,
        violated: bool,
        now_ms: i64,
    ) -> Option<AlarmRecord> {
        if violated {
            let suppressed = self.suppress_until_ms > now_ms;
            if let Some(active) = self.active.as_mut() {
                // 活跃中：续期真实在线状态（不增 count——同一次触发的持续期）。
                active.last_seen_at = now_ms.to_string();
                return Some(active.clone());
            }
            if suppressed {
                // 抑制窗口内：照常武装（窗口一过、条件仍成立就立刻触发），
                // 但本次不触发、不建记录（避免刷屏式重复告警）。
                if self.armed_since_ms.is_none() {
                    self.armed_since_ms = Some(now_ms);
                }
                return None;
            }
            let start = match self.armed_since_ms {
                Some(start) => start,
                None => now_ms,
            };
            let duration = ms_to_i64(rule.duration_ms);
            let settled = duration == 0 || now_ms.saturating_sub(start) >= duration;
            if !settled {
                // 还没持续够 duration_ms：不算触发，但要把起点**记住**（否则每次
                // 采样都从「本次」重新计时，`duration_ms > 0` 的规则永远响不了）。
                // 条件回落时轨道会把这个起点清掉，语义就是「须连续满足」。
                if self.armed_since_ms.is_none() {
                    self.armed_since_ms = Some(start);
                }
                return None;
            }
            self.armed_since_ms = None;
            self.suppress_until_ms = now_ms.saturating_add(ms_to_i64(rule.suppress_ms));
            let record = new_record(rule, sample, start, now_ms);
            self.active = Some(record.clone());
            return Some(record);
        }

        // 条件不再满足：活跃中的告警到此结束（resolved），未成形的去抖窗口丢弃。
        if let Some(active) = self.active.take() {
            self.armed_since_ms = None;
            let mut resolved = active;
            resolved.last_seen_at = now_ms.to_string();
            resolved.state = "resolved".to_string();
            return Some(resolved);
        }
        self.armed_since_ms = None;
        None
    }
}

/// 告警记录（引擎产出 / 管理面回显）。
///
/// 字段命名走 camelCase（`sourceType` / `firstSeenAt` …），与 `AlarmsPage.vue`
/// 的 `mapAlarmRow` 逐字对齐；时间戳**一律毫秒字符串**（大数红线）。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AlarmRecord {
    /// 记录主键（规则 × 设备 × 点位，稳定——同一次告警反复触发沿用同一 id）。
    pub id: String,
    /// 触发它的规则 id。
    pub rule_id: String,
    /// 本次告警绑定的设备（不限定时 = 网关标识或 `*`，如实标注）。
    pub device_id: String,
    /// 触发点位（不限定时为空串，不做无意义的通配占位）。
    pub point_id: String,
    /// 级别（`critical` / `major` / `minor` / `warning`）。
    pub level: String,
    /// 标题（前端列表首列）。
    pub title: String,
    /// 详情（阈值 / 实际值 / 条件原文）。
    pub detail: String,
    /// 来源类型。
    pub source_type: String,
    /// 来源标签（列表副标题）。
    pub source_label: String,
    /// 状态（`open` / `acking` / `resolved`）。
    pub state: String,
    /// 首次触发（毫秒 epoch 字符串）。
    pub first_seen_at: String,
    /// 最近一次（毫秒 epoch 字符串）。
    pub last_seen_at: String,
    /// 触发次数（引擎真实记账：每次由非活跃转活跃 +1）。
    pub count: u64,
    /// 确认人（空 = 未确认）。
    pub acked_by: String,
    /// 处置说明。
    pub note: String,
}

/// 由规则 + 样本构造一条新告警记录。
fn new_record(
    rule: &AlarmRule,
    sample: &ProcessedSample,
    first_seen_ms: i64,
    now_ms: i64,
) -> AlarmRecord {
    let point_label = rule
        .point_id
        .as_deref()
        .unwrap_or(sample.point_id.as_str())
        .to_string();
    let point = point_label.as_str();
    AlarmRecord {
        id: format!("{}|{}|{}", rule.id, sample.device_id, point),
        rule_id: rule.id.clone(),
        device_id: sample.device_id.clone(),
        point_id: point_label.clone(),
        level: rule.level.clone(),
        title: rule.name.clone(),
        detail: format!(
            "{} {} {}（实际 {}{}）",
            point,
            rule.op.label(),
            rule.threshold,
            sample.value,
            if sample.unit.is_empty() {
                String::new()
            } else {
                format!(
                    " {}{}",
                    if sample.unit.starts_with(' ') {
                        ""
                    } else {
                        " "
                    },
                    sample.unit
                )
            }
        ),
        source_type: rule.source_type.clone(),
        source_label: sample.device_id.clone(),
        state: "open".to_string(),
        first_seen_at: first_seen_ms.to_string(),
        last_seen_at: now_ms.to_string(),
        count: 1,
        acked_by: String::new(),
        note: String::new(),
    }
}

/// 告警引擎：持有规则集与各轨道运行态，逐样本推进。
#[derive(Debug, Default)]
pub struct AlarmEngine {
    /// 已启用的规则（按配置顺序求值）。
    rules: Vec<AlarmRule>,
    /// 轨道运行态（`rule_id + device_id + point_id` → 轨道）。
    tracks: HashMap<String, RuleTrack>,
}

impl AlarmEngine {
    /// 由 `[alarms]` 段装配；只吃 **enabled** 规则，非法规则跳过（返回被跳过的
    /// 规则 id 供调用方 log）。
    ///
    /// `[alarms]` 缺省或 `enabled = false` → 空引擎（数据面据此跳过整个告警阶段，
    /// 热路径零开销）。
    #[must_use]
    pub fn from_config(config: &GatewayConfig) -> (Self, Vec<String>) {
        let mut rules: Vec<AlarmRule> = Vec::new();
        let mut skipped: Vec<String> = Vec::new();
        let Some(section) = config.alarms.as_ref() else {
            return (Self::default(), skipped);
        };
        if !section.enabled {
            return (Self::default(), skipped);
        }
        for raw in &section.rules {
            if !raw.enabled {
                continue;
            }
            match rule_from_config(raw) {
                Ok(rule) => rules.push(rule),
                Err(err) => {
                    skipped.push(err.clone());
                    tracing::warn!(error = %err, "alarm: rule skipped by the engine");
                }
            }
        }
        (
            Self {
                rules,
                ..Self::default()
            },
            skipped,
        )
    }

    /// 仅由 `[alarms]` 段装配（单测 / 端点回显用）。
    #[must_use]
    pub fn from_section(section: &AlarmsSection) -> (Self, Vec<String>) {
        let config = crate::config::GatewayConfig {
            alarms: Some(section.clone()),
            ..Default::default()
        };
        Self::from_config(&config)
    }

    /// 已装配的启用规则数。
    #[must_use]
    pub fn rule_count(&self) -> usize {
        self.rules.len()
    }

    /// 规则集（回显 / 测试用）。
    #[must_use]
    pub fn rules(&self) -> &[AlarmRule] {
        &self.rules
    }

    /// 轨道键。
    fn track_key(rule: &AlarmRule, sample: &ProcessedSample) -> String {
        format!("{}|{}|{}", rule.id, sample.device_id, sample.point_id)
    }

    /// 对一个样本求值一次，返回**需要落库**的记录（新触发 / 续期 / 恢复）。
    ///
    /// 返回空向量 = 本样本对告警无任何影响（无绑定规则 / 抑制中未成形 /
    /// 去抖未满）。调用方（数据面）据此写入 [`AlarmStore`]。
    #[must_use]
    pub fn evaluate(&mut self, sample: &ProcessedSample, now_ns: i64) -> Vec<AlarmRecord> {
        if self.rules.is_empty() {
            return Vec::new();
        }
        let now_ms = now_ns / 1_000_000;
        let mut changed: Vec<AlarmRecord> = Vec::new();
        for rule in &self.rules {
            if !rule.applies_to(sample) {
                continue;
            }
            let key = Self::track_key(rule, sample);
            let violated = rule.condition_matches(sample.value);
            let track = self.tracks.entry(key).or_default();
            if let Some(record) = track.step(rule, sample, violated, now_ms) {
                changed.push(record);
            }
        }
        changed
    }

    /// 规则集变化（配置热重载）后清空运行态：旧轨道的「已持续时长 / 抑制窗口」
    /// 对新规则毫无意义，留着只会让新规则的第一次触发被旧窗口吞掉。
    pub fn reset_tracks(&mut self) {
        self.tracks.clear();
    }
}

// ---- 记录仓库 ----

/// 告警记录仓库（进程内 historian，跨引擎重建存活）。
///
/// ⚠️ **诚实限制**：记录只在内存里，进程重启即清空。告警库落盘需要一张新表 +
/// 迁移 + 回滚路径，不在此 ticket 范围；在这里如实标注而不是假装持久。
#[derive(Debug, Default)]
pub struct AlarmStore {
    inner: StdMutex<Vec<AlarmRecord>>,
}

impl AlarmStore {
    /// 建一份共享仓库（挂在 `DaemonShared` 上，数据面写、管理面读）。
    #[must_use]
    pub fn shared() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// 写入一批记录（按 `id` 覆盖同 id 的旧记录；新记录追加）。
    ///
    /// 超出上限时从最旧的开始丢弃（活跃告警永远比历史新，丢新会让「当前有几条
    /// 待处理」失真）。
    pub fn upsert(&self, records: &[AlarmRecord]) {
        let mut rows = lock_or_recover(&self.inner);
        for incoming in records {
            match rows.iter_mut().find(|row| row.id == incoming.id) {
                Some(existing) => *existing = incoming.clone(),
                None => rows.push(incoming.clone()),
            }
        }
        while rows.len() > RECORD_LIMIT {
            rows.remove(0);
        }
    }

    /// 全量快照（最新在前；`source = "ok"`）。
    #[must_use]
    pub fn snapshot(&self) -> Vec<AlarmRecord> {
        let rows = lock_or_recover(&self.inner);
        let mut out = rows.clone();
        out.reverse();
        out
    }

    /// 按 id 取一条（处置端点用）。
    #[must_use]
    pub fn find(&self, id: &str) -> Option<AlarmRecord> {
        let rows = lock_or_recover(&self.inner);
        rows.iter().find(|row| row.id == id).cloned()
    }

    /// 按 id 改处置状态（`open` → `acking` / `resolved`）。
    ///
    /// 返回更新后的记录；`None` = 无此记录（调用方 404，**绝不凭 id 造一条**）。
    pub fn set_state(&self, id: &str, state: &str, actor: &str, note: &str) -> Option<AlarmRecord> {
        let mut rows = lock_or_recover(&self.inner);
        let found = rows.iter_mut().find(|row| row.id == id)?;
        found.state = state.to_string();
        found.acked_by = actor.to_string();
        found.note = note.to_string();
        Some(found.clone())
    }

    /// 记录条数（诊断 / 测试用）。
    #[must_use]
    pub fn len(&self) -> usize {
        lock_or_recover(&self.inner).len()
    }

    /// 无记录（`len` 的配套判据，`clippy::len_without_is_empty`）。
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// 告警记录 → `GET /api/alerts` 的 JSON 行。
#[must_use]
pub fn record_to_wire(record: &AlarmRecord) -> Value {
    json!({
        "id": record.id,
        "level": record.level,
        "title": record.title,
        "detail": record.detail,
        "source_type": record.source_type,
        "source_label": record.source_label,
        "state": record.state,
        // 毫秒 epoch 字符串：前端 formatEpochText 只认 10 位秒 / 13 位毫秒。
        "first_seen_at": record.first_seen_at,
        "last_seen_at": record.last_seen_at,
        "count": record.count,
        "acked_by": record.acked_by,
        "note": record.note,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(value: f64, device: &str, point: &str) -> ProcessedSample {
        ProcessedSample {
            device_id: device.to_string(),
            point_id: point.to_string(),
            value,
            unit: String::new(),
            device_ts_ns: None,
            collected_ts_ns: 1_700_000_000_000_000_000,
            quality: protocol_proto::Quality::Good,
        }
    }

    fn section(rules: Vec<AlarmRuleConfig>) -> AlarmsSection {
        AlarmsSection {
            enabled: true,
            rules,
        }
    }

    fn rule(id: &str, condition: &str) -> AlarmRuleConfig {
        AlarmRuleConfig {
            id: id.to_string(),
            name: Some(id.to_string()),
            condition: Some(condition.to_string()),
            ..AlarmRuleConfig::default()
        }
    }

    // ---- 条件解析 ----

    #[test]
    fn condition_parses_point_bracket_expression() {
        let parsed = parse_condition("[T_Barrel1] > 240").expect("condition must parse");
        assert_eq!(parsed.point_id.as_deref(), Some("T_Barrel1"));
        assert_eq!(parsed.op, Compare::Gt);
        assert!((parsed.threshold - 240.0).abs() < 1e-9);
    }

    #[test]
    fn condition_reads_two_char_operators_first() {
        // `>=` 不能被拆成 `>` + 无意义的 `=`。
        for (raw, op, threshold) in [
            ("[P] >= 3.5", Compare::Ge, 3.5),
            ("[P] <= 3.5", Compare::Le, 3.5),
            ("[P] == 3.5", Compare::Eq, 3.5),
            ("[P] != 3.5", Compare::Ne, 3.5),
        ] {
            let parsed = parse_condition(raw).unwrap_or_else(|err| panic!("{raw} → {err}"));
            assert_eq!(parsed.op, op, "{raw}");
            assert!((parsed.threshold - threshold).abs() < 1e-9, "{raw}");
        }
    }

    #[test]
    fn condition_without_operator_or_number_is_rejected() {
        assert!(parse_condition("[P] 240").is_err());
        assert!(parse_condition("[P > 240").is_err());
        assert!(parse_condition("[P] > abc").is_err());
    }

    #[test]
    fn compare_treats_nan_as_not_violated() {
        // 坏值样本不得触发阈值告警（那是质量语义的活）。
        for op in [
            Compare::Gt,
            Compare::Ge,
            Compare::Lt,
            Compare::Le,
            Compare::Eq,
            Compare::Ne,
        ] {
            assert!(!op.matches(f64::NAN, 0.0), "{}", op.as_str());
        }
    }

    // ---- 规则装配 ----

    #[test]
    fn rule_from_config_accepts_expression_wire_form() {
        let rule = rule_from_config(&rule("r1", "[T_Barrel1] > 240")).expect("rule must build");
        assert_eq!(rule.point_id.as_deref(), Some("T_Barrel1"));
        assert_eq!(rule.op, Compare::Gt);
        assert!((rule.threshold - 240.0).abs() < 1e-9);
        assert_eq!(rule.level, "minor", "缺省级别");
        assert_eq!(rule.source_type, "device", "缺省来源类型");
    }

    #[test]
    fn rule_from_config_prefers_condition_over_explicit_fields() {
        // 页面只发 `condition`，回显也是它；结构化字段若压过它，就会出现
        // 「显示 `> 40000`、引擎按旧阈值判」的自相矛盾记录。
        let mut raw = rule("r2", "[T_Barrel1] > 240");
        raw.op = Some("lt".to_string());
        raw.threshold = Some(10.0);
        let rule = rule_from_config(&raw).expect("rule must build");
        assert_eq!(rule.op, Compare::Gt, "条件原文说了算");
        assert!((rule.threshold - 240.0).abs() < 1e-9, "条件原文说了算");
        assert_eq!(rule.point_id.as_deref(), Some("T_Barrel1"));
    }

    #[test]
    fn rule_from_config_falls_back_to_explicit_fields() {
        // 纯 TOML 手写配置没有 `condition`，此时结构化字段是唯一判据。
        let mut raw = rule("r5", "");
        raw.op = Some("lt".to_string());
        raw.threshold = Some(10.0);
        raw.point_id = Some("T_Barrel1".to_string());
        let rule = rule_from_config(&raw).expect("rule must build");
        assert_eq!(rule.op, Compare::Lt);
        assert!((rule.threshold - 10.0).abs() < 1e-9);
        assert_eq!(rule.point_id.as_deref(), Some("T_Barrel1"));
    }

    #[test]
    fn rule_without_operator_or_threshold_is_unusable() {
        let mut raw = rule("r3", "");
        raw.threshold = None;
        assert!(rule_from_config(&raw).is_err(), "无阈值 = 永不为真的死规则");

        let mut bad_op = rule("r4", "");
        bad_op.op = Some("approx".to_string());
        assert!(rule_from_config(&bad_op).is_err());
    }

    #[test]
    fn rule_without_id_is_unusable() {
        let raw = rule("", "[P] > 1");
        assert!(rule_from_config(&raw).is_err());
    }

    // ---- 引擎行为 ----

    #[test]
    fn engine_skips_disabled_and_unusable_rules() {
        let mut disabled = rule("off", "[P] > 1");
        disabled.enabled = false;
        let mut broken = rule("broken", "not a condition");
        broken.threshold = None;

        let (engine, skipped) =
            AlarmEngine::from_section(&section(vec![rule("ok", "[P] > 1"), disabled, broken]));

        assert_eq!(engine.rule_count(), 1, "只装配可求值的启用规则");
        assert_eq!(skipped.len(), 1, "非法规则如实上报被跳过");
    }

    #[test]
    fn disabled_section_builds_empty_engine() {
        let (engine, skipped) = AlarmEngine::from_section(&AlarmsSection {
            enabled: false,
            rules: vec![rule("r", "[P] > 1")],
        });
        assert_eq!(engine.rule_count(), 0);
        assert!(skipped.is_empty());
    }

    #[test]
    fn engine_fires_once_and_renews_while_violated() {
        let mut engine = AlarmEngine::from_section(&section(vec![rule("r", "[T1] > 240")])).0;
        let fired = engine.evaluate(&sample(300.0, "gw", "T1"), 1_700_000_000_000_000_000);
        assert_eq!(fired.len(), 1, "首次满足即触发");
        assert_eq!(fired[0].state, "open");
        assert_eq!(fired[0].count, 1);
        assert_eq!(fired[0].first_seen_at, fired[0].last_seen_at);

        let fired_again = engine.evaluate(&sample(310.0, "gw", "T1"), 1_700_000_001_000_000_000);
        assert_eq!(fired_again.len(), 1, "续期同一条记录（不增 count）");
        assert_eq!(fired_again[0].count, 1, "count 只记触发次数，不记采样数");
        assert_eq!(
            fired_again[0].last_seen_at, "1700000001000",
            "last_seen_at 走毫秒"
        );
    }

    #[test]
    fn engine_dedups_by_rule_device_and_point() {
        // 条件不绑点位（无 `[...]`，否则规则只认那一个点位）——绑定缺省时
        // 「规则 × 设备 × 点位」才是轨道维度。
        let mut engine = AlarmEngine::from_section(&section(vec![rule("r", "> 240")])).0;
        let a = engine.evaluate(&sample(300.0, "gw", "T1"), 1_700_000_000_000_000_000);
        let b = engine.evaluate(&sample(300.0, "gw", "T2"), 1_700_000_000_000_000_000);
        assert_eq!(a.len(), 1);
        assert_eq!(b.len(), 1, "同一规则的其他点位是另一条轨道");
        assert_ne!(a[0].id, b[0].id, "轨道维度 = 规则 × 设备 × 点位");
    }

    #[test]
    fn engine_resolves_when_condition_clears() {
        let mut engine = AlarmEngine::from_section(&section(vec![rule("r", "[T1] > 240")])).0;
        let _opened = engine.evaluate(&sample(300.0, "gw", "T1"), 1_700_000_000_000_000_000);
        assert_eq!(_opened.len(), 1);
        let resolved = engine.evaluate(&sample(100.0, "gw", "T1"), 1_700_000_005_000_000_000);
        assert_eq!(resolved.len(), 1);
        assert_eq!(resolved[0].state, "resolved");
        // 恢复后再触发 → 又是**一次新触发**：记录 id 沿用同一次告警（同轨道），
        // `count` 重新从 1 起（一次触发 = 一次计数，不把恢复前后并成「第 2 次」）。
        let refired = engine.evaluate(&sample(300.0, "gw", "T1"), 1_700_000_006_000_000_000);
        assert_eq!(refired[0].state, "open");
        assert_eq!(refired[0].count, 1);
        assert_eq!(
            refired[0].first_seen_at, "1700000006000",
            "恢复后的这次触发是**新一轮**：first_seen 记本轮起点，与 count=1 自洽"
        );
    }

    #[test]
    fn engine_requires_condition_to_hold_for_duration_ms() {
        let mut raw = rule("r", "[T1] > 240");
        raw.duration_ms = Some(5_000);
        let mut engine = AlarmEngine::from_section(&section(vec![raw])).0;

        // 第 0ms 满足 → 武装；第 1000ms 回落 → 未成形的窗口丢弃；
        // 第 3000ms 再满足 → 仍是「首次」计时，不满 5s 不触发。
        assert!(engine
            .evaluate(&sample(300.0, "gw", "T1"), 1_700_000_000_000_000_000)
            .is_empty());
        let _dropped = engine.evaluate(&sample(100.0, "gw", "T1"), 1_700_000_001_000_000_000);
        assert!(engine
            .evaluate(&sample(300.0, "gw", "T1"), 1_700_000_003_000_000_000)
            .is_empty());

        // 连续满足满 5s。
        let fired = engine.evaluate(&sample(300.0, "gw", "T1"), 1_700_000_008_000_000_000);
        assert_eq!(fired.len(), 1, "去抖窗口满了才触发");
    }

    #[test]
    fn engine_suppresses_refire_but_not_recovery() {
        let mut raw = rule("r", "[T1] > 240");
        raw.suppress_ms = Some(10_000);
        let mut engine = AlarmEngine::from_section(&section(vec![raw])).0;

        let first = engine.evaluate(&sample(300.0, "gw", "T1"), 1_700_000_000_000_000_000);
        assert_eq!(first.len(), 1);

        // 抑制窗口内：已 active 的那条照常续期，不新增记录。
        let suppressed = engine.evaluate(&sample(300.0, "gw", "T1"), 1_700_000_002_000_000_000);
        assert_eq!(suppressed.len(), 1);
        assert_eq!(suppressed[0].id, first[0].id);
        assert!(
            suppressed[0].first_seen_at == first[0].first_seen_at
                && suppressed[0].last_seen_at == "1700000002000"
        );

        // 条件回落：resolved 不受抑制影响（否则告警永远恢复不了）。
        let resolved = engine.evaluate(&sample(1.0, "gw", "T1"), 1_700_000_005_000_000_000);
        assert_eq!(resolved[0].state, "resolved");
    }

    #[test]
    fn engine_ignores_samples_outside_rule_binding() {
        let mut raw = rule("r", "[T1] > 240");
        raw.device_id = Some("gw-other".to_string());
        let mut engine = AlarmEngine::from_section(&section(vec![raw])).0;
        assert!(
            engine
                .evaluate(&sample(999.0, "gw", "T1"), 1_700_000_000_000_000_000)
                .is_empty(),
            "设备绑定外的样本不参与求值"
        );
    }

    #[test]
    fn reset_tracks_clears_pending_windows() {
        let mut raw = rule("r", "[T1] > 240");
        raw.duration_ms = Some(5_000);
        let mut engine = AlarmEngine::from_section(&section(vec![raw])).0;
        assert!(
            engine
                .evaluate(&sample(300.0, "gw", "T1"), 1_700_000_000_000_000_000)
                .is_empty(),
            "去抖窗口未满，尚未武装成功"
        );
        engine.reset_tracks();
        // 配置热重载后旧轨道的计时 / 抑制窗口对新规则毫无意义，未成形的进度一并丢弃。
        assert!(
            engine
                .evaluate(&sample(300.0, "gw", "T1"), 1_700_000_001_000_000_000)
                .is_empty(),
            "旧去抖进度不得带到新规则上"
        );
        let fired = engine.evaluate(&sample(300.0, "gw", "T1"), 1_700_000_006_000_000_000);
        assert_eq!(fired.len(), 1, "重新计时满 5s 后照常触发");
    }

    // ---- 记录仓库 ----

    #[test]
    fn store_dedups_by_id_and_keeps_newest_first() {
        let store = AlarmStore::shared();
        let record = AlarmRecord {
            id: "r1|gw|T1".to_string(),
            state: "open".to_string(),
            count: 1,
            ..AlarmRecord::default_for_tests()
        };
        store.upsert(std::slice::from_ref(&record));
        let mut second = record.clone();
        second.id = "r2|gw|T1".to_string();
        second.count = 7;
        store.upsert(&[second]);
        assert_eq!(store.len(), 2);

        store.upsert(std::slice::from_ref(&record));
        assert_eq!(store.len(), 2, "同 id 覆盖，不新增");
        let snapshot = store.snapshot();
        // 仓库按写入顺序追加 → 快照反转后「最后写入的在前」。
        assert_eq!(snapshot[0].id, "r2|gw|T1", "最后写入的在前");
        assert_eq!(snapshot[1].id, record.id);
    }

    #[test]
    fn store_set_state_records_actor_and_note() {
        let store = AlarmStore::shared();
        let record = AlarmRecord {
            id: "r1|gw|T1".to_string(),
            state: "open".to_string(),
            ..AlarmRecord::default_for_tests()
        };
        store.upsert(&[record]);
        let updated = store
            .set_state("r1|gw|T1", "acked", "ops@local", "已现场确认")
            .expect("记录必须存在");
        assert_eq!(updated.state, "acked");
        assert_eq!(updated.acked_by, "ops@local");
        assert_eq!(updated.note, "已现场确认");
        assert!(store.set_state("nope", "acked", "ops@local", "").is_none());
    }

    #[test]
    fn record_wire_has_string_timestamps() {
        let record = AlarmRecord {
            first_seen_at: "1700000000000".to_string(),
            last_seen_at: "1700000001000".to_string(),
            count: 3,
            ..AlarmRecord::default_for_tests()
        };
        let wire = record_to_wire(&record);
        assert!(wire["first_seen_at"].is_string(), "毫秒时间戳走字符串");
        assert!(wire["last_seen_at"].is_string());
        assert_eq!(wire["count"], json!(3));
    }

    /// 时间戳口径的守门条：wire 上的 `first_seen_at` / `last_seen_at` 必须**恒**
    /// 是 13 位毫秒纯数字字符串。
    ///
    /// `AlarmsPage` 的告警时间筛选拿 `last_seen_at` 当**字符串**做字典序比较
    /// （`mapAlarmRow` 用 `pickText` 纯透传，不经过 `repo.ts` 之外的任何格式化），
    /// 因此口径必须是「一直如此」而不能只在一个 happy path 上成立：一旦某条分支
    /// 吐出纳秒串（13 位以上）或 10 位秒串，前端的字典序比较会**静默**错位——
    /// 这种失真不会报错、只会静悄悄把筛选结果算错，所以钉在 wire 层而不是靠上游自查。
    #[test]
    fn wire_timestamps_are_always_thirteen_digit_ms_strings() {
        let samples = [
            ("1700000000000", "1700000001000"),
            ("0000000000001", "9999999999999"),
            ("1712345678901", "1712345678999"),
        ];
        for (first, last) in samples {
            let wire = record_to_wire(&AlarmRecord {
                first_seen_at: first.to_string(),
                last_seen_at: last.to_string(),
                ..AlarmRecord::default_for_tests()
            });
            for (key, expected) in [("first_seen_at", first), ("last_seen_at", last)] {
                let value = wire[key].as_str().unwrap_or_default();
                assert_eq!(
                    value.len(),
                    13,
                    "{key}={value:?} 必须是 13 位毫秒串（前端按字符串字典序比较窗口）"
                );
                assert!(
                    value.chars().all(|c| c.is_ascii_digit()),
                    "{key}={value:?} 必须是纯数字，不能是纳秒/秒串或数字类型"
                );
                assert_eq!(value, expected);
            }
        }

        // 真实引擎产出也必须在同一口径上（构造值对了、引擎吐了纳秒，照样翻车）。
        // 注意evaluate 的时间参数是**纳秒**，内部再折算成毫秒输出。
        let mut engine = AlarmEngine::from_section(&section(vec![rule("r", "[T1] > 240")])).0;
        let fired = engine.evaluate(&sample(300.0, "gw", "T1"), 1_700_000_000_000_000_000);
        assert_eq!(fired.len(), 1, "样本必须真的触发了一条告警");
        for record in fired.iter().map(record_to_wire) {
            for key in ["first_seen_at", "last_seen_at"] {
                let value = record[key].as_str().unwrap_or_default();
                assert!(
                    value.len() == 13 && value.chars().all(|c| c.is_ascii_digit()),
                    "引擎产出的 {key}={value:?} 不是 13 位毫秒串"
                );
            }
        }
    }

    impl AlarmRecord {
        /// 测试用的默认记录（唯一构造点，避免各用例重复补齐 14 个字段）。
        fn default_for_tests() -> Self {
            Self {
                id: String::new(),
                rule_id: String::new(),
                device_id: String::new(),
                point_id: String::new(),
                level: "minor".to_string(),
                title: String::new(),
                detail: String::new(),
                source_type: "device".to_string(),
                source_label: String::new(),
                state: "open".to_string(),
                first_seen_at: String::new(),
                last_seen_at: String::new(),
                count: 0,
                acked_by: String::new(),
                note: String::new(),
            }
        }
    }
}

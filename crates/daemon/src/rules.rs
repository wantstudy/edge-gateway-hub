//! MQTT 转发规则引擎（计划 task 20，Wave 3）。
//!
//! **边界声明**（与 task 15 / task 37 互斥，越界即返工）：
//! - 本模块只交付**规则引擎框架**：规则 JSON schema、解析器、SELECT / WHERE / DO 求值、
//!   Topic 路由与 JSONPath 字段重映射；
//! - **动作集限于 P0 子集**：过滤（WHERE）+ 路由（Publish）+ 字段重映射（Remap）；
//! - `点位映射 / 单位换算 / 死区过滤` **一律复用 task 15 的
//!   [`crate::pipeline::DataProcessor`]**，本模块**不复制**其任何逻辑（见 [`Rule::transform`]）；
//! - **不**签序列化字节（= task 21）；**不**做规则版本管理、规则间组合与循环依赖检测（= task 37）。
//!
//! ## 数据流位置
//!
//! ```text
//! 驱动采样 → RawSample → [task15 DataProcessor：映射/换算/死区/时间统一] → ProcessedSample
//!          → [本模块：WHERE 过滤 → SELECT 投影 → DO 重映射/发布] → RoutedMessage
//!          → [task 19+ 北向 MQTT]
//! ```
//!
//! ## 规则 JSON schema
//!
//! 顶层为规则数组（也接受 `{"rules": [...]}` 对象形式）：
//!
//! ```json
//! [
//!   {
//!     "id": "temp_alarm",
//!     "select": ["$.value", "$.unit"],
//!     "when": {"kind": "cmp", "field": "value", "op": "gt", "value": 30},
//!     "actions": [
//!       {"kind": "remap", "fields": {"t": "$.value"}},
//!       {"kind": "publish", "topic": "alarms"}
//!     ],
//!     "transform": {
//!       "source_id": "40001", "target_point": "line1_m1_temp", "device_id": "line1_m1",
//!       "unit": "degC", "scale": 0.1, "offset": 2.0, "deadband": 0.5
//!     }
//!   }
//! ]
//! ```
//!
//! ## 关键设计决策
//!
//! 1. **数值变换委托 task 15**：`transform` 在构造期即翻译成 [`PointConfig`] 并交给
//!    [`DataProcessor::new`]；`evaluate` 全程不自行做 `value * scale + offset`，
//!    死区也由 `DataProcessor` 判定（`Ok(None)` = 被死区吞掉 → 不产出消息）。
//! 2. **一个 source_id 只能被一条规则声明 `transform`**：重复 `source_id` 由
//!    `DataProcessor` 报 [`DaemonError::ConfigError`] 并原样透传。因此「同一点位多条规则」
//!    **第二条起请省略 `transform`**（无 `transform` 的规则是**通配规则**，对所有样本生效，
//!    样本以直通配置进入路由）。
//! 3. **时间戳是字符串**：payload 中 `device_ts_ns` / `collected_ts_ns` 一律编码为
//!    **JSON 字符串**（项目红线：JSON number 是 IEEE754 double，纳秒时间戳超过 2^53-1 会
//!    静默丢精度）。数值比较时按字符串解析回 `f64`，语义不丢失。
//! 4. **求值顺序**：WHERE（在完整 payload 上）→ SELECT（投影）→ DO（动作按数组顺序执行，
//!    `Remap` 只在其之前的 `Publish` 之后才影响后续 `Publish`）。
//! 5. **构造期校验、运行期不 panic**：未知字段 / 字段类型与比较符不匹配 / 非法 JSONPath /
//!    规则 id 重复 / 空 topic，全部在 `from_json` / `from_rules` 阶段返回 `ConfigError`（错误码 2000）；
//!    运行期路径解析不到值一律视为「不匹配」，绝不 `unwrap` / `expect` / `panic`。
//! 6. **JSONPath 最小子集**：`$` 根 + `.` 分隔字段 + `[n]` 数组下标。payload 是扁平对象，
//!    支持下标仅为健壮性（越界 / 非数字下标在构造期即报 `ConfigError`）。

use std::collections::hash_map::Entry;
use std::collections::{BTreeMap, HashMap, HashSet};

use serde::de::{Deserializer, Error as DeError};
use serde::Deserialize;
use serde_json::{Map, Value};

use crate::error::{DaemonError, DaemonResult};
use crate::pipeline::{DataProcessor, PointConfig, ProcessedSample, RawSample};

// ---- 常量 ----

/// `Transform.unit` 缺省值（未声明单位的原始量值）。
const DEFAULT_UNIT: &str = "raw";
/// `Transform.scale` 缺省值。
const DEFAULT_SCALE: f64 = 1.0;
/// `Transform.offset` 缺省值。
const DEFAULT_OFFSET: f64 = 0.0;
/// `Transform.deadband` 缺省值（0 = 不过滤）。
const DEFAULT_DEADBAND: f64 = 0.0;

/// payload 数值字段（支持全部比较符，比较值须为 JSON number）。
const NUMERIC_FIELDS: [&str; 3] = ["value", "device_ts_ns", "collected_ts_ns"];
/// payload 字符串字段（仅支持 `eq` / `ne`，比较值须为 JSON string）。
const STRING_FIELDS: [&str; 4] = ["device_id", "point_id", "unit", "quality"];
/// payload 全部已知字段（WHERE / SELECT / Remap 路径根段的白名单）。
const KNOWN_FIELDS: [&str; 7] = [
    "device_id",
    "point_id",
    "value",
    "unit",
    "quality",
    "device_ts_ns",
    "collected_ts_ns",
];

/// 统一构造配置错误（错误码 2000，见 [`crate::error::ERR_CONFIG`]）。
fn config_error(message: impl std::fmt::Display) -> DaemonError {
    DaemonError::ConfigError(message.to_string())
}

// ---- 规则模型 ----

/// 数值变换规格：**委托 task 15 的 [`DataProcessor`]**（本模块不实现换算 / 死区）。
///
/// **未知字段一律拒绝**：键拼错必须报错，不得静默忽略导致变换失效。
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Transform {
    /// 输入点位（与 [`RawSample::source_id`] 对应）。
    pub source_id: String,
    /// 输出点位名（即 [`PointConfig::point_id`]）。
    pub target_point: String,
    /// 输出设备标识（即 [`PointConfig::device_id`]）。
    pub device_id: String,
    /// 工程单位；缺省 `"raw"`。
    pub unit: Option<String>,
    /// 换算系数；缺省 `1.0`。
    pub scale: Option<f64>,
    /// 换算偏移；缺省 `0.0`。
    pub offset: Option<f64>,
    /// 绝对死区阈值（工程单位，≥ 0）；缺省 `0.0`（不过滤）。
    pub deadband: Option<f64>,
}

impl Transform {
    /// 翻译为 task 15 的 [`PointConfig`]（缺省值在此归一）。
    fn to_point_config(&self) -> PointConfig {
        PointConfig {
            source_id: self.source_id.clone(),
            point_id: self.target_point.clone(),
            device_id: self.device_id.clone(),
            unit: self
                .unit
                .clone()
                .unwrap_or_else(|| DEFAULT_UNIT.to_string()),
            scale: self.scale.unwrap_or(DEFAULT_SCALE),
            offset: self.offset.unwrap_or(DEFAULT_OFFSET),
            deadband: self.deadband.unwrap_or(DEFAULT_DEADBAND),
        }
    }
}

/// 比较运算符（JSON 名为 snake_case：`gt` / `ge` / `lt` / `le` / `eq` / `ne`）。
///
/// 数值字段（[`NUMERIC_FIELDS`]）支持全部；字符串字段（[`STRING_FIELDS`]）仅 `eq` / `ne`。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CmpOp {
    /// `>` 大于。
    Gt,
    /// `>=` 大于等于。
    Ge,
    /// `<` 小于。
    Lt,
    /// `<=` 小于等于。
    Le,
    /// `=` 等于。
    Eq,
    /// `<>` 不等于。
    Ne,
}

/// 比较值：`untagged`，JSON number → [`Self::Num`]，JSON string → [`Self::Str`]。
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(untagged)]
pub enum ConditionValue {
    /// 数值比较值（JSON number）。
    Num(f64),
    /// 字符串比较值（JSON string）。
    Str(String),
}

/// WHERE 条件树。
///
/// JSON 形式（与 EMQX Rule SQL 的 WHERE 语义对齐）：
/// ```json
/// {"kind": "cmp", "field": "value", "op": "gt", "value": 30}
/// {"kind": "and", "conditions": [ ... ]}
/// {"kind": "or",  "conditions": [ ... ]}
/// {"kind": "not", "condition":  { ... }}
/// ```
///
/// `Deserialize` 为手写实现（见 [`Self::from_value`]）：`tag = "kind"` 的内部标签表示
/// 无法同时表达「结构体变体（cmp）」与「元组变体（and/or/not）」的自然 JSON 形态，
/// 手写实现可保证四种 kind 的 JSON 书写风格一致，并给出带上下文的错误信息。
#[derive(Debug, Clone, PartialEq)]
pub enum Condition {
    /// 单字段比较：`field` 为 JSONPath（扁平 payload 下即字段名）。
    Cmp {
        /// 字段路径（如 `"value"` / `"$.value"`）。
        field: String,
        /// 比较运算符。
        op: CmpOp,
        /// 比较值。
        value: ConditionValue,
    },
    /// 逻辑与（空数组 = 恒真）。
    And(Vec<Condition>),
    /// 逻辑或（空数组 = 恒假）。
    Or(Vec<Condition>),
    /// 逻辑非。
    Not(Box<Condition>),
}

impl Condition {
    /// 从已解析的 JSON 值构造（严格模式：未知 `kind` / 未知字段一律报错）。
    fn from_value(value: &Value) -> Result<Self, String> {
        let object = value
            .as_object()
            .ok_or_else(|| format!("condition must be a JSON object, got {}", kind_of(value)))?;
        let kind = object
            .get("kind")
            .and_then(Value::as_str)
            .ok_or_else(|| "condition: missing string field `kind`".to_string())?;
        match kind {
            "cmp" => {
                reject_unknown(object, "cmp condition", &["kind", "field", "op", "value"])?;
                let field = require_string(object, "field", "cmp condition")?;
                let op = require_enum::<CmpOp>(object, "op", "cmp condition")?;
                let value = require_enum::<ConditionValue>(object, "value", "cmp condition")?;
                Ok(Condition::Cmp { field, op, value })
            }
            "and" | "or" => {
                let list_key = "conditions";
                reject_unknown(object, &format!("{kind} condition"), &["kind", list_key])?;
                let items = require_array(object, list_key, &format!("{kind} condition"))?;
                let parsed = items
                    .iter()
                    .map(Condition::from_value)
                    .collect::<Result<Vec<Condition>, String>>()?;
                if kind == "and" {
                    Ok(Condition::And(parsed))
                } else {
                    Ok(Condition::Or(parsed))
                }
            }
            "not" => {
                reject_unknown(object, "not condition", &["kind", "condition"])?;
                let inner = object
                    .get("condition")
                    .ok_or_else(|| "not condition: missing field `condition`".to_string())?;
                Ok(Condition::Not(Box::new(Condition::from_value(inner)?)))
            }
            other => Err(format!(
                "condition: unknown kind {other:?} (expected `cmp` | `and` | `or` | `not`)"
            )),
        }
    }
}

impl<'de> Deserialize<'de> for Condition {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = Value::deserialize(deserializer)?;
        Condition::from_value(&value).map_err(D::Error::custom)
    }
}

/// DO 动作（P0 子集：路由 + 字段重映射）。
///
/// JSON 形式：
/// ```json
/// {"kind": "publish", "topic": "alarms"}
/// {"kind": "remap", "fields": {"t": "$.value", "dev": "$.device_id"}}
/// ```
#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    /// 向指定 topic 发布**当前** payload（一条规则可含多个 `Publish` → 产出多条消息）。
    Publish {
        /// 目标 topic（非空）。
        topic: String,
    },
    /// 字段重映射：**产出全新 payload**（仅含 `fields` 列出的 key，值为源 JSONPath 解析结果）。
    Remap {
        /// 输出 key → 源 JSONPath（`BTreeMap` 保证输出字段顺序稳定）。
        fields: BTreeMap<String, String>,
    },
}

impl Action {
    /// 从已解析的 JSON 值构造（严格模式：未知 `kind` / 未知字段一律报错）。
    fn from_value(value: &Value) -> Result<Self, String> {
        let object = value
            .as_object()
            .ok_or_else(|| format!("action must be a JSON object, got {}", kind_of(value)))?;
        let kind = object
            .get("kind")
            .and_then(Value::as_str)
            .ok_or_else(|| "action: missing string field `kind`".to_string())?;
        match kind {
            "publish" => {
                reject_unknown(object, "publish action", &["kind", "topic"])?;
                let topic = require_string(object, "topic", "publish action")?;
                Ok(Action::Publish { topic })
            }
            "remap" => {
                reject_unknown(object, "remap action", &["kind", "fields"])?;
                let fields = object
                    .get("fields")
                    .ok_or_else(|| "remap action: missing field `fields`".to_string())?;
                let map: BTreeMap<String, String> = serde_json::from_value(fields.clone())
                    .map_err(|err| {
                        format!("remap action: `fields` must be an object of string → path: {err}")
                    })?;
                Ok(Action::Remap { fields: map })
            }
            other => Err(format!(
                "action: unknown kind {other:?} (expected `publish` | `remap`)"
            )),
        }
    }
}

impl<'de> Deserialize<'de> for Action {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = Value::deserialize(deserializer)?;
        Action::from_value(&value).map_err(D::Error::custom)
    }
}

/// 一条转发规则（对应 EMQX 的 `SELECT ... WHERE ... DO ...`）。
///
/// **未知字段一律拒绝**（`deny_unknown_fields`）：顶层键拼错（如把 `transform` 写成
/// `tranform`）绝不允许静默降级为通配规则 —— 那会导致对所有样本全量转发。
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Rule {
    /// 规则唯一标识（全局唯一，重复 → `ConfigError`）。
    pub id: String,
    /// SELECT：JSONPath 白名单；**空 = 输出全部字段**。
    #[serde(default)]
    pub select: Vec<String>,
    /// WHERE：条件树；`None` = 恒真。
    pub when: Option<Condition>,
    /// DO：动作列表（按数组顺序执行）。
    #[serde(default)]
    pub actions: Vec<Action>,
    /// 数值变换规格（委托 task 15）；`None` = 本规则为**通配规则**，匹配所有样本。
    pub transform: Option<Transform>,
}

/// 路由结果：一条待发布的消息（topic + payload 均已定型，可直接交北向 MQTT）。
#[derive(Debug, Clone, PartialEq)]
pub struct RoutedMessage {
    /// 命中的规则 id（便于北向诊断与回溯）。
    pub rule_id: String,
    /// 目标 topic。
    pub topic: String,
    /// 消息体（SELECT 投影 + Remap 之后的 JSON 对象）。
    pub payload: Value,
}

// ---- JSONPath 最小子集 ----

/// JSONPath 片段。
#[derive(Debug, Clone, PartialEq, Eq)]
enum Segment {
    /// 对象字段名。
    Field(String),
    /// 数组下标。
    Index(usize),
}

/// 解析 JSONPath：`$` 根 + `.` 分隔字段 + `[n]` 数组下标。
///
/// 非法路径（空字符串 / 空段 / 未闭合 `[` / 非数字下标 / 负下标）返回错误描述，
/// 由调用方包装成 [`DaemonError::ConfigError`]。
fn parse_path(raw: &str) -> Result<Vec<Segment>, String> {
    if raw.is_empty() {
        return Err("path must not be empty".to_string());
    }
    let body = match raw.strip_prefix('$') {
        Some(rest) => {
            if rest.is_empty() {
                return Ok(Vec::new()); // `$` → 根
            }
            rest.strip_prefix('.')
                .ok_or_else(|| format!("path {raw:?}: expected `.` or end after `$`"))?
        }
        None => raw,
    };
    if body.is_empty() {
        return Ok(Vec::new());
    }

    let mut segments: Vec<Segment> = Vec::new();
    for part in body.split('.') {
        let (name, indexes) = match part.find('[') {
            Some(position) => (&part[..position], &part[position..]),
            None => (part, ""),
        };
        if !name.is_empty() {
            segments.push(Segment::Field(name.to_string()));
        } else if indexes.is_empty() {
            return Err(format!("path {raw:?}: empty field name"));
        }
        let mut tail = indexes;
        while !tail.is_empty() {
            if !tail.starts_with('[') {
                return Err(format!("path {raw:?}: unexpected {tail:?} after `]`"));
            }
            let end = tail
                .find(']')
                .ok_or_else(|| format!("path {raw:?}: unterminated `[`"))?;
            let literal = &tail[1..end];
            if literal.is_empty() {
                return Err(format!("path {raw:?}: empty array index"));
            }
            let index: usize = literal.parse().map_err(|_| {
                format!("path {raw:?}: array index {literal:?} is not a non-negative integer")
            })?;
            segments.push(Segment::Index(index));
            tail = &tail[end + 1..];
        }
    }
    Ok(segments)
}

/// 取路径的根字段名（payload 扁平，根段决定字段语义与类型）。
fn path_root_field(segments: &[Segment]) -> Option<&str> {
    match segments.first() {
        Some(Segment::Field(name)) => Some(name.as_str()),
        _ => None,
    }
}

/// SELECT 输出 key：取路径最后一段（`Field` → 名字，`Index` → 下标字符串，`$` → `"$"`）。
fn select_key(segments: &[Segment]) -> String {
    match segments.last() {
        Some(Segment::Field(name)) => name.clone(),
        Some(Segment::Index(index)) => index.to_string(),
        None => "$".to_string(),
    }
}

/// 在 JSON 值上解析路径；路径不通（字段缺失 / 类型不匹配 / 越界）返回 `None`。
fn resolve<'a>(root: &'a Value, segments: &[Segment]) -> Option<&'a Value> {
    let mut cursor = root;
    for segment in segments {
        cursor = match segment {
            Segment::Field(name) => cursor.get(name.as_str())?,
            Segment::Index(index) => cursor.get(*index)?,
        };
    }
    Some(cursor)
}

// ---- 构造期校验辅助 ----

/// JSON 值的类型名（错误信息用，避免直接打印整棵 value 导致日志膨胀）。
fn kind_of(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "bool",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

/// 严格模式：拒绝 `allowed` 之外的字段（对齐 `deny_unknown_fields` 语义）。
fn reject_unknown(object: &Map<String, Value>, what: &str, allowed: &[&str]) -> Result<(), String> {
    for key in object.keys() {
        if !allowed.contains(&key.as_str()) {
            return Err(format!(
                "{what}: unknown field {key:?} (allowed: {})",
                allowed.join(", ")
            ));
        }
    }
    Ok(())
}

/// 取必填字符串字段。
fn require_string(object: &Map<String, Value>, key: &str, what: &str) -> Result<String, String> {
    match object.get(key) {
        Some(Value::String(text)) => Ok(text.clone()),
        Some(other) => Err(format!(
            "{what}: field `{key}` must be a string, got {}",
            kind_of(other)
        )),
        None => Err(format!("{what}: missing field `{key}`")),
    }
}

/// 取必填数组字段。
fn require_array<'a>(
    object: &'a Map<String, Value>,
    key: &str,
    what: &str,
) -> Result<&'a Vec<Value>, String> {
    match object.get(key) {
        Some(Value::Array(items)) => Ok(items),
        Some(other) => Err(format!(
            "{what}: field `{key}` must be an array, got {}",
            kind_of(other)
        )),
        None => Err(format!("{what}: missing field `{key}`")),
    }
}

/// 借 `serde` derive 解析子结构（`CmpOp` / `ConditionValue`），保证 JSON 名与枚举同步。
fn require_enum<T>(object: &Map<String, Value>, key: &str, what: &str) -> Result<T, String>
where
    T: for<'de> Deserialize<'de>,
{
    let value = object
        .get(key)
        .ok_or_else(|| format!("{what}: missing field `{key}`"))?;
    serde_json::from_value::<T>(value.clone())
        .map_err(|err| format!("{what}: invalid field `{key}`: {err}"))
}

/// 把路径解析结果登记进构造期缓存（热路径不再重复解析）。
fn register_path(raw: &str, cache: &mut HashMap<String, Vec<Segment>>) -> Result<(), String> {
    let segments = parse_path(raw)?;
    cache.insert(raw.to_string(), segments);
    Ok(())
}

// ---- 引擎 ----

/// 声明式规则引擎（有状态：持有 task 15 的 [`DataProcessor`] 死区基准）。
///
/// 构造即完成全部语义校验，运行期只做匹配与投影，不返回配置类错误。
#[derive(Debug)]
pub struct RuleEngine {
    /// 规则列表（求值顺序 = 数组顺序）。
    rules: Vec<Rule>,
    /// 数值变换 / 死区：由所有规则的 `transform` 汇总而成。
    processor: DataProcessor,
    /// 直通处理器缓存（key = `source_id`）：供**未被任何 `transform` 声明**的样本走
    /// task 15 的 `passthrough` 配置（deadband = 0，等价于无状态）。
    passthrough: HashMap<String, DataProcessor>,
    /// 已声明 `transform` 的 `source_id` 集合。
    sources: HashSet<String>,
    /// 构造期解析好的 JSONPath 缓存（避免热路径重复解析）。
    paths: HashMap<String, Vec<Segment>>,
    /// 是否存在通配规则（无 `transform` 的规则）。
    has_wildcard: bool,
}

impl RuleEngine {
    /// 解析规则 JSON 并构造引擎；任何语法 / 语义错误都返回
    /// [`DaemonError::ConfigError`]（错误码 2000），**绝不 panic**。
    pub fn from_json(json: &str) -> DaemonResult<Self> {
        let root: Value = serde_json::from_str(json)
            .map_err(|err| config_error(format!("rule json parse: {err}")))?;
        let rules_value: Value = match &root {
            Value::Array(_) => root.clone(),
            Value::Object(map) => map.get("rules").cloned().ok_or_else(|| {
                config_error("rule json: object form must contain a `rules` array")
            })?,
            other => {
                return Err(config_error(format!(
                    "rule json: expected an array or an object with `rules`, got {}",
                    kind_of(other)
                )))
            }
        };
        let rules: Vec<Rule> = serde_json::from_value(rules_value)
            .map_err(|err| config_error(format!("rule schema: {err}")))?;
        Self::from_rules(rules)
    }

    /// 按规则列表构造引擎（重复 `source_id` 等配置错误原样透传 [`DataProcessor::new`]）。
    pub fn from_rules(rules: Vec<Rule>) -> DaemonResult<Self> {
        let mut seen_ids: HashSet<&str> = HashSet::new();
        let mut paths: HashMap<String, Vec<Segment>> = HashMap::new();
        let mut configs: Vec<PointConfig> = Vec::new();
        let mut sources: HashSet<String> = HashSet::new();
        let mut has_wildcard = false;

        for rule in &rules {
            if rule.id.is_empty() {
                return Err(config_error("rule: `id` must not be empty"));
            }
            if !seen_ids.insert(rule.id.as_str()) {
                return Err(config_error(format!("duplicate rule id {:?}", rule.id)));
            }

            // 1) Remap 产物先收集：它们也是 WHERE / SELECT / Remap 路径的合法根字段。
            let mut produced: HashSet<String> = HashSet::new();
            for action in &rule.actions {
                if let Action::Remap { fields } = action {
                    for key in fields.keys() {
                        if key.is_empty() {
                            return Err(config_error(format!(
                                "rule {:?}: remap output key must not be empty",
                                rule.id
                            )));
                        }
                        produced.insert(key.clone());
                    }
                }
            }

            // 2) SELECT 路径。
            for raw in &rule.select {
                register_path(raw, &mut paths)
                    .map_err(|err| config_error(format!("rule {:?}: select: {err}", rule.id)))?;
                validate_root(raw, &paths, &produced)
                    .map_err(|err| config_error(format!("rule {:?}: select: {err}", rule.id)))?;
            }

            // 3) DO 动作：topic 非空、remap 路径合法。
            for action in &rule.actions {
                match action {
                    Action::Publish { topic } => {
                        if topic.is_empty() {
                            return Err(config_error(format!(
                                "rule {:?}: publish `topic` must not be empty",
                                rule.id
                            )));
                        }
                    }
                    Action::Remap { fields } => {
                        for path in fields.values() {
                            register_path(path, &mut paths).map_err(|err| {
                                config_error(format!("rule {:?}: remap: {err}", rule.id))
                            })?;
                            validate_root(path, &paths, &produced).map_err(|err| {
                                config_error(format!("rule {:?}: remap: {err}", rule.id))
                            })?;
                        }
                    }
                }
            }

            // 4) WHERE：字段白名单 + 类型 / 比较符匹配。
            if let Some(condition) = &rule.when {
                validate_condition(condition, &produced, &mut paths)
                    .map_err(|err| config_error(format!("rule {:?}: where: {err}", rule.id)))?;
            }

            // 5) Transform → task 15 的 PointConfig（换算 / 死区 / 映射全部委托）。
            match &rule.transform {
                Some(transform) => {
                    if transform.source_id.is_empty() {
                        return Err(config_error(format!(
                            "rule {:?}: transform.source_id must not be empty",
                            rule.id
                        )));
                    }
                    if transform.target_point.is_empty() {
                        return Err(config_error(format!(
                            "rule {:?}: transform.target_point must not be empty",
                            rule.id
                        )));
                    }
                    if transform.device_id.is_empty() {
                        return Err(config_error(format!(
                            "rule {:?}: transform.device_id must not be empty",
                            rule.id
                        )));
                    }
                    sources.insert(transform.source_id.clone());
                    configs.push(transform.to_point_config());
                }
                None => has_wildcard = true,
            }
        }

        let processor = DataProcessor::new(configs)?;
        Ok(Self {
            rules,
            processor,
            passthrough: HashMap::new(),
            sources,
            paths,
            has_wildcard,
        })
    }

    /// 完整链路：先过 task 15 `DataProcessor`（映射 / 换算 / 死区 / 时间戳统一），
    /// 再求值 WHERE + DO。
    ///
    /// - 样本 `source_id` 被某条规则的 `transform` 声明 → 走该 `transform` 配置；
    /// - 否则走 `passthrough` 配置（仅当存在通配规则；没有任何规则关心时直接返回空）；
    /// - 被死区吞掉 / 未命中任何规则 → 返回空 `Vec`（不是错误）。
    pub fn evaluate(&mut self, sample: RawSample) -> DaemonResult<Vec<RoutedMessage>> {
        let source = sample.source_id.clone();
        if !self.sources.contains(source.as_str()) && !self.has_wildcard {
            return Ok(Vec::new());
        }
        let processed = if self.sources.contains(source.as_str()) {
            match self.processor.process(sample)? {
                Some(processed) => processed,
                None => return Ok(Vec::new()), // 死区吞掉
            }
        } else {
            match self.passthrough_process(sample)? {
                Some(processed) => processed,
                None => return Ok(Vec::new()),
            }
        };
        Ok(self.route(&processed))
    }

    /// 纯路由（无状态旁路）：已处理样本直接走 WHERE + SELECT + DO。
    ///
    /// 规则匹配：`transform` 为 `None` 的通配规则匹配所有样本；声明了 `transform` 的规则
    /// 按 `transform.target_point == sample.point_id` 匹配。
    pub fn route(&self, sample: &ProcessedSample) -> Vec<RoutedMessage> {
        let payload = sample_payload(sample);
        let mut messages = Vec::new();
        for rule in &self.rules {
            if !self.matches(rule, sample) {
                continue;
            }
            if let Some(condition) = &rule.when {
                if !self.eval_condition(condition, &payload) {
                    continue;
                }
            }
            let projected = self.project(rule, &payload);
            self.run_actions(rule, projected, &mut messages);
        }
        messages
    }

    /// 规则条数。
    pub fn rule_count(&self) -> usize {
        self.rules.len()
    }

    /// 规则是否匹配该样本（见 [`Self::route`]）。
    fn matches(&self, rule: &Rule, sample: &ProcessedSample) -> bool {
        match &rule.transform {
            Some(transform) => transform.target_point == sample.point_id,
            None => true,
        }
    }

    /// 未被任何 `transform` 声明的样本：仍走 task 15（直通配置，`deadband = 0`）。
    fn passthrough_process(&mut self, sample: RawSample) -> DaemonResult<Option<ProcessedSample>> {
        let source = sample.source_id.clone();
        let processor = match self.passthrough.entry(source.clone()) {
            Entry::Occupied(entry) => entry.into_mut(),
            Entry::Vacant(entry) => {
                let config = PointConfig::passthrough(&source, &source, &source, DEFAULT_UNIT);
                entry.insert(DataProcessor::new(vec![config])?)
            }
        };
        processor.process(sample)
    }

    /// 求值条件树；路径解析不到值一律视为「不匹配」（false）。
    fn eval_condition(&self, condition: &Condition, payload: &Value) -> bool {
        match condition {
            Condition::Cmp { field, op, value } => match self.paths.get(field.as_str()) {
                Some(segments) => match resolve(payload, segments) {
                    Some(actual) => compare(actual, *op, value),
                    None => false,
                },
                None => false,
            },
            Condition::And(list) => list.iter().all(|item| self.eval_condition(item, payload)),
            Condition::Or(list) => list.iter().any(|item| self.eval_condition(item, payload)),
            Condition::Not(inner) => !self.eval_condition(inner, payload),
        }
    }

    /// SELECT 投影：`select` 为空 → 全部字段；否则只保留白名单（缺失字段静默跳过）。
    fn project(&self, rule: &Rule, payload: &Value) -> Value {
        if rule.select.is_empty() {
            return payload.clone();
        }
        let mut projected = Map::new();
        for raw in &rule.select {
            let Some(segments) = self.paths.get(raw.as_str()) else {
                continue; // 构造期已校验，理论不可达
            };
            if let Some(value) = resolve(payload, segments) {
                projected.insert(select_key(segments), value.clone());
            }
        }
        Value::Object(projected)
    }

    /// DO：动作按数组顺序执行（`Remap` 影响其后的 `Publish`）。
    fn run_actions(&self, rule: &Rule, mut payload: Value, out: &mut Vec<RoutedMessage>) {
        for action in &rule.actions {
            match action {
                Action::Remap { fields } => {
                    let mut remapped = Map::new();
                    for (key, path) in fields {
                        if let Some(segments) = self.paths.get(path.as_str()) {
                            if let Some(value) = resolve(&payload, segments) {
                                remapped.insert(key.clone(), value.clone());
                            }
                        }
                    }
                    payload = Value::Object(remapped);
                }
                Action::Publish { topic } => out.push(RoutedMessage {
                    rule_id: rule.id.clone(),
                    topic: topic.clone(),
                    payload: payload.clone(),
                }),
            }
        }
    }
}

/// 校验路径根字段 ∈ 已知字段 ∪ 本规则的 Remap 产物。
fn validate_root(
    raw: &str,
    paths: &HashMap<String, Vec<Segment>>,
    produced: &HashSet<String>,
) -> Result<(), String> {
    let Some(segments) = paths.get(raw) else {
        return Err(format!("path {raw:?} is not registered"));
    };
    if segments.is_empty() {
        return Err(format!("path {raw:?}: `$` (whole payload) is not a field"));
    }
    match path_root_field(segments) {
        Some(name) if KNOWN_FIELDS.contains(&name) => Ok(()),
        Some(name) if produced.contains(name) => Ok(()),
        Some(name) => Err(format!(
            "unknown field {name:?} (known: {}, or a remap output key of this rule)",
            KNOWN_FIELDS.join(", ")
        )),
        None => Err(format!(
            "path {raw:?}: must start with a named field, not an array index"
        )),
    }
}

/// 递归校验 WHERE 条件树（字段白名单 + 类型 / 比较符匹配 + 路径合法）。
fn validate_condition(
    condition: &Condition,
    produced: &HashSet<String>,
    paths: &mut HashMap<String, Vec<Segment>>,
) -> Result<(), String> {
    match condition {
        Condition::Cmp { field, op, value } => {
            register_path(field, paths)?;
            validate_root(field, paths, produced)?;
            let Some(segments) = paths.get(field.as_str()) else {
                return Err(format!("path {field:?} is not registered"));
            };
            if segments.len() != 1 {
                return Err(format!(
                    "field {field:?}: payload is flat, nested path is not supported"
                ));
            }
            let Some(name) = path_root_field(segments) else {
                return Err(format!("field {field:?}: must reference a named field"));
            };
            if NUMERIC_FIELDS.contains(&name) {
                if matches!(value, ConditionValue::Str(_)) {
                    return Err(format!(
                        "field {name:?} is numeric: comparison value must be a number"
                    ));
                }
            } else if STRING_FIELDS.contains(&name) {
                if matches!(value, ConditionValue::Num(_)) {
                    return Err(format!(
                        "field {name:?} is a string field: comparison value must be a string"
                    ));
                }
                if !matches!(op, CmpOp::Eq | CmpOp::Ne) {
                    return Err(format!(
                        "field {name:?} is a string field: only `eq` / `ne` are supported, got `{op:?}`"
                    ));
                }
            }
            Ok(())
        }
        Condition::And(list) | Condition::Or(list) => {
            for item in list {
                validate_condition(item, produced, paths)?;
            }
            Ok(())
        }
        Condition::Not(inner) => validate_condition(inner, produced, paths),
    }
}

/// 把 [`ProcessedSample`] 转成规则求值用的 JSON payload。
///
/// **红线**：纳秒时间戳一律编码为 **JSON 字符串**（JSON number 是 double，> 2^53-1 丢精度）。
fn sample_payload(sample: &ProcessedSample) -> Value {
    let mut map = Map::new();
    map.insert(
        "device_id".to_string(),
        Value::String(sample.device_id.clone()),
    );
    map.insert(
        "point_id".to_string(),
        Value::String(sample.point_id.clone()),
    );
    map.insert("value".to_string(), Value::from(sample.value)); // NaN / ±Inf → null
    map.insert("unit".to_string(), Value::String(sample.unit.clone()));
    map.insert(
        "quality".to_string(),
        Value::String(sample.quality.as_str_name().to_string()),
    );
    map.insert(
        "device_ts_ns".to_string(),
        match sample.device_ts_ns {
            Some(nanos) => Value::String(nanos.to_string()),
            None => Value::Null,
        },
    );
    map.insert(
        "collected_ts_ns".to_string(),
        Value::String(sample.collected_ts_ns.to_string()),
    );
    Value::Object(map)
}

/// 取数值：JSON number 直接取；字符串按 `f64` 解析（纳秒时间戳是字符串编码）。
fn as_number(value: &Value) -> Option<f64> {
    match value {
        Value::Number(number) => number.as_f64(),
        Value::String(text) => text.parse::<f64>().ok(),
        _ => None,
    }
}

/// 单值比较；类型不匹配（含字段缺失）一律返回 `false`（不匹配，而非错误）。
fn compare(actual: &Value, op: CmpOp, expected: &ConditionValue) -> bool {
    match expected {
        ConditionValue::Num(target) => match as_number(actual) {
            Some(value) => match op {
                CmpOp::Gt => value > *target,
                CmpOp::Ge => value >= *target,
                CmpOp::Lt => value < *target,
                CmpOp::Le => value <= *target,
                CmpOp::Eq => value == *target,
                CmpOp::Ne => value != *target,
            },
            None => false,
        },
        ConditionValue::Str(target) => match actual.as_str() {
            Some(text) => match op {
                CmpOp::Eq => text == target.as_str(),
                CmpOp::Ne => text != target.as_str(),
                CmpOp::Gt | CmpOp::Ge | CmpOp::Lt | CmpOp::Le => false,
            },
            None => false,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use protocol_proto::Quality;

    const T0: i64 = 1_700_000_000_123_456_789;

    fn raw(source_id: &str, value: f64) -> RawSample {
        RawSample {
            source_id: source_id.to_string(),
            value,
            quality: Quality::Good,
            device_ts_ns: Some(T0),
        }
    }

    fn raw_with_quality(source_id: &str, value: f64, quality: Quality) -> RawSample {
        RawSample {
            source_id: source_id.to_string(),
            value,
            quality,
            device_ts_ns: Some(T0),
        }
    }

    fn processed(point_id: &str, value: f64, unit: &str) -> ProcessedSample {
        ProcessedSample {
            device_id: "line1_m1".to_string(),
            point_id: point_id.to_string(),
            value,
            unit: unit.to_string(),
            device_ts_ns: Some(T0),
            collected_ts_ns: T0,
            quality: Quality::Good,
        }
    }

    /// QA Happy：`WHERE value > 30 → publish "alarms"`，输入 36.5 → 1 条到 alarms；25.0 → 0 条。
    #[test]
    fn qa_happy_temp_over_30_routes_to_alarms() {
        let json = r#"[
            {
                "id": "temp_alarm",
                "when": {"kind": "cmp", "field": "value", "op": "gt", "value": 30},
                "actions": [{"kind": "publish", "topic": "alarms"}],
                "transform": {
                    "source_id": "40001",
                    "target_point": "line1_m1_temp",
                    "device_id": "line1_m1",
                    "unit": "degC"
                }
            }
        ]"#;
        let mut engine = RuleEngine::from_json(json).expect("rules must parse");

        let hit = engine.evaluate(raw("40001", 36.5)).expect("ok");
        assert_eq!(hit.len(), 1, "36.5 > 30 → 1 message");
        assert_eq!(hit[0].rule_id, "temp_alarm");
        assert_eq!(hit[0].topic, "alarms");
        assert_eq!(hit[0].payload["value"].as_f64(), Some(36.5));

        let miss = engine.evaluate(raw("40001", 25.0)).expect("ok");
        assert_eq!(miss.len(), 0, "25.0 <= 30 → no message");
    }

    /// SELECT 白名单：payload 只含所选 key。
    #[test]
    fn select_whitelist_limits_payload_keys() {
        let json = r#"[
            {
                "id": "telemetry",
                "select": ["$.value", "$.unit"],
                "actions": [{"kind": "publish", "topic": "telemetry"}],
                "transform": {"source_id": "t", "target_point": "temp", "device_id": "m1", "unit": "degC"}
            }
        ]"#;
        let mut engine = RuleEngine::from_json(json).expect("rules");
        let out = engine.evaluate(raw("t", 21.5)).expect("ok");
        assert_eq!(out.len(), 1);
        let payload = out[0].payload.as_object().expect("object payload");
        assert_eq!(payload.len(), 2, "only value + unit: {payload:?}");
        assert!(payload.contains_key("value"));
        assert!(payload.contains_key("unit"));
        assert!(!payload.contains_key("point_id"));
        assert!(!payload.contains_key("quality"));
        assert_eq!(payload["unit"].as_str(), Some("degC"));
    }

    /// Remap：输出 key 被重命名，值来自源路径；Remap 先于 Publish 生效。
    #[test]
    fn remap_renames_fields_before_publish() {
        let json = r#"[
            {
                "id": "remapped",
                "actions": [
                    {"kind": "remap", "fields": {"t": "$.value", "dev": "$.device_id"}},
                    {"kind": "publish", "topic": "north/remap"}
                ],
                "transform": {"source_id": "t", "target_point": "temp", "device_id": "m1", "unit": "degC"}
            }
        ]"#;
        let mut engine = RuleEngine::from_json(json).expect("rules");
        let out = engine.evaluate(raw("t", 42.0)).expect("ok");
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].topic, "north/remap");
        let payload = out[0].payload.as_object().expect("object payload");
        assert_eq!(
            payload.len(),
            2,
            "remap produces a fresh payload: {payload:?}"
        );
        assert_eq!(payload["t"].as_f64(), Some(42.0));
        assert_eq!(payload["dev"].as_str(), Some("m1"));
        assert!(!payload.contains_key("value"), "source key renamed away");
    }

    /// **委托验证**：`scale 0.1 + offset 2.0 + unit degC`，原始 500 → 52.0 / degC
    /// （证明换算走 task 15 的 DataProcessor，而非本模块自算）。
    #[test]
    fn transform_is_delegated_to_data_processor() {
        let json = r#"[
            {
                "id": "scaled",
                "actions": [{"kind": "publish", "topic": "scaled"}],
                "transform": {
                    "source_id": "raw_temp",
                    "target_point": "temp",
                    "device_id": "m1",
                    "unit": "degC",
                    "scale": 0.1,
                    "offset": 2.0
                }
            }
        ]"#;
        let mut engine = RuleEngine::from_json(json).expect("rules");
        let out = engine.evaluate(raw("raw_temp", 500.0)).expect("ok");
        assert_eq!(out.len(), 1);
        let payload = &out[0].payload;
        assert!(
            (payload["value"].as_f64().expect("number") - 52.0).abs() < 1e-9,
            "500 * 0.1 + 2.0 = 52.0, got {}",
            payload["value"]
        );
        assert_eq!(payload["unit"].as_str(), Some("degC"));
        assert_eq!(payload["point_id"].as_str(), Some("temp"));
        assert_eq!(payload["device_id"].as_str(), Some("m1"));
    }

    /// 死区委托：deadband 1.0，第二次变化 0.5 → 0 条；变化 2.0 → 1 条。
    #[test]
    fn deadband_is_delegated_to_data_processor() {
        let json = r#"[
            {
                "id": "debounced",
                "actions": [{"kind": "publish", "topic": "debounced"}],
                "transform": {
                    "source_id": "t",
                    "target_point": "temp",
                    "device_id": "m1",
                    "unit": "degC",
                    "deadband": 1.0
                }
            }
        ]"#;
        let mut engine = RuleEngine::from_json(json).expect("rules");

        let first = engine.evaluate(raw("t", 36.5)).expect("ok");
        assert_eq!(first.len(), 1, "first sample establishes the baseline");

        let small = engine.evaluate(raw("t", 37.0)).expect("ok");
        assert_eq!(small.len(), 0, "0.5 < 1.0 → swallowed by the deadband");

        let big = engine.evaluate(raw("t", 38.5)).expect("ok");
        assert_eq!(big.len(), 1, "2.0 >= 1.0 → emitted");
        assert_eq!(big[0].payload["value"].as_f64(), Some(38.5));
    }

    /// And / Or / Not 组合求值。
    #[test]
    fn and_or_not_combinations() {
        let json = r#"[
            {
                "id": "and_rule",
                "when": {"kind": "and", "conditions": [
                    {"kind": "cmp", "field": "value", "op": "gt", "value": 30},
                    {"kind": "cmp", "field": "quality", "op": "eq", "value": "GOOD"}
                ]},
                "actions": [{"kind": "publish", "topic": "and_topic"}],
                "transform": {"source_id": "t", "target_point": "temp", "device_id": "m1"}
            },
            {
                "id": "or_rule",
                "when": {"kind": "or", "conditions": [
                    {"kind": "cmp", "field": "value", "op": "lt", "value": 10},
                    {"kind": "cmp", "field": "value", "op": "ge", "value": 100}
                ]},
                "actions": [{"kind": "publish", "topic": "or_topic"}]
            },
            {
                "id": "not_rule",
                "when": {"kind": "not", "condition": {"kind": "cmp", "field": "value", "op": "gt", "value": 30}},
                "actions": [{"kind": "publish", "topic": "not_topic"}]
            }
        ]"#;
        let mut engine = RuleEngine::from_json(json).expect("rules");

        // AND：value 36.5 且 GOOD → 命中；value 36.5 但 BAD → 不命中。
        let good = engine
            .evaluate(raw_with_quality("t", 36.5, Quality::Good))
            .expect("ok");
        let topics: Vec<&str> = good.iter().map(|m| m.topic.as_str()).collect();
        assert!(topics.contains(&"and_topic"), "topics: {topics:?}");
        assert!(!topics.contains(&"not_topic"), "36.5 > 30 → Not 应排除");

        let bad = engine
            .evaluate(raw_with_quality("t", 36.5, Quality::Bad))
            .expect("ok");
        let topics: Vec<&str> = bad.iter().map(|m| m.topic.as_str()).collect();
        assert!(!topics.contains(&"and_topic"), "quality BAD → And 失败");

        // OR：5 < 10 命中；50 两边都不满足。
        let low = engine.evaluate(raw("t", 5.0)).expect("ok");
        assert!(low.iter().any(|m| m.topic == "or_topic"));
        let mid = engine.evaluate(raw("t", 50.0)).expect("ok");
        assert!(!mid.iter().any(|m| m.topic == "or_topic"));

        // NOT：25.0 不 > 30 → 命中 not_topic。
        let not_hit = engine.evaluate(raw("t", 25.0)).expect("ok");
        assert!(not_hit.iter().any(|m| m.topic == "not_topic"));
    }

    /// 多条规则同时命中 → 2 条消息且 rule_id 齐全（第二条为无 transform 的通配规则）。
    #[test]
    fn multiple_rules_hit_produce_multiple_messages() {
        let json = r#"[
            {
                "id": "alarm",
                "when": {"kind": "cmp", "field": "value", "op": "gt", "value": 30},
                "actions": [{"kind": "publish", "topic": "alarms"}],
                "transform": {"source_id": "t", "target_point": "temp", "device_id": "m1", "unit": "degC"}
            },
            {
                "id": "archive",
                "actions": [{"kind": "publish", "topic": "archive"}]
            }
        ]"#;
        let mut engine = RuleEngine::from_json(json).expect("rules");
        assert_eq!(engine.rule_count(), 2);

        let out = engine.evaluate(raw("t", 36.5)).expect("ok");
        assert_eq!(out.len(), 2, "both rules hit");
        let ids: Vec<&str> = out.iter().map(|m| m.rule_id.as_str()).collect();
        assert_eq!(ids, vec!["alarm", "archive"], "rule ids in rule order");
        let topics: Vec<&str> = out.iter().map(|m| m.topic.as_str()).collect();
        assert_eq!(topics, vec!["alarms", "archive"]);
    }

    /// QA Error：非法 JSON → ConfigError（错误码 2000），不 panic。
    #[test]
    fn malformed_json_is_config_error_2000() {
        for bad in ["{", "{\"id\":}", "not json at all", "[{\"id\":1}]"] {
            let err = RuleEngine::from_json(bad)
                .expect_err("malformed json must not be silently accepted");
            assert!(
                matches!(err, DaemonError::ConfigError(_)),
                "expected ConfigError for {bad:?}, got {err:?}"
            );
            assert_eq!(err.error_code(), 2000, "config domain error code");
        }
    }

    /// QA Error：WHERE 字段写错（`temp`）/ 字符串字段用 `gt` → 构造期 ConfigError。
    #[test]
    fn invalid_where_field_or_op_is_config_error() {
        let unknown_field = r#"[
            {"id": "r", "when": {"kind": "cmp", "field": "temp", "op": "gt", "value": 30},
             "actions": [{"kind": "publish", "topic": "x"}]}
        ]"#;
        let err = RuleEngine::from_json(unknown_field).expect_err("unknown field");
        assert!(matches!(err, DaemonError::ConfigError(_)), "got {err:?}");
        assert!(
            err.to_string().contains("temp"),
            "message names the field: {err}"
        );

        let string_field_gt = r#"[
            {"id": "r", "when": {"kind": "cmp", "field": "device_id", "op": "gt", "value": 3},
             "actions": [{"kind": "publish", "topic": "x"}]}
        ]"#;
        assert!(matches!(
            RuleEngine::from_json(string_field_gt).expect_err("string field gt"),
            DaemonError::ConfigError(_)
        ));

        let numeric_field_str = r#"[
            {"id": "r", "when": {"kind": "cmp", "field": "value", "op": "gt", "value": "30"},
             "actions": [{"kind": "publish", "topic": "x"}]}
        ]"#;
        assert!(matches!(
            RuleEngine::from_json(numeric_field_str).expect_err("numeric field with string value"),
            DaemonError::ConfigError(_)
        ));

        let unknown_kind = r#"[
            {"id": "r", "when": {"kind": "between", "field": "value", "op": "gt", "value": 1},
             "actions": [{"kind": "publish", "topic": "x"}]}
        ]"#;
        assert!(matches!(
            RuleEngine::from_json(unknown_kind).expect_err("unknown condition kind"),
            DaemonError::ConfigError(_)
        ));

        let unknown_action_field = r#"[
            {"id": "r", "actions": [{"kind": "publish", "topic": "x", "qos": 1}]}
        ]"#;
        assert!(matches!(
            RuleEngine::from_json(unknown_action_field).expect_err("unknown action field"),
            DaemonError::ConfigError(_)
        ));

        let bad_path = r#"[
            {"id": "r", "select": ["$.value[abc]"], "actions": [{"kind": "publish", "topic": "x"}]}
        ]"#;
        assert!(matches!(
            RuleEngine::from_json(bad_path).expect_err("bad jsonpath"),
            DaemonError::ConfigError(_)
        ));
    }

    /// 时间戳在 JSON payload 中是**字符串**（项目红线：JSON number 是 double，> 2^53-1 丢精度）。
    #[test]
    fn timestamps_are_json_strings() {
        let json = r#"[
            {"id": "ts", "actions": [{"kind": "publish", "topic": "ts"}],
             "transform": {"source_id": "t", "target_point": "temp", "device_id": "m1"}}
        ]"#;
        let mut engine = RuleEngine::from_json(json).expect("rules");
        let out = engine.evaluate(raw("t", 1.0)).expect("ok");
        let payload = &out[0].payload;
        assert!(
            payload["collected_ts_ns"].is_string(),
            "collected_ts_ns must be a string, got {}",
            payload["collected_ts_ns"]
        );
        assert!(
            payload["device_ts_ns"].is_string(),
            "device_ts_ns must be a string, got {}",
            payload["device_ts_ns"]
        );
        assert_eq!(
            payload["device_ts_ns"].as_str(),
            Some("1700000000123456789")
        );
        assert!(payload["value"].is_f64());
        assert_eq!(payload["quality"].as_str(), Some("GOOD"));
        assert_eq!(payload["unit"].as_str(), Some("raw"), "unit default");
    }

    /// 空规则集 / `when: None` → 恒真转发；空规则集不产出消息。
    #[test]
    fn empty_rule_set_and_unconditional_rule() {
        let mut empty = RuleEngine::from_json("[]").expect("empty rules are valid");
        assert_eq!(empty.rule_count(), 0);
        assert_eq!(
            empty.evaluate(raw("t", 1.0)).expect("ok").len(),
            0,
            "no rules → no messages"
        );

        let json = r#"[
            {"id": "always", "actions": [{"kind": "publish", "topic": "all"}],
             "transform": {"source_id": "t", "target_point": "temp", "device_id": "m1"}}
        ]"#;
        let mut engine = RuleEngine::from_json(json).expect("rules");
        for value in [f64::MIN_POSITIVE, 0.0, 1e9] {
            let out = engine.evaluate(raw("t", value)).expect("ok");
            assert_eq!(out.len(), 1, "`when: null` is always true");
            assert_eq!(out[0].topic, "all");
        }
    }

    /// 多个 Publish → 多条消息；Remap 只影响其后的 Publish。
    #[test]
    fn publish_order_and_multiple_topics() {
        let json = r#"[
            {"id": "multi", "actions": [
                {"kind": "publish", "topic": "raw"},
                {"kind": "remap", "fields": {"v": "$.value"}},
                {"kind": "publish", "topic": "slim"}
            ],
            "transform": {"source_id": "t", "target_point": "temp", "device_id": "m1"}}
        ]"#;
        let mut engine = RuleEngine::from_json(json).expect("rules");
        let out = engine.evaluate(raw("t", 7.0)).expect("ok");
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].topic, "raw");
        assert!(out[0]
            .payload
            .as_object()
            .expect("obj")
            .contains_key("unit"));
        assert_eq!(out[1].topic, "slim");
        assert_eq!(out[1].payload.as_object().expect("obj").len(), 1);
        assert_eq!(out[1].payload["v"].as_f64(), Some(7.0));
    }

    /// 纯路由 `route`：按 `transform.target_point == point_id` 匹配，不触碰 DataProcessor。
    #[test]
    fn route_evaluates_already_processed_samples() {
        let json = r#"[
            {"id": "alarm", "when": {"kind": "cmp", "field": "value", "op": "gt", "value": 30},
             "actions": [{"kind": "publish", "topic": "alarms"}],
             "transform": {"source_id": "t", "target_point": "temp", "device_id": "m1"}},
            {"id": "other", "actions": [{"kind": "publish", "topic": "other"}],
             "transform": {"source_id": "p", "target_point": "pressure", "device_id": "m1"}}
        ]"#;
        let engine = RuleEngine::from_json(json).expect("rules");
        assert_eq!(engine.rule_count(), 2);

        let out = engine.route(&processed("temp", 36.5, "degC"));
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].rule_id, "alarm");

        let out = engine.route(&processed("pressure", 36.5, "kPa"));
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].rule_id, "other");

        let out = engine.route(&processed("unknown_point", 36.5, ""));
        assert_eq!(out.len(), 0, "no rule owns this point");
    }

    /// 配置冲突：重复规则 id / 重复 transform source_id → ConfigError（后者由 task 15 透传）。
    #[test]
    fn duplicate_rule_id_and_source_are_rejected() {
        let dup_id = r#"[
            {"id": "same", "actions": [{"kind": "publish", "topic": "a"}]},
            {"id": "same", "actions": [{"kind": "publish", "topic": "b"}]}
        ]"#;
        let err = RuleEngine::from_json(dup_id).expect_err("duplicate rule id");
        assert!(matches!(err, DaemonError::ConfigError(_)));
        assert!(err.to_string().contains("duplicate rule id"), "got {err}");

        let dup_source = r#"[
            {"id": "r1", "actions": [{"kind": "publish", "topic": "a"}],
             "transform": {"source_id": "t", "target_point": "p1", "device_id": "m1"}},
            {"id": "r2", "actions": [{"kind": "publish", "topic": "b"}],
             "transform": {"source_id": "t", "target_point": "p2", "device_id": "m1"}}
        ]"#;
        let err = RuleEngine::from_json(dup_source).expect_err("duplicate source_id");
        assert!(matches!(err, DaemonError::ConfigError(_)));
        assert!(err.to_string().contains("duplicate source_id"), "got {err}");
    }

    /// 未被任何规则关心的 source：静默丢弃（不产出、不报错）。
    #[test]
    fn unknown_source_without_wildcard_rules_is_dropped() {
        let json = r#"[
            {"id": "only_t", "actions": [{"kind": "publish", "topic": "t"}],
             "transform": {"source_id": "t", "target_point": "temp", "device_id": "m1"}}
        ]"#;
        let mut engine = RuleEngine::from_json(json).expect("rules");
        let out = engine.evaluate(raw("unknown_source", 1.0)).expect("ok");
        assert_eq!(out.len(), 0, "no rule cares about this source");
    }

    /// 对象形式 `{"rules": [...]}` 同样可解析。
    #[test]
    fn object_form_with_rules_key_is_accepted() {
        let json = r#"{"rules": [
            {"id": "obj", "actions": [{"kind": "publish", "topic": "obj_topic"}],
             "transform": {"source_id": "t", "target_point": "temp", "device_id": "m1"}}
        ]}"#;
        let mut engine = RuleEngine::from_json(json).expect("object form");
        let out = engine.evaluate(raw("t", 1.0)).expect("ok");
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].topic, "obj_topic");
    }

    // ---- 回归：QA 发现的「未知字段静默忽略」缺陷（Major） ----

    /// 顶层未知字段必须报 ConfigError，**不得**静默忽略。
    ///
    /// QA 实测：`[{"id":"a","bogus":123,"actions":[]}]` 曾被 ACCEPTED。
    /// 最坏后果是把 `transform` 拼错后规则静默降级为通配规则 → 对所有样本全量转发。
    #[test]
    fn unknown_top_level_field_is_rejected() {
        let err = RuleEngine::from_json(r#"[{"id":"a","bogus":123,"actions":[]}]"#)
            .expect_err("顶层未知字段必须报错");
        assert_eq!(
            err.error_code(),
            crate::error::ERR_CONFIG,
            "必须是 ConfigError(2000)"
        );
        let msg = err.to_string();
        assert!(msg.contains("bogus"), "错误信息应指出字段名: {msg}");
    }

    /// 把 `transform` 拼错的典型误配置：必须报错，绝不允许降级为通配规则。
    #[test]
    fn misspelled_transform_is_rejected_not_silently_wildcarded() {
        let err = RuleEngine::from_json(
            r#"[{"id":"a","tranform":{"source_id":"t"},"actions":[{"kind":"publish","topic":"x"}]}]"#,
        )
        .expect_err("拼错的 transform 必须报错");
        assert_eq!(err.error_code(), crate::error::ERR_CONFIG);
        assert!(
            err.to_string().contains("tranform"),
            "错误信息应指出拼错的键名: {err}"
        );
    }

    /// transform 内部的未知字段同样拒绝。
    #[test]
    fn unknown_transform_field_is_rejected() {
        let err = RuleEngine::from_json(
            r#"[{"id":"a","transform":{"source_id":"t","target_point":"p","device_id":"d","scal":2.0},"actions":[{"kind":"publish","topic":"x"}]}]"#,
        )
        .expect_err("transform 内部未知字段必须报错");
        assert_eq!(err.error_code(), crate::error::ERR_CONFIG);
        assert!(err.to_string().contains("scal"), "应指出 scal: {err}");
    }
}

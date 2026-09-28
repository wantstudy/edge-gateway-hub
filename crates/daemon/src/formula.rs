//! 公式引擎与计算点（计划 task 70）。
//!
//! 受限表达式引擎：**自研递归下降解析器**（词法 → 语法 → AST → 编译期校验 → 求值），
//! 不使用任何 `eval` / 脚本引擎（Lua / JS / Python / SQL 片段），**禁用集在解析期即拒绝**。
//!
//! # 边界声明（与计划一致，越界即返工）
//!
//! - 本模块只解决「值怎么算」，**不做**转发规则引擎（task 20/37）；
//! - **不重复** task 15 的映射 / 单位换算 / 死区：公式求值跑在 task 15 换算**之后**，
//!   [`FormulaEngine::eval_cycle`] 收到的就是工程单位值；
//! - **不定义质量码语义**：质量码枚举、严重度顺序与「最差质量继承」精确口径
//!   全部由 **task 53**（`codec.rs`）定义，本模块**只做使用**（[`crate::codec::Quality`]）。
//!   本模块只输出 [`CalcOutcome::failed`] / [`CalcFailure`]（自有类型）与
//!   [`CalcOutcome::quality`]（引用 `codec::Quality`，零新增定义）；
//!   上层接线时 `failed == true` ⇒ `quality` 取 [`Quality::CalcFailed`]，
//!   **`hold_last` 保留值但 `failed` 仍为 true，不得伪装成 GOOD**。
//!   质量码与接线的转换一律走 `codec::Quality::to_wire()` / `from_wire()`（唯一边界），
//!   本模块不直接依赖 `protocol_proto` 的枚举。
//!   **失败映射表**（见下方「失败 → 质量码映射」）是唯一口径。

//! - **不实现 HTTP 接口**：`/api/points/formula/validate` 与 `dry-run` 属 mgmt 层，
//!   本模块只交付库层 API [`validate`] / [`dry_run_expr`] / [`FormulaEngine::dry_run`] / [`FormulaEngine::eval_cycle`]。
//!
//! # 语法
//!
//! - **字面量**：十进制数字（`12`、`3.14`、`1.5e3`），内部统一 `f64`；
//! - **点位引用**：`[point_id]`，允许字符 `A-Z a-z 0-9 _ - . : /`；可引用物理点与其它计算点；
//!   跨设备引用直接写目标 `point_id`（被引用设备离线 ⇒ 该引用按「缺失」处理，不阻塞其它计算点）；
//! - **运算符**（优先级由低到高）：`||` → `&&` → `== !=` → `< <= > >=` → `+ -` → `* / %`
//!   → 一元 `+ - !` → `^`（右结合）；括号 `( )` 提升优先级；
//!   比较与逻辑运算返回 `1.0`（真）/ `0.0`（假）；
//! - **函数白名单**：见 [`function_whitelist`]（每个函数带可求值示例与期望值，
//!   由测试 `whitelist_examples_all_evaluable` 保证示例真实可算）；
//! - **禁用集（解析期即拒绝）**：赋值 `=`、语句分隔 `;`、块 `{}`、字符串字面量 `" ' \``、
//!   关键字 `let/var/fn/return/while/for/loop/import/exec/eval/script/lua/js/sql/...`、
//!   任意非白名单标识符（含 I/O、反射、动态求值入口）。错误信息会列出全部允许函数。
//! - **数值语义**：内部统一 `f64`；输出按 [`OutputType`] 显式转换；整数输出做范围检查
//!   （越界 ⇒ [`CalcFailure::OutputOutOfRange`]）。
//!
//! # 依赖图与求值顺序
//!
//! - 解析每个计算点的引用 → 构建依赖 DAG → **拓扑排序**决定求值顺序（依赖先算）；
//! - **环检测在保存期即校验**，报错**必须给出环路径**（`R_Cost → R_Energy → R_Cost`，支持多级环）；
//! - **周期屏障**：同一采集周期内物理点（经换算）全部就绪后才执行公式求值；
//!   跨周期引用（`prev` / `delta` / `rate` / `hold`）只读上周期快照，不读本周期未完成值
//!   （历史快照在整周期求值结束后统一下移）；
//! - **跨设备引用**：被引用设备离线或该点本周期未更新 ⇒ 按 [`OnFailure`] 降级，
//!   **不阻塞其它计算点求值**。
//!
//! # 失败与策略
//!
//! 输入缺失 / 超时 / 非数值 / 除零 / 结果 NaN·±Inf / 输出越界 ⇒ `failed = true` + [`CalcFailure`]，
//! 并按 [`OnFailure`] 处理：`hold_last`（默认，保留上次有效值但**仍标记 failed**）、`null`（置空）、
//! `skip`（本周期不产出）。失败计数与最近原因见 [`FormulaEngine::failure_stat`]。
//!
//! ## 失败 → 质量码映射（唯一口径，口径本身属 task 53）
//!
//! | [`CalcFailure`] | 映射后的 [`Quality`] | 说明 |
//! |---|---|---|
//! | `MissingInput` | [`Quality::CalcFailed`] | 输入点缺失 / 超时 / 本周期未更新 |
//! | `NonNumeric` | [`Quality::CalcFailed`] | 输入有值但非数值（bool / 字符串 / 字节） |
//! | `DivideByZero` | [`Quality::CalcFailed`] | 除数为 0 |
//! | `NonFinite` | [`Quality::CalcFailed`] | 结果为 NaN / ±Inf |
//! | `OutputOutOfRange` | [`Quality::CalcFailed`] | 输出类型转换或限幅区间非法 |
//! | `Timeout` | [`Quality::CalcFailed`] | 超出求值步数预算 |
//! | `Cycle` | [`Quality::CalcFailed`] | 依赖成环（保存期即拒绝，运行期兜底） |
//! | `ResourceLimit` | [`Quality::CalcFailed`] | 资源超限 |
//!
//! **`OutOfRange` 留给物理点工程量程**（plan task 70 第 2686 行：计算点的「输出越界」归
//! `calc_failed`），因此本模块的 `OutputOutOfRange` **不映射**为 [`Quality::OutOfRange`]。
//!
//! ### 为什么「缺失输入不进 `worst()`」（关键，勿改）
//!
//! codec 的严重度是 `CalcFailed=3 < Bad=4 < Timeout=5`。
//! **只有真正取到可用数值的输入**（[`InputValue::Numeric`]）才以其自身 `quality` 参与
//! `worst()` 合并；[`InputValue::Missing`] 与 [`InputValue::NonNumeric`] 没有向公式提供
//! 可用数值，其携带的 `quality` **仅用于入口甄别与界面回显，绝不进 `worst()`**。
//! 否则一旦某个缺失输入携带 `Bad` / `Timeout`，`worst()` 就会选出它们，
//! 让严重度更低的 `CalcFailed` **永远浮不出来**，静默违背上表。
//! 求值失败时取 `Quality::worst(继承到的最差, Quality::CalcFailed)`。
//! dry-run 未命中与错误态 outcome 的展示兜底用 [`Quality::Bad`]，
//! 同样标注为**仅展示值，不进 `worst()`**。
//!
//! # 资源保护（防 DoS）
//!
//! 表达式长度上限 512 字节、AST 节点数上限 256、解析深度上限 32、单次求值步数预算 4096
//! （≈ 时间预算，远小于 5ms，**不起线程、不用定时器**）、依赖链深度上限 16、计算点数量上限 4096。
//! 超限即在保存期拒绝或标记 failed。
//!
//! # 历史不重算
//!
//! 公式变更**只对新增数据生效**：本模块的 [`HistoryRecord`] 是追加写日志，
//! [`FormulaEngine::reconfigure`] 只替换编译产物与依赖图，**不触碰任何既有历史记录**
//! （测试 `history_not_recomputed_after_formula_change` 断言改公式前后历史查询结果不变）。
//! 公式变更同时写入审计条目 [`FormulaAuditEntry`]（含改前 / 改后全文）。

use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
use std::fmt;

use crate::codec::Quality;
use crate::error::DaemonError;

/// 点位值表（`point_id` → 本周期值）。
pub type PointValues = HashMap<String, InputValue>;

/// 点位历史快照表（`point_id` → 环形历史，`[0]` = 上一周期，`[1]` = 上上周期）。
pub type PointHistory = HashMap<String, VecDeque<Option<f64>>>;

// ---- 资源上限 ----

/// 表达式长度上限（字节）。
pub const DEFAULT_MAX_EXPR_LEN: usize = 512;
/// AST 节点数上限。
pub const DEFAULT_MAX_AST_NODES: usize = 256;
/// 单次求值步数预算（等价于时间预算，约数十纳秒/步，≪ 5ms；不起线程）。
pub const DEFAULT_MAX_EVAL_STEPS: usize = 4096;
/// 解析递归深度上限（防括号嵌套爆栈）。
pub const DEFAULT_MAX_PARSE_DEPTH: usize = 32;
/// 依赖链深度上限。
pub const DEFAULT_MAX_DEP_DEPTH: usize = 16;
/// 计算点数量上限。
pub const DEFAULT_MAX_POINTS: usize = 4096;
/// 每点位保留的历史快照条数（`hold` / `prev` 的可回看窗口）。
pub const DEFAULT_MAX_HISTORY: usize = 64;

/// 资源保护上限（可测试注入；默认值即计划口径）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Limits {
    /// 表达式长度上限（字节）。
    pub max_expr_len: usize,
    /// AST 节点数上限。
    pub max_ast_nodes: usize,
    /// 单次求值步数预算。
    pub max_eval_steps: usize,
    /// 解析递归深度上限。
    pub max_parse_depth: usize,
    /// 依赖链深度上限。
    pub max_dep_depth: usize,
    /// 计算点数量上限。
    pub max_points: usize,
    /// 每点位保留的历史快照条数。
    pub max_history: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_expr_len: DEFAULT_MAX_EXPR_LEN,
            max_ast_nodes: DEFAULT_MAX_AST_NODES,
            max_eval_steps: DEFAULT_MAX_EVAL_STEPS,
            max_parse_depth: DEFAULT_MAX_PARSE_DEPTH,
            max_dep_depth: DEFAULT_MAX_DEP_DEPTH,
            max_points: DEFAULT_MAX_POINTS,
            max_history: DEFAULT_MAX_HISTORY,
        }
    }
}

// ---- 点位模型 ----

/// 计算点输出类型。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum OutputType {
    /// 单精度浮点（`f64 → f32`，溢出即 [`CalcFailure::OutputOutOfRange`]）。
    Float32,
    /// 双精度浮点（默认）。
    #[default]
    Float64,
    /// 64 位整数（`round` 后范围检查）。
    Int64,
    /// 布尔（非 0 ⇒ `1.0`，0 ⇒ `0.0`）。
    Bool,
}

impl OutputType {
    /// 配置里的稳定名字（供 JSON / 点位表持久化）。
    pub fn as_str(&self) -> &'static str {
        match self {
            OutputType::Float32 => "float32",
            OutputType::Float64 => "float64",
            OutputType::Int64 => "int64",
            OutputType::Bool => "bool",
        }
    }

    /// 从配置名解析；未知值返回 `None`（由调用方报错，不静默兜底）。
    pub fn from_str_value(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "float32" | "f32" => Some(OutputType::Float32),
            "float64" | "f64" | "double" => Some(OutputType::Float64),
            "int64" | "i64" => Some(OutputType::Int64),
            "bool" | "boolean" => Some(OutputType::Bool),
            _ => None,
        }
    }
}

/// 公式失败时的处置策略。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum OnFailure {
    /// 保留上次有效值（默认）；**值保留但失败标记不得伪装成成功**。
    #[default]
    HoldLast,
    /// 置空（对外输出 `None`）。
    Null,
    /// 本周期不产出（跳过该点）。
    Skip,
}

impl OnFailure {
    /// 配置里的稳定名字。
    pub fn as_str(&self) -> &'static str {
        match self {
            OnFailure::HoldLast => "hold_last",
            OnFailure::Null => "null",
            OnFailure::Skip => "skip",
        }
    }

    /// 从配置名解析；未知值返回 `None`。
    pub fn from_str_value(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "hold_last" | "hold" => Some(OnFailure::HoldLast),
            "null" | "none" => Some(OnFailure::Null),
            "skip" => Some(OnFailure::Skip),
            _ => None,
        }
    }
}

/// 计算点求值触发模式。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum EvalMode {
    /// 依赖变化才触发（任一依赖值或质量码变化）。
    #[default]
    OnChange,
    /// 每个采集周期都求值。
    Periodic,
}

impl EvalMode {
    /// 配置里的稳定名字。
    pub fn as_str(&self) -> &'static str {
        match self {
            EvalMode::OnChange => "on_change",
            EvalMode::Periodic => "periodic",
        }
    }

    /// 从配置名解析；未知值返回 `None`。
    pub fn from_str_value(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "on_change" | "onchange" => Some(EvalMode::OnChange),
            "periodic" | "cycle" => Some(EvalMode::Periodic),
            _ => None,
        }
    }
}

/// 计算点（派生点）配置：点位表里 `point_type = derived` 的一行。
#[derive(Debug, Clone, PartialEq)]
pub struct DerivedPointConfig {
    /// 点位标识（点位表主键，北向 `point_id`）。
    pub point_id: String,
    /// 点位名称；为空时以 `point_id` 兜底。
    pub name: String,
    /// 表达式原文（如 `[P_Inj] * [T_Barrel1] / 1000`）。
    pub expr: String,
    /// 输出类型。
    pub output_type: OutputType,
    /// 输出单位（如 `"kW"`）。
    pub unit: String,
    /// 输出死区（工程单位，≥ 0；`0.0` = 不过滤）。
    pub deadband: f64,
    /// 失败处置策略（默认 [`OnFailure::HoldLast`]）。
    pub on_failure: OnFailure,
    /// 求值触发模式（默认 [`EvalMode::OnChange`]）。
    pub eval_mode: EvalMode,
}

impl DerivedPointConfig {
    /// 便捷构造：浮点输出、无死区、默认策略与触发模式。
    pub fn new(point_id: &str, expr: &str) -> Self {
        Self {
            point_id: point_id.to_string(),
            name: point_id.to_string(),
            expr: expr.to_string(),
            output_type: OutputType::Float64,
            unit: String::new(),
            deadband: 0.0,
            on_failure: OnFailure::HoldLast,
            eval_mode: EvalMode::OnChange,
        }
    }

    /// 展示名：`name` 为空时回退 `point_id`。
    pub fn display_name(&self) -> &str {
        if self.name.is_empty() {
            &self.point_id
        } else {
            &self.name
        }
    }

    /// 单条配置自检。
    pub fn validate(&self) -> Result<(), FormulaError> {
        if self.point_id.trim().is_empty() {
            return Err(FormulaError::config(&self.point_id, "point_id 不能为空"));
        }
        if self.expr.trim().is_empty() {
            return Err(FormulaError::config(&self.point_id, "expr 不能为空"));
        }
        if !self.deadband.is_finite() || self.deadband < 0.0 {
            return Err(FormulaError::config(
                &self.point_id,
                format!("deadband 必须为有限且 >= 0 的数，实际 {}", self.deadband),
            ));
        }
        Ok(())
    }
}

// ---- 失败语义 ----

/// 公式求值失败原因（**自有类型**：`calc_failed` 的枚举与文案由 task 53 定义，本模块不越界）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CalcFailure {
    /// 输入缺失（点位不存在 / 设备离线 / 本周期未更新）。
    MissingInput,
    /// 输入非数值（bool / 字符串 / 字节）。
    NonNumeric,
    /// 除零或取模零。
    DivideByZero,
    /// 结果 NaN / ±Inf。
    NonFinite,
    /// 输出类型范围越界（整型超范围、f32 溢出等）。
    OutputOutOfRange,
    /// 求值超出步数预算（防 DoS，等价于超时）。
    Timeout,
    /// 依赖成环（保存期应已被拒绝；运行期兜底）。
    Cycle,
    /// 资源超限（长度 / 节点 / 深度 / 数量）。
    ResourceLimit,
}

impl CalcFailure {
    /// 稳定的英文代号（供 JSON / 日志 / 界面 i18n 映射）。
    pub fn as_str(&self) -> &'static str {
        match self {
            CalcFailure::MissingInput => "missing_input",
            CalcFailure::NonNumeric => "non_numeric",
            CalcFailure::DivideByZero => "divide_by_zero",
            CalcFailure::NonFinite => "non_finite",
            CalcFailure::OutputOutOfRange => "output_out_of_range",
            CalcFailure::Timeout => "timeout",
            CalcFailure::Cycle => "cycle",
            CalcFailure::ResourceLimit => "resource_limit",
        }
    }
}

impl fmt::Display for CalcFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// 求值结果。
///
/// - `failed == true` 时上层应映射为 task 53 的 `calc_failed` 质量码；
/// - `hold_last` 策略下 `value` 可能为 `Some(上次值)`，但 `failed` **必须保持 true**。
#[derive(Debug, Clone, PartialEq)]
pub struct CalcOutcome {
    /// 求值结果（失败策略生效后的最终值；`None` 表示无值）。
    pub value: Option<f64>,
    /// 是否失败（**不得因 `hold_last` 保留值而置 false**）。
    pub failed: bool,
    /// 失败原因。
    pub reason: Option<CalcFailure>,
    /// 质量码（**引用 task 53 的 [`crate::codec::Quality`]，本模块零定义**）。
    ///
    /// 取值规则：
    /// - 成功：`worst()` 合并**所有真正取到可用数值的输入**各自的 `quality`；
    /// - 失败：`Quality::worst(继承到的最差, Quality::CalcFailed)`；
    /// - 具体映射见模块文档「失败 → 质量码映射」。
    pub quality: Quality,
}

impl CalcOutcome {
    /// 成功结果。
    fn ok(value: f64, quality: Quality) -> Self {
        Self {
            value: Some(value),
            failed: false,
            reason: None,
            quality,
        }
    }

    /// 失败结果。
    fn failed(reason: CalcFailure, quality: Quality) -> Self {
        Self {
            value: None,
            failed: true,
            reason: Some(reason),
            quality,
        }
    }
}

/// 严重度序号数值编码（`quality([p])` 的返回值）。
///
/// # 口径（重要）
///
/// 返回的是 **codec 的 severity 序号**（`Good=0 … CommError=6`，见
/// [`crate::codec::Quality::severity`]），**不是 Rust 枚举的 discriminants**，
/// 也与旧 `telemetry.proto` 的枚举序号（GOOD=1 / UNCERTAIN=2 / BAD=3 / SIMULATED=4）不同。
/// 严重度序号是本模块的记录口径，取值由 task 53 定义，本模块只是引用。
/// 序号 0..6 的取值由测试 `severity_ordinal_is_stable_and_locked` 固化。
pub fn severity_code(quality: Quality) -> f64 {
    f64::from(quality.severity())
}

// ---- 输入值 ----

/// 参与求值的点位输入值（区分「缺失」与「非数值」两种失败语义）。
///
/// # `worst()` 参与度（关键，见模块文档「为什么缺失输入不进 `worst()`」）
///
/// 只有 [`Self::Numeric`]（真正取到可用数值）才参与最差质量继承；
/// [`Self::Missing`] 与 [`Self::NonNumeric`] 携带的 `quality`
/// **仅用于入口甄别与界面回显，不进 `worst()`**。
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum InputValue {
    /// 数值（已解码 + 已换算的工程单位值）。
    Numeric(f64, Quality),
    /// 缺失：点位不存在 / 设备离线 / 本周期未更新。
    Missing(Quality),
    /// 有值但非数值（bool / 字符串 / 字节）。
    NonNumeric(Quality),
}

impl InputValue {
    /// 良好数值。
    pub fn good(value: f64) -> Self {
        InputValue::Numeric(value, Quality::Good)
    }

    /// 缺失（离线 / 未更新）。
    ///
    /// 携带的 [`Quality::Bad`] 是**仅展示值**（缺失本身没有读数质量可言），
    /// **不参与 `worst()` 合并**——见模块文档。
    pub fn missing() -> Self {
        InputValue::Missing(Quality::Bad)
    }

    /// 取值（缺失与非数值均为 `None`）。
    pub fn value(&self) -> Option<f64> {
        match self {
            InputValue::Numeric(v, _) => Some(*v),
            InputValue::Missing(_) | InputValue::NonNumeric(_) => None,
        }
    }

    /// 质量码。
    pub fn quality(&self) -> Quality {
        match self {
            InputValue::Numeric(_, q) | InputValue::Missing(q) | InputValue::NonNumeric(q) => *q,
        }
    }

    /// **参与 `worst()` 合并的质量码**：只有取到可用数值的输入才有资格。
    ///
    /// `Missing` / `NonNumeric` 返回 `None`，即**不参与**最差质量继承；
    /// 它们转为试探性失败原因（[`CalcFailure::MissingInput`] /
    /// [`CalcFailure::NonNumeric`]），最终由
    /// `Quality::worst(继承到的最差, Quality::CalcFailed)` 决定输出。
    pub fn inheritable_quality(&self) -> Option<Quality> {
        match self {
            InputValue::Numeric(_, q) => Some(*q),
            InputValue::Missing(_) | InputValue::NonNumeric(_) => None,
        }
    }
}

// ---- 白名单函数 ----

/// 白名单函数。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Builtin {
    /// `abs(x)`
    Abs,
    /// `ceil(x)`
    Ceil,
    /// `floor(x)`
    Floor,
    /// `round(x)`
    Round,
    /// `clamp(x, lo, hi)`（`lo > hi` 属配置错误 ⇒ **判为失败**，不 panic、不静默兜底）
    Clamp,
    /// `min(a, b, ...)`
    Min,
    /// `max(a, b, ...)`
    Max,
    /// `sqrt(x)`（负数 ⇒ non_finite）
    Sqrt,
    /// `pow(x, y)`（`|y| > 1024` 直接判 non_finite，防算力滥用）
    Pow,
    /// `exp(x)`
    Exp,
    /// `log(x)`（自然对数，≤ 0 ⇒ non_finite）
    Log,
    /// `log10(x)`
    Log10,
    /// `if(cond, a, b)`（**惰性**：只求值被选中的分支）
    If,
    /// `prev([p])`：上一周期值
    Prev,
    /// `delta([p])`：本周期 − 上一周期
    Delta,
    /// `rate([p])`：`delta / dt`（单位时间变化率）
    Rate,
    /// `hold([p], n)`：取当前或最近 n 个周期内最近一次有效值
    Hold,
    /// `quality([p])`：取 **codec severity 序号**（`Good=0 … CommError=6`）
    Quality,
}

/// 函数文档（供手册与 UI 函数面板；`example` 保证可求值且等于 `example_value`）。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FunctionDoc {
    /// 函数名。
    pub name: &'static str,
    /// 签名。
    pub signature: &'static str,
    /// 用途说明。
    pub summary: &'static str,
    /// 可求值示例（依赖 `[X]=8`、`[Y]=2`、上一周期 `[X]=5`、`dt=1s`）。
    pub example: &'static str,
    /// 示例的期望求值结果。
    pub example_value: f64,
}

impl Builtin {
    /// 函数名。
    pub fn name(&self) -> &'static str {
        match self {
            Builtin::Abs => "abs",
            Builtin::Ceil => "ceil",
            Builtin::Floor => "floor",
            Builtin::Round => "round",
            Builtin::Clamp => "clamp",
            Builtin::Min => "min",
            Builtin::Max => "max",
            Builtin::Sqrt => "sqrt",
            Builtin::Pow => "pow",
            Builtin::Exp => "exp",
            Builtin::Log => "log",
            Builtin::Log10 => "log10",
            Builtin::If => "if",
            Builtin::Prev => "prev",
            Builtin::Delta => "delta",
            Builtin::Rate => "rate",
            Builtin::Hold => "hold",
            Builtin::Quality => "quality",
        }
    }

    /// 按函数名查表（大小写不敏感）。
    pub fn from_name(name: &str) -> Option<Self> {
        let lower = name.to_ascii_lowercase();
        let all = [
            Builtin::Abs,
            Builtin::Ceil,
            Builtin::Floor,
            Builtin::Round,
            Builtin::Clamp,
            Builtin::Min,
            Builtin::Max,
            Builtin::Sqrt,
            Builtin::Pow,
            Builtin::Exp,
            Builtin::Log,
            Builtin::Log10,
            Builtin::If,
            Builtin::Prev,
            Builtin::Delta,
            Builtin::Rate,
            Builtin::Hold,
            Builtin::Quality,
        ];
        all.iter().copied().find(|f| f.name() == lower)
    }

    /// 参数个数区间 `(min, max)`（`usize::MAX` 表示不限）。
    pub fn arity(&self) -> (usize, usize) {
        match self {
            Builtin::Clamp => (3, 3),
            Builtin::Min | Builtin::Max => (2, usize::MAX),
            Builtin::Pow | Builtin::Hold => (2, 2),
            Builtin::If => (3, 3),
            _ => (1, 1),
        }
    }

    /// 参数个数文案（如 `3` / `2..N` / `1`）。
    pub fn arity_desc(&self) -> String {
        let (lo, hi) = self.arity();
        if hi == usize::MAX {
            format!("{lo}..N")
        } else if lo == hi {
            format!("{lo}")
        } else {
            format!("{lo}..{hi}")
        }
    }

    /// 签名文案。
    pub fn signature(&self) -> String {
        format!("{}({})", self.name(), self.arity_desc())
    }

    /// 第一个参数是否必须是点位引用 `[point_id]`（状态类函数）。
    pub fn requires_point_arg(&self) -> bool {
        matches!(
            self,
            Builtin::Prev | Builtin::Delta | Builtin::Rate | Builtin::Hold | Builtin::Quality
        )
    }

    /// 是否惰性求值（`if` 只算被选中分支）。
    pub fn is_lazy(&self) -> bool {
        matches!(self, Builtin::If)
    }

    /// 全部白名单函数名（错误信息中回显，帮助用户定位）。
    pub fn allowed_list() -> String {
        let docs = function_whitelist();
        let mut names: Vec<&'static str> = Vec::with_capacity(docs.len());
        for doc in &docs {
            names.push(doc.name);
        }
        names.join(", ")
    }
}

/// 全部白名单函数及文档（**手册函数表的唯一来源**）。
pub fn function_whitelist() -> Vec<FunctionDoc> {
    vec![
        FunctionDoc {
            name: "abs",
            signature: "abs(x)",
            summary: "绝对值",
            example: "abs([Y] - [X])",
            example_value: 6.0,
        },
        FunctionDoc {
            name: "ceil",
            signature: "ceil(x)",
            summary: "向上取整",
            example: "ceil([X] / 3)",
            example_value: 3.0,
        },
        FunctionDoc {
            name: "floor",
            signature: "floor(x)",
            summary: "向下取整",
            example: "floor([X] / 3)",
            example_value: 2.0,
        },
        FunctionDoc {
            name: "round",
            signature: "round(x)",
            summary: "四舍五入",
            example: "round([X] / 3)",
            example_value: 3.0,
        },
        FunctionDoc {
            name: "clamp",
            signature: "clamp(x, lo, hi)",
            summary: "限幅（lo > hi 时返回 hi）",
            example: "clamp([X], 0, 5)",
            example_value: 5.0,
        },
        FunctionDoc {
            name: "min",
            signature: "min(a, b, ...)",
            summary: "最小值（≥2 个参数）",
            example: "min([X], [Y])",
            example_value: 2.0,
        },
        FunctionDoc {
            name: "max",
            signature: "max(a, b, ...)",
            summary: "最大值（≥2 个参数）",
            example: "max([X], [Y])",
            example_value: 8.0,
        },
        FunctionDoc {
            name: "sqrt",
            signature: "sqrt(x)",
            summary: "平方根（负数 → non_finite）",
            example: "sqrt([X] * 2)",
            example_value: 4.0,
        },
        FunctionDoc {
            name: "pow",
            signature: "pow(x, y)",
            summary: "幂（|y| > 1024 直接判 non_finite）",
            example: "pow([Y], 3)",
            example_value: 8.0,
        },
        FunctionDoc {
            name: "exp",
            signature: "exp(x)",
            summary: "自然指数",
            example: "exp([Y] - [Y])",
            example_value: 1.0,
        },
        FunctionDoc {
            name: "log",
            signature: "log(x)",
            summary: "自然对数（≤ 0 → non_finite）",
            example: "log([X] / [X])",
            example_value: 0.0,
        },
        FunctionDoc {
            name: "log10",
            signature: "log10(x)",
            summary: "常用对数",
            example: "log10([X] * 12.5)",
            example_value: 2.0,
        },
        FunctionDoc {
            name: "if",
            signature: "if(cond, a, b)",
            summary: "条件（惰性，只算选中分支）",
            example: "if([Y] > 0, [X] / [Y], 0)",
            example_value: 4.0,
        },
        FunctionDoc {
            name: "prev",
            signature: "prev([p])",
            summary: "上一周期值（跨周期只读快照）",
            example: "prev([X])",
            example_value: 5.0,
        },
        FunctionDoc {
            name: "delta",
            signature: "delta([p])",
            summary: "本周期 − 上一周期",
            example: "delta([X])",
            example_value: 3.0,
        },
        FunctionDoc {
            name: "rate",
            signature: "rate([p])",
            summary: "单位时间变化率（delta / dt）",
            example: "rate([X])",
            example_value: 3.0,
        },
        FunctionDoc {
            name: "hold",
            signature: "hold([p], n)",
            summary: "取当前或最近 n 个周期内最近一次有效值",
            example: "hold([X], 3)",
            example_value: 8.0,
        },
        FunctionDoc {
            name: "quality",
            signature: "quality([p])",
            summary: "质量码数值（GOOD=1 / UNCERTAIN=2 / BAD=3 / SIMULATED=4）",
            example: "quality([X])",
            example_value: 0.0,
        },
    ]
}

// ---- 词法 ----

/// 编译期错误（含字符位，供界面定位）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompileError {
    /// 错误说明（未知函数会回显全部允许函数）。
    pub message: String,
    /// 出错字符位（字节偏移，从 0 开始）。
    pub position: usize,
}

impl CompileError {
    /// 构造错误。
    pub fn new(position: usize, message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            position,
        }
    }
}

impl fmt::Display for CompileError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "公式语法错误（字符位 {}）：{}",
            self.position, self.message
        )
    }
}

impl std::error::Error for CompileError {}

#[derive(Debug, Clone, PartialEq)]
enum Token {
    Number(f64),
    Ident(String),
    PointRef(String),
    Plus,
    Minus,
    Star,
    Slash,
    Percent,
    Caret,
    LParen,
    RParen,
    Comma,
    EqEq,
    NotEq,
    Lt,
    Le,
    Gt,
    Ge,
    AndAnd,
    OrOr,
    Not,
}

#[derive(Debug, Clone, PartialEq)]
struct Tok {
    kind: Token,
    pos: usize,
}

/// 禁止的关键字 / 脚本构造（解析期即拒绝）。
const FORBIDDEN_KEYWORDS: &[&str] = &[
    "let", "var", "const", "fn", "func", "def", "return", "while", "for", "loop", "break",
    "continue", "import", "include", "use", "exec", "eval", "script", "lua", "js", "python", "sql",
    "select", "match", "goto", "switch", "case", "try", "catch", "throw", "class", "new", "delete",
    "typeof", "await", "yield", "print", "println", "write", "read", "open", "spawn",
];

fn is_point_char(c: u8) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, b'_' | b'-' | b'.' | b':' | b'/')
}

fn byte_desc(c: u8) -> String {
    if c.is_ascii_graphic() {
        format!("`{}`", c as char)
    } else {
        format!("0x{c:02X}")
    }
}

struct Lexer<'a> {
    src: &'a [u8],
    pos: usize,
}

impl<'a> Lexer<'a> {
    fn new(src: &'a str) -> Self {
        Self {
            src: src.as_bytes(),
            pos: 0,
        }
    }

    fn peek(&self) -> Option<u8> {
        self.src.get(self.pos).copied()
    }

    fn bump(&mut self) -> Option<u8> {
        let c = self.peek();
        if c.is_some() {
            self.pos += 1;
        }
        c
    }

    fn skip_ws(&mut self) {
        while matches!(self.peek(), Some(b' ' | b'\t' | b'\r' | b'\n')) {
            self.pos += 1;
        }
    }

    fn slice(&self, start: usize, end: usize) -> Result<&'a str, CompileError> {
        let raw = self
            .src
            .get(start..end)
            .ok_or_else(|| CompileError::new(start, "词法扫描越界"))?;
        std::str::from_utf8(raw).map_err(|_| CompileError::new(start, "表达式含非法 UTF-8 字节"))
    }

    fn read_number(&mut self) -> Result<f64, CompileError> {
        let start = self.pos;
        while matches!(self.peek(), Some(b'0'..=b'9')) {
            self.pos += 1;
        }
        if self.peek() == Some(b'.') {
            self.pos += 1;
            let before = self.pos;
            while matches!(self.peek(), Some(b'0'..=b'9')) {
                self.pos += 1;
            }
            if self.pos == before {
                return Err(CompileError::new(start, "数字字面量小数点后必须跟数字"));
            }
        }
        if matches!(self.peek(), Some(b'e' | b'E')) {
            let save = self.pos;
            self.pos += 1;
            if matches!(self.peek(), Some(b'+' | b'-')) {
                self.pos += 1;
            }
            let before = self.pos;
            while matches!(self.peek(), Some(b'0'..=b'9')) {
                self.pos += 1;
            }
            if self.pos == before {
                // `1e` 之类：回退到 `e` 之前，让后续标识符规则给出「未知标识符」错误。
                self.pos = save;
            }
        }
        let text = self.slice(start, self.pos)?;
        text.parse::<f64>()
            .map_err(|_| CompileError::new(start, format!("数字字面量无法解析：{text}")))
    }

    fn read_ident(&mut self) -> Result<String, CompileError> {
        let start = self.pos;
        while matches!(
            self.peek(),
            Some(b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'_')
        ) {
            self.pos += 1;
        }
        let text = self.slice(start, self.pos)?;
        let lower = text.to_ascii_lowercase();
        if FORBIDDEN_KEYWORDS.contains(&lower.as_str()) {
            return Err(CompileError::new(
                start,
                format!(
                    "禁止的关键字 / 脚本构造 `{text}`：公式引擎只支持纯表达式 \
                     （无赋值、无循环、无字符串、无 I/O、无反射、无动态求值）"
                ),
            ));
        }
        Ok(text.to_string())
    }

    fn read_point_ref(&mut self) -> Result<String, CompileError> {
        let start = self.pos;
        while let Some(c) = self.peek() {
            if c == b']' {
                break;
            }
            if !is_point_char(c) {
                return Err(CompileError::new(
                    self.pos,
                    format!(
                        "点位引用含非法字符 {}（只允许字母 / 数字 / `_ - . : /`）",
                        byte_desc(c)
                    ),
                ));
            }
            self.pos += 1;
        }
        if self.peek() != Some(b']') {
            return Err(CompileError::new(start, "点位引用 `[` 未闭合，缺少 `]`"));
        }
        let end = self.pos;
        self.bump();
        if end == start {
            return Err(CompileError::new(start, "空点位引用 `[]`"));
        }
        Ok(self.slice(start, end)?.to_string())
    }

    fn lex(&mut self) -> Result<Vec<Tok>, CompileError> {
        let mut toks: Vec<Tok> = Vec::new();
        loop {
            self.skip_ws();
            let Some(c) = self.peek() else { break };
            let start = self.pos;
            let kind = match c {
                b'0'..=b'9' | b'.' => Token::Number(self.read_number()?),
                b'[' => {
                    self.bump();
                    Token::PointRef(self.read_point_ref()?)
                }
                b'a'..=b'z' | b'A'..=b'Z' | b'_' => Token::Ident(self.read_ident()?),
                b'(' => {
                    self.bump();
                    Token::LParen
                }
                b')' => {
                    self.bump();
                    Token::RParen
                }
                b',' => {
                    self.bump();
                    Token::Comma
                }
                b'+' => {
                    self.bump();
                    Token::Plus
                }
                b'-' => {
                    self.bump();
                    Token::Minus
                }
                b'*' => {
                    self.bump();
                    Token::Star
                }
                b'/' => {
                    self.bump();
                    Token::Slash
                }
                b'%' => {
                    self.bump();
                    Token::Percent
                }
                b'^' => {
                    self.bump();
                    Token::Caret
                }
                b'=' => {
                    self.bump();
                    if self.peek() == Some(b'=') {
                        self.bump();
                        Token::EqEq
                    } else {
                        return Err(CompileError::new(
                            start,
                            "禁止赋值运算 `=`：公式只支持纯表达式（相等比较请用 `==`）",
                        ));
                    }
                }
                b'!' => {
                    self.bump();
                    if self.peek() == Some(b'=') {
                        self.bump();
                        Token::NotEq
                    } else {
                        Token::Not
                    }
                }
                b'<' => {
                    self.bump();
                    if self.peek() == Some(b'=') {
                        self.bump();
                        Token::Le
                    } else {
                        Token::Lt
                    }
                }
                b'>' => {
                    self.bump();
                    if self.peek() == Some(b'=') {
                        self.bump();
                        Token::Ge
                    } else {
                        Token::Gt
                    }
                }
                b'&' => {
                    self.bump();
                    if self.peek() == Some(b'&') {
                        self.bump();
                        Token::AndAnd
                    } else {
                        return Err(CompileError::new(
                            start,
                            "单个 `&` 无意义：逻辑与请用 `&&`（位运算不在白名单内）",
                        ));
                    }
                }
                b'|' => {
                    self.bump();
                    if self.peek() == Some(b'|') {
                        self.bump();
                        Token::OrOr
                    } else {
                        return Err(CompileError::new(
                            start,
                            "单个 `|` 无意义：逻辑或请用 `||`（位运算不在白名单内）",
                        ));
                    }
                }
                b';' | b'{' | b'}' => {
                    return Err(CompileError::new(
                        start,
                        format!(
                            "禁止的字符 {}：公式不支持语句 / 代码块（无赋值、无循环）",
                            byte_desc(c)
                        ),
                    ));
                }
                b'"' | b'\'' | b'`' => {
                    return Err(CompileError::new(
                        start,
                        format!(
                            "禁止的字符 {}：公式不支持字符串字面量与字符串操作",
                            byte_desc(c)
                        ),
                    ));
                }
                b'#' | b'@' | b'$' | b'~' | b'?' | b'\\' | b':' => {
                    return Err(CompileError::new(
                        start,
                        format!(
                            "禁止的字符 {}：只允许数字 / 点位引用 / 白名单函数 / 运算符",
                            byte_desc(c)
                        ),
                    ));
                }
                _ => {
                    return Err(CompileError::new(
                        start,
                        format!(
                            "非法字符 {}：只允许数字 / 点位引用 / 白名单函数 / 运算符",
                            byte_desc(c)
                        ),
                    ));
                }
            };
            toks.push(Tok { kind, pos: start });
        }
        Ok(toks)
    }
}

// ---- AST ----

/// 一元运算符。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnaryOp {
    /// 取负 `-x`
    Neg,
    /// 取正 `+x`
    Pos,
    /// 逻辑非 `!x`
    Not,
}

/// 二元运算符。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BinaryOp {
    /// `+`
    Add,
    /// `-`
    Sub,
    /// `*`
    Mul,
    /// `/`
    Div,
    /// `%`
    Rem,
    /// `^`
    Pow,
    /// `==`
    Eq,
    /// `!=`
    Ne,
    /// `<`
    Lt,
    /// `<=`
    Le,
    /// `>`
    Gt,
    /// `>=`
    Ge,
    /// `&&`
    And,
    /// `||`
    Or,
}

/// 表达式 AST（编译产物；求值只走 AST，不再解析原文）。
#[derive(Debug, Clone, PartialEq)]
pub enum Expr {
    /// 数字字面量。
    Number(f64),
    /// 点位引用 `[point_id]`。
    PointRef(String),
    /// 一元运算。
    Unary {
        /// 运算符。
        op: UnaryOp,
        /// 操作数。
        operand: Box<Expr>,
    },
    /// 二元运算。
    Binary {
        /// 运算符。
        op: BinaryOp,
        /// 左操作数。
        lhs: Box<Expr>,
        /// 右操作数。
        rhs: Box<Expr>,
    },
    /// 白名单函数调用。
    Call {
        /// 函数。
        func: Builtin,
        /// 实参列表。
        args: Vec<Expr>,
    },
}

const PREC_OR: u8 = 1;
const PREC_AND: u8 = 2;
const PREC_EQ: u8 = 3;
const PREC_REL: u8 = 4;
const PREC_ADD: u8 = 5;
const PREC_MUL: u8 = 6;
const PREC_UNARY: u8 = 7;
const PREC_POWER: u8 = 8;

fn binary_of(token: &Token) -> Option<(BinaryOp, u8)> {
    match token {
        Token::OrOr => Some((BinaryOp::Or, PREC_OR)),
        Token::AndAnd => Some((BinaryOp::And, PREC_AND)),
        Token::EqEq => Some((BinaryOp::Eq, PREC_EQ)),
        Token::NotEq => Some((BinaryOp::Ne, PREC_EQ)),
        Token::Lt => Some((BinaryOp::Lt, PREC_REL)),
        Token::Le => Some((BinaryOp::Le, PREC_REL)),
        Token::Gt => Some((BinaryOp::Gt, PREC_REL)),
        Token::Ge => Some((BinaryOp::Ge, PREC_REL)),
        Token::Plus => Some((BinaryOp::Add, PREC_ADD)),
        Token::Minus => Some((BinaryOp::Sub, PREC_ADD)),
        Token::Star => Some((BinaryOp::Mul, PREC_MUL)),
        Token::Slash => Some((BinaryOp::Div, PREC_MUL)),
        Token::Percent => Some((BinaryOp::Rem, PREC_MUL)),
        _ => None,
    }
}

fn prefix_of(token: &Token) -> Option<UnaryOp> {
    match token {
        Token::Minus => Some(UnaryOp::Neg),
        Token::Plus => Some(UnaryOp::Pos),
        Token::Not => Some(UnaryOp::Not),
        _ => None,
    }
}

struct Parser<'a> {
    toks: &'a [Tok],
    i: usize,
    nodes: usize,
    depth: usize,
    limits: &'a Limits,
}

impl<'a> Parser<'a> {
    fn new(toks: &'a [Tok], limits: &'a Limits) -> Self {
        Self {
            toks,
            i: 0,
            nodes: 0,
            depth: 0,
            limits,
        }
    }

    fn peek(&self) -> Option<&Tok> {
        self.toks.get(self.i)
    }

    fn bump(&mut self) -> Option<&Tok> {
        let tok = self.toks.get(self.i);
        if tok.is_some() {
            self.i += 1;
        }
        tok
    }

    fn at_end(&self) -> bool {
        self.i >= self.toks.len()
    }

    fn eof_pos(&self) -> usize {
        self.toks.last().map(|t| t.pos + 1).unwrap_or(0)
    }

    fn expect(&mut self, kind: &Token, what: &str) -> Result<(), CompileError> {
        match self.peek() {
            Some(tok) if &tok.kind == kind => {
                self.bump();
                Ok(())
            }
            Some(tok) => Err(CompileError::new(tok.pos, format!("期望 {what}"))),
            None => Err(CompileError::new(
                self.eof_pos(),
                format!("表达式意外结束：期望 {what}"),
            )),
        }
    }

    fn enter(&mut self) -> Result<(), CompileError> {
        self.depth += 1;
        if self.depth > self.limits.max_parse_depth {
            return Err(CompileError::new(
                self.eof_pos(),
                format!(
                    "表达式嵌套过深（超过 {} 层）：请拆分公式或降低括号 / 运算符嵌套",
                    self.limits.max_parse_depth
                ),
            ));
        }
        Ok(())
    }

    fn exit(&mut self) {
        self.depth = self.depth.saturating_sub(1);
    }

    fn count_node(&mut self) -> Result<(), CompileError> {
        self.nodes += 1;
        if self.nodes > self.limits.max_ast_nodes {
            return Err(CompileError::new(
                self.eof_pos(),
                format!(
                    "AST 节点数超过上限 {}：请拆分公式或简化表达式",
                    self.limits.max_ast_nodes
                ),
            ));
        }
        Ok(())
    }

    fn parse_root(&mut self) -> Result<Expr, CompileError> {
        if self.at_end() {
            return Err(CompileError::new(0, "表达式为空"));
        }
        let expr = self.parse_expr(0)?;
        if let Some(tok) = self.peek() {
            return Err(CompileError::new(
                tok.pos,
                "表达式结尾存在多余内容（是否漏了运算符或括号不匹配？）",
            ));
        }
        Ok(expr)
    }

    fn parse_expr(&mut self, min_prec: u8) -> Result<Expr, CompileError> {
        self.enter()?;
        let mut lhs = self.parse_unary(min_prec)?;
        while let Some(tok) = self.peek() {
            let Some((op, prec)) = binary_of(&tok.kind) else {
                break;
            };
            if prec < min_prec {
                break;
            }
            self.bump();
            let rhs = self.parse_expr(prec.saturating_add(1))?;
            self.count_node()?;
            lhs = Expr::Binary {
                op,
                lhs: Box::new(lhs),
                rhs: Box::new(rhs),
            };
        }
        self.exit();
        Ok(lhs)
    }

    fn parse_unary(&mut self, min_prec: u8) -> Result<Expr, CompileError> {
        let op = match self.peek() {
            Some(tok) => prefix_of(&tok.kind),
            None => None,
        };
        if let Some(op) = op {
            self.bump();
            let operand = self.parse_expr(PREC_UNARY)?;
            self.count_node()?;
            return Ok(Expr::Unary {
                op,
                operand: Box::new(operand),
            });
        }
        self.parse_power(min_prec)
    }

    fn parse_power(&mut self, min_prec: u8) -> Result<Expr, CompileError> {
        let base = self.parse_primary()?;
        if min_prec <= PREC_POWER && matches!(self.peek().map(|t| &t.kind), Some(Token::Caret)) {
            self.bump();
            let exponent = self.parse_expr(PREC_POWER)?;
            self.count_node()?;
            return Ok(Expr::Binary {
                op: BinaryOp::Pow,
                lhs: Box::new(base),
                rhs: Box::new(exponent),
            });
        }
        Ok(base)
    }

    fn parse_primary(&mut self) -> Result<Expr, CompileError> {
        let Some(tok) = self.peek() else {
            return Err(CompileError::new(
                self.eof_pos(),
                "表达式意外结束：期望数字 / 点位引用 / 函数 / `(`",
            ));
        };
        let pos = tok.pos;
        match tok.kind.clone() {
            Token::Number(v) => {
                self.bump();
                self.count_node()?;
                Ok(Expr::Number(v))
            }
            Token::PointRef(id) => {
                self.bump();
                self.count_node()?;
                Ok(Expr::PointRef(id))
            }
            Token::Ident(name) => {
                self.bump();
                if !matches!(self.peek().map(|t| &t.kind), Some(Token::LParen)) {
                    if name.eq_ignore_ascii_case("if") {
                        return Err(CompileError::new(
                            pos,
                            "`if` 必须作为函数使用：if(cond, a, b)",
                        ));
                    }
                    return Err(CompileError::new(
                        pos,
                        format!(
                            "未知标识符 `{name}`：只允许白名单函数调用与点位引用 `[point_id]`；允许的函数：{}",
                            Builtin::allowed_list()
                        ),
                    ));
                }
                self.parse_call(&name, pos)
            }
            Token::LParen => {
                self.bump();
                let inner = self.parse_expr(0)?;
                self.expect(&Token::RParen, "`)`")?;
                Ok(inner)
            }
            _ => Err(CompileError::new(
                pos,
                "此处期望数字 / 点位引用 / 函数 / `(`".to_string(),
            )),
        }
    }

    fn parse_call(&mut self, name: &str, pos: usize) -> Result<Expr, CompileError> {
        let func = Builtin::from_name(name).ok_or_else(|| {
            CompileError::new(
                pos,
                format!(
                    "未知函数 `{name}`：不在白名单内（禁用脚本 / I/O / 反射 / 动态求值）；允许的函数：{}",
                    Builtin::allowed_list()
                ),
            )
        })?;
        self.expect(&Token::LParen, &format!("函数 `{}` 后的 `(`", func.name()))?;
        let mut args: Vec<Expr> = Vec::new();
        if !matches!(self.peek().map(|t| &t.kind), Some(Token::RParen)) {
            loop {
                args.push(self.parse_expr(0)?);
                if matches!(self.peek().map(|t| &t.kind), Some(Token::Comma)) {
                    self.bump();
                    continue;
                }
                break;
            }
        }
        self.expect(&Token::RParen, &format!("函数 `{}` 的 `)`", func.name()))?;
        let (lo, hi) = func.arity();
        if args.len() < lo || args.len() > hi {
            return Err(CompileError::new(
                pos,
                format!(
                    "函数 `{}` 参数个数错误（期望 {}，实际 {}）；正确写法：{}",
                    func.name(),
                    func.arity_desc(),
                    args.len(),
                    func.signature()
                ),
            ));
        }
        if func.requires_point_arg() && !matches!(args.first(), Some(Expr::PointRef(_))) {
            return Err(CompileError::new(
                pos,
                format!(
                    "函数 `{}` 的第一个参数必须是点位引用 `[point_id]`，例如 {}([P_Temp])",
                    func.name(),
                    func.name()
                ),
            ));
        }
        self.count_node()?;
        Ok(Expr::Call { func, args })
    }
}

/// 编译产物：AST + 依赖列表 + 资源统计（**编译一次并缓存，禁止每周期重新解析**）。
#[derive(Debug, Clone, PartialEq)]
pub struct Program {
    source: String,
    root: Expr,
    deps: Vec<String>,
    nodes: usize,
}

impl Program {
    /// 按默认资源上限编译。
    pub fn compile(src: &str) -> Result<Self, CompileError> {
        Self::compile_with(src, &Limits::default())
    }

    /// 按指定资源上限编译（词法 → 语法 → 编译期校验）。
    pub fn compile_with(src: &str, limits: &Limits) -> Result<Self, CompileError> {
        if src.len() > limits.max_expr_len {
            return Err(CompileError::new(
                limits.max_expr_len,
                format!(
                    "表达式长度 {} 超过上限 {} 字节：请拆分为多个计算点",
                    src.len(),
                    limits.max_expr_len
                ),
            ));
        }
        let mut lexer = Lexer::new(src);
        let toks = lexer.lex()?;
        let mut parser = Parser::new(&toks, limits);
        let root = parser.parse_root()?;
        let mut deps: Vec<String> = Vec::new();
        collect_deps(&root, &mut deps);
        Ok(Self {
            source: src.to_string(),
            root,
            deps,
            nodes: parser.nodes,
        })
    }

    /// 表达式原文。
    pub fn source(&self) -> &str {
        &self.source
    }

    /// AST 根节点。
    pub fn root(&self) -> &Expr {
        &self.root
    }

    /// 引用到的点位（去重，按出现顺序）。
    pub fn deps(&self) -> &[String] {
        &self.deps
    }

    /// AST 节点数。
    pub fn node_count(&self) -> usize {
        self.nodes
    }
}

fn collect_deps(expr: &Expr, out: &mut Vec<String>) {
    match expr {
        Expr::Number(_) => {}
        Expr::PointRef(id) => {
            if !out.iter().any(|d| d == id) {
                out.push(id.clone());
            }
        }
        Expr::Unary { operand, .. } => collect_deps(operand, out),
        Expr::Binary { lhs, rhs, .. } => {
            collect_deps(lhs, out);
            collect_deps(rhs, out);
        }
        Expr::Call { args, .. } => {
            for arg in args {
                collect_deps(arg, out);
            }
        }
    }
}

// ---- 求值 ----

/// 求值上下文（**只读**；跨周期数据只能来自 `history` 快照）。
pub struct EvalContext<'a> {
    /// 本周期点位值（物理点已换算 + 本周期已算出的计算点）。
    pub current: &'a PointValues,
    /// 历史快照（`[0]` = 上一周期；周期屏障结束后统一下移）。
    pub history: &'a PointHistory,
    /// 本周期时长（秒），`rate` 用；`<= 0` 视为非法。
    pub dt_s: f64,
    /// 资源上限。
    pub limits: &'a Limits,
}

impl<'a> EvalContext<'a> {
    /// 构造求值上下文。
    pub fn new(
        current: &'a PointValues,
        history: &'a PointHistory,
        dt_s: f64,
        limits: &'a Limits,
    ) -> Self {
        Self {
            current,
            history,
            dt_s,
            limits,
        }
    }
}

#[derive(Debug)]
struct EvalState {
    steps: usize,
    worst: Quality,
    failure: Option<CalcFailure>,
}

impl EvalState {
    /// 初始状态：质量基准取 GOOD（后续按「更差者胜」累积）。
    fn new() -> Self {
        Self {
            steps: 0,
            worst: Quality::Good,
            failure: None,
        }
    }

    fn step(&mut self, limits: &Limits) -> bool {
        self.steps = self.steps.saturating_add(1);
        if self.steps > limits.max_eval_steps {
            self.fail(CalcFailure::Timeout);
            return false;
        }
        true
    }

    fn observe(&mut self, quality: Quality) {
        // 「最差质量继承」的唯一实现：直接用 task 53 的 `worst()`，本模块不自造规则。
        self.worst = Quality::worst(self.worst, quality);
    }

    fn fail(&mut self, reason: CalcFailure) {
        if self.failure.is_none() {
            self.failure = Some(reason);
        }
    }
}

fn finite_or_fail(v: f64, st: &mut EvalState) -> Option<f64> {
    if v.is_finite() {
        Some(v)
    } else {
        st.fail(CalcFailure::NonFinite);
        None
    }
}

fn current_value(id: &str, ctx: &EvalContext<'_>, st: &mut EvalState) -> Option<f64> {
    match ctx.current.get(id) {
        Some(InputValue::Numeric(v, q)) => {
            // 真正取到可用数值 ⇒ 以其自身 quality 参与最差质量继承。
            st.observe(*q);
            Some(*v)
        }
        Some(InputValue::Missing(_)) => {
            // **不 observe**：缺失输入没有可用数值，其携带的 quality 仅用于甄别与回显。
            // 若在此处 `observe(Bad)`，codec 下 `Bad=4 > CalcFailed=3` 会把
            // `calc_failed` 永久掩盖（测试 `missing_input_does_not_mask_calc_failed` 固化）。
            st.fail(CalcFailure::MissingInput);
            None
        }
        Some(InputValue::NonNumeric(_)) => {
            // 同上：有值但不可用于计算，不参与 `worst()`。
            st.fail(CalcFailure::NonNumeric);
            None
        }
        None => {
            st.fail(CalcFailure::MissingInput);
            None
        }
    }
}

fn history_value(id: &str, index: usize, ctx: &EvalContext<'_>) -> Option<f64> {
    ctx.history
        .get(id)
        .and_then(|series| series.get(index))
        .copied()
        .flatten()
}

fn point_arg(args: &[Expr], index: usize) -> Option<&str> {
    match args.get(index) {
        Some(Expr::PointRef(id)) => Some(id.as_str()),
        _ => None,
    }
}

fn eval_expr(expr: &Expr, ctx: &EvalContext<'_>, st: &mut EvalState) -> Option<f64> {
    if !st.step(ctx.limits) {
        return None;
    }
    match expr {
        Expr::Number(v) => Some(*v),
        Expr::PointRef(id) => current_value(id, ctx, st),
        Expr::Unary { op, operand } => {
            let v = eval_expr(operand, ctx, st)?;
            match op {
                UnaryOp::Neg => finite_or_fail(-v, st),
                UnaryOp::Pos => finite_or_fail(v, st),
                UnaryOp::Not => finite_or_fail(if v == 0.0 { 1.0 } else { 0.0 }, st),
            }
        }
        Expr::Binary { op, lhs, rhs } => {
            let a = eval_expr(lhs, ctx, st)?;
            let b = eval_expr(rhs, ctx, st)?;
            eval_binary(*op, a, b, st)
        }
        Expr::Call { func, args } => eval_call(*func, args, ctx, st),
    }
}

fn eval_binary(op: BinaryOp, a: f64, b: f64, st: &mut EvalState) -> Option<f64> {
    let raw = match op {
        BinaryOp::Add => a + b,
        BinaryOp::Sub => a - b,
        BinaryOp::Mul => a * b,
        BinaryOp::Div => {
            if b == 0.0 {
                st.fail(CalcFailure::DivideByZero);
                return None;
            }
            a / b
        }
        BinaryOp::Rem => {
            if b == 0.0 {
                st.fail(CalcFailure::DivideByZero);
                return None;
            }
            a % b
        }
        BinaryOp::Pow => {
            if !a.is_finite() || !b.is_finite() || b.abs() > 1024.0 {
                st.fail(CalcFailure::NonFinite);
                return None;
            }
            a.powf(b)
        }
        BinaryOp::Eq => bool01(a == b),
        BinaryOp::Ne => bool01(a != b),
        BinaryOp::Lt => bool01(a < b),
        BinaryOp::Le => bool01(a <= b),
        BinaryOp::Gt => bool01(a > b),
        BinaryOp::Ge => bool01(a >= b),
        BinaryOp::And => bool01(a != 0.0 && b != 0.0),
        BinaryOp::Or => bool01(a != 0.0 || b != 0.0),
    };
    finite_or_fail(raw, st)
}

fn bool01(flag: bool) -> f64 {
    if flag {
        1.0
    } else {
        0.0
    }
}

fn eval_call(
    func: Builtin,
    args: &[Expr],
    ctx: &EvalContext<'_>,
    st: &mut EvalState,
) -> Option<f64> {
    if func.is_lazy() {
        // `if(cond, a, b)`：只求值被选中的分支（避免未选中分支的除零 / 缺失污染结果）。
        let cond = eval_expr(args.first()?, ctx, st)?;
        let branch = if cond != 0.0 {
            args.get(1)?
        } else {
            args.get(2)?
        };
        return eval_expr(branch, ctx, st);
    }

    if func.requires_point_arg() {
        let id = point_arg(args, 0)?;
        return match func {
            Builtin::Prev => {
                st.observe_input_quality(id, ctx);
                match history_value(id, 0, ctx) {
                    Some(v) => Some(v),
                    None => {
                        st.fail(CalcFailure::MissingInput);
                        None
                    }
                }
            }
            Builtin::Delta => {
                let cur = current_value(id, ctx, st)?;
                let prev = history_value(id, 0, ctx).or_else(|| {
                    st.fail(CalcFailure::MissingInput);
                    None
                })?;
                finite_or_fail(cur - prev, st)
            }
            Builtin::Rate => {
                let cur = current_value(id, ctx, st)?;
                let prev = history_value(id, 0, ctx).or_else(|| {
                    st.fail(CalcFailure::MissingInput);
                    None
                })?;
                if !ctx.dt_s.is_finite() || ctx.dt_s <= 0.0 {
                    st.fail(CalcFailure::DivideByZero);
                    return None;
                }
                finite_or_fail((cur - prev) / ctx.dt_s, st)
            }
            Builtin::Hold => {
                let n_raw = eval_expr(args.get(1)?, ctx, st)?;
                if !n_raw.is_finite() {
                    st.fail(CalcFailure::NonFinite);
                    return None;
                }
                let window = n_raw.round().clamp(0.0, ctx.limits.max_history as f64) as usize;
                if let Some(input) = ctx.current.get(id) {
                    st.observe(input.quality());
                    if let Some(v) = input.value() {
                        return Some(v);
                    }
                }
                // 本周期缺失：回看最近 window 个周期的快照。
                for index in 0..window {
                    if let Some(v) = history_value(id, index, ctx) {
                        return Some(v);
                    }
                }
                st.fail(CalcFailure::MissingInput);
                None
            }
            Builtin::Quality => match ctx.current.get(id) {
                Some(input) => {
                    // 与 `current_value` 同口径：只有取到可用数值才进 `worst()`。
                    if let Some(q) = input.inheritable_quality() {
                        st.observe(q);
                    }
                    Some(severity_code(input.quality()))
                }
                None => {
                    st.fail(CalcFailure::MissingInput);
                    None
                }
            },
            _ => None,
        };
    }

    let mut values: Vec<f64> = Vec::with_capacity(args.len());
    for arg in args {
        values.push(eval_expr(arg, ctx, st)?);
    }
    let v0 = values.first().copied()?;
    let raw = match func {
        Builtin::Abs => v0.abs(),
        Builtin::Ceil => v0.ceil(),
        Builtin::Floor => v0.floor(),
        Builtin::Round => v0.round(),
        Builtin::Sqrt => v0.sqrt(),
        Builtin::Exp => v0.exp(),
        Builtin::Log => v0.ln(),
        Builtin::Log10 => v0.log10(),
        Builtin::Clamp => {
            let lo = values.get(1).copied()?;
            let hi = values.get(2).copied()?;
            // `lo > hi` 是表达式自身的**配置错误**（限幅区间非法）。**不做静默兜底**：
            // 既不像 `f64::clamp` 那样 panic（红线：非测试代码零 panic），
            // 也不交换边界或返回 `hi`（那会静默产出一个反直觉的错误值，
            // 违反「不得静默输出错误数值」），而是判为失败，
            // 交由 `Quality::CalcFailed` 在监控页与诊断页暴露，让配置者自行修正区间。
            if lo > hi {
                st.fail(CalcFailure::OutputOutOfRange);
                return None;
            }
            v0.max(lo).min(hi)
        }
        Builtin::Min => values.iter().copied().fold(f64::INFINITY, f64::min),
        Builtin::Max => values.iter().copied().fold(f64::NEG_INFINITY, f64::max),
        Builtin::Pow => {
            let v1 = values.get(1).copied()?;
            if !v0.is_finite() || !v1.is_finite() || v1.abs() > 1024.0 {
                st.fail(CalcFailure::NonFinite);
                return None;
            }
            v0.powf(v1)
        }
        _ => return None,
    };
    finite_or_fail(raw, st)
}

impl EvalState {
    /// 观察某点位的质量码（用于 `prev` 等不取当前值的场景）。
    fn observe_input_quality(&mut self, id: &str, ctx: &EvalContext<'_>) {
        if let Some(input) = ctx.current.get(id) {
            self.observe(input.quality());
        }
    }
}

/// 按 AST 求值（编译期已校验；运行期只走 AST，不解析原文）。
pub fn evaluate(program: &Program, ctx: &EvalContext<'_>) -> CalcOutcome {
    let mut st = EvalState::new();
    let value = eval_expr(program.root(), ctx, &mut st);
    if let Some(reason) = st.failure {
        return CalcOutcome::failed(reason, st.worst);
    }
    match value {
        Some(v) if v.is_finite() => CalcOutcome::ok(v, st.worst),
        _ => CalcOutcome::failed(CalcFailure::NonFinite, st.worst),
    }
}

/// 按输出类型转换（整数做范围检查，越界 ⇒ [`CalcFailure::OutputOutOfRange`]）。
pub fn convert_output(value: f64, output_type: OutputType) -> Result<f64, CalcFailure> {
    if !value.is_finite() {
        return Err(CalcFailure::NonFinite);
    }
    match output_type {
        OutputType::Float64 => Ok(value),
        OutputType::Float32 => {
            let narrow = value as f32;
            if !narrow.is_finite() {
                return Err(CalcFailure::OutputOutOfRange);
            }
            Ok(f64::from(narrow))
        }
        OutputType::Int64 => {
            let rounded = value.round();
            // 浮点陷阱：`i64::MAX as f64` 会**向上取整**到 `2^63`，故
            // `rounded > i64::MAX as f64` 对恰好等于 `2^63` 的值判为「未越界」而放行。
            // 改用以 `2^63` 为界的**排他**上界；下界 `i64::MIN as f64`（= -2^63，精确可表示）
            // 本身正确，保持 `<` 语义。
            const I64_UPPER_EXCLUSIVE: f64 = 9_223_372_036_854_775_808.0; // 2^63
            if rounded < i64::MIN as f64 || rounded >= I64_UPPER_EXCLUSIVE {
                return Err(CalcFailure::OutputOutOfRange);
            }
            Ok(rounded)
        }
        OutputType::Bool => Ok(if value == 0.0 { 0.0 } else { 1.0 }),
    }
}

// ---- 依赖图 ----

/// 依赖 DAG：拓扑序 + 依赖表 + 链深度。
#[derive(Debug, Clone, Default)]
pub struct DependencyGraph {
    order: Vec<String>,
    deps: BTreeMap<String, Vec<String>>,
    derived_deps: BTreeMap<String, BTreeSet<String>>,
    depth: BTreeMap<String, usize>,
}

impl DependencyGraph {
    /// 拓扑序（依赖先于依赖者）。
    pub fn order(&self) -> &[String] {
        &self.order
    }

    /// 某点位引用的全部点位（含物理点）。
    pub fn deps(&self, point_id: &str) -> &[String] {
        self.deps.get(point_id).map(|v| v.as_slice()).unwrap_or(&[])
    }

    /// 某点位引用的**计算点**（用于 DAG 边）。
    pub fn derived_deps(&self, point_id: &str) -> Option<&BTreeSet<String>> {
        self.derived_deps.get(point_id)
    }

    /// 某点位的依赖链深度（1 = 只依赖物理点）。
    pub fn depth(&self, point_id: &str) -> usize {
        self.depth.get(point_id).copied().unwrap_or(0)
    }
}

/// 构建依赖图：编译全部表达式 → 拓扑排序 → 环检测 → 深度检查。
pub fn build_graph(
    configs: &[DerivedPointConfig],
    limits: &Limits,
    cache: &mut HashMap<String, Program>,
) -> Result<DependencyGraph, FormulaError> {
    if configs.len() > limits.max_points {
        return Err(FormulaError::TooManyPoints {
            count: configs.len(),
            limit: limits.max_points,
        });
    }

    let mut ids: BTreeSet<String> = BTreeSet::new();
    let mut deps: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for cfg in configs {
        cfg.validate()?;
        if !ids.insert(cfg.point_id.clone()) {
            return Err(FormulaError::DuplicatePoint {
                point_id: cfg.point_id.clone(),
            });
        }
        let program = compile_cached(&cfg.expr, limits, cache)
            .map_err(|e| FormulaError::syntax(&cfg.point_id, e))?;
        deps.insert(cfg.point_id.clone(), program.deps().to_vec());
    }

    let mut derived_deps: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for (point_id, refs) in &deps {
        let mut set: BTreeSet<String> = BTreeSet::new();
        for r in refs {
            if r == point_id {
                return Err(FormulaError::Cycle {
                    cycle_path: vec![point_id.clone(), point_id.clone()],
                });
            }
            if ids.contains(r) {
                set.insert(r.clone());
            }
        }
        derived_deps.insert(point_id.clone(), set);
    }

    // Kahn 拓扑排序（BTree 保证确定性）。
    let mut indegree: BTreeMap<String, usize> = BTreeMap::new();
    let mut dependents: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for point_id in &ids {
        indegree.insert(point_id.clone(), 0);
    }
    for (point_id, set) in &derived_deps {
        for dep in set {
            dependents
                .entry(dep.clone())
                .or_default()
                .insert(point_id.clone());
        }
        indegree.insert(point_id.clone(), set.len());
    }

    let mut ready: Vec<String> = indegree
        .iter()
        .filter(|(_, d)| **d == 0)
        .map(|(k, _)| k.clone())
        .collect();
    let mut order: Vec<String> = Vec::with_capacity(ids.len());
    while let Some(node) = ready.first().cloned() {
        ready.remove(0);
        order.push(node.clone());
        if let Some(downstream) = dependents.get(&node) {
            for next in downstream {
                if let Some(d) = indegree.get_mut(next) {
                    *d = d.saturating_sub(1);
                    if *d == 0 {
                        ready.push(next.clone());
                        ready.sort();
                    }
                }
            }
        }
    }

    if order.len() != ids.len() {
        let remaining: BTreeSet<String> = ids
            .difference(&order.iter().cloned().collect::<BTreeSet<_>>())
            .cloned()
            .collect();
        let path = find_cycle(&remaining, &derived_deps)
            .unwrap_or_else(|| remaining.iter().cloned().collect());
        return Err(FormulaError::Cycle { cycle_path: path });
    }

    // 深度（最长依赖链）。
    let mut depth: BTreeMap<String, usize> = BTreeMap::new();
    for point_id in &order {
        let mut d = 1usize;
        if let Some(set) = derived_deps.get(point_id) {
            for dep in set {
                d = d.max(depth.get(dep).copied().unwrap_or(0).saturating_add(1));
            }
        }
        if d > limits.max_dep_depth {
            return Err(FormulaError::DepthLimit {
                point_id: point_id.clone(),
                depth: d,
                limit: limits.max_dep_depth,
            });
        }
        depth.insert(point_id.clone(), d);
    }

    Ok(DependencyGraph {
        order,
        deps,
        derived_deps,
        depth,
    })
}

/// 编译并写入缓存（同一表达式只解析一次）。
fn compile_cached(
    expr: &str,
    limits: &Limits,
    cache: &mut HashMap<String, Program>,
) -> Result<Program, CompileError> {
    if let Some(program) = cache.get(expr) {
        return Ok(program.clone());
    }
    let program = Program::compile_with(expr, limits)?;
    cache.insert(expr.to_string(), program.clone());
    Ok(program)
}

/// 在残余节点中找出一条环路径（迭代 DFS，不递归防爆栈）。
fn find_cycle(
    nodes: &BTreeSet<String>,
    edges: &BTreeMap<String, BTreeSet<String>>,
) -> Option<Vec<String>> {
    let mut visited: BTreeSet<String> = BTreeSet::new();
    for start in nodes {
        if visited.contains(start) {
            continue;
        }
        let mut path: Vec<String> = vec![start.clone()];
        let mut frames: Vec<(String, Vec<String>, usize)> = vec![(
            start.clone(),
            edges
                .get(start)
                .cloned()
                .unwrap_or_default()
                .into_iter()
                .collect(),
            0,
        )];
        while let Some(last) = frames.len().checked_sub(1) {
            let next = {
                let frame = match frames.get_mut(last) {
                    Some(f) => f,
                    None => break,
                };
                if frame.2 < frame.1.len() {
                    let n = frame.1.get(frame.2).cloned().unwrap_or_default();
                    frame.2 += 1;
                    Some(n)
                } else {
                    None
                }
            };
            match next {
                Some(node) => {
                    if let Some(pos) = path.iter().position(|p| *p == node) {
                        let mut cycle: Vec<String> = path[pos..].to_vec();
                        cycle.push(node);
                        return Some(cycle);
                    }
                    if visited.contains(&node) {
                        continue;
                    }
                    let next_deps: Vec<String> = edges
                        .get(&node)
                        .cloned()
                        .unwrap_or_default()
                        .into_iter()
                        .collect();
                    path.push(node.clone());
                    frames.push((node, next_deps, 0));
                }
                None => {
                    if let Some(frame) = frames.get(last) {
                        visited.insert(frame.0.clone());
                    }
                    path.pop();
                    frames.pop();
                }
            }
        }
    }
    None
}

/// 环路径格式化（`R_Cost → R_Energy → R_Cost`）。
pub fn format_cycle_path(path: &[String]) -> String {
    path.join(" → ")
}

// ---- 错误类型 ----

/// 公式域错误（配置 / 语法 / 环 / 深度 / 数量）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FormulaError {
    /// 点位标识重复。
    DuplicatePoint {
        /// 点位标识。
        point_id: String,
    },
    /// 配置非法（空 `point_id` / 空表达式 / 死区非法）。
    Config {
        /// 点位标识。
        point_id: String,
        /// 说明。
        detail: String,
    },
    /// 表达式语法 / 白名单 / 资源超限。
    Syntax {
        /// 点位标识。
        point_id: String,
        /// 错误说明（来自 [`CompileError`]）。
        message: String,
        /// 出错字符位。
        position: usize,
    },
    /// 依赖成环（含环路径）。
    Cycle {
        /// 环路径（首尾同点，如 `R_A → R_B → R_A`）。
        cycle_path: Vec<String>,
    },
    /// 依赖链超深。
    DepthLimit {
        /// 点位标识。
        point_id: String,
        /// 实际深度。
        depth: usize,
        /// 上限。
        limit: usize,
    },
    /// 计算点数量超限。
    TooManyPoints {
        /// 实际数量。
        count: usize,
        /// 上限。
        limit: usize,
    },
}

impl FormulaError {
    /// 构造配置错误。
    pub fn config(point_id: &str, detail: impl Into<String>) -> Self {
        FormulaError::Config {
            point_id: point_id.to_string(),
            detail: detail.into(),
        }
    }

    /// 由编译错误构造语法错误。
    pub fn syntax(point_id: &str, err: CompileError) -> Self {
        FormulaError::Syntax {
            point_id: point_id.to_string(),
            message: err.message,
            position: err.position,
        }
    }

    /// 相关点位标识（环错误取环首节点）。
    pub fn point_id(&self) -> &str {
        match self {
            FormulaError::DuplicatePoint { point_id }
            | FormulaError::Config { point_id, .. }
            | FormulaError::Syntax { point_id, .. }
            | FormulaError::DepthLimit { point_id, .. } => point_id,
            FormulaError::Cycle { cycle_path } => {
                cycle_path.first().map(String::as_str).unwrap_or("")
            }
            FormulaError::TooManyPoints { .. } => "",
        }
    }

    /// 环路径（若有）。
    pub fn cycle_path(&self) -> Option<&[String]> {
        match self {
            FormulaError::Cycle { cycle_path } => Some(cycle_path),
            _ => None,
        }
    }
}

impl fmt::Display for FormulaError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FormulaError::DuplicatePoint { point_id } => {
                write!(f, "FormulaError: 计算点 {point_id} 重复")
            }
            FormulaError::Config { point_id, detail } => {
                write!(f, "FormulaError: 计算点 {point_id} 配置非法：{detail}")
            }
            FormulaError::Syntax {
                point_id,
                message,
                position,
            } => write!(
                f,
                "FormulaError: 计算点 {point_id} 表达式错误（字符位 {position}）：{message}"
            ),
            FormulaError::Cycle { cycle_path } => write!(
                f,
                "FormulaError: 计算点依赖成环：{}",
                format_cycle_path(cycle_path)
            ),
            FormulaError::DepthLimit {
                point_id,
                depth,
                limit,
            } => write!(
                f,
                "FormulaError: 计算点 {point_id} 依赖链深度 {depth} 超过上限 {limit}"
            ),
            FormulaError::TooManyPoints { count, limit } => {
                write!(f, "FormulaError: 计算点数量 {count} 超过上限 {limit}")
            }
        }
    }
}

impl std::error::Error for FormulaError {}

impl From<FormulaError> for DaemonError {
    fn from(err: FormulaError) -> Self {
        DaemonError::ConfigError(err.to_string())
    }
}

// ---- 保存期校验 ----

/// 保存期校验错误（含引用存在性）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ValidationError {
    /// 点位标识重复。
    DuplicatePoint {
        /// 点位标识。
        point_id: String,
    },
    /// 配置非法。
    Config {
        /// 点位标识。
        point_id: String,
        /// 说明。
        detail: String,
    },
    /// 表达式语法 / 白名单 / 资源超限。
    Syntax {
        /// 点位标识。
        point_id: String,
        /// 说明。
        message: String,
        /// 字符位。
        position: usize,
    },
    /// 引用了不存在的点位。
    UnknownReference {
        /// 点位标识。
        point_id: String,
        /// 被引用但不存在的点位。
        reference: String,
    },
    /// 依赖成环。
    Cycle {
        /// 环路径。
        cycle_path: Vec<String>,
    },
    /// 依赖链超深。
    DepthLimit {
        /// 点位标识。
        point_id: String,
        /// 深度。
        depth: usize,
        /// 上限。
        limit: usize,
    },
    /// 数量超限。
    TooManyPoints {
        /// 数量。
        count: usize,
        /// 上限。
        limit: usize,
    },
}

impl ValidationError {
    /// 相关点位标识。
    pub fn point_id(&self) -> &str {
        match self {
            ValidationError::DuplicatePoint { point_id }
            | ValidationError::Config { point_id, .. }
            | ValidationError::Syntax { point_id, .. }
            | ValidationError::UnknownReference { point_id, .. }
            | ValidationError::DepthLimit { point_id, .. } => point_id,
            ValidationError::Cycle { cycle_path } => {
                cycle_path.first().map(String::as_str).unwrap_or("")
            }
            ValidationError::TooManyPoints { .. } => "",
        }
    }

    /// 错误说明（可直接展示）。
    pub fn message(&self) -> String {
        match self {
            ValidationError::DuplicatePoint { point_id } => {
                format!("计算点 {point_id} 重复")
            }
            ValidationError::Config { point_id, detail } => {
                format!("计算点 {point_id} 配置非法：{detail}")
            }
            ValidationError::Syntax {
                point_id,
                message,
                position,
            } => format!("计算点 {point_id} 表达式错误（字符位 {position}）：{message}"),
            ValidationError::UnknownReference {
                point_id,
                reference,
            } => format!("计算点 {point_id} 引用了不存在的点位 [{reference}]"),
            ValidationError::Cycle { cycle_path } => {
                format!("计算点依赖成环：{}", format_cycle_path(cycle_path))
            }
            ValidationError::DepthLimit {
                point_id,
                depth,
                limit,
            } => format!("计算点 {point_id} 依赖链深度 {depth} 超过上限 {limit}"),
            ValidationError::TooManyPoints { count, limit } => {
                format!("计算点数量 {count} 超过上限 {limit}")
            }
        }
    }

    /// 环路径（若有）。
    pub fn cycle_path(&self) -> Option<&[String]> {
        match self {
            ValidationError::Cycle { cycle_path } => Some(cycle_path),
            _ => None,
        }
    }
}

impl fmt::Display for ValidationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message())
    }
}

/// 校验报告（`POST /api/points/formula/validate` 的库层产物）。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ValidationReport {
    /// 是否全部通过。
    pub ok: bool,
    /// 错误列表（结构化，可定位到点位与字符位）。
    pub errors: Vec<ValidationError>,
    /// 通过时的求值顺序（拓扑序）。
    pub order: Option<Vec<String>>,
}

/// 保存期校验：语法 + 白名单 + 引用存在性 + 环检测 + 资源上限（默认上限）。
pub fn validate(
    configs: &[DerivedPointConfig],
    known_points: &BTreeSet<String>,
) -> ValidationReport {
    validate_with(configs, known_points, &Limits::default())
}

/// 保存期校验（指定资源上限）。
pub fn validate_with(
    configs: &[DerivedPointConfig],
    known_points: &BTreeSet<String>,
    limits: &Limits,
) -> ValidationReport {
    let mut report = ValidationReport::default();
    let mut cache: HashMap<String, Program> = HashMap::new();

    let mut ids: BTreeSet<String> = BTreeSet::new();
    for cfg in configs {
        if let Err(err) = cfg.validate() {
            report.errors.push(ValidationError::Config {
                point_id: cfg.point_id.clone(),
                detail: match &err {
                    FormulaError::Config { detail, .. } => detail.clone(),
                    other => other.to_string(),
                },
            });
            continue;
        }
        if !ids.insert(cfg.point_id.clone()) {
            report.errors.push(ValidationError::DuplicatePoint {
                point_id: cfg.point_id.clone(),
            });
        }
    }

    match build_graph(configs, limits, &mut cache) {
        Ok(graph) => {
            for (point_id, refs) in graph.deps.iter() {
                for r in refs {
                    if !ids.contains(r) && !known_points.contains(r) {
                        report.errors.push(ValidationError::UnknownReference {
                            point_id: point_id.clone(),
                            reference: r.clone(),
                        });
                    }
                }
            }
            report.order = Some(graph.order().to_vec());
        }
        Err(err) => report.errors.push(convert_formula_error(err)),
    }

    report.ok = report.errors.is_empty();
    if !report.ok {
        report.order = None;
    }
    report
}

fn convert_formula_error(err: FormulaError) -> ValidationError {
    match err {
        FormulaError::DuplicatePoint { point_id } => ValidationError::DuplicatePoint { point_id },
        FormulaError::Config { point_id, detail } => ValidationError::Config { point_id, detail },
        FormulaError::Syntax {
            point_id,
            message,
            position,
        } => ValidationError::Syntax {
            point_id,
            message,
            position,
        },
        FormulaError::Cycle { cycle_path } => ValidationError::Cycle { cycle_path },
        FormulaError::DepthLimit {
            point_id,
            depth,
            limit,
        } => ValidationError::DepthLimit {
            point_id,
            depth,
            limit,
        },
        FormulaError::TooManyPoints { count, limit } => {
            ValidationError::TooManyPoints { count, limit }
        }
    }
}

// ---- 运行期产物 ----

/// 单个计算点本周期的求值产物。
#[derive(Debug, Clone, PartialEq)]
pub struct DerivedResult {
    /// 点位标识。
    pub point_id: String,
    /// 最终值（失败策略生效后；`None` = 无值）。
    pub value: Option<f64>,
    /// 质量码（失败时为 BAD，不伪装 GOOD；与 `calc_failed` 的合并口径由 task 53 定义）。
    pub quality: Quality,
    /// 是否失败（**`hold_last` 保留值也必须为 true**）。
    pub failed: bool,
    /// 失败原因。
    pub reason: Option<CalcFailure>,
    /// 是否对外产出（`skip` 策略 / 死区过滤 / 未触发 ⇒ `false`）。
    pub emitted: bool,
    /// 采集时间戳（Unix 纳秒）。
    pub ts_ns: i64,
}

/// 历史记录（**追加写**；公式变更不触碰既有记录 = 历史不重算）。
#[derive(Debug, Clone, PartialEq)]
pub struct HistoryRecord {
    /// 点位标识。
    pub point_id: String,
    /// 采集时间戳（Unix 纳秒；JSON 路径必须编码为字符串）。
    pub ts_ns: i64,
    /// 值。
    pub value: Option<f64>,
    /// 质量码。
    pub quality: Quality,
    /// 是否失败。
    pub failed: bool,
}

/// 公式变更审计条目（含改前 / 改后全文）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FormulaAuditEntry {
    /// 点位标识。
    pub point_id: String,
    /// 改前表达式（`None` = 新增）。
    pub before: Option<String>,
    /// 改后表达式（`None` = 删除）。
    pub after: Option<String>,
    /// 变更时刻（Unix 纳秒；JSON 路径必须编码为字符串）。
    pub ts_ns: i64,
}

/// 失败统计（供监控页 / 诊断页）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FailureStat {
    /// 累计失败次数。
    pub count: u64,
    /// 最近一次失败原因。
    pub last_reason: CalcFailure,
    /// 最近一次失败时刻（Unix 纳秒）。
    pub last_ts_ns: i64,
}

/// 试算请求（`POST /api/points/formula/dry-run` 的库层入参）。
#[derive(Debug, Clone, Default)]
pub struct DryRunRequest {
    /// 目标计算点（表达式覆盖时可为空）。
    pub point_id: String,
    /// 表达式覆盖（试算未保存的草稿）。
    pub expr: Option<String>,
    /// 输出类型覆盖（不填则用点位配置）。
    pub output_type: Option<OutputType>,
    /// 输入点位值。
    pub inputs: PointValues,
    /// 历史快照（`prev` / `delta` / `rate` / `hold` 用）。
    pub history: PointHistory,
    /// 周期时长（秒）。
    pub dt_s: f64,
}

/// 试算过程中的依赖中间值。
#[derive(Debug, Clone, PartialEq)]
pub struct Intermediate {
    /// 点位标识。
    pub point_id: String,
    /// 值。
    pub value: Option<f64>,
    /// 质量码。
    pub quality: Quality,
}

/// 试算结果（权威试算：结果、各依赖中间值、质量码）。
#[derive(Debug, Clone, PartialEq)]
pub struct DryRunOutput {
    /// 是否试算成功。
    pub ok: bool,
    /// 求值结果。
    pub outcome: CalcOutcome,
    /// 按输出类型转换后的落库值。
    pub value: Option<f64>,
    /// 依赖中间值（含被依赖的计算点）。
    pub intermediates: Vec<Intermediate>,
    /// 求值顺序。
    pub order: Vec<String>,
    /// 错误说明（`ok == false` 时有值）。
    pub error: Option<String>,
}

impl DryRunOutput {
    fn failure(error: impl Into<String>) -> Self {
        Self {
            ok: false,
            outcome: CalcOutcome::failed(CalcFailure::ResourceLimit, Quality::CalcFailed),
            value: None,
            intermediates: Vec::new(),
            order: Vec::new(),
            error: Some(error.into()),
        }
    }
}

/// 试算一条表达式（不落库；用于前端草稿与后端权威试算）。
pub fn dry_run_expr(
    expr: &str,
    output_type: OutputType,
    inputs: &PointValues,
    history: &PointHistory,
    dt_s: f64,
) -> DryRunOutput {
    let limits = Limits::default();
    let program = match Program::compile_with(expr, &limits) {
        Ok(p) => p,
        Err(e) => return DryRunOutput::failure(e.to_string()),
    };
    let ctx = EvalContext::new(inputs, history, dt_s, &limits);
    let outcome = evaluate(&program, &ctx);
    let mut intermediates: Vec<Intermediate> = Vec::new();
    for dep in program.deps() {
        let (value, quality) = match inputs.get(dep) {
            Some(input) => (input.value(), input.quality()),
            None => (None, Quality::Bad),
        };
        intermediates.push(Intermediate {
            point_id: dep.clone(),
            value,
            quality,
        });
    }
    let value = outcome
        .value
        .and_then(|v| convert_output(v, output_type).ok());
    let failed = outcome.failed || (outcome.value.is_some() && value.is_none());
    // 失败原因：优先继承求值器给出的原因；仅「求值有值但输出越界被丢弃」时补
    // `OutputOutOfRange`。（`failed == true` ⇒ 此处必为 `Some`。）
    let reason = outcome
        .reason
        .or(failed.then_some(CalcFailure::OutputOutOfRange));
    // 契约（见模块文档 / `CalcOutcome::quality`）：`failed == true` ⇒ 质量码取
    // `Quality::worst(继承到的最差, Quality::CalcFailed)`，与 [`FormulaEngine::eval_cycle`]
    // 口径一致；否则监控页草稿试算会把「算不出来」误报为 GOOD。
    let quality = if failed {
        Quality::worst(outcome.quality, Quality::CalcFailed)
    } else {
        outcome.quality
    };
    // 契约（见 [`DryRunOutput::error`]）：`ok == false` 时 `error` 必有值——编译期失败已由
    // [`DryRunOutput::failure`] 填充；运行期失败（除零 / 缺输入等）在此按 `reason` 填充，
    // 否则前端按文档读取 `error` 会渲染空白消息。
    let error = if failed {
        Some(reason.map_or_else(|| CalcFailure::MissingInput.to_string(), |r| r.to_string()))
    } else {
        None
    };
    DryRunOutput {
        ok: !failed,
        outcome: CalcOutcome {
            value,
            failed,
            reason,
            quality,
        },
        value,
        intermediates,
        order: program.deps().to_vec(),
        error,
    }
}

// ---- 引擎 ----

/// 公式引擎：编译缓存 + 依赖图 + 周期求值 + 历史与审计。
#[derive(Debug, Clone)]
pub struct FormulaEngine {
    limits: Limits,
    configs: HashMap<String, DerivedPointConfig>,
    programs: HashMap<String, Program>,
    cache: HashMap<String, Program>,
    deps: HashMap<String, Vec<String>>,
    order: Vec<String>,
    history: PointHistory,
    last_good: HashMap<String, f64>,
    last_emitted: HashMap<String, (f64, Quality)>,
    last_deps: HashMap<String, Vec<(String, Option<f64>)>>,
    failures: HashMap<String, FailureStat>,
    audit: Vec<FormulaAuditEntry>,
    log: Vec<HistoryRecord>,
    last_ts_ns: i64,
    compile_count: usize,
}

impl FormulaEngine {
    /// 按默认资源上限构建（保存期即校验：语法 / 白名单 / 环 / 深度）。
    pub fn new(configs: Vec<DerivedPointConfig>) -> Result<Self, FormulaError> {
        Self::new_with_limits(configs, Limits::default())
    }

    /// 按指定资源上限构建。
    pub fn new_with_limits(
        configs: Vec<DerivedPointConfig>,
        limits: Limits,
    ) -> Result<Self, FormulaError> {
        let mut engine = Self::empty(limits);
        engine.reconfigure(configs, 0)?;
        Ok(engine)
    }

    fn empty(limits: Limits) -> Self {
        Self {
            limits,
            configs: HashMap::new(),
            programs: HashMap::new(),
            cache: HashMap::new(),
            deps: HashMap::new(),
            order: Vec::new(),
            history: HashMap::new(),
            last_good: HashMap::new(),
            last_emitted: HashMap::new(),
            last_deps: HashMap::new(),
            failures: HashMap::new(),
            audit: Vec::new(),
            log: Vec::new(),
            last_ts_ns: 0,
            compile_count: 0,
        }
    }

    /// 重新装载配置（**历史数据不重算**：只替换编译产物与依赖图）。
    ///
    /// 校验失败时保持原配置不变（保存期拒绝即不改运行时状态）。
    pub fn reconfigure(
        &mut self,
        configs: Vec<DerivedPointConfig>,
        ts_ns: i64,
    ) -> Result<(), FormulaError> {
        let mut cache = self.cache.clone();
        let graph = build_graph(&configs, &self.limits, &mut cache)?;

        let mut programs: HashMap<String, Program> = HashMap::new();
        for cfg in &configs {
            let program = cache
                .get(&cfg.expr)
                .cloned()
                .ok_or_else(|| FormulaError::config(&cfg.point_id, "编译缓存缺失（内部错误）"))?;
            programs.insert(cfg.point_id.clone(), program);
        }
        let compiled = cache.len().saturating_sub(self.cache.len());
        self.compile_count = self.compile_count.saturating_add(compiled);

        // 审计：改前 / 改后全文。
        let mut entries: Vec<FormulaAuditEntry> = Vec::new();
        let mut new_map: HashMap<String, DerivedPointConfig> = HashMap::new();
        for cfg in &configs {
            new_map.insert(cfg.point_id.clone(), cfg.clone());
            let before = self.configs.get(&cfg.point_id).map(|c| c.expr.clone());
            if before.as_deref() != Some(cfg.expr.as_str()) {
                entries.push(FormulaAuditEntry {
                    point_id: cfg.point_id.clone(),
                    before,
                    after: Some(cfg.expr.clone()),
                    ts_ns,
                });
            }
        }
        for point_id in self.configs.keys() {
            if !new_map.contains_key(point_id) {
                entries.push(FormulaAuditEntry {
                    point_id: point_id.clone(),
                    before: self.configs.get(point_id).map(|c| c.expr.clone()),
                    after: None,
                    ts_ns,
                });
            }
        }
        entries.sort_by(|a, b| a.point_id.cmp(&b.point_id));

        let mut deps: HashMap<String, Vec<String>> = HashMap::new();
        for point_id in &graph.order {
            deps.insert(point_id.clone(), graph.deps(point_id).to_vec());
        }

        self.configs = new_map;
        self.programs = programs;
        self.cache = cache;
        self.deps = deps;
        self.order = graph.order().to_vec();
        self.audit.extend(entries.iter().cloned());

        // 公式变更必须让下一次求值真正发生：清理受影响点位的「依赖变化」基准与
        // 死区基准（否则 OnChange 模式下输入不变 ⇒ 新公式不会被求值；
        // 与 task 15 的 `DataProcessor::reset` 同口径）。删除的点位同时清掉保持值。
        for entry in &entries {
            self.last_deps.remove(&entry.point_id);
            self.last_emitted.remove(&entry.point_id);
            if entry.after.is_none() {
                self.last_good.remove(&entry.point_id);
                self.history.remove(&entry.point_id);
            }
        }
        Ok(())
    }

    /// 资源上限。
    pub fn limits(&self) -> &Limits {
        &self.limits
    }

    /// 拓扑序（依赖先算）。
    pub fn order(&self) -> &[String] {
        &self.order
    }

    /// 某计算点的依赖列表。
    pub fn deps(&self, point_id: &str) -> &[String] {
        self.deps.get(point_id).map(|v| v.as_slice()).unwrap_or(&[])
    }

    /// 编译产物（调试 / 诊断用）。
    pub fn program(&self, point_id: &str) -> Option<&Program> {
        self.programs.get(point_id)
    }

    /// **实际解析次数**（AST 缓存证明：N 个计算点编译一次后，多周期求值不再解析）。
    pub fn compile_count(&self) -> usize {
        self.compile_count
    }

    /// 失败统计。
    pub fn failure_stat(&self, point_id: &str) -> Option<&FailureStat> {
        self.failures.get(point_id)
    }

    /// 公式变更审计日志（含改前 / 改后全文）。
    pub fn audit_log(&self) -> &[FormulaAuditEntry] {
        &self.audit
    }

    /// 历史记录（追加写）。
    pub fn history_log(&self) -> &[HistoryRecord] {
        &self.log
    }

    /// 历史查询（按点位与时间区间；**不受公式变更影响**）。
    pub fn query_history(
        &self,
        point_id: &str,
        from_ts_ns: i64,
        to_ts_ns: i64,
    ) -> Vec<&HistoryRecord> {
        self.log
            .iter()
            .filter(|r| r.point_id == point_id && r.ts_ns >= from_ts_ns && r.ts_ns <= to_ts_ns)
            .collect()
    }

    /// 上周期快照（`prev` / `delta` / `rate` / `hold` 的数据源）。
    pub fn history(&self) -> &PointHistory {
        &self.history
    }

    /// 执行一个采集周期的求值（周期时长由相邻时间戳推算，首周期按 1s）。
    ///
    /// **周期屏障**：入参 `inputs` 必须是本周期全部物理点经 task 15 换算后的工程单位值；
    /// 本方法内部按拓扑序求值，历史快照在整周期结束后统一下移。
    pub fn eval_cycle(&mut self, inputs: &PointValues, ts_ns: i64) -> Vec<DerivedResult> {
        let dt_s = if self.last_ts_ns > 0 && ts_ns > self.last_ts_ns {
            (ts_ns.saturating_sub(self.last_ts_ns)) as f64 / 1_000_000_000.0
        } else {
            1.0
        };
        self.eval_cycle_with_dt(inputs, ts_ns, dt_s)
    }

    /// 执行一个采集周期的求值（显式指定周期时长秒，`rate` 用）。
    pub fn eval_cycle_with_dt(
        &mut self,
        inputs: &PointValues,
        ts_ns: i64,
        dt_s: f64,
    ) -> Vec<DerivedResult> {
        let dt = if dt_s.is_finite() && dt_s > 0.0 {
            dt_s
        } else {
            0.0
        };
        let mut working: PointValues = inputs.clone();
        let mut results: Vec<DerivedResult> = Vec::with_capacity(self.order.len());
        let order = self.order.clone();

        for point_id in order {
            let cfg = match self.configs.get(&point_id).cloned() {
                Some(c) => c,
                None => continue,
            };
            let program = match self.programs.get(&point_id).cloned() {
                Some(p) => p,
                None => continue,
            };
            let refs = self.deps.get(&point_id).cloned().unwrap_or_default();

            // 1) 触发判定：OnChange 模式下依赖无变化则本周期不重算（保留上次值供下游使用）。
            if matches!(cfg.eval_mode, EvalMode::OnChange)
                && !self.deps_changed(&point_id, &refs, &working)
            {
                let held = self.last_good.get(&point_id).copied();
                let input = match held {
                    Some(v) => InputValue::Numeric(v, Quality::Good),
                    None => InputValue::Missing(Quality::Bad),
                };
                working.insert(point_id.clone(), input);
                results.push(DerivedResult {
                    point_id,
                    value: None,
                    quality: Quality::Bad,
                    failed: false,
                    reason: None,
                    emitted: false,
                    ts_ns,
                });
                continue;
            }
            let snapshot = self.snapshot_deps(&refs, &working);

            // 2) 求值（AST，无解析）。
            let outcome = {
                let ctx = EvalContext::new(&working, &self.history, dt, &self.limits);
                evaluate(&program, &ctx)
            };
            let mut outcome = outcome;

            // 3) 输出类型转换（含整数范围检查）。
            let mut final_value: Option<f64> = None;
            if let Some(raw) = outcome.value {
                match convert_output(raw, cfg.output_type) {
                    Ok(v) => {
                        final_value = Some(v);
                        self.last_good.insert(point_id.clone(), v);
                    }
                    Err(reason) => {
                        outcome.failed = true;
                        outcome.reason = Some(reason);
                        final_value = None;
                    }
                }
            } else if !outcome.failed {
                outcome.failed = true;
                outcome.reason = Some(CalcFailure::MissingInput);
            }

            // 4) 失败策略。**`hold_last` 保留值，但 `failed` 保持 true，禁止伪装成功**。
            let mut emitted = true;
            if outcome.failed {
                let reason = outcome.reason.unwrap_or(CalcFailure::MissingInput);
                self.record_failure(&point_id, reason, ts_ns);
                match cfg.on_failure {
                    OnFailure::HoldLast => final_value = self.last_good.get(&point_id).copied(),
                    OnFailure::Null => final_value = None,
                    OnFailure::Skip => {
                        final_value = None;
                        emitted = false;
                    }
                }
                outcome.quality = Quality::worst(outcome.quality, Quality::CalcFailed);
            }

            // 5) 输出死区（以「上次对外产出值」为基准；质量码变化强制输出）。
            if let (Some(v), Some((last_v, last_q))) =
                (final_value, self.last_emitted.get(&point_id))
            {
                if cfg.deadband > 0.0
                    && (v - *last_v).abs() < cfg.deadband
                    && outcome.quality == *last_q
                {
                    emitted = false;
                }
            }
            if emitted {
                if let Some(v) = final_value {
                    self.last_emitted
                        .insert(point_id.clone(), (v, outcome.quality));
                }
            }

            // 6) 写入本周期工作集，供后续（下游）计算点使用。
            let downstream = match final_value {
                Some(v) => InputValue::Numeric(v, outcome.quality),
                None => InputValue::Missing(outcome.quality),
            };
            working.insert(point_id.clone(), downstream);
            self.last_deps.insert(point_id.clone(), snapshot);

            results.push(DerivedResult {
                point_id,
                value: final_value,
                quality: outcome.quality,
                failed: outcome.failed,
                reason: outcome.reason,
                emitted,
                ts_ns,
            });
        }

        // 周期屏障：整周期求值结束后统一下移历史快照（保证跨周期引用只读上周期）。
        for (point_id, input) in &working {
            let series = self.history.entry(point_id.clone()).or_default();
            series.push_front(input.value());
            if series.len() > self.limits.max_history {
                series.truncate(self.limits.max_history);
            }
        }
        self.last_ts_ns = ts_ns;

        // 历史日志追加写（**公式变更不触碰既有记录**）。
        for result in &results {
            if result.emitted {
                self.log.push(HistoryRecord {
                    point_id: result.point_id.clone(),
                    ts_ns: result.ts_ns,
                    value: result.value,
                    quality: result.quality,
                    failed: result.failed,
                });
            }
        }
        results
    }

    /// 某点位的上次有效值（`hold_last` 的数据源）。
    pub fn last_value(&self, point_id: &str) -> Option<f64> {
        self.last_good.get(point_id).copied()
    }

    /// 权威试算：给定输入 → 返回结果、各依赖中间值与质量码（含表达式草稿覆盖）。
    pub fn dry_run(&self, req: DryRunRequest) -> DryRunOutput {
        let expr_src = match req.expr.clone() {
            Some(expr) => expr,
            None => match self.configs.get(&req.point_id) {
                Some(cfg) => cfg.expr.clone(),
                None => {
                    return DryRunOutput::failure(format!(
                        "未知计算点 {}（未配置表达式）",
                        req.point_id
                    ));
                }
            },
        };
        let output_type = req
            .output_type
            .or_else(|| self.configs.get(&req.point_id).map(|c| c.output_type))
            .unwrap_or(OutputType::Float64);
        let program = match Program::compile_with(&expr_src, &self.limits) {
            Ok(p) => p,
            Err(e) => return DryRunOutput::failure(e.to_string()),
        };

        // 传递闭包：表达式引用的计算点（含间接依赖）先按全局拓扑序算出。
        let mut closure: BTreeSet<String> = BTreeSet::new();
        let mut stack: Vec<String> = program.deps().to_vec();
        while let Some(id) = stack.pop() {
            if !self.configs.contains_key(&id) {
                continue;
            }
            if closure.insert(id.clone()) {
                for dep in self.deps(&id) {
                    stack.push(dep.clone());
                }
            }
        }

        let mut working: PointValues = req.inputs.clone();
        let mut order: Vec<String> = Vec::new();
        let mut intermediates: Vec<Intermediate> = Vec::new();
        for point_id in self.order.clone() {
            if !closure.contains(&point_id) {
                continue;
            }
            let program = match self.programs.get(&point_id).cloned() {
                Some(p) => p,
                None => continue,
            };
            let outcome = {
                let ctx = EvalContext::new(&working, &req.history, req.dt_s, &self.limits);
                evaluate(&program, &ctx)
            };
            let input = match outcome.value {
                Some(v) => InputValue::Numeric(v, outcome.quality),
                None => InputValue::Missing(outcome.quality),
            };
            working.insert(point_id.clone(), input);
            intermediates.push(Intermediate {
                point_id: point_id.clone(),
                value: outcome.value,
                quality: outcome.quality,
            });
            order.push(point_id);
        }

        // 目标表达式直接引用的物理点也作为中间值回显。
        for dep in program.deps() {
            if closure.contains(dep) {
                continue;
            }
            let (value, quality) = match working.get(dep) {
                Some(input) => (input.value(), input.quality()),
                None => (None, Quality::Bad),
            };
            intermediates.push(Intermediate {
                point_id: dep.clone(),
                value,
                quality,
            });
        }

        let outcome = {
            let ctx = EvalContext::new(&working, &req.history, req.dt_s, &self.limits);
            evaluate(&program, &ctx)
        };
        let value = outcome
            .value
            .and_then(|v| convert_output(v, output_type).ok());
        let failed = outcome.failed || (outcome.value.is_some() && value.is_none());
        order.push(req.point_id.clone());

        DryRunOutput {
            ok: !failed,
            outcome: CalcOutcome {
                value,
                failed,
                reason: outcome
                    .reason
                    .or_else(|| failed.then_some(CalcFailure::OutputOutOfRange)),
                quality: outcome.quality,
            },
            value,
            intermediates,
            order,
            error: None,
        }
    }

    fn deps_changed(&self, point_id: &str, refs: &[String], working: &PointValues) -> bool {
        match self.last_deps.get(point_id) {
            None => true,
            Some(prev) => refs.iter().any(|r| {
                let current = working.get(r).and_then(|v| v.value());
                match prev.iter().find(|(k, _)| k == r) {
                    Some((_, old)) => *old != current,
                    None => true,
                }
            }),
        }
    }

    fn snapshot_deps(&self, refs: &[String], working: &PointValues) -> Vec<(String, Option<f64>)> {
        refs.iter()
            .map(|r| (r.clone(), working.get(r).and_then(|v| v.value())))
            .collect()
    }

    fn record_failure(&mut self, point_id: &str, reason: CalcFailure, ts_ns: i64) {
        let count = self
            .failures
            .get(point_id)
            .map(|s| s.count.saturating_add(1))
            .unwrap_or(1);
        self.failures.insert(
            point_id.to_string(),
            FailureStat {
                count,
                last_reason: reason,
                last_ts_ns: ts_ns,
            },
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(point_id: &str, expr: &str) -> DerivedPointConfig {
        DerivedPointConfig::new(point_id, expr)
    }

    fn values(pairs: &[(&str, f64)]) -> PointValues {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), InputValue::good(*v)))
            .collect()
    }

    fn history_of(pairs: &[(&str, &[Option<f64>])]) -> PointHistory {
        pairs
            .iter()
            .map(|(k, series)| {
                (
                    (*k).to_string(),
                    series.iter().copied().collect::<VecDeque<Option<f64>>>(),
                )
            })
            .collect()
    }

    fn get<'a>(results: &'a [DerivedResult], point_id: &str) -> &'a DerivedResult {
        results
            .iter()
            .find(|r| r.point_id == point_id)
            .expect("test input: derived point must exist in result set")
    }

    /// QA Happy: `[P_Inj] * [T_Barrel1] / 1000` → 与手算一致；依赖中间值可回显。
    #[test]
    fn happy_arithmetic_matches_manual_calc() {
        let mut engine =
            FormulaEngine::new(vec![cfg("R_Power", "[P_Inj] * [T_Barrel1] / 1000")]).expect("ok");
        let inputs = values(&[("P_Inj", 250.0), ("T_Barrel1", 36.0)]);
        let results = engine.eval_cycle(&inputs, 1_000_000_000);
        let result = get(&results, "R_Power");
        assert!(!result.failed, "reason: {:?}", result.reason);
        assert_eq!(result.value, Some(9.0), "250 * 36 / 1000 = 9");
        assert_eq!(result.quality, Quality::Good);
        assert!(result.emitted);

        let dry = dry_run_expr(
            "[P_Inj] * [T_Barrel1] / 1000",
            OutputType::Float64,
            &inputs,
            &PointHistory::new(),
            1.0,
        );
        assert!(dry.ok);
        assert_eq!(dry.value, Some(9.0));
        assert_eq!(dry.intermediates.len(), 2);
    }

    /// QA Happy（单位时序）: 公式拿到的是**换算后**的工程单位值，而非原始寄存器值。
    #[test]
    fn unit_conversion_applied_before_formula() {
        // task 15：`raw=250000 Pa` × scale 0.001 + 0 → 250 kPa，公式必须看到 250 而非 250000。
        let converted = 250_000_f64 * 0.001_f64;
        let mut engine =
            FormulaEngine::new(vec![cfg("R_Load", "[Tank_Pressure] * 2")]).expect("ok");
        let results = engine.eval_cycle(&values(&[("Tank_Pressure", converted)]), 1_000_000_000);
        assert_eq!(get(&results, "R_Load").value, Some(500.0));
    }

    /// QA Error: 语法错误 / 未白名单 / 赋值 / 循环 / 字符串 → 解析期即拒绝，并回显允许函数。
    #[test]
    fn syntax_and_whitelist_rejected_with_allowed_list() {
        let cases: Vec<(&str, &str)> = vec![
            ("[A] +", "期望"),
            ("[A] = 1", "赋值"),
            ("exec([A])", "关键字"),
            ("foo([A])", "允许的函数"),
            ("if [A] > 0", "if(cond, a, b)"),
            ("sqrt", "允许的函数"),
            ("\"abc\" + 1", "字符串"),
            ("while([A])", "关键字"),
            ("[A] ; [B]", "语句"),
            ("[A] { }", "代码块"),
            ("[]", "空点位引用"),
            ("[A", "未闭合"),
        ];
        for (src, needle) in cases {
            let err = Program::compile(src)
                .expect_err("must be rejected at parse time")
                .to_string();
            assert!(
                err.contains(needle),
                "表达式 {src:?} 的错误应包含 {needle:?}，实际：{err}"
            );
        }
        // 未知函数的错误信息必须列出全部白名单，便于用户自查。
        let err = Program::compile("foo([A])").expect_err("must fail").message;
        for doc in function_whitelist() {
            assert!(err.contains(doc.name), "errors must list {}", doc.name);
        }
        // 正确表达的 Pos 也应被接受并定位字符位。
        let err = Program::compile("[A] = 1").expect_err("must fail");
        assert_eq!(err.position, 4, "赋值号字符位应可定位");
    }

    /// QA: 状态函数的参数规则（必须是点位引用）与 prev/delta/rate/hold/quality 语义。
    #[test]
    fn state_function_rules_and_semantics() {
        let err = Program::compile("prev([A] + 1)")
            .expect_err("must fail")
            .to_string();
        assert!(err.contains("点位引用"), "actual: {err}");

        let mut engine = FormulaEngine::new(vec![
            cfg("R_Prev", "prev([X])"),
            cfg("R_Delta", "delta([X])"),
            cfg("R_Rate", "rate([X])"),
            cfg("R_Hold", "hold([X], 3)"),
            cfg("R_Quality", "quality([X])"),
        ])
        .expect("ok");

        // 周期 1：无上周期值 → prev/delta/rate 失败；hold 与 quality 仍可用。
        let first = engine.eval_cycle(&values(&[("X", 5.0)]), 1_000_000_000);
        assert_eq!(
            get(&first, "R_Prev").reason,
            Some(CalcFailure::MissingInput)
        );
        assert_eq!(
            get(&first, "R_Delta").reason,
            Some(CalcFailure::MissingInput)
        );
        assert_eq!(get(&first, "R_Hold").value, Some(5.0));
        assert_eq!(get(&first, "R_Quality").value, Some(0.0));

        // 周期 2（dt = 1s）：prev=5, delta=3, rate=3。
        let second = engine.eval_cycle(&values(&[("X", 8.0)]), 2_000_000_000);
        assert_eq!(get(&second, "R_Prev").value, Some(5.0));
        assert_eq!(get(&second, "R_Delta").value, Some(3.0));
        assert_eq!(get(&second, "R_Rate").value, Some(3.0));
        assert_eq!(get(&second, "R_Hold").value, Some(8.0));

        // 周期 3：X 本周期缺失 → hold 回看最近 3 个周期取 8.0，其它状态函数失败。
        let third = engine.eval_cycle(&PointValues::new(), 3_000_000_000);
        assert_eq!(get(&third, "R_Hold").value, Some(8.0));
        assert_eq!(
            get(&third, "R_Delta").reason,
            Some(CalcFailure::MissingInput)
        );
    }

    /// QA Error: 环检测必须给出环路径（含自环、二元环、多级环 A→B→C→A）。
    #[test]
    fn cycle_detection_reports_two_and_three_node_paths() {
        let self_cycle = FormulaEngine::new(vec![cfg("R_Self", "[R_Self] + 1")])
            .expect_err("self reference must be rejected");
        assert_eq!(
            self_cycle.cycle_path(),
            Some(["R_Self".to_string(), "R_Self".to_string()].as_slice())
        );

        let two = FormulaEngine::new(vec![cfg("R_A", "[R_B] + 1"), cfg("R_B", "[R_A] + 1")])
            .expect_err("two-node cycle must be rejected");
        let path = two.cycle_path().expect("cycle path required");
        assert_eq!(format_cycle_path(path), "R_A → R_B → R_A");

        let three = FormulaEngine::new(vec![
            cfg("R_Cost", "[R_Energy] + 1"),
            cfg("R_Energy", "[R_Power] + 1"),
            cfg("R_Power", "[R_Cost] + 1"),
        ])
        .expect_err("three-node cycle must be rejected");
        let rendered = three.to_string();
        assert!(
            rendered.contains("R_Cost → R_Energy → R_Power → R_Cost")
                || rendered.contains("R_Energy → R_Power → R_Cost → R_Energy"),
            "多级环必须给出完整路径，实际：{rendered}"
        );

        // 保存期校验同样返回环路径。
        let report = validate(
            &[cfg("R_A", "[R_B] + 1"), cfg("R_B", "[R_A] + 1")],
            &BTreeSet::new(),
        );
        assert!(!report.ok);
        let cycle = report
            .errors
            .iter()
            .find_map(|e| e.cycle_path())
            .expect("cycle path required in validation report");
        assert!(cycle.len() >= 3, "cycle path: {cycle:?}");
    }

    /// QA Happy（拓扑序）: 依赖先算；命名使字母序与依赖序相反，真正验证拓扑排序。
    #[test]
    fn topological_order_evaluates_dependencies_first() {
        let mut engine = FormulaEngine::new(vec![
            cfg("M_Top", "[A_Sum] * 2"),
            cfg("A_Sum", "[Z_A] + [Z_B]"),
            cfg("Z_A", "[X] + 1"),
            cfg("Z_B", "[X] * 2"),
        ])
        .expect("ok");
        let order = engine.order();
        let pos = |id: &str| {
            order
                .iter()
                .position(|p| p == id)
                .expect("test input: id must be in order")
        };
        assert!(
            pos("Z_A") < pos("A_Sum"),
            "Z_A must precede A_Sum: {order:?}"
        );
        assert!(
            pos("Z_B") < pos("A_Sum"),
            "Z_B must precede A_Sum: {order:?}"
        );
        assert!(
            pos("A_Sum") < pos("M_Top"),
            "A_Sum must precede M_Top: {order:?}"
        );

        let results = engine.eval_cycle(&values(&[("X", 5.0)]), 1_000_000_000);
        assert_eq!(get(&results, "Z_A").value, Some(6.0));
        assert_eq!(get(&results, "Z_B").value, Some(10.0));
        assert_eq!(get(&results, "A_Sum").value, Some(16.0));
        assert_eq!(get(&results, "M_Top").value, Some(32.0));
    }

    /// QA Error: 除零 → failed；三策略（hold_last 保留值但不得伪装成功 / null / skip）。
    #[test]
    fn divide_by_zero_with_three_failure_strategies() {
        let make = |strategy: OnFailure| {
            let mut c = cfg("R_Div", "[A] / [B]");
            c.on_failure = strategy;
            c.eval_mode = EvalMode::Periodic;
            FormulaEngine::new(vec![c]).expect("ok")
        };
        let mut hold = make(OnFailure::HoldLast);
        let mut null = make(OnFailure::Null);
        let mut skip = make(OnFailure::Skip);

        let ok = values(&[("A", 10.0), ("B", 2.0)]);
        let zero = values(&[("A", 10.0), ("B", 0.0)]);
        assert_eq!(get(&hold.eval_cycle(&ok, 1), "R_Div").value, Some(5.0));
        assert_eq!(get(&null.eval_cycle(&ok, 1), "R_Div").value, Some(5.0));
        assert_eq!(get(&skip.eval_cycle(&ok, 1), "R_Div").value, Some(5.0));

        let hold_out = hold.eval_cycle(&zero, 2);
        let h = get(&hold_out, "R_Div");
        assert!(h.failed, "hold_last 也必须标记失败");
        assert_eq!(h.reason, Some(CalcFailure::DivideByZero));
        assert_eq!(h.value, Some(5.0), "hold_last 保留上次有效值");
        assert_ne!(h.quality, Quality::Good, "不得伪装成 GOOD");

        let null_out = null.eval_cycle(&zero, 2);
        let n = get(&null_out, "R_Div");
        assert!(n.failed && n.value.is_none(), "null 策略必须置空");
        assert_eq!(n.reason, Some(CalcFailure::DivideByZero));

        let skip_out = skip.eval_cycle(&zero, 2);
        let s = get(&skip_out, "R_Div");
        assert!(s.failed && !s.emitted, "skip 策略本周期不产出");
        // 失败计数与最近原因可查询。
        assert_eq!(skip.failure_stat("R_Div").map(|f| f.count), Some(1));
        assert_eq!(
            skip.failure_stat("R_Div").map(|f| f.last_reason),
            Some(CalcFailure::DivideByZero)
        );
    }

    /// QA Error: `pow(10, 1000000000)` 触发上限保护，且不拖垮采集周期。
    #[test]
    fn non_finite_pow_guard_protects_cycle() {
        let mut engine =
            FormulaEngine::new(vec![cfg("R_Boom", "pow(10, 1000000000)")]).expect("ok");
        let started = std::time::Instant::now();
        let results = engine.eval_cycle(&PointValues::new(), 1_000_000_000);
        let elapsed = started.elapsed();
        let result = get(&results, "R_Boom");
        assert!(result.failed);
        assert_eq!(result.reason, Some(CalcFailure::NonFinite));
        assert!(result.value.is_none(), "不得静默输出错误数值");
        assert!(
            elapsed.as_millis() < 50,
            "上限保护必须立即生效，实测 {elapsed:?}"
        );

        // 溢出到 ±Inf 与 NaN 同样按失败处理。
        let mut overflow = FormulaEngine::new(vec![cfg("R_Ovf", "1e308 * 10")]).expect("ok");
        let ovf_out = overflow.eval_cycle(&PointValues::new(), 1);
        let r = get(&ovf_out, "R_Ovf");
        assert_eq!(r.reason, Some(CalcFailure::NonFinite));
    }

    /// QA Error: 缺失 / 非数值输入分别给出不同失败原因。
    #[test]
    fn missing_and_non_numeric_inputs_fail_distinctly() {
        let mut engine = FormulaEngine::new(vec![
            cfg("R_Missing", "[OFFLINE_PT] + 1"),
            cfg("R_NonNumeric", "[STR_PT] + 1"),
            cfg("R_Ok", "[X] + 1"),
        ])
        .expect("ok");
        let mut inputs = values(&[("X", 41.0)]);
        inputs.insert(
            "STR_PT".to_string(),
            InputValue::NonNumeric(Quality::Uncertain),
        );
        let results = engine.eval_cycle(&inputs, 1_000_000_000);

        assert_eq!(
            get(&results, "R_Missing").reason,
            Some(CalcFailure::MissingInput)
        );
        assert_eq!(
            get(&results, "R_NonNumeric").reason,
            Some(CalcFailure::NonNumeric)
        );
        // 失败语义优先于继承质量码：`failed` 表达 calc_failed（口径归 task 53），
        // 统一质量码为 `CalcFailed`（不优于、不劣于继承质量，仅表达「计算失败」）；
        // 「成功时继承最差质量」见 worst_quality_inherited 用例。
        assert_eq!(get(&results, "R_NonNumeric").quality, Quality::CalcFailed);
        assert_eq!(get(&results, "R_Ok").value, Some(42.0));
    }

    /// QA: AST 缓存生效 —— 编译一次后，多个采集周期不再重新解析。
    #[test]
    fn ast_cache_compiles_once_across_cycles() {
        let specs = vec![cfg("R_A", "[X] * 2"), cfg("R_B", "[X] + 3")];
        let mut engine = FormulaEngine::new(specs.clone()).expect("ok");
        assert_eq!(engine.compile_count(), 2, "两个不同表达式各编译一次");
        for cycle in 0..5 {
            let ts = i64::from(cycle) * 1_000_000_000;
            let results = engine.eval_cycle(&values(&[("X", f64::from(cycle))]), ts);
            assert_eq!(results.len(), 2);
        }
        assert_eq!(engine.compile_count(), 2, "5 个周期后仍只解析 2 次");

        // 配置重载但表达式不变 ⇒ 复用编译缓存（仍为 2 次）。
        engine
            .reconfigure(specs.clone(), 6_000_000_000)
            .expect("ok");
        assert_eq!(engine.compile_count(), 2, "表达式未变不应重新解析");
        let results = engine.eval_cycle(&values(&[("X", 10.0)]), 6_000_000_000);
        assert_eq!(get(&results, "R_A").value, Some(20.0));

        // 表达式变更 ⇒ 只新增一次解析；历史数据与失败统计保留。
        let mut changed = specs;
        changed[0] = cfg("R_A", "[X] * 5");
        engine.reconfigure(changed, 7_000_000_000).expect("ok");
        assert_eq!(engine.compile_count(), 3);
        let results = engine.eval_cycle(&values(&[("X", 10.0)]), 7_000_000_000);
        assert_eq!(get(&results, "R_A").value, Some(50.0));
    }

    /// QA: 求值步数预算（等价于超时保护，不起线程）触发 `Timeout`。
    #[test]
    fn eval_step_budget_triggers_timeout() {
        let limits = Limits {
            max_eval_steps: 4,
            ..Limits::default()
        };
        let mut engine =
            FormulaEngine::new_with_limits(vec![cfg("R_Deep", "[A] + [A] + [A] + [A]")], limits)
                .expect("ok");
        let results = engine.eval_cycle(&values(&[("A", 1.0)]), 1_000_000_000);
        let result = get(&results, "R_Deep");
        assert!(result.failed);
        assert_eq!(result.reason, Some(CalcFailure::Timeout));

        // 同一表达式在默认预算下正常求值。
        let mut normal =
            FormulaEngine::new(vec![cfg("R_Deep", "[A] + [A] + [A] + [A]")]).expect("ok");
        let results = normal.eval_cycle(&values(&[("A", 1.0)]), 1_000_000_000);
        assert_eq!(get(&results, "R_Deep").value, Some(4.0));
    }

    /// QA: 长度 / 节点 / 深度 / 数量四类资源上限在保存期生效。
    #[test]
    fn resource_limits_len_nodes_depth_points() {
        let long = format!("[A]{}", " + 1".repeat(200));
        let err = Program::compile(&long).expect_err("too long").to_string();
        assert!(err.contains("超过上限"), "actual: {err}");

        let limits = Limits {
            max_ast_nodes: 3,
            ..Limits::default()
        };
        let err = Program::compile_with("[A] + 1 + 1", &limits)
            .expect_err("too many nodes")
            .to_string();
        assert!(err.contains("节点数"), "actual: {err}");

        let deep_limits = Limits {
            max_parse_depth: 4,
            ..Limits::default()
        };
        let err = Program::compile_with("(((((([A]))))))", &deep_limits)
            .expect_err("too deep")
            .to_string();
        assert!(err.contains("嵌套过深"), "actual: {err}");

        // 依赖链深度链上限。
        let chain: Vec<DerivedPointConfig> = (0..5)
            .map(|i| {
                if i == 0 {
                    cfg("D0", "[X] + 1")
                } else {
                    cfg(&format!("D{i}"), &format!("[D{}] + 1", i - 1))
                }
            })
            .collect();
        let depth_limits = Limits {
            max_dep_depth: 3,
            ..Limits::default()
        };
        let err = FormulaEngine::new_with_limits(chain, depth_limits)
            .expect_err("depth limit")
            .to_string();
        assert!(err.contains("依赖链深度"), "actual: {err}");

        // 计算点数量上限。
        let count_limits = Limits {
            max_points: 1,
            ..Limits::default()
        };
        let err = FormulaEngine::new_with_limits(
            vec![cfg("R_A", "[X] + 1"), cfg("R_B", "[X] + 2")],
            count_limits,
        )
        .expect_err("too many points")
        .to_string();
        assert!(err.contains("计算点数量"), "actual: {err}");
    }

    /// QA Error: 跨设备引用离线 ⇒ 该点按策略降级，**其它计算点仍正常求值**。
    #[test]
    fn cross_device_offline_degrades_only_that_point() {
        let mut engine = FormulaEngine::new(vec![
            cfg("R_Remote", "[REMOTE_PT] + 1"),
            cfg("R_Local", "[X] * 2"),
        ])
        .expect("ok");
        let results = engine.eval_cycle(&values(&[("X", 21.0)]), 1_000_000_000);
        let remote = get(&results, "R_Remote");
        assert!(remote.failed);
        assert_eq!(remote.reason, Some(CalcFailure::MissingInput));
        assert!(remote.value.is_none(), "hold_last 无历史值时应为空");

        let local = get(&results, "R_Local");
        assert!(!local.failed, "不应被离线点位阻塞");
        assert_eq!(local.value, Some(42.0));
    }

    /// QA Error: 改公式后查询历史数据 ⇒ 结果不变（**历史不重算**）。
    #[test]
    fn history_not_recomputed_after_formula_change() {
        let mut engine = FormulaEngine::new(vec![cfg("R", "[X] * 2")]).expect("ok");
        let inputs = values(&[("X", 5.0)]);
        engine.eval_cycle(&inputs, 1_000_000_000);
        let before = engine
            .query_history("R", 0, 1_000_000_000)
            .into_iter()
            .map(|r| (r.ts_ns, r.value))
            .collect::<Vec<_>>();
        assert_eq!(before, vec![(1_000_000_000, Some(10.0))]);

        // 修改公式：只对新增数据生效（本周期输入变化以触发 OnChange 求值）。
        engine
            .reconfigure(vec![cfg("R", "[X] * 3")], 2_000_000_000)
            .expect("ok");
        engine.eval_cycle(&values(&[("X", 6.0)]), 3_000_000_000);
        let after = engine
            .query_history("R", 0, i64::MAX)
            .into_iter()
            .map(|r| (r.ts_ns, r.value))
            .collect::<Vec<_>>();
        assert_eq!(
            after,
            vec![(1_000_000_000, Some(10.0)), (3_000_000_000, Some(18.0))],
            "改公式前已入库的历史不得被重算"
        );

        // 公式变更写入审计日志（含改前 / 改后全文）：首次装载记为「新增」，修改为第二条。
        let audit = engine.audit_log();
        assert_eq!(audit.len(), 2, "首次装载 + 一次修改共两条审计");
        assert_eq!(audit[0].before, None);
        assert_eq!(audit[0].after.as_deref(), Some("[X] * 2"));
        assert_eq!(audit[1].before.as_deref(), Some("[X] * 2"));
        assert_eq!(audit[1].after.as_deref(), Some("[X] * 3"));
    }

    /// QA: 输出类型转换与整数越界检查。
    #[test]
    fn output_type_conversion_and_range_check() {
        let mut int_cfg = cfg("R_Int", "[A]");
        int_cfg.output_type = OutputType::Int64;
        let mut bool_cfg = cfg("R_Bool", "[A] > 0");
        bool_cfg.output_type = OutputType::Bool;
        let mut big_cfg = cfg("R_Big", "[A] * [A]");
        big_cfg.output_type = OutputType::Int64;
        let mut engine = FormulaEngine::new(vec![int_cfg, bool_cfg, big_cfg.clone()]).expect("ok");

        let results = engine.eval_cycle(&values(&[("A", 3.7)]), 1_000_000_000);
        assert_eq!(get(&results, "R_Int").value, Some(4.0), "整型输出四舍五入");
        assert_eq!(get(&results, "R_Bool").value, Some(1.0));
        assert_eq!(get(&results, "R_Big").value, Some(14.0));

        let results = engine.eval_cycle(&values(&[("A", 0.0)]), 2_000_000_000);
        assert_eq!(get(&results, "R_Bool").value, Some(0.0));

        // 越界 ⇒ calc_failed（本模块语义：failed=true + OutputOutOfRange）。
        let mut overflow_engine = FormulaEngine::new(vec![big_cfg]).expect("ok");
        let results = overflow_engine.eval_cycle(&values(&[("A", 4.0e9)]), 1_000_000_000);
        let result = get(&results, "R_Big");
        assert!(result.failed);
        assert_eq!(result.reason, Some(CalcFailure::OutputOutOfRange));

        // float32 溢出同样按越界处理。
        let mut f32_cfg = cfg("R_F32", "[A]");
        f32_cfg.output_type = OutputType::Float32;
        let mut f32_engine = FormulaEngine::new(vec![f32_cfg]).expect("ok");
        let results = f32_engine.eval_cycle(&values(&[("A", 1.0e300)]), 1_000_000_000);
        assert_eq!(
            get(&results, "R_F32").reason,
            Some(CalcFailure::OutputOutOfRange)
        );
    }

    /// QA: 多输入继承最差质量码；保存期校验引用存在性。
    #[test]
    fn worst_quality_inherited_and_validate_unknown_reference() {
        let mut engine = FormulaEngine::new(vec![cfg("R_Mix", "[X] + [Y]")]).expect("ok");
        let mut inputs = values(&[("X", 1.0)]);
        inputs.insert(
            "Y".to_string(),
            InputValue::Numeric(2.0, Quality::Uncertain),
        );
        let results = engine.eval_cycle(&inputs, 1_000_000_000);
        let result = get(&results, "R_Mix");
        assert!(!result.failed);
        assert_eq!(result.value, Some(3.0));
        assert_eq!(result.quality, Quality::Uncertain, "必须继承最差质量码");

        // 引用不存在的点位 ⇒ 保存期拒绝。
        let mut known = BTreeSet::new();
        known.insert("X".to_string());
        let report = validate(&[cfg("R_Bad", "[NOPE] + 1")], &known);
        assert!(!report.ok);
        assert!(report.errors.iter().any(|e| matches!(
            e,
            ValidationError::UnknownReference { reference, .. } if reference == "NOPE"
        )));
        assert!(report.order.is_none(), "校验失败不得给出求值顺序");

        let report = validate(&[cfg("R_Ok", "[X] + 1")], &known);
        assert!(report.ok, "errors: {:?}", report.errors);
        assert_eq!(
            report.order.as_deref(),
            Some(["R_Ok".to_string()].as_slice())
        );
    }

    /// QA: dry-run 返回结果、各依赖中间值与求值顺序（含逐级依赖的计算点）。
    #[test]
    fn dry_run_reports_intermediates_and_engine_dry_run() {
        let inputs = values(&[("X", 8.0), ("Y", 2.0)]);
        let dry = dry_run_expr(
            "[X] / [Y]",
            OutputType::Float64,
            &inputs,
            &PointHistory::new(),
            1.0,
        );
        assert!(dry.ok);
        assert_eq!(dry.value, Some(4.0));
        assert_eq!(dry.intermediates.len(), 2);

        let engine = FormulaEngine::new(vec![
            cfg("R_C", "[R_A] + [R_B]"),
            cfg("R_A", "[X] + 1"),
            cfg("R_B", "[Y] * 2"),
        ])
        .expect("ok");
        let out = engine.dry_run(DryRunRequest {
            point_id: "R_C".to_string(),
            expr: None,
            output_type: None,
            inputs,
            history: PointHistory::new(),
            dt_s: 1.0,
        });
        assert!(out.ok, "error: {:?}", out.error);
        assert_eq!(out.value, Some(13.0), "9 + 4 = 13");
        assert!(out
            .intermediates
            .iter()
            .any(|i| i.point_id == "R_A" && i.value == Some(9.0)));
        assert!(out
            .intermediates
            .iter()
            .any(|i| i.point_id == "R_B" && i.value == Some(4.0)));

        // 草稿表达式语法错误 ⇒ dry-run 结构化返回错误，不 panic。
        let bad = engine.dry_run(DryRunRequest {
            point_id: "R_C".to_string(),
            expr: Some("[X] =".to_string()),
            output_type: None,
            inputs: PointValues::new(),
            history: PointHistory::new(),
            dt_s: 1.0,
        });
        assert!(!bad.ok);
        assert!(bad.error.is_some());
    }

    /// QA（手册基线）: 函数白名单里**每个示例都可实际求值**且等于文档期望值。
    #[test]
    fn whitelist_examples_all_evaluable() {
        let inputs = values(&[("X", 8.0), ("Y", 2.0)]);
        let history = history_of(&[("X", &[Some(5.0), Some(4.0)])]);
        let docs = function_whitelist();
        assert_eq!(docs.len(), 18, "白名单函数数量应与文档一致");
        for doc in docs {
            let out = dry_run_expr(doc.example, OutputType::Float64, &inputs, &history, 1.0);
            assert!(
                out.ok,
                "函数 {} 示例 `{}` 求值失败：{:?}",
                doc.name, doc.example, out.error
            );
            let value = out.value.unwrap_or(f64::NAN);
            assert!(
                (value - doc.example_value).abs() < 1e-9,
                "函数 {} 示例 `{}` 期望 {}，实际 {}",
                doc.name,
                doc.example,
                doc.example_value,
                value
            );
        }
    }

    /// QA: 表达式依赖收集、去重点与现代号映射（`quality()` 取值口径）。
    #[test]
    fn deps_collected_and_quality_code_mapping() {
        let program = Program::compile("[B] + [A] * 2 + [B]").expect("ok");
        assert_eq!(
            program.deps(),
            ["B".to_string(), "A".to_string()].as_slice()
        );
        assert!(program.node_count() > 0);
        assert_eq!(program.source(), "[B] + [A] * 2 + [B]");

        assert_eq!(severity_code(Quality::Good), 0.0);
        assert_eq!(severity_code(Quality::Uncertain), 1.0);
        assert_eq!(severity_code(Quality::Bad), 4.0);
        assert_eq!(severity_code(Quality::CommError), 6.0);
        assert_eq!(Quality::worst(Quality::Good, Quality::Bad), Quality::Bad);
    }

    /// QA: OnChange 模式下依赖无变化不重算；Static: `^` 右结合与惰性 `if` 避免未选中分支污染。
    #[test]
    fn on_change_trigger_and_lazy_if_semantics() {
        let mut cfg_point = cfg("R_OnChange", "[X] * 2");
        cfg_point.eval_mode = EvalMode::OnChange;
        let mut engine = FormulaEngine::new(vec![cfg_point]).expect("ok");

        let first = engine.eval_cycle(&values(&[("X", 4.0)]), 1_000_000_000);
        assert!(get(&first, "R_OnChange").emitted);
        assert_eq!(get(&first, "R_OnChange").value, Some(8.0));

        let same = engine.eval_cycle(&values(&[("X", 4.0)]), 2_000_000_000);
        assert!(!get(&same, "R_OnChange").emitted, "依赖未变化不应重算");

        let changed = engine.eval_cycle(&values(&[("X", 6.0)]), 3_000_000_000);
        assert!(get(&changed, "R_OnChange").emitted);
        assert_eq!(get(&changed, "R_OnChange").value, Some(12.0));

        // `^` 右结合：2 ^ 3 ^ 2 = 2 ^ (3 ^ 2) = 512。
        let dry = dry_run_expr(
            "2 ^ 3 ^ 2",
            OutputType::Float64,
            &PointValues::new(),
            &PointHistory::new(),
            1.0,
        );
        assert_eq!(dry.value, Some(512.0));

        // 惰性 if：未选中分支的除零不污染结果。
        let dry = dry_run_expr(
            "if([B] == 0, 0, [A] / [B])",
            OutputType::Float64,
            &values(&[("A", 10.0), ("B", 0.0)]),
            &PointHistory::new(),
            1.0,
        );
        assert!(dry.ok, "未选中分支不得触发除零：{:?}", dry.error);
        assert_eq!(dry.value, Some(0.0));
    }

    /// QA: 输出死区以「上次对外产出值」为基准，且质量码变化强制输出。
    #[test]
    fn output_deadband_filters_derived_output() {
        let mut cfg_point = cfg("R_Dead", "[X] * 2");
        cfg_point.deadband = 1.0;
        cfg_point.eval_mode = EvalMode::Periodic;
        let mut engine = FormulaEngine::new(vec![cfg_point]).expect("ok");

        let first = engine.eval_cycle(&values(&[("X", 5.0)]), 1_000_000_000);
        assert!(get(&first, "R_Dead").emitted);
        assert_eq!(get(&first, "R_Dead").value, Some(10.0));

        let within = engine.eval_cycle(&values(&[("X", 5.2)]), 2_000_000_000);
        assert!(
            !get(&within, "R_Dead").emitted,
            "10.4 与 10.0 差 0.4 < 1.0 应被过滤"
        );

        let beyond = engine.eval_cycle(&values(&[("X", 6.0)]), 3_000_000_000);
        assert!(get(&beyond, "R_Dead").emitted);
        assert_eq!(get(&beyond, "R_Dead").value, Some(12.0));
    }

    // ================= QA 独立验收（task 70 边界 / 反例） =================

    /// 周期模式配置（强制每周期重算，隔离 OnChange 触发判定的干扰）。
    fn cfg_periodic(point_id: &str, expr: &str) -> DerivedPointConfig {
        let mut c = cfg(point_id, expr);
        c.eval_mode = EvalMode::Periodic;
        c
    }

    /// 空上下文试算辅助。
    fn dry(expr: &str, output_type: OutputType) -> DryRunOutput {
        dry_run_expr(
            expr,
            output_type,
            &PointValues::new(),
            &PointHistory::new(),
            1.0,
        )
    }

    /// QA 边界：空表达式 / 仅空白必须报错；表达式长度上限恰好 512 通过、513 拒绝。
    #[test]
    fn qa_blank_and_expr_length_boundary() {
        for src in ["", "   ", "\t\n \r"] {
            assert!(Program::compile(src).is_err(), "blank {src:?} 必须拒绝");
        }
        let exact = format!("[A]{}", " ".repeat(DEFAULT_MAX_EXPR_LEN - 3));
        assert_eq!(exact.len(), DEFAULT_MAX_EXPR_LEN);
        assert!(Program::compile(&exact).is_ok(), "512 字节必须通过");

        let over = format!("[A]{} ", " ".repeat(DEFAULT_MAX_EXPR_LEN - 3));
        assert_eq!(over.len(), DEFAULT_MAX_EXPR_LEN + 1);
        let err = Program::compile(&over)
            .expect_err("513 字节必须拒绝")
            .to_string();
        assert!(err.contains("超过上限"), "actual: {err}");
    }

    /// QA 边界：解析递归深度上限的**精确**边界（默认 32，`parse_root` 已占 1 层）。
    #[test]
    fn qa_parse_depth_boundary_is_exact() {
        let nested = |n: usize| format!("{}[A]{}", "(".repeat(n), ")".repeat(n));
        let mut ok_depth = 0usize;
        for n in 1..=64 {
            if Program::compile(&nested(n)).is_ok() {
                ok_depth = n;
            } else {
                break;
            }
        }
        assert_eq!(
            ok_depth,
            DEFAULT_MAX_PARSE_DEPTH - 1,
            "恰好 {} 层括号应通过、下一层必须拒绝（`depth` 从 `parse_root` 起算）",
            DEFAULT_MAX_PARSE_DEPTH - 1
        );
        let err = Program::compile(&nested(ok_depth + 1))
            .expect_err("超深必须拒绝")
            .to_string();
        assert!(err.contains("嵌套过深"), "actual: {err}");
    }

    /// QA 反例：惰性 `if` 只求值被选中的分支——未选中分支的除零 / 缺失引用不得污染结果，
    /// 而被选中分支的错误必须如实上报（不得被惰性吞掉）。
    #[test]
    fn qa_lazy_if_evaluates_selected_branch_only() {
        for expr in ["if(1, 7, 1 / 0)", "if(0, 1 / 0, 7)", "if(0, [MISSING], 7)"] {
            let out = dry(expr, OutputType::Float64);
            assert!(
                out.ok,
                "{expr} 未选中分支不得报错：{:?}",
                out.outcome.reason
            );
            assert_eq!(out.value, Some(7.0), "{expr}");
        }
        // 条件为真但分支为 0 → 仍走「真」分支。
        assert_eq!(dry("if(2, 9, 7)", OutputType::Float64).value, Some(9.0));

        let out = dry("if(1, 1 / 0, 7)", OutputType::Float64);
        assert!(!out.ok, "被选中分支的除零必须上报");
        assert_eq!(out.outcome.reason, Some(CalcFailure::DivideByZero));
        assert!(out.outcome.failed);
        assert_eq!(out.value, None);
    }

    /// QA 回归（**RED**）：`dry_run_expr` 的失败结果**未套用**模块文档约定的质量映射
    /// `Quality::worst(继承到的最差, Quality::CalcFailed)`（`eval_cycle` 在
    /// 第 3014 行做了，`dry_run_expr` 漏了）。
    ///
    /// 后果：同一条失败表达式在**试算（监控页草稿）**上报 `GOOD`，在**实际周期求值**上报
    /// `CALC_FAILED` —— 预览与生产不一致，且违反「`failed == true` ⇒ `quality` 取
    /// `CalcFailed`」的模块契约。
    #[test]
    fn qa_dry_run_failure_must_report_calc_failed_quality() {
        let out = dry("1 / 0", OutputType::Float64);
        assert!(!out.ok);
        assert!(out.outcome.failed);
        assert_eq!(
            out.outcome.quality,
            Quality::CalcFailed,
            "失败结果的 quality 必须 >= CalcFailed（不得是 GOOD）"
        );
        // 对照：同样的失败在实际周期路径上确实是 CalcFailed。
        let mut engine = FormulaEngine::new(vec![cfg_periodic("R_Div", "1 / 0")]).expect("ok");
        let results = engine.eval_cycle(&PointValues::new(), 1_000_000_000);
        assert_eq!(get(&results, "R_Div").quality, Quality::CalcFailed);
    }

    /// QA 边界：除零 / 取模零（含分子为 0 的 `0/0`、`0%0`）——`b == 0` 先行判定，不看分子。
    #[test]
    fn qa_divide_and_modulo_by_zero_including_zero_numerator() {
        for expr in ["1 / 0", "0 / 0", "1 % 0", "0 % 0", "-5 / 0"] {
            let out = dry(expr, OutputType::Float64);
            assert!(!out.ok, "{expr} 必须失败");
            assert_eq!(
                out.outcome.reason,
                Some(CalcFailure::DivideByZero),
                "{expr}（0/0 也必须是 DivideByZero，而非 NonFinite）"
            );
        }
        // 负零作除数同样判除零（`-0.0 == 0.0`）。
        assert!(!dry("1 / -0.0", OutputType::Float64).ok);
    }

    /// QA 边界：`clamp` 逆序区间判失败（不 panic / 不静默交换）；`min` / `max` 单参数编译期拒绝。
    #[test]
    fn qa_clamp_inverted_bounds_and_min_max_arity() {
        let bad = dry("clamp(5, 10, 1)", OutputType::Float64);
        assert!(!bad.ok, "lo > hi 必须判失败");
        assert_eq!(bad.outcome.reason, Some(CalcFailure::OutputOutOfRange));
        assert_eq!(dry("clamp(5, 1, 10)", OutputType::Float64).value, Some(5.0));
        // 边界相等合法（闭区间）。
        assert_eq!(dry("clamp(5, 5, 5)", OutputType::Float64).value, Some(5.0));
        // 单参数：arity (2..) 编译期拒绝。
        for src in ["min(1)", "max(1)"] {
            assert!(Program::compile(src).is_err(), "{src} 必须拒绝");
        }
        assert_eq!(dry("min(3, 1, 2)", OutputType::Float64).value, Some(1.0));
        assert_eq!(dry("max(3, 1, 2)", OutputType::Float64).value, Some(3.0));
        assert_eq!(dry("min(2, 2)", OutputType::Float64).value, Some(2.0));
    }

    /// QA 边界（**RED**）：`Int64` 输出上限判定漏了 `2^63` 本身。
    ///
    /// `i64::MAX as f64` 会**向上取整**到 `2^63`，因此 `rounded > i64::MAX as f64`
    /// 对 `rounded == 2^63` 为假 → 一个无法表示为 `i64` 的值被放行。
    /// 正确写法：`rounded >= 9_223_372_036_854_775_808.0`（或用 `i64::try_from` 兜底）。
    #[test]
    fn qa_int64_output_upper_boundary_is_exclusive() {
        const TWO_POW_63: f64 = 9_223_372_036_854_775_808.0;
        // i64::MIN 恰好可表示 → 合法。
        assert_eq!(
            convert_output(-TWO_POW_63, OutputType::Int64),
            Ok(-TWO_POW_63)
        );
        // 略小于 2^63 → 合法。
        assert!(convert_output(9_223_372_036_854_774_000.0, OutputType::Int64).is_ok());
        // 恰好 2^63 → 超出 i64::MAX，必须判 OutputOutOfRange。
        assert_eq!(
            convert_output(TWO_POW_63, OutputType::Int64),
            Err(CalcFailure::OutputOutOfRange),
            "2^63 不可表示为 i64，必须拒绝"
        );
        // 更远 → 已拒绝。
        assert_eq!(
            convert_output(1.0e19, OutputType::Int64),
            Err(CalcFailure::OutputOutOfRange)
        );
    }

    /// QA 边界：`prev()` 首周期无历史 → 失败；`hold(x, 0)` 窗口为 0 → **绝不回看历史**。
    #[test]
    fn qa_hold_zero_window_never_looks_back() {
        let mut engine = FormulaEngine::new(vec![
            cfg_periodic("R_H0", "hold([X], 0)"),
            cfg_periodic("R_H3", "hold([X], 3)"),
            cfg_periodic("R_Prev", "prev([X])"),
        ])
        .expect("ok");

        // 周期 1：首周期无历史 → 三者皆 MissingInput。
        let first = engine.eval_cycle(&PointValues::new(), 1_000_000_000);
        for id in ["R_H0", "R_H3", "R_Prev"] {
            assert_eq!(
                get(&first, id).reason,
                Some(CalcFailure::MissingInput),
                "{id} 首周期必须失败"
            );
        }

        // 周期 2：本周期有值 → hold 直通；prev 仍无上周期值。
        let second = engine.eval_cycle(&values(&[("X", 3.0)]), 2_000_000_000);
        assert_eq!(get(&second, "R_H0").value, Some(3.0));
        assert_eq!(get(&second, "R_H3").value, Some(3.0));
        assert_eq!(
            get(&second, "R_Prev").reason,
            Some(CalcFailure::MissingInput)
        );

        // 周期 3：prev 取到上周期 3.0。
        let third = engine.eval_cycle(&values(&[("X", 3.0)]), 3_000_000_000);
        assert_eq!(get(&third, "R_Prev").value, Some(3.0));

        // 周期 4：本周期缺失——window=0 绝不回看（即便历史上确有 3.0），window=3 可回看。
        let fourth = engine.eval_cycle(&PointValues::new(), 4_000_000_000);
        assert_eq!(
            get(&fourth, "R_H0").reason,
            Some(CalcFailure::MissingInput),
            "hold(x, 0) 禁止回看历史"
        );
        assert!(get(&fourth, "R_H0").failed);
        assert_eq!(get(&fourth, "R_H3").value, Some(3.0), "hold(x, 3) 回看历史");
    }

    /// QA 反例（**已修复**）：`dry_run_expr` 文档称「`error` 在 `ok == false` 时有值」。
    /// 修复前**运行期求值失败**（非编译失败）路径返回 `error = None`（原因只在
    /// `outcome.reason`），前端按文档读取会渲染空消息；修复后运行期失败同样填充 `error`。
    #[test]
    fn qa_dry_run_runtime_failure_still_reports_reason() {
        let out = dry("1 / 0", OutputType::Float64);
        assert!(!out.ok);
        assert_eq!(out.outcome.reason, Some(CalcFailure::DivideByZero));
        assert_eq!(
            out.error.as_deref(),
            Some(CalcFailure::DivideByZero.as_str()),
            "运行期失败也必须填充 error（修复验收报告 P2）"
        );
        // 编译期失败同样填 error。
        let compile_fail = dry("min(1)", OutputType::Float64);
        assert!(!compile_fail.ok);
        assert!(compile_fail.error.is_some());
    }
}

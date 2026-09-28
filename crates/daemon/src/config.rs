//! 配置管理框架（计划 task 4）：TOML 强类型结构 + notify 热重载骨架。
//!
//! 设计：
//! - 三命名空间雏形：`[gateway]`（含授权 / 缓存 / 安全子节）、`[[outlets]]`（北向出口，
//!   每路独立声明编码 protobuf/json——计划北向编码决议）、`[[points]]`（点位平铺行，
//!   设备字段随行冗余，便于 CSV/XLSX 批量导入导出闭环）；
//! - 热重载：notify RecommendedWatcher 监听配置文件，**防抖窗口**内合并事件后整文件
//!   重解析；成功则原子替换共享快照并递增**版本号**，失败则保留旧快照并记 warn
//!   （错误路径不 panic，不中断采集）；
//! - 读侧通过 `Arc<ConfigShared>` 拿 `snapshot()`（`Arc<GatewayConfig>` 无锁读），
//!   `version()` 供调用方探测变更（调度器等按版本重建轮询计划）。

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
// `watch` 方法来自 Watcher trait，必须在作用域内。
use notify::Watcher as _;

use crate::error::{DaemonError, DaemonResult};

/// 热重载防抖窗口：窗口内多次写入合并为一次重载。
pub const DEBOUNCE_WINDOW: Duration = Duration::from_millis(300);
/// 事件轮询间隔（防抖判定精度）。
const POLL_INTERVAL: Duration = Duration::from_millis(50);

fn default_gateway_id() -> String {
    "gw-unset".to_string()
}

/// 数据根目录环境变量（task-61 验收 D-08 修复）：容器部署注入
/// `IOT_DAQ_DATA_DIR`（宿主持久卷挂载点）。修复前只有 preflight 读它，运行期
/// `gateway.data_dir` 默认相对路径 `data`（相对 WORKDIR = 容器 tmpfs）——
/// `docker rm && docker run` 即重置设备密钥 / 试用，正中陷阱 2。
pub const DATA_DIR_ENV: &str = "IOT_DAQ_DATA_DIR";

/// 本地开发默认数据根（相对 WORKDIR 的 `data`；容器形态必须经 env 或显式配置覆盖）。
const LOCAL_DEFAULT_DATA_DIR: &str = "data";

/// `data_dir` 缺省解析（纯函数，单测注入）：env 存在且非空白（`trim().is_empty()`
/// 判空，勿裸 `is_empty()`）→ 用 env 值；否则维持本地开发默认相对 `data`。
fn data_dir_from_env(env_value: Option<String>) -> PathBuf {
    env_value
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
        .map_or_else(|| PathBuf::from(LOCAL_DEFAULT_DATA_DIR), PathBuf::from)
}

fn default_data_dir() -> PathBuf {
    data_dir_from_env(std::env::var(DATA_DIR_ENV).ok())
}

fn default_topic_prefix() -> String {
    "telemetry".to_string()
}

fn default_qos() -> u8 {
    1
}

fn default_frequency_ms() -> u64 {
    1000
}

/// 点位推送开关缺省值（缺省**推送**，向后兼容老配置：无 `push` 键 = true）。
///
/// 语义（需求 6）：`push_enabled = false` 的点位**照常采集、照常进实时流
/// （`/api/stream`）**，但不进北向转发批次。过滤点在真实投递路径
/// [`crate::dataplane::NorthDataPlane`]（见该模块 `forward`）。
fn default_push_enabled() -> bool {
    true
}

/// 点位类型缺省值（`physical`；`derived` = 公式派生点）。
fn default_point_type() -> String {
    "physical".to_string()
}

/// 点位类型取值域（`PointConfig::point_type`）。
pub const POINT_TYPES: &[&str] = &["physical", "derived"];

/// 默认设备分组 id：**永远存在**且不可删除 / 不可改名（需求 4）。
///
/// 配置里没有 `[[device_groups]]` 段时，接口层仍必须体现本分组（无需强制写盘）；
/// `DeviceConfig::group_id` 为 `None` 即归属本分组。
pub const DEFAULT_GROUP_ID: &str = "default";

/// 默认设备分组显示名。
pub const DEFAULT_GROUP_NAME: &str = "默认分组";

fn default_heartbeat_secs() -> u64 {
    86_400
}

/// 试用天数默认（3 天，计划 task 23；与 `auth::client::TRIAL_DAYS` 口径一致）。
fn default_trial_days() -> u32 {
    3
}

/// 离线宽限天数默认（7 天，计划 task 22；与 `auth::client::GRACE_DAYS` 口径一致）。
fn default_grace_days() -> u32 {
    7
}

/// 北向出口编码（每路出口独立可选；计划决议：protobuf 默认 / json 可选）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum OutletEncoding {
    /// Protobuf 编码（默认）。
    #[default]
    Protobuf,
    /// JSON 编码（int64 → string 约定见 protocol-proto）。
    Json,
}

/// 授权配置（计划 task 22-24：激活码 / 试用 / 宽限 + 云服务端点）。
///
/// ⚠️ `activation_code` 是**敏感凭据**：
/// - 本结构手写 [`std::fmt::Debug`]——激活码输出一律 `<redacted>`（**永不进日志**）；
/// - `activation_code` 标注 `serde(skip_serializing)`——配置导出 / API 回显**绝不携带**；
/// - 仓库与示例配置文件只允许**占位符**，真实激活码经环境变量 `IOTDAQ_ACTIVATION_CODE`
///   或管理 UI 注入（装配方注入 [`crate::license::LicenseRuntimeConfig::activation_code`]）。
#[derive(Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct LicensingSection {
    /// 云授权服务地址；None = 纯本地模式（C 档雏形）。
    pub cloud_url: Option<String>,
    /// 心跳间隔（秒；计划默认 24h）。
    pub heartbeat_interval_secs: u64,
    /// 激活码（**敏感**；见结构体文档的脱敏纪律）。`None` = 未配置云端激活。
    #[serde(skip_serializing)]
    pub activation_code: Option<String>,
    /// 试用天数（默认 3，与 `auth::client::TRIAL_DAYS` 口径一致；计划 task 23）。
    pub trial_days: u32,
    /// 离线宽限天数（默认 7，与 `auth::client::GRACE_DAYS` 口径一致；计划 task 22）。
    pub grace_days: u32,
}

impl Default for LicensingSection {
    fn default() -> Self {
        Self {
            cloud_url: None,
            heartbeat_interval_secs: default_heartbeat_secs(),
            activation_code: None,
            trial_days: default_trial_days(),
            grace_days: default_grace_days(),
        }
    }
}

/// 手写 [`std::fmt::Debug`]：**激活码脱敏**（只暴露是否已配置，绝不输出原文）。
impl std::fmt::Debug for LicensingSection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LicensingSection")
            .field("cloud_url", &self.cloud_url)
            .field("heartbeat_interval_secs", &self.heartbeat_interval_secs)
            .field(
                "activation_code",
                &self.activation_code.as_ref().map(|_| "<redacted>"),
            )
            .field("trial_days", &self.trial_days)
            .field("grace_days", &self.grace_days)
            .finish()
    }
}

/// 缓存配置雏形（task 17-18 填充 SQLite 细节）。
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct CacheSection {
    /// 缓存库文件路径（相对 data_dir）。
    pub sqlite_path: String,
    /// 缓存上限（MB；超出后按环形覆盖策略淘汰）。
    pub max_size_mb: u64,
    /// 保留天数（计划：离线缓存 ≥7 天）。
    pub retention_days: u32,
}

impl Default for CacheSection {
    fn default() -> Self {
        Self {
            sqlite_path: "cache.db".to_string(),
            max_size_mb: 512,
            retention_days: 7,
        }
    }
}

/// 安全配置雏形（task 25/31/34 填充 SQLCipher / TLS 细节）。
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct SecuritySection {
    /// 北向/界面 TLS 证书路径。
    pub tls_cert_path: Option<String>,
    /// 北向/界面 TLS 私钥路径。
    pub tls_key_path: Option<String>,
    /// Web 管理界面账号鉴权开关（计划：强制账号密码 + HTTPS）。
    pub web_auth_enabled: bool,
}

impl Default for SecuritySection {
    fn default() -> Self {
        Self {
            tls_cert_path: None,
            tls_key_path: None,
            web_auth_enabled: true,
        }
    }
}

/// 备份策略默认保留份数（`[settings.backup_policy].retention_count`）。
fn default_backup_retention() -> u32 {
    20
}

/// `[settings]` 管理面设置段（**可选**；缺省 = 全部按默认策略，旧配置兼容）。
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize, Default)]
pub struct SettingsSection {
    /// 写前备份策略。
    #[serde(default)]
    pub backup_policy: BackupPolicy,
    /// 升级源声明（`[settings.updates]`；缺省 = 未配置 → 更新检查 / 执行诚实降级）。
    #[serde(default, skip_serializing_if = "UpdatesSection::is_unconfigured")]
    pub updates: UpdatesSection,
}

/// `[ops]` 运维段（**可选**：缺省 = 不启用计划重启；管理面
/// `PUT /api/ops/scheduled-restart` 写入，进程重启后重新装载）。
///
/// - `scheduled_restart_at`：每日定时重启时刻（`"HH:MM"`，24 小时制，本地时区）；
///   空串 / 未配置 = 关闭。同一天只生效一次（`last_restart_date` 记录已重启的
///   日期，跨日后自动恢复）。
/// - `last_restart_date`：最近一次计划重启的日期（`YYYY-MM-DD`；空 = 从未触发）。
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize, Default)]
pub struct OpsSection {
    /// 每日定时重启时刻（`"HH:MM"`；空 = 不启用）。
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub scheduled_restart_at: String,
    /// 最近一次计划重启日期（`YYYY-MM-DD`；空 = 从未触发）。
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub last_restart_date: String,
}

impl OpsSection {
    /// 是否启用每日定时重启（`scheduled_restart_at` 非空即为启用）。
    pub fn scheduled_restart_enabled(&self) -> bool {
        !self.scheduled_restart_at.trim().is_empty()
    }
}

/// 备份策略（`[settings.backup_policy]`；把既有「写前自动备份」行为显式化）。
///
/// - `auto_before_write`：`GatewayConfig::save` 落盘前是否自动备份（默认 `true`
///   = 既有行为显式化；显式关掉后写路径**不再**生成 `.bak-*`，恢复只能靠手动备份）。
/// - `retention_count`：`{config}.bak-*` 备份最大保留份数，超出删最旧
///   （**只清本服务自产的该前缀文件**，绝不碰用户其他文件）；`0` = 不清理。
/// - `interval_min`：周期备份间隔（分钟；`0` = 关闭，默认）。
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct BackupPolicy {
    /// 写路径前自动备份开关（默认 `true`）。
    #[serde(default = "default_true")]
    pub auto_before_write: bool,
    /// `.bak-*` 备份最大保留份数（超出删最旧；`0` = 不清理；默认 20）。
    #[serde(default = "default_backup_retention")]
    pub retention_count: u32,
    /// 周期备份间隔（分钟；`0` = 关闭；默认 0）。
    #[serde(default)]
    pub interval_min: u32,
}

impl Default for BackupPolicy {
    fn default() -> Self {
        Self {
            auto_before_write: true,
            retention_count: default_backup_retention(),
            interval_min: 0,
        }
    }
}

/// `[settings.updates]` 升级源声明段（**可选**；本版本只承载「声明」，运行时接线未实现）。
///
/// - `source_url`：升级源地址（如 `https://ota.example.com/gateway`）；`None` / 空白 =
///   **未配置升级源** → `GET /api/updates/check` 与 `POST /api/updates/apply` 一律结构化
///   返回 `supported:false` + **面向用户**的原因，绝不伪造「已是最新」/「升级成功」；
/// - `signing_key`：更新包签名校验公钥（Ed25519；PEM 或 hex）；`None` = 未配置。
///
/// 本段只承载声明；检查 / 下载 / 安装 / 回滚的运行时能力尚未接入（诚实降级，
/// 消费方见 `mgmt::ops_api`）。
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize, Default)]
pub struct UpdatesSection {
    /// 升级源地址（`None` / 空白 = 未配置）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_url: Option<String>,
    /// 更新包签名校验公钥（Ed25519；`None` = 未配置）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signing_key: Option<String>,
}

impl UpdatesSection {
    /// 未配置（两字段皆 `None`）：序列化时省略该段，旧 daemon 仍可读取新写出的文件。
    fn is_unconfigured(&self) -> bool {
        self.source_url.is_none() && self.signing_key.is_none()
    }
}

/// 管理面登录账号（task 57 全量接线：生产路凭证来源）。
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct MgmtAuthUser {
    /// 用户名（登录主体；登录时精确匹配）。
    pub name: String,
    /// 角色字面量（`ops` / `lic_ops` / `risk` / `system`；由 mgmt 层
    /// `rbac::Role::from_str` 解析，未知角色该账号被跳过——fail-closed）。
    pub role: String,
    /// `SHA-256(password)` 的 hex 编码（服务端只存哈希，比对走恒时比较；
    /// 明文密码永不写入配置文件）。
    pub password_hash: String,
    /// 显示名（`None` = 展示层回退 `name`；首次安装 bootstrap 可带）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    /// 账号状态（`"active"` / `"disabled"`；`None` = 按 active 处理——老配置兼容）。
    /// disabled 账号在登录装配时被跳过（fail-closed，见 `auth_login::sync_users`）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    /// 建号时刻（Unix 毫秒；账号管理端点写入。`None` = 老配置未记录）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_at_ms: Option<u64>,
    /// 最近一次登录成功时刻（Unix 毫秒；登录成功回写。`None` = 尚无记录）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_login_at_ms: Option<u64>,
}

/// 管理面自定义角色（`[[mgmt_auth.roles]]`；账号与角色页）。
///
/// **内置角色**（rbac 的四角色）不写入本段——本段只承载用户自建角色；
/// `permissions` 为权限 id 字符串列表（如 `device.write` / `point.write`）。
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct MgmtAuthRole {
    /// 角色标识（唯一键）。
    pub id: String,
    /// 角色显示名。
    pub name: String,
    /// 权限 id 列表（`rbac::Permission::as_str` 的取值域；空 = 无权限）。
    #[serde(default)]
    pub permissions: Vec<String>,
    /// 是否内置角色（**只读标记**；内置角色不入配置段，本段恒 `false`——
    /// 与前端 `RoleRecord.builtin` 对齐；老配置缺省 `false`）。
    #[serde(default)]
    pub builtin: bool,
}

/// 管理面登录凭证段（**可选**；缺省时生产路无凭证，登录仅开发路可用，
/// 详见 `mgmt::auth_login` 的两路 fail-closed 说明）。
#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct MgmtAuthSection {
    /// 登录账号列表（空列表 = 无任何登录凭证 → 登录端点全拒）。
    #[serde(default)]
    pub users: Vec<MgmtAuthUser>,
    /// 自定义角色列表（**可选**；缺省空 = 只有内置角色）。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub roles: Vec<MgmtAuthRole>,
}

fn default_true() -> bool {
    true
}

/// `[[rules]]` 转发规则（声明式存储行；引擎语义由 [`crate::rules`] 承载）。
///
/// 结构：**UI 字段**（`id` / `name` / `forwarder_id` / `priority` / `enabled`，
/// 供列表页展示与排序）+ **引擎字段**（`when` 条件树 / `actions` 动作列表，
/// 类型直接复用 [`crate::rules::Condition`] / [`crate::rules::Action`]——经
/// [`Self::to_rule`] 可零拷贝语义转成引擎规则）。
/// **禁用 `#[serde(flatten)]`**：`rules::Rule` 有 `deny_unknown_fields`，与
/// flatten 不兼容；本结构是独立的存储 schema，不直接反序列化成 `Rule`。
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
pub struct RuleConfig {
    /// 规则主键（唯一）。
    pub id: String,
    /// 规则名称。
    pub name: String,
    /// 生效出口 id（`[[outlets]].name`；`None` = 未绑定；UI 字段）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub forwarder_id: Option<String>,
    /// WHERE 条件树（**引擎字段**；`None` = 恒真 → 通配规则）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub when: Option<crate::rules::Condition>,
    /// DO 动作列表（**引擎字段**；按数组顺序执行；缺省空列表 = 无动作）。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub actions: Vec<crate::rules::Action>,
    /// SELECT 字段白名单（**引擎字段**；空 = 输出全部字段；UI 字段）。
    ///
    /// 由转发规则页「字段白名单（select，逗号分隔）」编辑（web-console
    /// `RulesPage.vue`）；`to_rule()` 逐字透传给 [`crate::rules::Rule::select`]。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub select: Vec<String>,
    /// 依赖的规则 id 列表（**引擎字段**；DAG 由 [`crate::rules::RuleEngine`]
    /// 构造期校验——环 / 自依赖 / 未知 id 一律拒绝；UI 字段）。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub depends_on: Vec<String>,
    /// 优先级（数字越小越先匹配；缺省 0；UI 字段）。
    #[serde(default)]
    pub priority: i64,
    /// 是否启用（缺省 true）。
    #[serde(default = "default_true")]
    pub enabled: bool,
}

impl Default for RuleConfig {
    fn default() -> Self {
        Self {
            id: String::new(),
            name: String::new(),
            forwarder_id: None,
            when: None,
            actions: Vec::new(),
            select: Vec::new(),
            depends_on: Vec::new(),
            priority: 0,
            enabled: true,
        }
    }
}

impl RuleConfig {
    /// 转换为引擎规则 [`crate::rules::Rule`]（`version` / `select` / `transform` /
    /// `depends_on` 取引擎缺省；`id` / `when` / `actions` 逐字透传）。
    ///
    /// 引擎侧构造期校验（字段白名单 / JSONPath / DAG 等）仍由
    /// [`crate::rules::RuleSet`] 在装载时执行——本方法不做语义校验。
    #[must_use]
    pub fn to_rule(&self) -> crate::rules::Rule {
        crate::rules::Rule {
            id: self.id.clone(),
            version: None,
            when: self.when.clone(),
            actions: self.actions.clone(),
            select: self.select.clone(),
            transform: None,
            depends_on: self.depends_on.clone(),
        }
    }
}

/// 单条告警规则（`[[alarms.rules]]`）。
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
pub struct AlarmRuleConfig {
    /// 规则主键（唯一）。
    pub id: String,
    /// 规则名称（`None` = 展示层回退 `id`）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// 告警级别（`critical` / `major` / `minor` / `warning`；`None` = 未声明）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub level: Option<String>,
    /// 来源类型（`device` / `forwarder` / `license` / `system`；`None` = 未声明）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_type: Option<String>,
    /// 触发条件描述（如 `status == offline`；`None` = 未声明）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub condition: Option<String>,
    /// 数值阈值（`None` = 非阈值型规则）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub threshold: Option<f64>,
    /// 绑定设备 id（**引擎消费**；`None` = 任意设备）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device_id: Option<String>,
    /// 绑定点位 id（**引擎消费**；`None` = 设备级聚合）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub point_id: Option<String>,
    /// 比较运算符（**引擎消费**；取值域见 [`ALARM_RULE_OPS`]；`None` = 未声明）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub op: Option<String>,
    /// 持续满足时长（毫秒；`None` = 立即触发）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
    /// 抑制窗口（毫秒；`None` = 不抑制）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub suppress_ms: Option<u64>,
    /// 是否启用（缺省 true）。
    #[serde(default = "default_true")]
    pub enabled: bool,
}

impl Default for AlarmRuleConfig {
    fn default() -> Self {
        Self {
            id: String::new(),
            name: None,
            level: None,
            source_type: None,
            condition: None,
            threshold: None,
            device_id: None,
            point_id: None,
            op: None,
            duration_ms: None,
            suppress_ms: None,
            enabled: true,
        }
    }
}

/// 告警规则比较运算符取值域（[`AlarmRuleConfig::op`]；`gt`>、`ge`>=、`lt`<、
/// `le`<=、`eq`==、`ne`!=）。仅声明 schema 值域，供告警引擎（BE-ALARM）校验。
pub const ALARM_RULE_OPS: &[&str] = &["gt", "ge", "lt", "le", "eq", "ne"];

/// `[alarms]` 告警配置段（**可选**；缺省 `None` = 未配置告警）。
#[derive(Debug, Clone, Deserialize, Serialize, Default)]
#[serde(default)]
pub struct AlarmsSection {
    /// 告警总开关（缺省 false——未显式启用不产生告警）。
    pub enabled: bool,
    /// 告警规则列表（`[[alarms.rules]]`）。
    pub rules: Vec<AlarmRuleConfig>,
}

/// OTA 轮询默认周期（秒）：保守值 1 小时。
fn default_ota_poll_interval_secs() -> u64 {
    3_600
}

/// `[gateway.ota]` 系统更新段（**可选**；缺省 `enabled = false` = 不做任何检查）。
///
/// 语义（诚实降级）：
/// - `enabled = false`（默认）→ 调度任务 no-op，**不发任何网络请求**、不报错；
/// - `enabled = true` 但 `manifest_url` / `signing_key_b64` 缺失 → 视为**未配置**：
///   调度任务记一次 `warn!` 说明「OTA 未配置，跳过升级检查」并附配置键名，
///   绝不 panic、绝不静默；
/// - `manifest_url` 指向授权端 `GET /updates/manifest`（V1 仅支持 `http://` 明文，
///   完整性由 Ed25519 验签兜底，理由见 [`crate::ota`] 模块文档）；
/// - `signing_key_b64` 是 Ed25519 **公钥**（32 字节，标准 base64）——用于校验
///   下发的 manifest 签名，签名者是持有对应私钥的授权端；
/// - `current_version` 是网关当前 OTA 版本号（u64 单调序）；新包版本必须**严格大于**
///   该值（防降级 / 防重放旧包）。缺省 `0`。
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(default)]
pub struct OtaSection {
    /// 是否启用 OTA 检查（缺省 **false** = 不动作）。
    pub enabled: bool,
    /// 授权端 manifest 端点（如 `http://licensing.internal:7080/updates/manifest`）。
    pub manifest_url: Option<String>,
    /// 轮询周期（秒；缺省 3600）。`0` 在运行时按 1 秒兜底。
    pub poll_interval_secs: u64,
    /// 授权端 OTA 签名公钥（Ed25519 公钥 32 字节的标准 base64）。
    pub signing_key_b64: Option<String>,
    /// 网关当前 OTA 版本号（u64 单调序；缺省 0）。
    pub current_version: u64,
}

impl Default for OtaSection {
    fn default() -> Self {
        Self {
            enabled: false,
            manifest_url: None,
            poll_interval_secs: default_ota_poll_interval_secs(),
            signing_key_b64: None,
            current_version: 0,
        }
    }
}

impl OtaSection {
    /// 是否**已配置**（enabled + manifest_url + signing_key_b64 三者齐备）。
    ///
    /// `enabled = false`、URL 为空串 / 全空白、公钥缺失 → 均视为未配置。
    pub fn is_configured(&self) -> bool {
        self.enabled
            && !self.manifest_url.as_deref().unwrap_or("").trim().is_empty()
            && !self
                .signing_key_b64
                .as_deref()
                .unwrap_or("")
                .trim()
                .is_empty()
    }

    /// 未配置时的**面向运维**提示（含配置键名与怎么配）。
    pub fn config_hint(&self) -> &'static str {
        "OTA 未配置，跳过升级检查；如需启用请在 [gateway.ota] 段配置 \
         enabled = true、manifest_url = \"http://<授权端主机>:<端口>/updates/manifest\"、\
         signing_key_b64 = \"<授权端 Ed25519 公钥 base64>\"（poll_interval_secs 可选，缺省 3600）"
    }
}

/// `[gateway]` 命名空间。
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct GatewaySection {
    /// 网关标识（平台侧登记）。
    pub gateway_id: String,
    /// 数据根目录（缓存 / 日志 / 授权状态落盘位置；容器内须挂持久卷）。
    pub data_dir: PathBuf,
    /// 授权配置。
    pub licensing: LicensingSection,
    /// 缓存配置。
    pub cache: CacheSection,
    /// 安全配置。
    pub security: SecuritySection,
    /// 系统更新（OTA）配置。
    pub ota: OtaSection,
}

impl Default for GatewaySection {
    fn default() -> Self {
        Self {
            gateway_id: default_gateway_id(),
            data_dir: default_data_dir(),
            licensing: LicensingSection::default(),
            cache: CacheSection::default(),
            security: SecuritySection::default(),
            ota: OtaSection::default(),
        }
    }
}

/// `[[outlets]]` 北向出口（每路独立：broker / topic / qos / tls / 编码）。
#[derive(Clone, Deserialize, Serialize)]
pub struct OutletConfig {
    /// 出口名（唯一键，日志与诊断用）。
    pub name: String,
    /// MQTT broker 地址（如 `mqtts://broker.local:8883`）。
    pub broker: String,
    /// 主题前缀。
    #[serde(default = "default_topic_prefix")]
    pub topic_prefix: String,
    /// QoS 等级（0/1/2）。
    #[serde(default = "default_qos")]
    pub qos: u8,
    /// 是否启用 TLS。
    ///
    /// 与 `broker` 的 scheme 必须**一致**：`mqtts://` ⇔ `tls = true`；
    /// `mqtt://` ⇔ `tls = false`。冲突（如 `tls = true` 配 `mqtt://`）在
    /// [`crate::north::runtime::endpoint_from_outlet`] 处报错，**绝不猜测意图**。
    #[serde(default)]
    pub tls: bool,
    /// 服务端 CA 证书 PEM 路径（task 25；TLS 出口**可选**——缺省用**操作系统根证书库**
    /// 校验 broker 证书；2026-09-25 用户决策「证书配置可选」。仍无跳过校验路径）。
    ///
    /// `None` = 用操作系统根证书库（TLS 未启用时忽略）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ca_cert_path: Option<String>,
    /// 客户端证书 PEM 路径（task 25；mTLS，与 `client_key_path` **成对**）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_cert_path: Option<String>,
    /// 客户端私钥 PEM 路径（task 25；mTLS，与 `client_cert_path` **成对**）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_key_path: Option<String>,
    /// SNI / 服务端名校验覆盖（task 25；缺省用 `broker` 的 host）。
    ///
    /// 用途：broker 证书 SAN 与连接地址不同（如经 DNS 别名/负载均衡接入）时，
    /// 显式声明应校验的服务端名——**不是**「跳过校验」开关。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server_name: Option<String>,
    /// ALPN 协议列表（task 25；如 `["mqtt"]`，空 = 不协商）。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub alpn: Vec<String>,
    /// 该路出口的载荷编码（默认 protobuf）。
    #[serde(default)]
    pub encoding: OutletEncoding,
    /// MQTT 认证用户名（**可选**；与 `password` 成对）。
    ///
    /// ⚠️ **明文存于 TOML 配置文件**——敏感凭据。日志 / 诊断导出 / 任何 `{:?}`
    /// 打印都**不得**包含明文口令；本结构实现了自定义 `Debug`，口令恒被掩码为
    /// `***`。与 `password` 同时配置时由
    /// [`crate::north::runtime::endpoint_from_outlet`] 注入连接；仅 `password`
    /// 无 `username` 时由 [`crate::north::mqtt::EndpointConfig::validate`] 拒绝。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub username: Option<String>,
    /// MQTT 认证口令（**可选**；与 `username` 成对）。
    ///
    /// ⚠️ **明文存于 TOML 配置文件**——与 `username` 同属敏感凭据，且**绝不**经
    /// 日志 / 诊断 / 任何 `{:?}` 泄露（自定义 `Debug` 已掩码）。仅当 `username`
    /// 一并配置时才被使用。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub password: Option<String>,
}

impl std::fmt::Debug for OutletConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OutletConfig")
            .field("name", &self.name)
            .field("broker", &self.broker)
            .field("topic_prefix", &self.topic_prefix)
            .field("qos", &self.qos)
            .field("tls", &self.tls)
            .field("ca_cert_path", &self.ca_cert_path)
            .field("client_cert_path", &self.client_cert_path)
            .field("client_key_path", &self.client_key_path)
            .field("server_name", &self.server_name)
            .field("alpn", &self.alpn)
            .field("encoding", &self.encoding)
            .field("username", &self.username)
            // 口令永远掩码，绝不打印明文（安全红线）。
            .field("password", &self.password.as_ref().map(|_| "***"))
            .finish()
    }
}

/// `[[points]]` 点位平铺行（设备级字段随行冗余，便于批量导入导出）。
///
/// 全部新字段（`push_enabled` 及元数据）均 `serde(default)`，老配置文件
/// （只含 `device_id/point_id/protocol/address/frequency_ms`）可无缝加载。
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct PointConfig {
    /// 南向设备标识。
    pub device_id: String,
    /// 点位标识（点位表主键）。
    pub point_id: String,
    /// 协议（modbus-tcp / modbus-rtu / opcua / s7 / mc / http / mqtt）。
    pub protocol: String,
    /// 设备接入地址（如 `192.168.1.10:502` 或串口号）。
    pub address: String,
    /// 点位级端点覆盖（可选；缺省 = 本点位 `address` 或所属设备
    /// `DeviceConfig.endpoint`）。
    ///
    /// V1 同批次引入，让端点有唯一归属：点位行 `address` 仍是端点事实源，
    /// 本字段仅作点位级覆盖冗余（例如同一设备下个别点位走不同网关出口）。
    /// `None` = 不覆盖，回退设备级端点 / `address`。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub endpoint: Option<String>,
    /// 采集频率（毫秒；计划指标 ≥100ms）。
    #[serde(default = "default_frequency_ms")]
    pub frequency_ms: u64,
    /// 北向推送开关（缺省 **true** = 推送）。`false` = 照常采集、照常进实时流，
    /// 但不进北向转发批次（需求 6）。TOML / JSON 键名 `push_enabled`，同时接受
    /// 别名 `push`（CSV 列名与前端习惯）。
    #[serde(default = "default_push_enabled", alias = "push")]
    pub push_enabled: bool,
    /// 点位显示名（`None` = 展示层回退 `point_id`）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// 数据类型（如 `float32` / `int16` / `bool`；`None` = 未声明）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data_type: Option<String>,
    /// 字节序（如 `ABCD` / `BADC` / `CDAB`；`None` = 未声明）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub byte_order: Option<String>,
    /// 工程单位（如 `degC` / `kPa`；`None` = 未声明）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unit: Option<String>,
    /// 死区阈值（工程单位；JSON 层为 number，**非**大整数——未声明 = `None`）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deadband: Option<f64>,
    /// 北向目标键名（`None` = 回退 `point_id`）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_key: Option<String>,
    /// 点位类型：`physical`（默认）| `derived`（公式派生）。
    #[serde(default = "default_point_type")]
    pub point_type: String,
    /// 公式表达式（`point_type = derived` 时使用；`None` = 未声明）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub formula: Option<String>,
    /// 点位模拟开关（缺省 false = 不模拟，走真实南向读）。
    #[serde(default)]
    pub sim_enabled: bool,
    /// 模拟模式（**可选**；`None` / 缺省 = 由模拟引擎取缺省波形）。取值域由
    /// 模拟引擎（BE-SIM）定义：建议 `random`（min~max 随机）| `fixed`（恒定
    /// `sim_min`）；未知值在引擎装载期拒绝，schema 层不校验。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sim_mode: Option<String>,
    /// 模拟值下限（`None` = 未声明，由模拟器取缺省）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sim_min: Option<f64>,
    /// 模拟值上限（`None` = 未声明）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sim_max: Option<f64>,
    /// 模拟值小数位数（`None` = 未声明；`u32` 计数按 JSON number 语义安全）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sim_dec: Option<u32>,
    /// 模拟刷新周期（毫秒；`None` = 未声明，回退点位 `frequency_ms`）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sim_period_ms: Option<u64>,
}

/// 手写 [`Default`]：`push_enabled = true`、`point_type = "physical"`，
/// 其余元数据为 `None`（与 serde 缺省语义一致；供测试字面量 `..Default::default()`）。
impl Default for PointConfig {
    fn default() -> Self {
        Self {
            device_id: String::new(),
            point_id: String::new(),
            protocol: String::new(),
            address: String::new(),
            endpoint: None,
            frequency_ms: default_frequency_ms(),
            push_enabled: default_push_enabled(),
            name: None,
            data_type: None,
            byte_order: None,
            unit: None,
            deadband: None,
            target_key: None,
            point_type: default_point_type(),
            formula: None,
            sim_enabled: false,
            sim_mode: None,
            sim_min: None,
            sim_max: None,
            sim_dec: None,
            sim_period_ms: None,
        }
    }
}

/// 设备登记行（可选 `[[devices]]` 段；管理面写接口的设备事实源）。
///
/// 设计取舍：设备列表 = 本段 ∪ `points.device_id` 去重聚合。**点位行仍是采集
/// 唯一事实源**——设备的协议 / 频率始终优先按点位行推导；本段只承载「无点位
/// 设备的登记」与「显示名 / 启停覆盖」，旧配置无此段时 serde default 为空列表，
/// 完全向后兼容。
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct DeviceConfig {
    /// 设备标识（唯一键；与点位行的 `device_id` 同一命名空间）。
    pub device_id: String,
    /// 显示名（`None` = 展示层回退 `device_id`）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// 启停（缺省 `true`）。
    #[serde(default = "default_device_enabled")]
    pub enabled: bool,
    /// 设备默认协议（可选；仅当该设备无点位行时在设备摘要中展示）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub protocol: Option<String>,
    /// 所属设备分组 id（可选；缺省 / 空 = 归属 [`DEFAULT_GROUP_ID`] 默认分组）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group_id: Option<String>,
    /// 设备级接入端点（可选；缺省 = 点位行的端点）。
    ///
    /// V1 同批次引入，让端点有唯一归属：设备登记段可在此声明端点，点位行
    /// `address` / `endpoint` 未覆盖时采用本值。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub endpoint: Option<String>,
}

fn default_device_enabled() -> bool {
    true
}

/// `[[device_groups]]` 设备分组（需求 4）。
///
/// 默认分组（[`DEFAULT_GROUP_ID`] / [`DEFAULT_GROUP_NAME`]）**不写入本段也永远存在**；
/// 本段只承载用户自建分组。`id` 唯一，`name` 为显示名。
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct DeviceGroupConfig {
    /// 分组标识（唯一键；不得等于 [`DEFAULT_GROUP_ID`]）。
    pub id: String,
    /// 分组显示名。
    pub name: String,
}

/// 网关强类型配置根。
#[derive(Debug, Clone, Deserialize, Serialize, Default)]
#[serde(default)]
pub struct GatewayConfig {
    /// `[gateway]` 命名空间。
    pub gateway: GatewaySection,
    /// `[[outlets]]` 北向出口列表。
    pub outlets: Vec<OutletConfig>,
    /// `[[points]]` 点位列表。
    pub points: Vec<PointConfig>,
    /// `[[devices]]` 设备登记段（**可选**，向后兼容：缺省空列表；见
    /// [`DeviceConfig`] 的取舍说明。序列化时空列表省略，避免旧版本 daemon
    /// 拒读新字段）。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub devices: Vec<DeviceConfig>,
    /// `[[device_groups]]` 设备分组段（**可选**；缺省空列表 = 只有默认分组，
    /// 接口层永远合成出默认分组，见 [`DEFAULT_GROUP_ID`]）。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub device_groups: Vec<DeviceGroupConfig>,
    /// `[[rules]]` 转发规则段（**可选**；缺省空列表 = 无规则）。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub rules: Vec<RuleConfig>,
    /// `[alarms]` 告警配置段（**可选**；缺省 `None` = 未配置告警）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub alarms: Option<AlarmsSection>,
    /// `[mgmt_auth]` 管理面登录凭证段（**可选**；缺省 = 生产路未配置凭证，
    /// 登录走 `mgmt::auth_login` 的开发路 / fail-closed 逻辑，既有字段语义不变）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mgmt_auth: Option<MgmtAuthSection>,
    /// `[settings]` 管理面设置段（**可选**；缺省 = 备份策略全默认，旧配置兼容）。
    #[serde(default)]
    pub settings: SettingsSection,
    /// `[ops]` 运维段（**可选**；缺省 = 不启用计划重启，旧配置兼容）。
    #[serde(default)]
    pub ops: OpsSection,
}

impl GatewayConfig {
    /// 从 TOML 文件加载。
    ///
    /// # Errors
    /// 读取失败映射 [`DaemonError::StorageError`]，解析失败映射
    /// [`DaemonError::ConfigError`]（错误路径不 panic）。
    pub fn load(path: impl AsRef<Path>) -> DaemonResult<Self> {
        let path = path.as_ref();
        let raw = std::fs::read_to_string(path)
            .map_err(|e| DaemonError::StorageError(format!("read {}: {e}", path.display())))?;
        let config: GatewayConfig = toml::from_str(&raw)?;
        warn_inverted_points(&config);
        Ok(config)
    }

    /// 从 TOML 字符串解析（测试与嵌入场景用）。
    ///
    /// # Errors
    /// 解析失败映射 [`DaemonError::ConfigError`]。
    pub fn parse(raw: &str) -> DaemonResult<Self> {
        Ok(toml::from_str(raw)?)
    }

    /// 保存配置到 TOML 文件（管理面写路径）：
    /// toml 序列化 → （按 `[settings.backup_policy]`）`backup_before_rewrite`
    /// 写前原子备份 + retention 清理 → 临时文件 + fsync + 同目录 rename 原子落盘。
    /// 任一步失败即中止，原文件保持写前状态（或可从 `.YYYYMMDD-HHmmss-NNN.bak` 备份恢复）。
    ///
    /// `auto_before_write = false` 时跳过写前备份（恢复只能靠手动备份端点）；
    /// retention 清理只删本服务自产的备份（新格式 `{file_name}.YYYYMMDD-HHmmss-NNN.bak`
    /// 与旧格式 `{file_name}.bak-*`），失败仅告警不阻断写路径。
    ///
    /// # Errors
    /// - 序列化失败 → [`DaemonError::ConfigError`]；
    /// - 备份 / 写盘失败 → [`DaemonError::StorageError`]（临时半成品一律清理）。
    pub fn save(&self, path: impl AsRef<Path>) -> DaemonResult<()> {
        let path = path.as_ref();
        let raw = toml::to_string_pretty(self)
            .map_err(|e| DaemonError::ConfigError(format!("serialize config: {e}")))?;
        if self.settings.backup_policy.auto_before_write {
            let backup = crate::migrations::backup_before_rewrite(path)?;
            tracing::info!(
                target: "daemon::config",
                path = %path.display(),
                backup = %backup.display(),
                "config: backup created before rewrite"
            );
            // retention 清理（auto 备份与手动备份共用同一策略；0 = 不清理）。
            let removed = crate::migrations::enforce_backup_retention(
                path,
                self.settings.backup_policy.retention_count,
            );
            if removed > 0 {
                tracing::info!(
                    target: "daemon::config",
                    removed,
                    retention = self.settings.backup_policy.retention_count,
                    "config: backup retention pruned oldest .bak files"
                );
            }
        }
        atomic_write(path, raw.as_bytes())?;
        Ok(())
    }
}

/// 启发式判定 `raw` 是否「长得像寄存器号」（而非设备端点 host:port / 串口 / URL）。
///
/// 仅接受纯 ASCII 数字（最多一个 `.` 表示位号，如 `40001.1`）；`192.168.1.10:502` /
/// `COM1` / `opc.tcp://...` 等含字母或冒号的合法端点一律返回 `false`，避免误报。
/// 不依赖 driver 解析器（保持 config 模块零 driver 依赖）；语义误判只影响告警、
/// 不影响任何功能（端点解析延迟到 `poll` 时显式报错）。
fn looks_like_register(raw: &str) -> bool {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return false;
    }
    let mut seen_dot = false;
    for ch in trimmed.chars() {
        if ch == '.' {
            if seen_dot {
                return false; // 多个点号 → 是 IP / URL，不是寄存器。
            }
            seen_dot = true;
        } else if !ch.is_ascii_digit() {
            return false; // 含任何非数字非单点 → 不是寄存器。
        }
    }
    true
}

/// 启动期存量检测：对 `address` 长得像寄存器号、且未显式声明 `endpoint` 的点位行
/// 打 `warn!` 并给出可操作文案。**绝不改写配置**——`persist_config` 会在热重载 / 持久化
/// 时把内存纠正值写回 `config.toml`，静默改用户资产；故此处只告警、不动数据。
fn warn_inverted_points(config: &GatewayConfig) {
    for point in &config.points {
        if looks_like_register(&point.address) && point.endpoint.is_none() {
            tracing::warn!(
                target: "daemon::config",
                device_id = %point.device_id,
                point_id = %point.point_id,
                address = %point.address,
                "point row `address`={:?} looks like a register number (e.g. 40001); in V1 the \
                 `address`/endpoint field is the DEVICE ACCESS ENDPOINT (host:port), while \
                 `point_id` holds the register. This row will fail to resolve at poll time. \
                 Fix manually: set `address`/`endpoint` to the device endpoint (e.g. \
                 192.168.1.10:502) and `point_id` to the register (e.g. 40001). Config is NOT \
                 auto-migrated.",
                point.address
            );
        }
    }
}

/// 原子写文件：同目录临时文件 + fsync + rename（与 `backup_before_rewrite`
/// 同一原子性口径；失败清理半成品临时文件，绝不 panic）。
fn atomic_write(path: &Path, data: &[u8]) -> DaemonResult<()> {
    use std::io::Write as IoWrite;
    let dir = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .map_or_else(|| PathBuf::from("."), Path::to_path_buf);
    let file_name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("config")
        .to_string();
    let tmp = dir.join(format!(".{file_name}.tmp-{}", std::process::id()));
    let write_result = (|| -> std::io::Result<()> {
        let mut file = std::fs::File::create(&tmp)?;
        file.write_all(data)?;
        file.sync_all()?;
        Ok(())
    })();
    if let Err(e) = write_result {
        let _ = std::fs::remove_file(&tmp);
        return Err(DaemonError::StorageError(format!(
            "atomic write {}: {e}",
            tmp.display()
        )));
    }
    if let Err(e) = std::fs::rename(&tmp, path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(DaemonError::StorageError(format!(
            "atomic write rename {} -> {}: {e}",
            tmp.display(),
            path.display()
        )));
    }
    Ok(())
}

/// 共享配置快照：读侧拿 `Arc<GatewayConfig>` 快照 + 版本号探测变更。
pub struct ConfigShared {
    config: RwLock<Arc<GatewayConfig>>,
    version: AtomicU64,
}

impl ConfigShared {
    /// 以初始配置创建（版本号 1）。
    pub fn new(config: GatewayConfig) -> Self {
        Self {
            config: RwLock::new(Arc::new(config)),
            version: AtomicU64::new(1),
        }
    }

    /// 当前配置快照（锁中毒时退回底层数据，读路径不 panic）。
    pub fn snapshot(&self) -> Arc<GatewayConfig> {
        self.config
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    /// 当前配置版本号（每次成功热重载 +1）。
    pub fn version(&self) -> u64 {
        self.version.load(Ordering::Acquire)
    }

    /// 原子替换快照并递增版本，返回新版本号。
    fn store(&self, config: GatewayConfig) -> u64 {
        let mut guard = self
            .config
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        *guard = Arc::new(config);
        drop(guard);
        self.version.fetch_add(1, Ordering::AcqRel) + 1
    }

    /// 原子替换快照并递增版本（管理面写路径专用）：`GatewayConfig::save`
    /// 落盘成功后调用，读侧**即时**可见新配置，无需等待 notify 防抖窗口。
    /// 随后 notify watch 对磁盘的重读为同内容幂等重放（版本再 +1，无害）。
    pub fn replace(&self, config: GatewayConfig) -> u64 {
        self.store(config)
    }
}

/// 热重载准入闸门（授权配额门控的注入点）：对**解析成功**的候选配置做终检，
/// `Err(reason)` = 拒绝本次重载（保留旧快照、版本号不变、`warn!` 记录原因）。
///
/// bootstrap 用 `LicenseRuntime::enforce_free_limits` 充当本闸门——免费版（Degraded）
/// 期间，设备数 / 协议 / 采集间隔超限的热重载一律拒绝（fail-closed，可解释）。
pub type ReloadGate = Arc<dyn Fn(&GatewayConfig) -> Result<(), String> + Send + Sync>;

/// 配置文件热重载器（notify + 防抖 + 后台线程）。
///
/// 生命周期：`spawn` 后台线程消费文件事件 → 防抖窗口静默后整文件重解析 →
/// 成功则替换快照并 `info!` 记录新版本，失败则 `warn!` 保留旧快照；
/// `stop()` 请求退出并回收线程。
pub struct ConfigHotReloader {
    /// 持有 watcher 以维持监听；drop watcher 会停止接收事件（故不允许被读取消除）。
    #[allow(dead_code)]
    watcher: notify::RecommendedWatcher,
    stop: Arc<std::sync::atomic::AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl ConfigHotReloader {
    /// 加载初始配置、启动监听与后台重载线程（无准入闸门）。
    ///
    /// # Errors
    /// 初始加载失败或 watcher 初始化失败时返回 [`DaemonError`]（此时未产生后台线程）。
    pub fn spawn(path: impl Into<PathBuf>) -> DaemonResult<(Self, Arc<ConfigShared>)> {
        Self::spawn_with_gate(path, None)
    }

    /// 加载初始配置、启动监听与后台重载线程，并注入热重载准入闸门。
    ///
    /// 每次文件重载解析成功后先过闸门（[`ReloadGate`]）；被拒则保留旧快照
    /// （版本号不变）并 `warn!` 拒绝原因——免费版配额在运行期热加载同样 fail-closed。
    ///
    /// # Errors
    /// 初始加载失败或 watcher 初始化失败时返回 [`DaemonError`]（此时未产生后台线程）。
    pub fn spawn_with_gate(
        path: impl Into<PathBuf>,
        gate: Option<ReloadGate>,
    ) -> DaemonResult<(Self, Arc<ConfigShared>)> {
        let path: PathBuf = path.into();
        // 初始加载失败直接返回错误：调用方据此走安全模式（task 55），绝不静默空配置。
        let initial = GatewayConfig::load(&path)?;
        let shared = Arc::new(ConfigShared::new(initial));

        let watch_dir = path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .map_or_else(|| PathBuf::from("."), ToOwned::to_owned);
        let target = path.clone();
        let (event_tx, event_rx) = std::sync::mpsc::channel::<()>();

        let mut watcher =
            notify::recommended_watcher(move |res: Result<notify::Event, notify::Error>| {
                let Ok(event) = res else { return };
                // 只对目标文件本身的修改类事件触发（目录监听下过滤无关文件）。
                let hit = event.paths.iter().any(|p| p == &target);
                if hit && event.kind.is_modify() {
                    // 通道关闭（线程已退出）时静默丢弃。
                    let _ = event_tx.send(());
                }
            })
            .map_err(DaemonError::from)?;
        watcher
            .watch(&watch_dir, notify::RecursiveMode::NonRecursive)
            .map_err(DaemonError::from)?;

        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let thread_path = path.clone();
        let thread_stop = stop.clone();
        let thread_shared = shared.clone();
        let handle = std::thread::Builder::new()
            .name("config-hot-reload".to_string())
            .spawn(move || {
                reload_loop(thread_path, thread_shared, thread_stop, event_rx, gate);
            })
            .map_err(|e| DaemonError::ConfigError(format!("spawn reload thread: {e}")))?;

        Ok((
            Self {
                watcher,
                stop,
                handle: Some(handle),
            },
            shared,
        ))
    }

    /// 请求后台线程退出并等待结束。
    pub fn stop(mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

impl Drop for ConfigHotReloader {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

/// 防抖重载主循环：静默窗口后整文件重解析；解析成功先过准入闸门再替换快照。
fn reload_loop(
    path: PathBuf,
    shared: Arc<ConfigShared>,
    stop: Arc<std::sync::atomic::AtomicBool>,
    event_rx: std::sync::mpsc::Receiver<()>,
    gate: Option<ReloadGate>,
) {
    let mut pending = false;
    let mut last_event: Option<Instant> = None;
    loop {
        if stop.load(Ordering::Acquire) {
            return;
        }
        match event_rx.recv_timeout(POLL_INTERVAL) {
            Ok(()) => {
                pending = true;
                last_event = Some(Instant::now());
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => return,
        }
        let Some(last) = last_event else { continue };
        if pending && last.elapsed() >= DEBOUNCE_WINDOW {
            pending = false;
            last_event = None;
            match GatewayConfig::load(&path) {
                Ok(config) => {
                    // 准入闸门（授权配额，fail-closed）：被拒则保留旧快照，
                    // 版本号不变，原因可解释（含恢复路径），绝不 panic。
                    if let Some(gate) = &gate {
                        if let Err(reason) = gate(&config) {
                            tracing::warn!(
                                reason = %reason,
                                path = %path.display(),
                                "config hot-reload rejected by license quota gate; \
                                 keeping previous config"
                            );
                            continue;
                        }
                    }
                    let version = shared.store(config);
                    tracing::info!(
                        version,
                        path = %path.display(),
                        "config hot-reloaded"
                    );
                }
                Err(err) => {
                    // 失败保留旧快照（版本号不变），绝不 panic、不中断采集。
                    tracing::warn!(
                        error = %err,
                        path = %path.display(),
                        "config hot-reload failed; keeping previous config"
                    );
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rules::{Action, CmpOp, Condition, ConditionValue};

    const EXAMPLE_TOML: &str = r#"
[gateway]
gateway_id = "gw-alpha"
data_dir = "data"

[gateway.licensing]
cloud_url = "https://licensing.example.com"
heartbeat_interval_secs = 86400
# ⚠️ 激活码是敏感凭据：此处只允许占位符；真实值经 env IOTDAQ_ACTIVATION_CODE 注入。
activation_code = "IOTDAQ-0000-0000-0000-0000"
trial_days = 3
grace_days = 7

[gateway.cache]
sqlite_path = "cache.db"
max_size_mb = 512
retention_days = 7

[gateway.security]
web_auth_enabled = true

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
"#;

    /// QA: 示例 TOML（1 个 Modbus 设备 + MQTT 配置）解析成功，协议 Modbus、频率 100ms。
    #[test]
    fn parse_example_config() {
        let config = GatewayConfig::parse(EXAMPLE_TOML).expect("parse ok");
        assert_eq!(config.gateway.gateway_id, "gw-alpha");
        assert_eq!(
            config.gateway.licensing.cloud_url.as_deref(),
            Some("https://licensing.example.com")
        );
        // task 19 尾巴：授权配置字段补齐（激活码 / 试用 / 宽限）。
        assert_eq!(
            config.gateway.licensing.activation_code.as_deref(),
            Some("IOTDAQ-0000-0000-0000-0000"),
            "activation_code must be configurable (placeholder in examples)"
        );
        assert_eq!(config.gateway.licensing.trial_days, 3);
        assert_eq!(config.gateway.licensing.grace_days, 7);
        assert_eq!(config.gateway.cache.retention_days, 7);
        assert!(config.gateway.security.web_auth_enabled);

        assert_eq!(config.outlets.len(), 2);
        let north1 = &config.outlets[0];
        assert_eq!(north1.qos, 1);
        assert!(north1.tls);
        assert_eq!(north1.encoding, OutletEncoding::Protobuf);
        // 每路出口独立编码：north-2 未写 encoding → 默认 protobuf？不——显式 json。
        assert_eq!(config.outlets[1].encoding, OutletEncoding::Json);

        let point = config
            .points
            .iter()
            .find(|p| p.device_id == "dev-01")
            .expect("modbus device row");
        assert_eq!(point.protocol, "modbus-tcp");
        assert_eq!(point.frequency_ms, 100);
    }

    /// 缺省值兜底：空 TOML 也能得到全默认配置（不 panic）。
    #[test]
    fn defaults_apply_for_empty_document() {
        let config = GatewayConfig::parse("").expect("empty toml defaults");
        assert_eq!(config.gateway.gateway_id, "gw-unset");
        assert_eq!(config.gateway.cache.retention_days, 7);
        assert!(config.outlets.is_empty());
        assert!(config.points.is_empty());
        // task 57：mgmt_auth 可选段缺省 = None（既有配置语义不变）。
        assert!(config.mgmt_auth.is_none(), "mgmt_auth must default to None");
        // task 19 尾巴：授权配置新字段缺省（向后兼容：老配置文件不含这些键也能解析）。
        let licensing = &config.gateway.licensing;
        assert_eq!(licensing.activation_code, None);
        assert_eq!(
            licensing.trial_days, 3,
            "trial_days default = 3 (TRIAL_DAYS)"
        );
        assert_eq!(
            licensing.grace_days, 7,
            "grace_days default = 7 (GRACE_DAYS)"
        );
    }

    /// OTA 配置层（`[gateway.ota]`）：
    /// 1. 缺省 = enabled=false / 无 URL / 无公钥 / 周期 3600 / current_version 0，
    ///    且 `is_configured()` 为 false（未配置 → 不动作）；
    /// 2. 显式段解析出各字段；
    /// 3. **enabled=true 但缺 URL / 缺公钥** → 仍判定「未配置」（诚实降级，不 panic）。
    #[test]
    fn ota_section_defaults_and_unconfigured_detection() {
        // 1) 空文档 → OTA 全缺省。
        let config = GatewayConfig::parse("").expect("empty toml defaults");
        let ota = &config.gateway.ota;
        assert_eq!(
            ota,
            &OtaSection::default(),
            "缺省等于 OtaSection::default()"
        );
        assert!(!ota.enabled, "OTA 缺省关闭");
        assert_eq!(ota.manifest_url, None);
        assert_eq!(ota.signing_key_b64, None);
        assert_eq!(ota.poll_interval_secs, 3_600, "缺省轮询周期为保守值 3600s");
        assert_eq!(ota.current_version, 0);
        assert!(!ota.is_configured(), "空配置 = 未配置");

        // 2) 显式段解析。
        let parsed = GatewayConfig::parse(
            r#"
[gateway.ota]
enabled = true
manifest_url = "http://licensing.internal:7080/updates/manifest"
poll_interval_secs = 60
signing_key_b64 = "AAAA"
current_version = 7
"#,
        )
        .expect("parse ota section");
        let ota = parsed.gateway.ota;
        assert!(ota.enabled);
        assert_eq!(
            ota.manifest_url.as_deref(),
            Some("http://licensing.internal:7080/updates/manifest")
        );
        assert_eq!(ota.poll_interval_secs, 60);
        assert_eq!(ota.signing_key_b64.as_deref(), Some("AAAA"));
        assert_eq!(ota.current_version, 7);
        assert!(ota.is_configured(), "三要素齐备 = 已配置");

        // 3) enabled=true 但 URL 为空 / 全空白 → 未配置；缺公钥同样未配置。
        let no_url = OtaSection {
            enabled: true,
            manifest_url: Some("   ".to_string()),
            signing_key_b64: Some("AAAA".to_string()),
            ..OtaSection::default()
        };
        assert!(!no_url.is_configured(), "空白 URL = 未配置");
        assert!(no_url.config_hint().contains("[gateway.ota]"));

        let no_key = OtaSection {
            enabled: true,
            manifest_url: Some("http://host/updates/manifest".to_string()),
            signing_key_b64: None,
            ..OtaSection::default()
        };
        assert!(!no_key.is_configured(), "缺公钥 = 未配置");
    }

    /// 激活码脱敏纪律（task 19 尾巴）：
    /// 1. `Debug` 输出只含 `<redacted>`，**绝不**含激活码原文（防日志泄露）；
    /// 2. `Serialize` 输出（配置导出 / API 回显）**不含** `activation_code` 键。
    #[test]
    fn activation_code_is_redacted_in_debug_and_serialize() {
        let section = LicensingSection {
            activation_code: Some("IOTDAQ-SECRETCODE-DO-NOT-LOG".to_string()),
            ..LicensingSection::default()
        };
        let dbg = format!("{section:?}");
        assert!(dbg.contains("<redacted>"), "Debug must redact: {dbg}");
        assert!(
            !dbg.contains("IOTDAQ-SECRETCODE"),
            "Debug leaked activation code: {dbg}"
        );

        let json = serde_json::to_string(&section).expect("serialize licensing section");
        assert!(
            !json.contains("activation_code") && !json.contains("IOTDAQ-SECRETCODE"),
            "Serialize leaked activation code: {json}"
        );
    }

    /// QA: task 57 可选 `[mgmt_auth]` 段解析——users 数组逐行承接
    /// name / role / password_hash；无该段仍为 None（向后兼容）。
    #[test]
    fn mgmt_auth_section_parses_optional_users() {
        // users 缺失 → 空列表（serde(default)），段本身存在。
        let config = GatewayConfig::parse("[mgmt_auth]").expect("empty mgmt_auth section");
        let section = config.mgmt_auth.expect("section present");
        assert!(section.users.is_empty());

        let config = GatewayConfig::parse(
            r#"
[[mgmt_auth.users]]
name = "alice"
role = "system"
password_hash = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"

[[mgmt_auth.users]]
name = "bob"
role = "ops"
password_hash = "aa7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
"#,
        )
        .expect("mgmt_auth users parse");
        let section = config.mgmt_auth.expect("section present");
        assert_eq!(section.users.len(), 2);
        assert_eq!(section.users[0].name, "alice");
        assert_eq!(section.users[0].role, "system");
        assert_eq!(
            section.users[0].password_hash,
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(section.users[1].role, "ops");
    }

    /// 非法 TOML 返回 ConfigError 而非 panic。
    #[test]
    fn invalid_toml_is_an_error() {
        let err = GatewayConfig::parse("[gateway\ngateway_id = ").expect_err("must fail");
        assert!(matches!(err, DaemonError::ConfigError(_)));
        assert_eq!(err.error_code(), crate::error::ERR_CONFIG);
    }

    // ---- D-08：data_dir 缺省随 env `IOT_DAQ_DATA_DIR` 落持久卷 ----

    /// data_dir 缺省解析（纯函数）：env 存在且非空白 → env 值；否则本地默认相对 `data`。
    #[test]
    fn data_dir_env_resolution_covers_all_branches() {
        assert_eq!(
            data_dir_from_env(None),
            PathBuf::from("data"),
            "unset → local default"
        );
        assert_eq!(
            data_dir_from_env(Some(String::new())),
            PathBuf::from("data"),
            "empty → local default"
        );
        assert_eq!(
            data_dir_from_env(Some("   ".to_string())),
            PathBuf::from("data"),
            "blank → local default"
        );
        assert_eq!(
            data_dir_from_env(Some("/var/lib/iot-daq".to_string())),
            PathBuf::from("/var/lib/iot-daq"),
            "env value wins"
        );
        assert_eq!(
            data_dir_from_env(Some(" /var/lib/iot-daq \n".to_string())),
            PathBuf::from("/var/lib/iot-daq"),
            "surrounding whitespace trimmed"
        );
    }

    /// 集成回归：env 注入后，空 TOML（data_dir 键缺省 → serde default）解析出的
    /// `gateway.data_dir` 指向 env 值——设备密钥 / 审计库 / 试用标记随之落持久卷。
    /// CI 不应设置该变量；被占用时跳过（避免污染并行测试）。
    #[test]
    fn data_dir_env_wins_over_local_default_in_parsed_config() {
        if std::env::var(DATA_DIR_ENV).is_ok() {
            return;
        }
        std::env::set_var(DATA_DIR_ENV, "/var/lib/iot-daq");
        let config = GatewayConfig::parse("").expect("empty toml defaults");
        std::env::remove_var(DATA_DIR_ENV);
        assert_eq!(
            config.gateway.data_dir,
            PathBuf::from("/var/lib/iot-daq"),
            "env-injected data_dir must flow into the parsed config"
        );
    }

    // ---- D-07：随镜像分发的默认配置模板逐行 schema 对齐 ----

    /// 默认配置模板（deploy/docker/config/gateway.default.toml）必须整体落在
    /// `GatewayConfig` schema 内：解析成功 + 关键字段符合预期 + 无惰性未知段。
    /// （D-07 修复前模板的 [storage]/[fingerprint]/[license]/[logging]/[integrity]/
    /// [serial] 段全部被 serde 静默忽略。）
    #[test]
    fn default_template_parses_within_schema() {
        const TEMPLATE: &str = include_str!("../../../deploy/docker/config/gateway.default.toml");
        let config =
            GatewayConfig::parse(TEMPLATE).expect("deploy template must parse into GatewayConfig");

        assert_eq!(config.gateway.gateway_id, "gw-unset");
        assert_eq!(
            config.gateway.data_dir,
            PathBuf::from("/var/lib/iot-daq"),
            "container data_dir must point at the persistent volume mount"
        );

        // 授权段：字段名与 schema 一致；模板不内置激活码（真实值经 env 注入，
        // 占位符也绝不默认启用——未配置 = NotConfigured，保持既有行为）。
        let licensing = &config.gateway.licensing;
        assert_eq!(licensing.cloud_url.as_deref(), Some(""));
        assert_eq!(licensing.heartbeat_interval_secs, 86_400);
        assert_eq!(
            licensing.activation_code, None,
            "template must not enable cloud activation by default"
        );
        assert_eq!(licensing.trial_days, 3);
        assert_eq!(licensing.grace_days, 7);

        // 缓存 / 安全段在 [gateway] 命名空间内。
        assert_eq!(config.gateway.cache.sqlite_path, "cache.db");
        assert_eq!(config.gateway.cache.max_size_mb, 512);
        assert_eq!(config.gateway.cache.retention_days, 7);
        assert!(config.gateway.security.web_auth_enabled);

        // 模板不含任何出口 / 点位 / 设备登记 / 凭证实例（注释示例不入 schema）。
        assert!(config.outlets.is_empty());
        assert!(config.points.is_empty());
        assert!(config.devices.is_empty());
        assert!(config.mgmt_auth.is_none());
    }

    /// QA: 从文件加载示例配置。
    #[test]
    fn load_from_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("config.toml");
        std::fs::write(&path, EXAMPLE_TOML).expect("write");
        let config = GatewayConfig::load(&path).expect("load ok");
        assert_eq!(config.points[0].frequency_ms, 100);
    }

    /// QA: 热重载生效——修改文件后版本号递增、新频率可见。
    #[test]
    fn hot_reload_picks_up_changes() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("config.toml");
        std::fs::write(&path, EXAMPLE_TOML).expect("write");

        let (reloader, shared) = ConfigHotReloader::spawn(&path).expect("spawn");
        assert_eq!(shared.version(), 1, "initial load is version 1");

        let updated = EXAMPLE_TOML.replace("frequency_ms = 100", "frequency_ms = 250");
        std::fs::write(&path, updated).expect("rewrite");

        let deadline = Instant::now() + Duration::from_secs(10);
        while shared.version() < 2 {
            assert!(
                Instant::now() < deadline,
                "hot reload did not trigger within 10s (version={})",
                shared.version()
            );
            std::thread::sleep(Duration::from_millis(50));
        }
        let snapshot = shared.snapshot();
        assert_eq!(snapshot.points[0].frequency_ms, 250);
        drop(reloader);
    }

    /// QA 错误路径: 热重载遇到非法配置 → 保留旧快照、版本号不变、不 panic。
    #[test]
    fn hot_reload_failure_keeps_previous_config() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("config.toml");
        std::fs::write(&path, EXAMPLE_TOML).expect("write");

        let (reloader, shared) = ConfigHotReloader::spawn(&path).expect("spawn");
        let version_before = shared.version();

        std::fs::write(&path, "[gateway\ngateway_id = ").expect("write broken toml");
        // 防抖 300ms + 余量：给失败重载足够时间发生。
        std::thread::sleep(Duration::from_millis(1200));

        assert_eq!(
            shared.version(),
            version_before,
            "failed reload must not bump version"
        );
        let snapshot = shared.snapshot();
        assert_eq!(
            snapshot.points[0].frequency_ms, 100,
            "previous config must survive"
        );
        drop(reloader);
    }

    /// QA: 热重载准入闸门（授权配额，fail-closed）—— 闸门拒绝期间版本号不变、
    /// 旧快照保留；闸门放行后（同一文件再触发一次事件）重载成功、版本号递增。
    #[test]
    fn hot_reload_gate_rejects_then_accepts_on_recovery() {
        use std::sync::atomic::{AtomicBool, Ordering as AtomicOrdering};

        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("config.toml");
        std::fs::write(&path, EXAMPLE_TOML).expect("seed");

        // 模拟「免费版（Degraded）→ 激活恢复」的闸门状态翻转。
        let degraded = Arc::new(AtomicBool::new(true));
        let gate: ReloadGate = {
            let degraded = Arc::clone(&degraded);
            Arc::new(move |_config| {
                if degraded.load(AtomicOrdering::SeqCst) {
                    Err(
                        "free-edition quota exceeded (license degraded): devices: 9 > 8 — \
                         activate a license to lift the limits"
                            .to_string(),
                    )
                } else {
                    Ok(())
                }
            })
        };

        let (reloader, shared) =
            ConfigHotReloader::spawn_with_gate(&path, Some(gate)).expect("spawn with gate");
        let version_before = shared.version();

        // 闸门拒绝：合法 TOML 但被闸门否决 → 版本号不变、旧快照保留。
        std::fs::write(
            &path,
            EXAMPLE_TOML.replace("frequency_ms = 100", "frequency_ms = 250"),
        )
        .expect("write candidate");
        std::thread::sleep(Duration::from_millis(1200));
        assert_eq!(
            shared.version(),
            version_before,
            "gate-rejected reload must not bump version"
        );
        assert_eq!(
            shared.snapshot().points[0].frequency_ms,
            100,
            "previous config must survive gate rejection"
        );

        // 授权恢复（Degraded → Licensed）后再次触发文件事件 → 同一候选配置过闸。
        degraded.store(false, AtomicOrdering::SeqCst);
        std::fs::write(
            &path,
            EXAMPLE_TOML.replace("frequency_ms = 100", "frequency_ms = 250"),
        )
        .expect("rewrite candidate");
        let deadline = Instant::now() + Duration::from_secs(10);
        while shared.version() == version_before && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(
            shared.version() > version_before,
            "gate-accepted reload must bump version"
        );
        assert_eq!(
            shared.snapshot().points[0].frequency_ms,
            250,
            "new config must be visible after gate acceptance"
        );
        drop(reloader);
    }

    /// QA: 管理面写路径 `save()`——落盘内容可往返解析、写前备份文件产生且
    /// 内容为写前快照、临时半成品不残留。
    #[test]
    fn save_roundtrips_and_creates_backup_snapshot() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("config.toml");
        std::fs::write(&path, EXAMPLE_TOML).expect("seed");
        let original_raw = EXAMPLE_TOML.to_string();

        let mut config = GatewayConfig::load(&path).expect("load");
        // 模拟管理面写操作：追加设备登记。
        config.devices.push(DeviceConfig {
            device_id: "dev-02".to_string(),
            name: Some("二号设备".to_string()),
            enabled: false,
            protocol: Some("s7".to_string()),
            group_id: None,
            endpoint: None,
        });
        config.save(&path).expect("save");

        // 落盘内容可重新解析且语义一致。
        let reloaded = GatewayConfig::load(&path).expect("reload");
        assert_eq!(reloaded.devices.len(), 1);
        assert_eq!(reloaded.devices[0].device_id, "dev-02");
        assert!(!reloaded.devices[0].enabled);
        assert_eq!(reloaded.devices[0].protocol.as_deref(), Some("s7"));
        assert_eq!(reloaded.points[0].frequency_ms, 100);
        assert_eq!(reloaded.gateway.gateway_id, "gw-alpha");

        // 写前备份存在且逐字节为写前快照。
        let backups: Vec<_> = std::fs::read_dir(dir.path())
            .expect("read_dir")
            .filter_map(|e| e.ok())
            .filter(|e| {
                let name = e.file_name();
                crate::migrations::is_backup_file_name(&name.to_string_lossy(), "config.toml")
            })
            .collect();
        assert!(!backups.is_empty(), "backup file must be created");
        let backup_raw = std::fs::read_to_string(backups[0].path()).expect("read backup");
        assert_eq!(
            backup_raw, original_raw,
            "backup must be the pre-write snapshot"
        );

        // 无 .tmp 半成品残留。
        let leftovers: Vec<_> = std::fs::read_dir(dir.path())
            .expect("read_dir")
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().contains(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "no tmp leftovers: {leftovers:?}");
    }

    // ---- B-2：备份策略 ----

    /// QA（B-2）: `[settings.backup_policy]` 向后兼容——旧配置（无 settings 段）
    /// 解析为默认策略（auto=true / retention=20 / interval=0）；显式段可覆盖；
    /// retention=0 语义合法（不清理）。
    #[test]
    fn backup_policy_defaults_and_override() {
        // 旧配置无 [settings] 段 → serde default。
        let config = GatewayConfig::parse(EXAMPLE_TOML).expect("parse legacy");
        let policy = &config.settings.backup_policy;
        assert!(
            policy.auto_before_write,
            "auto backup defaults on (现状显式化)"
        );
        assert_eq!(policy.retention_count, 20, "retention defaults to 20");
        assert_eq!(policy.interval_min, 0, "periodic backup defaults off");

        // 显式段覆盖。
        let config = GatewayConfig::parse(
            "[settings.backup_policy]\nauto_before_write = false\nretention_count = 3\ninterval_min = 15\n",
        )
        .expect("parse explicit policy");
        let policy = &config.settings.backup_policy;
        assert!(!policy.auto_before_write);
        assert_eq!(policy.retention_count, 3);
        assert_eq!(policy.interval_min, 15);
    }

    /// QA（更新源声明）: 旧配置无 `[settings.updates]` → 默认**未配置**（不臆造源）；
    /// 显式段可解析；未配置时序列化省略 `updates` 键（旧 daemon 可读新文件）。
    #[test]
    fn updates_section_defaults_unconfigured_and_parses() {
        // 缺省 = 未配置。
        let config = GatewayConfig::parse(EXAMPLE_TOML).expect("parse legacy");
        assert!(config.settings.updates.source_url.is_none());
        assert!(config.settings.updates.signing_key.is_none());

        // 显式配置可解析。
        let config = GatewayConfig::parse(
            "[settings.updates]\nsource_url = \"https://ota.example.com/gw\"\n\
             signing_key = \"deadbeef\"\n",
        )
        .expect("parse updates");
        assert_eq!(
            config.settings.updates.source_url.as_deref(),
            Some("https://ota.example.com/gw")
        );
        assert_eq!(
            config.settings.updates.signing_key.as_deref(),
            Some("deadbeef")
        );

        // 未配置 → 序列化省略该段（旧 daemon 仍可读新写出的文件）。
        let raw = toml::to_string_pretty(&config).expect("serialize");
        assert!(
            raw.contains("source_url"),
            "configured section emitted: {raw}"
        );
        let legacy = GatewayConfig::parse(EXAMPLE_TOML).expect("parse legacy again");
        let raw = toml::to_string_pretty(&legacy).expect("serialize legacy");
        assert!(
            !raw.contains("updates"),
            "unconfigured updates section must be omitted: {raw}"
        );
    }

    /// QA（B-2）: `auto_before_write = false` → save() 不再生成写前备份；
    /// `retention_count` 超限的旧 `.bak-*` 被清理（只清本服务自产前缀文件，
    /// 用户自建文件不动）。
    #[test]
    fn save_honors_backup_policy_and_retention() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("config.toml");
        std::fs::write(&path, EXAMPLE_TOML).expect("seed");

        // 造 3 个假旧备份（自产前缀）+ 1 个用户自建文件（不同前缀）。
        for name in [
            "config.toml.bak-1000",
            "config.toml.bak-2000",
            "config.toml.bak-3000",
            "config.toml.mybak-keepme",
        ] {
            std::fs::write(dir.path().join(name), b"old").expect("seed backup");
        }

        let mut config = GatewayConfig::load(&path).expect("load");
        config.settings.backup_policy.auto_before_write = true;
        config.settings.backup_policy.retention_count = 2; // 3 旧 + 1 新 → 保留最新 2
        config.gateway.gateway_id = "gw-policy-test".to_string();
        config.save(&path).expect("save");

        let mut bak: Vec<String> = std::fs::read_dir(dir.path())
            .expect("read_dir")
            .filter_map(|e| e.ok())
            .filter_map(|e| e.file_name().to_str().map(ToString::to_string))
            .filter(|n| crate::migrations::is_backup_file_name(n, "config.toml"))
            .collect();
        bak.sort();
        assert_eq!(
            bak.len(),
            2,
            "retention keeps newest 2 (incl. fresh): {bak:?}"
        );
        // 保留的是「最新两份」（写前快照 + bak-3000），最旧的 1000/2000 已删。
        assert!(
            !bak.contains(&"config.toml.bak-1000".to_string())
                && !bak.contains(&"config.toml.bak-2000".to_string()),
            "oldest backups pruned: {bak:?}"
        );
        // 用户自建文件绝不被清理。
        assert!(
            dir.path().join("config.toml.mybak-keepme").exists(),
            "user files must never be touched"
        );

        // auto_before_write = false → 下一次 save 不再新增备份。
        config.settings.backup_policy.auto_before_write = false;
        config.settings.backup_policy.retention_count = 0; // 0 = 不清理
        config.gateway.gateway_id = "gw-policy-test-2".to_string();
        let count_before = bak.len();
        config.save(&path).expect("save again");
        let bak_after: Vec<_> = std::fs::read_dir(dir.path())
            .expect("read_dir")
            .filter_map(|e| e.ok())
            .filter_map(|e| e.file_name().to_str().map(ToString::to_string))
            .filter(|n| crate::migrations::is_backup_file_name(n, "config.toml"))
            .collect();
        assert_eq!(
            bak_after.len(),
            count_before,
            "auto_before_write=false must skip write-before-backup"
        );
    }

    /// QA: `[[devices]]` 可选段向后兼容——旧配置（无 devices 段）解析为空列表；
    /// 空列表序列化时省略该键（旧版本 daemon 仍可读新写出的文件）。
    #[test]
    fn devices_section_is_backward_compatible() {
        // 旧配置无 devices 段 → serde default 空列表。
        let config = GatewayConfig::parse(EXAMPLE_TOML).expect("parse legacy config");
        assert!(
            config.devices.is_empty(),
            "legacy config without [[devices]] must default to empty"
        );

        // 空列表序列化时省略 devices 键。
        let raw = toml::to_string_pretty(&config).expect("serialize");
        assert!(
            !raw.contains("devices"),
            "empty devices must be skipped when serializing: {raw}"
        );

        // 带 devices 段的配置序列化 → `[[devices]]` 数组表。
        let with_devices =
            GatewayConfig::parse("[[devices]]\ndevice_id = \"dev-x\"\nenabled = true\n")
                .expect("parse devices");
        let raw = toml::to_string_pretty(&with_devices).expect("serialize");
        assert!(
            raw.contains("[[devices]]"),
            "devices section emitted: {raw}"
        );
        assert!(raw.contains("device_id = \"dev-x\""));
        // mgmt_auth None 不产生空表垃圾。
        assert!(!raw.contains("mgmt_auth"));
    }

    /// task 25：`[[outlets]]` 的 TLS / mTLS 字段可解析（CA / 客户端证书 / 私钥 /
    /// SNI / ALPN）。
    #[test]
    fn config_outlet_tls_fields_parse() {
        let raw = r#"
[[outlets]]
name = "tls-1"
broker = "mqtts://broker.local:8883"
tls = true
ca_cert_path = "/certs/ca.crt"
client_cert_path = "/certs/client.crt"
client_key_path = "/certs/client.key"
server_name = "broker.local"
alpn = ["mqtt"]
"#;
        let config = GatewayConfig::parse(raw).expect("parse tls outlet");
        assert_eq!(config.outlets.len(), 1);
        let outlet = &config.outlets[0];
        assert!(outlet.tls);
        assert_eq!(outlet.ca_cert_path.as_deref(), Some("/certs/ca.crt"));
        assert_eq!(
            outlet.client_cert_path.as_deref(),
            Some("/certs/client.crt")
        );
        assert_eq!(outlet.client_key_path.as_deref(), Some("/certs/client.key"));
        assert_eq!(outlet.server_name.as_deref(), Some("broker.local"));
        assert_eq!(outlet.alpn, vec!["mqtt".to_string()]);
    }

    /// task 25：无 TLS 字段的旧 `[[outlets]]` 仍可解析（向后兼容），缺省为
    /// `None` / 空 ALPN；且未设置字段在序列化时省略（旧 daemon 仍可读新文件）。
    #[test]
    fn config_outlet_tls_fields_backward_compatible() {
        let config =
            GatewayConfig::parse("[[outlets]]\nname = \"legacy\"\nbroker = \"mqtt://h:1883\"\n")
                .expect("parse legacy outlet");
        let outlet = &config.outlets[0];
        assert!(outlet.ca_cert_path.is_none());
        assert!(outlet.client_cert_path.is_none());
        assert!(outlet.client_key_path.is_none());
        assert!(outlet.server_name.is_none());
        assert!(outlet.alpn.is_empty());

        let raw = toml::to_string_pretty(&config).expect("serialize");
        for absent in [
            "ca_cert_path",
            "client_cert_path",
            "client_key_path",
            "server_name",
            "alpn",
        ] {
            assert!(
                !raw.contains(absent),
                "unset `{absent}` must be omitted when serializing: {raw}"
            );
        }
    }

    /// task：北向 MQTT 凭证字段 `username`/`password` 可随 `[[outlets]]` 解析；
    /// 同时验证「不带凭证的旧配置」仍可解析（向后兼容，新字段缺省为 `None`）。
    #[test]
    fn config_outlet_credentials_parse() {
        let raw = r#"
[[outlets]]
name = "cred-1"
broker = "mqtts://broker.local:8883"
tls = true
username = "gw-user"
password = "s3cr3t-password"
"#;
        let config = GatewayConfig::parse(raw).expect("parse credentialed outlet");
        assert_eq!(config.outlets.len(), 1);
        let outlet = &config.outlets[0];
        assert_eq!(outlet.username.as_deref(), Some("gw-user"));
        assert_eq!(outlet.password.as_deref(), Some("s3cr3t-password"));
    }

    /// task：未配置 `username`/`password` 的旧 `[[outlets]]` 仍可解析（向后兼容）；
    /// 凭证字段缺省为 `None`，且序列化时被省略（旧 daemon 仍可读取新文件）。
    #[test]
    fn config_outlet_credentials_backward_compatible() {
        let config =
            GatewayConfig::parse("[[outlets]]\nname = \"legacy\"\nbroker = \"mqtt://h:1883\"\n")
                .expect("parse legacy outlet without credentials");
        let outlet = &config.outlets[0];
        assert!(outlet.username.is_none());
        assert!(outlet.password.is_none());

        let raw = toml::to_string_pretty(&config).expect("serialize");
        assert!(
            !raw.contains("username"),
            "unset `username` must be omitted when serializing: {raw}"
        );
        assert!(
            !raw.contains("password"),
            "unset `password` must be omitted when serializing: {raw}"
        );
    }

    // ---- 需求 1/2/4/6：点位推送开关 / 点位元数据 / 设备分组（向后兼容） ----

    /// 老配置（无新字段）可加载；`push_enabled` 缺省 true、`point_type` 缺省
    /// physical、元数据为 None、`device_groups` 为空 —— 逐条断言向后兼容。
    #[test]
    fn new_point_and_group_fields_are_backward_compatible() {
        let config = GatewayConfig::parse(EXAMPLE_TOML).expect("parse legacy config");
        let point = &config.points[0];
        assert!(
            point.push_enabled,
            "legacy point without `push` must default to push-enabled (true)"
        );
        assert_eq!(point.point_type, "physical", "default point type");
        assert!(point.name.is_none());
        assert!(point.data_type.is_none());
        assert!(point.byte_order.is_none());
        assert!(point.unit.is_none());
        assert!(point.deadband.is_none());
        assert!(point.target_key.is_none());
        assert!(point.formula.is_none());
        assert!(
            config.device_groups.is_empty(),
            "legacy config without [[device_groups]] must default to empty"
        );
        // 空字段序列化时省略（旧 daemon 仍可读新写出的文件）。
        // 注：`push_enabled` / `point_type` 为带缺省的非 Option 字段，恒序列化
        //（值即缺省值，旧 daemon 忽略未知键，向后兼容）。
        let raw = toml::to_string_pretty(&config).expect("serialize");
        for absent in [
            "data_type",
            "byte_order",
            "target_key",
            "formula",
            "device_groups",
        ] {
            assert!(
                !raw.contains(absent),
                "unset `{absent}` must be omitted when serializing: {raw}"
            );
        }
    }

    /// 点位元数据 + 推送开关全字段解析；`deadband` 为 number（非大整数）。
    #[test]
    fn point_metadata_and_push_flag_parse() {
        let raw = r#"
[[points]]
device_id = "dev-01"
point_id = "p_temp"
protocol = "modbus-tcp"
address = "192.168.1.10:502"
frequency_ms = 1000
push = false
name = "炉温"
data_type = "float32"
byte_order = "ABCD"
unit = "degC"
deadband = 0.5
target_key = "temperature"
point_type = "derived"
formula = "p_a + p_b"
"#;
        let config = GatewayConfig::parse(raw).expect("parse point metadata");
        let point = &config.points[0];
        assert!(!point.push_enabled, "push=false honored");
        assert_eq!(point.name.as_deref(), Some("炉温"));
        assert_eq!(point.data_type.as_deref(), Some("float32"));
        assert_eq!(point.byte_order.as_deref(), Some("ABCD"));
        assert_eq!(point.unit.as_deref(), Some("degC"));
        assert_eq!(point.deadband, Some(0.5));
        assert_eq!(point.target_key.as_deref(), Some("temperature"));
        assert_eq!(point.point_type, "derived");
        assert_eq!(point.formula.as_deref(), Some("p_a + p_b"));
        // `push` 为 serde 别名（字段名 push_enabled 的对外/Toml 名）。
        let json = serde_json::to_value(point).expect("serialize point");
        assert_eq!(json["push_enabled"], serde_json::json!(false));
    }

    /// `[[device_groups]]` 段解析 + `DeviceConfig::group_id` 归属解析。
    #[test]
    fn device_groups_section_parses() {
        let raw = r#"
[[device_groups]]
id = "line-a"
name = "A 线"

[[device_groups]]
id = "line-b"
name = "B 线"

[[devices]]
device_id = "dev-01"
protocol = "modbus-tcp"
group_id = "line-a"
"#;
        let config = GatewayConfig::parse(raw).expect("parse device groups");
        assert_eq!(config.device_groups.len(), 2);
        assert_eq!(config.device_groups[0].id, "line-a");
        assert_eq!(config.device_groups[0].name, "A 线");
        assert_eq!(config.device_groups[1].id, "line-b");
        assert_eq!(config.devices[0].group_id.as_deref(), Some("line-a"));
        // 缺省 group_id = None（归属默认分组）。
        let bare = GatewayConfig::parse("[[devices]]\ndevice_id = \"dev-02\"\n")
            .expect("parse bare device");
        assert!(bare.devices[0].group_id.is_none());
    }

    /// `PointConfig::default()` 与 serde 缺省语义一致（push 默认开、类型 physical）。
    #[test]
    fn point_config_default_matches_serde_defaults() {
        let defaulted = PointConfig::default();
        assert!(defaulted.push_enabled);
        assert_eq!(defaulted.point_type, "physical");
        assert_eq!(defaulted.frequency_ms, default_frequency_ms());
        assert!(!defaulted.sim_enabled, "simulation is off by default");
    }

    // ---- 扩展 schema：点位模拟字段 / [[rules]] / [alarms] / [[mgmt_auth.roles]] ----

    /// 点位模拟字段全字段解析 + 老配置缺省（向后兼容）。
    #[test]
    fn point_simulation_fields_parse_and_default_off() {
        let raw = r#"
[[points]]
device_id = "dev-01"
point_id = "p_sim"
protocol = "modbus-tcp"
address = "192.168.1.10:502"
frequency_ms = 1000
sim_enabled = true
sim_min = 0.0
sim_max = 100.5
sim_dec = 2
sim_period_ms = 500
sim_mode = "random"
"#;
        let config = GatewayConfig::parse(raw).expect("parse sim point");
        let point = &config.points[0];
        assert!(point.sim_enabled);
        assert_eq!(point.sim_min, Some(0.0));
        assert_eq!(point.sim_max, Some(100.5));
        assert_eq!(point.sim_dec, Some(2));
        assert_eq!(point.sim_period_ms, Some(500));
        assert_eq!(point.sim_mode.as_deref(), Some("random"));

        // 老点位（无 sim 键）→ 模拟关闭、各字段 None。
        let legacy = GatewayConfig::parse(EXAMPLE_TOML).expect("parse legacy");
        let legacy_point = &legacy.points[0];
        assert!(
            !legacy_point.sim_enabled,
            "legacy point keeps simulation off"
        );
        assert!(legacy_point.sim_min.is_none());
        assert!(legacy_point.sim_max.is_none());
        assert!(legacy_point.sim_dec.is_none());
        assert!(legacy_point.sim_period_ms.is_none());
        assert!(legacy_point.sim_mode.is_none(), "sim_mode defaults to None");
    }

    /// `[[rules]]` 转发规则 schema 解析 + 缺省（priority=0 / enabled=true）+
    /// [`RuleConfig::to_rule`] 引擎转换 + 落盘往返。
    ///
    /// 引擎字段 `when` / `actions` 直接复用 `crate::rules` 的
    /// [`Condition`] / [`Action`] 类型（结构化，**非**字符串描述）。
    #[test]
    fn rules_section_parses_with_defaults() {
        let raw = r#"
[[rules]]
id = "r1"
name = "只转好值"
forwarder_id = "north-1"
priority = 10
enabled = false

[rules.when]
kind = "cmp"
field = "value"
op = "gt"
value = 30

[[rules.actions]]
kind = "publish"
topic = "alarms"

[[rules.actions]]
kind = "remap"

[rules.actions.fields]
t = "$.value"

[[rules]]
id = "r2"
name = "缺省项"
"#;
        let config = GatewayConfig::parse(raw).expect("parse rules");
        assert_eq!(config.rules.len(), 2);
        let r1 = &config.rules[0];
        assert_eq!(r1.id, "r1");
        assert_eq!(r1.forwarder_id.as_deref(), Some("north-1"));
        assert_eq!(r1.priority, 10);
        assert!(!r1.enabled);
        // 引擎字段：cmp 条件 + publish / remap 两种动作。
        assert_eq!(
            r1.when,
            Some(Condition::Cmp {
                field: "value".to_string(),
                op: CmpOp::Gt,
                value: ConditionValue::Num(30.0),
            })
        );
        assert_eq!(
            r1.actions,
            vec![
                Action::Publish {
                    topic: "alarms".to_string()
                },
                Action::Remap {
                    fields: std::collections::BTreeMap::from([(
                        "t".to_string(),
                        "$.value".to_string()
                    )]),
                },
            ]
        );
        // 缺省：priority 0 / enabled true / when None / actions 空。
        let r2 = &config.rules[1];
        assert_eq!(r2.priority, 0);
        assert!(r2.enabled, "rule defaults to enabled");
        assert!(r2.when.is_none());
        assert!(r2.actions.is_empty(), "actions defaults to empty");
        assert!(r2.forwarder_id.is_none());

        // to_rule()：id / when / actions 逐字透传，引擎专有字段取缺省。
        let engine_rule = r1.to_rule();
        assert_eq!(engine_rule.id, "r1");
        assert_eq!(engine_rule.when, r1.when);
        assert_eq!(engine_rule.actions, r1.actions);
        assert!(
            engine_rule.transform.is_none(),
            "transform defaults to None"
        );
        assert!(engine_rule.select.is_empty());
        assert!(engine_rule.depends_on.is_empty());
        let wildcard = r2.to_rule();
        assert!(wildcard.when.is_none(), "no when = wildcard rule");

        // 老配置无 rules 段 → 空列表；空列表序列化时省略。
        let legacy = GatewayConfig::parse(EXAMPLE_TOML).expect("parse legacy");
        assert!(legacy.rules.is_empty());
        let serialized = toml::to_string_pretty(&legacy).expect("serialize");
        assert!(
            !serialized.contains("rules"),
            "empty rules must be skipped when serializing: {serialized}"
        );
        // 结构化字段落盘往返：序列化 → 再解析 → 逐字段等值（同形编码互逆）。
        let roundtrip = toml::to_string_pretty(&config).expect("serialize rules config");
        let reparsed = GatewayConfig::parse(&roundtrip).expect("re-parse own output");
        assert_eq!(reparsed.rules, config.rules, "toml roundtrip lossless");
        // 空引擎字段不得出现在序列化结果里（避免旧版本拒读）。
        let r2_out = toml::to_string_pretty(r2).expect("serialize rule");
        assert!(!r2_out.contains("when"), "None when skipped: {r2_out}");
        assert!(
            !r2_out.contains("actions"),
            "empty actions skipped: {r2_out}"
        );
    }

    /// `[alarms]` + `[[alarms.rules]]` schema 解析 + 缺省（段缺省 None）。
    ///
    /// 覆盖阈值型**引擎字段**（`device_id` / `point_id` / `op` / `threshold` /
    /// `duration_ms` / `suppress_ms`，供 BE-ALARM）与展示字段（`level` /
    /// `source_type` / `condition`）并存，且老配置完全兼容。
    #[test]
    fn alarms_section_parses_and_defaults_to_none() {
        let raw = r#"
[alarms]
enabled = true

[[alarms.rules]]
id = "a1"
name = "设备离线"
level = "major"
source_type = "device"
condition = "status == offline"
device_id = "dev-1"
point_id = "p1"
op = "gt"
threshold = 0.5
duration_ms = 5000
suppress_ms = 60000
enabled = false

[[alarms.rules]]
id = "a2"
"#;
        let config = GatewayConfig::parse(raw).expect("parse alarms");
        let alarms = config.alarms.as_ref().expect("alarms section present");
        assert!(alarms.enabled);
        assert_eq!(alarms.rules.len(), 2);
        let a1 = &alarms.rules[0];
        assert_eq!(a1.id, "a1");
        assert_eq!(a1.level.as_deref(), Some("major"));
        assert_eq!(a1.source_type.as_deref(), Some("device"));
        assert_eq!(a1.device_id.as_deref(), Some("dev-1"));
        assert_eq!(a1.point_id.as_deref(), Some("p1"));
        assert_eq!(a1.op.as_deref(), Some("gt"));
        assert_eq!(a1.threshold, Some(0.5));
        assert_eq!(a1.duration_ms, Some(5000));
        assert_eq!(a1.suppress_ms, Some(60000));
        assert!(!a1.enabled);
        // 缺省：enabled true、可选字段 None。
        let a2 = &alarms.rules[1];
        assert!(a2.enabled);
        assert!(a2.level.is_none());
        assert!(a2.device_id.is_none());
        assert!(a2.op.is_none());
        assert!(a2.duration_ms.is_none());
        assert!(a2.suppress_ms.is_none());

        // 老配置无 [alarms] → None。
        let legacy = GatewayConfig::parse(EXAMPLE_TOML).expect("parse legacy");
        assert!(legacy.alarms.is_none(), "alarms default to None");

        // 空可选引擎字段不得序列化（避免旧版本拒读）。
        let a2_out = toml::to_string_pretty(a2).expect("serialize alarm rule");
        for key in ["device_id", "point_id", "op", "duration_ms", "suppress_ms"] {
            assert!(!a2_out.contains(key), "empty {key} skipped: {a2_out}");
        }
    }

    /// 告警运算符取值域常量自身正确（`gt`/`ge`/`lt`/`le`/`eq`/`ne`，无重复）。
    #[test]
    fn alarm_rule_ops_domain_is_canonical() {
        assert_eq!(ALARM_RULE_OPS, &["gt", "ge", "lt", "le", "eq", "ne"]);
        let mut seen = ALARM_RULE_OPS.to_vec();
        seen.sort_unstable();
        seen.dedup();
        assert_eq!(seen.len(), ALARM_RULE_OPS.len(), "ops must be unique");
    }

    /// `[[mgmt_auth.roles]]` 自定义角色 schema 解析 + 老配置兼容（缺省空列表）。
    #[test]
    fn mgmt_auth_custom_roles_parse_and_backward_compatible() {
        let raw = r#"
[[mgmt_auth.users]]
name = "alice"
role = "system"
password_hash = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"

[[mgmt_auth.roles]]
id = "viewer"
name = "只读"
permissions = ["device.view", "audit.view"]

[[mgmt_auth.roles]]
id = "writer"
name = "只需写点位"

[[mgmt_auth.roles]]
id = "ops-lite"
name = "带 builtin 标记的角色"
permissions = ["device.view"]
builtin = true
"#;
        let config = GatewayConfig::parse(raw).expect("parse mgmt_auth roles");
        let section = config.mgmt_auth.as_ref().expect("section present");
        assert_eq!(section.users.len(), 1);
        assert_eq!(section.roles.len(), 3);
        assert_eq!(section.roles[0].id, "viewer");
        assert_eq!(section.roles[0].name, "只读");
        assert_eq!(
            section.roles[0].permissions,
            vec!["device.view".to_string(), "audit.view".to_string()]
        );
        // 缺省 permissions 为空列表、builtin false（内置角色不入配置段）。
        assert!(section.roles[1].permissions.is_empty());
        assert!(!section.roles[0].builtin, "builtin defaults to false");
        assert!(!section.roles[1].builtin);
        // 显式 builtin = true 可解析（只读标记；由接口层保证不落自建角色）。
        assert!(section.roles[2].builtin, "explicit builtin marker parsed");

        // 老配置（只有 users、无 roles）→ roles 空列表。
        let legacy = GatewayConfig::parse("[mgmt_auth]").expect("empty mgmt_auth");
        let section = legacy.mgmt_auth.expect("section present");
        assert!(section.roles.is_empty(), "legacy mgmt_auth has no roles");
        assert!(section.users.is_empty());
    }
}

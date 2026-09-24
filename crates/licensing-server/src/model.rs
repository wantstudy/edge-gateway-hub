//! `licensing-server` 云端授权数据模型（计划 task 44 / 45 / 46）。
//!
//! 权威文档：`docs/design/licensing-data-model.md`（9 表 ER + 字段 + 状态枚举）。
//! 本模块只承载**数据结构与状态枚举**，不写任何 SQL（SQL 全部集中在 [`crate::store`]）。
//!
//! # 设计约束（逐条对齐设计文档）
//!
//! 1. **时间一律 UTC 秒 INTEGER**：跨端一致性红线——daemon / 授权服务 / admin-console
//!    三端对时间戳的口径必须完全一致，禁止出现 TEXT datetime、禁止毫秒/纳秒混用。
//! 2. **一机一码**：`activation_code.bound_device_id` 为可空外键，激活时锁定为 1:1；
//!    本模块不提供任何客户端自行解绑 / 重置的数据入口（设计 §1）。
//! 3. **废弃语义**：`status == revoked` 时 `revoked_at` 必填且 `revoked_reason` 非空，
//!    由 [`ActivationCode::validate`] 强制（设计 §3）。
//! 4. **容器形态**：`device.deploy_mode`（native/docker）+ `image_digest` 供运维溯源；
//!    容器指纹来自**宿主锚点**（`host_anchor_ref`），容器重建不产生新 device 记录（设计 §2）。
//! 5. **私钥隔离**：[`SigningKey`] 只存 `public_key` 与 `hsm_ref`，**私钥不落业务库**（设计 §5）。
//! 6. **主键不可枚举猜测**：所有主键由 [`now_ns_id`] 生成，含 8 位随机 hex（32 bit 熵）。
//!
//! # 不变量位置
//!
//! 结构体只做**数据承载**，业务不变量集中在 [`ActivationCode::validate`] /
//! [`Lease::is_usable_at`] / [`Device::anchor_match_count`] 等纯函数里，
//! 便于 `store` 层与 `service` 层复用同一判定口径。

use std::time::{SystemTime, UNIX_EPOCH};

use rand::Rng as _;
use serde::{Deserialize, Serialize};

use crate::error::{LicenseError, LicenseResult};

/// 机器锚点固定数量（N-of-M 同机判定的 M）。
///
/// 与 task 41 / task 59 口径一致：采集端固定上报 5 个锚点哈希，
/// 服务端按 N-of-M（N 由风控阈值决定）判定「同机」。
pub const ANCHOR_COUNT: usize = 5;

/// N-of-M 同机判定阈值：命中 ≥ 本值 → 同机（允许 `ANCHOR_COUNT - 本值` 项漂移）。
///
/// 依据 `docs/design/machine-fingerprint.md` §3：**N=4, M=5，允许 1 项漂移**。
/// 判定**只在服务端执行**（客户端不预判、不缓存「是否同机」结论）。
///
/// 本常量是同机判定的**单一来源**：阈值比较一律经
/// [`Device::is_same_machine_by_anchors`]，任何调用方（如 `service::activate` 的
/// §7 冲突检测）都**不得**再写 `>= 4` 字面量——否则阈值会悄悄分叉。
pub const SAME_MACHINE_MIN_HITS: usize = 4;

// ---- 时间与主键工具 ----

/// 当前 UTC 秒（跨端统一时间口径）。
///
/// **不 panic**：系统时钟早于 Unix epoch（或不可用）时返回 `0`，
/// 由调用方的时序守卫（如 `valid_from` / `valid_until`）处理该退化情形。
pub fn now_unix_secs() -> i64 {
    match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(d) => i64::try_from(d.as_secs()).unwrap_or(i64::MAX),
        Err(_) => 0,
    }
}

/// 生成不可枚举猜测的主键：`{prefix}-{utc_secs}-{8 hex 随机}`。
///
/// 熵来源：8 位随机 hex（32 bit）；前缀与秒级时间戳用于人工排查与排序，
/// **不承载安全性**。攻击者无法从已有主键推演出后续主键。
pub fn now_ns_id(prefix: &str) -> String {
    let mut rng = rand::rng();
    let suffix: u32 = rng.random();
    format!("{prefix}-{}-{:08x}", now_unix_secs(), suffix)
}

// ---- 状态枚举 ----

/// 校验档位（A 轻 / B 中 / C 重），随 Lease Token 下发。
///
/// `as_str()` 返回**大写** `"A"` / `"B"` / `"C"`（与设计文档及客户端契约一致）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VerifyMode {
    /// A 档：轻量在线校验（`/verify` + `nonce` 防重放）。
    A,
    /// B 档：A 档 + 审计回执（`audit_receipt` 跳空检测）。
    B,
    /// C 档：B 档 + 更严时序 / 锚点判定。
    C,
}

impl VerifyMode {
    /// 稳定字符串（落库 / Token / 审计用，勿依赖 `Debug` 输出格式）。
    ///
    /// **注意返回大写**：与设计文档 `enum verify_mode "A|B|C"` 完全一致。
    pub fn as_str(self) -> &'static str {
        match self {
            VerifyMode::A => "A",
            VerifyMode::B => "B",
            VerifyMode::C => "C",
        }
    }

    /// 从落库 / Token 字符串解析（大小写不敏感，容忍两端空白）。
    pub fn parse(s: &str) -> LicenseResult<Self> {
        match s.trim() {
            "A" | "a" => Ok(VerifyMode::A),
            "B" | "b" => Ok(VerifyMode::B),
            "C" | "c" => Ok(VerifyMode::C),
            other => Err(LicenseError::Storage(format!(
                "unknown verify_mode: {other}"
            ))),
        }
    }
}

/// 部署形态：原生进程 / 容器。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeployMode {
    /// 原生进程部署（锚点取自本机）。
    Native,
    /// 容器部署（锚点取自宿主只读挂载源 `host_anchor_ref`）。
    Docker,
}

impl DeployMode {
    /// 稳定字符串。
    pub fn as_str(self) -> &'static str {
        match self {
            DeployMode::Native => "native",
            DeployMode::Docker => "docker",
        }
    }

    /// 从落库字符串解析。
    pub fn parse(s: &str) -> LicenseResult<Self> {
        match s.trim() {
            "native" => Ok(DeployMode::Native),
            "docker" => Ok(DeployMode::Docker),
            other => Err(LicenseError::Storage(format!(
                "unknown deploy_mode: {other}"
            ))),
        }
    }
}

/// 设备状态机（`device.status`）。
///
/// 由租约状态机驱动（task 42）：服务端心跳结果回写。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeviceStatus {
    /// 正常：租约有效、心跳准时。
    Active,
    /// 宽限中：心跳迟滞但在离线宽限窗内（设计 §1.5：7 天）。
    Gracing,
    /// 降级：宽限超期，功能受限但仍在线。
    Degraded,
    /// 停止：废弃 / 长期失联，服务端拒绝续期。
    Stopped,
}

impl DeviceStatus {
    /// 稳定字符串。
    pub fn as_str(self) -> &'static str {
        match self {
            DeviceStatus::Active => "active",
            DeviceStatus::Gracing => "gracing",
            DeviceStatus::Degraded => "degraded",
            DeviceStatus::Stopped => "stopped",
        }
    }

    /// 从落库字符串解析。
    pub fn parse(s: &str) -> LicenseResult<Self> {
        match s.trim() {
            "active" => Ok(DeviceStatus::Active),
            "gracing" => Ok(DeviceStatus::Gracing),
            "degraded" => Ok(DeviceStatus::Degraded),
            "stopped" => Ok(DeviceStatus::Stopped),
            other => Err(LicenseError::Storage(format!(
                "unknown device.status: {other}"
            ))),
        }
    }
}

/// 租约状态机（`lease.status`）。
///
/// 取值与 [`DeviceStatus`] 相同，但**必须是独立类型**——两者是**两个不同的状态机**：
/// `device.status` 由租约状态机驱动，`lease.status` 由心跳结果 + 时间守卫驱动。
/// 合并为一个类型会让「设备状态」与「租约状态」语义耦合，属返工项。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LeaseStatus {
    /// 正常。
    Active,
    /// 宽限中（离线宽限窗内）。
    Gracing,
    /// 降级。
    Degraded,
    /// 停止。
    Stopped,
}

impl LeaseStatus {
    /// 稳定字符串。
    pub fn as_str(self) -> &'static str {
        match self {
            LeaseStatus::Active => "active",
            LeaseStatus::Gracing => "gracing",
            LeaseStatus::Degraded => "degraded",
            LeaseStatus::Stopped => "stopped",
        }
    }

    /// 从落库字符串解析。
    pub fn parse(s: &str) -> LicenseResult<Self> {
        match s.trim() {
            "active" => Ok(LeaseStatus::Active),
            "gracing" => Ok(LeaseStatus::Gracing),
            "degraded" => Ok(LeaseStatus::Degraded),
            "stopped" => Ok(LeaseStatus::Stopped),
            other => Err(LicenseError::Storage(format!(
                "unknown lease.status: {other}"
            ))),
        }
    }
}

/// 激活码状态机（`activation_code.status`）。
///
/// 迁移规则（设计「生命周期与状态枚举汇总」）：
/// `issued → bound`（设备首次激活，服务端）；
/// `bound/issued → revoked`（仅总管理后台）；
/// `revoked → reissued`（仅总管理后台，生成新码并溯源）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CodeStatus {
    /// 已发放：尚未绑定任何设备。
    Issued,
    /// 已绑定：已锁定到唯一设备（一机一码）。
    Bound,
    /// 已废弃：立即失效，`revoked_at` / `revoked_reason` 必填。
    Revoked,
    /// 已重发：由废弃码派生的新码，经 `reissued_from_id` 溯源。
    Reissued,
}

impl CodeStatus {
    /// 稳定字符串。
    pub fn as_str(self) -> &'static str {
        match self {
            CodeStatus::Issued => "issued",
            CodeStatus::Bound => "bound",
            CodeStatus::Revoked => "revoked",
            CodeStatus::Reissued => "reissued",
        }
    }

    /// 从落库字符串解析。
    pub fn parse(s: &str) -> LicenseResult<Self> {
        match s.trim() {
            "issued" => Ok(CodeStatus::Issued),
            "bound" => Ok(CodeStatus::Bound),
            "revoked" => Ok(CodeStatus::Revoked),
            "reissued" => Ok(CodeStatus::Reissued),
            other => Err(LicenseError::Storage(format!(
                "unknown activation_code.status: {other}"
            ))),
        }
    }
}

/// 心跳结果（`heartbeat.result`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HeartbeatResult {
    /// 通过。
    Ok,
    /// 租约 / 激活码已被废弃。
    Revoked,
    /// 客户端时钟回拨或超窗（skew 检测）。
    Skew,
    /// 未知设备（未激活或记录已被清理）。
    UnknownDevice,
}

impl HeartbeatResult {
    /// 稳定字符串。
    pub fn as_str(self) -> &'static str {
        match self {
            HeartbeatResult::Ok => "ok",
            HeartbeatResult::Revoked => "revoked",
            HeartbeatResult::Skew => "skew",
            HeartbeatResult::UnknownDevice => "unknown_device",
        }
    }

    /// 从落库字符串解析。
    pub fn parse(s: &str) -> LicenseResult<Self> {
        match s.trim() {
            "ok" => Ok(HeartbeatResult::Ok),
            "revoked" => Ok(HeartbeatResult::Revoked),
            "skew" => Ok(HeartbeatResult::Skew),
            "unknown_device" => Ok(HeartbeatResult::UnknownDevice),
            other => Err(LicenseError::Storage(format!(
                "unknown heartbeat.result: {other}"
            ))),
        }
    }
}

/// 签名密钥状态（`signing_key.status`）。
///
/// 与 [`crate::keys::KeyStatus`] 语义一致，但保持独立：本类型是**落库视图**，
/// `KeyStatus` 是**进程内密钥环视图**，两者由 service 层显式映射。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SigningKeyStatus {
    /// 启用中：新 Token 用此 kid 签发，同时可验签。
    Active,
    /// 退役中：不再签发，存量租约仍可验签。
    Retiring,
    /// 已退役：不再签发且拒绝验签。
    Retired,
}

impl SigningKeyStatus {
    /// 稳定字符串。
    pub fn as_str(self) -> &'static str {
        match self {
            SigningKeyStatus::Active => "active",
            SigningKeyStatus::Retiring => "retiring",
            SigningKeyStatus::Retired => "retired",
        }
    }

    /// 从落库字符串解析。
    pub fn parse(s: &str) -> LicenseResult<Self> {
        match s.trim() {
            "active" => Ok(SigningKeyStatus::Active),
            "retiring" => Ok(SigningKeyStatus::Retiring),
            "retired" => Ok(SigningKeyStatus::Retired),
            other => Err(LicenseError::Storage(format!(
                "unknown signing_key.status: {other}"
            ))),
        }
    }
}

/// 审计主体类型（`audit_log.actor_type`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActorType {
    /// 管理后台操作者。
    Admin,
    /// 服务端系统动作（定时清理、风控告警）。
    System,
    /// 设备侧（网关心跳 / 回执上报）。
    Device,
}

impl ActorType {
    /// 稳定字符串。
    pub fn as_str(self) -> &'static str {
        match self {
            ActorType::Admin => "admin",
            ActorType::System => "system",
            ActorType::Device => "device",
        }
    }

    /// 从落库字符串解析。
    pub fn parse(s: &str) -> LicenseResult<Self> {
        match s.trim() {
            "admin" => Ok(ActorType::Admin),
            "system" => Ok(ActorType::System),
            "device" => Ok(ActorType::Device),
            other => Err(LicenseError::Storage(format!(
                "unknown audit_log.actor_type: {other}"
            ))),
        }
    }
}

// ---- 表结构 ----

/// `tenant` 表：租户（订购方）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Tenant {
    /// 租户 ID（主键）。
    pub tenant_id: String,
    /// 租户名称。
    pub name: String,
    /// 租户级默认校验档位（设计 §7：设备级可被 `lease.verify_mode` 覆盖）。
    pub verify_mode_default: VerifyMode,
    /// 联系人。
    pub contact: String,
    /// 创建时间（UTC 秒）。
    pub created_at: i64,
}

impl Tenant {
    /// 构造租户，`verify_mode_default` 缺省为 [`VerifyMode::B`]（设计默认档位）。
    pub fn new(tenant_id: String, name: String, contact: String, created_at: i64) -> Self {
        Tenant {
            tenant_id,
            name,
            verify_mode_default: VerifyMode::B,
            contact,
            created_at,
        }
    }
}

/// `device` 表：设备（一机一码绑定的目标）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Device {
    /// 设备 ID（主键）。
    pub device_id: String,
    /// 所属租户（外键 → `tenant.tenant_id`）。
    pub tenant_id: String,
    /// 机器码组合指纹（HMAC 加盐截断，**唯一**）。
    pub machine_code: String,
    /// 锚点哈希集（JSON 数组，长度应为 [`ANCHOR_COUNT`]），N-of-M 同机判定基础。
    pub anchor_hashes: Vec<String>,
    /// 部署形态。
    pub deploy_mode: DeployMode,
    /// 镜像 digest（docker 形态溯源；native 为 `None`）。
    pub image_digest: Option<String>,
    /// 宿主锚点引用（容器形态只读挂载源；native 为 `None`）。
    pub host_anchor_ref: Option<String>,
    /// 首次激活时间（UTC 秒，试用兜底锚点；未激活为 `None`）。
    pub first_activation_at: Option<i64>,
    /// 设备状态。
    pub status: DeviceStatus,
    /// 创建时间（UTC 秒）。
    pub created_at: i64,
}

impl Device {
    /// 构造设备（默认 [`DeviceStatus::Active`]，未激活，native 形态）。
    pub fn new(
        device_id: String,
        tenant_id: String,
        machine_code: String,
        anchor_hashes: Vec<String>,
        created_at: i64,
    ) -> Self {
        Device {
            device_id,
            tenant_id,
            machine_code,
            anchor_hashes,
            deploy_mode: DeployMode::Native,
            image_digest: None,
            host_anchor_ref: None,
            first_activation_at: None,
            status: DeviceStatus::Active,
            created_at,
        }
    }

    /// 本机锚点与另一组锚点的**匹配个数**（N-of-M 同机判定的基础）。
    ///
    /// 规则：
    /// - **忽略大小写**（采集端在不同平台可能输出大小写不一的 hex）；
    /// - **去重**（双方各自内部重复的锚点只计一次）；
    /// - 两端空白被裁剪。
    ///
    /// 返回值 ∈ `[0, min(本机去重数, 对方去重数)]`。判定阈值（N）由风控策略决定，
    /// 本函数只提供匹配计数，不做阈值判断。
    pub fn anchor_match_count(&self, other: &[String]) -> usize {
        let normalize = |items: &[String]| -> std::collections::HashSet<String> {
            items
                .iter()
                .map(|s| s.trim().to_ascii_lowercase())
                .filter(|s| !s.is_empty())
                .collect()
        };
        let mine = normalize(&self.anchor_hashes);
        let theirs = normalize(other);
        mine.intersection(&theirs).count()
    }

    /// §7 判定：锚点命中数是否达到**同机阈值**（≥ [`SAME_MACHINE_MIN_HITS`]＝4/5）。
    ///
    /// 与 [`Device::anchor_match_count`] 一样是**纯函数**；把「阈值比较」收敛到此处，
    /// 使阈值只有一个来源（`service` 层只调用本方法，不再写 `>= 4` 字面量）。
    ///
    /// 语义（对齐 `docs/design/machine-fingerprint.md` §3）：
    /// - 命中 ≥4/5 → 同机（容忍 1 项漂移：换网卡 / 加硬盘 / 虚拟化迁移 / OS 重装）；
    /// - 命中 ≤3/5 → 异机（2 项及以上同时变化在合法运维中罕见，判异机）。
    pub fn is_same_machine_by_anchors(&self, other: &[String]) -> bool {
        self.anchor_match_count(other) >= SAME_MACHINE_MIN_HITS
    }

    /// 相对本机绑定记录，给定锚点集的**漂移项数**（`ANCHOR_COUNT - 命中数`）。
    ///
    /// 仅供审计留痕（「命中几项 / 漂移几项」）使用；**不参与判定**（判定见
    /// [`Device::is_same_machine_by_anchors`]）。饱和减避免下溢。
    pub fn anchor_drift_count(&self, other: &[String]) -> usize {
        ANCHOR_COUNT.saturating_sub(self.anchor_match_count(other))
    }
}

/// `activation_code` 表：激活码（厂商发放，一机一码绑定）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActivationCode {
    /// 激活码 ID（主键）。
    pub code_id: String,
    /// 码值（厂商发放，**唯一**）。**不得进日志 / 错误信息**。
    pub code: String,
    /// 状态。
    pub status: CodeStatus,
    /// 绑定设备（外键 → `device.device_id`；`issued` / `reissued` 为 `None`）。
    pub bound_device_id: Option<String>,
    /// 所属租户（外键 → `tenant.tenant_id`）。
    pub tenant_id: String,
    /// 授权档位 tier。
    pub tier: String,
    /// 有效期起（UTC 秒）。
    pub valid_from: i64,
    /// 有效期止（UTC 秒）。
    pub valid_until: i64,
    /// 来源订单 ID（未关联订单为 `None`）。
    pub source_order_id: Option<String>,
    /// 溯源：原码 ID（重发链；非重发码为 `None`）。
    pub reissued_from_id: Option<String>,
    /// 发放操作者。
    pub issued_by: String,
    /// 废弃时间（UTC 秒，立即失效；未废弃为 `None`）。
    pub revoked_at: Option<i64>,
    /// 废弃原因（**废弃时必填**）。
    pub revoked_reason: Option<String>,
    /// 幂等键（revoke / reissue 同事务去重）。
    pub idempotency_key: Option<String>,
    /// 创建时间（UTC 秒）。
    pub created_at: i64,
    /// 预绑定机器码（task 46）：厂商在**发放 / 重发**时指定，`None` = 留待首次激活自由绑定。
    ///
    /// 一机一码红线：一旦预绑定，该码**只能**被 `machine_code` 等于本值的设备激活
    /// （校验在 `LicensingService::activate` 内、绑定之前执行）。
    /// **空白串一律视为「未预绑定」**——统一以 `trim().is_empty()` 判定，
    /// 避免 `"   "` 被当成有效机器码而把设备永久锁死在空指纹上。
    pub prebind_machine_code: Option<String>,
}

impl ActivationCode {
    /// 构造一个**新发放**（`issued`、未绑定）的激活码。
    ///
    /// 参数较多是数据模型的固有宽度（设计文档 15 列），此处显式允许，
    /// 避免为一个「按列填充」的构造函数引入误导性的 builder 抽象。
    #[allow(clippy::too_many_arguments)]
    pub fn new_issued(
        code_id: String,
        code: String,
        tenant_id: String,
        tier: String,
        valid_from: i64,
        valid_until: i64,
        source_order_id: Option<String>,
        issued_by: String,
        created_at: i64,
    ) -> Self {
        ActivationCode {
            code_id,
            code,
            status: CodeStatus::Issued,
            bound_device_id: None,
            tenant_id,
            tier,
            valid_from,
            valid_until,
            source_order_id,
            reissued_from_id: None,
            issued_by,
            revoked_at: None,
            revoked_reason: None,
            idempotency_key: None,
            created_at,
            prebind_machine_code: None,
        }
    }

    /// 设置**预绑定机器码**（task 46 换机迁移：新码可直接锁定到新机）。
    ///
    /// 空白串统一归一为 `None`（"   " 不算「已提供」），保证
    /// 「留待首次激活绑定」与「显式留空」语义一致。
    pub fn with_prebind(mut self, machine_code: Option<String>) -> Self {
        self.prebind_machine_code = machine_code
            .map(|m| m.trim().to_string())
            .filter(|m| !m.is_empty());
        self
    }

    /// 预绑定是否命中给定机器码（`None` = 未预绑定 → 任意机器均可激活）。
    pub fn prebind_matches(&self, machine_code: &str) -> bool {
        match self.prebind_machine_code.as_deref() {
            None => true,
            Some(expected) => expected == machine_code,
        }
    }

    /// 校验状态不变量（**不可绕过的状态机防线**）。
    ///
    /// 规则：
    /// 1. `valid_until > valid_from`（有效期区间非空）；
    /// 2. `status == Revoked` → `revoked_at.is_some()` 且 `revoked_reason` 非空白；
    /// 3. `status == Bound` → `bound_device_id.is_some()` 且非空白；
    /// 4. `status != Revoked` → 不得携带 `revoked_at`（避免「未废弃却有废弃时间」的脏数据）。
    /// 5. 预绑定机器码若存在必须**非空白**（`"   "` 不得锁死设备指纹）。
    ///
    /// 错误信息**绝不包含码值 `code`**（错误会进日志与审计）。
    pub fn validate(&self) -> LicenseResult<()> {
        if self.valid_until <= self.valid_from {
            return Err(LicenseError::KeyStateIllegal(
                "activation_code validity window is empty: valid_until must be > valid_from".into(),
            ));
        }
        if self
            .prebind_machine_code
            .as_deref()
            .is_some_and(|m| m.trim().is_empty())
        {
            return Err(LicenseError::KeyStateIllegal(
                "activation_code prebind_machine_code must be non-blank when present".into(),
            ));
        }
        match self.status {
            CodeStatus::Revoked => {
                if self.revoked_at.is_none() {
                    return Err(LicenseError::KeyStateIllegal(
                        "activation_code status=revoked requires revoked_at".into(),
                    ));
                }
                match self.revoked_reason.as_deref() {
                    Some(reason) if !reason.trim().is_empty() => {}
                    _ => {
                        return Err(LicenseError::KeyStateIllegal(
                            "activation_code status=revoked requires a non-empty revoked_reason"
                                .into(),
                        ));
                    }
                }
            }
            CodeStatus::Bound => match self.bound_device_id.as_deref() {
                Some(device) if !device.trim().is_empty() => {}
                _ => {
                    return Err(LicenseError::KeyStateIllegal(
                        "activation_code status=bound requires bound_device_id".into(),
                    ));
                }
            },
            CodeStatus::Issued | CodeStatus::Reissued => {
                if self.revoked_at.is_some() {
                    return Err(LicenseError::KeyStateIllegal(
                        "activation_code carries revoked_at but status is not revoked".into(),
                    ));
                }
            }
        }
        Ok(())
    }

    /// 是否仍可被**激活使用**（状态为 `issued` 且未绑定）。
    pub fn is_activatable(&self) -> bool {
        matches!(self.status, CodeStatus::Issued) && self.bound_device_id.is_none()
    }

    /// 给定时刻是否在有效期内（闭区间 `[valid_from, valid_until]`）。
    pub fn is_valid_at(&self, now: i64) -> bool {
        now >= self.valid_from && now <= self.valid_until
    }
}

/// `lease` 表：租约（Lease Token 的服务端记录）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Lease {
    /// 租约 ID（主键）。
    pub lease_id: String,
    /// 设备（外键 → `device.device_id`）。
    pub device_id: String,
    /// 激活码（外键 → `activation_code.code_id`）。
    pub code_id: String,
    /// 签名密钥标识（外键 → `signing_key.kid`）。
    pub kid: String,
    /// Lease Token 签名（Ed25519，base64）。
    pub token_sig: String,
    /// 随 Token 下发的校验档位。
    pub verify_mode: VerifyMode,
    /// 档位。
    pub tier: String,
    /// 签发时间（UTC 秒）。
    pub issued_at: i64,
    /// 到期时间（UTC 秒）。
    pub valid_until: i64,
    /// 最近心跳时间（UTC 秒；从未心跳为 `None`）。
    pub last_heartbeat_at: Option<i64>,
    /// 租约状态。
    pub status: LeaseStatus,
}

impl Lease {
    /// 是否处于**已废弃**（停止）状态。
    ///
    /// 注意：`Lease` 自身无 `revoked_at` 字段——废弃经由 `activation_code.revoked_at`
    /// 表达；本方法是「租约状态机已被置为停止」的快速判定，供心跳路径短路。
    pub fn is_revoked(&self) -> bool {
        matches!(self.status, LeaseStatus::Stopped)
    }

    /// 给定时刻是否**已过期**。
    pub fn is_expired_at(&self, now: i64) -> bool {
        now > self.valid_until
    }

    /// 给定时刻是否**可用**（未废弃、未过期）。
    ///
    /// `gracing` / `degraded` 仍属可用（功能受限但未失效），二者语义由调用方
    /// （service 层）按档位进一步收紧。
    pub fn is_usable_at(&self, now: i64) -> bool {
        !self.is_revoked() && !self.is_expired_at(now)
    }
}

/// `heartbeat` 表：心跳记录（24h 周期）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Heartbeat {
    /// 记录 ID（主键）。
    pub id: String,
    /// 租约（外键 → `lease.lease_id`）。
    pub lease_id: String,
    /// 设备（外键 → `device.device_id`）。
    pub device_id: String,
    /// 客户端时间（UTC 秒，回拨检测依据）。
    pub client_ts: i64,
    /// 服务端时间（UTC 秒）。
    pub server_ts: i64,
    /// 心跳结果。
    pub result: HeartbeatResult,
    /// 携带的最近回执序号区间（如 `"10-25"`；未携带为 `None`）。
    pub receipt_cursor: Option<String>,
    /// 落库时间（UTC 秒）。
    pub created_at: i64,
}

/// `audit_receipt` 表：B 档审计回执（**仅白名单字段，无业务数值**）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuditReceipt {
    /// 回执 ID（主键）。
    pub id: String,
    /// 租约（外键 → `lease.lease_id`）。
    pub lease_id: String,
    /// 设备机器码。
    pub device_mid: String,
    /// 起始序号。
    pub seq_from: i64,
    /// 结束序号。
    pub seq_to: i64,
    /// 条数。
    pub count: i64,
    /// payload 摘要哈希（无业务数值）。
    pub payload_digest: String,
    /// 客户端时间（UTC 秒）。
    pub ts: i64,
    /// 设备签名。
    pub sig: String,
    /// 接收时间（UTC 秒，**可晚于 `ts`**：支持断网延迟补报）。
    pub received_at: i64,
    /// 跳空 / 回退 / 缺失标记（风控告警，人工核实、不自动封禁）。
    pub gap_flag: bool,
}

/// `nonce_cache` 表：防重放随机数缓存（全局唯一约束 + 过期清理）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NonceCache {
    /// 全局随机数（主键，防重放）。
    pub nonce: String,
    /// 设备（外键 → `device.device_id`）。
    pub device_id: String,
    /// 过期时间（UTC 秒）。
    pub expires_at: i64,
    /// 使用时间（UTC 秒）。
    pub used_at: i64,
}

/// `signing_key` 表：签名密钥**公开视图**（私钥绝不落业务库）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SigningKey {
    /// 密钥标识（主键）。
    pub kid: String,
    /// 密钥状态。
    pub status: SigningKeyStatus,
    /// 公钥（下发客户端公钥集）。
    pub public_key: String,
    /// KMS / 离线保管引用（**私钥的外部锚点，不是私钥本身**）。
    pub hsm_ref: Option<String>,
    /// 启用时间（UTC 秒）。
    pub enabled_at: i64,
    /// 退役时间（UTC 秒；未退认为 `None`）。
    pub retired_at: Option<i64>,
}

/// `audit_log` 表：全量审计（后台操作 / 网关心跳 / 风控告警）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuditLog {
    /// 审计 ID（主键）。
    pub id: String,
    /// 主体类型。
    pub actor_type: ActorType,
    /// 操作者。
    pub actor_id: String,
    /// 动作（issue / revoke / reissue / activation / ...）。
    pub action: String,
    /// 实体类型。
    pub entity_type: String,
    /// 实体 ID。
    pub entity_id: String,
    /// 详情（JSON 文本：原因 / 幂等键 / 命中项）。
    pub detail: String,
    /// 时间（UTC 秒）。
    pub ts: i64,
    /// 来源 IP / 来源标识。
    pub ip: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_code(status: CodeStatus) -> ActivationCode {
        let mut c = ActivationCode::new_issued(
            "ac-1".into(),
            "CODE-SECRET-VALUE".into(),
            "t-1".into(),
            "pro".into(),
            1_000,
            2_000,
            Some("order-1".into()),
            "admin-1".into(),
            900,
        );
        c.status = status;
        c
    }

    #[test]
    fn verify_mode_round_trips_and_is_uppercase() {
        for m in [VerifyMode::A, VerifyMode::B, VerifyMode::C] {
            assert_eq!(VerifyMode::parse(m.as_str()).expect("parse"), m);
        }
        // 大写红线：契约字符串必须是 "A"/"B"/"C"。
        assert_eq!(VerifyMode::A.as_str(), "A");
        assert_eq!(VerifyMode::B.as_str(), "B");
        assert_eq!(VerifyMode::C.as_str(), "C");
        // 容忍小写输入（历史数据 / 手工录入）。
        assert_eq!(VerifyMode::parse("b").expect("parse"), VerifyMode::B);
        assert!(VerifyMode::parse("D").is_err());
        assert!(VerifyMode::parse("").is_err());
    }

    #[test]
    fn all_status_enums_round_trip_and_reject_garbage() {
        for s in [
            DeviceStatus::Active,
            DeviceStatus::Gracing,
            DeviceStatus::Degraded,
            DeviceStatus::Stopped,
        ] {
            assert_eq!(DeviceStatus::parse(s.as_str()).expect("device"), s);
        }
        for s in [
            LeaseStatus::Active,
            LeaseStatus::Gracing,
            LeaseStatus::Degraded,
            LeaseStatus::Stopped,
        ] {
            assert_eq!(LeaseStatus::parse(s.as_str()).expect("lease"), s);
        }
        for s in [
            CodeStatus::Issued,
            CodeStatus::Bound,
            CodeStatus::Revoked,
            CodeStatus::Reissued,
        ] {
            assert_eq!(CodeStatus::parse(s.as_str()).expect("code"), s);
        }
        for s in [
            HeartbeatResult::Ok,
            HeartbeatResult::Revoked,
            HeartbeatResult::Skew,
            HeartbeatResult::UnknownDevice,
        ] {
            assert_eq!(HeartbeatResult::parse(s.as_str()).expect("hb"), s);
        }
        for s in [
            SigningKeyStatus::Active,
            SigningKeyStatus::Retiring,
            SigningKeyStatus::Retired,
        ] {
            assert_eq!(SigningKeyStatus::parse(s.as_str()).expect("key"), s);
        }
        for s in [ActorType::Admin, ActorType::System, ActorType::Device] {
            assert_eq!(ActorType::parse(s.as_str()).expect("actor"), s);
        }
        for s in [DeployMode::Native, DeployMode::Docker] {
            assert_eq!(DeployMode::parse(s.as_str()).expect("deploy"), s);
        }

        assert!(DeviceStatus::parse("zombie").is_err());
        assert!(LeaseStatus::parse("zombie").is_err());
        assert!(CodeStatus::parse("zombie").is_err());
        assert!(HeartbeatResult::parse("zombie").is_err());
        assert!(SigningKeyStatus::parse("zombie").is_err());
        assert!(ActorType::parse("zombie").is_err());
        assert!(DeployMode::parse("zombie").is_err());
    }

    #[test]
    fn device_status_and_lease_status_are_distinct_types() {
        // 同一底层取值，但类型不同——两者不能互相赋值（编译期保证）。
        let d = DeviceStatus::Active;
        let l = LeaseStatus::Active;
        assert_eq!(d.as_str(), l.as_str());
        assert_eq!(
            DeviceStatus::parse("stopped").expect("d"),
            DeviceStatus::Stopped
        );
        assert_eq!(
            LeaseStatus::parse("stopped").expect("l"),
            LeaseStatus::Stopped
        );
    }

    #[test]
    fn code_validate_accepts_wellformed_variants() {
        let issued = sample_code(CodeStatus::Issued);
        assert!(issued.validate().is_ok());

        let mut bound = sample_code(CodeStatus::Bound);
        bound.bound_device_id = Some("dev-1".into());
        assert!(bound.validate().is_ok());

        let mut revoked = sample_code(CodeStatus::Revoked);
        revoked.revoked_at = Some(1_500);
        revoked.revoked_reason = Some("换机重发".into());
        assert!(revoked.validate().is_ok());
    }

    #[test]
    fn code_validate_rejects_inverted_validity_window() {
        let mut c = sample_code(CodeStatus::Issued);
        c.valid_from = 2_000;
        c.valid_until = 2_000; // 空区间
        let err = c.validate().expect_err("must reject");
        assert!(matches!(err, LicenseError::KeyStateIllegal(_)), "{err:?}");

        c.valid_until = 1_999;
        assert!(c.validate().is_err());
    }

    #[test]
    fn code_validate_rejects_revoked_without_time_or_reason() {
        let mut c = sample_code(CodeStatus::Revoked);
        // 缺 revoked_at。
        c.revoked_reason = Some("原因".into());
        assert!(matches!(
            c.validate().expect_err("no revoked_at"),
            LicenseError::KeyStateIllegal(_)
        ));

        // 有 revoked_at 但原因缺失。
        c.revoked_at = Some(1_500);
        c.revoked_reason = None;
        assert!(c.validate().is_err());

        // 原因为空白。
        c.revoked_reason = Some("   ".into());
        assert!(c.validate().is_err());
    }

    #[test]
    fn code_validate_rejects_bound_without_device() {
        let c = sample_code(CodeStatus::Bound);
        assert_eq!(c.bound_device_id, None);
        assert!(matches!(
            c.validate().expect_err("no device"),
            LicenseError::KeyStateIllegal(_)
        ));
    }

    #[test]
    fn code_validate_error_never_leaks_code_value() {
        let mut c = sample_code(CodeStatus::Revoked);
        c.revoked_at = Some(1);
        c.revoked_reason = None;
        let rendered = c.validate().expect_err("must reject").to_string();
        assert!(
            !rendered.contains("CODE-SECRET-VALUE"),
            "错误信息泄露激活码原文: {rendered}"
        );
    }

    #[test]
    fn code_activatable_and_valid_at() {
        let mut c = sample_code(CodeStatus::Issued);
        assert!(c.is_activatable());
        c.bound_device_id = Some("dev-1".into());
        assert!(!c.is_activatable(), "已绑定即不可再激活");

        assert!(c.is_valid_at(1_000));
        assert!(c.is_valid_at(2_000));
        assert!(!c.is_valid_at(999));
        assert!(!c.is_valid_at(2_001));
    }

    #[test]
    fn lease_lifecycle_helpers() {
        let lease = Lease {
            lease_id: "l-1".into(),
            device_id: "dev-1".into(),
            code_id: "ac-1".into(),
            kid: "k1".into(),
            token_sig: "sig".into(),
            verify_mode: VerifyMode::B,
            tier: "pro".into(),
            issued_at: 1_000,
            valid_until: 2_000,
            last_heartbeat_at: None,
            status: LeaseStatus::Active,
        };
        assert!(!lease.is_revoked());
        assert!(!lease.is_expired_at(2_000));
        assert!(lease.is_expired_at(2_001));
        assert!(lease.is_usable_at(1_500));
        assert!(!lease.is_usable_at(2_001));

        let stopping = Lease {
            status: LeaseStatus::Stopped,
            ..lease.clone()
        };
        assert!(stopping.is_revoked());
        assert!(!stopping.is_usable_at(1_500));
        // 宽限中仍可用（功能受限但未失效）。
        let gracing = Lease {
            status: LeaseStatus::Gracing,
            ..lease
        };
        assert!(gracing.is_usable_at(1_500));
    }

    #[test]
    fn anchor_match_count_is_case_insensitive_and_deduped() {
        let device = Device::new(
            "dev-1".into(),
            "t-1".into(),
            "mc-1".into(),
            vec![
                "AA11".into(),
                "bb22".into(),
                "cc33".into(),
                "dd44".into(),
                "ee55".into(),
            ],
            1_000,
        );
        // 大小写不同 + 重复项 → 仍命中 2 个。
        let other = vec!["aa11".into(), "AA11".into(), "BB22".into(), "zz99".into()];
        assert_eq!(device.anchor_match_count(&other), 2);

        // 完全一致（含大小写差异）→ 5。
        let same: Vec<String> = device
            .anchor_hashes
            .iter()
            .map(|s| s.to_uppercase())
            .collect();
        assert_eq!(device.anchor_match_count(&same), 5);

        // 无交集 → 0；空 → 0。
        assert_eq!(device.anchor_match_count(&["nope".into()]), 0);
        assert_eq!(device.anchor_match_count(&[]), 0);
        // 空白项被忽略。
        assert_eq!(device.anchor_match_count(&["   ".into()]), 0);
    }

    #[test]
    fn now_unix_secs_is_plausible_and_never_panics() {
        let now = now_unix_secs();
        // 2020-01-01 之后（避免拿到 0 以外的退化值）。
        assert!(now > 1_577_836_800, "时钟异常: {now}");
        // 远小于 i64::MAX，说明未触发饱和分支。
        assert!(now < i64::MAX);
    }

    #[test]
    fn now_ns_id_has_prefix_and_random_suffix() {
        let a = now_ns_id("dev");
        assert!(a.starts_with("dev-"), "{a}");
        // {prefix}-{secs}-{8hex}
        let parts: Vec<&str> = a.split('-').collect();
        assert_eq!(parts.len(), 3, "{a}");
        assert_eq!(parts[2].len(), 8, "{a}");
        assert!(parts[2].chars().all(|c| c.is_ascii_hexdigit()), "{a}");
        // 同一秒内两次生成必须不同（不可枚举猜测）。
        let b = now_ns_id("dev");
        assert_ne!(a, b, "主键必须含随机熵");
    }

    #[test]
    fn tenant_defaults_to_verify_mode_b() {
        let t = Tenant::new("t-1".into(), "租户".into(), "ops@x".into(), 1_000);
        assert_eq!(t.verify_mode_default, VerifyMode::B);
        assert_eq!(t.created_at, 1_000);
    }

    #[test]
    fn models_serialize_round_trip() {
        let device = Device::new(
            "dev-1".into(),
            "t-1".into(),
            "mc".into(),
            vec!["a".into(); ANCHOR_COUNT],
            5,
        );
        let json = serde_json::to_string(&device).expect("ser");
        let back: Device = serde_json::from_str(&json).expect("de");
        assert_eq!(device, back);

        // 枚举在 JSON 中为 snake_case 字符串。
        let raw = serde_json::to_string(&ActorType::Device).expect("ser");
        assert_eq!(raw, "\"device\"");
    }

    #[test]
    fn anchor_count_is_five() {
        assert_eq!(ANCHOR_COUNT, 5);
    }

    #[test]
    fn same_machine_threshold_is_four_of_five_with_one_drift_tolerance() {
        // §3 红线：N=4, M=5 —— 恰好允许 1 项漂移（既不要求全等，也不容忍 2 项）。
        assert_eq!(SAME_MACHINE_MIN_HITS, 4);
        const {
            assert!(
                SAME_MACHINE_MIN_HITS < ANCHOR_COUNT,
                "阈值必须低于锚点总数，否则「允许漂移」名存实亡"
            );
        }
        assert_eq!(
            ANCHOR_COUNT - SAME_MACHINE_MIN_HITS,
            1,
            "必须恰好允许 1 项漂移（而非 0 或 2）"
        );
    }

    #[test]
    fn is_same_machine_by_anchors_covers_full_boundary_matrix() {
        let device = Device::new(
            "dev-1".into(),
            "t-1".into(),
            "mc-1".into(),
            vec![
                "a1".into(),
                "b2".into(),
                "c3".into(),
                "d4".into(),
                "e5".into(),
            ],
            1_000,
        );
        let v = |items: &[&str]| {
            items
                .iter()
                .map(|s| (*s).to_string())
                .collect::<Vec<String>>()
        };

        // 5/5、4/5（1 项漂移）→ 同机。
        assert!(device.is_same_machine_by_anchors(&device.anchor_hashes.clone()));
        assert!(device.is_same_machine_by_anchors(&v(&["a1", "b2", "c3", "d4", "zz"])));
        // 3/5、0/5、空集 → 异机。
        assert!(!device.is_same_machine_by_anchors(&v(&["a1", "b2", "c3", "yy", "zz"])));
        assert!(!device.is_same_machine_by_anchors(&v(&["p", "q", "r", "s", "t"])));
        assert!(!device.is_same_machine_by_anchors(&[]));
        // 大小写不敏感（沿用 `anchor_match_count` 语义）→ 5/5 仍同机。
        assert!(device.is_same_machine_by_anchors(&v(&["A1", "B2", "C3", "D4", "E5"])));
    }

    #[test]
    fn anchor_drift_count_reflects_misses() {
        let device = Device::new(
            "dev-1".into(),
            "t-1".into(),
            "mc-1".into(),
            vec![
                "a1".into(),
                "b2".into(),
                "c3".into(),
                "d4".into(),
                "e5".into(),
            ],
            1_000,
        );
        let v = |items: &[&str]| {
            items
                .iter()
                .map(|s| (*s).to_string())
                .collect::<Vec<String>>()
        };
        assert_eq!(device.anchor_drift_count(&device.anchor_hashes.clone()), 0);
        assert_eq!(
            device.anchor_drift_count(&v(&["a1", "b2", "c3", "d4", "zz"])),
            1
        );
        assert_eq!(
            device.anchor_drift_count(&v(&["a1", "b2", "c3", "yy", "zz"])),
            2
        );
        assert_eq!(device.anchor_drift_count(&v(&["p", "q", "r", "s", "t"])), 5);
        assert_eq!(device.anchor_drift_count(&[]), 5);
    }
}

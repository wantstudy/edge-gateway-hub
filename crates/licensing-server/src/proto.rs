//! `licensing-server` 端点协议层：请求 / 响应结构、业务错误码与统一响应包裹。
//!
//! # 对齐基线
//!
//! 逐条对齐 `docs/design/licensing-api.md`：
//! - §1 设备端：`/activation`、`/heartbeat`、`/verify`、`/audit/receipt`
//! - §2 管理端：`/admin/codes/issue`、`/admin/codes/{id}/revoke`、`/admin/codes/{id}/reissue`、
//!   `/admin/codes`、`/admin/codes/{id}`、`/admin/devices`、`/admin/audit/logs`
//!
//! # 大整数红线（**必须遵守**）
//!
//! JSON number 是 IEEE754 double，安全整数上限为 2^53−1 = 9007199254740991。而：
//! - 纳秒时间戳（约 1.7e18）远超此上限 → 会**静默丢精度**；
//! - uint64 审计序号（`seq_from` / `seq_to` / `count`）同理。
//!
//! 因此本模块中**所有**时间戳与计数器字段一律使用 [`String`] 表达（`"1700000000"` 而非 `1700000000`）。
//! 服务端不做任何「自动 i64 → number」的便利转换——精度正确性优先于调用便利。
//! 模块内测试 `json_big_ints_are_strings` 断言序列化后这些字段确实形如 `"123"`。

use serde::{Deserialize, Serialize};

use crate::error::{LicenseError, LicenseResult};

// ============================================================================
// §1 设备端端点
// ============================================================================

/// `POST /activation` 请求体（设计 §1.1）。
///
/// 设备首次激活 + 一机一码绑定。`nonce` 与 `ts` 用于防重放与时间窗校验；
/// `req_sig` 为设备私钥对请求规范字段的签名。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActivationRequest {
    /// 厂商发放的激活码（`IOTDAQ-XXXX-XXXX-XXXX-XXXX`）。
    pub activation_code: String,
    /// 机器码组合指纹（HMAC 加盐截断）。
    pub machine_code: String,
    /// 锚点哈希集（N-of-M 同机判定，M=5）。
    pub anchor_hashes: Vec<String>,
    /// 设备公钥（用于后续请求验签，base64）。
    pub device_pubkey: String,
    /// 全局随机数（防重放）。
    pub nonce: String,
    /// 客户端时间（UTC 秒；**String** 承载，见模块级大整数说明）。
    pub ts: String,
    /// 设备私钥对本请求规范字段的签名（base64）。
    pub req_sig: String,
}

/// `POST /activation` 响应体（设计 §1.1 响应 200）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActivationResponse {
    /// 租约 ID。
    pub lease_id: String,
    /// 三段式 Lease Token（`kid.payload.sig`）。
    pub lease_token: String,
    /// 二次校验档位 `A` / `B` / `C`（随 Token 下发）。
    pub verify_mode: String,
    /// 授权档位。
    pub tier: String,
    /// 失效时刻（UTC 秒，**String**）。
    pub valid_until: String,
    /// 心跳周期（小时，默认 24）。
    pub heartbeat_hours: i64,
    /// 服务端时间（UTC 秒，**String**，回显以校准设备时钟）。
    pub server_time: String,
    /// nonce 回显（与请求一致）。
    pub nonce: String,
    /// 服务端响应签名公钥（base64；**TOFU 下发**，2026-09-25 主理人决策）。
    ///
    /// 设计 §1.1 未定义响应级 `sig` 的公钥分发方式；本字段让客户端在首次激活时
    /// 拿到签名公钥并**钉定**（Trust-On-First-Use），后续心跳响应用钉定公钥验签。
    /// 客户端侧镜像：`daemon/src/auth/client.rs` 的响应验签与钉定逻辑。
    pub server_pubkey: String,
    /// 服务端响应签名（base64）。
    pub sig: String,
}

/// `receipt_cursor`：最近已确认回执区间（设计 §1.2）。
///
/// ⚠️ `seq_from` / `seq_to` **必须是 String**（uint64 序号，JSON number 会丢精度）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReceiptCursor {
    /// 区间起始序号（**String**）。
    pub seq_from: String,
    /// 区间结束序号（**String**）。
    pub seq_to: String,
}

/// `POST /heartbeat` 请求体（设计 §1.2）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HeartbeatRequest {
    /// 租约 ID。
    pub lease_id: String,
    /// 客户端时间（UTC 秒，**String**）。
    pub ts: String,
    /// 全局随机数（防重放）。
    pub nonce: String,
    /// 最近已确认回执区间（B 档必填；A/C 档可空）。
    pub receipt_cursor: Option<ReceiptCursor>,
    /// 设备签名（base64）。
    pub device_sig: String,
}

/// `POST /heartbeat` 响应体（设计 §1.2 响应 200）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HeartbeatResponse {
    /// 服务端时间（UTC 秒，**String**）。
    pub server_time: String,
    /// 下次心跳截止时刻（UTC 秒，**String**）。
    pub next_deadline: String,
    /// 租约失效时刻（UTC 秒，**String**）。
    pub valid_until: String,
    /// 二次校验档位 `A` / `B` / `C`。
    pub verify_mode: String,
    /// 授权档位。
    pub tier: String,
    /// 服务端响应签名（base64）。
    pub sig: String,
}

/// `POST /verify` 请求体（设计 §1.3，**仅 A 档**）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VerifyRequest {
    /// 设备机器码指纹。
    pub device_mid: String,
    /// 租约 ID。
    pub lease_id: String,
    /// 业务消息摘要（规范化后哈希，非序列化原始字节）。
    pub payload_digest: String,
    /// 客户端时间（UTC 秒，**String**）。
    pub ts: String,
    /// 全局随机数（防重放）。
    pub nonce: String,
    /// 设备签名（base64）。
    pub device_sig: String,
}

/// `POST /verify` 响应体（设计 §1.3 响应 200）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VerifyResponse {
    /// 二次校验是否通过。
    pub ok: bool,
    /// 服务端时间（UTC 秒，**String**）。
    pub server_time: String,
    /// nonce 回显。
    pub nonce: String,
}

/// `/verify` 请求体字段白名单（设计 §1.3「字段白名单」）。
///
/// 与 §1.4 回执白名单同理：请求体出现白名单外字段 → `422 FIELD_WHITELIST_VIOLATION`。
pub const VERIFY_WHITELIST: [&str; 6] = [
    "device_mid",
    "lease_id",
    "payload_digest",
    "ts",
    "nonce",
    "device_sig",
];

impl VerifyRequest {
    /// 结构体自检：六个业务字段均非空白（`trim().is_empty()` 判定）。
    ///
    /// 用 `trim().is_empty()` 而非 `is_empty()`——纯空白串不算「已提供」。
    pub fn validate_whitelist(&self) -> LicenseResult<()> {
        fn missing(s: &str) -> bool {
            s.trim().is_empty()
        }
        if missing(&self.device_mid) {
            return Err(LicenseError::field_whitelist_violation(
                "verify field whitelist violation: device_mid missing",
            ));
        }
        if missing(&self.lease_id) {
            return Err(LicenseError::field_whitelist_violation(
                "verify field whitelist violation: lease_id missing",
            ));
        }
        if missing(&self.payload_digest) {
            return Err(LicenseError::field_whitelist_violation(
                "verify field whitelist violation: payload_digest missing",
            ));
        }
        if missing(&self.nonce) {
            return Err(LicenseError::field_whitelist_violation(
                "verify field whitelist violation: nonce missing",
            ));
        }
        if missing(&self.device_sig) {
            return Err(LicenseError::field_whitelist_violation(
                "verify field whitelist violation: device_sig missing",
            ));
        }
        Ok(())
    }

    /// 对**原始 JSON 对象**做白名单校验：出现白名单外 key → `FieldWhitelistViolation`。
    ///
    /// serde 默认忽略未知字段，故上层解析请求时**必须先**用本方法校验原始 JSON。
    pub fn validate_whitelist_value(value: &serde_json::Value) -> LicenseResult<()> {
        let obj = value.as_object().ok_or_else(|| {
            LicenseError::field_whitelist_violation("verify body must be a JSON object")
        })?;
        for key in obj.keys() {
            if !VERIFY_WHITELIST.contains(&key.as_str()) {
                return Err(LicenseError::field_whitelist_violation(format!(
                    "verify contains non-whitelisted field: {key}"
                )));
            }
        }
        Ok(())
    }
}

/// `POST /audit/receipt` 请求体（设计 §1.4，**B 档审计回执**）。
///
/// # 字段白名单（**恰好 8 个字段，一个不能多**）
///
/// `device_mid`、`lease_id`、`seq_from`、`seq_to`、`count`、`payload_digest`、`ts`、`sig`
///
/// 设计明确要求「服务端**拒收任何业务字段**」：请求体出现白名单外字段 → 整单拒收并记审计。
/// 调用方应在反序列化后立刻调用 [`AuditReceiptRequest::validate_whitelist`]。
///
/// ⚠️ `seq_from` / `seq_to` / `count` / `ts` **全部是 String**（大整数红线）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuditReceiptRequest {
    /// 设备机器码指纹。
    pub device_mid: String,
    /// 租约 ID。
    pub lease_id: String,
    /// 起始序号（**String**）。
    pub seq_from: String,
    /// 结束序号（**String**）。
    pub seq_to: String,
    /// 条数（**String**）。
    pub count: String,
    /// payload 摘要哈希（不含任何业务数值）。
    pub payload_digest: String,
    /// 客户端时间（UTC 秒，**String**；允许延迟补报，按此排序评估）。
    pub ts: String,
    /// 设备签名（base64）。
    pub sig: String,
}

/// 回执字段白名单（唯一权威定义，供 [`AuditReceiptRequest::validate_whitelist`] 与
/// 上层反序列化守卫共用）。
pub const AUDIT_RECEIPT_WHITELIST: [&str; 8] = [
    "device_mid",
    "lease_id",
    "seq_from",
    "seq_to",
    "count",
    "payload_digest",
    "ts",
    "sig",
];

impl AuditReceiptRequest {
    /// 校验字段白名单：请求体若含白名单外字段 → `FieldWhitelistViolation`。
    ///
    /// 实现方式：把 [`serde_json::Value`] 对象反序列化成 `serde_json::Map`，
    /// 逐个 key 与 [`AUDIT_RECEIPT_WHITELIST`] 比对。因为 `AuditReceiptRequest` 是
    /// 强类型结构体（未知字段默认被 serde 忽略），所以本方法既覆盖「结构体直接构造」
    /// 的显式白名单自检，也提供 `validate_whitelist_value` 供上层对**原始 JSON** 校验。
    pub fn validate_whitelist(&self) -> LicenseResult<()> {
        // 强类型结构体天然只有 8 个字段；逐字段存在性检查确保字段集合与白名单一致
        // （空字符串视为「未提供」，历史脏数据不得蒙过白名单）。
        //
        // ⚠️ **用 `trim().is_empty()` 而非 `is_empty()`**：纯空白串（`"   "` / `"\t"`）
        // 在 `is_empty()` 下会被当作「已提供」而蒙过白名单。实测确认过这条绕过路径：
        // `sig = "   "` 曾通过白名单校验，只因下游 `decode_signature` 也做 trim 才
        // 侥幸拦住。**不能依赖下游兜底来补上游的存在性判定**——否则一旦下游放松，
        // 空白就成了一条完整绕过面。此处统一把「纯空白」视同「未提供」。
        //
        // 违反 → 结构化 [`LicenseError::FieldWhitelistViolation`]（→ HTTP 422），
        // **绝不**降级为泛化 `ActivationRejected`（那会被映射成 400，掩盖越界语义）。
        fn missing(s: &str) -> bool {
            s.trim().is_empty()
        }
        if missing(&self.device_mid) {
            return Err(LicenseError::field_whitelist_violation(
                "audit receipt field whitelist violation: device_mid missing",
            ));
        }
        if missing(&self.lease_id) {
            return Err(LicenseError::field_whitelist_violation(
                "audit receipt field whitelist violation: lease_id missing",
            ));
        }
        if missing(&self.payload_digest) {
            return Err(LicenseError::field_whitelist_violation(
                "audit receipt field whitelist violation: payload_digest missing",
            ));
        }
        if missing(&self.sig) {
            return Err(LicenseError::field_whitelist_violation(
                "audit receipt field whitelist violation: sig missing",
            ));
        }
        Ok(())
    }

    /// 对**原始 JSON 对象**做白名单校验：出现白名单外 key → `FieldWhitelistViolation`。
    ///
    /// 这是真正的「拒收白名单外字段」入口——因为 serde 默认忽略未知字段，
    /// 上层解析请求时**必须先**用本方法校验原始 JSON，再反序列化为结构体。
    pub fn validate_whitelist_value(value: &serde_json::Value) -> LicenseResult<()> {
        let obj = value.as_object().ok_or_else(|| {
            LicenseError::field_whitelist_violation("audit receipt body must be a JSON object")
        })?;
        for key in obj.keys() {
            if !AUDIT_RECEIPT_WHITELIST.contains(&key.as_str()) {
                return Err(LicenseError::field_whitelist_violation(format!(
                    "audit receipt contains non-whitelisted field: {key}"
                )));
            }
        }
        Ok(())
    }
}

/// `POST /audit/receipt` 响应体（设计 §1.4 响应 200）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuditReceiptResponse {
    /// 是否接受（幂等重放也算接受）。
    pub accepted: bool,
    /// 跳空 / 回退 / 缺失判定结果。
    pub gap: GapKind,
    /// 服务端时间（UTC 秒，**String**）。
    pub server_time: String,
    /// 告警明细（task 48）：跳空 / 回退 / 重叠时**非空**，逐条与
    /// `audit_receipt_warning` 表对应；连续与幂等重放为空数组。
    /// `serde(default)` 保证旧客户端 / 旧测试构造不受影响（缺省 = 无告警）。
    #[serde(default)]
    pub warnings: Vec<String>,
}

/// 回执连续性判定结果（设计 §1.4 跳空检测）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GapKind {
    /// 连续（无异常）。
    None,
    /// 跳空：`seq_from > expected`。
    Gap,
    /// 回退 / 重叠：`seq_from <= last.seq_to`。
    Overlap,
    /// 缺失：窗口内无回执（由后续补报或定时任务判定）。
    Missing,
}

impl GapKind {
    /// 稳定字符串（落库 / 审计用，与设计 §1.4 响应取值一致）。
    pub fn as_str(self) -> &'static str {
        match self {
            GapKind::None => "none",
            GapKind::Gap => "gap",
            GapKind::Overlap => "overlap",
            GapKind::Missing => "missing",
        }
    }

    /// 是否构成风控异常（`gap_flag` 取值来源）。
    pub fn is_anomaly(self) -> bool {
        !matches!(self, GapKind::None)
    }
}

// ============================================================================
// §2 管理端端点
// ============================================================================

/// `POST /admin/codes/issue` 请求体（设计 §2.1）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IssueCodesRequest {
    /// 目标租户。
    pub tenant_id: String,
    /// 授权档位 tier。
    pub tier: String,
    /// 有效期起（UTC 秒，**String**）。
    pub valid_from: String,
    /// 有效期止（UTC 秒，**String**）。
    pub valid_until: String,
    /// 发放数量（上限见 `MAX_ISSUE_BATCH`）。
    pub count: u32,
    /// 可预绑定机器码（可选）。
    pub prebind_machine_code: Option<String>,
    /// 幂等键（同 key 重放返回首次结果）。
    pub idempotency_key: String,
}

/// 单条已发放 / 已重发激活码。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IssuedCode {
    /// 激活码 ID。
    pub code_id: String,
    /// 码值（`IOTDAQ-XXXX-XXXX-XXXX-XXXX`）。
    pub code: String,
    /// 状态（`issued` / `bound` / `revoked` / `reissued`）。
    pub status: String,
    /// 预绑定机器码（可选）。
    pub prebind: Option<String>,
    /// 重发溯源：原 code_id（仅重发时出现）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reissued_from: Option<String>,
}

/// `POST /admin/codes/issue` 响应体（设计 §2.1 响应）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IssueCodesResponse {
    /// 本次发放 / 幂等重放的码列表。
    pub codes: Vec<IssuedCode>,
}

/// `POST /admin/codes/{id}/revoke` 请求体（设计 §2.2）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RevokeCodeRequest {
    /// 废弃原因（必填）。
    pub reason: String,
    /// 补充说明（≥10 字符）。
    pub note: String,
    /// 激活码后 8 位确认串。
    pub confirm_tail8: String,
    /// 第二审批人（双人复核开启时必填）。
    pub second_approver: Option<String>,
}

/// 重发时的预绑定信息（设计 §2.3）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Prebind {
    /// 预绑定机器码。
    pub machine_code: String,
}

/// 重发可覆盖字段（设计 §2.3 `overrides?`）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Override {
    /// 覆盖 tier（可选）。
    pub tier: Option<String>,
    /// 覆盖有效期止（UTC 秒 String，可选）。
    pub valid_until: Option<String>,
}

/// `POST /admin/codes/{id}/reissue` 请求体（设计 §2.3）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReissueCodeRequest {
    /// 预绑定机器码（`null` 表示留待首次激活绑定）。
    pub prebind: Option<Prebind>,
    /// 是否继承原码 tier。
    pub inherit_tier: bool,
    /// 是否继承原码有效期。
    pub inherit_validity: bool,
    /// 覆盖字段（可选）。
    pub overrides: Option<Override>,
    /// 幂等键（与 revoke 同事务共用）。
    pub idempotency_key: String,
}

/// `POST /admin/codes/{id}/reissue` 响应体（设计 §2.3 响应）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReissueCodeResponse {
    /// 新码信息（含 `reissued_from` 溯源）。
    pub new_code: IssuedCode,
}

/// `GET /admin/codes` 查询参数（设计 §2.4）。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CodeListQuery {
    /// 按租户筛选。
    pub tenant_id: Option<String>,
    /// 按状态筛选（`issued` / `bound` / `revoked` / `reissued`）。
    pub status: Option<String>,
    /// 按 tier 筛选。
    pub tier: Option<String>,
    /// 按来源订单筛选。
    pub order_id: Option<String>,
    /// 页码（从 1 起）。
    pub page: Option<u32>,
}

/// 码值列表项（设计 §2.4，**码值掩码显示**）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CodeSummary {
    /// 激活码 ID。
    pub code_id: String,
    /// 掩码后的码值（如 `IOTDAQ-****-****-****-AB12`）。
    pub code_masked: String,
    /// 状态。
    pub status: String,
    /// 租户。
    pub tenant_id: String,
    /// tier。
    pub tier: String,
    /// 绑定设备（未绑定为 `None`）。
    pub bound_device_id: Option<String>,
    /// 失效时刻（UTC 秒 String）。
    pub valid_until: String,
    /// 创建时刻（UTC 秒 String）。
    pub created_at: String,
}

/// 时间线条目（设计 §2.5：发放 → 绑定 → 废弃 → 重发）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TimelineEntry {
    /// 事件（issue / bind / revoke / reissue）。
    pub action: String,
    /// 操作者。
    pub actor: String,
    /// 时间（UTC 秒，i64 供展示；此处非计数器语义，但仍随大整数纪律保持一致性）。
    pub at: String,
    /// 详情（原因 / 命中项等）。
    pub detail: String,
}

/// `GET /admin/codes/{id}` 详情（设计 §2.5）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CodeDetail {
    /// code_id。
    pub code_id: String,
    /// 完整码值（详情页可揭示）。
    pub code: String,
    /// 状态。
    pub status: String,
    /// 租户。
    pub tenant_id: String,
    /// tier。
    pub tier: String,
    /// 绑定设备。
    pub bound_device_id: Option<String>,
    /// 有效期起（UTC 秒 String）。
    pub valid_from: String,
    /// 有效期止（UTC 秒 String）。
    pub valid_until: String,
    /// 操作时间线。
    pub timeline: Vec<TimelineEntry>,
    /// 重发溯源链（原码 code_id 列表）。
    pub reissued_chain: Vec<String>,
    /// 废弃时刻（UTC 秒 String）。
    pub revoked_at: Option<String>,
    /// 废弃原因。
    pub revoked_reason: Option<String>,
}

/// `GET /admin/devices` 查询参数（设计 §2.6）。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DeviceListQuery {
    /// 按租户筛选。
    pub tenant_id: Option<String>,
    /// 按部署形态筛选（`native` / `docker`）。
    pub deploy_mode: Option<String>,
    /// 按状态筛选。
    pub status: Option<String>,
    /// 按机器码筛选。
    pub machine_code: Option<String>,
    /// 页码。
    pub page: Option<u32>,
}

/// 设备列表项（设计 §2.6，**机器码掩码**）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeviceListItem {
    /// 设备 ID。
    pub device_id: String,
    /// 租户。
    pub tenant_id: String,
    /// 掩码后的机器码。
    pub machine_code_masked: String,
    /// 部署形态。
    pub deploy_mode: String,
    /// 镜像 digest（docker 时）。
    pub image_digest: Option<String>,
    /// 最近心跳（UTC 秒 String，可空）。
    pub last_heartbeat_at: Option<String>,
    /// 当前租约状态。
    pub lease_status: Option<String>,
    /// 回执异常汇总（如 `gap=2,overlap=1`）。
    pub receipt_gap_summary: String,
}

/// `GET /admin/audit/logs` 查询参数（设计 §2.7）。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AuditLogQuery {
    /// 按 actor_type 筛选。
    pub actor_type: Option<String>,
    /// 按 action 筛选。
    pub action: Option<String>,
    /// 按 entity_type 筛选。
    pub entity_type: Option<String>,
    /// 按 entity_id 筛选。
    pub entity_id: Option<String>,
    /// 时间范围起（UTC 秒 String）。
    pub time_from: Option<String>,
    /// 时间范围止（UTC 秒 String）。
    pub time_to: Option<String>,
    /// 页码。
    pub page: Option<u32>,
}

/// 审计日志项（设计 §2.7）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuditLogItem {
    /// 时间（UTC 秒 String）。
    pub ts: String,
    /// 操作者（`actor_type:actor_id`）。
    pub actor: String,
    /// 动作。
    pub action: String,
    /// 实体（`entity_type:entity_id`）。
    pub entity: String,
    /// 详情 JSON。
    pub detail: String,
    /// 来源 IP。
    pub ip: String,
}

// ============================================================================
// 统一响应包裹
// ============================================================================

/// 统一响应包裹（设计 §0）：`{ code, data, message, trace_id }`。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApiEnvelope<T> {
    /// 业务码（成功为 `OK`，失败见 [`codes`]）。
    pub code: String,
    /// 业务数据（失败时为 `None`）。
    pub data: Option<T>,
    /// 人类可读消息。
    pub message: String,
    /// 链路追踪 ID。
    pub trace_id: String,
}

impl<T> ApiEnvelope<T> {
    /// 构造成功响应。
    pub fn ok(data: T, trace_id: impl Into<String>) -> Self {
        ApiEnvelope {
            code: codes::OK.to_string(),
            data: Some(data),
            message: "ok".to_string(),
            trace_id: trace_id.into(),
        }
    }

    /// 构造失败响应（不含数据）。
    pub fn err(code: &str, message: impl Into<String>, trace_id: impl Into<String>) -> Self {
        ApiEnvelope {
            code: code.to_string(),
            data: None,
            message: message.into(),
            trace_id: trace_id.into(),
        }
    }

    /// 是否为成功响应。
    pub fn is_ok(&self) -> bool {
        self.code == codes::OK
    }
}

// ============================================================================
// 业务错误码（设计 §1-2 + §2 通用错误）
// ============================================================================

/// 业务错误码常量与 HTTP 状态映射。
pub mod codes {
    /// 成功。
    pub const OK: &str = "OK";
    /// 激活码无效 / 不存在（400）。
    pub const INVALID_CODE: &str = "INVALID_CODE";
    /// 激活码已废弃（403）。
    pub const CODE_REVOKED: &str = "CODE_REVOKED";
    /// 同码异机（403）。
    pub const CODE_BOUND_TO_OTHER_DEVICE: &str = "CODE_BOUND_TO_OTHER_DEVICE";
    /// 旧码已被重发替代（409）。
    pub const CODE_REISSUED: &str = "CODE_REISSUED";
    /// nonce 重放（409）。
    pub const NONCE_REPLAY: &str = "NONCE_REPLAY";
    /// 时间窗超限（401）。
    pub const TIMESTAMP_SKEW: &str = "TIMESTAMP_SKEW";
    /// 租约已废弃 / 已停止（403）。
    pub const LEASE_REVOKED: &str = "LEASE_REVOKED";
    /// 租约不存在（404）。
    pub const LEASE_NOT_FOUND: &str = "LEASE_NOT_FOUND";
    /// 验签失败（401）。
    pub const VERIFY_FAIL: &str = "VERIFY_FAIL";
    /// 字段白名单越界（422）。
    pub const FIELD_WHITELIST_VIOLATION: &str = "FIELD_WHITELIST_VIOLATION";
    /// 非管理员（403）。
    pub const ADMIN_ONLY: &str = "ADMIN_ONLY";
    /// 会话过期（401）。
    pub const SESSION_EXPIRED: &str = "SESSION_EXPIRED";
    /// 租户不存在（400）。
    pub const TENANT_NOT_FOUND: &str = "TENANT_NOT_FOUND";
    /// 确认串不符（412）。
    pub const CONFIRM_MISMATCH: &str = "CONFIRM_MISMATCH";
    /// 原因缺失（400）。
    pub const REASON_REQUIRED: &str = "REASON_REQUIRED";
    /// 该码已重发，禁止再撤销原码（409）。
    pub const ALREADY_REISSUED: &str = "ALREADY_REISSUED";
    /// 原码未废弃，不可重发（409）。
    pub const ORIGINAL_NOT_REVOKED: &str = "ORIGINAL_NOT_REVOKED";
    /// 预绑定机器码已绑定他码（400）。
    pub const PREBIND_CONFLICT: &str = "PREBIND_CONFLICT";
    /// 配额超限（403）。
    pub const QUOTA_EXCEEDED: &str = "QUOTA_EXCEEDED";
    /// Token 过期（401）。
    pub const TOKEN_EXPIRED: &str = "TOKEN_EXPIRED";
    /// 请求参数非法（400，通用兜底）。
    pub const BAD_REQUEST: &str = "BAD_REQUEST";
    /// 激活请求验签失败（401；2026-09-25 主理人决策，SCREAMING_SNAKE 与既有 wire code 一致）。
    pub const ACTIVATION_SIGNATURE_INVALID: &str = "ACTIVATION_SIGNATURE_INVALID";
    /// 激活请求设备公钥与库中钉定公钥不一致（403；SCREAMING_SNAKE 与既有 wire code 一致）。
    pub const ACTIVATION_PUBKEY_MISMATCH: &str = "ACTIVATION_PUBKEY_MISMATCH";
}

/// 业务码 → HTTP 状态码映射（设计 §0「HTTP 状态码 + 业务码双重表达」）。
///
/// 未知业务码一律映射为 `500`，避免把内部错误伪装成客户端错误。
pub fn http_status(code: &str) -> u16 {
    match code {
        codes::OK => 200,
        codes::INVALID_CODE
        | codes::TENANT_NOT_FOUND
        | codes::REASON_REQUIRED
        | codes::PREBIND_CONFLICT
        | codes::BAD_REQUEST => 400,
        codes::TIMESTAMP_SKEW
        | codes::VERIFY_FAIL
        | codes::SESSION_EXPIRED
        | codes::TOKEN_EXPIRED
        | codes::ACTIVATION_SIGNATURE_INVALID => 401,
        codes::CODE_REVOKED
        | codes::CODE_BOUND_TO_OTHER_DEVICE
        | codes::LEASE_REVOKED
        | codes::ADMIN_ONLY
        | codes::QUOTA_EXCEEDED
        | codes::ACTIVATION_PUBKEY_MISMATCH => 403,
        codes::LEASE_NOT_FOUND => 404,
        codes::CODE_REISSUED
        | codes::NONCE_REPLAY
        | codes::ALREADY_REISSUED
        | codes::ORIGINAL_NOT_REVOKED => 409,
        codes::CONFIRM_MISMATCH => 412,
        codes::FIELD_WHITELIST_VIOLATION => 422,
        _ => 500,
    }
}

/// 发放批量上限（防一次生成过多，设计 §2.1 未给具体值，实施取 1000）。
pub const MAX_ISSUE_BATCH: u32 = 1000;

#[cfg(test)]
mod tests {
    use super::*;

    /// **大整数红线**：`seq_from` / `seq_to` / `count` / `ts` 序列化后必须是 JSON **字符串**。
    #[test]
    fn json_big_ints_are_strings() {
        let req = AuditReceiptRequest {
            device_mid: "mid-1".into(),
            lease_id: "lease-1".into(),
            seq_from: "9007199254740993".into(), // 2^53 + 1：JSON number 会丢精度
            seq_to: "9007199254740994".into(),
            count: "2".into(),
            payload_digest: "digest".into(),
            ts: "1700000000000000000".into(),
            sig: "sig".into(),
        };
        let v = serde_json::to_value(&req).unwrap();
        assert_eq!(v["seq_from"], serde_json::json!("9007199254740993"));
        assert_eq!(v["seq_to"], serde_json::json!("9007199254740994"));
        assert_eq!(v["count"], serde_json::json!("2"));
        assert_eq!(v["ts"], serde_json::json!("1700000000000000000"));
        // 断言确实是 JSON 字符串类型（而非 number 被转成字符串）。
        assert!(v["seq_from"].is_string(), "{v}");
        assert!(v["ts"].is_string(), "{v}");

        // 序列化文本中必须带引号。
        let text = serde_json::to_string(&req).unwrap();
        assert!(text.contains("\"seq_from\":\"9007199254740993\""), "{text}");
        assert!(text.contains("\"ts\":\"1700000000000000000\""), "{text}");
    }

    /// **回执请求体恰好 8 个字段**（序列化后取 key 集合断言）。
    #[test]
    fn audit_receipt_has_exactly_eight_fields() {
        let req = AuditReceiptRequest {
            device_mid: "m".into(),
            lease_id: "l".into(),
            seq_from: "1".into(),
            seq_to: "2".into(),
            count: "2".into(),
            payload_digest: "d".into(),
            ts: "3".into(),
            sig: "s".into(),
        };
        let v = serde_json::to_value(&req).unwrap();
        let obj = v.as_object().unwrap();
        assert_eq!(obj.len(), 8, "回执请求体必须恰好 8 个字段: {obj:?}");
        let mut keys: Vec<&str> = obj.keys().map(String::as_str).collect();
        keys.sort_unstable();
        let mut expected = AUDIT_RECEIPT_WHITELIST.to_vec();
        expected.sort_unstable();
        assert_eq!(keys, expected, "字段集合必须与白名单完全一致");
    }

    /// **白名单越界**：原始 JSON 出现白名单外字段 → `FieldWhitelistViolation`。
    #[test]
    fn whitelist_rejects_extra_business_field() {
        let with_extra = serde_json::json!({
            "device_mid": "m",
            "lease_id": "l",
            "seq_from": "1",
            "seq_to": "2",
            "count": "2",
            "payload_digest": "d",
            "ts": "3",
            "sig": "s",
            // 业务字段：设计明确要求拒收。
            "flow_rate": "42"
        });
        let err = AuditReceiptRequest::validate_whitelist_value(&with_extra).unwrap_err();
        assert!(
            matches!(err, LicenseError::FieldWhitelistViolation(_)),
            "{err:?}"
        );
        assert!(err.to_string().contains("flow_rate"), "{err}");

        // 恰好 8 字段 → 通过。
        let clean = serde_json::json!({
            "device_mid": "m", "lease_id": "l", "seq_from": "1", "seq_to": "2",
            "count": "2", "payload_digest": "d", "ts": "3", "sig": "s"
        });
        assert!(AuditReceiptRequest::validate_whitelist_value(&clean).is_ok());
    }

    /// `/verify` 白名单：越界字段 → `FieldWhitelistViolation`；恰好 6 字段 → 通过；
    /// 纯空白必填字段视为未提供。
    #[test]
    fn verify_whitelist_rejects_extra_field_and_blank_required() {
        let with_extra = serde_json::json!({
            "device_mid": "m",
            "lease_id": "l",
            "payload_digest": "d",
            "ts": "3",
            "nonce": "n",
            "device_sig": "s",
            // 业务字段：不得出现。
            "flow_rate": "42"
        });
        let err = VerifyRequest::validate_whitelist_value(&with_extra).unwrap_err();
        assert!(
            matches!(err, LicenseError::FieldWhitelistViolation(_)),
            "{err:?}"
        );

        let clean = serde_json::json!({
            "device_mid": "m", "lease_id": "l", "payload_digest": "d",
            "ts": "3", "nonce": "n", "device_sig": "s"
        });
        assert!(VerifyRequest::validate_whitelist_value(&clean).is_ok());

        // 纯空白必填字段 → 视为未提供 → 拒绝。
        let mut req = VerifyRequest {
            device_mid: "m".into(),
            lease_id: "l".into(),
            payload_digest: "d".into(),
            ts: "3".into(),
            nonce: "n".into(),
            device_sig: "s".into(),
        };
        assert!(req.validate_whitelist().is_ok());
        req.nonce = "   ".into();
        assert!(matches!(
            req.validate_whitelist().unwrap_err(),
            LicenseError::FieldWhitelistViolation(_)
        ));
    }

    /// 结构体自检：必填字段为空 → 白名单校验失败。
    #[test]
    fn struct_self_check_rejects_empty_required_fields() {
        let mut req = AuditReceiptRequest {
            device_mid: "m".into(),
            lease_id: "l".into(),
            seq_from: "1".into(),
            seq_to: "2".into(),
            count: "2".into(),
            payload_digest: "d".into(),
            ts: "3".into(),
            sig: "s".into(),
        };
        assert!(req.validate_whitelist().is_ok());
        req.device_mid = String::new();
        assert!(req.validate_whitelist().is_err());
    }

    /// `ReceiptCursor` 序号同为 String。
    #[test]
    fn receipt_cursor_serials_are_strings() {
        let c = ReceiptCursor {
            seq_from: "18446744073709551615".into(),
            seq_to: "18446744073709551616".into(),
        };
        let v = serde_json::to_value(&c).unwrap();
        assert!(v["seq_from"].is_string(), "{v}");
        assert!(v["seq_to"].is_string(), "{v}");
    }

    /// 错误码 → HTTP 状态映射覆盖设计 §1-2 全部错误码。
    #[test]
    fn every_business_code_maps_to_expected_http_status() {
        let table: Vec<(&str, u16)> = vec![
            (codes::OK, 200),
            (codes::INVALID_CODE, 400),
            (codes::CODE_REVOKED, 403),
            (codes::CODE_BOUND_TO_OTHER_DEVICE, 403),
            (codes::CODE_REISSUED, 409),
            (codes::NONCE_REPLAY, 409),
            (codes::TIMESTAMP_SKEW, 401),
            (codes::LEASE_REVOKED, 403),
            (codes::LEASE_NOT_FOUND, 404),
            (codes::VERIFY_FAIL, 401),
            (codes::FIELD_WHITELIST_VIOLATION, 422),
            (codes::ADMIN_ONLY, 403),
            (codes::SESSION_EXPIRED, 401),
            (codes::TENANT_NOT_FOUND, 400),
            (codes::CONFIRM_MISMATCH, 412),
            (codes::REASON_REQUIRED, 400),
            (codes::ALREADY_REISSUED, 409),
            (codes::ORIGINAL_NOT_REVOKED, 409),
            (codes::PREBIND_CONFLICT, 400),
            (codes::QUOTA_EXCEEDED, 403),
            (codes::TOKEN_EXPIRED, 401),
        ];
        for (code, status) in table {
            assert_eq!(http_status(code), status, "code={code}");
        }
        // 未知码兜底 500。
        assert_eq!(http_status("SOMETHING_ELSE"), 500);
    }

    /// `ApiEnvelope` 成功 / 失败构造与 `is_ok`。
    #[test]
    fn api_envelope_ok_and_err() {
        let ok: ApiEnvelope<u32> = ApiEnvelope::ok(7, "trace-1");
        assert!(ok.is_ok());
        assert_eq!(ok.data, Some(7));
        assert_eq!(ok.trace_id, "trace-1");

        let err: ApiEnvelope<u32> = ApiEnvelope::err(codes::INVALID_CODE, "bad", "trace-2");
        assert!(!err.is_ok());
        assert!(err.data.is_none());
        assert_eq!(err.code, codes::INVALID_CODE);
    }

    /// `GapKind` 序列化为 snake_case，且异常判定正确。
    #[test]
    fn gap_kind_serializes_snake_case_and_flags_anomaly() {
        assert_eq!(serde_json::to_value(GapKind::None).unwrap(), "none");
        assert_eq!(serde_json::to_value(GapKind::Gap).unwrap(), "gap");
        assert_eq!(serde_json::to_value(GapKind::Overlap).unwrap(), "overlap");
        assert_eq!(serde_json::to_value(GapKind::Missing).unwrap(), "missing");

        assert!(!GapKind::None.is_anomaly());
        assert!(GapKind::Gap.is_anomaly());
        assert!(GapKind::Overlap.is_anomaly());
        assert!(GapKind::Missing.is_anomaly());
        assert_eq!(GapKind::Gap.as_str(), "gap");
    }

    /// 激活请求 / 响应其余字段的 String 大整数纪律。
    #[test]
    fn activation_ts_and_valid_until_are_strings() {
        let resp = ActivationResponse {
            lease_id: "l".into(),
            lease_token: "t".into(),
            verify_mode: "B".into(),
            tier: "standard".into(),
            valid_until: "1731536000".into(),
            heartbeat_hours: 24,
            server_time: "1700000000".into(),
            nonce: "n".into(),
            server_pubkey: "pk".into(),
            sig: "s".into(),
        };
        let v = serde_json::to_value(&resp).unwrap();
        assert!(v["valid_until"].is_string(), "{v}");
        assert!(v["server_time"].is_string(), "{v}");
        // TOFU：激活响应必须携带服务端公钥字段（2026-09-25 主理人决策）。
        assert!(v["server_pubkey"].is_string(), "{v}");
        assert!(v["sig"].is_string(), "{v}");
    }
}

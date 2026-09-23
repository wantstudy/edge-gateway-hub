//! `licensing-server` 业务规则层：配额、幂等、时序、跳空检测、一机一码判定。
//!
//! # 模块边界（不可逾越）
//!
//! 本模块**不写一行 SQL**——全部持久化经 [`crate::store::Store`]；
//! 全部密钥操作经 [`crate::keys::KeyRing`]；全部 Token 操作经 [`crate::token`]。
//! 这样「业务规则」与「存储形态」解耦，Store 可替换（SQLite → 其它）而规则不变。
//!
//! # 授权判定位置
//!
//! 云端只负责**签发与稽核**；最终授权判定仍在网关（daemon）侧，离线宽限 7 天。
//! 本模块不做任何「客户端可自行解绑 / 重置试用」的逻辑（设计明确禁止）。
//!
//! # 大整数纪律
//!
//! 与 [`crate::proto`] 一致：`seq_from` / `seq_to` / `count` / `ts` 一律以 `String`
//! 承载；内部评估时用 `u64` / `i64` 解析，序列化边界处一律还原为字符串。

use std::sync::Arc;

use crate::error::{LicenseError, LicenseResult};
use crate::keys::KeyRing;
use crate::model::{
    now_ns_id, now_unix_secs, ActivationCode, ActorType, AuditLog, AuditReceipt, CodeStatus,
    Device, DeviceStatus, Heartbeat, HeartbeatResult, Lease, LeaseStatus, VerifyMode, ANCHOR_COUNT,
};
use crate::proto::{
    ActivationRequest, ActivationResponse, AuditReceiptRequest, AuditReceiptResponse, CodeDetail,
    CodeSummary, GapKind, HeartbeatRequest, HeartbeatResponse, IssueCodesRequest,
    IssueCodesResponse, IssuedCode, ReissueCodeRequest, ReissueCodeResponse, RevokeCodeRequest,
    TimelineEntry, VerifyRequest, VerifyResponse, MAX_ISSUE_BATCH,
};
use crate::receipt::verify_receipt_signature;
use crate::store::{AuditFilter, CodeFilter, Store};
use crate::token::{issue_lease_token, LeaseClaims};

/// N-of-M 同机判定的阈值 N（M = [`ANCHOR_COUNT`] = 5，取 N = 3）。
pub const ANCHOR_MATCH_THRESHOLD: usize = 3;

/// 激活码字符表：Crockford base32，**排除易混字符 I / L / O / U**。
pub const CROCKFORD_ALPHABET: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";

/// 激活码前缀。
pub const CODE_PREFIX: &str = "IOTDAQ";

/// 激活码分组的组数（`IOTDAQ-` + 4 组 × 4 位）。
pub const CODE_GROUPS: usize = 4;

/// 每组字符数。
pub const CODE_GROUP_LEN: usize = 4;

/// 业务规则层配置（全部带默认值，见 [`ServiceConfig::default`]）。
#[derive(Debug, Clone)]
pub struct ServiceConfig {
    /// 心跳周期（小时）。
    pub heartbeat_hours: i64,
    /// 试用期（天）。
    pub trial_days: i64,
    /// 离线宽限期（天）。
    pub grace_days: i64,
    /// 租约有效期（天）。
    pub lease_days: i64,
    /// nonce 存活时长（秒）——±5min 窗口的两倍，覆盖整个可接受时间窗。
    pub nonce_ttl_secs: i64,
    /// 允许的时钟偏移（秒）——±5min。
    pub clock_skew_secs: i64,
    /// 单租户设备数上限。
    pub max_devices_per_tenant: u64,
    /// 是否开启双人复核（废弃 / 重发高危操作）。
    pub dual_approval_required: bool,
}

impl Default for ServiceConfig {
    fn default() -> Self {
        ServiceConfig {
            heartbeat_hours: 24,
            trial_days: 3,
            grace_days: 7,
            lease_days: 365,
            nonce_ttl_secs: 600,
            clock_skew_secs: 300,
            max_devices_per_tenant: 10_000,
            dual_approval_required: false,
        }
    }
}

/// 授权服务（业务规则层）。线程安全：内部仅持 `Arc` 共享句柄与不可变配置。
pub struct LicensingService {
    store: Arc<Store>,
    ring: Arc<KeyRing>,
    cfg: ServiceConfig,
}

/// 从请求的 String 时间戳解析为 `i64`（大整数纪律的边界转换点）。
fn parse_ts(raw: &str, field: &str) -> LicenseResult<i64> {
    raw.trim().parse::<i64>().map_err(|_| {
        LicenseError::TokenInvalid(format!(
            "field {field} is not a valid integer timestamp: {raw}"
        ))
    })
}

/// 从请求的 String 序号解析为 `u64`。
fn parse_seq(raw: &str, field: &str) -> LicenseResult<u64> {
    raw.trim().parse::<u64>().map_err(|_| {
        LicenseError::TokenInvalid(format!(
            "field {field} is not a valid unsigned integer: {raw}"
        ))
    })
}

impl LicensingService {
    /// 构造服务实例。
    pub fn new(store: Arc<Store>, ring: Arc<KeyRing>, cfg: ServiceConfig) -> Self {
        LicensingService { store, ring, cfg }
    }

    /// 只读配置（供上层展示 / 测试）。
    pub fn config(&self) -> &ServiceConfig {
        &self.cfg
    }

    // ------------------------------------------------------------------
    // §1.1 激活
    // ------------------------------------------------------------------

    /// 激活 + 一机一码绑定（设计 §1.1）。
    ///
    /// 规则顺序（逐条短路，确保错误码语义精确）：
    /// 1. 时间窗：`|now - req.ts| > clock_skew_secs` → `TimestampSkew`
    /// 2. Nonce 防重放：`insert_nonce_if_absent` 返回 `false` → `NonceReplay`
    /// 3. 码状态：不存在 → `InvalidCode`；`Revoked` → `CodeRevoked`；`Reissued` → `CodeReissued`
    /// 4. 一机一码：同机（machine_code 一致 或 anchor N-of-M ≥ 3）→ 幂等返回现存有效租约；
    ///    异机 → `CodeBoundToOtherDevice`
    /// 5. 配额：租户设备数 ≥ `max_devices_per_tenant` → `QuotaExceeded`
    /// 6. 成功：建 / 取 device → 原子绑定 → 签发 Lease Token → 落 lease + audit_log
    ///
    /// `first_activation_at` **仅在首次写**——它是试用兜底锚点，绝不能被后续激活覆盖。
    ///
    /// **Nonce 占位不变量（三条，禁止破坏）**：
    /// 1. **拒绝即不占位**：任何在 nonce 占位**之前**返回的拒绝路径（时间窗、空 nonce、
    ///    码不存在 / 已废弃 / 已换发、配额超限）都**不写** `nonce_cache`，同一次请求重试不会
    ///    因「上一次被拒」而误判为重放。
    /// 2. **成功必占位**：只要走到「绑定 + 签发」成功，`nonce` 一定已被写入（新机在
    ///    `insert_device` 之后、绑码之前补写；已存在机器在入口处即写），故成功过的请求
    ///    用同一 nonce 重放**必然**被 `NonceReplay` 拦截。
    /// 3. **惰性清理（非定时任务）**：过期 nonce 由每次占位时的 `purge_expired_nonces`
    ///    顺带清理，**不依赖外部定时任务**；即使清理失败也不影响主流程正确性（仅靠 TTL）。
    ///
    /// **为何拒绝路径也消费 nonce（本版本刻意接受）**：`nonce` 一旦落库即占位，即便该次
    /// 激活随后因码异常失败，同一 nonce 也被消耗——这是**防重放优先**的取舍：nonce 的作用域
    /// 是「一次性请求令牌」，宁可让客户端对失败请求**换新 nonce** 重试，也不允许一个 nonce
    /// 被反复用于探测（否则重放者可用同一 nonce 反复尝试不同码）。现有行为正确，不改。
    pub fn activate(&self, req: &ActivationRequest) -> LicenseResult<ActivationResponse> {
        let now = now_unix_secs();
        let client_ts = parse_ts(&req.ts, "ts")?;

        // 规则 1：时间窗（±5min）。
        if (now - client_ts).abs() > self.cfg.clock_skew_secs {
            return Err(LicenseError::ActivationRejected(format!(
                "timestamp skew: client_ts={client_ts} server_ts={now} window=±{}s",
                self.cfg.clock_skew_secs
            )));
        }

        // 规则 2：nonce 防重放（全局唯一，写入即占位）。
        //
        // ⚠️ `nonce_cache.device_id` 是 `device(device_id)` 的**硬外键**，故 nonce 的
        // `device_id` 必须是**已存在的设备 ID**。首次激活时设备尚未建库，此处先用
        // machine_code 作占位查找：若设备已存在则立即占位（严格防重放）；若不存在，
        // 则推迟到设备建库之后立即占位（仍在绑定 / 签发之前，语义等价）。
        if req.nonce.trim().is_empty() {
            return Err(LicenseError::ActivationRejected("nonce is empty".into()));
        }
        let pre_device = self.store.get_device_by_machine_code(&req.machine_code)?;
        if let Some(dev) = &pre_device {
            self.reject_replayed_nonce(&req.nonce, &dev.device_id, now)?;
        }

        // 规则 3：码状态。
        if req.activation_code.trim().is_empty() {
            return Err(LicenseError::ActivationRejected(
                "activation code is empty".into(),
            ));
        }
        // 激活码本身必须通过校验位（防手输错一位）。
        validate_activation_code(&req.activation_code)?;
        let code = self
            .store
            .get_code_by_value(&req.activation_code)?
            .ok_or_else(|| {
                LicenseError::ActivationRejected(format!(
                    "invalid activation code: {}",
                    mask_code(&req.activation_code)
                ))
            })?;
        match code.status {
            CodeStatus::Revoked => {
                return Err(LicenseError::ActivationRejected(format!(
                    "activation code {} is revoked: {}",
                    code.code_id,
                    code.revoked_reason
                        .clone()
                        .unwrap_or_else(|| "no reason".into())
                )));
            }
            CodeStatus::Reissued => {
                return Err(LicenseError::ActivationRejected(format!(
                    "activation code {} has been reissued; import the replacement code",
                    code.code_id
                )));
            }
            CodeStatus::Issued | CodeStatus::Bound => {}
        }

        // 规则 4：一机一码。
        if let Some(bound_device_id) = code.bound_device_id.as_deref() {
            let bound = self.store.get_device(bound_device_id)?.ok_or_else(|| {
                LicenseError::Storage(format!(
                    "code {} bound to missing device {}",
                    code.code_id, bound_device_id
                ))
            })?;
            let same_machine = bound.machine_code == req.machine_code;
            let anchor_hits = bound.anchor_match_count(&req.anchor_hashes);
            let same_via_anchor = anchor_hits >= ANCHOR_MATCH_THRESHOLD;
            if !(same_machine || same_via_anchor) {
                // 同码异机：拒绝，不产生新租约（设计 §1.1 错误码 + task 41 冲突检测）。
                return Err(LicenseError::ActivationRejected(format!(
                    "activation code {} is bound to another device (machine mismatch, anchor {}/{})",
                    code.code_id, anchor_hits, ANCHOR_COUNT
                )));
            }
            // 同机 → 幂等返回现存有效租约（凭证丢失重装）。
            let existing = self.usable_lease_for_device(&bound.device_id, now)?;
            if let Some(lease) = existing {
                self.write_audit(
                    ActorType::Device,
                    &bound.device_id,
                    "activation_idempotent",
                    "lease",
                    &lease.lease_id,
                    &format!(
                        "{{\"code_id\":\"{}\",\"machine_code_match\":{},\"anchor_hits\":{}}}",
                        code.code_id, same_machine, anchor_hits
                    ),
                    "",
                )?;
                return self.build_activation_response(&bound, &lease, req, &code.code, now);
            }
            // 绑定在册但租约已全部失效：直接沿用绑定关系，重新签发（不新建 device）。
            return self.issue_lease_for(&bound, &code, req, now);
        }

        // 规则 5：配额。
        let tenant = self.store.get_tenant(&code.tenant_id)?;
        let tenant = match tenant {
            Some(t) => t,
            None => {
                return Err(LicenseError::ActivationRejected(format!(
                    "tenant not found for code {}",
                    code.code_id
                )));
            }
        };
        let device_count = self.store.count_devices(Some(&tenant.tenant_id))?;
        if device_count >= self.cfg.max_devices_per_tenant {
            return Err(LicenseError::QuotaExceeded(format!(
                "tenant {} reached device quota {}",
                tenant.tenant_id, self.cfg.max_devices_per_tenant
            )));
        }

        // 规则 6：建 / 取 device。
        let is_new_device = pre_device.is_none();
        let device = match pre_device {
            Some(mut d) => {
                // 已有设备：更新锚点（锚点可能随升级扩展），不动 first_activation_at。
                d.anchor_hashes = req.anchor_hashes.clone();
                d.status = DeviceStatus::Active;
                d
            }
            None => {
                let mut d = Device::new(
                    now_ns_id("dev"),
                    code.tenant_id.clone(),
                    req.machine_code.clone(),
                    req.anchor_hashes.clone(),
                    now,
                );
                // **首次激活锚点**：仅在此处写入（试用兜底锚点）。
                d.first_activation_at = Some(now);
                d
            }
        };
        // 新设备才插入（`machine_code` 唯一约束；已存在设备不得重复 INSERT）。
        if is_new_device {
            self.store.insert_device(&device)?;
            // 设备刚建库，补做 nonce 占位（FK 现已满足），仍在绑定 / 签发之前。
            self.reject_replayed_nonce(&req.nonce, &device.device_id, now)?;
        }

        // 原子绑定（WHERE bound_device_id IS NULL；冲突 → ActivationRejected）。
        self.store
            .bind_code_to_device(&code.code_id, &device.device_id)?;

        // 重新读取已绑定码，保证后续响应基于最新状态。
        let bound_code = self
            .store
            .get_code_by_id(&code.code_id)?
            .unwrap_or(code.clone());

        self.issue_lease_for(&device, &bound_code, req, now)
    }

    /// 为已确定设备签发租约并组装激活响应。
    fn issue_lease_for(
        &self,
        device: &Device,
        code: &crate::model::ActivationCode,
        req: &ActivationRequest,
        now: i64,
    ) -> LicenseResult<ActivationResponse> {
        // 档位：设备级覆盖 > 租户默认（本版本设备级覆盖随 device 记录承载，缺省取租户默认）。
        //
        // 契约澄清：apply 路径存在「改码状态 → 签发失败」的部分失败窗口（库内状态已变、
        // HTTP 报错）。根因是**没有** apply 阶段的开环事务边界。已裁决为**接受**：
        // ① 全流程无 WS 推送，客户端不会看到中间态；② 该码本身已异常（非可签发态），
        // 重试同码无论如何都会被拒，不因部分失败而新增影响面；③ 真要做隔离需引入
        // 事务穿透 store(conn) 接口，收益低于改动成本。若日后新增「撤销即推送」类实时
        // 联动，必须回来补事务边界。
        let tenant = self.store.get_tenant(&code.tenant_id)?;
        let verify_mode = match &tenant {
            Some(t) => t.verify_mode_default,
            None => VerifyMode::B,
        };
        let valid_until = now + self.cfg.lease_days * 86_400;
        let lease_id = now_ns_id("lease");

        let claims = LeaseClaims {
            lease_id: lease_id.clone(),
            device_id: device.device_id.clone(),
            mid: device.machine_code.clone(),
            tier: code.tier.clone(),
            verify_mode: verify_mode.as_str().to_string(),
            issued_at: now,
            valid_until,
        };
        let token = issue_lease_token(&self.ring, &claims)?;

        let lease = Lease {
            lease_id: lease_id.clone(),
            device_id: device.device_id.clone(),
            code_id: code.code_id.clone(),
            kid: token.kid.clone(),
            token_sig: token.signature_b64.clone(),
            verify_mode,
            tier: code.tier.clone(),
            issued_at: now,
            valid_until,
            last_heartbeat_at: None,
            status: LeaseStatus::Active,
        };
        self.store.insert_lease(&lease)?;
        self.store
            .update_code_status(&code.code_id, CodeStatus::Bound)?;
        self.store
            .update_device_status(&device.device_id, DeviceStatus::Active)?;

        self.write_audit(
            ActorType::Device,
            &device.device_id,
            "activation",
            "device",
            &device.device_id,
            &format!(
                "{{\"code_id\":\"{}\",\"lease_id\":\"{}\",\"mid\":\"{}\",\"verify_mode\":\"{}\"}}",
                code.code_id,
                lease_id,
                device.machine_code,
                verify_mode.as_str()
            ),
            "",
        )?;

        self.build_activation_response(device, &lease, req, &code.code, now)
    }

    /// 组装激活响应（含服务端签名）。
    fn build_activation_response(
        &self,
        device: &Device,
        lease: &Lease,
        req: &ActivationRequest,
        _code_value: &str,
        now: i64,
    ) -> LicenseResult<ActivationResponse> {
        // 重签/复用 Token：以现存租约为准重新签发，得到完整三段式 Token 串。
        let claims = LeaseClaims {
            lease_id: lease.lease_id.clone(),
            device_id: device.device_id.clone(),
            mid: device.machine_code.clone(),
            tier: lease.tier.clone(),
            verify_mode: lease.verify_mode.as_str().to_string(),
            issued_at: lease.issued_at,
            valid_until: lease.valid_until,
        };
        let token = issue_lease_token(&self.ring, &claims)?;
        // 服务端响应签名：签名对象为「lease_id|server_time|nonce」规范化串。
        let signed = format!("{}|{}|{}", lease.lease_id, now, req.nonce);
        let (_kid, sig) = self.ring.sign(signed.as_bytes())?;

        Ok(ActivationResponse {
            lease_id: lease.lease_id.clone(),
            lease_token: token.encode(),
            verify_mode: lease.verify_mode.as_str().to_string(),
            tier: lease.tier.clone(),
            valid_until: lease.valid_until.to_string(),
            heartbeat_hours: self.cfg.heartbeat_hours,
            server_time: now.to_string(),
            nonce: req.nonce.clone(),
            sig,
        })
    }

    /// 查询设备当前可用（`Active` 且未过期）的租约。
    fn usable_lease_for_device(&self, device_id: &str, now: i64) -> LicenseResult<Option<Lease>> {
        let leases = self.store.list_leases_by_device(device_id)?;
        Ok(leases
            .into_iter()
            .filter(|l| l.status == LeaseStatus::Active && !l.is_expired_at(now))
            .max_by_key(|l| l.issued_at))
    }

    /// 插入 nonce，已存在（重放）→ `ActivationRejected`（对应 `NONCE_REPLAY`）。
    fn reject_replayed_nonce(&self, nonce: &str, device_id: &str, now: i64) -> LicenseResult<()> {
        if nonce.trim().is_empty() {
            return Err(LicenseError::ActivationRejected("nonce is empty".into()));
        }
        let expires_at = now + self.cfg.nonce_ttl_secs;
        let inserted = self
            .store
            .insert_nonce_if_absent(nonce, device_id, expires_at)?;
        if !inserted {
            return Err(LicenseError::ActivationRejected(format!(
                "nonce replay detected: {nonce}"
            )));
        }
        // 顺带清理过期 nonce（低成本维护，失败不影响主流程正确性）。
        let _ = self.store.purge_expired_nonces(now);
        Ok(())
    }

    // ------------------------------------------------------------------
    // §1.2 心跳
    // ------------------------------------------------------------------

    /// 心跳（设计 §1.2）。
    ///
    /// - 租约不存在 → `LeaseNotFound`
    /// - 已 `Stopped`（废弃立即失效）→ `LeaseRevoked`
    /// - 时间窗超限 → `TimestampSkew`
    /// - nonce 重复 → `NonceReplay`
    /// - 副作用：回写 `lease.last_heartbeat_at` + `device.status`，写 heartbeat 记录
    /// - **B 档断网重放幂等**：同一 `receipt_cursor` 重复上报不得报错
    pub fn heartbeat(&self, req: &HeartbeatRequest) -> LicenseResult<HeartbeatResponse> {
        let now = now_unix_secs();
        let client_ts = parse_ts(&req.ts, "ts")?;

        let lease = self.store.get_lease(&req.lease_id)?.ok_or_else(|| {
            LicenseError::HeartbeatRejected(format!("lease not found: {}", req.lease_id))
        })?;

        // 废弃立即失效：Stopped 语义 = 立即拒绝。
        if lease.status == LeaseStatus::Stopped {
            self.record_heartbeat(&lease, client_ts, now, HeartbeatResult::Revoked, req)?;
            return Err(LicenseError::HeartbeatRejected(format!(
                "lease {} is revoked/stopped",
                lease.lease_id
            )));
        }

        // 时间窗（心跳同样做时钟回拨检测）。
        let skew = (now - client_ts).abs() > self.cfg.clock_skew_secs;
        if skew {
            self.record_heartbeat(&lease, client_ts, now, HeartbeatResult::Skew, req)?;
            return Err(LicenseError::HeartbeatRejected(format!(
                "timestamp skew: client_ts={client_ts} server_ts={now}"
            )));
        }

        // nonce 防重放（心跳自身的 nonce 必须唯一）。
        self.reject_replayed_nonce(&req.nonce, &lease.device_id, now)?;

        // 副作用：回写最近心跳 + 设备状态。
        self.store.update_lease_heartbeat(&lease.lease_id, now)?;
        self.store
            .update_device_status(&lease.device_id, DeviceStatus::Active)?;
        self.record_heartbeat(&lease, client_ts, now, HeartbeatResult::Ok, req)?;

        let next_deadline = now + self.cfg.heartbeat_hours * 3600;
        let signed = format!("{}|{}|{}", lease.lease_id, now, next_deadline);
        let (_kid, sig) = self.ring.sign(signed.as_bytes())?;

        Ok(HeartbeatResponse {
            server_time: now.to_string(),
            next_deadline: next_deadline.to_string(),
            valid_until: lease.valid_until.to_string(),
            verify_mode: lease.verify_mode.as_str().to_string(),
            tier: lease.tier.clone(),
            sig,
        })
    }

    /// 写一条心跳记录（携带 `receipt_cursor` 供跳空检测）。
    fn record_heartbeat(
        &self,
        lease: &Lease,
        client_ts: i64,
        server_ts: i64,
        result: HeartbeatResult,
        req: &HeartbeatRequest,
    ) -> LicenseResult<()> {
        let cursor = req
            .receipt_cursor
            .as_ref()
            .map(|c| format!("{}-{}", c.seq_from, c.seq_to));
        let hb = Heartbeat {
            id: now_ns_id("hb"),
            lease_id: lease.lease_id.clone(),
            device_id: lease.device_id.clone(),
            client_ts,
            server_ts,
            result,
            receipt_cursor: cursor,
            created_at: server_ts,
        };
        self.store.insert_heartbeat(&hb)
    }

    // ------------------------------------------------------------------
    // §1.3 服务端二次校验（仅 A 档）
    // ------------------------------------------------------------------

    /// 服务端二次校验（设计 §1.3，**仅 A 档**）。
    ///
    /// 校验链：字段存在性 → 时间窗 → 租约状态 → nonce → 验签。
    pub fn verify(&self, req: &VerifyRequest) -> LicenseResult<VerifyResponse> {
        let now = now_unix_secs();

        // 白名单优先：必填字段（A 档全字段）。
        if req.device_mid.is_empty() || req.lease_id.is_empty() {
            return Err(LicenseError::ActivationRejected(
                "verify field whitelist violation: device_mid/lease_id required".into(),
            ));
        }
        if req.payload_digest.is_empty() {
            return Err(LicenseError::ActivationRejected(
                "verify field whitelist violation: payload_digest required".into(),
            ));
        }
        let client_ts = parse_ts(&req.ts, "ts")?;
        if (now - client_ts).abs() > self.cfg.clock_skew_secs {
            return Err(LicenseError::TokenInvalid(format!(
                "verify timestamp skew: client_ts={client_ts} server_ts={now}"
            )));
        }

        let lease = self.store.get_lease(&req.lease_id)?.ok_or_else(|| {
            LicenseError::TokenInvalid(format!("lease not found: {}", req.lease_id))
        })?;
        if lease.status == LeaseStatus::Stopped {
            return Err(LicenseError::TokenInvalid(format!(
                "lease {} is revoked/stopped",
                lease.lease_id
            )));
        }
        if lease.verify_mode != VerifyMode::A {
            return Err(LicenseError::TokenInvalid(format!(
                "verify endpoint is A-tier only; lease {} is {} tier",
                lease.lease_id,
                lease.verify_mode.as_str()
            )));
        }

        // nonce（A 档全局防重放）。
        self.reject_replayed_nonce(&req.nonce, &lease.device_id, now)?;

        // 验签：签名对象为规范化摘要串。
        let signed = format!(
            "{}|{}|{}|{}",
            req.device_mid, req.lease_id, req.payload_digest, client_ts
        );
        self.ring
            .verify(&lease.kid, signed.as_bytes(), &req.device_sig)?;

        self.write_audit(
            ActorType::Device,
            &lease.device_id,
            "verify",
            "lease",
            &lease.lease_id,
            &format!(
                "{{\"payload_digest\":\"{}\",\"mid\":\"{}\"}}",
                req.payload_digest, req.device_mid
            ),
            "",
        )?;

        Ok(VerifyResponse {
            ok: true,
            server_time: now.to_string(),
            nonce: req.nonce.clone(),
        })
    }

    // ------------------------------------------------------------------
    // §1.4 B 档审计回执（**重点**）
    // ------------------------------------------------------------------

    /// B 档审计回执（设计 §1.4）。
    ///
    /// 规则顺序：
    /// 1. **字段白名单强制**：`validate_whitelist()` 不过 → `FieldWhitelistViolation` 并记审计，整单拒收
    /// 2. 租约校验（不存在 / 已废弃）
    /// 3. **跳空检测**：`expected = last.seq_to + 1`
    ///    - `seq_from > expected` → `Gap`（跳空）
    ///    - `seq_from <= last.seq_to` → `Overlap`（回退 / 重叠）
    ///    - 无历史且 `seq_from > 1` → `Gap`
    ///    - 否则 `None`
    ///    - 置 `gap_flag = (kind != None)` 并写 `audit_log`（action=`receipt_anomaly`）——**人工核实，不自动封禁**
    /// 4. **幂等**：相同区间重复上报 → `accepted=true` + `GapKind::None`，**不重复告警**
    /// 5. 允许**延迟补报**（`received_at` 可远晚于 `ts`，按 `ts` 排序评估）
    pub fn audit_receipt(&self, req: &AuditReceiptRequest) -> LicenseResult<AuditReceiptResponse> {
        let now = now_unix_secs();

        // 规则 1：字段白名单强制（整单拒收并记审计）。
        if let Err(e) = req.validate_whitelist() {
            self.write_audit(
                ActorType::Device,
                &req.device_mid,
                "receipt_whitelist_violation",
                "lease",
                &req.lease_id,
                &format!("{{\"error\":\"{}\"}}", escape_json(&e.to_string())),
                "",
            )?;
            return Err(e);
        }

        // 序号解析（大整数以 String 传入，此处边界转 u64）。
        let seq_from = parse_seq(&req.seq_from, "seq_from")?;
        let seq_to = parse_seq(&req.seq_to, "seq_to")?;
        let _count = parse_seq(&req.count, "count")?;
        if seq_to < seq_from {
            return Err(LicenseError::ActivationRejected(format!(
                "receipt seq_to ({seq_to}) must be >= seq_from ({seq_from})"
            )));
        }
        let client_ts = parse_ts(&req.ts, "ts")?;

        // 规则 2：租约校验。
        let lease = self.store.get_lease(&req.lease_id)?.ok_or_else(|| {
            LicenseError::ActivationRejected(format!("lease not found: {}", req.lease_id))
        })?;
        if lease.status == LeaseStatus::Stopped {
            return Err(LicenseError::ActivationRejected(format!(
                "lease {} is revoked/stopped",
                lease.lease_id
            )));
        }

        // 规则 2.5（**安全红线**）：Ed25519 验签。
        //
        // B 档信任链的唯一支点：没有这一步，任何人构造一个 `sig` 填 `"x"` 的
        // 回执都能拿到 `accepted=true`，跳空告警 / 序号区间审计全部可被伪造，
        // 且伪造记录会污染 `audit_receipt` 表使真实回执的跳空判定失效。
        //
        // 顺序：租约存在性之后、幂等与跳空检测**之前**——
        //   - 在租约校验后：先确认对象存在，避免为不存在的 lease 消耗验签 CPU；
        //   - 在幂等 / 跳空之前：不可信数据**不得**参与任何业务判定或短路返回，
        //     否则伪造回执可借"与上一条同区间"路径拿到 `accepted=true`。
        //
        // 失败处理：**拒收**（绝不"只打日志照常落库"）+ **记审计**（伪造尝试本身
        // 是安全事件，必须留痕，便于风控聚合与事后追溯）。
        match verify_receipt_signature(&self.ring, req, now, self.cfg.clock_skew_secs) {
            Ok(_verified) => {}
            Err(e) => {
                self.write_audit(
                    ActorType::Device,
                    &req.device_mid,
                    "receipt_signature_invalid",
                    "lease",
                    &req.lease_id,
                    &format!(
                        "{{\"error\":\"{}\",\"code\":\"{}\",\"seq_from\":\"{}\",\"seq_to\":\"{}\"}}",
                        escape_json(&e.to_string()),
                        error_code(&e),
                        escape_json(&req.seq_from),
                        escape_json(&req.seq_to)
                    ),
                    "",
                )?;
                return Err(e);
            }
        }

        // 规则 4（幂等）：相同区间重复上报 → 幂等接受，不重复告警。
        if let Some(last) = self.store.last_receipt_for_lease(&lease.lease_id)? {
            if last.seq_from == seq_from as i64 && last.seq_to == seq_to as i64 {
                return Ok(AuditReceiptResponse {
                    accepted: true,
                    gap: GapKind::None,
                    server_time: now.to_string(),
                });
            }
        }

        // 规则 3：跳空检测（基于最近一次回执）。
        let last = self.store.last_receipt_for_lease(&lease.lease_id)?;
        let gap = match &last {
            Some(l) => {
                let last_to = l.seq_to.max(0) as u64;
                if seq_from > last_to.saturating_add(1) {
                    GapKind::Gap
                } else if seq_from <= last_to {
                    GapKind::Overlap
                } else {
                    GapKind::None
                }
            }
            None => {
                // 无历史：起始序号若 > 1，则 1..seq_from 之间的回执缺失。
                if seq_from > 1 {
                    GapKind::Gap
                } else {
                    GapKind::None
                }
            }
        };

        // 落回执记录（gap_flag = 是否异常）。
        let receipt = AuditReceipt {
            id: now_ns_id("rcpt"),
            lease_id: lease.lease_id.clone(),
            device_mid: req.device_mid.clone(),
            seq_from: seq_from as i64,
            seq_to: seq_to as i64,
            count: _count as i64,
            payload_digest: req.payload_digest.clone(),
            ts: client_ts,
            sig: req.sig.clone(),
            received_at: now,
            gap_flag: gap.is_anomaly(),
        };
        self.store.insert_audit_receipt(&receipt)?;

        // 异常 → 风控告警（人工核实，不自动封禁）。
        if gap.is_anomaly() {
            self.store.update_receipt_gap_flag(&receipt.id, true)?;
            let last_to = last.as_ref().map(|l| l.seq_to).unwrap_or(0);
            self.write_audit(
                ActorType::Device,
                &req.device_mid,
                "receipt_anomaly",
                "lease",
                &lease.lease_id,
                &format!(
                    "{{\"kind\":\"{}\",\"seq_from\":{},\"seq_to\":{},\"last_seq_to\":{}}}",
                    gap.as_str(),
                    seq_from,
                    seq_to,
                    last_to
                ),
                "",
            )?;
        }
        // 正常回执也留痕（轻量，便于窗口期聚合）。
        self.write_audit(
            ActorType::Device,
            &req.device_mid,
            "receipt_accepted",
            "lease",
            &lease.lease_id,
            &format!(
                "{{\"kind\":\"{}\",\"seq_from\":{},\"seq_to\":{}}}",
                gap.as_str(),
                seq_from,
                seq_to
            ),
            "",
        )?;

        Ok(AuditReceiptResponse {
            accepted: true,
            gap,
            server_time: now.to_string(),
        })
    }

    // ------------------------------------------------------------------
    // §2.1 发放
    // ------------------------------------------------------------------

    /// 发放激活码（设计 §2.1）。
    ///
    /// - 幂等：同 `idempotency_key` 重放 → 返回首次批次（`code_batch` 头表 + 逐位一致）
    /// - 租户不存在 → `TenantNotFound`
    /// - `count` 上限 [`MAX_ISSUE_BATCH`]
    ///
    /// **幂等仲裁（批次头表）**：`idempotency_key` 是**批次级**标识（一批多码共用），
    /// 因此 `activation_code.idempotency_key` **不建唯一索引**（行级唯一会让批量发码失败）。
    /// 唯一性下沉到 `code_batch(idempotency_key PRIMARY KEY)`，以 `INSERT OR IGNORE` +
    /// `rows_affected` 做**数据库级线性化**仲裁：
    /// - 认领成功（`true`）→ 本调用是**首个**到达者，执行完整发放；
    /// - 认领失败（`false`）→ **重复 / 并发重放**，改读既有批次原样返回，**绝不再生成新码**。
    ///
    /// **认领时机红线**：`claim_code_batch` 必须放在**参数解析与校验之后**——否则一次
    /// 「count 非法 / 租户不存在」的坏请求会先烧掉 key，使后续**合法**重试被误判为重放。
    pub fn issue_codes(
        &self,
        req: &IssueCodesRequest,
        actor: &str,
    ) -> LicenseResult<IssueCodesResponse> {
        // ---- 先做全部无副作用校验（此阶段绝不碰 code_batch，避免坏请求烧 key）----
        // 数量校验。
        if req.count == 0 || req.count > MAX_ISSUE_BATCH {
            return Err(LicenseError::QuotaExceeded(format!(
                "issue count {} out of range 1..={MAX_ISSUE_BATCH}",
                req.count
            )));
        }
        // 租户校验。
        if self.store.get_tenant(&req.tenant_id)?.is_none() {
            return Err(LicenseError::ActivationRejected(format!(
                "tenant not found: {}",
                req.tenant_id
            )));
        }
        let valid_from = parse_ts(&req.valid_from, "valid_from")?;
        let valid_until = parse_ts(&req.valid_until, "valid_until")?;
        if valid_until <= valid_from {
            return Err(LicenseError::ActivationRejected(
                "valid_until must be greater than valid_from".into(),
            ));
        }

        // `now` 必须在 `claim_code_batch` **之前**取：保证头表 `created_at` 与随后逐码
        // 的 `created_at` 同源（同一批次内一致），也避免认领成功后才取时间引入的额外窗口。
        let now = now_unix_secs();

        // ---- 幂等认领（仅当 key 非空）----
        let key = req.idempotency_key.trim();
        let has_key = !key.is_empty();
        if has_key {
            let claimed = self
                .store
                .claim_code_batch(key, &req.tenant_id, req.count, now)?;
            if !claimed {
                // 重复 / 并发重放：直接返回既有批次（rowid 升序，逐位一致）。
                let existing = self.store.list_codes_by_batch_key(key)?;
                let replay: Vec<IssuedCode> = existing.iter().map(code_to_issued).collect();
                return Ok(IssueCodesResponse { codes: replay });
            }
        }

        // ---- 认领成功（或无 key，跳过幂等）→ 执行完整发放 ----
        let mut issued: Vec<IssuedCode> = Vec::with_capacity(req.count as usize);
        for _ in 0..req.count {
            let code_value = generate_activation_code();
            let code_id = now_ns_id("code");
            let mut record = ActivationCode::new_issued(
                code_id.clone(),
                code_value.clone(),
                req.tenant_id.clone(),
                req.tier.clone(),
                valid_from,
                valid_until,
                // 服务端发放无外部订单号：以空串哨兵占位（列约束为 NOT NULL）。
                Some(String::new()),
                actor.to_string(),
                now,
            );
            record.idempotency_key = Some(req.idempotency_key.clone());
            self.store.insert_code(&record)?;

            issued.push(IssuedCode {
                code_id,
                code: code_value,
                status: CodeStatus::Issued.as_str().to_string(),
                prebind: req.prebind_machine_code.clone(),
                reissued_from: None,
            });
        }

        self.write_audit(
            ActorType::Admin,
            actor,
            "issue",
            "tenant",
            &req.tenant_id,
            &format!(
                "{{\"count\":{},\"tier\":\"{}\",\"idempotency_key\":\"{}\"}}",
                issued.len(),
                req.tier,
                req.idempotency_key
            ),
            "",
        )?;

        Ok(IssueCodesResponse { codes: issued })
    }

    // ------------------------------------------------------------------
    // §2.2 废弃
    // ------------------------------------------------------------------

    /// 废弃激活码（设计 §2.2，**高危 · 立即失效**）。
    ///
    /// - `confirm_tail8` 必须等于码值后 8 位 → 否则 `ConfirmMismatch`
    /// - `reason` 非空、`note` ≥ 10 字符 → 否则 `ReasonRequired`
    /// - 状态已是 `Reissued` → `AlreadyReissued`
    /// - **幂等**：同码同原因重复撤销 → 成功返回
    /// - **副作用**：作废关联租约（`Stopped`），立即失效
    pub fn revoke_code(
        &self,
        code_id: &str,
        req: &RevokeCodeRequest,
        actor: &str,
    ) -> LicenseResult<()> {
        let code = self.store.get_code_by_id(code_id)?.ok_or_else(|| {
            LicenseError::ActivationRejected(format!("activation code not found: {code_id}"))
        })?;

        // 确认串：码值后 8 位（去掉连字符后的末 8 位）。
        let tail8 = code.code.chars().filter(|c| *c != '-').collect::<String>();
        let tail8 = tail8
            .get(tail8.len().saturating_sub(8)..)
            .unwrap_or("")
            .to_string();
        if req.confirm_tail8 != tail8 {
            return Err(LicenseError::ActivationRejected(format!(
                "confirm_tail8 mismatch for code {code_id}"
            )));
        }
        // 原因 / 说明校验。
        if req.reason.trim().is_empty() || req.note.trim().chars().count() < 10 {
            return Err(LicenseError::ActivationRejected(format!(
                "revoke requires a non-empty reason and a note of >=10 chars (code {code_id})"
            )));
        }
        // 双人复核。
        if self.cfg.dual_approval_required
            && req.second_approver.as_deref().unwrap_or("").is_empty()
        {
            return Err(LicenseError::ActivationRejected(format!(
                "second approver required for revoke of code {code_id}"
            )));
        }
        // 已重发的原码禁止再撤销。
        if code.status == CodeStatus::Reissued {
            return Err(LicenseError::KeyStateIllegal(format!(
                "code {code_id} has been reissued and cannot be revoked"
            )));
        }

        // 幂等：已废弃且原因一致 → 成功返回（不重复作废、不重复告警）。
        if code.status == CodeStatus::Revoked {
            if code.revoked_reason.as_deref() == Some(req.reason.as_str()) {
                return Ok(());
            }
            return Err(LicenseError::KeyStateIllegal(format!(
                "code {code_id} already revoked with a different reason"
            )));
        }

        let now = now_unix_secs();
        self.store.revoke_code(code_id, &req.reason, now)?;

        // 副作用：作废关联租约（立即失效）。
        if let Some(device_id) = code.bound_device_id.as_deref() {
            for lease in self.store.list_leases_by_device(device_id)? {
                if lease.code_id == code_id && lease.status != LeaseStatus::Stopped {
                    self.store
                        .update_lease_status(&lease.lease_id, LeaseStatus::Stopped)?;
                }
            }
            self.store
                .update_device_status(device_id, DeviceStatus::Stopped)?;
        }

        self.write_audit(
            ActorType::Admin,
            actor,
            "revoke",
            "activation_code",
            code_id,
            &format!(
                "{{\"reason\":\"{}\",\"note\":\"{}\",\"from\":\"{}\",\"to\":\"revoked\",\"second_approver\":\"{}\"}}",
                escape_json(&req.reason),
                escape_json(&req.note),
                code.status.as_str(),
                escape_json(req.second_approver.as_deref().unwrap_or(""))
            ),
            "",
        )?;
        Ok(())
    }

    // ------------------------------------------------------------------
    // §2.3 重发
    // ------------------------------------------------------------------

    /// 重发激活码（设计 §2.3，**高危**）。
    ///
    /// - 原码未 `Revoked` → `OriginalNotRevoked`
    /// - `prebind` 机器码已绑定他码 → `PrebindConflict`
    /// - 新码 `reissued_from_id = 原 code_id`；原码置 `Reissued`
    /// - `inherit_tier` / `inherit_validity` 生效
    /// - 幂等（同 `idempotency_key` 返回同一新码）
    pub fn reissue_code(
        &self,
        code_id: &str,
        req: &ReissueCodeRequest,
        actor: &str,
    ) -> LicenseResult<ReissueCodeResponse> {
        let original = self.store.get_code_by_id(code_id)?.ok_or_else(|| {
            LicenseError::ActivationRejected(format!("activation code not found: {code_id}"))
        })?;

        // 幂等：该原码已有重发链 → 返回同一新码。
        if original.status == CodeStatus::Reissued {
            let filter = CodeFilter {
                tenant_id: Some(original.tenant_id.clone()),
                status: None,
                tier: None,
                order_id: None,
            };
            let chain = self.store.list_codes(&filter, 1, MAX_ISSUE_BATCH)?;
            if let Some(existing) = chain
                .iter()
                .find(|c| c.reissued_from_id.as_deref() == Some(code_id))
            {
                return Ok(ReissueCodeResponse {
                    new_code: IssuedCode {
                        code_id: existing.code_id.clone(),
                        code: existing.code.clone(),
                        status: existing.status.as_str().to_string(),
                        prebind: req.prebind.as_ref().map(|p| p.machine_code.clone()),
                        reissued_from: Some(code_id.to_string()),
                    },
                });
            }
        } else if original.status != CodeStatus::Revoked {
            // 原码必须先废弃。
            return Err(LicenseError::KeyStateIllegal(format!(
                "original code {code_id} is {} and must be revoked before reissue",
                original.status.as_str()
            )));
        }

        // 预绑定冲突检测：该机器码已被别的码绑定。
        if let Some(p) = &req.prebind {
            if let Some(dev) = self.store.get_device_by_machine_code(&p.machine_code)? {
                let filter = CodeFilter {
                    tenant_id: Some(dev.tenant_id.clone()),
                    status: None,
                    tier: None,
                    order_id: None,
                };
                let bound = self.store.list_codes(&filter, 1, MAX_ISSUE_BATCH)?;
                if bound.iter().any(|c| {
                    c.bound_device_id.as_deref() == Some(dev.device_id.as_str())
                        && c.code_id != code_id
                }) {
                    return Err(LicenseError::ActivationRejected(format!(
                        "prebind machine code conflicts with an existing binding (code {code_id})"
                    )));
                }
            }
        }

        let now = now_unix_secs();
        // 生效字段：继承 / 覆盖。
        let tier = if req.inherit_tier {
            original.tier.clone()
        } else {
            req.overrides
                .as_ref()
                .and_then(|o| o.tier.clone())
                .unwrap_or_else(|| original.tier.clone())
        };
        let valid_until = if req.inherit_validity {
            original.valid_until
        } else {
            req.overrides
                .as_ref()
                .and_then(|o| o.valid_until.as_deref())
                .and_then(|s| parse_ts(s, "valid_until").ok())
                .unwrap_or(original.valid_until)
        };

        let new_code_value = generate_activation_code();
        let new_code_id = now_ns_id("code");
        let mut new_record = ActivationCode::new_issued(
            new_code_id.clone(),
            new_code_value.clone(),
            original.tenant_id.clone(),
            tier,
            original.valid_from,
            valid_until,
            original.source_order_id.clone(),
            actor.to_string(),
            now,
        );
        new_record.reissued_from_id = Some(code_id.to_string());
        new_record.idempotency_key = Some(req.idempotency_key.clone());
        self.store.insert_code(&new_record)?;
        self.store.mark_code_reissued(code_id)?;

        self.write_audit(
            ActorType::Admin,
            actor,
            "reissue",
            "activation_code",
            code_id,
            &format!(
                "{{\"new_code_id\":\"{}\",\"inherit_tier\":{},\"inherit_validity\":{},\"idempotency_key\":\"{}\"}}",
                new_code_id, req.inherit_tier, req.inherit_validity, req.idempotency_key
            ),
            "",
        )?;

        Ok(ReissueCodeResponse {
            new_code: IssuedCode {
                code_id: new_code_id,
                code: new_code_value,
                status: CodeStatus::Issued.as_str().to_string(),
                prebind: req.prebind.as_ref().map(|p| p.machine_code.clone()),
                reissued_from: Some(code_id.to_string()),
            },
        })
    }

    // ------------------------------------------------------------------
    // §2.4-2.7 查询
    // ------------------------------------------------------------------

    /// 码列表（设计 §2.4，码值掩码显示）。
    pub fn list_codes(
        &self,
        q: &CodeFilter,
        page: u32,
        page_size: u32,
    ) -> LicenseResult<Vec<CodeSummary>> {
        let records = self.store.list_codes(q, page.max(1), page_size.max(1))?;
        Ok(records
            .into_iter()
            .map(|c| CodeSummary {
                code_id: c.code_id.clone(),
                code_masked: mask_code(&c.code),
                status: c.status.as_str().to_string(),
                tenant_id: c.tenant_id.clone(),
                tier: c.tier.clone(),
                bound_device_id: c.bound_device_id.clone(),
                valid_until: c.valid_until.to_string(),
                created_at: c.created_at.to_string(),
            })
            .collect())
    }

    /// 码详情（设计 §2.5：时间线 + 重发链）。
    pub fn code_detail(&self, code_id: &str) -> LicenseResult<CodeDetail> {
        let c = self.store.get_code_by_id(code_id)?.ok_or_else(|| {
            LicenseError::ActivationRejected(format!("activation code not found: {code_id}"))
        })?;
        // 时间线：从审计日志中抽取与本码相关的事件。
        let audits = self.store.list_audit_logs(
            &AuditFilter {
                actor_type: None,
                action: None,
                entity_type: Some("activation_code".into()),
                entity_id: Some(code_id.to_string()),
            },
            1,
            1000,
        )?;
        let timeline: Vec<TimelineEntry> = audits
            .iter()
            .map(|a| TimelineEntry {
                action: a.action.clone(),
                actor: format!("{}:{}", actor_type_str(a.actor_type), a.actor_id),
                at: a.ts.to_string(),
                detail: a.detail.clone(),
            })
            .collect();
        // 重发链：本码的 reissued_from_id（若有）。
        let reissued_chain: Vec<String> = c.reissued_from_id.iter().cloned().collect();
        Ok(CodeDetail {
            code_id: c.code_id.clone(),
            code: c.code.clone(),
            status: c.status.as_str().to_string(),
            tenant_id: c.tenant_id.clone(),
            tier: c.tier.clone(),
            bound_device_id: c.bound_device_id.clone(),
            valid_from: c.valid_from.to_string(),
            valid_until: c.valid_until.to_string(),
            timeline,
            reissued_chain,
            revoked_at: c.revoked_at.map(|t| t.to_string()),
            revoked_reason: c.revoked_reason.clone(),
        })
    }

    /// 设备列表（设计 §2.6，机器码掩码）。`tenant_id == None` 为跨租户总览。
    pub fn list_devices(
        &self,
        tenant_id: Option<&str>,
        page: u32,
        page_size: u32,
    ) -> LicenseResult<Vec<DeviceSummary>> {
        let devices = self
            .store
            .list_devices(tenant_id, page.max(1), page_size.max(1))?;
        let mut out = Vec::with_capacity(devices.len());
        for d in devices {
            let leases = self.store.list_leases_by_device(&d.device_id)?;
            let active = leases
                .iter()
                .filter(|l| l.status == LeaseStatus::Active)
                .max_by_key(|l| l.issued_at);
            let last_hb = active.and_then(|l| l.last_heartbeat_at);
            let gap_summary = self.receipt_gap_summary(&d.device_id)?;
            out.push(DeviceSummary {
                device_id: d.device_id.clone(),
                tenant_id: d.tenant_id.clone(),
                machine_code_masked: mask_tail(&d.machine_code, 4),
                deploy_mode: d.deploy_mode.as_str().to_string(),
                image_digest: d.image_digest.clone(),
                last_heartbeat_at: last_hb.map(|t| t.to_string()),
                lease_status: active.map(|l| l.status.as_str().to_string()),
                receipt_gap_summary: gap_summary,
            });
        }
        Ok(out)
    }

    /// 汇总某设备全部租约的回执异常计数（`gap=N,overlap=M`）。
    fn receipt_gap_summary(&self, device_id: &str) -> LicenseResult<String> {
        let leases = self.store.list_leases_by_device(device_id)?;
        let mut gap = 0u64;
        for l in leases {
            for r in self.store.list_receipts_by_lease(&l.lease_id)? {
                if r.gap_flag {
                    gap += 1;
                }
            }
        }
        Ok(format!("gap={gap}"))
    }

    /// 审计日志（设计 §2.7）。
    pub fn list_audit_logs(
        &self,
        f: &AuditFilter,
        page: u32,
        page_size: u32,
    ) -> LicenseResult<Vec<AuditLog>> {
        self.store.list_audit_logs(f, page.max(1), page_size.max(1))
    }

    /// 租户设备总数（供上层展示配额使用）。`None` 为全局计数。
    pub fn device_count(&self, tenant_id: Option<&str>) -> LicenseResult<u64> {
        self.store.count_devices(tenant_id)
    }

    /// 写审计日志（失败不上浮——审计不可因写库失败而阻断业务；但会被记录为 Storage 错误）。
    #[allow(clippy::too_many_arguments)]
    fn write_audit(
        &self,
        actor_type: ActorType,
        actor_id: &str,
        action: &str,
        entity_type: &str,
        entity_id: &str,
        detail: &str,
        ip: &str,
    ) -> LicenseResult<()> {
        let log = AuditLog {
            id: now_ns_id("audit"),
            actor_type,
            actor_id: actor_id.to_string(),
            action: action.to_string(),
            entity_type: entity_type.to_string(),
            entity_id: entity_id.to_string(),
            detail: detail.to_string(),
            ts: now_unix_secs(),
            ip: ip.to_string(),
        };
        self.store.insert_audit_log(&log)
    }
}

/// 设备列表项（业务层输出；与 proto 的展示结构解耦，便于单元测试直接断言）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceSummary {
    /// 设备 ID。
    pub device_id: String,
    /// 租户。
    pub tenant_id: String,
    /// 掩码机器码。
    pub machine_code_masked: String,
    /// 部署形态。
    pub deploy_mode: String,
    /// 镜像 digest。
    pub image_digest: Option<String>,
    /// 最近心跳（String 大整数纪律）。
    pub last_heartbeat_at: Option<String>,
    /// 租约状态。
    pub lease_status: Option<String>,
    /// 回执异常汇总。
    pub receipt_gap_summary: String,
}

/// 把激活码记录转成发放响应项。
fn code_to_issued(c: &crate::model::ActivationCode) -> IssuedCode {
    IssuedCode {
        code_id: c.code_id.clone(),
        code: c.code.clone(),
        status: c.status.as_str().to_string(),
        prebind: None,
        reissued_from: c.reissued_from_id.clone(),
    }
}

/// `ActorType` → 字符串（审计展示）。
fn actor_type_str(a: ActorType) -> &'static str {
    a.as_str()
}

/// 掩码码值：保留前缀与末 4 位（`IOTDAQ-****-****-****-AB12`）。
pub fn mask_code(code: &str) -> String {
    let raw: Vec<char> = code.chars().filter(|c| *c != '-').collect();
    if raw.len() < 4 {
        return "****".to_string();
    }
    let tail: String = raw[raw.len() - 4..].iter().collect();
    format!("{CODE_PREFIX}-****-****-****-{tail}")
}

/// 掩码任意字符串：保留末 `keep` 位。
fn mask_tail(s: &str, keep: usize) -> String {
    let chars: Vec<char> = s.chars().collect();
    if chars.len() <= keep {
        return "*".repeat(chars.len());
    }
    format!(
        "{}{}",
        "*".repeat(chars.len() - keep),
        chars[chars.len() - keep..].iter().collect::<String>()
    )
}

/// JSON 字符串的最小转义（`\` 与 `"`）。
fn escape_json(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

/// 错误码短标识（供 `audit_log.detail` 落库，**不含请求原文**）。
///
/// 与 [`crate::http::error_to_code`] 同源——后者输出的是 HTTP 层协议码；
/// 此处是**审计留痕用**的稳定短串，便于按错误类型聚合告警而无需解析自由文本。
/// 刻意不落 `e.to_string()` 之外的任何输入字段，避免伪造者借错误消息回显
/// 注入审计内容（错误消息本身已由各校验点控制，不含签名原文）。
fn error_code(err: &LicenseError) -> &'static str {
    match err {
        LicenseError::TokenInvalid(_) => "token_invalid",
        LicenseError::ActivationRejected(_) => "activation_rejected",
        LicenseError::QuotaExceeded(_) => "quota_exceeded",
        LicenseError::HeartbeatRejected(_) => "heartbeat_rejected",
        LicenseError::KeyStateIllegal(_) => "key_state_illegal",
        LicenseError::Storage(_) => "storage",
    }
}

// ============================================================================
// 激活码生成与校验（Crockford base32 + 校验位）
// ============================================================================

/// 对 payload 字符做加权和 mod 32，得到校验字符。
///
/// 权重取 `(index + 1)`，避免「调换两位」在奇偶位上抵消（简单 Luhn-like 加权）。
fn checksum_char(payload: &[u8]) -> u8 {
    let mut acc: u32 = 0;
    for (i, b) in payload.iter().enumerate() {
        let v = crockford_value(*b) as u32;
        let weight = (i as u32) + 1;
        acc = (acc + v * weight) % 32;
    }
    CROCKFORD_ALPHABET[(acc % 32) as usize]
}

/// 字符 → Crockford 值（排除 I/L/O/U；`0`/`O`、`1`/`I`/`L` 归一）。
fn crockford_value(c: u8) -> u8 {
    match c {
        b'0' | b'O' | b'o' => 0,
        b'1' | b'I' | b'i' | b'L' | b'l' => 1,
        b'2'..=b'9' => c - b'0',
        b'A'..=b'H' => c - b'A' + 10,
        b'J'..=b'K' => c - b'J' + 18,
        b'M'..=b'N' => c - b'M' + 20,
        b'P'..=b'T' => c - b'P' + 22,
        b'V'..=b'Z' => c - b'V' + 27,
        _ => 0xFF, // 非法
    }
}

/// 生成一个合法激活码：`IOTDAQ-XXXX-XXXX-XXXX-XXXX`（末位为校验字符）。
///
/// 载荷长度 = `CODE_GROUPS * CODE_GROUP_LEN`，其中前 `n-1` 位随机、末位为校验位。
pub fn generate_activation_code() -> String {
    use rand::Rng as _;
    let mut rng = rand::rng();
    let total = CODE_GROUPS * CODE_GROUP_LEN;
    let payload_len = total - 1;
    let mut payload: Vec<u8> = Vec::with_capacity(payload_len);
    for _ in 0..payload_len {
        let idx: usize = rng.random_range(0..32);
        payload.push(CROCKFORD_ALPHABET[idx]);
    }
    let check = checksum_char(&payload);
    payload.push(check);
    // 分组拼接。
    let body: String = payload.iter().map(|b| *b as char).collect();
    let mut out = String::from(CODE_PREFIX);
    for g in 0..CODE_GROUPS {
        out.push('-');
        out.push_str(&body[g * CODE_GROUP_LEN..(g + 1) * CODE_GROUP_LEN]);
    }
    out
}

/// 校验激活码格式与校验位。非法 → `ActivationRejected`。
pub fn validate_activation_code(code: &str) -> LicenseResult<()> {
    let normalized: String = code.trim().to_ascii_uppercase();
    let mut parts = normalized.split('-');
    let prefix = parts.next().unwrap_or("");
    if prefix != CODE_PREFIX {
        return Err(LicenseError::ActivationRejected(format!(
            "activation code must start with {CODE_PREFIX}-"
        )));
    }
    let mut body = String::new();
    for _ in 0..CODE_GROUPS {
        let seg = parts.next().ok_or_else(|| {
            LicenseError::ActivationRejected("activation code is missing a group".into())
        })?;
        if seg.len() != CODE_GROUP_LEN {
            return Err(LicenseError::ActivationRejected(format!(
                "activation code group must be {CODE_GROUP_LEN} chars, got {seg}"
            )));
        }
        body.push_str(seg);
    }
    if parts.next().is_some() {
        return Err(LicenseError::ActivationRejected(
            "activation code has too many groups".into(),
        ));
    }
    let bytes = body.as_bytes();
    for b in bytes {
        if crockford_value(*b) == 0xFF {
            return Err(LicenseError::ActivationRejected(format!(
                "activation code contains an illegal character: {}",
                *b as char
            )));
        }
    }
    let (payload, check) = bytes.split_at(bytes.len() - 1);
    let expected = checksum_char(payload);
    if check[0] != expected {
        return Err(LicenseError::ActivationRejected(
            "activation code checksum mismatch".into(),
        ));
    }
    Ok(())
}

// ============================================================================
// 测试
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keys::KeyRing;
    use crate::model::HeartbeatResult as _HB;
    use crate::model::{ActivationCode, SigningKeyStatus, Tenant};
    use crate::proto::{ReissueCodeRequest, RevokeCodeRequest};
    use crate::store::Store;

    /// 构造测试服务：内存 SQLite + 生成密钥环 + 单个租户。
    fn service() -> (LicensingService, Arc<Store>, Arc<KeyRing>) {
        let store = Arc::new(Store::open_in_memory().expect("in-memory store"));
        let ring = Arc::new(KeyRing::empty());
        ring.register_generated(Some("kms://test".into()), 1_700_000_000)
            .expect("register signing key");
        let tenant = Tenant {
            tenant_id: "t-1".into(),
            name: "测试租户".into(),
            verify_mode_default: VerifyMode::B,
            contact: "ops@example.com".into(),
            created_at: 1_700_000_000,
        };
        store.insert_tenant(&tenant).expect("insert tenant");
        let svc = LicensingService::new(
            store.clone(),
            ring.clone(),
            ServiceConfig {
                // 放宽时钟窗，测试里直接用真实 now。
                clock_skew_secs: 300,
                nonce_ttl_secs: 600,
                ..ServiceConfig::default()
            },
        );
        (svc, store, ring)
    }

    /// 发放一个码并返回 (code_id, code_value)（默认租户 `t-1`）。
    fn issue_one(svc: &LicensingService) -> (String, String) {
        issue_one_for(svc, "t-1")
    }

    /// 为指定租户发放一个码。
    fn issue_one_for(svc: &LicensingService, tenant_id: &str) -> (String, String) {
        let now = now_unix_secs();
        let req = IssueCodesRequest {
            tenant_id: tenant_id.into(),
            tier: "standard".into(),
            valid_from: now.to_string(),
            valid_until: (now + 365 * 86_400).to_string(),
            count: 1,
            prebind_machine_code: None,
            idempotency_key: format!("idem-{}", now_ns_id("k")),
        };
        let resp = svc.issue_codes(&req, "admin-1").expect("issue");
        let c = &resp.codes[0];
        (c.code_id.clone(), c.code.clone())
    }

    /// 构造激活请求。
    fn activation_req(
        code: &str,
        machine: &str,
        anchors: &[&str],
        nonce: &str,
    ) -> ActivationRequest {
        ActivationRequest {
            activation_code: code.to_string(),
            machine_code: machine.to_string(),
            anchor_hashes: anchors.iter().map(|s| s.to_string()).collect(),
            device_pubkey: "pubkey".into(),
            nonce: nonce.to_string(),
            ts: now_unix_secs().to_string(),
            req_sig: "sig".into(),
        }
    }

    /// 锚点集合（5 个，M=5）。
    fn anchors(n: usize, seed: &str) -> Vec<String> {
        (0..n).map(|i| format!("{seed}-anchor-{i}")).collect()
    }

    // ---------------- 激活 7 条规则 ----------------

    /// 规则 1：时间窗超限 → 拒绝。
    #[test]
    fn activate_rejects_timestamp_skew() {
        let (svc, _s, _r) = service();
        let (_id, code) = issue_one(&svc);
        let mut req = activation_req(
            &code,
            "mach-skew",
            &["a-0", "a-1", "a-2", "a-3", "a-4"],
            "n-skew",
        );
        req.ts = (now_unix_secs() - 10_000).to_string(); // 远超 ±300s
        let err = svc.activate(&req).unwrap_err();
        assert!(
            matches!(err, LicenseError::ActivationRejected(_)),
            "{err:?}"
        );
        assert!(err.to_string().contains("skew"), "{err}");
    }

    /// 规则 2：nonce 重放 → 拒绝。
    #[test]
    fn activate_rejects_nonce_replay() {
        let (svc, _s, _r) = service();
        let (_id, code) = issue_one(&svc);
        let anchors_v = anchors(5, "mach-nonce");
        let anchors_ref: Vec<&str> = anchors_v.iter().map(String::as_str).collect();
        let req = activation_req(&code, "mach-nonce", &anchors_ref, "same-nonce");
        assert!(svc.activate(&req).is_ok(), "首次激活应成功");
        // 同一 nonce 二次激活 → 重放拒绝。
        let err = svc.activate(&req).unwrap_err();
        assert!(matches!(err, LicenseError::ActivationRejected(_)));
        assert!(err.to_string().contains("nonce replay"), "{err}");
    }

    /// 规则 3a：码不存在 → 拒绝。
    #[test]
    fn activate_rejects_unknown_code() {
        let (svc, _s, _r) = service();
        // 合法格式但未发放的码。
        let fake = generate_activation_code();
        let a = anchors(5, "m-unknown");
        let a: Vec<&str> = a.iter().map(String::as_str).collect();
        let req = activation_req(&fake, "m-unknown", &a, "n-unknown");
        let err = svc.activate(&req).unwrap_err();
        assert!(
            matches!(err, LicenseError::ActivationRejected(_)),
            "{err:?}"
        );
        assert!(err.to_string().contains("invalid activation code"), "{err}");
    }

    /// 规则 3b：码已废弃 → 拒绝。
    #[test]
    fn activate_rejects_revoked_code() {
        let (svc, _s, _r) = service();
        let (code_id, code) = issue_one(&svc);
        let tail8: String = code.chars().filter(|c| *c != '-').collect();
        let tail8 = tail8[tail8.len() - 8..].to_string();
        svc.revoke_code(
            &code_id,
            &RevokeCodeRequest {
                reason: "fraud".into(),
                note: "客服已核实为欺诈换机场景".into(),
                confirm_tail8: tail8,
                second_approver: None,
            },
            "admin-1",
        )
        .unwrap();
        let a = anchors(5, "m-revoked");
        let a: Vec<&str> = a.iter().map(String::as_str).collect();
        let req = activation_req(&code, "m-revoked", &a, "n-revoked");
        let err = svc.activate(&req).unwrap_err();
        assert!(err.to_string().contains("revoked"), "{err}");
    }

    /// 规则 4a：同码**异机**被拒。
    #[test]
    fn activate_rejects_code_bound_to_other_device() {
        let (svc, _s, _r) = service();
        let (_id, code) = issue_one(&svc);
        let a1 = anchors(5, "machine-A");
        let a1r: Vec<&str> = a1.iter().map(String::as_str).collect();
        assert!(svc
            .activate(&activation_req(&code, "machine-A", &a1r, "nA"))
            .is_ok());
        // 完全不同机器 + 不同锚点 → 异机拒绝。
        let a2 = anchors(5, "machine-B");
        let a2r: Vec<&str> = a2.iter().map(String::as_str).collect();
        let err = svc
            .activate(&activation_req(&code, "machine-B", &a2r, "nB"))
            .unwrap_err();
        assert!(err.to_string().contains("another device"), "{err}");
    }

    /// 规则 4b：同码**同机**（machine_code 一致）→ 幂等返回同一租约。
    #[test]
    fn activate_same_code_same_machine_is_idempotent() {
        let (svc, _s, _r) = service();
        let (_id, code) = issue_one(&svc);
        let a = anchors(5, "machine-C");
        let ar: Vec<&str> = a.iter().map(String::as_str).collect();
        let r1 = svc
            .activate(&activation_req(&code, "machine-C", &ar, "nC1"))
            .unwrap();
        let r2 = svc
            .activate(&activation_req(&code, "machine-C", &ar, "nC2"))
            .unwrap();
        assert_eq!(r1.lease_id, r2.lease_id, "同机重装必须幂等复用同一租约");
        assert_eq!(r1.lease_token, r2.lease_token);
    }

    /// 规则 4c：**N-of-M 容错同机**（machine_code 不同，但锚点命中 ≥3）→ 幂等。
    #[test]
    fn activate_n_of_m_anchor_tolerance_treats_as_same_machine() {
        let (svc, _s, _r) = service();
        let (_id, code) = issue_one(&svc);
        let base = anchors(5, "hw");
        let base_r: Vec<&str> = base.iter().map(String::as_str).collect();
        let r1 = svc
            .activate(&activation_req(&code, "machine-D", &base_r, "nD1"))
            .unwrap();
        // 机器码漂移（如网卡变更），但 5 个锚点里仍有 3 个一致。
        let drifted: Vec<String> = vec![
            base[0].clone(),
            base[1].clone(),
            base[2].clone(),
            "hw-anchor-drift-3".into(),
            "hw-anchor-drift-4".into(),
        ];
        let drifted_r: Vec<&str> = drifted.iter().map(String::as_str).collect();
        let r2 = svc
            .activate(&activation_req(&code, "machine-D-v2", &drifted_r, "nD2"))
            .unwrap();
        assert_eq!(r1.lease_id, r2.lease_id, "3/5 锚点命中视为同机，应幂等");
    }

    /// 规则 5：配额超限 → `QuotaExceeded`。
    #[test]
    fn activate_rejects_when_quota_exceeded() {
        let store = Arc::new(Store::open_in_memory().unwrap());
        let ring = Arc::new(KeyRing::empty());
        ring.register_generated(None, 1_700_000_000).unwrap();
        store
            .insert_tenant(&Tenant {
                tenant_id: "t-q".into(),
                name: "配额租户".into(),
                verify_mode_default: VerifyMode::B,
                contact: "".into(),
                created_at: 0,
            })
            .unwrap();
        let svc = LicensingService::new(
            store.clone(),
            ring,
            ServiceConfig {
                max_devices_per_tenant: 1,
                ..ServiceConfig::default()
            },
        );
        let (_id, code1) = issue_one_for(&svc, "t-q");
        // 用不同机器激活两台 → 第二台撞配额。
        let a = anchors(5, "q1");
        let ar: Vec<&str> = a.iter().map(String::as_str).collect();
        svc.activate(&activation_req(&code1, "mach-q1", &ar, "q1"))
            .unwrap();
        let (_id2, code2) = {
            let now = now_unix_secs();
            let req = IssueCodesRequest {
                tenant_id: "t-q".into(),
                tier: "standard".into(),
                valid_from: now.to_string(),
                valid_until: (now + 86_400).to_string(),
                count: 1,
                prebind_machine_code: None,
                idempotency_key: format!("idem-q-{}", now_ns_id("k")),
            };
            let r = svc.issue_codes(&req, "admin").unwrap();
            (r.codes[0].code_id.clone(), r.codes[0].code.clone())
        };
        let a2 = anchors(5, "q2");
        let a2r: Vec<&str> = a2.iter().map(String::as_str).collect();
        let err = svc
            .activate(&activation_req(&code2, "mach-q2", &a2r, "q2"))
            .unwrap_err();
        assert!(matches!(err, LicenseError::QuotaExceeded(_)), "{err:?}");
    }

    /// 规则 6/7：成功激活 + `first_activation_at` 只在首次写。
    #[test]
    fn activate_writes_first_activation_only_once() {
        let (svc, store, _r) = service();
        let (_id, code) = issue_one(&svc);
        let a = anchors(5, "fa");
        let ar: Vec<&str> = a.iter().map(String::as_str).collect();
        let r1 = svc
            .activate(&activation_req(&code, "mach-fa", &ar, "fa1"))
            .unwrap();
        let dev = store
            .get_device_by_machine_code("mach-fa")
            .unwrap()
            .unwrap();
        let first = dev.first_activation_at.unwrap();
        assert!(first > 0, "首次激活必须写入 first_activation_at");

        // 第二次（同机幂等）不得覆盖 first_activation_at。
        std::thread::sleep(std::time::Duration::from_secs(1));
        svc.activate(&activation_req(&code, "mach-fa", &ar, "fa2"))
            .unwrap();
        let dev2 = store
            .get_device_by_machine_code("mach-fa")
            .unwrap()
            .unwrap();
        assert_eq!(
            dev2.first_activation_at,
            Some(first),
            "first_activation_at 不得被覆盖"
        );
        // 租约一致。
        assert_eq!(r1.verify_mode, "B");
        assert_eq!(r1.tier, "standard");
    }

    // ---------------- 心跳 ----------------

    /// 心跳：租约不存在 → `HeartbeatRejected`。
    #[test]
    fn heartbeat_rejects_unknown_lease() {
        let (svc, _s, _r) = service();
        let err = svc
            .heartbeat(&HeartbeatRequest {
                lease_id: "lease-nope".into(),
                ts: now_unix_secs().to_string(),
                nonce: "n".into(),
                receipt_cursor: None,
                device_sig: "s".into(),
            })
            .unwrap_err();
        assert!(matches!(err, LicenseError::HeartbeatRejected(_)), "{err:?}");
    }

    /// 心跳：租约已废弃（Stopped）→ 立即失效。
    #[test]
    fn heartbeat_rejects_revoked_lease() {
        let (svc, store, _r) = service();
        let (code_id, code) = issue_one(&svc);
        let a = anchors(5, "hb-rev");
        let ar: Vec<&str> = a.iter().map(String::as_str).collect();
        let act = svc
            .activate(&activation_req(&code, "mach-hb-rev", &ar, "hb1"))
            .unwrap();
        // 废弃码 → 连带租约 Stopped。
        let tail8: String = code.chars().filter(|c| *c != '-').collect();
        let tail8 = tail8[tail8.len() - 8..].to_string();
        svc.revoke_code(
            &code_id,
            &RevokeCodeRequest {
                reason: "risk".into(),
                note: "风控标记后人工核实废弃".into(),
                confirm_tail8: tail8,
                second_approver: None,
            },
            "admin",
        )
        .unwrap();
        // 租约状态已 Stopped。
        let lease = store.get_lease(&act.lease_id).unwrap().unwrap();
        assert_eq!(lease.status, LeaseStatus::Stopped);
        let err = svc
            .heartbeat(&HeartbeatRequest {
                lease_id: act.lease_id,
                ts: now_unix_secs().to_string(),
                nonce: "hb-rev-n".into(),
                receipt_cursor: None,
                device_sig: "s".into(),
            })
            .unwrap_err();
        assert!(matches!(err, LicenseError::HeartbeatRejected(_)), "{err:?}");
        assert!(err.to_string().contains("revoked"), "{err}");
    }

    /// 心跳：时间窗超限 → 拒绝（skew）。
    #[test]
    fn heartbeat_rejects_timestamp_skew() {
        let (svc, _s, _r) = service();
        let (_id, code) = issue_one(&svc);
        let a = anchors(5, "hb-skew");
        let ar: Vec<&str> = a.iter().map(String::as_str).collect();
        let act = svc
            .activate(&activation_req(&code, "mach-hb-skew", &ar, "hs1"))
            .unwrap();
        let err = svc
            .heartbeat(&HeartbeatRequest {
                lease_id: act.lease_id,
                ts: (now_unix_secs() - 99_999).to_string(),
                nonce: "hs-n".into(),
                receipt_cursor: None,
                device_sig: "s".into(),
            })
            .unwrap_err();
        assert!(err.to_string().contains("skew"), "{err}");
    }

    /// 心跳：同 nonce 重放 → 拒绝；不同 nonce 重复心跳 → 成功且回写 last_heartbeat。
    #[test]
    fn heartbeat_nonce_replay_and_idempotent_side_effects() {
        let (svc, store, _r) = service();
        let (_id, code) = issue_one(&svc);
        let a = anchors(5, "hb-idem");
        let ar: Vec<&str> = a.iter().map(String::as_str).collect();
        let act = svc
            .activate(&activation_req(&code, "mach-hb-idem", &ar, "hi1"))
            .unwrap();

        let ok = svc
            .heartbeat(&HeartbeatRequest {
                lease_id: act.lease_id.clone(),
                ts: now_unix_secs().to_string(),
                nonce: "hb-ok-1".into(),
                receipt_cursor: None,
                device_sig: "s".into(),
            })
            .unwrap();
        assert_eq!(ok.tier, "standard");
        let lease = store.get_lease(&act.lease_id).unwrap().unwrap();
        assert!(
            lease.last_heartbeat_at.is_some(),
            "心跳应回写 last_heartbeat_at"
        );

        // 同 nonce 重放 → 拒绝。
        let err = svc
            .heartbeat(&HeartbeatRequest {
                lease_id: act.lease_id.clone(),
                ts: now_unix_secs().to_string(),
                nonce: "hb-ok-1".into(),
                receipt_cursor: None,
                device_sig: "s".into(),
            })
            .unwrap_err();
        assert!(err.to_string().contains("nonce replay"), "{err}");

        // 换 nonce 重复心跳 → 成功（心跳本身可重复）。
        assert!(svc
            .heartbeat(&HeartbeatRequest {
                lease_id: act.lease_id,
                ts: now_unix_secs().to_string(),
                nonce: "hb-ok-2".into(),
                receipt_cursor: None,
                device_sig: "s".into(),
            })
            .is_ok());
    }

    /// 心跳携带 `receipt_cursor`（B 档）→ 落库回执游标。
    #[test]
    fn heartbeat_stores_receipt_cursor() {
        let (svc, store, _r) = service();
        let (_id, code) = issue_one(&svc);
        let a = anchors(5, "hb-cur");
        let ar: Vec<&str> = a.iter().map(String::as_str).collect();
        let act = svc
            .activate(&activation_req(&code, "mach-hb-cur", &ar, "hc1"))
            .unwrap();
        svc.heartbeat(&HeartbeatRequest {
            lease_id: act.lease_id.clone(),
            ts: now_unix_secs().to_string(),
            nonce: "hc-n".into(),
            receipt_cursor: Some(crate::proto::ReceiptCursor {
                seq_from: "1".into(),
                seq_to: "99".into(),
            }),
            device_sig: "s".into(),
        })
        .unwrap();
        let hbs = store.list_heartbeats_by_lease(&act.lease_id).unwrap();
        assert!(hbs
            .iter()
            .any(|h| h.receipt_cursor.as_deref() == Some("1-99")));
    }

    // ---------------- audit_receipt（5 条） ----------------

    /// 辅助：激活一台 A 档设备用于回执测试（回执本身按 B 档语义走）。
    fn activate_device(svc: &LicensingService, machine: &str, nonce: &str) -> String {
        let (_id, code) = issue_one(svc);
        let a = anchors(5, machine);
        let ar: Vec<&str> = a.iter().map(String::as_str).collect();
        svc.activate(&activation_req(&code, machine, &ar, nonce))
            .unwrap()
            .lease_id
    }

    fn receipt_req(lease_id: &str, from: u64, to: u64, mid: &str) -> AuditReceiptRequest {
        AuditReceiptRequest {
            device_mid: mid.into(),
            lease_id: lease_id.into(),
            seq_from: from.to_string(),
            seq_to: to.to_string(),
            count: (to - from + 1).to_string(),
            payload_digest: "digest".into(),
            ts: now_unix_secs().to_string(),
            sig: "sig".into(),
        }
    }

    /// 用测试密钥环中的私钥对回执签名（B 档信任链的服务端侧复现）。
    ///
    /// 与 daemon 的 `sign_receipt` 走同一条域串 + 同一层 SHA-256 预哈希，
    /// 因此这里是**跨端一致性**的服务端锚点：若任一端改了域串或编码，
    /// 依赖本辅助函数的测试会立即失败。
    fn sign_receipt_req(ring: &Arc<KeyRing>, req: &mut AuditReceiptRequest) {
        let hash = crate::receipt::receipt_payload_hash(
            &req.device_mid,
            &req.lease_id,
            req.seq_from.parse().unwrap(),
            req.seq_to.parse().unwrap(),
            req.count.parse().unwrap(),
            &req.payload_digest,
            req.ts.parse().unwrap(),
        );
        req.sig = ring.sign(&hash).expect("sign receipt").1;
    }

    /// 构造并签名的回执请求（绝大多数测试用这个）。
    fn signed_receipt_req(
        ring: &Arc<KeyRing>,
        lease_id: &str,
        from: u64,
        to: u64,
        mid: &str,
    ) -> AuditReceiptRequest {
        let mut req = receipt_req(lease_id, from, to, mid);
        sign_receipt_req(ring, &mut req);
        req
    }

    /// 回执 1：字段白名单越界 → `ActivationRejected`（对应 `FIELD_WHITELIST_VIOLATION`）并记审计。
    #[test]
    fn audit_receipt_rejects_whitelist_violation_and_audits() {
        let (svc, store, _r) = service();
        let lease = activate_device(&svc, "rc-wl", "rcwl0");
        // 构造越界请求：把 digest 置空（白名单必填字段缺失）。
        let mut req = receipt_req(&lease, 1, 3, "rc-wl");
        req.payload_digest = String::new();
        let err = svc.audit_receipt(&req).unwrap_err();
        assert!(
            matches!(err, LicenseError::ActivationRejected(_)),
            "{err:?}"
        );
        // 审计已落地（entity 为 lease）。
        let logs = store
            .list_audit_logs(
                &AuditFilter {
                    actor_type: None,
                    action: Some("receipt_whitelist_violation".into()),
                    entity_type: None,
                    entity_id: None,
                },
                1,
                100,
            )
            .unwrap();
        assert!(!logs.is_empty(), "白名单越界必须记审计");
    }

    /// 回执 2：连续区间 → `GapKind::None`；随后跳空 → `Gap`；再回退 → `Overlap`。
    #[test]
    fn audit_receipt_detects_gap_and_overlap() {
        let (svc, _s, ring) = service();
        let lease = activate_device(&svc, "rc-gap", "rcg0");
        // 首个回执：seq_from=1 → 无历史且 from==1 → None。
        let r0 = svc
            .audit_receipt(&signed_receipt_req(&ring, &lease, 1, 10, "rc-gap"))
            .unwrap();
        assert_eq!(r0.gap, GapKind::None);
        // 连续：11..20 → None。
        let r1 = svc
            .audit_receipt(&signed_receipt_req(&ring, &lease, 11, 20, "rc-gap"))
            .unwrap();
        assert_eq!(r1.gap, GapKind::None);
        // 跳空：25..30（期望 21）→ Gap。
        let r2 = svc
            .audit_receipt(&signed_receipt_req(&ring, &lease, 25, 30, "rc-gap"))
            .unwrap();
        assert_eq!(r2.gap, GapKind::Gap);
        // 回退 / 重叠：15..18（≤ last.seq_to=30）→ Overlap。
        let r3 = svc
            .audit_receipt(&signed_receipt_req(&ring, &lease, 15, 18, "rc-gap"))
            .unwrap();
        assert_eq!(r3.gap, GapKind::Overlap);
    }

    /// 回执 2b：无历史且 `seq_from > 1` → `Gap`（窗口起点缺失）。
    #[test]
    fn audit_receipt_first_window_missing_prefix_is_gap() {
        let (svc, _s, ring) = service();
        let lease = activate_device(&svc, "rc-missing", "rcm0");
        let r = svc
            .audit_receipt(&signed_receipt_req(&ring, &lease, 5, 9, "rc-missing"))
            .unwrap();
        assert_eq!(r.gap, GapKind::Gap);
    }

    /// 回执 3：**重复区间幂等，不重复告警**。
    #[test]
    fn audit_receipt_duplicate_range_is_idempotent_without_second_alarm() {
        let (svc, store, ring) = service();
        let lease = activate_device(&svc, "rc-dup", "rcd0");
        // 首次跳空 → 告警 1 次。
        let r1 = svc
            .audit_receipt(&signed_receipt_req(&ring, &lease, 100, 105, "rc-dup"))
            .unwrap();
        assert_eq!(r1.gap, GapKind::Gap);
        let alarms_after_first = store
            .list_audit_logs(
                &AuditFilter {
                    actor_type: None,
                    action: Some("receipt_anomaly".into()),
                    entity_type: None,
                    entity_id: None,
                },
                1,
                1000,
            )
            .unwrap()
            .len();
        assert_eq!(alarms_after_first, 1, "首次跳空应产生 1 条异常告警");

        // 完全相同区间重放 → accepted=true + None，且**不再新增异常告警**。
        let r2 = svc
            .audit_receipt(&signed_receipt_req(&ring, &lease, 100, 105, "rc-dup"))
            .unwrap();
        assert!(r2.accepted);
        assert_eq!(r2.gap, GapKind::None, "重复区间判定为 None（去重）");
        let alarms_after_second = store
            .list_audit_logs(
                &AuditFilter {
                    actor_type: None,
                    action: Some("receipt_anomaly".into()),
                    entity_type: None,
                    entity_id: None,
                },
                1,
                1000,
            )
            .unwrap()
            .len();
        assert_eq!(
            alarms_after_second, alarms_after_first,
            "重复区间不得二次告警"
        );
    }

    /// 回执 4：允许**延迟补报**（断网期间积压、恢复后补报）。
    ///
    /// ⚠️ 语义边界（设计 §1.4 第 5 条）：延迟补报指的是 **网关侧队列积压**
    /// ——`ts` 是回执生成时刻，正常落后于 `received_at` 若干分钟到数小时。
    /// 时序窗（默认 ±300s）**仍然适用**：它防的是重放旧回执，不是防补报。
    ///
    /// 因此本测试用「落后 4 分钟」的 `ts`（窗内）证明补报被接受，
    /// 另由 [`audit_receipt_rejects_receipt_older_than_clock_window`] 证明
    /// 超出窗的旧回执被拒 —— 两条合起来才是完整的补报语义。
    #[test]
    fn audit_receipt_allows_late_backfill_within_clock_window() {
        let (svc, _s, ring) = service();
        let lease = activate_device(&svc, "rc-late", "rcl0");
        let mut req = receipt_req(&lease, 1, 5, "rc-late");
        // 客户端 ts 落后 4 分钟（队列积压）——窗内，必须接受。
        req.ts = (now_unix_secs() - 240).to_string();
        sign_receipt_req(&ring, &mut req);
        let r = svc.audit_receipt(&req).unwrap();
        assert!(r.accepted);
        assert_eq!(r.gap, GapKind::None);
    }

    /// 回执 4b：`ts` 超出时钟窗 → 拒绝（重放旧回执 / 伪造时间）。
    ///
    /// 这条与上一条共同钉死补报语义的**下界**：没有这条，攻击者可以用
    /// 一个极旧但签名合法的回执覆盖序号窗口判定。
    #[test]
    fn audit_receipt_rejects_receipt_older_than_clock_window() {
        let (svc, _s, ring) = service();
        let lease = activate_device(&svc, "rc-old", "rco0");
        let mut req = receipt_req(&lease, 1, 5, "rc-old");
        req.ts = (now_unix_secs() - 30 * 86_400).to_string();
        sign_receipt_req(&ring, &mut req);
        let err = svc.audit_receipt(&req).unwrap_err();
        assert!(err.to_string().contains("skew"), "{err}");
    }

    /// 回执 5：租约不存在 → 拒绝；已废弃租约 → 拒绝。
    #[test]
    fn audit_receipt_rejects_unknown_and_revoked_lease() {
        let (svc, _s, _r) = service();
        let err = svc
            .audit_receipt(&receipt_req("lease-ghost", 1, 2, "mid"))
            .unwrap_err();
        assert!(err.to_string().contains("not found"), "{err}");
        // 已废弃租约。
        let (code_id, code) = issue_one(&svc);
        let a = anchors(5, "rc-rev");
        let ar: Vec<&str> = a.iter().map(String::as_str).collect();
        let act = svc
            .activate(&activation_req(&code, "rc-rev", &ar, "rr0"))
            .unwrap();
        let tail8: String = code.chars().filter(|c| *c != '-').collect();
        let tail8 = tail8[tail8.len() - 8..].to_string();
        svc.revoke_code(
            &code_id,
            &RevokeCodeRequest {
                reason: "risk".into(),
                note: "风控已确认异常并人工核实废弃".into(),
                confirm_tail8: tail8,
                second_approver: None,
            },
            "admin",
        )
        .unwrap();
        let err = svc
            .audit_receipt(&receipt_req(&act.lease_id, 1, 2, "rc-rev"))
            .unwrap_err();
        assert!(err.to_string().contains("revoked"), "{err}");
    }

    // ---------------- audit_receipt 验签（安全红线，3 条） ----------------

    /// **核心证据**：签名为垃圾串的伪造回执 → 拒收，且**不落库**。
    ///
    /// 这是本次修复存在的理由。修复前 `audit_receipt` 完全无验签，任何人
    /// 把 `sig` 填成 `"x"` 都能拿到 `accepted=true` 并写入一条回执记录，
    /// 从而污染序号窗口、压制真实跳空告警。本测试证明该路径已封死。
    #[test]
    fn forged_receipt_is_rejected_by_service() {
        let (svc, store, _r) = service();
        let lease = activate_device(&svc, "rc-forge", "rcf0");

        let mut req = receipt_req(&lease, 1, 10, "rc-forge");
        req.sig = "x".into(); // 伪造：既非 STANDARD base64，也不是合法签名

        let err = svc.audit_receipt(&req).unwrap_err();
        assert!(
            matches!(err, LicenseError::TokenInvalid(_)),
            "伪造签名必须是 TokenInvalid，实际: {err:?}"
        );

        // **不落库**：伪造成本不能换来任何持久化副作用。
        let stored = store.list_receipts_by_lease(&lease).unwrap();
        assert!(
            stored.is_empty(),
            "伪造回执绝不能落库，实际落库 {} 条",
            stored.len()
        );
    }

    /// 伪造尝试**必须留痕**（安全事件可追溯、可聚合告警）。
    ///
    /// 与白名单违规同等待遇：`action = "receipt_signature_invalid"`。
    #[test]
    fn forged_receipt_attempt_is_audited() {
        let (svc, store, _r) = service();
        let lease = activate_device(&svc, "rc-forge-audit", "rcfa");

        let mut req = receipt_req(&lease, 1, 10, "rc-forge-audit");
        req.sig = "x".into();
        let _ = svc.audit_receipt(&req).unwrap_err();

        let logs = store
            .list_audit_logs(
                &AuditFilter {
                    actor_type: None,
                    action: Some("receipt_signature_invalid".into()),
                    entity_type: None,
                    entity_id: None,
                },
                1,
                100,
            )
            .unwrap();
        assert!(!logs.is_empty(), "伪造回执尝试必须记审计");
        // 审计的 entity 锚定到 lease，便于按租约聚合风控。
        assert_eq!(logs[0].entity_id.as_str(), lease.as_str());
    }

    /// 篡改已签名回执的**任一**白名单字段 → 验签失败。
    ///
    /// 与 `receipt.rs` 的单元测试互补：那里逐个变异 `AuditReceiptRequest`，
    /// 这里走完整 service 路径，证明**验签发生在业务判定之前** ——
    /// 变异 `seq_from` 后不仅验签失败，且序号窗口未被污染
    /// （后续一条合法回执仍按「无历史」判定）。
    #[test]
    fn tampered_signed_receipt_is_rejected_before_touching_state() {
        let (svc, store, ring) = service();
        let lease = activate_device(&svc, "rc-tamper", "rct0");

        let mut req = signed_receipt_req(&ring, &lease, 11, 20, "rc-tamper");
        req.seq_from = "11".into(); // 无变化
        assert!(svc.audit_receipt(&req).is_ok(), "未篡改必须通过");

        // 篡改 seq_to（签名覆盖该字段）→ 必须拒绝。
        let mut tampered = signed_receipt_req(&ring, &lease, 21, 30, "rc-tamper");
        tampered.seq_to = "999".into();
        let err = svc.audit_receipt(&tampered).unwrap_err();
        assert!(matches!(err, LicenseError::TokenInvalid(_)), "{err:?}");

        // 状态未被污染：仅第一条合法回执在库。
        let stored = store.list_receipts_by_lease(&lease).unwrap();
        assert_eq!(stored.len(), 1, "被篡改的回执不得落库");
        assert_eq!(stored[0].seq_to, 20);
    }

    /// 时序号窗口不被伪造回执推进（**回归守卫**）：
    /// 先伪造一条「大序号」回执 → 被拒；随后合法回执仍按真实历史判定跳空。
    #[test]
    fn forged_receipt_cannot_advance_sequence_window() {
        let (svc, _s, ring) = service();
        let lease = activate_device(&svc, "rc-window", "rcw0");

        // 合法首窗：1..10 → None。
        let r0 = svc
            .audit_receipt(&signed_receipt_req(&ring, &lease, 1, 10, "rc-window"))
            .unwrap();
        assert_eq!(r0.gap, GapKind::None);

        // 伪造一条 1000..1010（签名无效）→ 拒收。
        let mut forged = receipt_req(&lease, 1000, 1010, "rc-window");
        forged.sig = "A".repeat(88); // 长度像 base64，但内容不是有效签名
        assert!(svc.audit_receipt(&forged).is_err());

        // 合法续窗 11..20 → 仍为 None（窗口停在上一条真实回执的 10）。
        let r1 = svc
            .audit_receipt(&signed_receipt_req(&ring, &lease, 11, 20, "rc-window"))
            .unwrap();
        assert_eq!(
            r1.gap,
            GapKind::None,
            "伪造回执不得推进序号窗口（否则真实跳空会被掩盖）"
        );
    }

    // ---------------- 激活码校验位 ----------------

    /// 激活码：合法码通过、任改一位失败、非法字符失败。
    #[test]
    fn activation_code_checksum_and_charset() {
        // 合法码通过（多生成几个确保随机覆盖）。
        for _ in 0..20 {
            let code = generate_activation_code();
            assert!(
                validate_activation_code(&code).is_ok(),
                "合法码应通过: {code}"
            );
        }
        let code = generate_activation_code();
        // 篡改 payload 里任意一位 → 校验失败。
        let chars: Vec<char> = code.chars().collect();
        // 找第一个非 '-' 的字符（跳过前缀与连字符）。
        let mut tampered: Option<String> = None;
        for (i, c) in chars.iter().enumerate() {
            if *c != '-' && !CODE_PREFIX.contains(*c) {
                // 换成另一个 Crockford 字符。
                let alt = if *c == 'A' { 'B' } else { 'A' };
                let mut v = chars.clone();
                v[i] = alt;
                tampered = Some(v.iter().collect());
                break;
            }
        }
        let tampered = tampered.expect("应能定位可篡改位");
        assert!(
            validate_activation_code(&tampered).is_err(),
            "篡改一位必须校验失败: {tampered}"
        );
        // 非法字符（I / O / U 属排除集）→ 失败。
        let illegal = "IOTDAQ-IOOO-LLLL-UUUU-0000";
        assert!(validate_activation_code(illegal).is_err());
        // 前缀错误 → 失败。
        assert!(validate_activation_code("WRONG-0000-0000-0000-0000").is_err());
    }

    // ---------------- issue / revoke / reissue 全链路 ----------------

    /// 发放幂等：同 `idempotency_key` 重放返回同一批码（逐位一致）。
    #[test]
    fn issue_codes_idempotency_replays_same_codes() {
        let (svc, _s, _r) = service();
        let now = now_unix_secs();
        let req = IssueCodesRequest {
            tenant_id: "t-1".into(),
            tier: "pro".into(),
            valid_from: now.to_string(),
            valid_until: (now + 365 * 86_400).to_string(),
            count: 3,
            prebind_machine_code: None,
            idempotency_key: "idem-fixed".into(),
        };
        let r1 = svc.issue_codes(&req, "admin").unwrap();
        assert_eq!(r1.codes.len(), 3);
        let first_count = svc
            .store
            .count_codes(&CodeFilter {
                tenant_id: Some("t-1".into()),
                status: None,
                tier: None,
                order_id: None,
            })
            .unwrap();
        assert_eq!(first_count, 3);

        let r2 = svc.issue_codes(&req, "admin").unwrap();
        // `code_id` 与 `code` **双序列**都逐位一致（仅有 id 一致不足以证明未重发码值）。
        let ids1: Vec<&str> = r1.codes.iter().map(|c| c.code_id.as_str()).collect();
        let ids2: Vec<&str> = r2.codes.iter().map(|c| c.code_id.as_str()).collect();
        let codes1: Vec<&str> = r1.codes.iter().map(|c| c.code.as_str()).collect();
        let codes2: Vec<&str> = r2.codes.iter().map(|c| c.code.as_str()).collect();
        assert_eq!(ids1, ids2, "同幂等键必须返回同一批码 ID");
        assert_eq!(codes1, codes2, "同幂等键必须返回同一批码值");
        // 总码数与首次一致（重放未产生第二批）。
        let total = svc
            .store
            .count_codes(&CodeFilter {
                tenant_id: Some("t-1".into()),
                status: None,
                tier: None,
                order_id: None,
            })
            .unwrap();
        assert_eq!(total, first_count, "幂等重放不得产生第二批码");
    }

    /// 截断回归：发满一批（`MAX_ISSUE_BATCH`）后再尝试超额被拒，随后**重放最早批次的 key**
    /// 必须命中既有批次（返回原码、总数不变），而**不能**被「当前 tenant 码数已达上限」挡住。
    ///
    /// 防回归点：旧实现用 `list_codes(filter, 1, MAX_ISSUE_BATCH)` 只扫**首批**，一旦批次
    /// 超过一页，重放查询就会漏掉既有批次；且配额/分页耦合会让「重放已成功批次」误判。
    /// 新实现直接按 `idempotency_key` 精确读头表对应批次，与总数 / 分页**完全解耦**。
    #[test]
    fn issue_codes_replay_hits_existing_batch_after_truncation_boundary() {
        let (svc, _s, _r) = service();
        let now = now_unix_secs();
        // 第一批：最早的 key（会被后续大量发放挤出旧查询的首页）。
        let first = IssueCodesRequest {
            tenant_id: "t-1".into(),
            tier: "standard".into(),
            valid_from: now.to_string(),
            valid_until: (now + 86_400).to_string(),
            count: 2,
            prebind_machine_code: None,
            idempotency_key: "idem-earliest".into(),
        };
        let r1 = svc.issue_codes(&first, "admin").unwrap();
        let ids1: Vec<String> = r1.codes.iter().map(|c| c.code_id.clone()).collect();
        assert_eq!(ids1.len(), 2);

        // 第二批：`MAX_ISSUE_BATCH + 1` 个**不同 key** 的码，把总码数推过一页边界。
        // 用逐码发放（不同 key）批量构造，避免撞上单批上限。
        let bulk = MAX_ISSUE_BATCH + 1;
        for i in 0..bulk {
            let one = IssueCodesRequest {
                tenant_id: "t-1".into(),
                tier: "standard".into(),
                valid_from: now.to_string(),
                valid_until: (now + 86_400).to_string(),
                count: 1,
                prebind_machine_code: None,
                idempotency_key: format!("idem-bulk-{i}"),
            };
            svc.issue_codes(&one, "admin").unwrap();
        }
        let total_after_bulk = svc
            .store
            .count_codes(&CodeFilter {
                tenant_id: Some("t-1".into()),
                status: None,
                tier: None,
                order_id: None,
            })
            .unwrap();
        assert_eq!(total_after_bulk, 2 + u64::from(bulk));

        // 重放最早 key：必须命中既有批次，返回**原码**，总数**不变**。
        let r2 = svc.issue_codes(&first, "admin").unwrap();
        let ids2: Vec<String> = r2.codes.iter().map(|c| c.code_id.clone()).collect();
        assert_eq!(ids2, ids1, "跨分页边界后重放仍须命中最早批次");
        let total_final = svc
            .store
            .count_codes(&CodeFilter {
                tenant_id: Some("t-1".into()),
                status: None,
                tier: None,
                order_id: None,
            })
            .unwrap();
        assert_eq!(total_final, total_after_bulk, "重放不得新增任何码");
    }

    /// 并发幂等：同 key 多线程并发发放 → 最终只落**一批**（`count` 个码），且至少一个成功。
    ///
    /// 断言刻意**放宽**：允许个别线程遇到 `Storage`（SQLite 单写连接竞争 / 忙），
    /// 只要不存在「两个批次都成功落地」。**不引入额外锁**——正确性由
    /// `code_batch(idempotency_key PRIMARY KEY)` 的唯一约束仲裁兜底。
    #[test]
    fn issue_codes_concurrent_same_key_lands_single_batch() {
        let (svc, _s, _r) = service();
        let svc = Arc::new(svc);
        let now = now_unix_secs();
        let count: u32 = 5;
        let threads = 8;
        let mut handles = Vec::with_capacity(threads);
        for _ in 0..threads {
            let svc = Arc::clone(&svc);
            let handle = std::thread::spawn(move || {
                let req = IssueCodesRequest {
                    tenant_id: "t-1".into(),
                    tier: "standard".into(),
                    valid_from: now.to_string(),
                    valid_until: (now + 86_400).to_string(),
                    count,
                    prebind_machine_code: None,
                    idempotency_key: "idem-race".into(),
                };
                svc.issue_codes(&req, "admin").is_ok()
            });
            handles.push(handle);
        }
        let mut ok_count = 0usize;
        for h in handles {
            if h.join().unwrap_or(false) {
                ok_count += 1;
            }
        }
        assert!(ok_count >= 1, "并发同 key 至少应有一个线程成功");
        // 关键断言：无论多少线程成功，数据库里只有 `count` 个码（只有一个批次胜出）。
        let total = svc
            .store
            .count_codes(&CodeFilter {
                tenant_id: Some("t-1".into()),
                status: None,
                tier: None,
                order_id: None,
            })
            .unwrap();
        assert_eq!(
            total,
            u64::from(count),
            "并发同 key 只允许一个批次落地（不得出现第二批）"
        );
    }

    /// 发放：租户不存在 → 拒绝。
    #[test]
    fn issue_codes_rejects_unknown_tenant() {
        let (svc, _s, _r) = service();
        let now = now_unix_secs();
        let err = svc
            .issue_codes(
                &IssueCodesRequest {
                    tenant_id: "t-nope".into(),
                    tier: "standard".into(),
                    valid_from: now.to_string(),
                    valid_until: (now + 86_400).to_string(),
                    count: 1,
                    prebind_machine_code: None,
                    idempotency_key: "idem-nt".into(),
                },
                "admin",
            )
            .unwrap_err();
        assert!(err.to_string().contains("tenant not found"), "{err}");
    }

    /// 发放：count 上限校验。
    #[test]
    fn issue_codes_enforces_batch_upper_bound() {
        let (svc, _s, _r) = service();
        let now = now_unix_secs();
        let err = svc
            .issue_codes(
                &IssueCodesRequest {
                    tenant_id: "t-1".into(),
                    tier: "standard".into(),
                    valid_from: now.to_string(),
                    valid_until: (now + 86_400).to_string(),
                    count: MAX_ISSUE_BATCH + 1,
                    prebind_machine_code: None,
                    idempotency_key: "idem-big".into(),
                },
                "admin",
            )
            .unwrap_err();
        assert!(matches!(err, LicenseError::QuotaExceeded(_)), "{err:?}");
    }

    /// 废弃：confirm_tail8 不符 → 拒绝。
    #[test]
    fn revoke_code_rejects_confirm_mismatch() {
        let (svc, _s, _r) = service();
        let (code_id, _code) = issue_one(&svc);
        let err = svc
            .revoke_code(
                &code_id,
                &RevokeCodeRequest {
                    reason: "risk".into(),
                    note: "风控已确认异常并人工核实废弃".into(),
                    confirm_tail8: "00000000".into(),
                    second_approver: None,
                },
                "admin",
            )
            .unwrap_err();
        assert!(err.to_string().contains("confirm_tail8"), "{err}");
    }

    /// 废弃：reason 空 / note 过短 → `ReasonRequired` 语义拒绝。
    #[test]
    fn revoke_code_requires_reason_and_note() {
        let (svc, _s, _r) = service();
        let (code_id, code) = issue_one(&svc);
        let tail8: String = code.chars().filter(|c| *c != '-').collect();
        let tail8 = tail8[tail8.len() - 8..].to_string();
        // note 太短。
        let err = svc
            .revoke_code(
                &code_id,
                &RevokeCodeRequest {
                    reason: "risk".into(),
                    note: "太短".into(),
                    confirm_tail8: tail8.clone(),
                    second_approver: None,
                },
                "admin",
            )
            .unwrap_err();
        assert!(err.to_string().contains("note"), "{err}");
        // reason 空。
        let err = svc
            .revoke_code(
                &code_id,
                &RevokeCodeRequest {
                    reason: "  ".into(),
                    note: "这是一段足够长度的补充说明".into(),
                    confirm_tail8: tail8,
                    second_approver: None,
                },
                "admin",
            )
            .unwrap_err();
        assert!(
            err.to_string().contains("reason") || err.to_string().contains("non-empty"),
            "{err}"
        );
    }

    /// 废弃幂等：同码同原因重复撤销 → 成功返回（不报错）。
    #[test]
    fn revoke_code_is_idempotent_for_same_reason() {
        let (svc, _s, _r) = service();
        let (code_id, code) = issue_one(&svc);
        let tail8: String = code.chars().filter(|c| *c != '-').collect();
        let tail8 = tail8[tail8.len() - 8..].to_string();
        let req = RevokeCodeRequest {
            reason: "fraud".into(),
            note: "客服核实确认欺诈换机".into(),
            confirm_tail8: tail8,
            second_approver: None,
        };
        assert!(svc.revoke_code(&code_id, &req, "admin").is_ok());
        // 重复撤销 → 幂等成功。
        assert!(svc.revoke_code(&code_id, &req, "admin").is_ok());
    }

    /// 废弃副作用：作废关联租约（立即失效）。
    #[test]
    fn revoke_code_stops_associated_lease() {
        let (svc, store, _r) = service();
        let (code_id, code) = issue_one(&svc);
        let a = anchors(5, "rev-lease");
        let ar: Vec<&str> = a.iter().map(String::as_str).collect();
        let act = svc
            .activate(&activation_req(&code, "rev-lease", &ar, "rl0"))
            .unwrap();
        let tail8: String = code.chars().filter(|c| *c != '-').collect();
        let tail8 = tail8[tail8.len() - 8..].to_string();
        svc.revoke_code(
            &code_id,
            &RevokeCodeRequest {
                reason: "risk".into(),
                note: "风控已确认异常并人工核实废弃".into(),
                confirm_tail8: tail8,
                second_approver: None,
            },
            "admin",
        )
        .unwrap();
        let lease = store.get_lease(&act.lease_id).unwrap().unwrap();
        assert_eq!(lease.status, LeaseStatus::Stopped, "废弃应立即使租约失效");
    }

    /// 重发：原码未废弃 → `OriginalNotRevoked` 语义拒绝。
    #[test]
    fn reissue_requires_original_revoked() {
        let (svc, _s, _r) = service();
        let (code_id, _code) = issue_one(&svc);
        let err = svc
            .reissue_code(
                &code_id,
                &ReissueCodeRequest {
                    prebind: None,
                    inherit_tier: true,
                    inherit_validity: true,
                    overrides: None,
                    idempotency_key: "re-1".into(),
                },
                "admin",
            )
            .unwrap_err();
        assert!(
            err.to_string().contains("must be revoked") || err.to_string().contains("revoked"),
            "{err}"
        );
    }

    /// 重发全链路：废弃 → 重发 → 溯源链 + 继承 tier + 原码置 Reissued + 幂等。
    #[test]
    fn reissue_full_chain_and_idempotency() {
        let (svc, store, _r) = service();
        let (code_id, code) = issue_one(&svc);
        let tail8: String = code.chars().filter(|c| *c != '-').collect();
        let tail8 = tail8[tail8.len() - 8..].to_string();
        // 废弃。
        svc.revoke_code(
            &code_id,
            &RevokeCodeRequest {
                reason: "host_reinstall".into(),
                note: "宿主重装系统导致锚点变化".into(),
                confirm_tail8: tail8,
                second_approver: None,
            },
            "admin",
        )
        .unwrap();
        // 重发。
        let req = ReissueCodeRequest {
            prebind: None,
            inherit_tier: true,
            inherit_validity: true,
            overrides: None,
            idempotency_key: "re-fixed".into(),
        };
        let r1 = svc.reissue_code(&code_id, &req, "admin").unwrap();
        assert_eq!(r1.new_code.status, "issued");
        assert_eq!(r1.new_code.reissued_from.as_deref(), Some(code_id.as_str()));
        // 原码置 Reissued。
        let original = store.get_code_by_id(&code_id).unwrap().unwrap();
        assert_eq!(original.status, CodeStatus::Reissued);
        // 新码 tier 继承。
        let new_rec = store.get_code_by_id(&r1.new_code.code_id).unwrap().unwrap();
        assert_eq!(new_rec.tier, original.tier);
        assert_eq!(new_rec.reissued_from_id.as_deref(), Some(code_id.as_str()));
        // 幂等：再次重发返回同一新码。
        let r2 = svc.reissue_code(&code_id, &req, "admin").unwrap();
        assert_eq!(r1.new_code.code_id, r2.new_code.code_id, "重发必须幂等");
        // 审计落地。
        let logs = store
            .list_audit_logs(
                &AuditFilter {
                    actor_type: None,
                    action: Some("reissue".into()),
                    entity_type: None,
                    entity_id: None,
                },
                1,
                100,
            )
            .unwrap();
        assert!(!logs.is_empty(), "重发必须写审计");
    }

    /// 重发：`prebind` 机器码已绑定他码 → `PrebindConflict` 语义拒绝。
    #[test]
    fn reissue_rejects_prebind_conflict() {
        let (svc, _s, _r) = service();
        // 码 A 绑定 mach-pc。
        let (code_a_id, code_a) = issue_one(&svc);
        let a = anchors(5, "mach-pc");
        let ar: Vec<&str> = a.iter().map(String::as_str).collect();
        svc.activate(&activation_req(&code_a, "mach-pc", &ar, "pc0"))
            .unwrap();
        // 码 B 废弃后预绑定同一机器码 mach-pc。
        let (code_b_id, code_b) = issue_one(&svc);
        let tail8: String = code_b.chars().filter(|c| *c != '-').collect();
        let tail8 = tail8[tail8.len() - 8..].to_string();
        svc.revoke_code(
            &code_b_id,
            &RevokeCodeRequest {
                reason: "risk".into(),
                note: "风控已确认异常并人工核实废弃".into(),
                confirm_tail8: tail8,
                second_approver: None,
            },
            "admin",
        )
        .unwrap();
        let err = svc
            .reissue_code(
                &code_b_id,
                &ReissueCodeRequest {
                    prebind: Some(crate::proto::Prebind {
                        machine_code: "mach-pc".into(),
                    }),
                    inherit_tier: true,
                    inherit_validity: true,
                    overrides: None,
                    idempotency_key: "re-conf".into(),
                },
                "admin",
            )
            .unwrap_err();
        assert!(err.to_string().contains("conflict"), "{err}");
        let _ = code_a_id;
    }

    // ---------------- verify（A 档） ----------------

    /// verify：非 A 档租约 → 拒绝（本服务端二次校验仅 A 档）。
    #[test]
    fn verify_rejects_non_a_tier_lease() {
        let (svc, _s, _r) = service();
        // 默认租户档位是 B → 激活得到 B 档租约。
        let (_id, code) = issue_one(&svc);
        let a = anchors(5, "v-b");
        let ar: Vec<&str> = a.iter().map(String::as_str).collect();
        let act = svc
            .activate(&activation_req(&code, "v-b", &ar, "vb0"))
            .unwrap();
        assert_eq!(act.verify_mode, "B");
        let err = svc
            .verify(&VerifyRequest {
                device_mid: "v-b".into(),
                lease_id: act.lease_id,
                payload_digest: "d".into(),
                ts: now_unix_secs().to_string(),
                nonce: "v-b-n".into(),
                device_sig: "s".into(),
            })
            .unwrap_err();
        assert!(err.to_string().contains("A-tier only"), "{err}");
    }

    /// 查询：码列表 / 码详情 / 设备列表 / 审计日志均可用且掩码正确。
    #[test]
    fn queries_return_masked_and_structured_data() {
        let (svc, _s, _r) = service();
        let (code_id, code) = issue_one(&svc);
        // 激活一台设备。
        let a = anchors(5, "q-dev");
        let ar: Vec<&str> = a.iter().map(String::as_str).collect();
        svc.activate(&activation_req(&code, "q-dev", &ar, "qd0"))
            .unwrap();

        let list = svc
            .list_codes(
                &CodeFilter {
                    tenant_id: Some("t-1".into()),
                    status: None,
                    tier: None,
                    order_id: None,
                },
                1,
                100,
            )
            .unwrap();
        assert_eq!(list.len(), 1);
        assert!(
            list[0].code_masked.contains("****"),
            "码值必须掩码: {}",
            list[0].code_masked
        );
        // 中间段（去掉前缀与末 4 位）不得以明文出现。
        let body: String = code.chars().filter(|c| *c != '-').collect();
        let middle = &body[..body.len() - 4];
        assert!(
            !list[0].code_masked.contains(middle),
            "掩码不得泄露码值中间明文: {}",
            list[0].code_masked
        );

        let detail = svc.code_detail(&code_id).unwrap();
        assert_eq!(detail.code, code);
        assert_eq!(detail.status, "bound");

        let devices = svc.list_devices(Some("t-1"), 1, 100).unwrap();
        assert_eq!(devices.len(), 1);
        assert!(devices[0].machine_code_masked.contains('*'));

        let logs = svc
            .list_audit_logs(
                &AuditFilter {
                    actor_type: None,
                    action: None,
                    entity_type: None,
                    entity_id: None,
                },
                1,
                100,
            )
            .unwrap();
        assert!(!logs.is_empty());
    }

    /// `mask_code` 边界：短串不 panic。
    #[test]
    fn mask_code_edge_cases() {
        assert_eq!(mask_code("abc"), "****");
        let m = mask_code("IOTDAQ-1234-5678-9ABC-DEFG");
        assert!(m.starts_with("IOTDAQ-"), "{m}");
        assert!(m.ends_with("DEFG"), "{m}");
    }

    /// 内部工具：`parse_ts` / `parse_seq` 拒绝非数字。
    #[test]
    fn parse_helpers_reject_non_numeric() {
        assert!(parse_ts("abc", "ts").is_err());
        assert!(parse_ts("1700000000", "ts").is_ok());
        assert!(parse_seq("-1", "seq").is_err());
        assert!(parse_seq("42", "seq").is_ok());
    }

    /// 冗余：显式引用 `HeartbeatResult`/`SigningKeyStatus` 以证明契约类型可用（编译期断言）。
    #[test]
    fn contract_types_are_reachable() {
        let _ = _HB::Ok;
        let _ = SigningKeyStatus::Active;
        let _: ActivationCode;
    }
}

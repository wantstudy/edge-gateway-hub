//! `licensing-server` 业务规则层（task 46：激活码生命周期 + 一机一码预绑定）。
//!
//! 本模块是 `activation_code` 状态机与一机一码判定的唯一权威实现位置，承载：
//!
//! - **发放（issue）**：批量生成激活码，支持 `prebind_machine_code`（预绑定到指定机器码）；
//!   通过 [`Store::claim_code_batch`] 把幂等仲裁下沉到数据库唯一约束，保证同 `idempotency_key`
//!   重放返回首次结果（绝不重复签发）。
//! - **激活（activate）**：按码值取码 → 校验可激活 / 有效期 → **强制预绑定匹配** → 解析或创建设备
//!   → 原子绑定（`bind_code_to_device`）→ 签发 Lease Token。
//! - **废弃（revoke）**：仅 `issued`/`bound` 可废弃；已废弃视为幂等成功（fail-closed）。
//! - **重发（reissue）**：原码 `revoked → reissued` 单向迁移，生成新码并溯源
//!   （`reissued_from_id`）；通过 `idempotency_key` 索引直查实现重发幂等。
//!
//! # G1–G5 行为落点（与 lead 验收一一对应）
//!
//! - **G1 预绑定持久化与强制**：预绑定在「发放」时经 [`ActivationCode::with_prebind`] 落库；
//!   「激活」时以 [`ActivationCode::prebind_matches`] 校验，不匹配 → [`LicenseError::prebind_conflict`]
//!   （`PrebindKind::ActivationMachineMismatch`）。空预绑定 = 不约束。
//! - **G2 重发幂等（fail-closed）**：经 `list_codes_by_batch_key(idempotency_key)` **索引直查**
//!   （不翻页），命中既有重发码则原样返回；绝不静默重复签发。
//! - **G3 预绑定冲突检测**：发放 / 重发时，同租户内已存在「预绑定到同机器码且仍可用」的码
//!   （[`Store::find_code_by_prebind`]）或「该机器码对应设备已被另一码绑定」
//!   （[`Store::find_bound_code_for_device`]）→ 结构化 [`LicenseError::PrebindConflict`]
//!   （`MachineAlreadyClaimed` / `MachineAlreadyBound`），绝不 panic。
//! - **G4 空白预绑定**：`"   "` / `"\t"` 一律视为「未提供」，统一经 [`ActivationCode::with_prebind`]
//!   的 `trim().is_empty()` 归一为 `None`，避免把设备永久锁死在空指纹上。
//! - **G5 幂等键归一**：存储 / 查询前对 `idempotency_key` 做 `trim()` 归一，避免尾部空白割裂同一逻辑键。

use crate::error::{LicenseError, LicenseResult, PrebindKind};
use crate::keys::KeyRing;
use crate::model::{
    now_ns_id, now_unix_secs, ActivationCode, ActorType, AuditLog, CodeStatus, Device,
    DeviceStatus, Lease, LeaseStatus, VerifyMode,
};
use crate::proto::{
    ActivationRequest, ActivationResponse, IssueCodesRequest, IssueCodesResponse, IssuedCode,
    ReissueCodeRequest, ReissueCodeResponse, RevokeCodeRequest,
};
use crate::store::Store;
use crate::token::{issue_lease_token, LeaseClaims};

/// 心跳周期（小时），随激活响应下发（设计 §1.1）。
const HEARTBEAT_HOURS: i64 = 24;

/// 授权服务：聚合 [`Store`] 与 [`KeyRing`]，对外暴露激活码生命周期业务方法。
///
/// 设计为「薄聚合」：所有 SQL 仍集中在 [`Store`]，所有签名集中在 [`KeyRing`] 与
/// [`crate::token`]，本结构体只做业务编排与状态机判定。
pub struct LicensingService {
    /// 仓储层（单连接 + Mutex，事务语义由 store 方法保证）。
    store: Store,
    /// 签名密钥环（Ed25519，私钥经环境变量注入，绝不落库）。
    keyring: KeyRing,
}

impl LicensingService {
    /// 构造服务（store 与 keyring 由调用方注入；keyring 须持可用签发密钥才能签发 Lease Token）。
    pub fn new(store: Store, keyring: KeyRing) -> Self {
        LicensingService { store, keyring }
    }

    /// 只读访问仓储层（测试与审核用）。
    pub fn store(&self) -> &Store {
        &self.store
    }

    /// 只读访问密钥环（测试与轮换用）。
    pub fn keyring(&self) -> &KeyRing {
        &self.keyring
    }

    // ----------------------------- 发放（issue） -----------------------------

    /// 批量发放激活码（支持预绑定 + 幂等）。
    ///
    /// 行为次序（见模块级 G1–G5 说明）：
    /// 1. 租户存在性校验；
    /// 2. **G5**：归一化 `idempotency_key`；
    /// 3. **G4**：预绑定经 `with_prebind` 归一（空白 → `None`）；
    /// 4. **G3**：同租户预绑定冲突检测；
    /// 5. **G2**：`claim_code_batch` 原子认领批次；若已认领（重复 / 并发重放）则直读既有批次返回；
    /// 6. 否则生成 `count` 张码并落库（每张码携带归一化后的 `idempotency_key`）。
    pub fn issue_codes(&self, req: &IssueCodesRequest) -> LicenseResult<IssueCodesResponse> {
        let tenant_id = req.tenant_id.trim();
        if tenant_id.is_empty() {
            return Err(LicenseError::ActivationRejected(
                "tenant_id must not be empty".into(),
            ));
        }
        if self.store.get_tenant(tenant_id)?.is_none() {
            return Err(LicenseError::ActivationRejected(format!(
                "unknown tenant: {tenant_id}"
            )));
        }

        // G5：归一化幂等键（trim / 去尾部空白），空键直接拒绝。
        let idem = self.normalize_idempotency_key(&req.idempotency_key)?;

        // G4：空白预绑定（"   " / "\t"）→ None，交给 with_prebind 统一归一。
        let prebind = req.prebind_machine_code.clone();

        // G3：同租户预绑定冲突检测（结构化错误，不 panic）。
        if let Some(mc) = &prebind {
            if !mc.trim().is_empty() {
                self.check_prebind_conflict(tenant_id, mc.trim())?;
            }
        }

        let now = now_unix_secs();

        // G2：批次幂等认领（数据库唯一约束仲裁，并发安全）。
        let claimed = self
            .store
            .claim_code_batch(&idem, tenant_id, req.count, now)?;
        if !claimed {
            // 重复 / 并发重放：直读既有批次，原样返回，绝不重新签发。
            let existing = self.store.list_codes_by_batch_key(&idem)?;
            let codes: Vec<IssuedCode> = existing.iter().map(Self::to_issued_code).collect();
            return Ok(IssueCodesResponse { codes });
        }

        let valid_from = Self::parse_ts(&req.valid_from)?;
        let valid_until = Self::parse_ts(&req.valid_until)?;
        if valid_until <= valid_from {
            return Err(LicenseError::KeyStateIllegal(
                "issue: valid_until must be greater than valid_from".into(),
            ));
        }

        let mut codes = Vec::with_capacity(req.count as usize);
        for _ in 0..req.count {
            let code_id = now_ns_id("ac");
            let code_value = Self::generate_code_value();
            let mut ac = ActivationCode::new_issued(
                code_id,
                code_value,
                tenant_id.to_string(),
                req.tier.clone(),
                valid_from,
                valid_until,
                // `source_order_id` 列在 schema 中为 NOT NULL（store.rs:122）；本端点不绑定订单，
                // 以空串作为「无关联订单」哨兵，避免把 `None` 落入 NOT NULL 列。
                Some(String::new()),
                "admin".to_string(),
                now,
            );
            // G1 + G4：预绑定经 with_prebind 落库（空白 → None）。
            ac = ac.with_prebind(prebind.clone());
            // G5：每张码共享归一化后的幂等键（批次头表承担唯一性）。
            ac.idempotency_key = Some(idem.clone());
            // 不变量校验（prebind 非空白、有效期非空区间等）；失败即 KeyStateIllegal。
            ac.validate()?;
            self.store.insert_code(&ac)?;
            codes.push(Self::to_issued_code(&ac));
        }

        self.audit(tenant_id, "admin", "issue", "activation_code", &idem, now)?;

        Ok(IssueCodesResponse { codes })
    }

    // ----------------------------- 激活（activate） -----------------------------

    /// 设备首次激活 + 一机一码绑定。
    ///
    /// **G1（预绑定强制）**：取码后先校验可激活、有效期，再以
    /// [`ActivationCode::prebind_matches`] 校验机器码；不匹配即返回结构化
    /// [`LicenseError::PrebindConflict`]（绝不走 `msg.contains` 反查）。
    pub fn activate(&self, req: &ActivationRequest) -> LicenseResult<ActivationResponse> {
        let now = now_unix_secs();

        let code_value = req.activation_code.trim();
        let code = self
            .store
            .get_code_by_value(code_value)?
            .ok_or_else(|| LicenseError::ActivationRejected("unknown activation code".into()))?;

        let tenant_id = code.tenant_id.clone();

        if !code.is_activatable() {
            return Err(LicenseError::ActivationRejected(format!(
                "activation code is not activatable (status={})",
                code.status.as_str()
            )));
        }
        if !code.is_valid_at(now) {
            return Err(LicenseError::ActivationRejected(
                "activation code is outside its validity window".into(),
            ));
        }

        // G1：预绑定匹配（None = 任意机器均可；Some = 精确匹配）。
        if !code.prebind_matches(&req.machine_code) {
            return Err(LicenseError::prebind_conflict(
                PrebindKind::ActivationMachineMismatch,
            ));
        }

        let machine_code = req.machine_code.clone();
        let device =
            self.resolve_or_create_device(&tenant_id, &machine_code, &req.anchor_hashes, now)?;

        // 原子绑定：单条条件 UPDATE + rows_affected 判定，防并发双绑。
        self.store
            .bind_code_to_device(&code.code_id, &device.device_id)?;

        // Lease + Token 签发。
        let lease_id = now_ns_id("lease");
        let claims = LeaseClaims {
            lease_id: lease_id.clone(),
            device_id: device.device_id.clone(),
            mid: machine_code.clone(),
            tier: code.tier.clone(),
            verify_mode: VerifyMode::B.as_str().to_string(),
            issued_at: now,
            valid_until: code.valid_until,
        };
        let token = issue_lease_token(&self.keyring, &claims)?;
        let lease = Lease {
            lease_id: lease_id.clone(),
            device_id: device.device_id.clone(),
            code_id: code.code_id.clone(),
            kid: token.kid.clone(),
            token_sig: token.signature_b64.clone(),
            verify_mode: VerifyMode::B,
            tier: code.tier.clone(),
            issued_at: now,
            valid_until: code.valid_until,
            last_heartbeat_at: None,
            status: LeaseStatus::Active,
        };
        self.store.insert_lease(&lease)?;
        self.store
            .update_device_status(&device.device_id, DeviceStatus::Active)?;

        // 服务端响应签名（防篡改；密钥来自 keyring）。
        let sig = self.sign_response(&lease_id, &req.nonce, now)?;

        self.audit(
            &tenant_id,
            &machine_code,
            "activation",
            "activation_code",
            &code.code_id,
            now,
        )?;

        Ok(ActivationResponse {
            lease_id,
            lease_token: token.encode(),
            verify_mode: VerifyMode::B.as_str().to_string(),
            tier: code.tier.clone(),
            valid_until: code.valid_until.to_string(),
            heartbeat_hours: HEARTBEAT_HOURS,
            server_time: now.to_string(),
            nonce: req.nonce.clone(),
            sig,
        })
    }

    // ----------------------------- 废弃（revoke） -----------------------------

    /// 废弃激活码（仅总管理后台；`issued` / `bound` → `revoked`）。
    ///
    /// **G2（revoke 幂等）**：已 `revoked` 的原码视为幂等成功（no-op），
    /// 既不报错也不重复动作——fail-closed，绝不重新签发或重复废弃。
    pub fn revoke(
        &self,
        tenant_id: &str,
        code_id: &str,
        req: &RevokeCodeRequest,
        actor_id: &str,
    ) -> LicenseResult<()> {
        let now = now_unix_secs();

        if tenant_id.trim().is_empty() {
            return Err(LicenseError::ActivationRejected(
                "tenant_id must not be empty".into(),
            ));
        }
        if self.store.get_tenant(tenant_id)?.is_none() {
            return Err(LicenseError::ActivationRejected(format!(
                "unknown tenant: {tenant_id}"
            )));
        }

        let code = self.store.get_code_by_id(code_id)?.ok_or_else(|| {
            LicenseError::KeyStateIllegal(format!("activation code not found: {code_id}"))
        })?;
        if code.tenant_id != tenant_id {
            return Err(LicenseError::KeyStateIllegal(
                "activation code belongs to a different tenant".into(),
            ));
        }

        // G2：已废弃 = 幂等 no-op（fail-closed）。
        if matches!(code.status, CodeStatus::Revoked) {
            return Ok(());
        }

        if req.reason.trim().is_empty() {
            return Err(LicenseError::KeyStateIllegal(
                "revoke requires a non-empty reason".into(),
            ));
        }

        self.store.revoke_code(code_id, &req.reason, now)?;
        self.audit(
            tenant_id,
            actor_id,
            "revoke",
            "activation_code",
            code_id,
            now,
        )?;
        Ok(())
    }

    // ----------------------------- 重发（reissue） -----------------------------

    /// 重发激活码（换机迁移：原码 `revoked → reissued`，生成新码并溯源）。
    ///
    /// **G2（重发幂等，fail-closed）**：经 `list_codes_by_batch_key(idempotency_key)`
    /// **索引直查**（不翻页、不内存过滤），命中既有重发码（其 `reissued_from_id == 原码`）
    /// 则原样返回，绝不重复签发新码。
    /// **G3**：新预绑定若与同租户已用码 / 已绑定设备冲突 → 结构化 [`LicenseError::PrebindConflict`]。
    /// **G4**：空白预绑定 → `None`。
    /// **G5**：`idempotency_key` 归一后存储 / 查询。
    #[allow(clippy::too_many_arguments)]
    pub fn reissue(
        &self,
        tenant_id: &str,
        original_code_id: &str,
        req: &ReissueCodeRequest,
        actor_id: &str,
    ) -> LicenseResult<ReissueCodeResponse> {
        let now = now_unix_secs();

        // G5：归一化幂等键。
        let idem = self.normalize_idempotency_key(&req.idempotency_key)?;

        // G2：索引直查既有重发码（同一逻辑键 → 同一条既有记录）。
        let prior = self.store.list_codes_by_batch_key(&idem)?;
        for c in &prior {
            if c.reissued_from_id.as_deref() == Some(original_code_id) {
                return Ok(ReissueCodeResponse {
                    new_code: Self::to_issued_code(c),
                });
            }
        }

        if tenant_id.trim().is_empty() {
            return Err(LicenseError::ActivationRejected(
                "tenant_id must not be empty".into(),
            ));
        }
        if self.store.get_tenant(tenant_id)?.is_none() {
            return Err(LicenseError::ActivationRejected(format!(
                "unknown tenant: {tenant_id}"
            )));
        }

        let original = self
            .store
            .get_code_by_id(original_code_id)?
            .ok_or_else(|| {
                LicenseError::KeyStateIllegal(format!(
                    "activation code not found: {original_code_id}"
                ))
            })?;
        if original.tenant_id != tenant_id {
            return Err(LicenseError::KeyStateIllegal(
                "activation code belongs to a different tenant".into(),
            ));
        }
        // 重发的前置：原码必须已废弃。
        if !matches!(original.status, CodeStatus::Revoked) {
            return Err(LicenseError::KeyStateIllegal(format!(
                "reissue requires a revoked activation code; {} is {}",
                original_code_id,
                original.status.as_str()
            )));
        }

        // G4：空白预绑定 → None（交由 with_prebind 归一）。
        let new_prebind: Option<String> = req.prebind.as_ref().map(|p| p.machine_code.clone());

        // G3：同租户预绑定冲突检测。
        if let Some(mc) = &new_prebind {
            if !mc.trim().is_empty() {
                self.check_prebind_conflict(tenant_id, mc.trim())?;
            }
        }

        // tier / 有效期继承或覆盖。
        let tier = if req.inherit_tier {
            original.tier.clone()
        } else {
            req.overrides
                .as_ref()
                .and_then(|o| o.tier.clone())
                .unwrap_or_else(|| original.tier.clone())
        };

        let valid_from = now;
        let valid_until = if req.inherit_validity {
            original.valid_until
        } else {
            req.overrides
                .as_ref()
                .and_then(|o| o.valid_until.as_ref())
                .and_then(|v| Self::parse_ts(v).ok())
                .unwrap_or(original.valid_until)
        };
        if valid_until <= valid_from {
            return Err(LicenseError::KeyStateIllegal(
                "reissue: resulting validity window is empty".into(),
            ));
        }

        let code_id = now_ns_id("ac");
        let code_value = Self::generate_code_value();
        let mut new_code = ActivationCode::new_issued(
            code_id,
            code_value,
            tenant_id.to_string(),
            tier,
            valid_from,
            valid_until,
            Some(original.source_order_id.clone().unwrap_or_default()),
            actor_id.to_string(),
            now,
        );
        // G1 + G4：预绑定落库（空白 → None）。
        new_code = new_code.with_prebind(new_prebind);
        // G5：重发码携带归一化幂等键，供索引直查幂等。
        new_code.idempotency_key = Some(idem.clone());
        // 溯源：新码指向原码（原码随后置 `reissued`）。
        new_code.reissued_from_id = Some(original_code_id.to_string());
        new_code.validate()?;
        self.store.insert_code(&new_code)?;
        // 原码单向迁移 revoked → reissued。
        self.store.mark_code_reissued(original_code_id)?;

        self.audit(
            tenant_id,
            actor_id,
            "reissue",
            "activation_code",
            original_code_id,
            now,
        )?;

        Ok(ReissueCodeResponse {
            new_code: Self::to_issued_code(&new_code),
        })
    }

    // ----------------------------- 内部辅助 -----------------------------

    /// 解析时间戳字符串为 `i64`（JSON 路径一律 String，见设计大整数红线）。
    fn parse_ts(value: &str) -> LicenseResult<i64> {
        value
            .trim()
            .parse::<i64>()
            .map_err(|_| LicenseError::KeyStateIllegal(format!("invalid timestamp: {value}")))
    }

    /// 归一化幂等键：trim 头部 / 尾部空白；空键直接拒绝。
    fn normalize_idempotency_key(&self, key: &str) -> LicenseResult<String> {
        let normalized = key.trim().to_string();
        if normalized.is_empty() {
            return Err(LicenseError::ActivationRejected(
                "idempotency_key must not be empty".into(),
            ));
        }
        Ok(normalized)
    }

    /// 生成随机激活码值（`IOTDAQ-XXXX-XXXX-XXXX-XXXX`，4 组 16-bit hex）。
    ///
    /// **绝不硬编码**：熵来自 `rand`，且码值**不进日志 / 不进错误信息**。
    fn generate_code_value() -> String {
        use rand::Rng as _;
        let mut rng = rand::rng();
        let groups: Vec<String> = (0..4)
            .map(|_| {
                let n: u32 = rng.random();
                format!("{:04X}", n & 0xFFFF)
            })
            .collect();
        format!("IOTDAQ-{}", groups.join("-"))
    }

    /// 把 [`ActivationCode`] 投影为对外响应结构 [`IssuedCode`]。
    fn to_issued_code(code: &ActivationCode) -> IssuedCode {
        IssuedCode {
            code_id: code.code_id.clone(),
            code: code.code.clone(),
            status: code.status.as_str().to_string(),
            prebind: code.prebind_machine_code.clone(),
            reissued_from: code.reissued_from_id.clone(),
        }
    }

    /// G3 预绑定冲突检测（同租户）：返回结构化 [`LicenseError::PrebindConflict`]。
    ///
    /// 覆盖两条冲突路径（设计 §「一机一码」）：
    /// 1. **码侧**：已有仍可用码（`issued`/`bound`）预绑定到同机器码 → `MachineAlreadyClaimed`；
    /// 2. **设备侧**：该机器码对应设备已被另一张码实际绑定 → `MachineAlreadyBound`。
    fn check_prebind_conflict(&self, tenant_id: &str, machine_code: &str) -> LicenseResult<()> {
        if let Some(existing) = self.store.find_code_by_prebind(machine_code)? {
            if existing.tenant_id == tenant_id {
                return Err(LicenseError::prebind_conflict(
                    PrebindKind::MachineAlreadyClaimed,
                ));
            }
        }
        if let Some(device) = self.store.get_device_by_machine_code(machine_code)? {
            if let Some(bound) = self.store.find_bound_code_for_device(&device.device_id)? {
                if bound.tenant_id == tenant_id {
                    return Err(LicenseError::prebind_conflict(
                        PrebindKind::MachineAlreadyBound,
                    ));
                }
            }
        }
        Ok(())
    }

    /// 解析或创建设备：按机器码查找；不存在则新建（并发插入竞态下回读复用）。
    fn resolve_or_create_device(
        &self,
        tenant_id: &str,
        machine_code: &str,
        anchor_hashes: &[String],
        now: i64,
    ) -> LicenseResult<Device> {
        if let Some(device) = self.store.get_device_by_machine_code(machine_code)? {
            if device.tenant_id != tenant_id {
                return Err(LicenseError::ActivationRejected(
                    "machine_code is registered to a different tenant".into(),
                ));
            }
            return Ok(device);
        }
        let device_id = now_ns_id("dev");
        let mut dev = Device::new(
            device_id,
            tenant_id.to_string(),
            machine_code.to_string(),
            anchor_hashes.to_vec(),
            now,
        );
        dev.first_activation_at = Some(now);
        match self.store.insert_device(&dev) {
            Ok(()) => Ok(dev),
            Err(e) => {
                // 并发下可能已被同机请求抢先插入：回读后复用，避免重复设备记录。
                match self.store.get_device_by_machine_code(machine_code)? {
                    Some(device) => {
                        if device.tenant_id != tenant_id {
                            return Err(LicenseError::ActivationRejected(
                                "machine_code is registered to a different tenant".into(),
                            ));
                        }
                        Ok(device)
                    }
                    None => Err(e),
                }
            }
        }
    }

    /// 服务端对激活响应的签名（防篡改；密钥来自密钥环）。
    fn sign_response(
        &self,
        lease_id: &str,
        nonce: &str,
        server_time: i64,
    ) -> LicenseResult<String> {
        let message = format!("activation|{lease_id}|{nonce}|{server_time}");
        let (_kid, sig) = self.keyring.sign(message.as_bytes())?;
        Ok(sig)
    }

    /// 写入一条审计日志（后台动作统一入口）。
    fn audit(
        &self,
        _tenant_id: &str,
        actor_id: &str,
        action: &str,
        entity_type: &str,
        entity_id: &str,
        now: i64,
    ) -> LicenseResult<()> {
        let log = AuditLog {
            id: now_ns_id("audit"),
            actor_type: ActorType::Admin,
            actor_id: actor_id.to_string(),
            action: action.to_string(),
            entity_type: entity_type.to_string(),
            entity_id: entity_id.to_string(),
            detail: String::new(),
            ts: now,
            ip: String::new(),
        };
        self.store.insert_audit_log(&log)
    }
}

#[cfg(test)]
mod tests {
    use super::LicensingService;
    use crate::error::{LicenseError, PrebindKind};
    use crate::keys::KeyRing;
    use crate::model::{now_unix_secs, CodeStatus, Tenant};
    use crate::proto::{
        ActivationRequest, IssueCodesRequest, Prebind, ReissueCodeRequest, RevokeCodeRequest,
    };
    use crate::store::Store;
    use std::sync::Arc;

    /// 构造带内存库 + 已注册签发密钥的服务（测试专用）。
    fn build_service() -> Arc<LicensingService> {
        let store = Store::open_in_memory().expect("open in-memory store");
        let tenant = Tenant::new(
            "t-1".to_string(),
            "Tenant".to_string(),
            "ops@x".to_string(),
            now_unix_secs(),
        );
        store.insert_tenant(&tenant).expect("seed tenant");
        let keyring = KeyRing::empty();
        keyring
            .register_generated(None, 1_700_000_000)
            .expect("register signing key");
        Arc::new(LicensingService::new(store, keyring))
    }

    fn issue_req(tenant: &str, prebind: Option<&str>, idem: &str) -> IssueCodesRequest {
        let n = now_unix_secs();
        IssueCodesRequest {
            tenant_id: tenant.to_string(),
            tier: "pro".to_string(),
            valid_from: (n - 1000).to_string(),
            valid_until: (n + 365 * 86_400).to_string(),
            count: 1,
            prebind_machine_code: prebind.map(|s| s.to_string()),
            idempotency_key: idem.to_string(),
        }
    }

    fn activate_req(code_value: &str, machine: &str) -> ActivationRequest {
        ActivationRequest {
            activation_code: code_value.to_string(),
            machine_code: machine.to_string(),
            anchor_hashes: vec!["a".to_string(); 5],
            device_pubkey: "x".to_string(),
            nonce: "n1".to_string(),
            ts: now_unix_secs().to_string(),
            req_sig: "s".to_string(),
        }
    }

    fn revoke_req(reason: &str) -> RevokeCodeRequest {
        RevokeCodeRequest {
            reason: reason.to_string(),
            note: "note note note".to_string(),
            confirm_tail8: "tail1234".to_string(),
            second_approver: None,
        }
    }

    fn reissue_req(prebind: Option<&str>, idem: &str) -> ReissueCodeRequest {
        ReissueCodeRequest {
            prebind: prebind.map(|mc| Prebind {
                machine_code: mc.to_string(),
            }),
            inherit_tier: true,
            inherit_validity: true,
            overrides: None,
            idempotency_key: idem.to_string(),
        }
    }

    // ---------------- G1：预绑定持久化 + 强制 ----------------

    #[test]
    fn t46_prebind_persisted_and_enforced() {
        let svc = build_service();
        // 发放时预绑定 M1。
        let resp = svc
            .issue_codes(&issue_req("t-1", Some("M1"), "g1-issue"))
            .unwrap();
        let code = &resp.codes[0];
        assert_eq!(code.prebind.as_deref(), Some("M1"), "预绑定必须落库");

        // 正确机器激活成功。
        let act = svc.activate(&activate_req(&code.code, "M1")).unwrap();
        assert!(!act.lease_token.is_empty());

        // 错误机器激活 → PrebindConflict::ActivationMachineMismatch。
        let resp2 = svc
            .issue_codes(&issue_req("t-1", Some("M2"), "g1-issue-b"))
            .unwrap();
        let code2 = &resp2.codes[0];
        let err = svc
            .activate(&activate_req(&code2.code, "WRONG"))
            .unwrap_err();
        match err {
            LicenseError::PrebindConflict { kind } => {
                assert_eq!(kind, PrebindKind::ActivationMachineMismatch)
            }
            other => panic!("expected PrebindConflict, got {other:?}"),
        }
    }

    #[test]
    fn t46_empty_prebind_allows_any_machine() {
        let svc = build_service();
        let resp = svc
            .issue_codes(&issue_req("t-1", None, "g1-empty"))
            .unwrap();
        let code = &resp.codes[0];
        assert!(code.prebind.is_none());
        let act = svc
            .activate(&activate_req(&code.code, "ANY-MACHINE"))
            .unwrap();
        assert!(!act.lease_token.is_empty());
    }

    // ---------------- G2：重发幂等（fail-closed） ----------------

    #[test]
    fn t46_reissue_idempotent_by_idempotency_key() {
        let svc = build_service();
        let resp = svc
            .issue_codes(&issue_req("t-1", None, "g2-issue"))
            .unwrap();
        let code_id = resp.codes[0].code_id.clone();
        svc.revoke("t-1", &code_id, &revoke_req("compromised"), "admin")
            .unwrap();

        let r1 = svc
            .reissue("t-1", &code_id, &reissue_req(None, "g2-reissue"), "admin")
            .unwrap();
        let r2 = svc
            .reissue("t-1", &code_id, &reissue_req(None, "g2-reissue"), "admin")
            .unwrap();

        // 同逻辑键 → 同一条重发码（绝不重复签发）。
        assert_eq!(r1.new_code.code_id, r2.new_code.code_id, "重发必须幂等");
        assert_eq!(r1.new_code.reissued_from.as_deref(), Some(code_id.as_str()));

        // 该幂等键下仅存在一条重发码（fail-closed：无重复）。
        let all = svc.store().list_codes_by_batch_key("g2-reissue").unwrap();
        assert_eq!(all.len(), 1, "must not create duplicate reissued codes");

        // 原码单向迁移到 reissued；新码可激活（status = issued）。
        let orig = svc.store().get_code_by_id(&code_id).unwrap().unwrap();
        assert_eq!(orig.status, CodeStatus::Reissued);
        assert_eq!(r1.new_code.status, "issued");
    }

    #[test]
    fn t46_issue_idempotent_by_idempotency_key() {
        let svc = build_service();
        let r1 = svc
            .issue_codes(&issue_req("t-1", None, "g2-issue-dup"))
            .unwrap();
        let r2 = svc
            .issue_codes(&issue_req("t-1", None, "g2-issue-dup"))
            .unwrap();
        assert_eq!(r1.codes.len(), 1);
        assert_eq!(
            r1.codes[0].code_id, r2.codes[0].code_id,
            "发放必须幂等（同键返回首次结果）"
        );
        let stored = svc
            .store()
            .get_code_by_id(&r1.codes[0].code_id)
            .unwrap()
            .unwrap();
        assert_eq!(stored.idempotency_key.as_deref(), Some("g2-issue-dup"));
    }

    // ---------------- G3：预绑定冲突检测 ----------------

    #[test]
    fn t46_prebind_conflict_on_issue_same_tenant() {
        let svc = build_service();
        svc.issue_codes(&issue_req("t-1", Some("M-COLLIDE"), "g3-issue-a"))
            .unwrap();
        // 同租户再发同预绑定 → MachineAlreadyClaimed。
        let err = svc
            .issue_codes(&issue_req("t-1", Some("M-COLLIDE"), "g3-issue-b"))
            .unwrap_err();
        match err {
            LicenseError::PrebindConflict { kind } => {
                assert_eq!(kind, PrebindKind::MachineAlreadyClaimed)
            }
            other => panic!("expected PrebindConflict, got {other:?}"),
        }

        // 跨租户不冲突（预绑定按租户隔离）。
        let t2 = Tenant::new(
            "t-2".to_string(),
            "T2".to_string(),
            "ops".to_string(),
            now_unix_secs(),
        );
        svc.store().insert_tenant(&t2).unwrap();
        let ok = svc
            .issue_codes(&issue_req("t-2", Some("M-COLLIDE"), "g3-issue-c"))
            .unwrap();
        assert_eq!(ok.codes.len(), 1);
    }

    #[test]
    fn t46_prebind_conflict_when_device_already_bound() {
        let svc = build_service();
        // 发放一张无预绑定码并激活到机器 M-BOUND（设备新建 + 码绑定）。
        let resp = svc
            .issue_codes(&issue_req("t-1", None, "g3-issue-d"))
            .unwrap();
        let code = &resp.codes[0];
        svc.activate(&activate_req(&code.code, "M-BOUND")).unwrap();

        // 再发一张预绑定到 M-BOUND 的码 → 该设备已被另一码绑定 → MachineAlreadyBound。
        let err = svc
            .issue_codes(&issue_req("t-1", Some("M-BOUND"), "g3-issue-e"))
            .unwrap_err();
        match err {
            LicenseError::PrebindConflict { kind } => {
                assert_eq!(kind, PrebindKind::MachineAlreadyBound)
            }
            other => panic!("expected PrebindConflict(MachineAlreadyBound), got {other:?}"),
        }
    }

    #[test]
    fn t46_reissue_prebind_conflict() {
        let svc = build_service();
        // 一张已预绑定 M-X 的码（issued）。
        svc.issue_codes(&issue_req("t-1", Some("M-X"), "g3-issue-f"))
            .unwrap();
        // 另一张待重发的原码。
        let resp = svc
            .issue_codes(&issue_req("t-1", None, "g3-issue-g"))
            .unwrap();
        let code_id = resp.codes[0].code_id.clone();
        svc.revoke("t-1", &code_id, &revoke_req("x"), "admin")
            .unwrap();

        // 重发预绑定到 M-X 与已用码冲突 → MachineAlreadyClaimed。
        let err = svc
            .reissue(
                "t-1",
                &code_id,
                &reissue_req(Some("M-X"), "g3-reissue"),
                "admin",
            )
            .unwrap_err();
        match err {
            LicenseError::PrebindConflict { kind } => {
                assert_eq!(kind, PrebindKind::MachineAlreadyClaimed)
            }
            other => panic!("expected PrebindConflict, got {other:?}"),
        }
    }

    // ---------------- G4：空白预绑定视为未提供 ----------------

    #[test]
    fn t46_prebind_whitespace_treated_as_none() {
        let svc = build_service();
        let resp = svc
            .issue_codes(&issue_req("t-1", Some("   "), "g4-issue"))
            .unwrap();
        let stored = svc
            .store()
            .get_code_by_id(&resp.codes[0].code_id)
            .unwrap()
            .unwrap();
        assert!(
            stored.prebind_machine_code.is_none(),
            "空白预绑定必须归一为 None"
        );
        // 任意机器均可激活（无预绑定约束）。
        let act = svc
            .activate(&activate_req(&resp.codes[0].code, "M-ANY"))
            .unwrap();
        assert!(!act.lease_token.is_empty());

        // 重发空白预绑定同样 → None。
        let resp2 = svc
            .issue_codes(&issue_req("t-1", None, "g4-issue-b"))
            .unwrap();
        let cid = resp2.codes[0].code_id.clone();
        svc.revoke("t-1", &cid, &revoke_req("x"), "admin").unwrap();
        let r = svc
            .reissue(
                "t-1",
                &cid,
                &reissue_req(Some(" \t "), "g4-reissue"),
                "admin",
            )
            .unwrap();
        let stored2 = svc
            .store()
            .get_code_by_id(&r.new_code.code_id)
            .unwrap()
            .unwrap();
        assert!(
            stored2.prebind_machine_code.is_none(),
            "重发空白预绑定必须归一为 None"
        );
    }

    // ---------------- G5：幂等键归一（trim） ----------------

    #[test]
    fn t46_idempotency_key_normalized_trim() {
        let svc = build_service();
        let r1 = svc
            .issue_codes(&issue_req("t-1", None, "  key-norm  "))
            .unwrap();
        // 尾部 / 头部空白变体命中同一逻辑键 → 幂等。
        let r2 = svc
            .issue_codes(&issue_req("t-1", None, "key-norm"))
            .unwrap();
        assert_eq!(r1.codes[0].code_id, r2.codes[0].code_id);
        let stored = svc
            .store()
            .get_code_by_id(&r1.codes[0].code_id)
            .unwrap()
            .unwrap();
        assert_eq!(
            stored.idempotency_key.as_deref(),
            Some("key-norm"),
            "存储的幂等键必须被 trim"
        );
    }

    #[test]
    fn t46_reissue_idempotency_key_normalized() {
        let svc = build_service();
        let resp = svc
            .issue_codes(&issue_req("t-1", None, "g5-issue"))
            .unwrap();
        let cid = resp.codes[0].code_id.clone();
        svc.revoke("t-1", &cid, &revoke_req("x"), "admin").unwrap();
        let r1 = svc
            .reissue("t-1", &cid, &reissue_req(None, "  g5-reissue  "), "admin")
            .unwrap();
        let r2 = svc
            .reissue("t-1", &cid, &reissue_req(None, "g5-reissue"), "admin")
            .unwrap();
        assert_eq!(r1.new_code.code_id, r2.new_code.code_id);
        let stored = svc
            .store()
            .get_code_by_id(&r1.new_code.code_id)
            .unwrap()
            .unwrap();
        assert_eq!(stored.idempotency_key.as_deref(), Some("g5-reissue"));
    }

    // ---------------- 其他状态机健全性 ----------------

    #[test]
    fn t46_activate_unknown_code_rejected() {
        let svc = build_service();
        let err = svc
            .activate(&activate_req("IOTDAQ-NOPE-NOPE-NOPE-NOPE", "M1"))
            .unwrap_err();
        assert!(
            matches!(err, LicenseError::ActivationRejected(_)),
            "{err:?}"
        );
    }

    #[test]
    fn t46_reissue_requires_revoked_original() {
        let svc = build_service();
        let resp = svc
            .issue_codes(&issue_req("t-1", None, "g-state-issue"))
            .unwrap();
        let cid = resp.codes[0].code_id.clone();
        // 未废弃直接重发 → KeyStateIllegal。
        let err = svc
            .reissue("t-1", &cid, &reissue_req(None, "g-state-reissue"), "admin")
            .unwrap_err();
        assert!(matches!(err, LicenseError::KeyStateIllegal(_)), "{err:?}");
    }

    #[test]
    fn t46_revoke_already_revoked_is_idempotent() {
        let svc = build_service();
        let resp = svc
            .issue_codes(&issue_req("t-1", None, "g-state-issue-b"))
            .unwrap();
        let cid = resp.codes[0].code_id.clone();
        svc.revoke("t-1", &cid, &revoke_req("first"), "admin")
            .unwrap();
        // 再次废弃：幂等成功（no-op）。
        assert!(svc
            .revoke("t-1", &cid, &revoke_req("second"), "admin")
            .is_ok());
        let stored = svc.store().get_code_by_id(&cid).unwrap().unwrap();
        assert_eq!(stored.status, CodeStatus::Revoked);
    }
}

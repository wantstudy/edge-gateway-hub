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

use crate::admin_auth::{sha256_hex, AdminAccount, Role};
use crate::audit::{BatchOutcome, BatchRecord, ReceiptLedger};
use crate::device_auth;
use crate::error::{LicenseError, LicenseResult, PrebindKind};
use crate::keys::{current_year, KeyRing};
use crate::model::{
    normalize_machine_code, now_ns_id, now_unix_secs, ota_signing_message, ActivationCode,
    ActorType, AuditLog, CodeStatus, Device, DeviceStatus, Heartbeat, HeartbeatResult, Lease,
    LeaseStatus, OtaPackage, OtaStatus, Tenant, VerifyMode, MAX_OTA_PAYLOAD_BYTES,
};
use crate::proto::{
    ActivationRequest, ActivationResponse, AuditReceiptRequest, AuditReceiptResponse, GapKind,
    HeartbeatRequest, HeartbeatResponse, IssueCodesRequest, IssueCodesResponse, IssuedCode,
    OtaManifestResponse, OtaStatusRequest, ReceiptCursor, ReissueCodeRequest, ReissueCodeResponse,
    RevokeCodeRequest, UploadOtaRequest, VerifyRequest, VerifyResponse, OTA_CHANNELS,
};
use crate::receipt;
use crate::store::Store;
use crate::token::{issue_lease_token, LeaseClaims, LeaseToken};
use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine as _;
use sha2::{Digest, Sha256};

/// 心跳周期（小时），随激活响应下发（设计 §1.1）。
const HEARTBEAT_HOURS: i64 = 24;

/// 心跳周期（秒）。
const HEARTBEAT_SECS: i64 = HEARTBEAT_HOURS * 3_600;

/// 激活 nonce 最大长度（daemon 侧为 uuid simple：32 hex；2026-09-25 主理人决策）。
const MAX_NONCE_LEN: usize = 128;

/// 时钟偏移窗口（±5min，设计 §0 / §1.2 / §1.3）。
pub const CLOCK_SKEW_SECS: i64 = 300;

/// nonce 缓存有效期（秒）：取时钟窗的两倍，保证「窗口内 nonce 不可复用」。
pub const NONCE_TTL_SECS: i64 = CLOCK_SKEW_SECS * 2;

// ---- 激活码格式（`IOT-2026-XXXX-XXXX-XXXX-XX`，与网关端 repo.ts 同一口径） ----

/// 激活码字符集：大写字母 + 数字，**剔除易混字符 `0` / `O` / `1` / `I`**
/// （手写抄录 / 电话口述场景最易混淆的三对），共 31 个字符。
const CODE_ALPHABET: &str = "ACDEFGHJKLMNPQRSTUVWXYZ23456789";

/// 激活码分段数：末段为 2 位，其余 3 段各 4 位（`XXXX`）。
const CODE_SEG_COUNT: usize = 4;

/// 非末段宽度（4 位：`XXXX`）。
const CODE_GROUP_WIDTH: usize = 4;

/// 末段宽度（2 位：`XX`）。
const CODE_TAIL_WIDTH: usize = 2;

// ---- 服务端响应签名契约（daemon 侧逐字节镜像，跨端一致性由契约测试锁定） ----
//
// 响应级 `sig` = 当前签发密钥对「域串」的 Ed25519 签名（STANDARD base64）。
// 域串 = `{domain}|{lease_id}|{nonce}|{server_time}`：**业务语义确定性哈希域串**
// （非序列化字节，符合「禁签 Protobuf 序列化字节」红线）；`server_time` 以十进制
// 秒字符串渲染（大整数红线）。
//
// 公钥分发（**TOFU**，2026-09-25 主理人决策）：激活响应携带 `server_pubkey`
// （当前签发密钥公钥，base64）；客户端验签激活响应后钉定该公钥，后续心跳响应
// 一律用钉定公钥验签（异钥 / 缺签 / 篡改 → fail-closed）。

/// 服务端响应签名域：`/activation` 响应（daemon 侧 `RESPONSE_SIG_DOMAIN_ACTIVATION`
/// 逐字节一致）。
pub const RESPONSE_SIG_DOMAIN_ACTIVATION: &str = "activation";

/// 服务端响应签名域：`/heartbeat` 响应（daemon 侧 `RESPONSE_SIG_DOMAIN_HEARTBEAT`
/// 逐字节一致）。
pub const RESPONSE_SIG_DOMAIN_HEARTBEAT: &str = "heartbeat";

/// 渲染服务端响应签名域串（**未哈希**；daemon 侧 `auth::client::render_response_signing_message`
/// 逐字节一致）。
///
/// 格式：`{domain}|{lease_id}|{nonce}|{server_time}`。
#[must_use]
pub fn render_response_signing_message(
    domain: &str,
    lease_id: &str,
    nonce: &str,
    server_time: i64,
) -> String {
    format!("{domain}|{lease_id}|{nonce}|{server_time}")
}

/// 授权服务：聚合 [`Store`] 与 [`KeyRing`]，对外暴露激活码生命周期业务方法。
///
/// 设计为「薄聚合」：所有 SQL 仍集中在 [`Store`]，所有签名集中在 [`KeyRing`] 与
/// [`crate::token`]，本结构体只做业务编排与状态机判定。
pub struct LicensingService {
    /// 仓储层（单连接 + Mutex，事务语义由 store 方法保证）。
    store: Store,
    /// 签名密钥环（Ed25519，私钥经环境变量注入，绝不落库）。
    keyring: KeyRing,
    /// 回执批次账本（task 48：幂等仲裁 + 设备级序号 cursor + 跳空告警）。
    ledger: ReceiptLedger,
}

impl LicensingService {
    /// 构造服务（store 与 keyring 由调用方注入；keyring 须持可用签发密钥才能签发 Lease Token）。
    ///
    /// 回执账本缺省为**内存库**（幂等记忆随进程生命周期）；生产应改用
    /// [`LicensingService::with_ledger`] 注入文件库以跨重启保留幂等记忆。
    pub fn new(store: Store, keyring: KeyRing) -> Self {
        Self::with_ledger(store, keyring, ReceiptLedger::open_in_memory())
    }

    /// 构造服务并注入回执账本（文件库 → 重启后幂等记忆保留）。
    pub fn with_ledger(store: Store, keyring: KeyRing, ledger: ReceiptLedger) -> Self {
        LicensingService {
            store,
            keyring,
            ledger,
        }
    }

    /// 只读访问仓储层（测试与审核用）。
    pub fn store(&self) -> &Store {
        &self.store
    }

    /// 只读访问密钥环（测试与轮换用）。
    pub fn keyring(&self) -> &KeyRing {
        &self.keyring
    }

    /// 只读访问回执账本（测试与运营核查用）。
    pub fn ledger(&self) -> &ReceiptLedger {
        &self.ledger
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
            // fail-closed 保留；独立变体让 admin-console 能给出「请先创建租户」引导。
            return Err(LicenseError::tenant_not_found(format!(
                "unknown tenant: {tenant_id}; create the tenant first via POST /admin/tenants"
            )));
        }

        // 机器码必填（2026-09-27 主理人决策）：发放侧一机一码闭环，
        // `prebind_machine_code` 缺失 / 空白 → 400 `MACHINE_CODE_REQUIRED`。
        let prebind_raw = req.prebind_machine_code.as_deref().unwrap_or("").trim();
        if prebind_raw.is_empty() {
            return Err(LicenseError::machine_code_required(
                "issue requires a non-empty prebind_machine_code",
            ));
        }
        // G5：归一化幂等键（trim / 去尾部空白），空键直接拒绝。
        let idem = self.normalize_idempotency_key(&req.idempotency_key)?;

        // 预绑定取 trim 后值（发放路径不再接受空白——上方已拒绝）。
        let prebind = Some(prebind_raw.to_string());

        // G2（前置回放检查）：同幂等键已有批次 → 直接返回既有结果。
        // 必须先于 G3 冲突检测：机器码必填后，同键重放携带同一预绑定值会命中
        // MachineAlreadyClaimed，若不短路将掩盖「重放返回首次结果」契约。
        let prior_batch = self.store.list_codes_by_batch_key(&idem)?;
        if !prior_batch.is_empty() {
            let codes: Vec<IssuedCode> = prior_batch.iter().map(Self::to_issued_code).collect();
            return Ok(IssueCodesResponse { codes });
        }

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

    /// 设备激活：首激 + 一机一码绑定，以及**已绑定码的同机冲突检测**。
    ///
    /// # 行为次序（对齐 `docs/design/machine-fingerprint.md` §7 与 `licensing-api.md` §1.1）
    ///
    /// 0. **请求鉴权（2026-09-25 主理人决策，先于任何 DB 读取 / §7 判定 / 幂等短路 /
    ///    状态变更，拒绝路径零 DB 写入）**：
    ///    - `ts` 解析（非整数秒 → [`LicenseError::TimestampSkew`]）；
    ///    - nonce 字段白名单（空白 / 超长 → [`LicenseError::FieldWhitelistViolation`]）；
    ///    - **验签**：用请求自带设备公钥验证 `req_sig`
    ///      （[`device_auth::verify_activation_signature`]，对业务语义确定性哈希签名，
    ///      非 Protobuf / JSON 序列化字节）→ [`LicenseError::ActivationSignatureInvalid`]；
    ///    - **±5min 时间窗**（与 A 档 `/verify` 一致，设计 §1.1）→
    ///      [`LicenseError::TimestampSkew`]；
    /// 1. **取码 + 状态守卫（④）**：`revoked` / `reissued` 一律拒绝；
    /// 2. **有效期窗口**：`code.is_valid_at(now)` 不满足 → 拒绝；
    /// 3. **码已绑定（`status = bound`）** → [`Self::activate_against_binding`] 执行 §7 ①②③：
    ///    - ⓪ 请求公钥与设备**已钉定公钥**一致性（不一致 →
    ///      [`LicenseError::ActivationPubkeyMismatch`]，403）；
    ///    - ① `machine_code` 一致 → 同机（幂等恢复 / 重签租约）；
    ///    - ② 不一致但锚点命中 ≥4/5 → 同机（重装 / 漂移）→ 自动改绑 + 重签；
    ///    - ③ 命中 ≤3/5 → **异机** → [`LicenseError::CodeBoundToOtherDevice`]（403），无副作用。
    /// 4. **码未绑定（`issued`）**：
    ///    - **G1 预绑定匹配**（`None` = 任意机器）→ 不匹配 [`PrebindKind::ActivationMachineMismatch`]；
    ///    - 解析 / 创建设备 → 公钥一致性 → **nonce 防重放认领**（全局 `nonce_cache`，
    ///      TTL = 2×时钟窗；同 nonce 重放 → [`LicenseError::NonceReplay`]）→
    ///    - **原子绑定**（`bind_code_to_device`）→ 钉定公钥 → 签发 Lease Token。
    ///
    /// # nonce / 公钥的副作用窗口（显式记录）
    /// nonce 认领与公钥钉定只发生在**成功路径**（该路径的全部拒绝判定均已通过），
    /// 紧邻首个状态变更之前；因此所有拒绝路径（未知码 / 状态守卫 / 预绑定 / §7③ /
    /// 公钥不一致）对 `nonce_cache`、`audit_log`、`activation_code`、`lease` 均**零写入**。
    /// 唯一例外是首激路径上 `resolve_or_create_device` 的设备行创建：它幂等且以
    /// `machine_code` 唯一约束仲裁，重放方只会复用既有设备行，不产生孤儿状态。
    ///
    /// # 顺序说明（相对既有守卫的**唯一**调整）
    /// 预绑定校验从「取码后立即判定」**下沉到未绑定分支**：预绑定只约束「谁可**首次**
    /// 认领该码」；一旦码已绑定，同机判定改由 §7 的锚点 N-of-M 接管。否则一张预绑定码在
    /// 设备漂移（换网卡 / OS 重装 → `machine_code` 改变）后会被**静态预绑定值**挡在 §7 之前，
    /// 误杀「合法运维」路径。该调整**不影响**未绑定码的既有语义（回归见
    /// `t46_prebind_mismatch_still_rejected_as_activation_machine_mismatch`）。
    pub fn activate(&self, req: &ActivationRequest) -> LicenseResult<ActivationResponse> {
        let now = now_unix_secs();

        // ============ 0) 请求鉴权（2026-09-25 主理人决策） ============
        // 0a) ts 解析（JSON 路径 String → i64，大整数红线）。
        let ts = Self::parse_ts(&req.ts)
            .map_err(|_| LicenseError::timestamp_skew("activation ts must be integer seconds"))?;
        // 0b) nonce 字段白名单（空白判空统一 trim；req_sig / device_pubkey 的格式校验
        //     在验签内统一做，避免两处规则漂移）。
        Self::validate_activation_nonce(&req.nonce)?;
        // 0c) **验签**：用请求自带设备公钥验证 req_sig（签名对象为业务语义确定性哈希，
        //     与心跳 / verify / 回执同一纪律——绝不签 Protobuf / JSON 序列化字节）。
        let payload_hash = device_auth::activation_payload_hash(
            req.activation_code.trim(),
            &req.machine_code,
            &req.anchor_hashes,
            &req.device_pubkey,
            &req.nonce,
            ts,
        );
        device_auth::verify_activation_signature(&req.device_pubkey, &payload_hash, &req.req_sig)?;
        // 0d) ±5min 时间窗（与 A 档 /verify 一致，设计 §1.1「401 TIMESTAMP_SKEW（±5min 外）」）。
        if (now - ts).abs() > CLOCK_SKEW_SECS {
            return Err(LicenseError::timestamp_skew(format!(
                "activation ts {ts} outside ±{CLOCK_SKEW_SECS}s of server {now}"
            )));
        }

        let code_value = req.activation_code.trim();
        let code = self
            .store
            .get_code_by_value(code_value)?
            .ok_or_else(|| LicenseError::ActivationRejected("unknown activation code".into()))?;

        let tenant_id = code.tenant_id.clone();

        // ④ 状态守卫：仅 `issued` / `bound` 可进入激活流程；`revoked` / `reissued` 一律拒绝。
        if !matches!(code.status, CodeStatus::Issued | CodeStatus::Bound) {
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

        // §7 一机一码冲突检测：码**已绑定** → 走 ①②③，绝不新建 device / 新租约
        // （步骤 ③ 甚至不得有任何 device / lease / 绑定副作用）。
        if matches!(code.status, CodeStatus::Bound) {
            let bound_device_id = code.bound_device_id.clone().unwrap_or_default();
            if bound_device_id.trim().is_empty() {
                return Err(LicenseError::ActivationRejected(
                    "activation code is bound but has no bound_device_id".into(),
                ));
            }
            return self.activate_against_binding(&code, &bound_device_id, req, now);
        }

        // 未绑定：必须是 `issued` 且未绑定（守住「issued 却带 bound_device_id」的脏数据）。
        if !code.is_activatable() {
            return Err(LicenseError::ActivationRejected(format!(
                "activation code is not activatable (status={})",
                code.status.as_str()
            )));
        }

        // G1：预绑定匹配（仅约束首次认领；绑定后由 §7 判定接管）。
        if !code.prebind_matches(&req.machine_code) {
            return Err(LicenseError::prebind_conflict(
                PrebindKind::ActivationMachineMismatch,
            ));
        }

        // 机器码归一（小写）后再落库 / 入租约：保证 `device.machine_code` 在库内只有
        // 一种表示，与 `store` 的下行迁移、读取侧的 [`normalize_machine_code`] 共同闭合。
        let machine_code = normalize_machine_code(&req.machine_code);
        let device =
            self.resolve_or_create_device(&tenant_id, &machine_code, &req.anchor_hashes, now)?;

        // 公钥一致性：该机器已有设备记录（其他码激活过 / 并发首激）→ 请求公钥必须与
        // 钉定值一致（纯读，无副作用）。
        self.check_pinned_pubkey(&device, &req.device_pubkey)?;
        // nonce 防重放认领（全局表；同 nonce → NonceReplay）——先于绑定（首个状态变更）。
        self.claim_activation_nonce(req, &device.device_id, now)?;
        // 钉定设备激活公钥（first-write-wins；幂等）。
        self.store
            .pin_device_pubkey_if_absent(&device.device_id, &req.device_pubkey)?;

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

        // 服务端响应签名（防篡改；密钥来自 keyring）+ TOFU 公钥下发。
        let sig = self.sign_response(&lease_id, &req.nonce, now)?;
        let server_pubkey = self.signing_pubkey_b64()?;

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
            server_pubkey,
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
            return Err(LicenseError::tenant_not_found(format!(
                "unknown tenant: {tenant_id}; verify the X-Tenant-Id header"
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

        // 高危操作契约校验（设计 §2.2，先于幂等短路——confirm_tail8 是操作者侧的
        // 「我确实要废弃这张码」确认，对已废弃码的重复提交同样强制）：
        // - reason 必填（空白 → REASON_REQUIRED，400）；
        // - note ≥ 10 字符（不足 → BAD_REQUEST）；
        // - confirm_tail8 与码值尾 8 位一致（不符 → CONFIRM_MISMATCH，412）。
        Self::validate_revoke_contract(&code, req)?;

        // G2：已废弃 = 幂等 no-op（fail-closed）。
        if matches!(code.status, CodeStatus::Revoked) {
            return Ok(());
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

    /// 废弃契约校验（设计 §2.2；**结构化错误变体**，绝不 msg.contains）。
    ///
    /// - `reason` 空白 → [`LicenseError::ReasonRequired`]（400 `REASON_REQUIRED`）；
    /// - `note` 少于 10 字符（`trim` 后按 Unicode 字符计数）→
    ///   [`LicenseError::KeyStateIllegal`]（400 `BAD_REQUEST`）；
    /// - `confirm_tail8` 与码值「去分隔符后尾 8 位（大写）」不一致（含空白）→
    ///   [`LicenseError::ConfirmMismatch`]（412 `CONFIRM_MISMATCH`）。
    ///   与 admin-console `tail8Of` 逐字段同构：`code.replace(/[^0-9A-Za-z]/g,'')
    ///   .slice(-8).toUpperCase()`。
    ///
    /// 消息**不含**激活码原文与 confirm 值（错误会进日志与响应体）。
    fn validate_revoke_contract(
        code: &ActivationCode,
        req: &RevokeCodeRequest,
    ) -> LicenseResult<()> {
        if req.reason.trim().is_empty() {
            return Err(LicenseError::reason_required(
                "revoke requires a non-empty reason",
            ));
        }
        if req.note.trim().chars().count() < 10 {
            return Err(LicenseError::KeyStateIllegal(
                "revoke note must be at least 10 characters".into(),
            ));
        }
        let expected_tail8: String = code
            .code
            .chars()
            .filter(char::is_ascii_alphanumeric)
            .rev()
            .take(8)
            .collect::<String>()
            .chars()
            .rev()
            .collect::<String>()
            .to_uppercase();
        let supplied = req.confirm_tail8.trim().to_uppercase();
        if supplied.is_empty() || supplied != expected_tail8 {
            return Err(LicenseError::confirm_mismatch(
                "confirm_tail8 does not match the activation code tail",
            ));
        }
        Ok(())
    }

    // ----------------------------- 管理端：租户自举 / 策略 -----------------------------

    /// 创建租户（`POST /admin/tenants`；新部署自举必需——issue 对不存在租户直接拒绝）。
    ///
    /// - `tenant_id` / `name` 空白 → [`LicenseError::KeyStateIllegal`]（400）；
    /// - 租户已存在 → [`LicenseError::KeyStateIllegal`]（400，幂等键不适用：创建
    ///   请求天然低频且要求显式唯一 ID）；
    /// - `verify_mode` 非法 → 由 [`VerifyMode::parse`] 归一为 400；
    /// - 成功后写审计（actor = 登录用户名）。
    pub fn admin_create_tenant(
        &self,
        tenant_id: &str,
        name: &str,
        contact: &str,
        verify_mode_raw: Option<&str>,
        actor_id: &str,
    ) -> LicenseResult<Tenant> {
        let tenant_id = tenant_id.trim();
        let name = name.trim();
        if tenant_id.is_empty() || name.is_empty() {
            return Err(LicenseError::KeyStateIllegal(
                "create tenant requires non-empty tenant_id and name".into(),
            ));
        }
        if self.store.get_tenant(tenant_id)?.is_some() {
            return Err(LicenseError::KeyStateIllegal(format!(
                "tenant already exists: {tenant_id}"
            )));
        }
        let verify_mode = match verify_mode_raw {
            Some(raw) if !raw.trim().is_empty() => VerifyMode::parse(raw).map_err(|_| {
                LicenseError::KeyStateIllegal(format!("invalid verify_mode_default: {raw}"))
            })?,
            _ => VerifyMode::B,
        };
        let tenant = Tenant {
            tenant_id: tenant_id.to_string(),
            name: name.to_string(),
            verify_mode_default: verify_mode,
            contact: contact.trim().to_string(),
            created_at: now_unix_secs(),
        };
        self.store.insert_tenant(&tenant)?;
        self.audit(
            tenant_id,
            actor_id,
            "tenant_create",
            "tenant",
            tenant_id,
            now_unix_secs(),
        )?;
        Ok(tenant)
    }

    /// 更新租户默认校验档位（`PUT /admin/tenants/:id/policy`）。
    ///
    /// 租户不存在 → [`LicenseError::KeyStateIllegal`]（400）；档位非法 → 400。
    /// 成功后写审计（actor = 登录用户名）。
    pub fn admin_update_tenant_policy(
        &self,
        tenant_id: &str,
        verify_mode_raw: &str,
        actor_id: &str,
    ) -> LicenseResult<()> {
        let verify_mode = VerifyMode::parse(verify_mode_raw).map_err(|_| {
            LicenseError::KeyStateIllegal(format!("invalid verify_mode_default: {verify_mode_raw}"))
        })?;
        self.store.update_tenant_policy(tenant_id, verify_mode)?;
        self.audit(
            tenant_id,
            actor_id,
            "tenant_policy_update",
            "tenant",
            tenant_id,
            now_unix_secs(),
        )?;
        Ok(())
    }

    // ----------------------------- 管理端账号（可配置账号 / 角色） -----------------------------

    /// 列出全部管理员账号（口令摘要**绝不下发**，由 http 层映射为 `AdminAccountItem`）。
    pub fn admin_list_admin_accounts(&self) -> LicenseResult<Vec<AdminAccount>> {
        self.store.list_admin_accounts()
    }

    /// 创建管理员账号（仅 system 可调用；账号唯一、角色规范、口令即刻摘要）。
    pub fn admin_create_admin_account(
        &self,
        account: &str,
        display_name: &str,
        role_raw: &str,
        password: &str,
        actor_id: &str,
    ) -> LicenseResult<AdminAccount> {
        let account = account.trim();
        if account.is_empty() {
            return Err(LicenseError::KeyStateIllegal(
                "create admin account requires non-empty account".into(),
            ));
        }
        if password.is_empty() {
            return Err(LicenseError::KeyStateIllegal(
                "create admin account requires non-empty password".into(),
            ));
        }
        let role = Role::from_str(role_raw.trim())
            .ok_or_else(|| LicenseError::KeyStateIllegal(format!("unknown role: {role_raw}")))?;
        if self.store.get_admin_account(account)?.is_some() {
            return Err(LicenseError::KeyStateIllegal(format!(
                "admin account already exists: {account}"
            )));
        }
        let now = now_unix_secs();
        let acct = AdminAccount {
            account: account.to_string(),
            display_name: display_name.trim().to_string(),
            role: role.as_str().to_string(),
            password_sha256: sha256_hex(password),
            status: "active".to_string(),
            last_login_at: None,
            created_at: now,
            updated_at: now,
        };
        self.store.insert_admin_account(&acct)?;
        self.audit(
            "",
            actor_id,
            "admin_account_create",
            "admin_account",
            account,
            now,
        )?;
        Ok(acct)
    }

    /// 更新管理员账号（显示名 / 角色 / 口令 / 状态；缺省项保持原值）。
    ///
    /// `note` 为危险操作补充说明（可选），非空时写入审计详情（缺省 `""`）。
    #[allow(clippy::too_many_arguments)]
    pub fn admin_update_admin_account(
        &self,
        account: &str,
        display_name: Option<&str>,
        role_raw: Option<&str>,
        password: Option<&str>,
        status_raw: Option<&str>,
        note: &str,
        actor_id: &str,
    ) -> LicenseResult<()> {
        let existing = self.store.get_admin_account(account)?.ok_or_else(|| {
            LicenseError::KeyStateIllegal(format!("admin account not found: {account}"))
        })?;
        let role = match role_raw {
            Some(r) if !r.trim().is_empty() => Role::from_str(r.trim())
                .ok_or_else(|| LicenseError::KeyStateIllegal(format!("unknown role: {r}")))?
                .as_str()
                .to_string(),
            _ => existing.role.clone(),
        };
        let status = match status_raw {
            Some(s) if !s.trim().is_empty() => {
                let s = s.trim();
                if s != "active" && s != "disabled" {
                    return Err(LicenseError::KeyStateIllegal(format!(
                        "invalid account status: {s}"
                    )));
                }
                s.to_string()
            }
            _ => existing.status.clone(),
        };
        let password_sha256 = match password {
            Some(p) if !p.is_empty() => sha256_hex(p),
            _ => existing.password_sha256.clone(),
        };
        let name = match display_name {
            Some(n) => n.trim().to_string(),
            None => existing.display_name.clone(),
        };
        let now = now_unix_secs();
        self.store
            .update_admin_account(account, &name, &role, &password_sha256, &status, now)?;
        self.write_audit(
            ActorType::Admin,
            actor_id,
            "admin_account_update",
            "admin_account",
            account,
            note,
            now,
        )?;
        Ok(())
    }

    /// 删除管理员账号（`note` 写入审计详情）。
    pub fn admin_delete_admin_account(
        &self,
        account: &str,
        note: &str,
        actor_id: &str,
    ) -> LicenseResult<()> {
        let now = now_unix_secs();
        self.store.delete_admin_account(account)?;
        self.write_audit(
            ActorType::Admin,
            actor_id,
            "admin_account_delete",
            "admin_account",
            account,
            note,
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
            return Err(LicenseError::tenant_not_found(format!(
                "unknown tenant: {tenant_id}; verify the X-Tenant-Id header"
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

    // ----------------------------- 心跳（heartbeat） -----------------------------

    /// `POST /heartbeat`：24h 心跳保活（设计 §1.2）。
    ///
    /// # 行为次序（安全红线：**验签先于任何可短路分支**）
    /// 1. 规范化摘要验签（[`device_auth`]，kid 公钥集）；
    /// 2. ±5min 时间窗（超出 → [`LicenseError::TimestampSkew`]）；
    /// 3. 租约存在性（未知 → [`LicenseError::LeaseNotFound`]）+ 废弃判定
    ///    （**废弃语义 = 立即失效** → [`LicenseError::LeaseRevoked`]）；
    /// 4. 全局 nonce 防重放（同 nonce → [`LicenseError::NonceReplay`]）；
    /// 5. 副作用：回写 `lease.last_heartbeat_at` 与 `device.status`、落心跳记录、
    ///    把 `receipt_cursor` 喂入跳空检测游标；
    /// 6. 签发服务端响应签名。
    ///
    /// 心跳本身**可重复**（无害），但当请求携带非空 `nonce` 时，同 nonce 重放被拒——
    /// 与设计 §1.2「幂等语义」一致。
    pub fn heartbeat(&self, req: &HeartbeatRequest) -> LicenseResult<HeartbeatResponse> {
        let now = now_unix_secs();

        // 1) 规范化摘要验签 —— **先于任何可短路分支**（安全红线）。
        let ts = Self::parse_ts(&req.ts)
            .map_err(|_| LicenseError::timestamp_skew("heartbeat ts must be integer seconds"))?;
        let cursor = match &req.receipt_cursor {
            Some(c) => Some(Self::parse_receipt_cursor(c)?),
            None => None,
        };
        let hash = device_auth::heartbeat_payload_hash(&req.lease_id, ts, &req.nonce, cursor);
        let _kid = device_auth::verify_device_signature(&self.keyring, &hash, &req.device_sig)?;

        // 2) ±5min 时间窗。
        if (now - ts).abs() > CLOCK_SKEW_SECS {
            return Err(LicenseError::timestamp_skew(format!(
                "heartbeat ts {ts} outside ±{CLOCK_SKEW_SECS}s of server {now}"
            )));
        }

        // 3) 租约存在性 + 废弃判定。
        let lease = self
            .store
            .get_lease(req.lease_id.trim())?
            .ok_or_else(|| LicenseError::lease_not_found("heartbeat lease_id is unknown"))?;
        if lease.is_revoked() {
            return Err(LicenseError::lease_revoked(
                "heartbeat lease is revoked (immediate invalidation)",
            ));
        }

        // 4) 全局 nonce 防重放（主键唯一约束仲裁）。
        if !self
            .store
            .insert_nonce_if_absent(&req.nonce, &lease.device_id, now + NONCE_TTL_SECS)?
        {
            return Err(LicenseError::nonce_replay("heartbeat nonce already used"));
        }

        // 5) 服务端副作用。
        self.store.update_lease_heartbeat(&lease.lease_id, now)?;
        self.store
            .update_device_status(&lease.device_id, DeviceStatus::Active)?;
        self.store.insert_heartbeat(&Heartbeat {
            id: now_ns_id("hb"),
            lease_id: lease.lease_id.clone(),
            device_id: lease.device_id.clone(),
            client_ts: ts,
            server_ts: now,
            result: HeartbeatResult::Ok,
            receipt_cursor: cursor.map(|(f, t)| format!("{f}-{t}")),
            created_at: now,
        })?;

        // 5b) receipt_cursor 喂入跳空检测游标（心跳携带「最近已确认回执区间」）。
        if let Some((from, to)) = cursor {
            if from <= to {
                let device_mid = self.device_mid_of(&lease.device_id)?;
                // 心跳游标仅用于刷新游标前沿；结果（含告警）由回执端点为权威，
                // 此处不改变心跳响应形状。
                let _ = self.ledger.record_batch(&BatchRecord {
                    device_mid: &device_mid,
                    lease_id: &lease.lease_id,
                    seq_from: u64::try_from(from).unwrap_or(0),
                    seq_to: u64::try_from(to).unwrap_or(0),
                    count: u64::try_from(to.saturating_sub(from).saturating_add(1)).unwrap_or(0),
                    payload_digest: "",
                    ts,
                    accepted_at: now,
                })?;
            }
        }

        // 6) 响应签名 + 组装。
        let next_deadline = now + HEARTBEAT_SECS;
        let sig = self.sign_server_response(
            RESPONSE_SIG_DOMAIN_HEARTBEAT,
            &lease.lease_id,
            &req.nonce,
            now,
        )?;
        Ok(HeartbeatResponse {
            server_time: now.to_string(),
            next_deadline: next_deadline.to_string(),
            valid_until: lease.valid_until.to_string(),
            verify_mode: lease.verify_mode.as_str().to_string(),
            tier: lease.tier.clone(),
            sig,
        })
    }

    // ----------------------------- A 档二次校验（verify） -----------------------------

    /// `POST /verify`：**仅 A 档**的业务消息级服务端二次校验（设计 §1.3）。
    ///
    /// # 校验链（顺序即设计 §1.3，逐级短路）
    /// 1. Ed25519 验签（kid 公钥集，验签对象为**规范化摘要**，非序列化原始字节）
    ///    → [`LicenseError::VerifyFailed`]；
    /// 2. ±5min 时间窗 → [`LicenseError::TimestampSkew`]；
    /// 3. 租约存在 / 废弃 → [`LicenseError::LeaseNotFound`] / [`LicenseError::LeaseRevoked`]；
    /// 4. 全局 nonce 防重放 → [`LicenseError::NonceReplay`]；
    /// 5. 字段白名单 → [`LicenseError::FieldWhitelistViolation`]；
    /// 6. 计量入账（写审计）。
    ///
    /// # 入参
    /// `raw` 为**原始 JSON**：白名单需在链尾对原始对象做「越界字段」判定
    /// （serde 默认忽略未知字段，故必须保留原始视图）。
    pub fn verify(&self, raw: &serde_json::Value) -> LicenseResult<VerifyResponse> {
        let now = now_unix_secs();

        // 反序列化（未知字段被 serde 忽略，白名单在链尾单独校验）。
        let req: VerifyRequest = serde_json::from_value(raw.clone()).map_err(|_| {
            LicenseError::field_whitelist_violation("verify body does not match the field schema")
        })?;

        // 1) Ed25519 验签（kid 公钥集）。
        let ts = Self::parse_ts(&req.ts)
            .map_err(|_| LicenseError::timestamp_skew("verify ts must be integer seconds"))?;
        let hash = device_auth::verify_payload_hash(
            &req.device_mid,
            &req.lease_id,
            &req.payload_digest,
            ts,
            &req.nonce,
        );
        let _kid = device_auth::verify_device_signature(&self.keyring, &hash, &req.device_sig)?;

        // 2) ±5min 时间窗。
        if (now - ts).abs() > CLOCK_SKEW_SECS {
            return Err(LicenseError::timestamp_skew(format!(
                "verify ts {ts} outside ±{CLOCK_SKEW_SECS}s of server {now}"
            )));
        }

        // 3) 租约存在 / 废弃。
        let lease = self
            .store
            .get_lease(req.lease_id.trim())?
            .ok_or_else(|| LicenseError::lease_not_found("verify lease_id is unknown"))?;
        if lease.is_revoked() {
            return Err(LicenseError::lease_revoked(
                "verify lease is revoked (immediate invalidation)",
            ));
        }

        // 4) 全局 nonce 防重放。
        if !self
            .store
            .insert_nonce_if_absent(&req.nonce, &lease.device_id, now + NONCE_TTL_SECS)?
        {
            return Err(LicenseError::nonce_replay("verify nonce already used"));
        }

        // 5) 字段白名单（越界字段 → 422）。
        VerifyRequest::validate_whitelist_value(raw)?;
        req.validate_whitelist()?;

        // 6) 计量入账（A 档：每条业务消息验签 → 计入审计）。
        self.write_audit(
            ActorType::Device,
            &req.device_mid,
            "verify",
            "lease",
            &lease.lease_id,
            &format!("payload_digest={}", req.payload_digest),
            now,
        )?;

        Ok(VerifyResponse {
            ok: true,
            server_time: now.to_string(),
            nonce: req.nonce.clone(),
        })
    }

    // ----------------------------- B 档审计回执（audit/receipt） -----------------------------

    /// `POST /audit/receipt`：B 档审计回执（设计 §1.4）。
    ///
    /// # 行为次序（**验签 + 白名单必须先于幂等短路**，安全红线 1）
    /// 1. 原始 JSON 白名单（越界业务字段 → 422，整单拒收 + 记审计）；
    /// 2. 结构自检（必填字段非空白）+ 数值字段解析 + 区间方向；
    /// 3. ±5min 时间窗；
    /// 4. **验签**（[`receipt::verify_receipt_signature`]）；
    /// 5. 租约存在 / 废弃；
    /// 6. 幂等落账 + 跳空 / 回退 / 缺失判定（[`crate::audit::ReceiptLedger`]）。
    ///
    /// # 幂等 / 补报
    /// 相同区间重复上报 → 批次键命中 → **幂等接受（去重，不重复告警）**；
    /// 断网延迟补报（`now` 远晚于 `ts`）按 `ts` 排序评估（`ts` 仍须在 ±5min 窗内——
    /// 该窗由客户端签名时间保障，见设计 §1.4）。
    ///
    /// # `missing` 判定
    /// 请求级响应只能产出 `none` / `gap` / `overlap`；`missing`（**窗口内无回执**）
    /// 在此定义为「该设备**无任何已确认回执**（前沿 0）且起始序号 > 1」——
    /// 即前缀区间 `[1, seq_from-1]` 从未上报。全局性缺失由后续补报收敛。
    pub fn audit_receipt(&self, raw: &serde_json::Value) -> LicenseResult<AuditReceiptResponse> {
        let now = now_unix_secs();

        // 1) 原始 JSON 白名单：越界字段 → 整单拒收 + 记审计。
        if let Err(e) = AuditReceiptRequest::validate_whitelist_value(raw) {
            self.write_audit(
                ActorType::Device,
                "",
                "audit_receipt_whitelist_violation",
                "audit_receipt",
                &Self::entity_hint(raw),
                &e.to_string(),
                now,
            )?;
            return Err(e);
        }

        // 2) 反序列化 + 结构自检 + 数值字段解析。
        let req: AuditReceiptRequest = serde_json::from_value(raw.clone()).map_err(|_| {
            LicenseError::field_whitelist_violation(
                "audit receipt body does not match the field schema",
            )
        })?;
        req.validate_whitelist()?;

        let (seq_from, seq_to, count, ts) =
            receipt::parse_receipt_ints(&req.seq_from, &req.seq_to, &req.count, &req.ts).map_err(
                |_| {
                    LicenseError::field_whitelist_violation(
                        "audit receipt numeric field is not a valid integer",
                    )
                },
            )?;
        if seq_from < 0 || seq_to < 0 || count < 0 {
            return Err(LicenseError::field_whitelist_violation(
                "audit receipt seq_from / seq_to / count must be non-negative",
            ));
        }
        if seq_from > seq_to {
            return Err(LicenseError::field_whitelist_violation(
                "audit receipt seq_from must not exceed seq_to",
            ));
        }

        // 3) ±5min 时间窗。
        if (now - ts).abs() > CLOCK_SKEW_SECS {
            return Err(LicenseError::timestamp_skew(format!(
                "audit receipt ts {ts} outside ±{CLOCK_SKEW_SECS}s of server {now}"
            )));
        }

        // 4) **验签 —— 必须先于幂等短路**（否则伪造回执可借「同区间」路径蒙过验签）。
        let verified = receipt::verify_receipt_signature(&self.keyring, &req, now, CLOCK_SKEW_SECS)
            .map_err(|e| LicenseError::verify_failed(e.to_string()))?;

        // 5) 租约存在 / 废弃。
        let lease = self
            .store
            .get_lease(verified.lease_id.trim())?
            .ok_or_else(|| LicenseError::lease_not_found("audit receipt lease_id is unknown"))?;
        if lease.is_revoked() {
            return Err(LicenseError::lease_revoked(
                "audit receipt lease is revoked (immediate invalidation)",
            ));
        }

        // 6) 幂等落账 + 跳空 / 回退 / 缺失判定。
        let outcome = self.ledger.record_batch(&BatchRecord {
            device_mid: &verified.device_mid,
            lease_id: &verified.lease_id,
            seq_from: u64::try_from(verified.seq_from).unwrap_or(0),
            seq_to: u64::try_from(verified.seq_to).unwrap_or(0),
            count: u64::try_from(verified.count).unwrap_or(0),
            payload_digest: &verified.payload_digest,
            ts: verified.ts,
            accepted_at: now,
        })?;

        let (gap, warnings) = match outcome {
            // 幂等重放：不重复告警。
            BatchOutcome::Replay => (GapKind::None, Vec::new()),
            BatchOutcome::Recorded {
                gap,
                warnings,
                last_seq_to,
            } => {
                // 无历史前沿（last_seq_to == 0）且起始 > 1 → 前缀区间缺失 = missing。
                let normalized = if last_seq_to == 0 && seq_from > 1 {
                    GapKind::Missing
                } else {
                    gap
                };
                (normalized, warnings)
            }
        };

        // 异常（gap / overlap / missing）写审计 —— **人工核实、不自动封禁**。
        if gap.is_anomaly() {
            self.write_audit(
                ActorType::System,
                "system",
                "audit_receipt_anomaly",
                "audit_receipt",
                &verified.lease_id,
                &format!("gap={} device_mid={}", gap.as_str(), verified.device_mid),
                now,
            )?;
        }

        Ok(AuditReceiptResponse {
            accepted: true,
            gap,
            server_time: now.to_string(),
            warnings,
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

    /// nonce 字段白名单：非空白、长度 ≤ [`MAX_NONCE_LEN`]（daemon 侧为 uuid simple
    /// 32 hex；上限留 4 倍余量，防超长串写库 / 撑爆 nonce_cache，2026-09-25 主理人决策）。
    fn validate_activation_nonce(nonce: &str) -> LicenseResult<()> {
        if nonce.trim().is_empty() {
            return Err(LicenseError::field_whitelist_violation(
                "activation nonce must not be empty",
            ));
        }
        if nonce.len() > MAX_NONCE_LEN {
            return Err(LicenseError::field_whitelist_violation(
                "activation nonce exceeds the 128-byte limit",
            ));
        }
        Ok(())
    }

    /// 校验请求设备公钥与库中**已钉定**公钥一致（只读，无副作用）。
    ///
    /// 「钉定」语义（2026-09-25 主理人决策）：设备首次激活成功时把请求公钥
    /// first-write-wins 写入 `device.device_pubkey`（历史设备列为 NULL，不阻断，
    /// 由成功路径补钉定）；此后任何激活请求（同码重激活 / 漂移改绑 / 换码同机）
    /// 必须携带同一公钥，不一致 → [`LicenseError::ActivationPubkeyMismatch`]（403）。
    fn check_pinned_pubkey(&self, device: &Device, req_pubkey: &str) -> LicenseResult<()> {
        if let Some(pinned) = self.store.get_device_pubkey(&device.device_id)? {
            if pinned != req_pubkey {
                return Err(LicenseError::activation_pubkey_mismatch(
                    "activation device pubkey does not match the pinned pubkey of this device",
                ));
            }
        }
        Ok(())
    }

    /// 认领激活 nonce（全局防重放仲裁；同 nonce 二次提交 → [`LicenseError::NonceReplay`]）。
    ///
    /// - TTL 与心跳 / verify 一致（[`NONCE_TTL_SECS`] = 2×时钟窗，窗口内 nonce 不可复用）；
    /// - 认领前顺带清理过期 nonce（`idx_nonce_expires` 索引，代价可控；
    ///   参照库内既有清理写法 `Store::purge_expired_nonces`）；
    /// - daemon 侧重试语义：激活失败重试会换新 nonce / ts，故同请求不做幂等保留；
    /// - **仅在成功路径调用**（全部拒绝判定之后、首个状态变更之前），拒绝路径零写入。
    fn claim_activation_nonce(
        &self,
        req: &ActivationRequest,
        device_id: &str,
        now: i64,
    ) -> LicenseResult<()> {
        self.store.purge_expired_nonces(now)?;
        if !self
            .store
            .insert_nonce_if_absent(&req.nonce, device_id, now + NONCE_TTL_SECS)?
        {
            return Err(LicenseError::nonce_replay("activation nonce already used"));
        }
        Ok(())
    }

    /// 解析心跳 `receipt_cursor` 的序号区间（String → `i64`）。
    ///
    /// 畸形（非整数）→ [`LicenseError::FieldWhitelistViolation`]（字段不合规，HTTP 422）。
    fn parse_receipt_cursor(cursor: &ReceiptCursor) -> LicenseResult<(i64, i64)> {
        let from = cursor.seq_from.trim().parse::<i64>().map_err(|_| {
            LicenseError::field_whitelist_violation("receipt_cursor.seq_from is not an integer")
        })?;
        let to = cursor.seq_to.trim().parse::<i64>().map_err(|_| {
            LicenseError::field_whitelist_violation("receipt_cursor.seq_to is not an integer")
        })?;
        Ok((from, to))
    }

    /// 由 `device_id` 反查设备机器码（`device_mid`）；设备不存在时回退为 `device_id`。
    fn device_mid_of(&self, device_id: &str) -> LicenseResult<String> {
        Ok(match self.store.get_device(device_id)? {
            Some(device) => device.machine_code,
            None => device_id.to_string(),
        })
    }

    /// 从原始 JSON 中提取 `lease_id`（审计实体提示；缺失返回空串）。
    fn entity_hint(raw: &serde_json::Value) -> String {
        raw.get("lease_id")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string()
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

    /// 生成激活码值（`IOT-2026-XXXX-XXXX-XXXX-XX`）。
    ///
    /// 结构为 `IOT-` + 签发当年（[`keys::current_year`]，非硬编码）+ 3 段各 4 位，
    /// 再加末段 2 位，共 14 位 payload；字符集 [`CODE_ALPHABET`] 剔除易混的
    /// `0/O/1/I`（抄写 / 口述场景），与网关端 `repo.ts` 入口校验同一口径。
    ///
    /// 年份取签发时刻而非硬编码常量；熵来自 `rand`，码值不进日志 / 不进错误信息；
    /// 唯一性由存储层 `code_value` 唯一约束兜底（`Store::insert_code` 冲突即 storage
    /// error，不做静默重试）。
    fn generate_code_value() -> String {
        use rand::Rng as _;
        let mut rng = rand::rng();
        let groups: Vec<String> = (0..4)
            .map(|seg| {
                let width = if seg == CODE_SEG_COUNT - 1 {
                    CODE_TAIL_WIDTH
                } else {
                    CODE_GROUP_WIDTH
                };
                let mut group = String::with_capacity(width);
                for _ in 0..width {
                    let idx = rng.random_range(0..CODE_ALPHABET.len());
                    group.push(CODE_ALPHABET.as_bytes()[idx] as char);
                }
                group
            })
            .collect();
        format!(
            "IOT-{:04}-{}",
            current_year(now_unix_secs()),
            groups.join("-")
        )
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
    ///
    /// **入参先归一**（[`normalize_machine_code`]）：库内值已由 `store` 的下行迁移
    /// 统一为小写，若查询入参仍是发放端提交的大写原值，`WHERE ... = ?1` 会**静默落空**，
    /// 使「同租户同机器码已发过码」的冲突检测被绕过（一机一码破口）。
    /// 归一责任在**本层**，不在 store —— 见 [`Store::find_code_by_prebind`] 的注释。
    fn check_prebind_conflict(&self, tenant_id: &str, machine_code: &str) -> LicenseResult<()> {
        let machine_code = normalize_machine_code(machine_code);
        if let Some(existing) = self.store.find_code_by_prebind(&machine_code)? {
            if existing.tenant_id == tenant_id {
                return Err(LicenseError::prebind_conflict(
                    PrebindKind::MachineAlreadyClaimed,
                ));
            }
        }
        if let Some(device) = self.store.get_device_by_machine_code(&machine_code)? {
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

    /// §7 判定：对**已绑定**码执行 ①②③（同机幂等恢复 / 漂移自动改绑 / 异机拒绝）。
    ///
    /// 阈值比较**只**经 [`Device::is_same_machine_by_anchors`]（单一来源），
    /// 本函数不写 `>= 4` 字面量。
    fn activate_against_binding(
        &self,
        code: &ActivationCode,
        bound_device_id: &str,
        req: &ActivationRequest,
        now: i64,
    ) -> LicenseResult<ActivationResponse> {
        let device = self.store.get_device(bound_device_id)?.ok_or_else(|| {
            // 绑定关系指向不存在的设备 = 库不一致（不应由请求触发），如实报内部错误。
            LicenseError::Storage(format!(
                "activation code {} is bound to a device that no longer exists",
                code.code_id
            ))
        })?;

        // ⓪ 公钥钉定一致性（先于 §7 分支；纯读，无副作用；2026-09-25 主理人决策）。
        self.check_pinned_pubkey(&device, &req.device_pubkey)?;

        // ① 上报 machine_code 与绑定记录一致 → 同机（幂等恢复 / 重签租约）。
        //    两侧归一（大小写 + 空白）：裸 `==` 会把「同机、仅大小写不同」误判为
        //    需要走锚点 N-of-M，进而可能落到步骤 ③ 把合法运维拒成异机。
        if normalize_machine_code(&req.machine_code) == normalize_machine_code(&device.machine_code)
        {
            return self.activate_same_machine(code, &device, req, now);
        }

        // ②/③ machine_code 不一致 → 按**锚点命中数**判定（设计 §3：≥4/5 同机、≤3/5 异机）。
        let hits = device.anchor_match_count(&req.anchor_hashes);
        let drifts = device.anchor_drift_count(&req.anchor_hashes);
        if device.is_same_machine_by_anchors(&req.anchor_hashes) {
            // ② 同机（重装 / 漂移）：自动改绑 + 作废旧租约 + 重签新租约（同一事务）+ 审计留痕。
            self.auto_rebind_same_machine(code, &device, req, hits, drifts, now)
        } else {
            // ③ 异机：拒绝 403（仅写审计，**无任何 device / lease / 绑定副作用**）。
            self.write_audit(
                ActorType::System,
                "system",
                "activation_rejected_bound_to_other_device",
                "activation_code",
                &code.code_id,
                &format!("hits={hits} drifts={drifts}"),
                now,
            )?;
            Err(LicenseError::code_bound_to_other_device())
        }
    }

    /// §7 步骤 ①：**同机**（`machine_code` 一致）→ 幂等恢复 / 重签租约。
    ///
    /// 幂等语义（`licensing-api.md` §1.1「返回现存有效租约」）：若该设备存在**仍可用**的
    /// 租约，直接返回它，**不新建 device、不追加租约行**；无可用租约时复用既有 device 重签一张。
    /// `Lease Token` 由既有租约载荷**确定性**重签（Ed25519 确定性 → 载荷不变则 Token 逐字节不变）。
    fn activate_same_machine(
        &self,
        code: &ActivationCode,
        device: &Device,
        req: &ActivationRequest,
        now: i64,
    ) -> LicenseResult<ActivationResponse> {
        // nonce 防重放 + 公钥钉定：幂等返回路径同样要求**新 nonce**（重放一律拒绝；
        // daemon 重试会换新 nonce / ts，同请求不做幂等保留，2026-09-25 主理人决策）。
        self.claim_activation_nonce(req, &device.device_id, now)?;
        self.store
            .pin_device_pubkey_if_absent(&device.device_id, &req.device_pubkey)?;

        if let Some(lease) = self.find_usable_lease(&device.device_id, &code.code_id, now)? {
            // 幂等：返回现存有效租约（同码同机重复激活，如凭证丢失重装）。
            let claims = Self::lease_claims(&lease, &device.machine_code);
            let token = issue_lease_token(&self.keyring, &claims)?;
            return self.activation_response(&lease, token, req, now);
        }

        // 无可用租约 → 复用既有 device **重签**一张（绝不新建设备）。
        let lease_id = now_ns_id("lease");
        let claims = LeaseClaims {
            lease_id: lease_id.clone(),
            device_id: device.device_id.clone(),
            mid: device.machine_code.clone(),
            tier: code.tier.clone(),
            verify_mode: VerifyMode::B.as_str().to_string(),
            issued_at: now,
            valid_until: code.valid_until,
        };
        let token = issue_lease_token(&self.keyring, &claims)?;
        let lease = Lease {
            lease_id,
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
        self.write_audit(
            ActorType::System,
            "system",
            "activation_same_machine",
            "activation_code",
            &code.code_id,
            "same-machine re-activation: lease re-signed",
            now,
        )?;
        self.activation_response(&lease, token, req, now)
    }

    /// §7 步骤 ②：**同机（重装 / 漂移）** → 改绑 + 作废旧租约 + 重签新租约（同一事务）+ 审计。
    ///
    /// 命中 ≥4/5 说明锚点高度重合，判同机；把绑定记录前移到当前机器
    /// （`machine_code` + `anchor_hashes` 一起刷新），作废旧租约、签发新租约。全程单事务。
    fn auto_rebind_same_machine(
        &self,
        code: &ActivationCode,
        device: &Device,
        req: &ActivationRequest,
        hits: usize,
        drifts: usize,
        now: i64,
    ) -> LicenseResult<ActivationResponse> {
        // 与首激路径（[`Self::activate`]）同一归一口径：改绑写入库内的机器码恒为小写。
        let new_machine_code = normalize_machine_code(&req.machine_code);

        // nonce 防重放 + 公钥钉定（改绑事务之前认领；重放 → NonceReplay，零状态变更）。
        self.claim_activation_nonce(req, &device.device_id, now)?;
        self.store
            .pin_device_pubkey_if_absent(&device.device_id, &req.device_pubkey)?;

        let lease_id = now_ns_id("lease");
        let claims = LeaseClaims {
            lease_id: lease_id.clone(),
            device_id: device.device_id.clone(),
            mid: new_machine_code.clone(),
            tier: code.tier.clone(),
            verify_mode: VerifyMode::B.as_str().to_string(),
            issued_at: now,
            valid_until: code.valid_until,
        };
        let token = issue_lease_token(&self.keyring, &claims)?;
        let new_lease = Lease {
            lease_id,
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

        // 审计留痕：命中项数 / 漂移项数（**绝不写锚点原文 / `machine_code` / 码值**）。
        let audit = AuditLog {
            id: now_ns_id("audit"),
            actor_type: ActorType::System,
            actor_id: "system".to_string(),
            action: "activation_rebind".to_string(),
            entity_type: "activation_code".to_string(),
            entity_id: code.code_id.clone(),
            detail: format!(
                "auto_rebind device_id={} hits={hits} drifts={drifts}",
                device.device_id
            ),
            ts: now,
            ip: String::new(),
        };

        // 改绑 + 作废旧租约 + 写入新租约 + 审计 —— **单事务**，避免中间态。
        self.store.rebind_device_and_issue_lease(
            &device.device_id,
            &new_machine_code,
            &req.anchor_hashes,
            &new_lease,
            &audit,
        )?;

        self.activation_response(&new_lease, token, req, now)
    }

    /// 该设备在给定码下**仍可用**的租约（`list_leases_by_device` 已按最新在前排序）。
    fn find_usable_lease(
        &self,
        device_id: &str,
        code_id: &str,
        now: i64,
    ) -> LicenseResult<Option<Lease>> {
        for lease in self.store.list_leases_by_device(device_id)? {
            if lease.code_id == code_id && lease.is_usable_at(now) {
                return Ok(Some(lease));
            }
        }
        Ok(None)
    }

    /// 由既有租约构造 Lease Token 载荷（`mid` 由调用方给出，其余取自租约记录）。
    fn lease_claims(lease: &Lease, mid: &str) -> LeaseClaims {
        LeaseClaims {
            lease_id: lease.lease_id.clone(),
            device_id: lease.device_id.clone(),
            mid: mid.to_string(),
            tier: lease.tier.clone(),
            verify_mode: lease.verify_mode.as_str().to_string(),
            issued_at: lease.issued_at,
            valid_until: lease.valid_until,
        }
    }

    /// 组装激活响应：签发租约 Token（载荷取自既有租约，**确定性**）+ 服务端响应签名 + TOFU 公钥。
    fn activation_response(
        &self,
        lease: &Lease,
        token: LeaseToken,
        req: &ActivationRequest,
        now: i64,
    ) -> LicenseResult<ActivationResponse> {
        let sig = self.sign_response(&lease.lease_id, &req.nonce, now)?;
        let server_pubkey = self.signing_pubkey_b64()?;
        Ok(ActivationResponse {
            lease_id: lease.lease_id.clone(),
            lease_token: token.encode(),
            verify_mode: lease.verify_mode.as_str().to_string(),
            tier: lease.tier.clone(),
            valid_until: lease.valid_until.to_string(),
            heartbeat_hours: HEARTBEAT_HOURS,
            server_time: now.to_string(),
            nonce: req.nonce.clone(),
            server_pubkey,
            sig,
        })
    }

    /// 当前**签发**密钥的公钥（STANDARD base64；激活响应 TOFU 下发用）。
    ///
    /// # Errors
    /// 无可用签发密钥（`signing_kid` 为空 / 密钥环不一致）→ [`LicenseError::TokenInvalid`]。
    fn signing_pubkey_b64(&self) -> LicenseResult<String> {
        let kid = self
            .keyring
            .signing_kid()
            .ok_or_else(|| LicenseError::TokenInvalid("no active signing key available".into()))?;
        let entry = self.keyring.get(&kid).ok_or_else(|| {
            LicenseError::TokenInvalid(format!("signing kid vanished from keyring: {kid}"))
        })?;
        Ok(entry.public_key_b64().to_string())
    }

    /// 服务端对激活响应的签名（防篡改；密钥来自密钥环）。
    fn sign_response(
        &self,
        lease_id: &str,
        nonce: &str,
        server_time: i64,
    ) -> LicenseResult<String> {
        self.sign_server_response(RESPONSE_SIG_DOMAIN_ACTIVATION, lease_id, nonce, server_time)
    }

    /// 通用服务端响应签名：`{domain}|{lease_id}|{nonce}|{server_time}`（密钥来自密钥环）。
    ///
    /// 域前缀区分端点（[`RESPONSE_SIG_DOMAIN_ACTIVATION`] / [`RESPONSE_SIG_DOMAIN_HEARTBEAT`]），
    /// 防止跨端点签名混用；域串渲染统一走 [`render_response_signing_message`]（daemon 侧逐字节镜像）。
    fn sign_server_response(
        &self,
        domain: &str,
        lease_id: &str,
        nonce: &str,
        server_time: i64,
    ) -> LicenseResult<String> {
        let message = render_response_signing_message(domain, lease_id, nonce, server_time);
        let (_kid, sig) = self.keyring.sign(message.as_bytes())?;
        Ok(sig)
    }

    // ------------------------- 管理端：OTA 升级包（发布物仓库） -------------------------

    /// 列出升级包（`GET /admin/updates`；`status` 为 `None` / 空白 = 不过滤）。
    ///
    /// 排序由 [`crate::store::Store::list_ota_packages`] 保证：版本号**数值**降序
    /// （不是字典序，否则 `"9"` 会排在 `"10"` 之后）。
    pub fn admin_list_ota_packages(&self, status: Option<&str>) -> LicenseResult<Vec<OtaPackage>> {
        let filter = match status.map(str::trim).filter(|s| !s.is_empty()) {
            Some(raw) => Some(OtaStatus::parse(raw).ok_or_else(|| {
                LicenseError::KeyStateIllegal(format!(
                    "unknown ota status filter: {raw}; allowed: draft | published | disabled | revoked"
                ))
            })?),
            None => None,
        };
        self.store.list_ota_packages_by_status(filter)
    }

    /// 上传升级包（`POST /admin/updates`；仅 system）：解码 → 算 SHA-256 → 用密钥环
    /// 当前签发密钥对 **OTA 域消息**签名 → 落库为 `draft`。
    ///
    /// ## 签名域（两端打通的关键）
    /// 待签消息 = [`crate::model::ota_signing_message`]，与
    /// `crates/daemon/src/ota.rs:119` **逐字节同构**：
    /// `iotdaq.ota.v1|ver=<len>:<dec>|sha256=<len>:<hex>|`。
    /// `payload_sha256` 一律**小写** hex（网关 `parse_manifest` 会 `to_ascii_lowercase()`，
    /// 本侧必须与签名对象一致，否则验签必然失败）。
    ///
    /// ## 危险操作四要素
    /// `reason` / `note_detail` / `confirm` 三个**彼此独立**的字段（详见
    /// [`Self::validate_ota_dangerous_contract`]）；`note`（发布说明）是业务字段，
    /// **不参与**四要素校验。
    ///
    /// # Errors
    /// - 版本号非法 / 通道非法 / payload 非 base64 / 超大 → [`LicenseError::KeyStateIllegal`]（400）；
    /// - 四要素不符 → `ReasonRequired`（400）/ `ConfirmMismatch`（412）；
    /// - 同 `(version, channel)` 已存在 → `KeyStateIllegal`（400，**不做 upsert**）；
    /// - 无可用签发密钥 → [`LicenseError::TokenInvalid`]。
    pub fn admin_upload_ota(
        &self,
        req: &UploadOtaRequest,
        actor_id: &str,
    ) -> LicenseResult<OtaPackage> {
        let version = Self::validate_ota_version(&req.version)?;
        let channel = Self::validate_ota_channel(&req.channel)?;
        Self::validate_ota_dangerous_contract(
            &req.reason,
            req.note_detail.as_deref().unwrap_or(""),
            &req.confirm,
            &version,
        )?;

        let payload_b64 = req.payload_b64.trim();
        if payload_b64.is_empty() {
            return Err(LicenseError::KeyStateIllegal(
                "upload ota requires a non-empty payload_b64".into(),
            ));
        }
        let payload = B64.decode(payload_b64.as_bytes()).map_err(|e| {
            LicenseError::KeyStateIllegal(format!("payload_b64 is not valid base64: {e}"))
        })?;
        if payload.is_empty() {
            return Err(LicenseError::KeyStateIllegal(
                "upload ota payload must not be empty".into(),
            ));
        }
        if payload.len() > MAX_OTA_PAYLOAD_BYTES {
            return Err(LicenseError::KeyStateIllegal(format!(
                "ota payload is too large: {} bytes exceeds the {} byte limit",
                payload.len(),
                MAX_OTA_PAYLOAD_BYTES
            )));
        }

        if self.store.get_ota_package(&version, &channel)?.is_some() {
            return Err(LicenseError::KeyStateIllegal(format!(
                "ota package already exists: version {version} on channel {channel}; \
                 upload a different version instead of overwriting a published artifact"
            )));
        }

        let payload_sha256 = Self::sha256_lower_hex(&payload);
        let (kid, sig_b64) = self
            .keyring
            .sign(&ota_signing_message(&version, &payload_sha256))?;

        let pkg = OtaPackage {
            version: version.clone(),
            channel,
            size: payload.len().to_string(),
            payload_sha256,
            payload_b64: payload_b64.to_string(),
            sig_b64,
            kid,
            status: OtaStatus::Draft,
            published_at: String::new(),
            published_by: String::new(),
            note: req.note.trim().to_string(),
        };
        self.store.insert_ota_package(&pkg)?;
        let now = now_unix_secs();
        self.write_audit(
            ActorType::Admin,
            actor_id,
            "ota_upload",
            "ota_package",
            &format!("{}:{}", pkg.version, pkg.channel),
            &Self::ota_audit_detail(&req.reason, req.note_detail.as_deref().unwrap_or("")),
            now,
        )?;
        Ok(pkg)
    }

    /// 发布升级包（`POST /admin/updates/:version/publish`；仅 system）：`draft | disabled
    /// → published`。
    ///
    /// 只有 `published` 包会被 [`Self::ota_manifest`] 下发给网关——上传即发布会让
    /// 「未过审的包」直接触达现场设备，故上传后必须显式发布。
    ///
    /// # Errors
    /// 包不存在 → `KeyStateIllegal`（400）；`revoked` 为终态，不可再发布。
    pub fn admin_publish_ota(
        &self,
        version_raw: &str,
        req: &OtaStatusRequest,
        actor_id: &str,
    ) -> LicenseResult<OtaPackage> {
        self.set_ota_status(version_raw, req, actor_id, OtaStatus::Published)
    }

    /// 停用升级包（`POST /admin/updates/:version/disable`；仅 system）：`published → disabled`。
    ///
    /// 停用是**可逆**的下架（与 `revoked` 终态不同）：包字节保留，可重新发布。
    ///
    /// # Errors
    /// 包不存在 / 当前状态不是 `published` → `KeyStateIllegal`（400）。
    pub fn admin_disable_ota(
        &self,
        version_raw: &str,
        req: &OtaStatusRequest,
        actor_id: &str,
    ) -> LicenseResult<OtaPackage> {
        self.set_ota_status(version_raw, req, actor_id, OtaStatus::Disabled)
    }

    /// 发布 / 停用共用路径（状态机收口在一处，避免两条分支各写一套校验而分叉）。
    fn set_ota_status(
        &self,
        version_raw: &str,
        req: &OtaStatusRequest,
        actor_id: &str,
        target: OtaStatus,
    ) -> LicenseResult<OtaPackage> {
        let version = Self::validate_ota_version(version_raw)?;
        let channel = Self::validate_ota_channel(&req.channel)?;
        Self::validate_ota_dangerous_contract(&req.reason, &req.note, &req.confirm, &version)?;

        let existing = self
            .store
            .get_ota_package(&version, &channel)?
            .ok_or_else(|| {
                LicenseError::KeyStateIllegal(format!(
                    "ota package not found: version {version} on channel {channel}"
                ))
            })?;
        match (existing.status, target) {
            // 幂等：已经是目标状态直接返回当前包（重复点击不报错、不重复入审计环语义）。
            (s, t) if s == t => return Ok(existing),
            (OtaStatus::Revoked, _) => {
                return Err(LicenseError::KeyStateIllegal(format!(
                    "ota package version {version} on channel {channel} is revoked and cannot be {target}",
                    target = target.as_str()
                )));
            }
            (_, OtaStatus::Published) => {}
            (OtaStatus::Published, OtaStatus::Disabled) => {}
            (from, _) => {
                return Err(LicenseError::KeyStateIllegal(format!(
                    "cannot set ota package status from {} to {}",
                    from.as_str(),
                    target.as_str()
                )));
            }
        }

        let now = now_unix_secs();
        let published_at = match target {
            OtaStatus::Published => now.to_string(),
            _ => existing.published_at.clone(),
        };
        let published_by = match target {
            OtaStatus::Published => actor_id.to_string(),
            _ => existing.published_by.clone(),
        };
        let changed = self.store.update_ota_status(
            &version,
            &channel,
            target,
            &published_at,
            &published_by,
        )?;
        if !changed {
            return Err(LicenseError::Storage(
                "ota package status update affected no rows".into(),
            ));
        }
        self.write_audit(
            ActorType::Admin,
            actor_id,
            match target {
                OtaStatus::Published => "ota_publish",
                _ => "ota_disable",
            },
            "ota_package",
            &format!("{version}:{channel}"),
            &Self::ota_audit_detail(&req.reason, &req.note),
            now,
        )?;
        self.store
            .get_ota_package(&version, &channel)?
            .ok_or_else(|| LicenseError::Storage("ota package vanished right after update".into()))
    }

    /// 网关拉取 manifest（`GET /updates/manifest`）：返回该通道**最新已发布**的包。
    ///
    /// ## 诚实空态（绝不伪造）
    /// - 该通道无任何 `published` 包 → `available:false` + 面向用户的 `reason`；
    /// - 传了 `current` 且最新已发布版本 **不大于** 它 → `available:false`（网关
    ///   `OtaManager::verify_and_stage` 强制版本严格递增，此时下发必然被拒，
    ///   与其让网关验签后才报「版本回退」，不如在此诚实说明「当前已是最新版本」）。
    ///
    /// ## 传输（V1 已知限制）
    /// 与激活同通道走 **HTTP 明文**（`daemon::ota` V1 不支持 TLS）。机密性无保障，
    /// 完整性由 Ed25519 签名兜底；生产部署建议经可信内网下发。
    pub fn ota_manifest(
        &self,
        channel_raw: Option<&str>,
        current_raw: Option<&str>,
    ) -> LicenseResult<OtaManifestResponse> {
        let channel = Self::validate_ota_channel(channel_raw.unwrap_or("stable"))?;
        let Some(pkg) = self.store.latest_published_ota(&channel)? else {
            return Ok(OtaManifestResponse {
                available: false,
                version: String::new(),
                ts_ns: String::new(),
                size: String::new(),
                payload_sha256: String::new(),
                payload_b64: String::new(),
                sig_b64: String::new(),
                kid: String::new(),
                reason: format!(
                    "当前 {channel} 通道还没有已发布的更新包。请在授权管理后台的「系统更新」\
                     页上传并发布一个更新包后，网关即可检查到更新。"
                ),
            });
        };
        if let Some(current) = current_raw.map(str::trim).filter(|s| !s.is_empty()) {
            match (current.parse::<u64>(), pkg.version.parse::<u64>()) {
                (Ok(cur), Ok(latest)) if latest <= cur => {
                    return Ok(OtaManifestResponse {
                        available: false,
                        version: pkg.version.clone(),
                        ts_ns: String::new(),
                        size: String::new(),
                        payload_sha256: String::new(),
                        payload_b64: String::new(),
                        sig_b64: String::new(),
                        kid: String::new(),
                        reason: format!(
                            "当前版本 {cur} 已是 {channel} 通道的最新版本（已发布最高版本 \
                             {}），没有需要安装的更新。",
                            pkg.version
                        ),
                    });
                }
                _ => {}
            }
        }
        // `ts_ns` 由发布时刻（UTC 秒）派生为纳秒；网关侧仅作信息性字段（不进签名对象）。
        let ts_ns = pkg
            .published_at
            .parse::<i64>()
            .unwrap_or(0)
            .saturating_mul(1_000_000_000)
            .to_string();
        Ok(OtaManifestResponse {
            available: true,
            version: pkg.version,
            ts_ns,
            size: pkg.size,
            payload_sha256: pkg.payload_sha256,
            payload_b64: pkg.payload_b64,
            sig_b64: pkg.sig_b64,
            kid: pkg.kid,
            reason: String::new(),
        })
    }

    /// 版本号归一（u64 单调序的**规范十进制串**）。
    ///
    /// 拒绝前导零 / 空白 / 非数字：`"007"` 与 `"7"` 若都合法，`(version, channel)`
    /// 主键会出现两个逻辑同版本的包，且签名消息里的 `ver=<len>` 也会分叉。
    fn validate_ota_version(raw: &str) -> LicenseResult<String> {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            return Err(LicenseError::KeyStateIllegal(
                "ota version must not be empty".into(),
            ));
        }
        let parsed: u64 = trimmed.parse().map_err(|_| {
            LicenseError::KeyStateIllegal(format!(
                "ota version must be a decimal u64, got {trimmed:?}"
            ))
        })?;
        if parsed == 0 {
            return Err(LicenseError::KeyStateIllegal(
                "ota version must be greater than 0".into(),
            ));
        }
        let canonical = parsed.to_string();
        if canonical != trimmed {
            return Err(LicenseError::KeyStateIllegal(format!(
                "ota version must be the canonical decimal form of {parsed} (no leading zeros), got {trimmed:?}"
            )));
        }
        Ok(canonical)
    }

    /// 通道归一（缺省 / 空白 = `stable`；未知通道一律拒绝，不静默回退）。
    fn validate_ota_channel(raw: &str) -> LicenseResult<String> {
        let trimmed = raw.trim();
        let channel = if trimmed.is_empty() {
            "stable"
        } else {
            trimmed
        };
        if OTA_CHANNELS.contains(&channel) {
            Ok(channel.to_string())
        } else {
            Err(LicenseError::KeyStateIllegal(format!(
                "unknown ota channel: {channel}; allowed: {}",
                OTA_CHANNELS.join(" | ")
            )))
        }
    }

    /// 危险操作四要素校验（`reason` / `note` / `confirm` 三个**彼此独立**的字段）。
    ///
    /// `note` **绝不允许**拼进 `reason`——二者分别入审计：前者是「为什么做」（枚举原因），
    /// 后者是「补充说明」。拼接会让审计无法区分，也让「填了 reason 就算过」的绕过成立。
    ///
    /// - `reason` trim 后空白 → [`LicenseError::ReasonRequired`]（400 `REASON_REQUIRED`）；
    /// - `note` trim 后少于 10 字符 → `KeyStateIllegal`（400 `BAD_REQUEST`）；
    /// - `confirm` trim 后与 `expected` **大小写不敏感**精确匹配失败 →
    ///   [`LicenseError::ConfirmMismatch`]（412 `CONFIRM_MISMATCH`，沿用本服务既有口径）。
    fn validate_ota_dangerous_contract(
        reason: &str,
        note: &str,
        confirm: &str,
        expected: &str,
    ) -> LicenseResult<()> {
        if reason.trim().is_empty() {
            return Err(LicenseError::reason_required(
                "ota write requires a non-empty reason (independent of note)",
            ));
        }
        if note.trim().chars().count() < 10 {
            return Err(LicenseError::KeyStateIllegal(
                "ota write note must be at least 10 characters and must not be merged into reason"
                    .into(),
            ));
        }
        let supplied = confirm.trim().to_uppercase();
        let expected_upper = expected.trim().to_uppercase();
        if supplied.is_empty() || supplied != expected_upper {
            return Err(LicenseError::confirm_mismatch(
                "confirm does not match the target ota version",
            ));
        }
        Ok(())
    }

    /// 审计详情（JSON；**不承载** payload / 签名等大字段与任何敏感值）。
    fn ota_audit_detail(reason: &str, note: &str) -> String {
        serde_json::json!({ "reason": reason.trim(), "note": note.trim() }).to_string()
    }

    /// payload → **小写** hex SHA-256（签名对象里就是它，大小写必须稳定）。
    fn sha256_lower_hex(data: &[u8]) -> String {
        let digest = Sha256::digest(data);
        digest.iter().map(|b| format!("{b:02x}")).collect()
    }

    /// 写入一条审计日志（后台动作统一入口，actor 固定为管理员）。
    fn audit(
        &self,
        _tenant_id: &str,
        actor_id: &str,
        action: &str,
        entity_type: &str,
        entity_id: &str,
        now: i64,
    ) -> LicenseResult<()> {
        self.write_audit(
            ActorType::Admin,
            actor_id,
            action,
            entity_type,
            entity_id,
            "",
            now,
        )
    }

    /// 写入一条审计日志（通用入口：任意 actor_type + 详情）。
    ///
    /// **不承载敏感值**：`detail` 不得包含激活码原文 / 私钥。设备侧动作（心跳 / 校验 /
    /// 回执异常）以 [`ActorType::Device`] / [`ActorType::System`] 记录。
    #[allow(clippy::too_many_arguments)]
    fn write_audit(
        &self,
        actor_type: ActorType,
        actor_id: &str,
        action: &str,
        entity_type: &str,
        entity_id: &str,
        detail: &str,
        now: i64,
    ) -> LicenseResult<()> {
        let log = AuditLog {
            id: now_ns_id("audit"),
            actor_type,
            actor_id: actor_id.to_string(),
            action: action.to_string(),
            entity_type: entity_type.to_string(),
            entity_id: entity_id.to_string(),
            detail: detail.to_string(),
            ts: now,
            ip: String::new(),
        };
        self.store.insert_audit_log(&log)
    }
}

#[cfg(test)]
mod tests {
    use super::LicensingService;
    use crate::audit::ReceiptLedger;
    use crate::error::{LicenseError, PrebindKind};
    use crate::keys::{current_year, KeyRing};
    use crate::model::{now_ns_id, now_unix_secs, CodeStatus, Device, LeaseStatus, Tenant};
    use crate::proto::{
        ActivationRequest, GapKind, HeartbeatRequest, IssueCodesRequest, Prebind, ReceiptCursor,
        ReissueCodeRequest, RevokeCodeRequest,
    };
    use crate::store::{AuditFilter, Store};
    use base64::engine::general_purpose::STANDARD as B64;
    use base64::Engine as _;
    use ed25519_dalek::{Signer, SigningKey};
    use serde_json::json;
    use std::sync::Arc;

    /// **TEST_ONLY_** 设备/签发密钥种子（仅测试；生产密钥绝不硬编码、绝不入仓库）。
    const TEST_ONLY_SEED: [u8; 32] = *b"iotdaq-test-seed-service-0000001";
    /// **TEST_ONLY_** 独立异钥种子（用于「伪造签名必被拒」的负例）。
    const TEST_ONLY_ROGUE_SEED: [u8; 32] = *b"iotdaq-rogue-seed-service-000001";
    /// **TEST_ONLY_** 设备密钥种子（激活请求 `req_sig` 的签名密钥；仅测试，禁止真实部署）。
    const TEST_ONLY_DEVICE_SEED: [u8; 32] = *b"iotdaq-device-seed-service-00001";

    /// 设备公钥（STANDARD base64，32 字节 Ed25519），由给定种子导出。
    fn device_pubkey_b64(seed: &[u8; 32]) -> String {
        let key = SigningKey::from_bytes(seed);
        B64.encode(key.verifying_key().to_bytes())
    }

    /// 构造带内存库 + 已知测试密钥的服务（测试专用）。
    ///
    /// 用**已知种子**注册密钥环，使测试可对心跳 / 校验请求生成可验证的 `device_sig`。
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
            .register_from_b64(
                "k-test",
                &B64.encode(TEST_ONLY_SEED),
                Some("TEST_ONLY_kms".into()),
                1_700_000_000,
            )
            .expect("register signing key");
        Arc::new(LicensingService::with_ledger(
            store,
            keyring,
            ReceiptLedger::open_in_memory(),
        ))
    }

    /// 用测试密钥对给定摘要签名（STANDARD base64）。
    fn sign_hash(hash: &[u8; 32]) -> String {
        let key = SigningKey::from_bytes(&TEST_ONLY_SEED);
        B64.encode(key.sign(hash).to_bytes())
    }

    /// 用**异钥**对给定摘要签名（负例用）。
    fn sign_hash_rogue(hash: &[u8; 32]) -> String {
        let key = SigningKey::from_bytes(&TEST_ONLY_ROGUE_SEED);
        B64.encode(key.sign(hash).to_bytes())
    }

    /// 走一遍激活，返回 `(lease_id, machine_code)`（心跳 / 校验 / 回执测试的前置）。
    fn activate_one(svc: &LicensingService) -> (String, String) {
        let resp = svc
            .issue_codes(&issue_req("t-1", Some("MID-A"), &now_ns_id("idem")))
            .expect("issue");
        let code = &resp.codes[0];
        let act = svc
            .activate(&activate_req(&code.code, "MID-A"))
            .expect("activate");
        (act.lease_id, "MID-A".to_string())
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

    /// 构造一个**签名合法、nonce 全局唯一**的激活请求（默认设备密钥）。
    fn activate_req(code_value: &str, machine: &str) -> ActivationRequest {
        let anchors: Vec<String> = vec!["a".to_string(); 5];
        build_activation_req(code_value, machine, &anchors, &TEST_ONLY_DEVICE_SEED)
    }

    /// 构造带**自定义锚点集**的激活请求（N-of-M 冲突检测测试用；默认设备密钥）。
    fn activate_req_with_anchors(
        code_value: &str,
        machine: &str,
        anchors: &[&str],
    ) -> ActivationRequest {
        let owned: Vec<String> = anchors.iter().map(|s| (*s).to_string()).collect();
        build_activation_req(code_value, machine, &owned, &TEST_ONLY_DEVICE_SEED)
    }

    /// 构造激活请求（给定设备密钥种子）：`req_sig` = 设备私钥对
    /// `activation_payload_hash`（规范化语义哈希）的签名；nonce 取 `now_ns_id`（唯一）；
    /// ts 取当前时钟。与 daemon 侧 `auth::client::activate` 的构造逐字段同构。
    fn build_activation_req(
        code_value: &str,
        machine: &str,
        anchors: &[String],
        seed: &[u8; 32],
    ) -> ActivationRequest {
        let ts = now_unix_secs();
        let key = SigningKey::from_bytes(seed);
        let pubkey = B64.encode(key.verifying_key().to_bytes());
        let nonce = now_ns_id("n");
        let hash = crate::device_auth::activation_payload_hash(
            code_value, machine, anchors, &pubkey, &nonce, ts,
        );
        ActivationRequest {
            activation_code: code_value.to_string(),
            machine_code: machine.to_string(),
            anchor_hashes: anchors.to_vec(),
            device_pubkey: pubkey,
            nonce,
            ts: ts.to_string(),
            req_sig: B64.encode(key.sign(&hash).to_bytes()),
        }
    }

    /// 统计审计日志条数（副作用断言用）。
    fn count_audit(svc: &LicensingService) -> usize {
        svc.store()
            .list_audit_logs(&AuditFilter::default(), 1, 1000)
            .unwrap()
            .len()
    }

    /// 按机器码取设备（断言必须存在）。
    fn device_of(svc: &LicensingService, machine: &str) -> Device {
        svc.store()
            .get_device_by_machine_code(machine)
            .expect("get device")
            .expect("device must exist")
    }

    /// 构造废弃请求（`confirm_tail8` 按码值尾 8 位计算，与 HTTP 契约同构）。
    fn revoke_req(code_value: &str, reason: &str) -> RevokeCodeRequest {
        let tail8: String = code_value
            .chars()
            .filter(char::is_ascii_alphanumeric)
            .collect::<String>()
            .to_uppercase()
            .chars()
            .rev()
            .take(8)
            .collect::<String>()
            .chars()
            .rev()
            .collect();
        RevokeCodeRequest {
            reason: reason.to_string(),
            note: "note note note".to_string(),
            confirm_tail8: tail8,
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
        assert_eq!(
            code.prebind.as_deref(),
            Some("m1"),
            "预绑定必须落库（归一后小写）"
        );

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

    /// 2026-09-27 契约变更：发放侧一机一码闭环——缺失 / 空白机器码一律拒绝
    /// （400 `MACHINE_CODE_REQUIRED`），不再归一为「无预绑定」。
    #[test]
    fn t46_missing_or_blank_prebind_is_rejected() {
        let svc = build_service();
        // 缺失（None）。
        let mut missing = issue_req("t-1", Some("MID-A"), "g1-empty");
        missing.prebind_machine_code = None;
        let err = svc.issue_codes(&missing).unwrap_err();
        assert!(
            matches!(err, LicenseError::MachineCodeRequired(_)),
            "{err:?}"
        );
        assert_eq!(err.error_code(), crate::error::ERR_LICENSE_MACHINE_CODE);

        // 空白（"   "）同样拒绝。
        let err = svc
            .issue_codes(&issue_req("t-1", Some("   "), "g1-empty-blank"))
            .unwrap_err();
        assert!(
            matches!(err, LicenseError::MachineCodeRequired(_)),
            "{err:?}"
        );
    }

    // ---------------- G2：重发幂等（fail-closed） ----------------

    #[test]
    fn t46_reissue_idempotent_by_idempotency_key() {
        let svc = build_service();
        let resp = svc
            .issue_codes(&issue_req("t-1", Some("MID-A"), "g2-issue"))
            .unwrap();
        let code_id = resp.codes[0].code_id.clone();
        svc.revoke(
            "t-1",
            &code_id,
            &revoke_req(&resp.codes[0].code, "compromised"),
            "admin",
        )
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
            .issue_codes(&issue_req("t-1", Some("MID-A"), "g2-issue-dup"))
            .unwrap();
        let r2 = svc
            .issue_codes(&issue_req("t-1", Some("MID-A"), "g2-issue-dup"))
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
        // 2026-09-27 契约变更后，构造「设备已被另一张码绑定」：
        // A 码预绑定 MID-A 并激活（设备 MID-A 建立）→ 废弃 → 重发出**无预绑定**的 B 码
        // （重发不强制机器码）→ B 码绑定到机器 M-BOUND。此时发放预绑定 M-BOUND 的新码：
        // 码侧 claim 不命中（B 无预绑定），设备侧命中 → MachineAlreadyBound。
        let resp_a = svc
            .issue_codes(&issue_req("t-1", Some("MID-A"), "g3-issue-d"))
            .unwrap();
        let code_a = &resp_a.codes[0];
        svc.activate(&activate_req(&code_a.code, "MID-A")).unwrap();
        svc.revoke(
            "t-1",
            &code_a.code_id,
            &revoke_req(&code_a.code, "replace"),
            "admin",
        )
        .unwrap();
        let reissued = svc
            .reissue(
                "t-1",
                &code_a.code_id,
                &reissue_req(None, "g3-issue-d-reissue"),
                "admin",
            )
            .unwrap();
        svc.activate(&activate_req(&reissued.new_code.code, "M-BOUND"))
            .unwrap();

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
            .issue_codes(&issue_req("t-1", Some("MID-A"), "g3-issue-g"))
            .unwrap();
        let code_id = resp.codes[0].code_id.clone();
        svc.revoke(
            "t-1",
            &code_id,
            &revoke_req(&resp.codes[0].code, "x"),
            "admin",
        )
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

    // ---------------- G4：空白预绑定（发放拒绝 / 重发仍归一） ----------------

    #[test]
    fn t46_reissue_whitespace_prebind_treated_as_none() {
        let svc = build_service();
        // 发放：机器码必填（见 t46_missing_or_blank_prebind_is_rejected）；
        // 本用例覆盖重发路径的空白预绑定归一（重发不强制机器码，契约保留）。
        let resp2 = svc
            .issue_codes(&issue_req("t-1", Some("MID-A"), "g4-issue-b"))
            .unwrap();
        let cid = resp2.codes[0].code_id.clone();
        svc.revoke("t-1", &cid, &revoke_req(&resp2.codes[0].code, "x"), "admin")
            .unwrap();
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

    /// 未知租户 → 独立变体 `TenantNotFound`（400 `TENANT_NOT_FOUND`），
    /// 消息含「先创建租户」引导，绝不静默放行（fail-closed 保留）。
    #[test]
    fn t_issue_unknown_tenant_maps_to_tenant_not_found() {
        let svc = build_service();
        let err = svc
            .issue_codes(&issue_req("t-nope", Some("MID-A"), "g-tenant-missing"))
            .unwrap_err();
        assert!(matches!(err, LicenseError::TenantNotFound(_)), "{err:?}");
        assert_eq!(err.error_code(), crate::error::ERR_LICENSE_TENANT);
        let msg = err.to_string();
        assert!(msg.contains("t-nope"), "消息应含租户 ID: {msg}");
        assert!(
            msg.contains("create the tenant first"),
            "消息应含创建租户引导: {msg}"
        );
    }

    // ---------------- G5：幂等键归一（trim） ----------------

    #[test]
    fn t46_idempotency_key_normalized_trim() {
        let svc = build_service();
        let r1 = svc
            .issue_codes(&issue_req("t-1", Some("MID-A"), "  key-norm  "))
            .unwrap();
        // 尾部 / 头部空白变体命中同一逻辑键 → 幂等。
        let r2 = svc
            .issue_codes(&issue_req("t-1", Some("MID-A"), "key-norm"))
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
            .issue_codes(&issue_req("t-1", Some("MID-A"), "g5-issue"))
            .unwrap();
        let cid = resp.codes[0].code_id.clone();
        svc.revoke("t-1", &cid, &revoke_req(&resp.codes[0].code, "x"), "admin")
            .unwrap();
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

    // ================= §7 一机一码冲突检测（N-of-M 服务端强制点） =================

    /// 参考锚点集（5 个互异值，模拟真实逐锚点 HMAC 哈希）。
    const BASE_ANCHORS: [&str; 5] = [
        "h-anchor-1",
        "h-anchor-2",
        "h-anchor-3",
        "h-anchor-4",
        "h-anchor-5",
    ];

    /// ① 同码同机重复激活 → **幂等**：device 数不变、租约数不变、返回同一租约与 Token。
    #[test]
    fn t46_same_code_same_machine_reactivation_is_idempotent() {
        let svc = build_service();
        let resp = svc
            .issue_codes(&issue_req("t-1", Some("MID-A"), "nofm-idem"))
            .unwrap();
        let code = &resp.codes[0];

        let first = svc
            .activate(&activate_req_with_anchors(
                &code.code,
                "MID-A",
                &BASE_ANCHORS,
            ))
            .unwrap();
        let dev = device_of(&svc, "MID-A");
        assert_eq!(
            svc.store().count_devices(None).unwrap(),
            1,
            "只应有 1 台设备"
        );
        assert_eq!(
            svc.store()
                .list_leases_by_device(&dev.device_id)
                .unwrap()
                .len(),
            1,
            "首次激活应恰好签发 1 条租约"
        );

        // 同码同机重复激活 → 幂等：同一租约、同一 Token、无新增。
        let second = svc
            .activate(&activate_req_with_anchors(
                &code.code,
                "MID-A",
                &BASE_ANCHORS,
            ))
            .unwrap();
        assert_eq!(first.lease_id, second.lease_id, "必须返回现存租约（幂等）");
        assert_eq!(
            first.lease_token, second.lease_token,
            "同机重签必须逐字节一致（Ed25519 确定性 + 载荷取自既有租约）"
        );
        assert_eq!(svc.store().count_devices(None).unwrap(), 1, "device 数不变");
        assert_eq!(
            svc.store()
                .list_leases_by_device(&dev.device_id)
                .unwrap()
                .len(),
            1,
            "租约数不变（绝不产生第二条重复租约）"
        );
    }

    /// ② 异机但命中恰好 4/5（1 项漂移，模拟换网卡）→ **自动改绑 + 重签**，审计留痕。
    #[test]
    fn t46_one_anchor_drift_rebinds_same_machine_and_reissues_lease() {
        let svc = build_service();
        let resp = svc
            .issue_codes(&issue_req("t-1", Some("MID-A"), "nofm-drift1"))
            .unwrap();
        let code = &resp.codes[0];

        let first = svc
            .activate(&activate_req_with_anchors(
                &code.code,
                "MID-A",
                &BASE_ANCHORS,
            ))
            .unwrap();
        let dev = device_of(&svc, "MID-A");

        // machine_code 变化 + 恰好 1 项锚点漂移（命中 4/5）→ 判同机。
        let drifted = [
            "h-anchor-1",
            "h-anchor-2",
            "h-anchor-3",
            "h-anchor-4",
            "h-anchor-DRIFT",
        ];
        let second = svc
            .activate(&activate_req_with_anchors(&code.code, "MID-B", &drifted))
            .unwrap();
        assert_ne!(first.lease_id, second.lease_id, "改绑必须重签新租约");

        // 改绑生效：同一 device 的身份锚点前移到新机器。
        let rebound = device_of(&svc, "MID-B");
        assert_eq!(
            rebound.device_id, dev.device_id,
            "复用同一 device（不新建）"
        );
        assert_eq!(
            rebound.anchor_hashes,
            drifted.iter().map(|s| (*s).to_string()).collect::<Vec<_>>(),
            "anchor_hashes 必须随 machine_code 一起前移"
        );
        assert!(
            svc.store()
                .get_device_by_machine_code("MID-A")
                .unwrap()
                .is_none(),
            "旧 machine_code 不应再指向任何设备"
        );

        // 旧租约已作废、新租约已签发且可用。
        assert_eq!(
            svc.store()
                .list_leases_by_device(&dev.device_id)
                .unwrap()
                .len(),
            2,
            "旧租约（作废）保留 + 新租约，共 2 条"
        );
        assert_eq!(
            svc.store()
                .get_lease(&first.lease_id)
                .unwrap()
                .unwrap()
                .status,
            LeaseStatus::Stopped,
            "旧租约必须被作废"
        );
        assert!(
            svc.store()
                .get_lease(&second.lease_id)
                .unwrap()
                .unwrap()
                .is_usable_at(now_unix_secs()),
            "新租约必须可用"
        );

        // 审计留痕：命中 4 / 漂移 1，且不含任何敏感值。
        let logs = svc
            .store()
            .list_audit_logs(
                &AuditFilter {
                    action: Some("activation_rebind".into()),
                    ..Default::default()
                },
                1,
                10,
            )
            .unwrap();
        assert_eq!(logs.len(), 1, "改绑必须留一条审计");
        assert!(
            logs[0].detail.contains("hits=4"),
            "审计须记命中项数: {}",
            logs[0].detail
        );
        assert!(
            logs[0].detail.contains("drifts=1"),
            "审计须记漂移项数: {}",
            logs[0].detail
        );
        assert!(!logs[0].detail.contains("h-anchor"), "审计不得含锚点原文");
        assert!(
            !logs[0].detail.contains("MID-A") && !logs[0].detail.contains("MID-B"),
            "审计不得含 machine_code 明文: {}",
            logs[0].detail
        );
    }

    /// ③ 异机且命中恰好 3/5 → `CodeBoundToOtherDevice`（403），**无任何副作用**。
    #[test]
    fn t46_two_anchor_drift_is_rejected_as_other_device_without_side_effects() {
        let svc = build_service();
        let resp = svc
            .issue_codes(&issue_req("t-1", Some("MID-A"), "nofm-drift2"))
            .unwrap();
        let code = &resp.codes[0];
        svc.activate(&activate_req_with_anchors(
            &code.code,
            "MID-A",
            &BASE_ANCHORS,
        ))
        .unwrap();
        let dev = device_of(&svc, "MID-A");

        // 命中恰好 3/5 → 异机。
        let foreign = ["h-anchor-1", "h-anchor-2", "h-anchor-3", "h-x", "h-y"];
        let err = svc
            .activate(&activate_req_with_anchors(&code.code, "MID-B", &foreign))
            .unwrap_err();
        assert!(
            matches!(err, LicenseError::CodeBoundToOtherDevice { .. }),
            "异机必须判 CodeBoundToOtherDevice，实际: {err:?}"
        );
        assert_eq!(
            err.error_code(),
            crate::error::ERR_LICENSE_BOUND_OTHER_DEVICE
        );

        // 无副作用：device 记录未变、device 数 / 租约数不变、无新绑定。
        assert_eq!(device_of(&svc, "MID-A"), dev, "设备记录不得被改动");
        assert_eq!(svc.store().count_devices(None).unwrap(), 1);
        assert_eq!(
            svc.store()
                .list_leases_by_device(&dev.device_id)
                .unwrap()
                .len(),
            1
        );
        assert!(
            svc.store()
                .get_device_by_machine_code("MID-B")
                .unwrap()
                .is_none(),
            "不得产生新绑定"
        );
        // 拒绝同样留审计（命中 3 / 漂移 2）。
        let logs = svc
            .store()
            .list_audit_logs(
                &AuditFilter {
                    action: Some("activation_rejected_bound_to_other_device".into()),
                    ..Default::default()
                },
                1,
                10,
            )
            .unwrap();
        assert_eq!(logs.len(), 1);
        assert!(
            logs[0].detail.contains("hits=3") && logs[0].detail.contains("drifts=2"),
            "{}",
            logs[0].detail
        );
    }

    /// ③ 命中 0/5、空 `anchor_hashes` → 同样拒绝（且无副作用）。
    #[test]
    fn t46_zero_hits_and_empty_anchors_are_rejected() {
        let svc = build_service();
        let resp = svc
            .issue_codes(&issue_req("t-1", Some("MID-A"), "nofm-zero"))
            .unwrap();
        let code = &resp.codes[0];
        svc.activate(&activate_req_with_anchors(
            &code.code,
            "MID-A",
            &BASE_ANCHORS,
        ))
        .unwrap();

        let err = svc
            .activate(&activate_req_with_anchors(
                &code.code,
                "MID-Z",
                &["z1", "z2", "z3", "z4", "z5"],
            ))
            .unwrap_err();
        assert!(
            matches!(err, LicenseError::CodeBoundToOtherDevice { .. }),
            "{err:?}"
        );

        let err = svc
            .activate(&activate_req_with_anchors(&code.code, "MID-Y", &[]))
            .unwrap_err();
        assert!(
            matches!(err, LicenseError::CodeBoundToOtherDevice { .. }),
            "空锚点集必须判异机: {err:?}"
        );

        assert_eq!(
            svc.store().count_devices(None).unwrap(),
            1,
            "拒绝不得产生设备"
        );
    }

    /// 回归：预绑定不匹配（未绑定码）仍走既有 `ActivationMachineMismatch`，未被 §7 覆盖。
    #[test]
    fn t46_prebind_mismatch_still_rejected_as_activation_machine_mismatch() {
        let svc = build_service();
        let resp = svc
            .issue_codes(&issue_req("t-1", Some("MID-PB"), "nofm-prebind"))
            .unwrap();
        let code = &resp.codes[0];
        let err = svc
            .activate(&activate_req_with_anchors(
                &code.code,
                "MID-OTHER",
                &BASE_ANCHORS,
            ))
            .unwrap_err();
        match err {
            LicenseError::PrebindConflict { kind } => {
                assert_eq!(kind, PrebindKind::ActivationMachineMismatch)
            }
            other => panic!("expected PrebindConflict, got {other:?}"),
        }
    }

    /// 回归：revoked 码仍被拒（④），且不产生设备。
    #[test]
    fn t46_revoked_code_is_rejected() {
        let svc = build_service();
        let resp = svc
            .issue_codes(&issue_req("t-1", Some("MID-A"), "nofm-revoked"))
            .unwrap();
        let cid = resp.codes[0].code_id.clone();
        let code = resp.codes[0].code.clone();
        svc.revoke("t-1", &cid, &revoke_req(&code, "gone"), "admin")
            .unwrap();

        let err = svc
            .activate(&activate_req_with_anchors(&code, "MID-A", &BASE_ANCHORS))
            .unwrap_err();
        assert!(
            matches!(err, LicenseError::ActivationRejected(_)),
            "{err:?}"
        );
        assert_eq!(
            svc.store().count_devices(None).unwrap(),
            0,
            "拒绝不得产生设备"
        );
    }

    /// **敏感性断言**：拒绝路径的错误消息与审计事件**都不含**锚点原文 / `machine_code` / 码值。
    #[test]
    fn t46_reject_path_never_leaks_machine_code_anchors_or_code_value() {
        let svc = build_service();
        let resp = svc
            .issue_codes(&issue_req("t-1", Some("MACHINE-BOUND"), "nofm-leak"))
            .unwrap();
        let code = &resp.codes[0];

        // 绑定记录锚点（会落 device.anchor_hashes）。
        let bound = [
            "SECRET-ANCHOR-1",
            "SECRET-ANCHOR-2",
            "SECRET-ANCHOR-3",
            "SECRET-ANCHOR-4",
            "SECRET-ANCHOR-5",
        ];
        svc.activate(&activate_req_with_anchors(
            &code.code,
            "MACHINE-BOUND",
            &bound,
        ))
        .unwrap();

        // 异机请求：machine_code 与锚点都带可识别串。
        let err = svc
            .activate(&activate_req_with_anchors(
                &code.code,
                "LEAK-MACHINE-XYZ",
                &[
                    "LEAK-ANCHOR-A",
                    "LEAK-ANCHOR-B",
                    "LEAK-ANCHOR-C",
                    "LEAK-ANCHOR-D",
                    "LEAK-ANCHOR-E",
                ],
            ))
            .unwrap_err();
        let msg = err.to_string();
        for secret in [
            "LEAK-MACHINE-XYZ",
            "LEAK-ANCHOR-A",
            "SECRET-ANCHOR-1",
            code.code.as_str(),
        ] {
            assert!(!msg.contains(secret), "错误信息泄露敏感值 {secret}: {msg}");
        }
        assert!(msg.contains("machine replacement"), "须给可操作提示: {msg}");

        // 审计事件同样不得含敏感值。
        let logs = svc
            .store()
            .list_audit_logs(
                &AuditFilter {
                    action: Some("activation_rejected_bound_to_other_device".into()),
                    ..Default::default()
                },
                1,
                10,
            )
            .unwrap();
        assert_eq!(logs.len(), 1);
        let blob = format!(
            "{} {} {}",
            logs[0].actor_id, logs[0].detail, logs[0].entity_id
        );
        for secret in [
            "LEAK-MACHINE-XYZ",
            "LEAK-ANCHOR-A",
            "SECRET-ANCHOR-1",
            code.code.as_str(),
        ] {
            assert!(!blob.contains(secret), "审计泄露敏感值 {secret}: {blob}");
        }
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
            .issue_codes(&issue_req("t-1", Some("MID-A"), "g-state-issue"))
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
            .issue_codes(&issue_req("t-1", Some("MID-A"), "g-state-issue-b"))
            .unwrap();
        let cid = resp.codes[0].code_id.clone();
        svc.revoke(
            "t-1",
            &cid,
            &revoke_req(&resp.codes[0].code, "first"),
            "admin",
        )
        .unwrap();
        // 再次废弃：幂等成功（no-op）。
        assert!(svc
            .revoke(
                "t-1",
                &cid,
                &revoke_req(&resp.codes[0].code, "second"),
                "admin"
            )
            .is_ok());
        let stored = svc.store().get_code_by_id(&cid).unwrap().unwrap();
        assert_eq!(stored.status, CodeStatus::Revoked);
    }

    // ======================= 心跳 / 校验 / 回执 端点 =======================

    /// 构造已签名的心跳请求。
    fn heartbeat_req(
        lease_id: &str,
        nonce: &str,
        ts: i64,
        cursor: Option<(i64, i64)>,
        sign_ts: i64,
    ) -> HeartbeatRequest {
        let hash = crate::device_auth::heartbeat_payload_hash(lease_id, sign_ts, nonce, cursor);
        let sig = sign_hash(&hash);
        HeartbeatRequest {
            lease_id: lease_id.to_string(),
            ts: ts.to_string(),
            nonce: nonce.to_string(),
            receipt_cursor: cursor.map(|(f, t)| ReceiptCursor {
                seq_from: f.to_string(),
                seq_to: t.to_string(),
            }),
            device_sig: sig,
        }
    }

    /// 构造 `/verify` 原始请求 JSON（已签名）。
    fn verify_value(
        mid: &str,
        lease_id: &str,
        digest: &str,
        ts: i64,
        nonce: &str,
        sign_ts: i64,
    ) -> serde_json::Value {
        let hash = crate::device_auth::verify_payload_hash(mid, lease_id, digest, sign_ts, nonce);
        json!({
            "device_mid": mid,
            "lease_id": lease_id,
            "payload_digest": digest,
            "ts": ts.to_string(),
            "nonce": nonce,
            "device_sig": sign_hash(&hash),
        })
    }

    /// 构造已签名的回执请求 JSON。
    fn receipt_value(
        mid: &str,
        lease_id: &str,
        from: i64,
        to: i64,
        digest: &str,
        signer: &dyn Fn(&[u8; 32]) -> String,
    ) -> serde_json::Value {
        let now = now_unix_secs();
        let count = to - from + 1;
        let hash =
            crate::receipt::receipt_payload_hash(mid, lease_id, from, to, count, digest, now);
        json!({
            "device_mid": mid,
            "lease_id": lease_id,
            "seq_from": from.to_string(),
            "seq_to": to.to_string(),
            "count": count.to_string(),
            "payload_digest": digest,
            "ts": now.to_string(),
            "sig": signer(&hash),
        })
    }

    // ---------------- 心跳 ----------------

    #[test]
    fn hb_happy_writes_last_heartbeat_and_device_active() {
        let svc = build_service();
        let (lease_id, mid) = activate_one(&svc);
        let now = now_unix_secs();
        let req = heartbeat_req(&lease_id, "hb-happy", now, None, now);
        let resp = svc.heartbeat(&req).expect("heartbeat ok");
        assert!(!resp.sig.is_empty(), "响应必须带服务端签名");
        assert_eq!(resp.verify_mode, "B");
        assert_eq!(resp.server_time, now.to_string());
        assert!(resp.next_deadline.parse::<i64>().unwrap() > now);

        let lease = svc.store().get_lease(&lease_id).unwrap().unwrap();
        assert!(
            lease.last_heartbeat_at.is_some(),
            "必须回写 last_heartbeat_at"
        );
        let device = svc
            .store()
            .get_device_by_machine_code(&mid)
            .unwrap()
            .unwrap();
        assert_eq!(device.status, crate::model::DeviceStatus::Active);
        // 心跳记录落库。
        assert_eq!(
            svc.store()
                .list_heartbeats_by_lease(&lease_id)
                .unwrap()
                .len(),
            1
        );
    }

    #[test]
    fn hb_revoked_lease_returns_lease_revoked() {
        let svc = build_service();
        let (lease_id, _) = activate_one(&svc);
        svc.store()
            .update_lease_status(&lease_id, LeaseStatus::Stopped)
            .unwrap();
        let now = now_unix_secs();
        let err = svc
            .heartbeat(&heartbeat_req(&lease_id, "hb-rev", now, None, now))
            .unwrap_err();
        assert!(matches!(err, LicenseError::LeaseRevoked(_)), "{err:?}");
    }

    #[test]
    fn hb_unknown_lease_returns_lease_not_found() {
        let svc = build_service();
        let now = now_unix_secs();
        let err = svc
            .heartbeat(&heartbeat_req("lease-missing", "hb-404", now, None, now))
            .unwrap_err();
        assert!(matches!(err, LicenseError::LeaseNotFound(_)), "{err:?}");
    }

    #[test]
    fn hb_replayed_nonce_returns_nonce_replay() {
        let svc = build_service();
        let (lease_id, _) = activate_one(&svc);
        let now = now_unix_secs();
        svc.heartbeat(&heartbeat_req(&lease_id, "hb-replay", now, None, now))
            .expect("first ok");
        let err = svc
            .heartbeat(&heartbeat_req(&lease_id, "hb-replay", now, None, now))
            .unwrap_err();
        assert!(matches!(err, LicenseError::NonceReplay(_)), "{err:?}");
    }

    #[test]
    fn hb_skewed_ts_returns_timestamp_skew() {
        let svc = build_service();
        let (lease_id, _) = activate_one(&svc);
        let now = now_unix_secs();
        let skewed = now - 10_000;
        // 签在 skew 的 ts 上 → 验签通过、时间窗拒绝（证明窗在验签之后判定）。
        let err = svc
            .heartbeat(&heartbeat_req(&lease_id, "hb-skew", skewed, None, skewed))
            .unwrap_err();
        assert!(matches!(err, LicenseError::TimestampSkew(_)), "{err:?}");
    }

    #[test]
    fn hb_forged_signature_returns_verify_failed() {
        let svc = build_service();
        let (lease_id, _) = activate_one(&svc);
        let now = now_unix_secs();
        let hash = crate::device_auth::heartbeat_payload_hash(&lease_id, now, "hb-forge", None);
        let req = HeartbeatRequest {
            lease_id,
            ts: now.to_string(),
            nonce: "hb-forge".into(),
            receipt_cursor: None,
            device_sig: sign_hash_rogue(&hash),
        };
        let err = svc.heartbeat(&req).unwrap_err();
        assert!(matches!(err, LicenseError::VerifyFailed(_)), "{err:?}");
    }

    // ---------------- /verify ----------------

    #[test]
    fn verify_happy_returns_ok_and_echoes_nonce() {
        let svc = build_service();
        let (lease_id, mid) = activate_one(&svc);
        let now = now_unix_secs();
        let v = verify_value(&mid, &lease_id, "sha256:abc", now, "v-ok", now);
        let resp = svc.verify(&v).expect("verify ok");
        assert!(resp.ok);
        assert_eq!(resp.nonce, "v-ok");
        assert_eq!(resp.server_time, now.to_string());
    }

    #[test]
    fn verify_forged_signature_returns_verify_failed() {
        let svc = build_service();
        let (lease_id, mid) = activate_one(&svc);
        let now = now_unix_secs();
        let mut v = verify_value(&mid, &lease_id, "sha256:abc", now, "v-forge", now);
        v["device_sig"] = json!("AAAA");
        let err = svc.verify(&v).unwrap_err();
        assert!(matches!(err, LicenseError::VerifyFailed(_)), "{err:?}");
    }

    #[test]
    fn verify_skewed_ts_returns_timestamp_skew() {
        let svc = build_service();
        let (lease_id, mid) = activate_one(&svc);
        let now = now_unix_secs();
        let skewed = now + 9_999;
        let v = verify_value(&mid, &lease_id, "sha256:abc", skewed, "v-skew", skewed);
        let err = svc.verify(&v).unwrap_err();
        assert!(matches!(err, LicenseError::TimestampSkew(_)), "{err:?}");
    }

    #[test]
    fn verify_replayed_nonce_returns_nonce_replay() {
        let svc = build_service();
        let (lease_id, mid) = activate_one(&svc);
        let now = now_unix_secs();
        let v = verify_value(&mid, &lease_id, "sha256:abc", now, "v-replay", now);
        svc.verify(&v).expect("first ok");
        let err = svc.verify(&v).unwrap_err();
        assert!(matches!(err, LicenseError::NonceReplay(_)), "{err:?}");
    }

    #[test]
    fn verify_extra_field_returns_field_whitelist_violation() {
        let svc = build_service();
        let (lease_id, mid) = activate_one(&svc);
        let now = now_unix_secs();
        let mut v = verify_value(&mid, &lease_id, "sha256:abc", now, "v-wl", now);
        v["flow_rate"] = json!("42"); // 白名单外业务字段
        let err = svc.verify(&v).unwrap_err();
        assert!(
            matches!(err, LicenseError::FieldWhitelistViolation(_)),
            "{err:?}"
        );
    }

    // ---------------- /audit/receipt ----------------

    #[test]
    fn receipt_happy_is_accepted_with_gap_none() {
        let svc = build_service();
        let (lease_id, mid) = activate_one(&svc);
        let v = receipt_value(&mid, &lease_id, 1, 100, "sha256:d", &sign_hash);
        let resp = svc.audit_receipt(&v).expect("accepted");
        assert!(resp.accepted);
        assert_eq!(resp.gap, GapKind::None);
        assert!(resp.warnings.is_empty());
    }

    #[test]
    fn receipt_gap_overlap_missing_are_detected() {
        let svc = build_service();
        let (lease_id, mid) = activate_one(&svc);

        // 连续窗口 1..100。
        let r = svc
            .audit_receipt(&receipt_value(&mid, &lease_id, 1, 100, "d1", &sign_hash))
            .unwrap();
        assert_eq!(r.gap, GapKind::None);

        // gap：150..3000（期望 101）。
        let r = svc
            .audit_receipt(&receipt_value(
                &mid, &lease_id, 150, 3_000, "d2", &sign_hash,
            ))
            .unwrap();
        assert_eq!(r.gap, GapKind::Gap, "跳空必须被检出");
        assert!(!r.warnings.is_empty());

        // overlap（回退）：40..50（前沿 3000）。
        let r = svc
            .audit_receipt(&receipt_value(&mid, &lease_id, 40, 50, "d3", &sign_hash))
            .unwrap();
        assert_eq!(r.gap, GapKind::Overlap, "回退 / 重叠必须被检出");

        // missing：另一台设备首个回执从 5 起步 → 前缀 [1,4] 缺失。
        let other = svc
            .issue_codes(&issue_req("t-1", Some("MID-OTHER"), &now_ns_id("idem-m")))
            .unwrap();
        let act = svc
            .activate(&activate_req(&other.codes[0].code, "MID-OTHER"))
            .unwrap();
        let r = svc
            .audit_receipt(&receipt_value(
                "MID-OTHER",
                &act.lease_id,
                5,
                9,
                "d4",
                &sign_hash,
            ))
            .unwrap();
        assert_eq!(r.gap, GapKind::Missing, "窗口内无回执必须判为 missing");
    }

    #[test]
    fn receipt_repeat_interval_is_idempotent_without_duplicate_alert() {
        let svc = build_service();
        let (lease_id, mid) = activate_one(&svc);
        let v = receipt_value(&mid, &lease_id, 1, 100, "d", &sign_hash);
        let first = svc.audit_receipt(&v).expect("first");
        assert!(first.accepted);

        // 相同区间重复上报 → 幂等接受，无重复告警。
        let second = svc.audit_receipt(&v).expect("replay accepted");
        assert!(second.accepted);
        assert_eq!(second.gap, GapKind::None);
        assert!(second.warnings.is_empty(), "重放不得重复告警");

        // 批次账本只落一条。
        assert_eq!(svc.ledger().batch_count().unwrap(), 1);
        assert!(svc.ledger().warnings_for(&mid).unwrap().is_empty());
    }

    #[test]
    fn receipt_extra_business_field_is_rejected() {
        let svc = build_service();
        let (lease_id, mid) = activate_one(&svc);
        let mut v = receipt_value(&mid, &lease_id, 1, 100, "d", &sign_hash);
        v["flow_rate"] = json!("42");
        let err = svc.audit_receipt(&v).unwrap_err();
        assert!(
            matches!(err, LicenseError::FieldWhitelistViolation(_)),
            "{err:?}"
        );
        // 整单拒收：账本无批次。
        assert_eq!(svc.ledger().batch_count().unwrap(), 0);
    }

    /// **回归（安全红线 1）**：对**已受理区间**伪造签名的回执必须被拒，
    /// **不得**借「与上次同区间」的幂等路径拿到 `accepted = true`。
    #[test]
    fn receipt_forged_for_seen_interval_is_rejected_not_short_circuited() {
        let svc = build_service();
        let (lease_id, mid) = activate_one(&svc);
        // 先让合法回执 [1,100] 入账（此后同区间命中幂等）。
        svc.audit_receipt(&receipt_value(&mid, &lease_id, 1, 100, "d", &sign_hash))
            .expect("legit first");
        assert_eq!(svc.ledger().batch_count().unwrap(), 1);

        // 同区间但用异钥签名 → 必须验签失败（而非幂等接受）。
        let forged = receipt_value(&mid, &lease_id, 1, 100, "d", &sign_hash_rogue);
        let err = svc.audit_receipt(&forged).unwrap_err();
        assert!(
            matches!(err, LicenseError::VerifyFailed(_)),
            "伪造回执必须验签失败，实际: {err:?}"
        );
        // 账本未被污染。
        assert_eq!(svc.ledger().batch_count().unwrap(), 1);
    }

    #[test]
    fn receipt_unknown_lease_returns_lease_not_found() {
        let svc = build_service();
        let v = receipt_value("MID-0001", "lease-missing", 1, 10, "d", &sign_hash);
        let err = svc.audit_receipt(&v).unwrap_err();
        assert!(matches!(err, LicenseError::LeaseNotFound(_)), "{err:?}");
    }

    // ================= 激活鉴权（验签 / ts 窗口 / nonce 防重放） =================

    /// 正例：合法 `req_sig` + 合法 nonce/ts → 激活成功；nonce 落防重放表、公钥被钉定。
    #[test]
    fn t_act_valid_request_activates_and_pins_pubkey_and_nonce() {
        let svc = build_service();
        let resp = svc
            .issue_codes(&issue_req("t-1", Some("MID-A"), "act-ok"))
            .unwrap();
        let code = &resp.codes[0];
        let req = activate_req(&code.code, "MID-A");
        let act = svc.activate(&req).expect("activation must succeed");
        assert!(!act.lease_token.is_empty());

        // nonce 已入全局防重放表。
        assert!(svc.store().get_nonce(&req.nonce).unwrap().is_some());
        // 设备公钥已钉定。
        let dev = device_of(&svc, "MID-A");
        assert_eq!(
            svc.store()
                .get_device_pubkey(&dev.device_id)
                .unwrap()
                .as_deref(),
            Some(req.device_pubkey.as_str())
        );
    }

    /// 错签名（异钥伪造）→ `ActivationSignatureInvalid`（401），且**零副作用**：
    /// 无设备、码未绑定、无审计、nonce 未入表。
    #[test]
    fn t_act_forged_signature_rejected_without_side_effects() {
        let svc = build_service();
        let resp = svc
            .issue_codes(&issue_req("t-1", Some("MID-A"), "act-forge"))
            .unwrap();
        let code = &resp.codes[0];

        let ts = now_unix_secs();
        let anchors: Vec<String> = vec!["a".to_string(); 5];
        let nonce = now_ns_id("n");
        let hash = crate::device_auth::activation_payload_hash(
            &code.code,
            "MID-A",
            &anchors,
            &device_pubkey_b64(&TEST_ONLY_DEVICE_SEED),
            &nonce,
            ts,
        );
        let req = ActivationRequest {
            activation_code: code.code.clone(),
            machine_code: "MID-A".to_string(),
            anchor_hashes: anchors,
            device_pubkey: device_pubkey_b64(&TEST_ONLY_DEVICE_SEED),
            nonce,
            ts: ts.to_string(),
            // 异钥（rogue）签名：公钥声明为真设备，签名却是另一把私钥 → 必须验签失败。
            req_sig: sign_hash_rogue(&hash),
        };
        // 审计基线：issue_codes 自身会写一条 issue 审计，拒绝路径不得再新增。
        let audit_before = count_audit(&svc);
        let err = svc.activate(&req).unwrap_err();
        assert!(
            matches!(err, LicenseError::ActivationSignatureInvalid(_)),
            "{err:?}"
        );
        assert_eq!(err.error_code(), crate::error::ERR_LICENSE_ACTIVATION_SIG);

        // 零副作用断言。
        assert_eq!(svc.store().count_devices(None).unwrap(), 0, "不得产生设备");
        let stored = svc.store().get_code_by_value(&code.code).unwrap().unwrap();
        assert_eq!(stored.status, CodeStatus::Issued, "码不得被绑定");
        assert!(stored.bound_device_id.is_none());
        assert!(svc.store().get_nonce(&req.nonce).unwrap().is_none());
        assert_eq!(count_audit(&svc), audit_before, "拒绝路径不得写审计");
    }

    /// 已钉定公钥不一致（异钥设备同码重激活）→ `ActivationPubkeyMismatch`（403），零副作用。
    #[test]
    fn t_act_pubkey_mismatch_on_pinned_device_rejected_without_side_effects() {
        let svc = build_service();
        let resp = svc
            .issue_codes(&issue_req("t-1", Some("MID-A"), "act-pk"))
            .unwrap();
        let code = &resp.codes[0];
        let first = svc
            .activate(&activate_req(&code.code, "MID-A"))
            .expect("first activation ok");

        // 异钥设备：签名自洽（rogue 私钥签 rogue 公钥），但公钥 ≠ 钉定值。
        let rogue_req = build_activation_req(
            &code.code,
            "MID-A",
            &vec!["a".to_string(); 5],
            &TEST_ONLY_ROGUE_SEED,
        );
        let dev = device_of(&svc, "MID-A");
        let audit_before = count_audit(&svc);
        let leases_before = svc
            .store()
            .list_leases_by_device(&dev.device_id)
            .unwrap()
            .len();

        let err = svc.activate(&rogue_req).unwrap_err();
        assert!(
            matches!(err, LicenseError::ActivationPubkeyMismatch(_)),
            "{err:?}"
        );
        assert_eq!(
            err.error_code(),
            crate::error::ERR_LICENSE_ACTIVATION_PUBKEY
        );

        // 零副作用：审计 / 租约 / 绑定均无新行，原租约仍有效。
        assert_eq!(count_audit(&svc), audit_before, "拒绝路径不得写审计");
        assert_eq!(
            svc.store()
                .list_leases_by_device(&dev.device_id)
                .unwrap()
                .len(),
            leases_before
        );
        assert_eq!(
            svc.store()
                .get_lease(&first.lease_id)
                .unwrap()
                .unwrap()
                .status,
            LeaseStatus::Active,
            "不得停既有租约"
        );
        assert!(svc.store().get_nonce(&rogue_req.nonce).unwrap().is_none());
    }

    /// 过期 ts / 未来 ts（±5min 窗外，签名自洽）→ `TimestampSkew`，零副作用。
    #[test]
    fn t_act_stale_and_future_ts_rejected_without_side_effects() {
        let svc = build_service();
        let resp = svc
            .issue_codes(&issue_req("t-1", Some("MID-A"), "act-ts"))
            .unwrap();
        let code = &resp.codes[0];
        let count_audit_before = count_audit(&svc);

        for offset in [-10_000i64, 10_000] {
            let ts = now_unix_secs() + offset;
            let anchors: Vec<String> = vec!["a".to_string(); 5];
            let key = SigningKey::from_bytes(&TEST_ONLY_DEVICE_SEED);
            let pubkey = B64.encode(key.verifying_key().to_bytes());
            let nonce = now_ns_id("n");
            let hash = crate::device_auth::activation_payload_hash(
                &code.code, "MID-A", &anchors, &pubkey, &nonce, ts,
            );
            let req = ActivationRequest {
                activation_code: code.code.clone(),
                machine_code: "MID-A".to_string(),
                anchor_hashes: anchors,
                device_pubkey: pubkey,
                nonce,
                ts: ts.to_string(),
                req_sig: B64.encode(key.sign(&hash).to_bytes()),
            };
            let err = svc.activate(&req).unwrap_err();
            assert!(matches!(err, LicenseError::TimestampSkew(_)), "{err:?}");
            assert!(svc.store().get_nonce(&req.nonce).unwrap().is_none());
        }
        assert_eq!(svc.store().count_devices(None).unwrap(), 0);
        assert_eq!(
            count_audit(&svc),
            count_audit_before,
            "拒绝路径不得新增审计"
        );
    }

    /// nonce 重放（同 nonce 二次提交）→ `NonceReplay`（409），零副作用。
    #[test]
    fn t_act_nonce_replay_rejected_without_side_effects() {
        let svc = build_service();
        let resp = svc
            .issue_codes(&issue_req("t-1", Some("MID-A"), "act-replay"))
            .unwrap();
        let code = &resp.codes[0];

        let mk = |nonce: &str| {
            let ts = now_unix_secs();
            let anchors: Vec<String> = vec!["a".to_string(); 5];
            let key = SigningKey::from_bytes(&TEST_ONLY_DEVICE_SEED);
            let pubkey = B64.encode(key.verifying_key().to_bytes());
            let hash = crate::device_auth::activation_payload_hash(
                &code.code, "MID-A", &anchors, &pubkey, nonce, ts,
            );
            ActivationRequest {
                activation_code: code.code.clone(),
                machine_code: "MID-A".to_string(),
                anchor_hashes: anchors,
                device_pubkey: pubkey,
                nonce: nonce.to_string(),
                ts: ts.to_string(),
                req_sig: B64.encode(key.sign(&hash).to_bytes()),
            }
        };

        let first = mk("n-replay-1");
        svc.activate(&first).expect("first activation ok");
        let dev = device_of(&svc, "MID-A");
        let leases_before = svc
            .store()
            .list_leases_by_device(&dev.device_id)
            .unwrap()
            .len();
        let audit_before = count_audit(&svc);

        let err = svc.activate(&mk("n-replay-1")).unwrap_err();
        assert!(matches!(err, LicenseError::NonceReplay(_)), "{err:?}");

        // 零副作用：幂等返回路径也不得因重放追加租约 / 审计。
        assert_eq!(
            svc.store()
                .list_leases_by_device(&dev.device_id)
                .unwrap()
                .len(),
            leases_before
        );
        assert_eq!(count_audit(&svc), audit_before);
    }

    /// nonce 是**全局**防重放：另一张码复用已用 nonce → 同样 `NonceReplay`。
    #[test]
    fn t_act_nonce_is_global_across_codes() {
        let svc = build_service();
        // 2026-09-27 契约：发放必填机器码——两张码必须预绑定不同机器。
        let r1 = svc
            .issue_codes(&issue_req("t-1", Some("MID-A"), "act-g1"))
            .unwrap();
        let r2 = svc
            .issue_codes(&issue_req("t-1", Some("MID-B"), "act-g2"))
            .unwrap();

        let mk = |code_value: &str, machine: &str, nonce: &str| {
            let ts = now_unix_secs();
            let anchors: Vec<String> = vec!["a".to_string(); 5];
            let key = SigningKey::from_bytes(&TEST_ONLY_DEVICE_SEED);
            let pubkey = B64.encode(key.verifying_key().to_bytes());
            let hash = crate::device_auth::activation_payload_hash(
                code_value, machine, &anchors, &pubkey, nonce, ts,
            );
            ActivationRequest {
                activation_code: code_value.to_string(),
                machine_code: machine.to_string(),
                anchor_hashes: anchors,
                device_pubkey: pubkey,
                nonce: nonce.to_string(),
                ts: ts.to_string(),
                req_sig: B64.encode(key.sign(&hash).to_bytes()),
            }
        };

        svc.activate(&mk(&r1.codes[0].code, "MID-A", "n-global-1"))
            .expect("first activation ok");
        let err = svc
            .activate(&mk(&r2.codes[0].code, "MID-B", "n-global-1"))
            .unwrap_err();
        assert!(matches!(err, LicenseError::NonceReplay(_)), "{err:?}");

        // 第二张码仍未绑定。
        let stored = svc
            .store()
            .get_code_by_value(&r2.codes[0].code)
            .unwrap()
            .unwrap();
        assert_eq!(stored.status, CodeStatus::Issued);
    }

    /// 字段格式非法：空 / 超长 nonce，空 / 非 base64 / 错长 sig，畸形公钥 → 拒绝且零副作用。
    #[test]
    fn t_act_malformed_fields_rejected_without_side_effects() {
        let svc = build_service();
        let resp = svc
            .issue_codes(&issue_req("t-1", Some("MID-A"), "act-malformed"))
            .unwrap();
        let code = &resp.codes[0];

        let ts = now_unix_secs();
        let pubkey = device_pubkey_b64(&TEST_ONLY_DEVICE_SEED);
        let anchors: Vec<String> = vec!["a".to_string(); 5];
        let audit_before = count_audit(&svc);
        let mk = |nonce: String, sig: String, pk: String| ActivationRequest {
            activation_code: code.code.clone(),
            machine_code: "MID-A".to_string(),
            anchor_hashes: anchors.clone(),
            device_pubkey: pk,
            nonce,
            ts: ts.to_string(),
            req_sig: sig,
        };

        // 空 / 全空白 nonce → FieldWhitelistViolation。
        let err = svc
            .activate(&mk("   ".to_string(), "x".to_string(), pubkey.clone()))
            .unwrap_err();
        assert!(
            matches!(err, LicenseError::FieldWhitelistViolation(_)),
            "{err:?}"
        );
        // 超长 nonce（>128 字节）→ FieldWhitelistViolation。
        let err = svc
            .activate(&mk("n".repeat(129), "x".to_string(), pubkey.clone()))
            .unwrap_err();
        assert!(
            matches!(err, LicenseError::FieldWhitelistViolation(_)),
            "{err:?}"
        );
        // 空 sig / 非 base64 sig / 长度非 64 sig → ActivationSignatureInvalid。
        for bad_sig in [
            String::new(),
            "   ".to_string(),
            "!!!not b64!!!".to_string(),
            B64.encode([0u8; 63]),
            B64.encode([0u8; 65]),
        ] {
            let err = svc
                .activate(&mk(now_ns_id("n"), bad_sig, pubkey.clone()))
                .unwrap_err();
            assert!(
                matches!(err, LicenseError::ActivationSignatureInvalid(_)),
                "malformed sig must be rejected: {err:?}"
            );
        }
        // 畸形公钥（非 base64 / 长度非 32）→ ActivationSignatureInvalid。
        for bad_pk in [
            "not-b64".to_string(),
            B64.encode([0u8; 31]),
            B64.encode([0u8; 33]),
        ] {
            let err = svc
                .activate(&mk(now_ns_id("n"), "x".to_string(), bad_pk))
                .unwrap_err();
            assert!(
                matches!(err, LicenseError::ActivationSignatureInvalid(_)),
                "malformed pubkey must be rejected: {err:?}"
            );
        }

        // 零副作用：无设备、码未绑定、无审计新增、无 nonce 行。
        assert_eq!(svc.store().count_devices(None).unwrap(), 0);
        assert_eq!(count_audit(&svc), audit_before, "拒绝路径不得新增审计");
        let stored = svc.store().get_code_by_value(&code.code).unwrap().unwrap();
        assert_eq!(stored.status, CodeStatus::Issued);
        assert!(stored.bound_device_id.is_none());
    }

    /// 网关端 `repo.ts` 激活码正则的**逐字符镜像**（测试侧不引入 regex 依赖）。
    ///
    /// 对应 `^IOT-\d{4}-[A-Z2-9AC-HJ-NP-Z]{4}-[A-Z2-9AC-HJ-NP-Z]{4}-[A-Z2-9AC-HJ-NP-Z]{4}-[A-Z2-9AC-HJ-NP-Z]{2}$`。
    fn gateway_code_shape_ok(code: &str) -> bool {
        const ALPHABET: &str = "ACDEFGHJKLMNPQRSTUVWXYZ23456789";
        let mut segs = code.split('-');
        match (
            segs.next(),
            segs.next(),
            segs.next(),
            segs.next(),
            segs.next(),
            segs.next(),
        ) {
            (Some("IOT"), Some(year), Some(a), Some(b), Some(c), Some(tail)) => {
                year.len() == 4
                    && year.chars().all(|ch| ch.is_ascii_digit())
                    && [a, b, c].iter().all(|seg| seg.len() == 4)
                    && tail.len() == 2
                    && [a, b, c, tail]
                        .iter()
                        .all(|seg| !seg.is_empty() && seg.chars().all(|ch| ALPHABET.contains(ch)))
            }
            _ => false,
        }
    }

    /// **激活码格式契约**（2026-09-27）：发放返回的码必须精确匹配网关端口径
    /// `IOT-2026-XXXX-XXXX-XXXX-XX`，且字符集剔除易混的 `0` / `1` / `I` / `O`。
    #[test]
    fn issued_code_value_matches_gateway_format() {
        let year = current_year(now_unix_secs());
        for _ in 0..256 {
            let code = LicensingService::generate_code_value();
            assert!(gateway_code_shape_ok(&code), "码不符合网关格式: {code}");
            // 易混字符检查只看 payload（前缀 `IOT-` 自带 `I`，年份含 `0`）。
            let payload = &code[code.len() - 14..];
            assert!(
                !payload.contains(['0', '1', 'I', 'O']),
                "码含易混字符: {code}"
            );
            assert!(
                code.starts_with(&format!("IOT-{year:04}-")),
                "年份前缀错: {code}"
            );
        }
    }

    /// **端到端**：`issue_codes` 真正落库并返回的码值即新格式（发放 → 激活取码同一口径）。
    #[test]
    fn issue_codes_returns_gateway_format_codes() {
        let svc = build_service();
        let resp = svc
            .issue_codes(&IssueCodesRequest {
                tenant_id: "t-1".to_string(),
                count: 5,
                tier: "pro".to_string(),
                valid_from: (now_unix_secs() - 1000).to_string(),
                valid_until: (now_unix_secs() + 365 * 86_400).to_string(),
                prebind_machine_code: Some("M1".to_string()),
                idempotency_key: "fmt-check".to_string(),
            })
            .unwrap();
        assert_eq!(resp.codes.len(), 5);
        for issued in &resp.codes {
            assert!(
                gateway_code_shape_ok(&issued.code),
                "发放码不符合格式: {}",
                issued.code
            );
            // 落库值必须与返回一致（激活按码值查库）。
            let stored = svc
                .store()
                .get_code_by_value(&issued.code)
                .unwrap()
                .unwrap_or_else(|| panic!("码未落库: {}", issued.code));
            assert_eq!(stored.code, issued.code);
        }
    }
}

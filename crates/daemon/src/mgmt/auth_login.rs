//! task 57 全量接线 — 管理面登录 / JWT 签发 / whoami（凭证两路 fail-closed）。
//!
//! ## 凭证两路（fail-closed）
//! - **生产路**：`config.toml` 顶层可选 `[mgmt_auth]` 段
//!   （`users = [{name, role, password_hash}]`，`password_hash` 为
//!   **Argon2id 的 PHC 串**（`$argon2id$v=19$m=19456,t=2,p=1$<b64salt>$<b64hash>`）；
//!   比对走 `password-hash` 的 `PasswordVerifier::verify_password`）。角色字面量经
//!   [`super::rbac::Role::from_str`] 解析，未知角色该账号**跳过**（fail-closed）。
//! - **口令哈希格式与迁移**（安全基线）：历史上存储的是**无盐单轮 SHA-256** 的 hex，
//!   可被引弱口令字典直接反查。现改为 `Argon2id`（参数见 [`ARGON2_M_COST`] 等常量）：
//!   旧格式条目**继续可登录**（向后兼容，不锁死生产），且登录成功的那一刻立即用
//!   Argon2id 重新哈希 → 回写账号表 + 配置（备份 + 原子写 + 快照热替换），
//!   此后该账号走新路径；回写失败（如未装配 config 路径）只告警，旧格式仍可用。
//! - **开发路（仅本地）**：`mgmt_auth` **未配置** 且设置了
//!   `IOT_DAQ_DEV_ADMIN_PASS` 环境变量时，启用 dev 管理员
//!   （username = [`DEV_ADMIN_USER`]，role = `system`）。**生产环境禁用**。
//! - 两路皆空 → 登录端点全拒（401），启动时 warn 提示。
//!
//! ## JWT 密钥
//! `IOT_DAQ_JWT_SECRET`（64 个 hex 字符 = 32 字节）→ [`IssuerKey`]；
//! 未提供 / 非法 → 回退内置 dev 常量密钥并 warn（**仅限本地**，生产必须配置）。
//!
//! ## 安全红线
//! - 未知用户 / 错误密码 / 请求体非法一律**同一 401 响应**（不区分，防账号枚举）；
//!   无匹配用户时也对固定哑哈希做一次恒时比对，抹平用户枚举时序差。
//! - 密码摘要比对为恒时（XOR 折叠）；摘要只在内存中计算，明文不落盘不落日志。
//! - **口令绝不以裸摘要形态存储**：新写入一律 Argon2id（每账号独立随机盐）；
//!   历史裸 sha256 条目只在「登录成功瞬间」被就地升级，不设静默绕过。
//! - token TTL = [`TOKEN_TTL_SECS`]（1h）；`jti` 用 uuid v4。
//! - **大数红线**：本模块自产响应体中整数（`exp`）一律字符串编码；
//!   JWT claims 内的 `exp`/`iat` 按 RFC 7519 NumericDate（标准互操作字段，例外）。

use argon2::{
    password_hash::{rand_core::OsRng, SaltString},
    Algorithm, Argon2, Params, PasswordHash, PasswordHasher, PasswordVerifier, Version,
};
use serde::Deserialize;
use sha2::{Digest, Sha256};

use axum::body::Bytes;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Json, Response};

use super::auth_jwt::{now_unix_secs, sign, Claims, IssuerKey, JwtError};
use super::rbac::{permissions_of, AuthedRole, Permission, RbacAuth, Role};
use crate::config::GatewayConfig;

/// JWT 签名密钥环境变量（64 个 hex 字符 = 32 字节）。
pub const JWT_SECRET_ENV: &str = "IOT_DAQ_JWT_SECRET";
/// dev 管理员密码环境变量（**生产禁用**；仅在 `mgmt_auth` 未配置时生效）。
pub const DEV_ADMIN_PASS_ENV: &str = "IOT_DAQ_DEV_ADMIN_PASS";
/// dev 管理员用户名。
pub const DEV_ADMIN_USER: &str = "dev";
/// 登录签发 token 的有效期（秒）。
pub const TOKEN_TTL_SECS: i64 = 3600;

/// 开发兜底签名密钥（固定 32 字节；**仅限本地开发**——生产必须提供
/// `IOT_DAQ_JWT_SECRET`，回退时启动日志会打 warn 提醒）。
const DEV_FALLBACK_KEY: IssuerKey = IssuerKey([0x1Du8; 32]);

/// 无匹配用户时参与恒时比对的哑哈希输入（抹平用户枚举时序差）。
const DUMMY_HASH_INPUT: &[u8] = b"iot-daq-dummy-user";

// ---- 凭证存储 ----

/// Argon2id 内存代价：19 456 KiB ≈ 19 MiB。
///
/// 选型理由：`m` 决定抗 GPU/ASIC 的**内存墙**，是 Argon2 三项参数里唯一无法靠
/// 算力堆砌绕过的一项。19 MiB 落在 RFC 9106「交互式（interactive）推荐区间」
/// 上沿，单条哈希在 2 核 x86 上约数十毫秒——对管理面登录（每秒个位数量级）
/// 完全够用，却能让离线字典攻击必须为该网关单独准备 19 MiB/词条的显存。
const ARGON2_M_COST: u32 = 19_456;
/// Argon2id 迭代轮数：2。
///
/// 选型理由：RFC 9106 交互式基线。配合 19 MiB 内存墙，2 轮把 CPU 时间成本也拉到
/// 与内存成本同量级，逼近 Argon2 在该内存下的最低可行代价（t=1 时抗分解攻击偏弱）。
const ARGON2_T_COST: u32 = 2;
/// Argon2id 并行车道数：1。
///
/// 选型理由：固定为 1，把代价压在「内存 × 轮数」上；车道数只影响 GPU 并行度，
/// 生产环境不存在为登录端点预留多车道吞吐的场景。
const ARGON2_P_COST: u32 = 1;

/// 存储形态：新 / 旧两种口令摘要编码，二者共存于同一个账号表。
#[derive(Debug, Clone)]
enum PasswordRef {
    /// **当前格式**：Argon2id 的 PHC 字符串（`password-hash` 序列化结果）。
    Phc(String),
    /// **遗留格式**：`SHA-256(password)` 的 hex（`PasswordRef::is_legacy_hex` 判定）。
    ///
    /// 仅用于**读取历史存量配置**：它们照样能登录，但一登录成功就会在
    /// [`MgmtAuth::login`] 里被就地换成 [`PasswordRef::Phc`]。
    LegacyHex(String),
}

impl PasswordRef {
    /// 是否遗留（无盐 SHA-256）格式——决定登录成功后是否需要升级。
    fn is_legacy(&self) -> bool {
        matches!(self, Self::LegacyHex(_))
    }
}

/// 单个登录账号（口令只存摘要，明文永不落盘）。
#[derive(Debug, Clone)]
pub struct LoginUser {
    /// 用户名（精确匹配）。
    pub name: String,
    /// 角色 id 字面量（内置四角色之一，或自定义角色 id；进 JWT `role` claim）。
    pub role_id: String,
    /// 解析后的权限集（B-3：登录签发时随 token 携带，签名防篡改；
    /// 内置角色 = 现有矩阵映射零变化，自定义角色 = permissions 并集）。
    pub perms: Vec<Permission>,
    /// 口令摘要的存储形态（[`PasswordRef`]；模块私有，外部只经 [`MgmtAuth`] 判定）。
    password: PasswordRef,
}

/// 管理面登录器：账号表 + JWT 签发密钥（注入式，不做 IO）。
///
/// `key` 与 `users` 分离：配置热重载（bootstrap 落盘 / 手动改 `config.toml`）时
/// 只允许重建 `users`，签名密钥保持启动时的取值——否则一次热重载就会让已签发的
/// token 全部失效（登录成功的用户被踢下线）。
#[derive(Debug, Clone)]
pub struct MgmtAuth {
    key: IssuerKey,
    users: Vec<LoginUser>,
    /// dev 管理员口令（`None` = 未启用；仅在生产路零账号时补位，见
    /// [`Self::sync_users`]——**生产禁用**）。记录它以让热重载后判定规则不变。
    dev_pass: Option<String>,
    /// 本次 [`Self::login`] 中「遗留格式校验成功」的账号 → `(用户名, 新 PHC 串)`。
    ///
    /// 登录处理器据此把新格式回写（配置备份 + 原子写 + 快照热替换），把一次性迁移
    /// 对外可见；[`Self::take_pending_migration`] 取走即清空（幂等）。
    pending_migration: Option<(String, String)>,
}

impl MgmtAuth {
    /// 以指定签名密钥构造（无任何账号 → 登录全拒，fail-closed）。
    pub fn new(key: IssuerKey) -> Self {
        Self {
            key,
            users: Vec::new(),
            dev_pass: None,
            pending_migration: None,
        }
    }

    /// 追加一个账号（`stored` 为口令摘要的**存储串**：Argon2id 的 PHC 串，或历史
    /// 遗留的 `SHA-256(password)` hex；两者皆非法 / 空用户名 → warn 跳过该账号，
    /// 绝不放入弱凭证）。`role_id` = 角色 id 字面量；`perms` = 解析后的权限集。
    ///
    /// 返回是否真的装入（调用方据此统计「可用账号数」）。
    pub fn with_user(
        mut self,
        name: &str,
        role_id: &str,
        perms: Vec<Permission>,
        stored: &str,
    ) -> Self {
        self.add_user(name, role_id, perms, stored);
        self
    }

    /// 追加 dev 管理员（role = system；**仅限本地开发，生产禁用**）。
    ///
    /// 同时记住口令，使 [`Self::sync_users`] 能在热重载后按同一规则补回。
    /// 口令摘要按**新格式**（Argon2id）落账——dev 账号不落 config，但同样不留裸摘要。
    pub fn with_dev_admin(mut self, password: &str) -> Self {
        self.dev_pass = Some(password.to_string());
        self.users.push(LoginUser {
            name: DEV_ADMIN_USER.to_string(),
            role_id: Role::System.as_str().to_string(),
            perms: permissions_of(Role::System).to_vec(),
            password: phc_or_legacy(password),
        });
        self
    }

    /// 按配置快照重建账号表（**只换 `users`，绝不动签名密钥**）。
    ///
    /// 用途：bootstrap 落盘 / `config.toml` 热重载之后，让登录判定跟随最新配置。
    /// 规则与启动时 [`build`] 完全一致：停用 / 角色解析失败 / 非法摘要的账号跳过
    /// （fail-closed）；生产路零可用账号且已启用 dev 管理员时补回 dev 账号。
    /// 返回可用账号数。
    pub fn sync_users(&mut self, config: &GatewayConfig) -> usize {
        self.users.clear();
        let mut loaded = 0usize;
        if let Some(section) = &config.mgmt_auth {
            for user in &section.users {
                // 停用账号 fail-closed：不进登录判定表（与「角色解析失败跳过」同口径）。
                if user.status.as_deref() == Some("disabled") {
                    tracing::warn!(
                        user = %user.name,
                        "mgmt auth: account disabled; user skipped (fail-closed)"
                    );
                    continue;
                }
                match resolve_user_role(section, &user.role) {
                    Some((role_id, perms)) => {
                        if self.add_user(&user.name, &role_id, perms, &user.password_hash) {
                            loaded += 1;
                        }
                    }
                    None => tracing::warn!(
                        user = %user.name,
                        role = %user.role,
                        "mgmt auth: role not resolvable (unknown builtin literal, undefined \
                         custom role, or custom role with unknown permission id); \
                         user skipped (fail-closed)"
                    ),
                }
            }
        }
        if loaded == 0 {
            if let Some(pass) = self.dev_pass.as_deref() {
                self.users.push(LoginUser {
                    name: DEV_ADMIN_USER.to_string(),
                    role_id: Role::System.as_str().to_string(),
                    perms: permissions_of(Role::System).to_vec(),
                    password: phc_or_legacy(pass),
                });
            }
        }
        loaded
    }

    /// 装入单条账号；非法条目 warn 跳过并返回 `false`（绝不放入弱凭证）。
    ///
    /// 接受两种存储串（[`PasswordRef`]）：Argon2id 的 PHC 串，或遗留的
    /// `SHA-256(password)` hex；两者都不是 → 视为弱凭证，跳过。
    fn add_user(
        &mut self,
        name: &str,
        role_id: &str,
        perms: Vec<Permission>,
        stored: &str,
    ) -> bool {
        let password = parse_password_ref(stored);
        match password {
            Some(password) if !name.trim().is_empty() => {
                self.users.push(LoginUser {
                    name: name.trim().to_string(),
                    role_id: role_id.to_string(),
                    perms,
                    password,
                });
                true
            }
            _ => {
                tracing::warn!(
                    user = %name,
                    "mgmt auth: invalid password_hash (must be an Argon2id PHC string \
                     or the legacy SHA-256 hex) or empty name; user skipped (fail-closed)"
                );
                false
            }
        }
    }

    /// 签名密钥（供测试与手动校验路径）。
    pub fn key(&self) -> IssuerKey {
        self.key
    }

    /// 已装载的账号数（0 = 登录全拒）。
    pub fn user_count(&self) -> usize {
        self.users.len()
    }

    /// 仍是**遗留（无盐 SHA-256）**格式的账号数（> 0 会在启动时 warn 提示迁移）。
    ///
    /// 迁移不阻塞登录：这些账号能正常登录，登录成功的那一刻会被就地升级。
    pub fn legacy_hash_count(&self) -> usize {
        self.users.iter().filter(|u| u.password.is_legacy()).count()
    }

    /// 取走本次登录产生的「遗留格式 → Argon2id」升级结果（取走即清空，幂等）。
    ///
    /// 返回 `(用户名, 新 PHC 串)`；调用方负责回写。
    pub fn take_pending_migration(&mut self) -> Option<(String, String)> {
        self.pending_migration.take()
    }

    /// 登录判定：口令校验（Argon2id → 遗留 SHA-256 回退）→ 签发 JWT。
    ///
    /// 成功返回 `(token, role_id)`（role_id = 角色 id 字面量，内置或自定义）；
    /// 用户不存在 / 密码错 / 签发失败一律 `None`（调用方统一转 401，不区分原因）。
    ///
    /// ## 遗留格式升级（就地、幂等）
    /// 若命中账号存的还是无盐 `SHA-256(password)`，且口令校验通过，则**立即**以
    /// Argon2id（独立随机盐）重新哈希：
    /// - 写回**本进程账号表**（本次请求后续签发走新格式）；
    /// - 记入 [`Self::pending_migration`]，由调用方回写配置（见
    ///   [`persist_password_migration`]）——这一步失败只告警，账号照旧可登录，
    ///   下次登录会再试一次。
    pub fn login(&mut self, username: &str, password: &str) -> Option<(String, String)> {
        let matched = self.users.iter().position(|u| u.name == username);
        let verified = match matched {
            Some(index) => verify_password(&self.users[index], password),
            // 无匹配用户：对固定哑哈希做一次恒时比对（时长与命中路径对齐），
            // 结果必然是不通过——与「口令错」同路径返回，不泄漏用户是否存在。
            None => {
                let _ = ct_eq(
                    &sha256_bytes(password.as_bytes()),
                    &sha256_bytes(DUMMY_HASH_INPUT),
                );
                Verify::Reject
            }
        };
        if !matches!(verified, Verify::Accepted | Verify::AcceptedLegacy) {
            return None;
        }
        let index = matched?;
        if let Verify::AcceptedLegacy = verified {
            // 就地升级：本进程账号表先换成 Argon2id（本次签发即走新格式），
            // 并登记待回写的 (用户名, 新 PHC 串)。
            if let Some(phc) = hash_phc(password) {
                if let Some(name) = self.users.get(index).map(|u| u.name.clone()) {
                    self.users[index].password = PasswordRef::Phc(phc.clone());
                    self.pending_migration = Some((name, phc));
                }
            }
        }
        let user = &self.users[index];
        let (token, _) = self
            .issue_token(&user.name, &user.role_id, user.perms.clone())
            .ok()?;
        Some((token, user.role_id.clone()))
    }

    /// 以给定主体 / 角色 id / 权限集签发 JWT（`exp = now + TOKEN_TTL_SECS`，
    /// `jti` = uuid v4）。`perms` 随 token 签名携带（B-3 防篡改授权源）。
    ///
    /// 返回 `(token, exp)`；`exp` 供测试断言与日志取用。
    pub fn issue_token(
        &self,
        sub: &str,
        role_id: &str,
        perms: Vec<Permission>,
    ) -> Result<(String, i64), JwtError> {
        let now = now_unix_secs();
        let exp = now.saturating_add(TOKEN_TTL_SECS);
        let claims = Claims {
            sub: sub.to_string(),
            role: role_id.to_string(),
            perms: Some(perms),
            exp,
            iat: now,
            nbf: None,
            jti: uuid::Uuid::new_v4().to_string(),
        };
        let token = sign(&claims, self.key)?;
        Ok((token, exp))
    }
}

// ---- 密钥 / 凭证装配 ----

/// 解析 JWT 签名密钥：`IOT_DAQ_JWT_SECRET`（64 hex）→ 成功返回
/// `(key, false)`；未提供或非法 → 回退 dev 常量密钥并 warn（返回 `(key, true)`）。
pub fn resolve_issuer_key(env: &dyn Fn(&str) -> Option<String>) -> (IssuerKey, bool) {
    if let Some(raw) = env(JWT_SECRET_ENV) {
        let decoded = hex::decode(raw.trim())
            .ok()
            .and_then(|b| IssuerKey::from_slice(&b));
        if let Some(key) = decoded {
            return (key, false);
        }
        tracing::warn!(
            "mgmt auth: {JWT_SECRET_ENV} invalid (need 64 hex chars = 32 bytes); \
             falling back to built-in dev key (LOCAL ONLY)"
        );
        return (DEV_FALLBACK_KEY, true);
    }
    tracing::warn!(
        "mgmt auth: {JWT_SECRET_ENV} not set; using built-in dev signing key \
         (LOCAL ONLY — production must configure a 32-byte hex secret)"
    );
    (DEV_FALLBACK_KEY, true)
}

/// 装配管理面鉴权（生产入口；`env` 注入读取环境变量的闭包，测试可注入受控值）。
///
/// 返回 `(RbacAuth（JWT 校验用）, MgmtAuth（登录签发用）)`，二者共享同一密钥。
pub fn build(config: &GatewayConfig, env: &dyn Fn(&str) -> Option<String>) -> (RbacAuth, MgmtAuth) {
    let (key, dev_key) = resolve_issuer_key(env);
    let mut login = MgmtAuth::new(key);

    // 生产路：config `[mgmt_auth]` 账号表（停用 / 角色解析失败 / 非法哈希的
    // 账号跳过，fail-closed）。
    let mut loaded = 0usize;
    if let Some(section) = &config.mgmt_auth {
        for user in &section.users {
            match resolve_user_role(section, &user.role) {
                Some((role_id, perms)) => {
                    let before = login.user_count();
                    login = login.with_user(&user.name, &role_id, perms, &user.password_hash);
                    if login.user_count() > before {
                        loaded += 1;
                    }
                }
                None => tracing::warn!(
                    user = %user.name,
                    role = %user.role,
                    "mgmt auth: role not resolvable (unknown builtin literal, undefined custom \
                     role, or custom role with unknown permission id); user skipped (fail-closed)"
                ),
            }
        }
    }

    // 开发路：生产路零账号 且设置了 dev 管理员密码 → dev 管理员（**生产禁用**）。
    if loaded == 0 {
        let dev_pass = env(DEV_ADMIN_PASS_ENV).filter(|p| !p.is_empty());
        if let Some(pass) = dev_pass {
            tracing::warn!(
                "[WARN] dev admin enabled: mgmt_auth 未配置且 {DEV_ADMIN_PASS_ENV} 已设置 \
                 —— 仅限本地开发调试，生产环境禁用"
            );
            login = login.with_dev_admin(&pass);
        } else {
            tracing::warn!(
                "mgmt auth: no login credentials configured (mgmt_auth absent, \
                 {DEV_ADMIN_PASS_ENV} unset); /api/auth/login is fail-closed (all logins rejected)"
            );
        }
    }

    // 遗留（无盐 SHA-256）口令哈希：不阻塞登录，但必须让人看见「还没迁完」。
    let legacy = login.legacy_hash_count();
    if legacy > 0 {
        tracing::warn!(
            count = legacy,
            "mgmt auth: {legacy} account(s) still store the legacy unsalted SHA-256 hash; \
             they keep working and are upgraded to Argon2id automatically on first successful login"
        );
    }

    // 密钥兜底提示放在最后，保证日志里两条 warn 都可见。
    if dev_key {
        tracing::warn!("mgmt auth: dev JWT signing key in use (LOCAL ONLY)");
    }

    (RbacAuth::new(key), login)
}

// ---- B-3：角色 → 权限集解析（登录装配唯一入口） ----

/// 把配置账号的 `role` 字段解析为 `(role_id, 权限集)`（B-3 单一解析入口；
/// `sync_users` / `build` 共用）。
///
/// - **内置角色字面量** → 现有矩阵映射（`permissions_of`）——回归零变化；
/// - **自定义角色 id** → 在 `[mgmt_auth.roles]` 中定位，permissions 逐项经
///   `Permission::from_id` 解析成并集（`accounts_api` 建角色时已校验合法 id，
///   手改配置出现未知 id → 整个账号解析失败——fail-closed，绝不「半解析」放行）；
/// - 其余（未知字面量 / 未定义的自定义角色 id）→ `None`，调用方跳过该账号。
pub(crate) fn resolve_user_role(
    section: &crate::config::MgmtAuthSection,
    role_raw: &str,
) -> Option<(String, Vec<Permission>)> {
    if let Some(role) = Role::from_str(role_raw) {
        return Some((role.as_str().to_string(), permissions_of(role).to_vec()));
    }
    let custom = section.roles.iter().find(|r| r.id == role_raw)?;
    let mut perms: Vec<Permission> = Vec::with_capacity(custom.permissions.len());
    for id in &custom.permissions {
        let permission = Permission::from_id(id)?;
        if !perms.contains(&permission) {
            perms.push(permission);
        }
    }
    Some((custom.id.clone(), perms))
}

// ---- 摘要、校验与恒时比较 ----

/// 口令校验结果（三态：新格式通过 / 遗留格式通过 / 不通过）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Verify {
    /// Argon2id（PHC 串）校验通过。
    Accepted,
    /// 遗留无盐 SHA-256 校验通过 —— 调用方应就地升级。
    AcceptedLegacy,
    /// 校验失败（口令错 / 存储串无法解析）。
    Reject,
}

/// Argon2id 参数实例（[`ARGON2_M_COST`] / [`ARGON2_T_COST`] / [`ARGON2_P_COST`] 见选型注释）。
///
/// 用 `Params::new` 显式构造（而非 `Params::DEFAULT`）是为了让参数在代码里可读可审计；
/// 本组常量必然合法，万一构造失败退回同一组默认值，保持「无 unwrap / 无 panic」。
fn argon2() -> Argon2<'static> {
    Argon2::new(
        Algorithm::Argon2id,
        Version::V0x13, // Argon2 版本 0x13 = 19（当前唯一有安全实践的版本）
        Params::new(ARGON2_M_COST, ARGON2_T_COST, ARGON2_P_COST, None).unwrap_or(Params::DEFAULT),
    )
}

/// 口令 → Argon2id PHC 串（每次调用生成**新的 16 字节随机盐**，故同口令两次哈希必异）。
///
/// 失败（理论上不可达）返回 `None`，调用方自行降级——生产代码里不放 `unwrap`。
/// `pub(crate)`：账号管理写端点（`accounts_api`）建号 / 重置口令共用同一哈希参数。
pub(crate) fn hash_phc(password: &str) -> Option<String> {
    let salt = SaltString::generate(OsRng);
    argon2()
        .hash_password(password.as_bytes(), &salt)
        .ok()
        .map(|hashed| hashed.to_string())
}

/// 口令 → 「新格式存储串」（Argon2id）。
///
/// 由明文口令直接建号（dev 管理员 / 首次 bootstrap 前的内存态）时用它落账：
/// 哈希不可达（理论上不可达，盐只需 CSPRNG）时退回遗留摘要，**绝不因为算不出
/// 哈希就把刚建出来的账号丢掉**。
fn phc_or_legacy(password: &str) -> PasswordRef {
    match hash_phc(password) {
        Some(phc) => PasswordRef::Phc(phc),
        None => PasswordRef::LegacyHex(hex::encode(sha256_bytes(password.as_bytes()))),
    }
}

/// 解析存储串的两种形态；两者都不是 → `None`（视为弱凭证，装配时跳过该账号）。
fn parse_password_ref(stored: &str) -> Option<PasswordRef> {
    let trimmed = stored.trim();
    // 空串 `hex::decode` 也能成功（空摘要）——必须在这一层挡掉：装进账号表的条目会被
    // 计入「已初始化」，一个永远无法登录的空摘要账号等于把系统锁死（fail-closed 取反）。
    if trimmed.is_empty() {
        return None;
    }
    // PHC 串以 `$` 开头（`password-hash` 的解析契约）——先看它能否解析，避免把
    // 64 位 hex 误当 PHC；解析通过则**原样保留存储串**（回写时字节一致）。
    if PasswordHash::new(trimmed).is_ok() {
        return Some(PasswordRef::Phc(trimmed.to_string()));
    }
    // 遗留格式：`SHA-256(password)` 的 hex。
    hex::decode(trimmed)
        .ok()
        .map(|bytes| PasswordRef::LegacyHex(hex::encode(bytes)))
}

/// 校验账号存储串与待验口令是否匹配（新格式优先，失败再回退遗留 SHA-256）。
fn verify_password(user: &LoginUser, password: &str) -> Verify {
    match &user.password {
        PasswordRef::Phc(phc) => {
            match PasswordHash::new(phc) {
                Ok(parsed) => {
                    if argon2()
                        .verify_password(password.as_bytes(), &parsed)
                        .is_ok()
                    {
                        return Verify::Accepted;
                    }
                }
                // 存储串被外部改坏（非 PHC）→ 直接拒绝，不给回退补偿。
                Err(_) => return Verify::Reject,
            }
            Verify::Reject
        }
        PasswordRef::LegacyHex(legacy_hex) => {
            // 遗留路径：无盐 SHA-256 + 恒时比较。命中只代表「这一段历史存量可用」，
            // 登录成功后 [MgmtAuth::login] 会把它升级掉。
            match hex::decode(legacy_hex) {
                Ok(stored) if ct_eq(&sha256_bytes(password.as_bytes()), &stored) => {
                    Verify::AcceptedLegacy
                }
                _ => Verify::Reject,
            }
        }
    }
}

/// SHA-256 摘要字节。
fn sha256_bytes(data: &[u8]) -> Vec<u8> {
    let mut hasher = Sha256::new();
    hasher.update(data);
    hasher.finalize().to_vec()
}

/// 恒时字节比较（XOR 折叠，无提前退出；长度差异仅泄漏摘要定长信息——
/// 双方均为 SHA-256 的 32 字节输出，无秘密可泄漏）。
fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

// ---- 热重载感知的凭证判定 ----

/// 当前**生效**的登录判定表（账号表按配置快照重建；**签名密钥不动**）。
///
/// 直接读 `MgmtState::login_auth()` 拿到的只是**启动时**装配的那份账号表：
/// bootstrap 落盘或 `config.toml` 热重载后，新账号并不会自动回灌进去，于是
/// 「bootstrap 建好号 → 回登录页 401」（P0：首次安装入口实际不可用）。
///
/// 这里每次取用都按最新配置快照重建 `users`，但克隆的是同一把签名密钥——
/// 已签发的 token 不会因热重载失效（登录中的用户不会被踢下线）。
pub(super) fn live_login_auth(state: &super::MgmtState) -> MgmtAuth {
    let mut live = state.login_auth().clone();
    live.sync_users(&state.config());
    live
}

/// 把登录时升级出来的 Argon2id 摘要回写到配置：备份 + 原子写（`GatewayConfig::save`）
/// + 快照热替换（`ConfigShared::replace`，热重载Watcher 同步读到新格式）。
///
/// 只改那一条 `password_hash`，其余配置原样保留（`state.config()` 是最新快照的克隆）。
/// 失败 / 未装配 config 路径（单测、无落盘目标）→ 仅 warn 日志：内存态账号表**已经**
/// 升级，登录结果不受影响，下一次登录会再试一次回写。
fn persist_password_migration(state: &super::MgmtState, user: &str, phc: &str) {
    // 与所有配置写（bootstrap / 设备 / 点位）共用同一把写锁：避免「取快照 → 落盘」
    // 之间被另一条写路径插入、反过来覆盖掉它。本函数全程无 `await`，持锁不会卡住执行器。
    let _guard = super::writeapi::write_guard();
    let Some(path) = state.config_path() else {
        tracing::warn!(
            user = %user,
            "auth: Argon2id migration kept in memory only (no config path bound); \
             it will be retried on the next successful login"
        );
        return;
    };
    let mut next = (*state.config()).clone();
    let Some(section) = next.mgmt_auth.as_mut() else {
        return;
    };
    let Some(target) = section.users.iter_mut().find(|u| u.name == user) else {
        return;
    };
    if target.password_hash == phc {
        return; // 已是新格式（并发登录的另一路刚写完）→ 幂等返回
    }
    target.password_hash = phc.to_string();
    if let Err(err) = next.save(&path) {
        tracing::warn!(
            user = %user,
            error = %err,
            "auth: Argon2id migration failed to persist; legacy digest stays usable"
        );
        return;
    }
    let version = state.daemon().config_shared().replace(next);
    tracing::info!(
        user = %user,
        config_version = version,
        "auth: legacy password hash upgraded to Argon2id (migrated on successful login)"
    );
}

/// 登录成功 → 回写 `last_login_at_ms`（Unix 毫秒；账号清单端点读它展示）。
///
/// 与 [`persist_password_migration`] 同范式（写锁 → 快照改 → save → replace），
/// 差异：目标字段缺失（老配置 / 用户不存在）静默返回——登录通道绝不能因为
/// 元数据回写失败而受影响（fail-safe，只 warn）。
fn persist_last_login(state: &super::MgmtState, user: &str) {
    let _guard = super::writeapi::write_guard();
    let Some(path) = state.config_path() else {
        return; // dev 账号等不落 config 的登录：无回写目标，静默
    };
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    let mut next = (*state.config()).clone();
    let Some(section) = next.mgmt_auth.as_mut() else {
        return;
    };
    let Some(target) = section.users.iter_mut().find(|u| u.name == user) else {
        return; // dev 管理员等内存账号：无 config 行，静默
    };
    target.last_login_at_ms = Some(now_ms);
    if let Err(err) = next.save(&path) {
        tracing::warn!(user = %user, error = %err, "auth: last_login_at persist failed");
        return;
    }
    state.daemon().config_shared().replace(next);
}

/// 账号体系是否已初始化：系统里只要存在**任何一个可用账号**（含 dev 管理员，
/// 它来自环境变量、不落 config）即视为已初始化 ⇒ bootstrap 必须恒 409
/// （否则回环访问者可在「配了 dev pass 但无 `[[mgmt_auth.users]]`」的实例上
/// 再建一个 system 账号）。
pub(super) fn is_initialized(state: &super::MgmtState) -> bool {
    live_login_auth(state).user_count() > 0
}

// ---- REST 处理器 ----

/// POST /api/auth/login 请求体（字段缺失按空串处理，统一走 401 路径）。
#[derive(Debug, Default, Deserialize)]
struct LoginBody {
    #[serde(default)]
    username: String,
    #[serde(default)]
    password: String,
}

/// POST /api/auth/login `{username, password}` → 200 `{token, role}`。
///
/// 未知用户 / 错误密码 / 请求体非法（非 JSON / 缺字段）一律**同一 401**
/// `{"error":"unauthorized","message":"invalid credentials"}`——不区分原因，
/// 防账号枚举（安全契约见模块注释）。
pub async fn login(State(state): State<super::MgmtState>, body: Bytes) -> Response {
    let parsed = serde_json::from_slice::<LoginBody>(&body).ok();
    // task 26：登录成功 / 失败均落持久安全审计（防篡改哈希链）。detail 不含
    // 凭据、不区分失败原因（与 401 统一响应同口径，防账号枚举）；写失败仅告警。
    let username = parsed
        .as_ref()
        .map(|req| req.username.trim().to_string())
        .unwrap_or_default();
    // 判定表取「跟随配置热重载」的那一份：建号 / 改密后无需重启即可登录。
    let mut live = live_login_auth(&state);
    let outcome = parsed
        .filter(|req| !req.username.is_empty() && !req.password.is_empty())
        .and_then(|req| live.login(&req.username, &req.password));
    // 遗留格式登录成功 → 顺手把新摘要回写（见 [`persist_password_migration`]）。
    // 只在登录成功时触发，失败登录不产生写 IO。
    if outcome.is_some() {
        if let Some((user, phc)) = live.take_pending_migration() {
            persist_password_migration(&state, &user, &phc);
        }
        // 登录成功回写 last_login_at（元数据落盘；与迁移回写同范式，不做备份——
        // 非配置语义变更，且登录频率下 IO 可忽略）。
        persist_last_login(&state, &username);
    }
    if let Some(logger) = state.daemon().audit_logger() {
        let (event, outcome_literal) = if outcome.is_some() {
            (
                crate::audit::AuditEventType::Login,
                crate::audit::OUTCOME_ACCEPTED,
            )
        } else {
            (
                crate::audit::AuditEventType::LoginFailed,
                crate::audit::OUTCOME_DENIED,
            )
        };
        let actor = if username.is_empty() {
            "<missing>"
        } else {
            username.as_str()
        };
        if let Err(err) = logger.record(actor, event, outcome_literal, "mgmt login") {
            tracing::warn!(error = %err, "auth_login: persistent audit record failed");
        }
    }
    match outcome {
        Some((token, role_id)) => Json(serde_json::json!({
            "token": token,
            "role": role_id,
        }))
        .into_response(),
        None => (
            StatusCode::UNAUTHORIZED,
            Json(serde_json::json!({
                "error": "unauthorized",
                "message": "invalid credentials",
            })),
        )
            .into_response(),
    }
}

/// GET /api/auth/whoami → 当前身份（`sub` / `role` / `exp`；**exp 字符串编码**，
/// 大数红线照旧）。无 / 非法 token → 401（extractor 语义）。
pub async fn whoami(authed: AuthedRole) -> Response {
    Json(serde_json::json!({
        "sub": authed.claims.sub,
        "role": authed.claims.role,
        "exp": authed.claims.exp.to_string(),
    }))
    .into_response()
}

// ---- 首次安装：账号初始化入口（#27） ----

/// 首个账号的默认角色字面量。
///
/// **判定依据**：当前 `rbac::Role::from_str` 的字面量集合只有 `ops` / `lic_ops` /
/// `risk` / `system`，且**不接受任何别名**——写入 `role = "admin"` 会在启动装配
/// 时被「未知角色」整条跳过（fail-closed，见 `build` 的 warn + `with_user`）。
/// 因此本机落地的是 **`system`**（当前 RBAC 中的集中角色，语义等同管理员）。
/// 若确需对外暴露 `admin` 字面量，必须先给 `rbac::Role` 加变体（改 `rbac.rs`），
/// 本常量随之改成 `"admin"`——否则账号在重启后静默失效。
pub const BOOTSTRAP_ROLE_LITERAL: &str = "system";
/// 口令最小长度（字符数）。首个账号是全系统唯一匿名创建动作的入口，此处收口常量，
/// 供后续「新建账号」写端点复用同一套强校验口径。
pub const MIN_PASSWORD_LEN: usize = 8;

/// `POST /api/auth/bootstrap` 请求体（`display_name` / `idempotency_key` 均可选）。
///
/// `idempotency_key` 为**批次级**幂等键：本端点按语义只会被接受一次（无账号 → 建号），
/// 因此实现上**不做行级唯一索引**，只随审计落来源信息；带相同 key 的重复调用会因
/// 「已初始化」而稳定返回 409。
#[derive(Debug, Default, Deserialize)]
pub struct BootstrapBody {
    #[serde(default)]
    username: String,
    #[serde(default)]
    password: String,
    /// 展示名：同时接受 `display_name` 与前端契约里的 `displayName`。
    #[serde(default, alias = "displayName")]
    display_name: String,
    #[serde(default)]
    idempotency_key: String,
}

/// 非本地来源 / 拿不到对端地址 → 403。bootstrap 是全系统唯一的匿名写通道，
/// 任何拿不到本地对端信息的情况一律 fail-closed（不猜、不超时放行）。
///
/// 文案为**中文**：该 `message` 会被控制台登录页原样透出（`LoginPage.vue` 的
/// `describeBootstrapFailure` 优先展示后端 `message`），英文会直接变成界面上的
/// 英文提示。
const NOT_LOCAL_MESSAGE: &str = "该操作仅允许从本机回环地址调用（127.0.0.1 / ::1）";

/// `GET /api/auth/state` → 账号体系初始化状态（**免认证**，无副作用，不写审计）。
///
/// 契约（字段语义稳定）：
/// ```json
/// {"initialized":false,"accounts":0,"roles":0,"trial_enabled":false,
///  "trial_days_left":0,"server_time":"2026-09-27T10:00:00Z"}
/// ```
/// - `initialized`：`accounts > 0`（一个账号都没有 ⇒ `false`，前端据此走初始化页）；
/// - `roles`：配置里 `[mgmt_auth.roles]` 的自定义角色数（内置四角色不入配置，不计入）；
/// - `trial_enabled` / `trial_days_left`：来源为授权运行期的 `Trial` 判定；拿不到
///   授权运行期时如实报 `false` + `0` 并附 `note` 说明，**绝不编造**；
/// - `server_time`：服务端时间，**字符串**（大数红线：不因「时间看起来不大」破例）。
pub async fn auth_state(State(state): State<super::MgmtState>) -> Response {
    let config = state.config();
    let section = config.mgmt_auth.as_ref();
    // `accounts` / `initialized` 走「可用账号」口径（含 dev 管理员），与 bootstrap
    // 的一次性闸门同源——避免 state 说未初始化、bootstrap 却放行（AUTH-1）。
    let accounts = live_login_auth(&state).user_count();
    let roles = section.map_or(0, |s| s.roles.len());

    // 试用判定：仅 `LicenseState::Trial` 视为试用中；其余状态（含授权有效 / 降级）
    // 一律 `false` / `0`，并如实留下原因（`note` 只在异常时附，字段形状保持稳定）。
    let mut trial = serde_json::json!({
        "trial_enabled": false,
        "trial_days_left": 0,
    });
    let mut note = Option::<String>::None;
    match state.daemon().license_runtime().map(|rt| rt.state()) {
        Some(crate::auth::client::LicenseState::Trial { days_left }) => {
            trial = serde_json::json!({
                "trial_enabled": true,
                "trial_days_left": days_left,
            });
        }
        Some(_) => {
            note = Some("当前非试用状态（已授权 / 宽限期 / 降级 / 未授权）".to_string());
        }
        None => note = Some("网关授权模块未就绪，试用状态暂不可判定".to_string()),
    }

    let mut body = serde_json::json!({
        "initialized": accounts > 0,
        "accounts": accounts,
        "roles": roles,
        "server_time": format_rfc3339_utc(now_unix_secs()),
    });
    body["trial_enabled"] = trial["trial_enabled"].clone();
    body["trial_days_left"] = trial["trial_days_left"].clone();
    if let Some(reason) = note {
        body["note"] = serde_json::json!(reason);
    }
    Json(body).into_response()
}

/// `POST /api/auth/bootstrap` → 创建**首个**账号并返回与 login 同构的 token。
///
/// ## 安全契约
/// - **本地请求**：仅当对端为回环地址（或拿不到对端信息）才继续；否则 403
///   ——这是全系统唯一的匿名写操作，必须同时满足「无账号」+「本地」；
/// - **一次性**：`initialized == true` ⇒ 409 `already_initialized`，入口永久关闭
///   （fail-closed，不因任何路径回到可写状态）；
/// - **口令只存摘要**：Argon2id 的 PHC 串（与 `auth_login` 的存储同一方式），
///   明文不落盘、不入日志、不入审计；
/// - **审计必录**：成功 / 被拒都会入 ops 审计环 + 持久哈希链，`actor` = 被创建的用户名，
///   detail 含 `auth.bootstrap: ... {source}`（来源 + 可选 `idempotency_key`）；
/// - 校验顺序：安全前置（本地）→ 入参合法性 → 初始化闸门（409）——安全与
///   fail-closed 判定均在提前返回之前完成。
pub async fn bootstrap(
    State(state): State<super::MgmtState>,
    connect: Option<axum::extract::ConnectInfo<std::net::SocketAddr>>,
    body: Bytes,
) -> Response {
    // ① 安全前置：本地对端。任何缺失 / 非回环 ⇒ 立即拒绝（fail-closed，不进业务分支）。
    let Some(peer) = is_local_peer(connect) else {
        return (
            StatusCode::FORBIDDEN,
            Json(serde_json::json!({
                "error": "forbidden",
                "message": NOT_LOCAL_MESSAGE,
            })),
        )
            .into_response();
    };

    // ② 入参解析与校验（字段缺失按空串处理，统一走 400）。
    let parsed = serde_json::from_slice::<BootstrapBody>(&body).unwrap_or_default();
    let username = parsed.username.trim();
    let password = parsed.password.as_str();
    if username.is_empty() {
        return super::writeapi::validation_error("username", "账号不能为空", "非空账号");
    }
    if password.chars().count() < MIN_PASSWORD_LEN {
        return super::writeapi::validation_error(
            "password",
            &format!("口令至少需要 {MIN_PASSWORD_LEN} 个字符"),
            "非空账号，且口令长度 ≥ 8",
        );
    }

    // ③ 一次性闸门：判重与落盘同处一把写锁内，杜绝并发双建（TOCTOU）。
    let _guard = super::writeapi::write_guard();
    let config = state.config();
    // 「已初始化」按**可用账号**口径（含 dev 管理员，它不落 config）——只看
    // `[[mgmt_auth.users]]` 会让有 dev 账号的实例被误判为未初始化（AUTH-1）。
    if is_initialized(&state) {
        super::writeapi::audit(
            &state,
            username,
            super::remote_ops::OpsAction::AuthBootstrap,
            false,
            crate::audit::OUTCOME_DENIED,
            &format!(
                "{}; bootstrap refused: already initialized",
                bootstrap_audit_detail(username, peer, &parsed)
            ),
        );
        return (
            StatusCode::CONFLICT,
            Json(serde_json::json!({
                "error": "already_initialized",
                "message": "账号体系已初始化，创建首个管理员的入口已永久关闭",
            })),
        )
            .into_response();
    }

    // ④ 落盘首个账号（写前备份 + 原子写 + 热替换；审计由 persist_config 统一记）。
    // 摘要用 Argon2id（加盐、抗字典反查），**不再**是无盐 SHA-256。
    let digest = hash_phc(password).unwrap_or_else(|| {
        // 理论上不可达（盐只需 CSPRNG）。真发生也必须建号，否则首次安装入口直接死掉；
        // 退回遗留摘要，登录成功时同样会被升级。
        tracing::warn!("mgmt auth: Argon2id hashing unavailable; falling back to legacy digest");
        hex::encode(sha256_bytes(password.as_bytes()))
    });
    let display_name = parsed.display_name.trim();
    let mut new_config = (*config).clone();
    let mut section = new_config.mgmt_auth.clone().unwrap_or_default();
    section.users.push(crate::config::MgmtAuthUser {
        name: username.to_string(),
        role: BOOTSTRAP_ROLE_LITERAL.to_string(),
        password_hash: digest,
        display_name: (!display_name.is_empty()).then(|| display_name.to_string()),
        status: Some("active".to_string()),
        created_at_ms: Some(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0),
        ),
        last_login_at_ms: None,
    });
    new_config.mgmt_auth = Some(section);
    let detail = bootstrap_audit_detail(username, peer, &parsed);
    let version = match super::pages::persist_config(
        &state,
        new_config,
        username,
        super::remote_ops::OpsAction::AuthBootstrap,
        &detail,
    ) {
        Ok(version) => version,
        Err(response) => return response,
    };

    // ⑤ 签发 token（与 login 同构：token + role；`config_version` 字符串编码）。
    let role = crate::mgmt::rbac::Role::from_str(BOOTSTRAP_ROLE_LITERAL)
        .unwrap_or(crate::mgmt::rbac::Role::System);
    let role_literal = role.as_str().to_string();
    match state
        .login_auth()
        .issue_token(username, &role_literal, permissions_of(role).to_vec())
    {
        Ok((token, _exp)) => Json(serde_json::json!({
            "token": token,
            "role": role_literal,
            "config_version": version.to_string(),
        }))
        .into_response(),
        Err(err) => {
            tracing::warn!(error = %err, "auth bootstrap: issue token failed after config write");
            super::writeapi::audit(
                &state,
                username,
                super::remote_ops::OpsAction::AuthBootstrap,
                false,
                crate::audit::OUTCOME_FAILED,
                &format!("{detail}; token issuance failed: {err}"),
            );
            super::writeapi::internal("bootstrap succeeded but token issuance failed")
        }
    }
}

/// 对端地址判定：仅**回环**放行；拿不到 ConnectInfo（服务未按
/// `into_make_service_with_connect_info` 装配）⇒ 视为非本地，fail-closed。
///
/// 抽成纯函数是为了可单测：非回环 / 缺失两种情况都走同一条拒绝路径。
fn is_local_peer(
    connect: Option<axum::extract::ConnectInfo<std::net::SocketAddr>>,
) -> Option<std::net::SocketAddr> {
    connect
        .filter(|info| info.0.ip().is_loopback())
        .map(|info| info.0)
}

/// bootstrap 的审计 detail（**不含凭据**：只有账号名 / 来源 / 批次级幂等键前缀）。
fn bootstrap_audit_detail(
    username: &str,
    peer: std::net::SocketAddr,
    body: &BootstrapBody,
) -> String {
    let raw_key = body.idempotency_key.trim();
    // 只截前 32 字符：幂等键无需全量入链，避免长串污染审计。
    let key: String = raw_key.chars().take(32).collect();
    let key = if raw_key.is_empty() {
        "none".to_string()
    } else {
        key
    };
    format!("first-run bootstrap: account={username} peer={peer} idempotency_key={key}")
}

///  RFC3339 UTC（秒）→ `YYYY-MM-DDTHH:MM:SSZ`（纯算术，不引第三方时间库）。
///
/// 由 [`now_unix_secs`] 的 Unix 秒换算民用日期（Hinnant 的 civil-from-days 算法，
/// 无查表、无 panic）。`server_time` 之所以走字符串：项目红线要求 JSON 里的时间 /
/// 计数不落双精度陷阱，且本端点对前端是无条件可达的免认证读。
fn format_rfc3339_utc(secs: i64) -> String {
    let days = secs / 86_400;
    let (year, month, day) = civil_from_days(days);
    let rest = secs % 86_400;
    let hour = rest / 3_600;
    let minute = (rest % 3_600) / 60;
    let second = rest % 60;
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}Z")
}

/// 自 epoch 天数 → 民用日期（`(年, 月, 日)`；算法见 [`format_rfc3339_utc`]）。
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let shifted = days + 719_468;
    let era = if shifted >= 0 {
        shifted
    } else {
        shifted - 1_460_967
    } / 146_097;
    let doe = shifted - era * 146_097; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_097) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32; // [1, 12]
                                                              // `y` 以 400 年周期起点计年，落在 1 / 2 月时整个 era 起点偏移一年（1970 起算）。
    (y + if month <= 2 { 1 } else { 0 }, month, day)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bootstrap::DaemonShared;
    use crate::config::ConfigShared;
    use crate::mgmt::auth_jwt::verify;
    use crate::mgmt::MgmtState;
    use serde_json::Value;
    use std::sync::Arc;
    use std::time::Duration;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::{TcpListener, TcpStream};

    /// 已知向量：SHA-256("abc")（NIST FIPS-180 示例；known-answer 锚定 hex 格式）。
    const SHA256_ABC_HEX: &str = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";
    /// dev 管理员口令（仅本地开发路；与 `dev_env` 配套）。
    const DEV_PASS: &str = "devpass";

    /// 测试配置（带 [mgmt_auth] 生产路账号：alice/system 密码 "abc"）。
    fn config_with_users() -> GatewayConfig {
        GatewayConfig::parse(&format!(
            r#"
[gateway]
gateway_id = "gw-auth"

[[points]]
device_id = "dev-01"
point_id = "p1"
protocol = "modbus-tcp"
address = "127.0.0.1:502"
frequency_ms = 100

[[mgmt_auth.users]]
name = "alice"
role = "system"
password_hash = "{SHA256_ABC_HEX}"

[[mgmt_auth.users]]
name = "oliver"
role = "ops"
password_hash = "{SHA256_ABC_HEX}"
"#
        ))
        .expect("parse auth config")
    }

    /// 无凭证配置（生产路缺省 + 开发路关闭）。
    fn config_without_users() -> GatewayConfig {
        GatewayConfig::parse(
            r#"
[gateway]
gateway_id = "gw-auth"

[[points]]
device_id = "dev-01"
point_id = "p1"
protocol = "modbus-tcp"
address = "127.0.0.1:502"
frequency_ms = 100
"#,
        )
        .expect("parse plain config")
    }

    /// 装配 MgmtState：显式 build（注入受控 env 闭包）+ with_auth，避免测试间环境变量竞争。
    fn make_state_with(config: &GatewayConfig, env: &dyn Fn(&str) -> Option<String>) -> MgmtState {
        let (auth, login) = build(config, env);
        let daemon = DaemonShared::new();
        daemon.set_config(Arc::new(ConfigShared::new(config.clone())));
        // 写落盘目标：绑定到临时目录下的**独立**种子配置文件（未装配时
        // persist_config 会 fail-closed 拒写，bootstrap 落盘链路无法验证）。
        // 文件按调用序号命名：用例并行且同进程时若复用同一路径会互相覆盖种子。
        static SEED_SEQ: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let seq = SEED_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "iot-daq-auth-bootstrap-seed-{}-{}.toml",
            std::process::id(),
            seq
        ));
        std::fs::write(
            &path,
            toml::to_string_pretty(config).expect("dump seed config"),
        )
        .expect("write seed config");
        MgmtState::new(daemon, Arc::new(config.clone()))
            .with_config_path(&path)
            .with_auth(auth, login)
    }

    /// 恒定无 env（生产路专属）。
    fn no_env(key: &str) -> Option<String> {
        let _ = key;
        None
    }

    /// 仅启用 dev 管理员口令（模拟「有可用账号但不落 config」的实例）。
    fn dev_env(key: &str) -> Option<String> {
        if key == DEV_ADMIN_PASS_ENV {
            Some(DEV_PASS.to_string())
        } else {
            None
        }
    }

    /// 在 127.0.0.1 随机端口启动 axum 服务（本机回环，不依赖外网）。
    async fn spawn_server(state: MgmtState) -> u16 {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let port = listener.local_addr().expect("addr").port();
        tokio::spawn(async move {
            axum::serve(listener, crate::mgmt::router(state))
                .await
                .expect("serve error");
        });
        port
    }

    /// 起一个只挂载 auth 三条路由的测试服务（生产路由表由 `mgmt::router` 收口）。
    ///
    /// 这里也必须按生产装配成 `into_make_service_with_connect_info`：bootstrap 依赖
    /// 对端地址判定本地来源，缺 `ConnectInfo` 时按 fail-closed（403）处理。
    /// 挂载 `login` 是为了验证「建号后立即可登录」——登录判定表必须跟随热重载。
    async fn spawn_auth_router(state: MgmtState) -> u16 {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let port = listener.local_addr().expect("addr").port();
        let router = axum::Router::new()
            .route("/api/auth/state", axum::routing::get(auth_state))
            .route("/api/auth/bootstrap", axum::routing::post(bootstrap))
            .route("/api/auth/login", axum::routing::post(login))
            .with_state(state);
        tokio::spawn(async move {
            axum::serve(
                listener,
                router.into_make_service_with_connect_info::<std::net::SocketAddr>(),
            )
            .await
            .expect("serve error");
        });
        port
    }

    /// 解析原始 HTTP 响应 → (状态码, 头部文本, body)。
    fn parse_response(raw: &str) -> (u16, String, String) {
        let (head, body) = raw
            .split_once("\r\n\r\n")
            .expect("response must contain header/body separator");
        let status_line = head.lines().next().expect("status line");
        let status: u16 = status_line
            .split_whitespace()
            .nth(1)
            .and_then(|s| s.parse().ok())
            .expect("status code");
        (status, head.to_string(), body.to_string())
    }

    /// 手写 HTTP 请求（method/path/body/token），整体 3s 超时防挂死。
    async fn http_request(
        port: u16,
        method: &str,
        path: &str,
        body: Option<&str>,
        token: Option<&str>,
    ) -> (u16, String, String) {
        tokio::time::timeout(Duration::from_secs(3), async move {
            let mut stream = TcpStream::connect(("127.0.0.1", port)).await.expect("connect");
            let body = body.unwrap_or("");
            let auth = token
                .map(|t| format!("Authorization: Bearer {t}\r\n"))
                .unwrap_or_default();
            let request = if method == "POST" {
                format!(
                    "POST {path} HTTP/1.1\r\nHost: 127.0.0.1\r\n{auth}Content-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                )
            } else {
                format!("GET {path} HTTP/1.1\r\nHost: 127.0.0.1\r\n{auth}Connection: close\r\n\r\n")
            };
            stream.write_all(request.as_bytes()).await.expect("write");
            stream.flush().await.expect("flush");
            let mut buf = Vec::new();
            stream.read_to_end(&mut buf).await.expect("read");
            parse_response(&String::from_utf8(buf).expect("utf8"))
        })
        .await
        .expect("http_request timed out")
    }

    async fn http_get(port: u16, path: &str) -> (u16, String, String) {
        http_request(port, "GET", path, None, None).await
    }

    async fn http_post(port: u16, path: &str, body: &str) -> (u16, String, String) {
        http_request(port, "POST", path, Some(body), None).await
    }

    async fn http_get_bearer(port: u16, path: &str, token: &str) -> (u16, String, String) {
        http_request(port, "GET", path, None, Some(token)).await
    }

    async fn http_post_bearer(
        port: u16,
        path: &str,
        body: &str,
        token: &str,
    ) -> (u16, String, String) {
        http_request(port, "POST", path, Some(body), Some(token)).await
    }

    /// 登录并返回 (状态码, body)。
    async fn try_login(port: u16, username: &str, password: &str) -> (u16, String) {
        let body = serde_json::json!({ "username": username, "password": password }).to_string();
        let (status, _, body) = http_post(port, "/api/auth/login", &body).await;
        (status, body)
    }

    // ---- 登录（生产路） ----

    /// QA Happy: 生产路账号登录 → 200 {token, role}；token 可用配置密钥验真，
    /// claims 的 sub/role/exp 与登录账号一致。
    #[tokio::test]
    async fn config_user_login_returns_verifiable_token() {
        let config = config_with_users();
        let state = make_state_with(&config, &no_env);
        let port = spawn_server(state.clone()).await;

        let (status, body) = try_login(port, "alice", "abc").await;
        assert_eq!(status, 200, "{body}");
        let value: Value = serde_json::from_str(&body).expect("json");
        assert_eq!(value["role"], "system");
        let token = value["token"].as_str().expect("token string");

        let claims = verify(token, state.login_auth().key(), now_unix_secs(), 60)
            .expect("token must verify with configured key");
        assert_eq!(claims.sub, "alice");
        assert_eq!(claims.role, "system");
    }

    /// QA 安全: 错误密码 → 401；未知用户 → 401；且两者响应体**完全一致**
    ///（不区分原因，防账号枚举）。
    #[tokio::test]
    async fn wrong_password_and_unknown_user_are_uniform_401() {
        let config = config_with_users();
        let state = make_state_with(&config, &no_env);
        let port = spawn_server(state).await;

        let (wrong_pass_status, wrong_pass_body) = try_login(port, "alice", "wrong").await;
        let (unknown_status, unknown_body) = try_login(port, "nobody", "abc").await;

        assert_eq!(wrong_pass_status, 401);
        assert_eq!(unknown_status, 401);
        assert_eq!(
            wrong_pass_body, unknown_body,
            "responses must be byte-identical (no user enumeration)"
        );
    }

    /// QA Error: 请求体非法（非 JSON / 缺字段 / 空串）→ 一律 401（不区分）。
    #[tokio::test]
    async fn malformed_login_bodies_are_401() {
        let config = config_with_users();
        let state = make_state_with(&config, &no_env);
        let port = spawn_server(state).await;

        for body in [
            "not-json{{{",
            "{}",
            r#"{"username":"alice"}"#,
            r#"{"username":"","password":"abc"}"#,
        ] {
            let (status, _, resp) = http_post(port, "/api/auth/login", body).await;
            assert_eq!(status, 401, "body {body:?} must be 401, got {resp}");
        }
    }

    /// QA 安全: 生产路无账号 + 开发路关闭 → 登录端点全拒（fail-closed）。
    #[tokio::test]
    async fn no_credentials_fail_closes_all_logins() {
        let config = config_without_users();
        let state = make_state_with(&config, &no_env);
        assert_eq!(state.login_auth().user_count(), 0, "zero users loaded");
        let port = spawn_server(state).await;

        let (status, body) = try_login(port, "dev", "anything").await;
        assert_eq!(status, 401, "{body}");
    }

    // ---- 口令哈希：Argon2id 与遗留格式向后兼容 ----

    /// 取配置里某账号的**存储串**（升级前后形态的断言口径）。
    fn stored_password(state: &MgmtState, name: &str) -> String {
        state
            .config()
            .mgmt_auth
            .as_ref()
            .expect("mgmt_auth")
            .users
            .iter()
            .find(|u| u.name == name)
            .unwrap_or_else(|| panic!("account {name} must be present in config"))
            .password_hash
            .clone()
    }

    /// QA 安全（存储加固）: 遗留的**无盐 SHA-256** 条目依然能登录——存量账号不被
    /// 升级动作锁死；同时 `legacy_hash_count()` 如实报出待迁移条数（启动 warn 依据）。
    #[tokio::test]
    async fn legacy_sha256_account_can_login_without_lockout() {
        let config = config_with_users(); // alice / oliver 都是遗留 sha256
        let state = make_state_with(&config, &no_env);
        assert_eq!(
            state.login_auth().legacy_hash_count(),
            2,
            "两条都是遗留格式"
        );
        let port = spawn_server(state).await;

        let (status, body) = try_login(port, "alice", "abc").await;
        assert_eq!(status, 200, "{body}");
    }

    /// QA 安全（本次加固的核心）: 遗留条目登录成功后，存储**就地升级**为 Argon2id
    /// PHC 串并回写配置——裸摘要从配置里消失；未登录的其它遗留条目不动。
    #[tokio::test]
    async fn legacy_account_is_upgraded_to_argon2id_on_login() {
        let config = config_with_users();
        let state = make_state_with(&config, &no_env);
        let port = spawn_server(state.clone()).await;
        let (status, _) = try_login(port, "alice", "abc").await;
        assert_eq!(status, 200);

        let alice_hash = stored_password(&state, "alice");
        assert!(
            alice_hash.starts_with("$argon2id$"),
            "升级后必须是 PHC 串: {alice_hash}"
        );
        assert_ne!(
            alice_hash, SHA256_ABC_HEX,
            "裸 SHA-256 摘要不得残留在配置里"
        );
        // 只迁移登录命中的那条。
        assert_eq!(
            stored_password(&state, "oliver"),
            SHA256_ABC_HEX,
            "未登录的遗留条目保持原样（不惊扰其它账号）"
        );
    }

    /// QA 安全: 升级后再次登录仍成功（幂等，且走 Argon2id 路径），错口令依旧 401。
    #[tokio::test]
    async fn login_after_migration_is_idempotent_with_argon2id() {
        let config = config_with_users();
        let state = make_state_with(&config, &no_env);
        let port = spawn_server(state.clone()).await;

        let (first_status, first_body) = try_login(port, "alice", "abc").await;
        assert_eq!(first_status, 200, "{first_body}");
        let upgraded = stored_password(&state, "alice");
        assert!(upgraded.starts_with("$argon2id$"), "首登已升级: {upgraded}");

        // 第二次登录：存储已是 PHC，但必须仍然通过（迁移可重复执行、不会把自己锁死）。
        let (second_status, second_body) = try_login(port, "alice", "abc").await;
        assert_eq!(second_status, 200, "{second_body}");
        // 幂等：再次登录不会改写存储串。
        assert_eq!(
            stored_password(&state, "alice"),
            upgraded,
            "重复登录不得改写已升级的摘要"
        );
        assert_eq!(
            super::live_login_auth(&state).legacy_hash_count(),
            1,
            "热重载后的生效账号表里只剩 oliver 待迁移（回写已生效）"
        );

        // 错口令：无论新 / 旧格式都必须拒绝。
        let (bad_status, _) = try_login(port, "alice", "wrong").await;
        assert_eq!(bad_status, 401);
    }

    /// QA 安全: 错口令走遗留回退路径也必须被拒，且**不产生升级**（绝不把错误摘要写回）。
    #[tokio::test]
    async fn wrong_password_does_not_trigger_migration() {
        let config = config_with_users();
        let state = make_state_with(&config, &no_env);
        let port = spawn_server(state.clone()).await;

        let (status, _) = try_login(port, "alice", "definitely-wrong").await;
        assert_eq!(status, 401);

        assert_eq!(
            stored_password(&state, "alice"),
            SHA256_ABC_HEX,
            "失败登录不得改写存储（否则等于写入错误凭据）"
        );
    }

    /// QA 安全: Argon2id 校验——遗留存储串对新口令通过 / 错口令拒绝。
    #[test]
    fn legacy_storage_verifies_then_stays_legacy() {
        let user = LoginUser {
            name: "legacy".to_string(),
            role_id: Role::Ops.as_str().to_string(),
            perms: Vec::new(),
            password: PasswordRef::LegacyHex(SHA256_ABC_HEX.to_string()),
        };
        assert_eq!(verify_password(&user, "abc"), Verify::AcceptedLegacy);
        assert_eq!(verify_password(&user, "wrong"), Verify::Reject);
        assert!(user.password.is_legacy(), "纯校验不产生迁移副作用");
        // 装配期解析：遗留 hex 与 PHC 串都能装入（空口令串不是合法形态）。
        assert!(matches!(
            parse_password_ref(SHA256_ABC_HEX),
            Some(PasswordRef::LegacyHex(_))
        ));
        let phc = hash_phc("phc-parses").expect("hash");
        assert!(matches!(
            parse_password_ref(&phc),
            Some(PasswordRef::Phc(_))
        ));
        // 两种都不是 → 视为弱凭证，装配时跳过该账号。
        assert!(parse_password_ref("not-a-hash").is_none());
        assert!(parse_password_ref("").is_none());
    }

    /// QA 安全: Argon2id 加盐——同一次口令两次哈希的 salt 必不同（无彩虹表可行），
    /// 且两条 PHC 串都能各自校验该口令、拒绝错口令。
    #[test]
    fn argon2_hashes_same_password_with_different_salts() {
        let first = hash_phc("same-password").expect("hash #1");
        let second = hash_phc("same-password").expect("hash #2");

        // 参数钉死在 RFC 9106 交互式基线上（m=19456 KiB≈19 MiB, t=2, p=1）。
        assert!(
            first.starts_with("$argon2id$v=19$m=19456,t=2,p=1$"),
            "PHC 串必须带 Argon2id + 选定参数: {first}"
        );
        assert_ne!(first, second, "每次哈希必须换随机盐");
        // salt 段不同（而非整串随机）——同一参数的两次实例化仍可被独立校验。
        assert_ne!(
            first.split('$').nth(4).expect("salt 段"),
            second.split('$').nth(4).expect("salt 段")
        );

        let lhs = LoginUser {
            name: "a".to_string(),
            role_id: Role::System.as_str().to_string(),
            perms: Vec::new(),
            password: PasswordRef::Phc(first),
        };
        let rhs = LoginUser {
            name: "b".to_string(),
            role_id: Role::System.as_str().to_string(),
            perms: Vec::new(),
            password: PasswordRef::Phc(second),
        };
        assert_eq!(verify_password(&lhs, "same-password"), Verify::Accepted);
        assert_eq!(
            verify_password(&rhs, "same-password"),
            Verify::Accepted,
            "salt 独立，互不影响"
        );
        assert_eq!(verify_password(&lhs, "other-password"), Verify::Reject);
        assert_eq!(verify_password(&rhs, "other-password"), Verify::Reject);
    }

    /// QA 安全（交付物一致性）: 出厂模板里的 `password_hash` **不得是裸 64 位 hex**。
    ///
    /// 这两个文件是运维照抄的样板；模板若留着旧的 `sha256(password)` 描述，照抄即把
    /// 无盐摘要写回配置（本次加固的口子重新打开，且首次登录会自动升级 → 静默失效）。
    /// 只断言「不是裸 hex」，反向不强制必须是 PHC——存量的 sha256 条目依旧合法。
    #[test]
    fn shipped_config_templates_do_not_seed_legacy_sha256_hashes() {
        for path in [
            concat!(env!("CARGO_MANIFEST_DIR"), "/../../config.example.toml"),
            concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../../deploy/docker/config/gateway.default.toml"
            ),
        ] {
            let raw = std::fs::read_to_string(path)
                .unwrap_or_else(|err| panic!("read shipped template: {path}: {err}"));
            let mut checked = 0usize;
            for line in raw.lines() {
                let Some(value) = line
                    .split_once("password_hash = \"")
                    .and_then(|(_, rest)| rest.split_once('"').map(|(value, _)| value))
                else {
                    continue;
                };
                checked += 1;
                assert!(
                    value.len() != 64 || !value.chars().all(|c| c.is_ascii_hexdigit()),
                    "出厂模板不得留无盐 SHA-256 的 64 位 hex（会被照抄回配置）: {path} -> {value}"
                );
            }
            assert!(checked > 0, "模板里应有 password_hash 示例行: {path}");
        }
    }

    // ---- 登录（开发路：dev admin） ----

    /// QA: `mgmt_auth` 未配置 + `IOT_DAQ_DEV_ADMIN_PASS` 已设 → dev 管理员
    /// 可登录（username=dev, role=system）。
    #[tokio::test]
    async fn dev_admin_env_enabled_login_works() {
        let config = config_without_users();
        let state = make_state_with(&config, &|key: &str| {
            if key == DEV_ADMIN_PASS_ENV {
                Some("devpass".to_string())
            } else {
                None
            }
        });
        assert_eq!(state.login_auth().user_count(), 1);
        let port = spawn_server(state.clone()).await;

        let (status, body) = try_login(port, "dev", "devpass").await;
        assert_eq!(status, 200, "{body}");
        let value: Value = serde_json::from_str(&body).expect("json");
        assert_eq!(value["role"], "system");
    }

    /// QA 安全: dev 管理员密码错误 → 401；环境变量未设（开发路关闭）→ 登录全拒。
    #[tokio::test]
    async fn dev_admin_disabled_or_wrong_password_rejected() {
        // 环境变量未设 → dev 不可登录。
        let config = config_without_users();
        let state = make_state_with(&config, &no_env);
        let port = spawn_server(state).await;
        let (status, _) = try_login(port, "dev", "devpass").await;
        assert_eq!(status, 401, "dev admin must be disabled without env");

        // 环境变量已设但密码错 → 401。
        let state = make_state_with(&config, &|key: &str| {
            if key == DEV_ADMIN_PASS_ENV {
                Some("devpass".to_string())
            } else {
                None
            }
        });
        let port = spawn_server(state).await;
        let (status, body) = try_login(port, "dev", "wrong").await;
        assert_eq!(status, 401, "{body}");
    }

    // ---- /api/ops/* 守卫（端到端） ----

    /// QA Happy 端到端: 登录（system 账号）→ token → Bearer 调 /api/ops/restart
    /// → 200 accepted（confirm 回显 gateway_id）。
    #[tokio::test]
    async fn login_token_grants_ops_restart_200() {
        let config = config_with_users();
        let state = make_state_with(&config, &no_env);
        let port = spawn_server(state).await;

        let (_, login_body) = try_login(port, "alice", "abc").await;
        let token: String = serde_json::from_str::<Value>(&login_body).expect("json")["token"]
            .as_str()
            .expect("token")
            .to_string();

        let (status, _, body) = http_post_bearer(
            port,
            "/api/ops/restart",
            r#"{"actor":"alice","confirm":"gw-auth","reason":"task57 e2e"}"#,
            &token,
        )
        .await;
        assert_eq!(status, 200, "{body}");
        let value: Value = serde_json::from_str(&body).expect("json");
        assert_eq!(value["accepted"], true);
        assert_eq!(value["mode"], "graceful");
    }

    /// QA 安全: 无 token 调 /api/ops/restart → 401（中间件层拒绝）。
    #[tokio::test]
    async fn ops_restart_without_token_is_401() {
        let config = config_with_users();
        let state = make_state_with(&config, &no_env);
        let port = spawn_server(state).await;

        let (status, _, body) = http_post(
            port,
            "/api/ops/restart",
            r#"{"actor":"alice","confirm":"gw-auth"}"#,
        )
        .await;
        assert_eq!(status, 401, "{body}");
    }

    /// QA 安全: ops 角色账号登录 → token 调 /api/ops/restart → 403
    ///（identity 已建立但权限不足；ops 不持 ops.restart）。
    #[tokio::test]
    async fn ops_role_token_restart_is_403() {
        let config = config_with_users();
        let state = make_state_with(&config, &no_env);
        let port = spawn_server(state).await;

        let (_, login_body) = try_login(port, "oliver", "abc").await; // role=ops
        let token: String = serde_json::from_str::<Value>(&login_body).expect("json")["token"]
            .as_str()
            .expect("token")
            .to_string();

        let (status, _, body) = http_post_bearer(
            port,
            "/api/ops/restart",
            r#"{"actor":"oliver","confirm":"gw-auth"}"#,
            &token,
        )
        .await;
        assert_eq!(status, 403, "{body}");
        let value: Value = serde_json::from_str(&body).expect("json");
        assert_eq!(value["error"], "forbidden");
    }

    /// QA 安全: 垃圾 token → 401（中间件层，签名/结构校验在 Rust 侧）。
    #[tokio::test]
    async fn garbage_token_is_401() {
        let config = config_with_users();
        let state = make_state_with(&config, &no_env);
        let port = spawn_server(state).await;

        let (status, _, _) = http_get_bearer(port, "/api/auth/whoami", "not.a.jwt").await;
        assert_eq!(status, 401);
    }

    // ---- whoami / 读接口 ----

    /// QA: whoami 返回当前 role + exp（**字符串编码**，大数红线）+ sub；
    /// exp 与 token claims 一致。
    #[tokio::test]
    async fn whoami_returns_role_and_string_exp() {
        let config = config_with_users();
        let state = make_state_with(&config, &no_env);
        let port = spawn_server(state.clone()).await;

        let (_, login_body) = try_login(port, "alice", "abc").await;
        let token: String = serde_json::from_str::<Value>(&login_body).expect("json")["token"]
            .as_str()
            .expect("token")
            .to_string();

        let (status, _, body) = http_get_bearer(port, "/api/auth/whoami", &token).await;
        assert_eq!(status, 200, "{body}");
        let value: Value = serde_json::from_str(&body).expect("json");
        assert_eq!(value["role"], "system");
        assert_eq!(value["sub"], "alice");
        assert!(
            value["exp"].is_string(),
            "exp must be string (大数红线): {value}"
        );

        let claims = verify(&token, state.login_auth().key(), now_unix_secs(), 60).expect("verify");
        assert_eq!(value["exp"], claims.exp.to_string());
    }

    /// QA: 读接口保持开放（无 token → 200），16 页 web-console 契约不变。
    /// 注：`/api/events` 是长连接 SSE（读到 EOF 的手写 HTTP 客户端会挂住），
    /// 其开放性由 mod.rs 既有 SSE 测试覆盖，此处不重复断言。
    #[tokio::test]
    async fn read_endpoints_stay_open_without_token() {
        let config = config_with_users();
        let state = make_state_with(&config, &no_env);
        let port = spawn_server(state).await;

        let (status, _, _) = http_get(port, "/api/health").await;
        assert_eq!(status, 200);
        let (status, _, _) = http_get(port, "/api/status").await;
        assert_eq!(status, 200);
        let (status, _, _) = http_get(port, "/api/devices").await;
        assert_eq!(status, 200);
        let (status, _, _) = http_get(port, "/api/points?device_id=dev-01").await;
        assert_eq!(status, 200);
        let (status, _, _) = http_get(port, "/api/outlets").await;
        assert_eq!(status, 200);
    }

    // ---- 密钥解析（env 注入） ----

    /// QA: 64 hex 合法密钥 → 精确解析（dev_fallback=false）；
    /// 非法 hex / 长度不符 / 未提供 → dev 兜底（dev_fallback=true）。
    #[test]
    fn issuer_key_resolution_env_and_fallback() {
        let hex64 = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
        let (key, fallback) =
            resolve_issuer_key(&|k| (k == JWT_SECRET_ENV).then(|| hex64.to_string()));
        assert!(!fallback);
        assert_eq!(
            key,
            IssuerKey::from_slice(&hex::decode(hex64).expect("hex")).expect("32b")
        );

        for bad in ["zz", "0123", "", "00".repeat(31).as_str()] {
            let (_, fallback) =
                resolve_issuer_key(&|k| (k == JWT_SECRET_ENV).then(|| bad.to_string()));
            assert!(fallback, "invalid secret {bad:?} must fall back to dev key");
        }
        let (_, fallback) = resolve_issuer_key(&|_| None);
        assert!(fallback, "missing secret must fall back to dev key");
    }

    /// QA: 配置账号角色非法（历史别名 admin）→ 该账号被跳过（fail-closed），
    /// 登录 401；其余合法账号不受影响。
    #[tokio::test]
    async fn config_user_with_unknown_role_is_skipped() {
        let config = GatewayConfig::parse(&format!(
            r#"
[gateway]
gateway_id = "gw-auth"

[[mgmt_auth.users]]
name = "legacy"
role = "admin"
password_hash = "{SHA256_ABC_HEX}"

[[mgmt_auth.users]]
name = "alice"
role = "system"
password_hash = "{SHA256_ABC_HEX}"
"#
        ))
        .expect("parse");
        let state = make_state_with(&config, &no_env);
        assert_eq!(state.login_auth().user_count(), 1, "legacy user skipped");
        let port = spawn_server(state).await;

        let (status, _) = try_login(port, "legacy", "abc").await;
        assert_eq!(status, 401, "skipped user must not authenticate");
        let (status, _) = try_login(port, "alice", "abc").await;
        assert_eq!(status, 200, "valid user unaffected");
    }

    // ---- 首次安装（#27） ----

    /// QA 契约: `GET /api/auth/state` **免认证可达**（无 token 即 200），返回六字段固定
    /// 形状；`server_time` 必须是**字符串**（大数红线），未初始化时 `initialized=false`。
    #[tokio::test]
    async fn auth_state_is_open_without_token_and_reports_fresh_install() {
        let config = config_without_users();
        let state = make_state_with(&config, &no_env);
        let port = spawn_auth_router(state).await;

        let (status, _, body) = http_get(port, "/api/auth/state").await;
        assert_eq!(status, 200, "{body}");

        // 免认证：无 Authorization 头即可 200（端点本身不写审计、无副作用）。
        let value: Value = serde_json::from_str(&body).expect("json");
        assert_eq!(value["initialized"], false, "fresh install");
        assert_eq!(value["accounts"], 0);
        assert_eq!(value["roles"], 0);
        assert!(
            value["server_time"].is_string(),
            "server_time must be a string: {value}"
        );
        // 授权运行期未装配 → 如实报 false/0 并给出原因，绝不伪造成功。
        assert_eq!(value["trial_enabled"], false);
        assert_eq!(value["trial_days_left"], 0);
        assert!(
            value["note"].as_str().is_some(),
            "unavailable trial state must carry a reason: {value}"
        );
    }

    /// QA 契约: 已配置账号时 `initialized=true` 且计数正确（角色数 = 自定义角色数）。
    #[tokio::test]
    async fn auth_state_reports_initialized_with_accounts_and_roles() {
        let config = GatewayConfig::parse(
            r#"
[gateway]
gateway_id = "gw-auth"

[[points]]
device_id = "dev-01"
point_id = "p1"
protocol = "modbus-tcp"
address = "127.0.0.1:502"
frequency_ms = 100

[[mgmt_auth.users]]
name = "alice"
role = "system"
password_hash = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"

[[mgmt_auth.roles]]
id = "viewer"
name = "只读"
"#,
        )
        .expect("parse");
        let state = make_state_with(&config, &no_env);
        let port = spawn_auth_router(state).await;

        let (status, _, body) = http_get(port, "/api/auth/state").await;
        assert_eq!(status, 200, "{body}");
        let value: Value = serde_json::from_str(&body).expect("json");
        assert_eq!(value["initialized"], true);
        assert_eq!(value["accounts"], 1);
        assert_eq!(value["roles"], 1, "custom roles counted");
    }

    /// QA 安全: 首次 bootstrap → 200 + token（与 login 同构）落库首个账号；`displayName`
    /// 落盘展示名；口令只存 **Argon2id PHC 串**（配置里无明文、无裸摘要）。
    #[tokio::test]
    async fn bootstrap_creates_first_account_and_issues_token() {
        let config = config_without_users();
        let state = make_state_with(&config, &no_env);
        let port = spawn_auth_router(state.clone()).await;

        let (status, _, body) = http_post(
            port,
            "/api/auth/bootstrap",
            r#"{"username":"root","password":"s3cr3t-pass","displayName":"超级管理员"}"#,
        )
        .await;
        assert_eq!(status, 200, "{body}");
        let value: Value = serde_json::from_str(&body).expect("json");
        assert!(value["token"].as_str().is_some(), "token returned: {value}");
        assert_eq!(value["role"], BOOTSTRAP_ROLE_LITERAL, "first account role");

        // 落盘生效：配置快照里出现该账号（persist_config 热替换后立即可见）。
        let snapshot = state.config();
        let stored = snapshot.mgmt_auth.as_ref().expect("mgmt_auth section");
        assert_eq!(stored.users.len(), 1);
        assert_eq!(stored.users[0].name, "root");
        assert_eq!(
            stored.users[0].display_name.as_deref(),
            Some("超级管理员"),
            "displayName persisted"
        );
        assert!(
            stored.users[0].password_hash.starts_with("$argon2id$"),
            "口令必须存 Argon2id PHC 串（加盐，抗字典反查）: {}",
            stored.users[0].password_hash
        );
        assert!(
            stored.users[0].password_hash.len() > SHA256_ABC_HEX.len(),
            "PHC 串必须比裸摘要长（含 salt/参数）: {}",
            stored.users[0].password_hash
        );
        assert!(
            stored.users[0].password_hash != "s3cr3t-pass",
            "plaintext must never be stored"
        );

        // token 可用同一密钥验真，且 sub = 新账号。
        let token = value["token"].as_str().expect("token").to_string();
        let claims =
            verify(&token, state.login_auth().key(), now_unix_secs(), 60).expect("token verifies");
        assert_eq!(claims.sub, "root");
    }

    /// QA 安全（P0 AUTH-2 正向证据）: bootstrap 落盘后该账号**立即可登录**
    /// ——登录判定表跟随配置热重载，不是启动时那份陈旧快照。
    #[tokio::test]
    async fn bootstrapped_account_can_login_immediately() {
        let config = config_without_users();
        let state = make_state_with(&config, &no_env);
        let port = spawn_auth_router(state.clone()).await;

        let (status, _, body) = http_post(
            port,
            "/api/auth/bootstrap",
            r#"{"username":"root","password":"devpass1234"}"#,
        )
        .await;
        assert_eq!(status, 200, "{body}");

        // 同一进程、同一密钥、未重启 → 必须能登录（AUTH-2 的核心断言）。
        let (login_status, _, login_body) = http_post(
            port,
            "/api/auth/login",
            r#"{"username":"root","password":"devpass1234"}"#,
        )
        .await;
        assert_eq!(
            login_status, 200,
            "bootstrapped account must be able to log in: {login_body}"
        );
        let value: Value = serde_json::from_str(&login_body).expect("json");
        assert_eq!(value["role"], BOOTSTRAP_ROLE_LITERAL);
        let claims = verify(
            value["token"].as_str().expect("token"),
            state.login_auth().key(),
            now_unix_secs(),
            60,
        )
        .expect("token verifies");
        assert_eq!(claims.sub, "root", "token subject = new account");

        // 反向：错口令仍 401（热重载没有削弱口令校验）。
        let (bad_status, _, _) = http_post(
            port,
            "/api/auth/login",
            r#"{"username":"root","password":"wrong-pass"}"#,
        )
        .await;
        assert_eq!(bad_status, 401, "wrong password must stay rejected");
    }

    /// QA 安全（P0 AUTH-1）: dev 管理员（不落 config）计入「已初始化」→
    /// `GET /api/auth/state` 报 `initialized=true`，bootstrap 恒 409。
    #[tokio::test]
    async fn dev_admin_present_makes_state_initialized_and_blocks_bootstrap() {
        let config = config_without_users();
        let state = make_state_with(&config, &dev_env);
        let port = spawn_auth_router(state.clone()).await;

        let (status, _, body) = http_get(port, "/api/auth/state").await;
        assert_eq!(status, 200, "{body}");
        let value: Value = serde_json::from_str(&body).expect("json");
        assert_eq!(
            value["initialized"], true,
            "an existing usable account must mark the system initialized: {value}"
        );
        assert_eq!(value["accounts"], 1, "dev admin counted");

        let (status, _, body) = http_post(
            port,
            "/api/auth/bootstrap",
            r#"{"username":"root","password":"devpass1234"}"#,
        )
        .await;
        assert_eq!(status, 409, "{body}");
        let value: Value = serde_json::from_str(&body).expect("json");
        assert_eq!(value["error"], "already_initialized");

        // 不得借「无 config 账号」的缝隙新建 system 账号。
        let snapshot = state.config();
        assert!(
            !snapshot
                .mgmt_auth
                .as_ref()
                .is_some_and(|section| !section.users.is_empty()),
            "no account may be created: {snapshot:?}"
        );

        // 既有 dev 账号照常可登录。
        let (login_status, _, _) = http_post(
            port,
            "/api/auth/login",
            &format!(r#"{{"username":"{DEV_ADMIN_USER}","password":"{DEV_PASS}"}}"#),
        )
        .await;
        assert_eq!(login_status, 200);
    }

    /// QA 安全（P0 AUTH-2 的根因护栏）: `sync_users` 只换账号表，**不轮换签名
    /// 密钥**——已签发 token 不得因热重载失效（否则登录用户被踢下线）。
    #[test]
    fn sync_users_rebuilds_accounts_without_rotating_signing_key() {
        let seed = GatewayConfig::parse(
            r#"
[gateway]
gateway_id = "gw-auth"

[[points]]
device_id = "dev-01"
point_id = "p1"
protocol = "modbus-tcp"
address = "127.0.0.1:502"
frequency_ms = 100

[[mgmt_auth.users]]
name = "alice"
role = "system"
password_hash = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
"#,
        )
        .expect("parse seed");
        let state = make_state_with(&seed, &no_env);
        let before = state.login_auth().key();
        assert_eq!(state.login_auth().user_count(), 1);

        // 换一份「账号已变」的配置做热重载后的同步。
        let next = GatewayConfig::parse(
            r#"
[gateway]
gateway_id = "gw-auth"

[[points]]
device_id = "dev-01"
point_id = "p1"
protocol = "modbus-tcp"
address = "127.0.0.1:502"
frequency_ms = 100

[[mgmt_auth.users]]
name = "root"
role = "system"
password_hash = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
"#,
        )
        .expect("parse next");
        let mut live = state.login_auth().clone();
        assert_eq!(live.sync_users(&next), 1);
        assert_eq!(live.user_count(), 1);
        assert_eq!(
            live.key(),
            before,
            "signing key must survive a config reload"
        );
        // 旧口令随即失效、新账号可用（账号表确实被重建）。
        assert!(
            live.login("alice", "abc").is_none(),
            "stale account must be gone"
        );
        assert!(live.login("root", "abc").is_some(), "new account works");
    }

    /// QA 安全（fail-closed 实证）: 二次 bootstrap → 409 `already_initialized`，
    /// 入口永久关闭；违规请求仍按已初始化处理（不创建新账号）。
    #[tokio::test]
    async fn bootstrap_second_call_is_409_already_initialized() {
        let config = config_without_users();
        let state = make_state_with(&config, &no_env);
        let port = spawn_auth_router(state.clone()).await;

        let (first_status, _, _) = http_post(
            port,
            "/api/auth/bootstrap",
            r#"{"username":"root","password":"s3cr3t-pass"}"#,
        )
        .await;
        assert_eq!(first_status, 200);

        let (second_status, _, second_body) = http_post(
            port,
            "/api/auth/bootstrap",
            r#"{"username":"second","password":"another-pass"}"#,
        )
        .await;
        assert_eq!(second_status, 409, "{second_body}");
        let value: Value = serde_json::from_str(&second_body).expect("json");
        assert_eq!(value["error"], "already_initialized");

        // 配置里仍只有首个账号（第二个账号未创建）。
        let snapshot = state.config();
        let stored = snapshot.mgmt_auth.as_ref().expect("mgmt_auth section");
        assert_eq!(
            stored.users.len(),
            1,
            "no second account may be created: {stored:?}"
        );
        assert_eq!(stored.users[0].name, "root");
    }

    /// QA 契约: 口令 < 门槛长度 / 空用户名 → 400 `validation_failed`（字段 + 原因）；
    /// 恰好等于门槛长度放行。
    #[tokio::test]
    async fn bootstrap_rejects_short_password_and_empty_username() {
        let config = config_without_users();
        let state = make_state_with(&config, &no_env);
        let port = spawn_auth_router(state.clone()).await;

        for (index, body) in [(
            MIN_PASSWORD_LEN - 1,
            r#"{"username":"root","password":"short7"}"#,
        )]
        .iter()
        {
            let (status, _, response) = http_post(port, "/api/auth/bootstrap", body).await;
            assert_eq!(status, 400, "len {index} must be rejected: {response}");
            let value: Value = serde_json::from_str(&response).expect("json");
            assert_eq!(value["error"], "validation_failed");
            assert_eq!(value["field"], "password");
        }

        let (status, _, response) = http_post(
            port,
            "/api/auth/bootstrap",
            r#"{"username":"  ","password":"long-enough-pw"}"#,
        )
        .await;
        assert_eq!(status, 400, "{response}");
        let value: Value = serde_json::from_str(&response).expect("json");
        assert_eq!(value["field"], "username");

        // 校验失败不得创建任何账号。
        assert!(!state
            .config()
            .mgmt_auth
            .as_ref()
            .is_some_and(|s| !s.users.is_empty()));
        // 恰好等于门槛长度（8）才放行，验证阈值语义。
        let (status, _, body) = http_post(
            port,
            "/api/auth/bootstrap",
            r#"{"username":"root","password":"exactly8"}"#,
        )
        .await;
        assert_eq!(status, 200, "{body}");
    }

    /// QA 安全: 非回环 / 缺失对端信息 → 403（bootstrap 是唯一匿名写通道，fail-closed）。
    #[test]
    fn bootstrap_only_accepts_loopback_peers() {
        let loopback_v4 = "127.0.0.1:8080".parse().expect("addr");
        let loopback_v6 = "[::1]:8080".parse().expect("addr");
        let remote = "203.0.113.7:51234".parse().expect("addr");
        assert_eq!(
            is_local_peer(Some(axum::extract::ConnectInfo(loopback_v4))),
            Some(loopback_v4)
        );
        assert_eq!(
            is_local_peer(Some(axum::extract::ConnectInfo(loopback_v6))),
            Some(loopback_v6)
        );
        assert_eq!(
            is_local_peer(Some(axum::extract::ConnectInfo(remote))),
            None
        );
        assert_eq!(is_local_peer(None), None, "missing connect info = reject");
    }

    /// QA 契约: 审计必录——成功创建首个账号后，持久安全审计里出现
    /// `actor=<新账号>`、detail 含 `auth.bootstrap` 与 `first-run bootstrap`。
    #[tokio::test]
    async fn bootstrap_lands_in_persistent_audit_chain() {
        let config = config_without_users();
        let state = make_state_with(&config, &no_env);
        let dir = tempfile::tempdir().expect("tempdir");
        let logger = crate::audit::AuditLogger::open(&dir.path().join("audit.db"), Some(b"ikm"))
            .expect("open audit db");
        let logger = Arc::new(logger);
        state.daemon().set_audit_logger(Arc::clone(&logger));

        let port = spawn_auth_router(state).await;
        let (status, _, _) = http_post(
            port,
            "/api/auth/bootstrap",
            r#"{"username":"root","password":"s3cr3t-pass","idempotency_key":"batch-42"}"#,
        )
        .await;
        assert_eq!(status, 200);

        let rows = logger
            .query(&crate::audit::AuditQuery::new())
            .expect("query audit");
        let created = rows
            .iter()
            .find(|row| row.actor == "root")
            .expect("bootstrap must be audited with actor = new account");
        assert!(
            created.detail.contains("auth.bootstrap"),
            "detail must carry the action: {}",
            created.detail
        );
        assert!(
            created.detail.contains("first-run bootstrap"),
            "detail must mark first-run: {}",
            created.detail
        );
        assert!(
            !created.detail.contains("s3cr3t-pass"),
            "audit must not contain the plaintext password: {}",
            created.detail
        );
        assert!(logger.verify_chain().expect("verify").ok);
    }

    /// QA: RFC3339 换算——epoch 与 2026-01-01 两个已知锚点（纯算术，无第三方库）。
    #[test]
    fn rfc3339_utc_known_anchors() {
        assert_eq!(format_rfc3339_utc(0), "1970-01-01T00:00:00Z");
        assert_eq!(format_rfc3339_utc(1), "1970-01-01T00:00:01Z");
        // 2026-01-01T00:00:00Z = (56 年 × 365 + 14 个闰年) × 86400
        assert_eq!(format_rfc3339_utc(1_767_225_600), "2026-01-01T00:00:00Z");
        assert_eq!(
            format_rfc3339_utc(1_767_225_600 + 86_399),
            "2026-01-01T23:59:59Z"
        );
    }

    /// QA: 恒时比较与摘要——ct_eq 等长同值 true、异值 false；ct_eq 短路长度
    /// 仅发生于摘要定长场景（32 字节），sha256 已知向量锚定。
    #[test]
    fn ct_eq_and_sha256_known_answer() {
        assert!(ct_eq(b"aaaa", b"aaaa"));
        assert!(!ct_eq(b"aaaa", b"aaab"));
        assert!(!ct_eq(b"aaaa", b"aaa"));
        let digest = sha256_bytes(b"abc");
        assert_eq!(hex::encode(&digest), SHA256_ABC_HEX, "known-answer vector");
        assert_eq!(digest.len(), 32);
    }

    /// QA（task 26）: 登录成功 / 失败均落**持久安全审计**——成功 = `login`
    ///（accepted），失败 = `login_failed`（denied）；detail 不含凭据；两次事件
    /// 入防篡改哈希链且整链校验通过。
    #[tokio::test]
    async fn login_events_land_in_persistent_audit_chain() {
        let config = config_with_users();
        let state = make_state_with(&config, &no_env);
        // 挂载审计库（make_state_with 本身不挂载；task 26 生产路由 bootstrap 装配）。
        let dir = tempfile::tempdir().expect("tempdir");
        let logger = crate::audit::AuditLogger::open(&dir.path().join("audit.db"), Some(b"ikm"))
            .expect("open audit db");
        let logger = Arc::new(logger);
        state.daemon().set_audit_logger(Arc::clone(&logger));

        let port = spawn_server(state).await;

        // 成功登录（alice/system，密码 "abc"）→ 200 + login(accepted)。
        let (status, _, _) = http_post(
            port,
            "/api/auth/login",
            r#"{"username":"alice","password":"abc"}"#,
        )
        .await;
        assert_eq!(status, 200);

        // 失败登录 → 401 + login_failed(denied)。
        let (status, _, _) = http_post(
            port,
            "/api/auth/login",
            r#"{"username":"alice","password":"wrong"}"#,
        )
        .await;
        assert_eq!(status, 401);

        // 两条事件都在持久审计里：actor、event、outcome 正确，detail 无凭据。
        let rows = logger
            .query(&crate::audit::AuditQuery::new())
            .expect("query audit");
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].event, "login");
        assert_eq!(rows[0].actor, "alice");
        assert_eq!(rows[0].outcome, crate::audit::OUTCOME_ACCEPTED);
        assert_eq!(rows[1].event, "login_failed");
        assert_eq!(rows[1].outcome, crate::audit::OUTCOME_DENIED);
        for row in &rows {
            assert!(
                !row.detail.contains("abc"),
                "audit must not contain credentials"
            );
            assert!(
                !row.detail.contains("wrong"),
                "audit must not contain credentials"
            );
        }
        // 哈希链完好（登录事件正确入链）。
        assert!(logger.verify_chain().expect("verify").ok);
    }

    // ---- B-3：自定义角色运行时授权融合 ----

    /// 已知向量：SHA-256("s3cret-pass")（自定义角色账号测试用口令摘要）。
    fn sha256_hex(input: &str) -> String {
        use sha2::Digest as _;
        let mut hasher = Sha256::new();
        hasher.update(input.as_bytes());
        hex::encode(hasher.finalize())
    }

    /// 带自定义角色的配置：`role-custom` 授予 `account.view` + `receipt.view`，
    /// 账号 carol 绑定该自定义角色；bob 绑定**未定义**的 ghost 角色。
    fn config_with_custom_role() -> GatewayConfig {
        let hash = sha256_hex("s3cret-pass");
        GatewayConfig::parse(&format!(
            r#"
[gateway]
gateway_id = "gw-custom-role"

[[mgmt_auth.users]]
name = "carol"
role = "role-custom"
password_hash = "{hash}"

[[mgmt_auth.users]]
name = "bob"
role = "role-ghost"
password_hash = "{hash}"

[[mgmt_auth.roles]]
id = "role-custom"
name = "车间操作员"
permissions = ["account.view", "receipt.view"]

[[mgmt_auth.roles]]
id = "role-bad-perm"
name = "坏权限角色"
permissions = ["account.view", "nuke.all"]

[[mgmt_auth.users]]
name = "dave"
role = "role-bad-perm"
password_hash = "{hash}"
"#
        ))
        .expect("parse custom-role config")
    }

    /// QA（B-3 核心）: 自定义角色账号登录成功，token `role` = 自定义 id、
    /// `perms` = 角色 permissions 并集（签名 claim，防篡改）；内置角色账号
    /// 权限集仍等于既有矩阵（回归零变化）。
    #[test]
    fn custom_role_account_resolves_perms_into_token() {
        let config = config_with_custom_role();
        let key = IssuerKey([0x77u8; 32]);
        let mut login = MgmtAuth::new(key);
        // 仅 carol 可用（dave 绑定含未知权限 id 的角色、bob 绑定未定义角色 → 跳过）。
        let loaded = login.sync_users(&config);
        assert_eq!(
            loaded, 1,
            "only carol resolves; dave/bob skipped (fail-closed)"
        );

        let (token, role_id) = login
            .login("carol", "s3cret-pass")
            .expect("custom-role login must succeed");
        assert_eq!(role_id, "role-custom", "role id literal returned as-is");

        let claims = verify(&token, key, now_unix_secs(), 60).expect("token verifies");
        assert_eq!(claims.role, "role-custom");
        let perms = claims.perms.as_ref().expect("perms claim present");
        assert_eq!(perms.len(), 2, "union of the custom role permissions");
        assert!(perms.contains(&Permission::AccountView));
        assert!(perms.contains(&Permission::ReceiptView));
        assert!(
            !perms.contains(&Permission::DeviceWrite),
            "custom role must not inherit builtin matrix"
        );

        // 内置角色回归：权限集 = permissions_of(System) 逐项相等（零变化）。
        let builtin = config_with_users();
        let mut login2 = MgmtAuth::new(key);
        assert_eq!(login2.sync_users(&builtin), 2);
        let (token2, role2) = login2.login("alice", "abc").expect("builtin login");
        assert_eq!(role2, "system");
        let claims2 = verify(&token2, key, now_unix_secs(), 60).expect("verify");
        assert_eq!(
            claims2.perms.as_deref(),
            Some(permissions_of(Role::System)),
            "builtin role perms = existing matrix (regression zero-change)"
        );
    }

    /// QA（B-3 fail-closed）: 自定义角色 permissions 含未知权限 id → 账号整体
    /// 跳过（绝不「半解析」后放行剩余权限）；绑定未定义自定义角色 id 的账号
    /// 同样跳过（历史语义）。
    #[test]
    fn custom_role_fail_closed_on_unknown_perm_or_undefined_role() {
        let config = config_with_custom_role();
        let key = IssuerKey([0x78u8; 32]);
        let mut login = MgmtAuth::new(key);
        let loaded = login.sync_users(&config);
        assert_eq!(
            loaded, 1,
            "dave (unknown perm id) and bob (undefined role) skipped; only carol loaded"
        );
        assert!(
            login.login("dave", "s3cret-pass").is_none(),
            "unknown perm id in custom role → account not loaded (fail-closed)"
        );
        assert!(
            login.login("bob", "s3cret-pass").is_none(),
            "undefined custom role id → account not loaded (fail-closed)"
        );
    }
}

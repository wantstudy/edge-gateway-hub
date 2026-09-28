//! `licensing-server` 管理端鉴权（管理员登录 + JWT 会话 + RBAC 角色门控）。
//!
//! # 实现口径（镜像 daemon `mgmt/auth_jwt.rs` / `mgmt/auth_login.rs`，同构简化版）
//!
//! - **JWT = HS256**，用既有 `hmac` + `sha2` 手工实现，编码用既有 `base64`
//!   （`URL_SAFE_NO_PAD`，JWT 规范要求 base64url 无填充）——不引入
//!   `jsonwebtoken` 等新依赖（Cargo.lock 红线）。
//! - **签名比对必须常数时间**：`hmac::Mac::verify_slice`（内部恒时比较），
//!   禁止 `==` 直比 MAC 字节（防时序侧信道）。
//! - **口令摘要比对恒时**（XOR 折叠）：口令只存 `SHA-256` 摘要字节，明文不落盘
//!   不落日志；无匹配用户时对固定哑哈希做一次恒时比对（抹平用户枚举时序差）。
//! - **凭据来源（fail-closed）**：环境变量注入初始管理员——
//!   `IOTDAQ_ADMIN_PASSWORD_SHA256`（64 hex = SHA-256 口令摘要，生产推荐）或
//!   `IOTDAQ_ADMIN_PASSWORD`（明文，内存内即刻摘要，本地/容器引导用）；
//!   用户名 `IOTDAQ_ADMIN_USER`（缺省 `admin`）。两者皆缺 → **零账号**，
//!   登录端点全拒（401）并 warn（绝不放行默认弱凭证）。
//! - **JWT 密钥**：`IOTDAQ_JWT_SECRET`（任意非空字符串，经 SHA-256 归一为
//!   32 字节密钥）；未提供 → 回退内置 dev 常量密钥并 warn（仅限本地，生产必配）。
//! - **RBAC 四角色**（`docs/design/licensing-api.md` §4，与 daemon `mgmt/rbac.rs`、
//!   前端 `ui-kit/src/rbac.ts` 同一套 id）：`ops / lic_ops / risk / system`。
//!   服务端签发的 JWT 只发四个规范 id，别名（`admin`/`viewer`）一律拒绝。
//!
//! # 大数红线说明
//! JWT 的 `exp` / `iat` 是标准 NumericDate（RFC 7519 §2），**按规范以 JSON number
//! 编码**（秒级精度不超 JS 安全整数）；mgmt 面自产响应体的大数红线不适用于
//! 标准 JWT 互操作字段（与 daemon `auth_jwt.rs` 同一豁免口径）。

use std::time::{SystemTime, UNIX_EPOCH};

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;
use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::model::now_ns_id;

/// HS256 HMAC 实例别名。
type HmacSha256 = Hmac<Sha256>;

/// JWT 固定 header（只签发 HS256；校验侧同样只接受 HS256）。
const HEADER_JSON: &str = r#"{"alg":"HS256","typ":"JWT"}"#;

/// 管理员口令摘要（64 hex = SHA-256）环境变量（**生产推荐**）。
pub const ADMIN_PASSWORD_HASH_ENV: &str = "IOTDAQ_ADMIN_PASSWORD_SHA256";
/// 管理员明文口令环境变量（内存内即刻摘要；本地 / 容器引导用）。
pub const ADMIN_PASSWORD_ENV: &str = "IOTDAQ_ADMIN_PASSWORD";
/// 初始管理员用户名环境变量（缺省 [`DEFAULT_ADMIN_USER`]）。
pub const ADMIN_USER_ENV: &str = "IOTDAQ_ADMIN_USER";
/// JWT 签名密钥环境变量（任意非空字符串，经 SHA-256 归一为 32 字节）。
pub const JWT_SECRET_ENV: &str = "IOTDAQ_JWT_SECRET";

/// 初始管理员缺省用户名。
pub const DEFAULT_ADMIN_USER: &str = "admin";
/// 登录签发 token 的有效期（秒）。
pub const TOKEN_TTL_SECS: i64 = 3600;
/// 时钟偏移容忍（±60s，与 daemon `DEFAULT_LEEWAY_SECS` 一致）。
pub const LEEWAY_SECS: i64 = 60;

/// 开发兜底签名密钥（固定 32 字节；**仅限本地开发**——生产必须提供
/// `IOTDAQ_JWT_SECRET`，回退时启动日志会打 warn 提醒）。
const DEV_FALLBACK_KEY: IssuerKey = IssuerKey([0x5Eu8; 32]);

/// 无匹配用户时参与恒时比对的哑哈希输入（抹平用户枚举时序差）。
const DUMMY_HASH_INPUT: &[u8] = b"iotdaq-licensing-dummy-user";

// ---------------------------------------------------------------------------
// RBAC 角色（licensing-api.md §4；与 daemon mgmt/rbac.rs 同一套 id）
// ---------------------------------------------------------------------------

/// 管理面四角色（与 daemon `mgmt/rbac.rs`、前端 `ui-kit/src/rbac.ts` 同一套 id）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Role {
    /// 运营（只读 + 发放）。
    Ops,
    /// 授权运营（发放 / 废弃 / 重发，高危操作承担者）。
    LicOps,
    /// 风控（回执异常 / 审计只读）。
    Risk,
    /// 系统（租户创建 / 策略配置等全部管理动作）。
    System,
}

impl Role {
    /// 角色字面量（JWT `role` claim 取值；与前端 `ROLES` 完全一致）。
    pub fn as_str(self) -> &'static str {
        match self {
            Role::Ops => "ops",
            Role::LicOps => "lic_ops",
            Role::Risk => "risk",
            Role::System => "system",
        }
    }

    /// 解析角色字面量（大小写敏感、**不接受别名**：`admin`/`viewer` 等历史命名
    /// 一律 `None`，防两端漂移）。
    #[allow(clippy::should_implement_trait)]
    pub fn from_str(raw: &str) -> Option<Self> {
        match raw {
            "ops" => Some(Role::Ops),
            "lic_ops" => Some(Role::LicOps),
            "risk" => Some(Role::Risk),
            "system" => Some(Role::System),
            _ => None,
        }
    }
}

// ---------------------------------------------------------------------------
// JWT（HS256）签发与校验
// ---------------------------------------------------------------------------

/// HS256 签名密钥（32 字节；注入式，不做 IO、不落盘）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IssuerKey(pub [u8; 32]);

/// JWT claims（sub / role / exp / iat / jti；nbf 不使用——登录即签即用）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JwtClaims {
    /// 主体（登录用户名，即审计 actor）。
    pub sub: String,
    /// 规范角色字面量（[`Role::as_str`] 四 id 之一）。
    pub role: String,
    /// 过期时刻（秒级 Unix，NumericDate）。
    pub exp: i64,
    /// 签发时刻（秒级 Unix，NumericDate）。
    pub iat: i64,
    /// Token 唯一标识（审计去重键）。
    pub jti: String,
}

/// HMAC-SHA256（32 字节密钥；`new_from_slice` 对定长输入不会失败，仍诚实兜底）。
fn hmac_sha256(key: &IssuerKey, msg: &[u8]) -> Result<Vec<u8>, String> {
    let mut mac = HmacSha256::new_from_slice(&key.0).map_err(|e| format!("hmac init: {e}"))?;
    mac.update(msg);
    Ok(mac.finalize().into_bytes().to_vec())
}

/// 签发 HS256 JWT：`b64url(header) + "." + b64url(claims) + "." + b64url(HMAC)`。
fn jwt_sign(claims: &JwtClaims, key: IssuerKey) -> Result<String, String> {
    let header_b64 = URL_SAFE_NO_PAD.encode(HEADER_JSON);
    let payload_json = serde_json::to_vec(&claims).map_err(|e| format!("claims encode: {e}"))?;
    let payload_b64 = URL_SAFE_NO_PAD.encode(payload_json);
    let signing_input = format!("{header_b64}.{payload_b64}");
    let sig_b64 = URL_SAFE_NO_PAD.encode(hmac_sha256(&key, signing_input.as_bytes())?);
    Ok(format!("{signing_input}.{sig_b64}"))
}

/// 校验 JWT 并返回 claims（结构 → alg → 恒时验签 → claims → 时间窗）。
///
/// 时间窗：`now > exp + LEEWAY_SECS` → 过期；`now < iat - LEEWAY_SECS` → 未来签发。
/// 失败一律 `Err(String)`（调用方统一转 401，不区分原因——防探测）。
fn jwt_verify(token: &str, key: IssuerKey, now_secs: i64) -> Result<JwtClaims, String> {
    // ① 结构：三段式。
    let segments: Vec<&str> = token.split('.').collect();
    let [header_b64, payload_b64, sig_b64] = segments.as_slice() else {
        return Err(format!(
            "malformed token: expected 3 segments, got {}",
            segments.len()
        ));
    };

    // ② header：必须显式声明 alg=HS256（alg none / HS512 等替换攻击直接拒）。
    let header_raw = URL_SAFE_NO_PAD
        .decode(header_b64)
        .map_err(|e| format!("header base64: {e}"))?;
    let header: serde_json::Value =
        serde_json::from_slice(&header_raw).map_err(|e| format!("header json: {e}"))?;
    let alg = header
        .get("alg")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| "header missing alg".to_string())?;
    if alg != "HS256" {
        return Err(format!("alg {alg} not allowed (HS256 only)"));
    }

    // ③ 签名：对「所呈现的」header/payload 原文逐字节重建签名输入后恒时比对。
    let sig_raw = URL_SAFE_NO_PAD
        .decode(sig_b64)
        .map_err(|e| format!("signature base64: {e}"))?;
    let signing_input = format!("{header_b64}.{payload_b64}");
    let mut mac = HmacSha256::new_from_slice(&key.0).map_err(|e| format!("hmac init: {e}"))?;
    mac.update(signing_input.as_bytes());
    mac.verify_slice(&sig_raw)
        .map_err(|_| "MAC verification failed".to_string())?;

    // ④ claims。
    let payload_raw = URL_SAFE_NO_PAD
        .decode(payload_b64)
        .map_err(|e| format!("payload base64: {e}"))?;
    let claims: JwtClaims =
        serde_json::from_slice(&payload_raw).map_err(|e| format!("payload json: {e}"))?;
    if claims.sub.trim().is_empty() {
        return Err("missing claim: sub".to_string());
    }
    if Role::from_str(&claims.role).is_none() {
        return Err(format!("unknown role: {}", claims.role));
    }

    // ⑤ 时间窗（饱和运算防对抗性 i64 极值溢出）。
    if now_secs > claims.exp.saturating_add(LEEWAY_SECS) {
        return Err("token expired".to_string());
    }
    if now_secs < claims.iat.saturating_sub(LEEWAY_SECS) {
        return Err("token issued in the future".to_string());
    }
    Ok(claims)
}

// ---------------------------------------------------------------------------
// 摘要与恒时比较
// ---------------------------------------------------------------------------

/// SHA-256 摘要字节。
fn sha256_bytes(data: &[u8]) -> Vec<u8> {
    let mut hasher = Sha256::new();
    hasher.update(data);
    hasher.finalize().to_vec()
}

/// 口令 → 64 位小写 hex SHA-256 摘要（`admin_account.password_sha256` 的存取形态）。
pub fn sha256_hex(input: &str) -> String {
    sha256_bytes(input.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// 恒时字节比较（XOR 折叠，无提前退出；与 daemon `auth_login.rs` 同一口径）。
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

/// 解码 64 hex 字符为 32 字节（口令摘要用；手写解码避免引入 `hex` 依赖）。
fn hex_decode_32(raw: &str) -> Option<[u8; 32]> {
    let trimmed = raw.trim();
    if trimmed.len() != 64 {
        return None;
    }
    let mut out = [0u8; 32];
    let bytes = trimmed.as_bytes();
    for (i, chunk) in bytes.chunks(2).enumerate() {
        let hi = (chunk[0] as char).to_digit(16)?;
        let lo = (chunk[1] as char).to_digit(16)?;
        out[i] = ((hi << 4) | lo) as u8;
    }
    Some(out)
}

/// 当前 UTC 秒级 Unix 时间（时钟早于纪元按 0 处理，不 panic）。
pub fn now_unix_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// AdminAuth：账号表 + JWT 签发密钥
// ---------------------------------------------------------------------------

/// 单个登录账号（密码只存 SHA-256 摘要字节）。
#[derive(Debug, Clone)]
pub struct LoginUser {
    /// 用户名（精确匹配）。
    pub name: String,
    /// 规范角色。
    pub role: Role,
    /// `SHA-256(password)` 摘要字节。
    pub password_hash_bytes: Vec<u8>,
}

/// 已认证管理端身份（JWT 校验通过后的 claims 视图）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthedAdmin {
    /// 登录用户名（审计 actor）。
    pub sub: String,
    /// 规范角色。
    pub role: Role,
}

/// 管理员账号行（store `admin_account` 表的内存视图；口令只存 64-hex SHA-256 摘要）。
///
/// 这是**账号 / 角色可配置**的持久化单元：`login` 实时读本表判定，`/admin/accounts*`
/// 端点对其增删改查。字段与 `store::admin_account` 表一一对应。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdminAccount {
    /// 账号（登录名，主键）。
    pub account: String,
    /// 展示名（姓名）。
    pub display_name: String,
    /// 规范角色字面量（`ops / lic_ops / risk / system`）。
    pub role: String,
    /// `SHA-256(password)` 的 64-hex 摘要（明文绝不落盘）。
    pub password_sha256: String,
    /// 状态（`active` / `disabled`）。
    pub status: String,
    /// 最近登录时刻（UTC 秒；从未登录为 `None`）。
    pub last_login_at: Option<i64>,
    /// 创建时刻（UTC 秒）。
    pub created_at: i64,
    /// 更新时刻（UTC 秒）。
    pub updated_at: i64,
}

impl AdminAccount {
    /// 账号是否处于启用态（仅启用态可登录）。
    pub fn is_active(&self) -> bool {
        self.status == "active"
    }

    /// 口令摘要字节（hex 非法时 `None`，调用方退化为哑哈希比对）。
    fn password_bytes(&self) -> Option<Vec<u8>> {
        hex_decode_32(&self.password_sha256).map(|b| b.to_vec())
    }
}

/// 管理端登录器：账号表 + JWT 签发密钥（注入式，不做 IO）。
#[derive(Debug, Clone)]
pub struct AdminAuth {
    key: IssuerKey,
    users: Vec<LoginUser>,
}

impl AdminAuth {
    /// 以指定签名密钥构造（无任何账号 → 登录全拒，fail-closed）。
    pub fn new(key: IssuerKey) -> Self {
        AdminAuth {
            key,
            users: Vec::new(),
        }
    }

    /// 追加一个账号（`password` 为明文，本方法内即刻摘要，明文不驻留）。
    ///
    /// 空用户名 / 空口令 → warn 跳过该账号，绝不放入弱凭证。
    pub fn with_user(mut self, name: &str, role: Role, password: &str) -> Self {
        if name.trim().is_empty() || password.is_empty() {
            tracing::warn!(
                user = %name,
                "admin auth: empty username or password; user skipped (fail-closed)"
            );
            return self;
        }
        self.users.push(LoginUser {
            name: name.trim().to_string(),
            role,
            password_hash_bytes: sha256_bytes(password.as_bytes()),
        });
        self
    }

    /// 追加一个以 64-hex SHA-256 摘要声明的账号（生产配置形态）。
    ///
    /// 非法 hex / 空用户名 → warn 跳过（fail-closed）。
    pub fn with_user_hash(mut self, name: &str, role: Role, password_hash_hex: &str) -> Self {
        match hex_decode_32(password_hash_hex) {
            Some(bytes) if !name.trim().is_empty() => self.users.push(LoginUser {
                name: name.trim().to_string(),
                role,
                password_hash_bytes: bytes.to_vec(),
            }),
            _ => tracing::warn!(
                user = %name,
                "admin auth: invalid password hash (need 64 hex chars) or empty name; \
                 user skipped (fail-closed)"
            ),
        }
        self
    }

    /// 已装载的账号数（0 = 登录全拒）。
    pub fn user_count(&self) -> usize {
        self.users.len()
    }

    /// JWT 签名密钥（供测试断言）。
    pub fn key(&self) -> IssuerKey {
        self.key
    }

    /// 签发一枚管理员 JWT（HS256，1h TTL）。
    pub fn issue_token(&self, sub: &str, role: Role) -> Option<String> {
        let now = now_unix_secs();
        let claims = JwtClaims {
            sub: sub.to_string(),
            role: role.as_str().to_string(),
            exp: now.saturating_add(TOKEN_TTL_SECS),
            iat: now,
            jti: now_ns_id("jti"),
        };
        jwt_sign(&claims, self.key).ok()
    }

    /// 登录判定：恒时比对口令摘要 → 签发 JWT（**内存账号表路径**；真实部署走
    /// [`AdminAuth::login_with_accounts`] 实时读 store）。
    ///
    /// 成功返回 `(token, role)`；用户不存在 / 口令错 / 签发失败一律 `None`
    /// （调用方统一转 401，不区分原因，防账号枚举）。
    pub fn login(&self, username: &str, password: &str) -> Option<(String, Role)> {
        let user = self.users.iter().find(|u| u.name == username);
        let supplied = sha256_bytes(password.as_bytes());
        // 无匹配用户时对固定哑哈希做一次恒时比对（时长与命中路径对齐）。
        let stored: Vec<u8> = match user {
            Some(u) => u.password_hash_bytes.clone(),
            None => sha256_bytes(DUMMY_HASH_INPUT),
        };
        if !ct_eq(&supplied, &stored) {
            return None;
        }
        let user = user?;
        Some((self.issue_token(&user.name, user.role)?, user.role))
    }

    /// 实时登录：对 **store 账号表**判定（口令恒时比对），命中**启用态**账号才签发 JWT。
    ///
    /// 账号不存在 / 口令错 / 账号停用 / 角色非法 / 摘要损坏 / 签发失败一律 `None`
    /// （调用方统一转 401，不区分原因，防账号枚举）。
    pub fn login_with_accounts(
        &self,
        username: &str,
        password: &str,
        accounts: &[AdminAccount],
    ) -> Option<(String, Role)> {
        let username = username.trim();
        let user = accounts
            .iter()
            .find(|a| a.account == username && a.is_active());
        let supplied = sha256_bytes(password.as_bytes());
        // 无匹配账号时对固定哑哈希做一次恒时比对（时长与命中路径对齐）。
        let stored = match user {
            Some(u) => u
                .password_bytes()
                .unwrap_or_else(|| sha256_bytes(DUMMY_HASH_INPUT)),
            None => sha256_bytes(DUMMY_HASH_INPUT),
        };
        if !ct_eq(&supplied, &stored) {
            return None;
        }
        let user = user?;
        let role = Role::from_str(&user.role)?;
        Some((self.issue_token(&user.account, role)?, role))
    }

    /// 校验 Bearer token 并返回已认证身份（签名 / 结构 / 时间窗任一失败 → `None`）。
    pub fn verify_token(&self, token: &str) -> Option<AuthedAdmin> {
        let claims = jwt_verify(token, self.key, now_unix_secs()).ok()?;
        let role = Role::from_str(&claims.role)?;
        Some(AuthedAdmin {
            sub: claims.sub,
            role,
        })
    }

    /// 从环境变量装配（生产入口；`env` 注入读取函数，测试可注入受控值）。
    ///
    /// - 凭据：`IOTDAQ_ADMIN_PASSWORD_SHA256`（优先）→ `IOTDAQ_ADMIN_PASSWORD` →
    ///   皆缺 → 零账号（登录全拒）+ warn；
    /// - JWT 密钥：`IOTDAQ_JWT_SECRET` → SHA-256 归一 32 字节；缺省 → dev 兜底 + warn。
    pub fn from_env_fn(env: &dyn Fn(&str) -> Option<String>) -> Self {
        // JWT 密钥解析。
        let secret = env(JWT_SECRET_ENV)
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        let (key, dev_key) = match secret {
            Some(raw) => (
                // SHA-256 输出恒为 32 字节；`unwrap_or` 分支理论不可达（纯兜底）。
                IssuerKey(
                    sha256_bytes(raw.as_bytes())
                        .try_into()
                        .unwrap_or(DEV_FALLBACK_KEY.0),
                ),
                false,
            ),
            None => {
                tracing::warn!(
                    "admin auth: {JWT_SECRET_ENV} not set; using built-in dev signing key \
                     (LOCAL ONLY — production must configure a secret)"
                );
                (DEV_FALLBACK_KEY, true)
            }
        };

        let username = env(ADMIN_USER_ENV)
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| DEFAULT_ADMIN_USER.to_string());

        let mut auth = AdminAuth::new(key);
        let hash_env = env(ADMIN_PASSWORD_HASH_ENV)
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        let pass_env = env(ADMIN_PASSWORD_ENV)
            .map(|s| s.to_string())
            .filter(|s| !s.is_empty());
        if let Some(hex) = hash_env {
            auth = auth.with_user_hash(&username, Role::System, &hex);
        } else if let Some(pass) = pass_env {
            auth = auth.with_user(&username, Role::System, &pass);
        } else {
            tracing::warn!(
                "admin auth: no credentials configured ({ADMIN_PASSWORD_HASH_ENV} / \
                 {ADMIN_PASSWORD_ENV} unset); /admin/auth/login is fail-closed (all logins rejected)"
            );
        }

        if dev_key {
            tracing::warn!("admin auth: dev JWT signing key in use (LOCAL ONLY)");
        }
        auth
    }

    /// 从环境变量解析初始管理员凭据（**store 首次 seeding 用**）。
    ///
    /// 返回 `(username, password_sha256_hex, role)`；凭据缺失 / 摘要非法 → `None`
    /// （fail-closed：绝不落弱凭证）。摘要路优先，明文路兜底（内存内即刻摘要）。
    pub fn bootstrap_from_env_fn(
        env: &dyn Fn(&str) -> Option<String>,
    ) -> Option<(String, String, Role)> {
        let username = env(ADMIN_USER_ENV)
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| DEFAULT_ADMIN_USER.to_string());
        let hash_env = env(ADMIN_PASSWORD_HASH_ENV)
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        let pass_env = env(ADMIN_PASSWORD_ENV)
            .map(|s| s.to_string())
            .filter(|s| !s.is_empty());
        if let Some(hex) = hash_env {
            if hex_decode_32(&hex).is_some() {
                return Some((username, hex.to_lowercase(), Role::System));
            }
            tracing::warn!(
                "admin auth: {ADMIN_PASSWORD_HASH_ENV} is not a valid 64-hex digest; \
                 admin account seeding skipped (fail-closed)"
            );
            return None;
        }
        if let Some(pass) = pass_env {
            return Some((username, sha256_hex(&pass), Role::System));
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// 测试密钥（固定 32 字节）。
    const KEY: IssuerKey = IssuerKey([0x42u8; 32]);
    /// 测试基准时刻（jwt_verify 直测用）。
    const NOW: i64 = 1_700_000_000;

    /// 构造有效 claims。
    fn claims() -> JwtClaims {
        JwtClaims {
            sub: "admin".to_string(),
            role: "system".to_string(),
            exp: NOW + 600,
            iat: NOW,
            jti: "jti-1".to_string(),
        }
    }

    /// QA Happy: 签发 → 校验往返，claims 无损；三段式 + base64url 无填充。
    #[test]
    fn sign_verify_roundtrip_preserves_claims() {
        let token = jwt_sign(&claims(), KEY).expect("sign");
        let verified = jwt_verify(&token, KEY, NOW).expect("verify");
        assert_eq!(verified, claims());
        let parts: Vec<&str> = token.split('.').collect();
        assert_eq!(parts.len(), 3);
        assert!(!token.contains('='), "base64url must be unpadded");
    }

    /// QA 安全: 篡改 payload / 篡改签名 / 错误密钥 → 一律拒绝。
    #[test]
    fn tampered_tokens_rejected() {
        let token = jwt_sign(&claims(), KEY).expect("sign");
        let parts: Vec<&str> = token.split('.').collect();

        // 篡改 payload（改 sub 首字节 → 签名不再匹配）。
        let mut payload = URL_SAFE_NO_PAD.decode(parts[1]).expect("payload b64");
        payload[0] ^= 0x01;
        let tampered = format!(
            "{}.{}.{}",
            parts[0],
            URL_SAFE_NO_PAD.encode(&payload),
            parts[2]
        );
        assert!(jwt_verify(&tampered, KEY, NOW).is_err());

        // 错误密钥。
        let other = jwt_sign(&claims(), IssuerKey([0x00u8; 32])).expect("sign");
        assert!(jwt_verify(&other, KEY, NOW).is_err());

        // 垃圾输入。
        for bad in ["not-a-jwt", "a.b", "a.b.c.d", ""] {
            assert!(
                jwt_verify(bad, KEY, NOW).is_err(),
                "{bad:?} must be rejected"
            );
        }
    }

    /// QA 安全: alg 替换攻击（none / 缺 alg）→ 拒。
    #[test]
    fn algorithm_substitution_rejected() {
        let payload = json!({
            "sub": "admin", "role": "system",
            "exp": NOW + 600, "iat": NOW, "jti": "j",
        });
        let craft = |header: &serde_json::Value| -> String {
            let h = URL_SAFE_NO_PAD.encode(header.to_string());
            let p = URL_SAFE_NO_PAD.encode(payload.to_string());
            let input = format!("{h}.{p}");
            let mut mac = HmacSha256::new_from_slice(&KEY.0).expect("hmac");
            mac.update(input.as_bytes());
            format!(
                "{input}.{}",
                URL_SAFE_NO_PAD.encode(mac.finalize().into_bytes().as_slice())
            )
        };
        assert!(jwt_verify(&craft(&json!({"alg":"none","typ":"JWT"})), KEY, NOW).is_err());
        assert!(jwt_verify(&craft(&json!({"typ":"JWT"})), KEY, NOW).is_err());
    }

    /// QA: 过期 / 未来签发 → 拒（容忍窗边界语义与 daemon 一致）。
    #[test]
    fn time_window_enforced() {
        let mut c = claims();
        c.exp = NOW - 61;
        let token = jwt_sign(&c, KEY).expect("sign");
        assert!(
            jwt_verify(&token, KEY, NOW).is_err(),
            "beyond leeway must expire"
        );

        let mut c = claims();
        c.exp = NOW - 60;
        let token = jwt_sign(&c, KEY).expect("sign");
        assert!(
            jwt_verify(&token, KEY, NOW).is_ok(),
            "at leeway boundary passes"
        );

        let mut c = claims();
        c.iat = NOW + 61;
        let token = jwt_sign(&c, KEY).expect("sign");
        assert!(jwt_verify(&token, KEY, NOW).is_err(), "future iat rejected");
    }

    /// QA: 未知角色 claim → 拒（不静默映射）。
    #[test]
    fn unknown_role_claim_rejected() {
        let mut c = claims();
        c.role = "admin".to_string();
        let token = jwt_sign(&c, KEY).expect("sign");
        assert!(jwt_verify(&token, KEY, NOW).is_err());
    }

    /// QA: 登录正 / 负路径 + 防账号枚举（错口令与未知用户均 None）。
    #[test]
    fn login_paths() {
        let auth = AdminAuth::new(KEY)
            .with_user("admin", Role::System, "secret-pw")
            .with_user("oliver", Role::Ops, "ops-pw");
        assert_eq!(auth.user_count(), 2);

        let (token, role) = auth.login("admin", "secret-pw").expect("login ok");
        assert_eq!(role, Role::System);
        let authed = auth.verify_token(&token).expect("token valid");
        assert_eq!(authed.sub, "admin");
        assert_eq!(authed.role, Role::System);

        assert!(auth.login("admin", "wrong").is_none(), "wrong password");
        assert!(auth.login("nobody", "secret-pw").is_none(), "unknown user");
        assert!(auth.login("  ", "secret-pw").is_none(), "blank username");
    }

    /// QA: 弱凭证防护——空用户名 / 空口令账号被跳过（fail-closed）。
    #[test]
    fn weak_credentials_skipped() {
        let auth = AdminAuth::new(KEY)
            .with_user("", Role::System, "pw")
            .with_user("u", Role::System, "")
            .with_user("ok", Role::System, "pw");
        assert_eq!(auth.user_count(), 1);
    }

    /// QA: 64-hex 摘要账号可登录（生产配置形态）；非法 hex 跳过。
    #[test]
    fn user_hash_roundtrip() {
        let digest_hex = {
            let d = sha256_bytes(b"abc");
            d.iter().map(|b| format!("{b:02x}")).collect::<String>()
        };
        assert_eq!(digest_hex.len(), 64);
        let auth = AdminAuth::new(KEY).with_user_hash("admin", Role::System, &digest_hex);
        assert!(auth.login("admin", "abc").is_some());
        assert!(auth.login("admin", "abd").is_none());

        let bad = AdminAuth::new(KEY).with_user_hash("x", Role::System, "zz");
        assert_eq!(bad.user_count(), 0, "invalid hex must be skipped");
    }

    /// QA: hex_decode_32 已知向量（SHA-256("abc")）。
    #[test]
    fn hex_decode_known_answer() {
        let decoded =
            hex_decode_32("ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad")
                .expect("decode");
        assert_eq!(decoded, sha256_bytes(b"abc").as_slice());
        assert!(hex_decode_32("zz").is_none());
        assert!(hex_decode_32("00".repeat(31).as_str()).is_none());
    }

    /// QA: from_env_fn —— 摘要路优先、明文路兜底、皆缺 fail-closed、JWT 密钥归一。
    #[test]
    fn env_loading_matrix() {
        let digest_hex = sha256_bytes(b"prod-pw")
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>();

        // 摘要路。
        let auth = AdminAuth::from_env_fn(&|k: &str| match k {
            ADMIN_PASSWORD_HASH_ENV => Some(digest_hex.clone()),
            JWT_SECRET_ENV => Some("prod-secret".to_string()),
            _ => None,
        });
        assert_eq!(auth.user_count(), 1);
        assert!(auth.login("admin", "prod-pw").is_some());
        assert!(!auth.key().0.starts_with(&DEV_FALLBACK_KEY.0[..1]));

        // 明文路。
        let auth = AdminAuth::from_env_fn(&|k: &str| match k {
            ADMIN_PASSWORD_ENV => Some("plain-pw".to_string()),
            _ => None,
        });
        assert!(auth.login("admin", "plain-pw").is_some());

        // 皆缺 → 零账号（fail-closed）。
        let auth = AdminAuth::from_env_fn(&|_| None);
        assert_eq!(auth.user_count(), 0);
        assert!(auth.login("admin", "anything").is_none());

        // 自定义用户名。
        let auth = AdminAuth::from_env_fn(&|k: &str| match k {
            ADMIN_USER_ENV => Some("root".to_string()),
            ADMIN_PASSWORD_ENV => Some("pw".to_string()),
            _ => None,
        });
        assert!(auth.login("root", "pw").is_some());
        assert!(auth.login("admin", "pw").is_none());
    }

    /// 恒时比较语义：等长同值 true、异值 false、异长 false。
    #[test]
    fn ct_eq_semantics() {
        assert!(ct_eq(b"aaaa", b"aaaa"));
        assert!(!ct_eq(b"aaaa", b"aaab"));
        assert!(!ct_eq(b"aaaa", b"aaa"));
    }
}

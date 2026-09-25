//! task 57 全量接线 — 管理面登录 / JWT 签发 / whoami（凭证两路 fail-closed）。
//!
//! ## 凭证两路（fail-closed）
//! - **生产路**：`config.toml` 顶层可选 `[mgmt_auth]` 段
//!   （`users = [{name, role, password_hash}]`，`password_hash` 为
//!   `SHA-256(password)` 的 hex；比对走恒时字节比较）。角色字面量经
//!   [`super::rbac::Role::from_str`] 解析，未知角色该账号**跳过**（fail-closed）。
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
//! - token TTL = [`TOKEN_TTL_SECS`]（1h）；`jti` 用 uuid v4。
//! - **大数红线**：本模块自产响应体中整数（`exp`）一律字符串编码；
//!   JWT claims 内的 `exp`/`iat` 按 RFC 7519 NumericDate（标准互操作字段，例外）。

use serde::Deserialize;
use sha2::{Digest, Sha256};

use axum::body::Bytes;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Json, Response};

use super::auth_jwt::{now_unix_secs, sign, Claims, IssuerKey, JwtError};
use super::rbac::{AuthedRole, RbacAuth, Role};
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

/// 单个登录账号（密码只存 SHA-256 摘要字节）。
#[derive(Debug, Clone)]
pub struct LoginUser {
    /// 用户名（精确匹配）。
    pub name: String,
    /// 已解析的规范角色。
    pub role: Role,
    /// `SHA-256(password)` 摘要字节（hex 解码后）。
    pub password_hash_bytes: Vec<u8>,
}

/// 管理面登录器：账号表 + JWT 签发密钥（注入式，不做 IO）。
#[derive(Debug, Clone)]
pub struct MgmtAuth {
    key: IssuerKey,
    users: Vec<LoginUser>,
}

impl MgmtAuth {
    /// 以指定签名密钥构造（无任何账号 → 登录全拒，fail-closed）。
    pub fn new(key: IssuerKey) -> Self {
        Self {
            key,
            users: Vec::new(),
        }
    }

    /// 追加一个账号（`password_hash_hex` 为 SHA-256 摘要的 hex；
    /// 非法 hex / 空用户名 → warn 跳过该账号，绝不放入弱凭证）。
    pub fn with_user(mut self, name: &str, role: Role, password_hash_hex: &str) -> Self {
        match hex::decode(password_hash_hex.trim()) {
            Ok(bytes) if !name.trim().is_empty() => self.users.push(LoginUser {
                name: name.trim().to_string(),
                role,
                password_hash_bytes: bytes,
            }),
            _ => tracing::warn!(
                user = %name,
                "mgmt auth: invalid password_hash (must be hex) or empty name; user skipped (fail-closed)"
            ),
        }
        self
    }

    /// 追加 dev 管理员（role = system；**仅限本地开发，生产禁用**）。
    pub fn with_dev_admin(mut self, password: &str) -> Self {
        self.users.push(LoginUser {
            name: DEV_ADMIN_USER.to_string(),
            role: Role::System,
            password_hash_bytes: sha256_bytes(password.as_bytes()),
        });
        self
    }

    /// 签名密钥（供测试与手动校验路径）。
    pub fn key(&self) -> IssuerKey {
        self.key
    }

    /// 已装载的账号数（0 = 登录全拒）。
    pub fn user_count(&self) -> usize {
        self.users.len()
    }

    /// 登录判定：恒时比对密码摘要 → 签发 JWT。
    ///
    /// 成功返回 `(token, role)`；用户不存在 / 密码错 / 签发失败一律 `None`
    /// （调用方统一转 401，不区分原因）。
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
        let (token, _) = self.issue_token(&user.name, user.role).ok()?;
        Some((token, user.role))
    }

    /// 以给定主体 / 角色签发 JWT（`exp = now + TOKEN_TTL_SECS`，`jti` = uuid v4）。
    ///
    /// 返回 `(token, exp)`；`exp` 供测试断言与日志取用。
    pub fn issue_token(&self, sub: &str, role: Role) -> Result<(String, i64), JwtError> {
        let now = now_unix_secs();
        let exp = now.saturating_add(TOKEN_TTL_SECS);
        let claims = Claims {
            sub: sub.to_string(),
            role,
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

    // 生产路：config `[mgmt_auth]` 账号表（未知角色 / 非法哈希的账号跳过，fail-closed）。
    let mut loaded = 0usize;
    if let Some(section) = &config.mgmt_auth {
        for user in &section.users {
            match Role::from_str(&user.role) {
                Some(role) => {
                    let before = login.user_count();
                    login = login.with_user(&user.name, role, &user.password_hash);
                    if login.user_count() > before {
                        loaded += 1;
                    }
                }
                None => tracing::warn!(
                    user = %user.name,
                    role = %user.role,
                    "mgmt auth: unknown role in config; user skipped (fail-closed)"
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

    // 密钥兜底提示放在最后，保证日志里两条 warn 都可见。
    if dev_key {
        tracing::warn!("mgmt auth: dev JWT signing key in use (LOCAL ONLY)");
    }

    (RbacAuth::new(key), login)
}

// ---- 摘要与恒时比较 ----

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
    let outcome = parsed
        .filter(|req| !req.username.is_empty() && !req.password.is_empty())
        .and_then(|req| state.login_auth().login(&req.username, &req.password));
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
        Some((token, role)) => Json(serde_json::json!({
            "token": token,
            "role": role.as_str(),
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
        "role": authed.role.as_str(),
        "exp": authed.claims.exp.to_string(),
    }))
    .into_response()
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
        MgmtState::new(daemon, Arc::new(config.clone())).with_auth(auth, login)
    }

    /// 恒定无 env（生产路专属）。
    fn no_env(key: &str) -> Option<String> {
        let _ = key;
        None
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
        assert_eq!(claims.role, Role::System);
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
}

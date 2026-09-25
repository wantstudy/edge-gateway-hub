//! 设置页真实落盘端点（web-console 设置页联调缺口补齐：此前 `GET /api/settings`
//! 404，表单只能本地态）。
//!
//! ## 端点
//! - `GET /api/settings`          设置只读视图（读，开放——与 /api/status 同口径）；
//! - `PUT /api/settings`          设置持久化写（`device.write`，仅 system）；
//! - `GET /api/settings/backups`  配置备份清单（读，开放；空列表合法）。
//!
//! ## GET 响应形状（五组页签：基础 / OEM / 网络 / 存储 / 安全；snake_case）
//! ```json
//! {
//!   "basic":    { "gateway_id": "gw-1", "data_dir": "./data" },
//!   "oem":      { "managed_by": "...", "note": "..." },
//!   "network":  { "outlets": [ { "id", "name", "broker", "topic_prefix", "qos",
//!                               "tls", "encoding", "username", "password" } ] },
//!   "storage":  { "sqlite_path", "max_size_mb", "retention_days" },
//!   "security": { "tls_cert_path", "tls_key_path", "web_auth_enabled",
//!                 "activation_code_set", "mgmt_users": [ { "name", "role" } ] },
//!   "config_version": "3"
//! }
//! ```
//!
//! ## 脱敏红线（敏感字段一律不回明文）
//! - 出口 `password`：已配置 → `"<redacted>"`，未配置 → `null`；
//! - 激活码：**整个字段不出现**，只回 `activation_code_set` 布尔位（与
//!   config.rs `LicensingSection` 的 skip_serializing 纪律一致）；
//! - `[mgmt_auth]` 用户只回 `name` + `role`，**password_hash 绝不出现**。
//!
//! ## PUT 契约（writeapi 既有范式）
//! 1. **鉴权**：`AuthedRole`（401）→ `ensure(DeviceWrite)`（403，入审计）；
//! 2. **白名单**：顶层组 ⊆ `{basic, storage, security}`；组内键同样白名单，
//!    任何未知字段 → 400（`validation_failed`，字段带组前缀）；`oem` / `network`
//!    视为已知但**只读**（不在白名单 → 400 + 原因说明）；
//! 3. **落盘**：全进程写锁 → 从热快照克隆改字段 → `pages::persist_config`
//!    （license 配额闸门 → 写前备份 `config.toml.bak-*` → 原子落盘 → 热快照
//!    即时替换）→ 失败 fail-closed 保留原配置；
//! 4. **生效**：落盘成功后发布 `config_reloaded` 管理事件（SSE 三源合流）；
//! 5. **响应**：`{accepted, config_version, backup}`（`config_version` /
//!    `backup` 均字符串或 `null`；backup = 本次写前备份的文件名）。
//!
//! ## 大数红线
//! `config_version` / `max_size_mb` / `retention_days` / `qos` / 字节数 /
//! mtime 一律字符串编码（手工 `serde_json::json!`，不走 derive）。

use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use axum::body::Bytes;
use axum::extract::State;
use axum::response::{IntoResponse, Json, Response};
use serde_json::{json, Map, Value};

use super::pages;
use super::rbac::{AuthedRole, Permission};
use super::remote_ops::{OpsAction, OUTCOME_BAD_REQUEST, OUTCOME_DENIED};
use super::writeapi;
use super::{MgmtEvent, MgmtState};
use crate::config::OutletEncoding;

/// PUT 允许的顶层设置组（白名单第一层）。
const SETTING_GROUPS: &[&str] = &["basic", "storage", "security"];
/// 各组内允许的字段（白名单第二层；`oem` / `network` 已知但只读，不在列）。
const BASIC_FIELDS: &[&str] = &["gateway_id"];
const STORAGE_FIELDS: &[&str] = &["sqlite_path", "max_size_mb", "retention_days"];
const SECURITY_FIELDS: &[&str] = &["web_auth_enabled", "tls_cert_path", "tls_key_path"];

// ---- GET /api/settings ----

/// `GET /api/settings` → 设置只读视图（从 MgmtState 配置快照读，不新开文件）。
pub async fn get_settings(State(state): State<MgmtState>) -> Response {
    let config = state.config();
    let gateway = &config.gateway;
    let outlets: Vec<Value> = config
        .outlets
        .iter()
        .map(|outlet| {
            json!({
                "id": outlet.name,
                "name": outlet.name,
                "broker": outlet.broker,
                "topic_prefix": outlet.topic_prefix,
                "qos": outlet.qos.to_string(),
                "tls": outlet.tls,
                "encoding": encoding_str(outlet.encoding),
                "username": outlet.username,
                "password": redact_secret(&outlet.password),
            })
        })
        .collect();
    // [mgmt_auth] 用户只回 name + role：password_hash 绝不进响应。
    let users: Vec<Value> = config
        .mgmt_auth
        .iter()
        .flat_map(|section| section.users.iter())
        .map(|user| json!({ "name": user.name, "role": user.role }))
        .collect();
    Json(json!({
        "basic": {
            "gateway_id": gateway.gateway_id,
            "data_dir": gateway.data_dir.display().to_string(),
        },
        "oem": {
            "managed_by": "vendor-license-backend",
            "note": "贴牌品牌信息由厂商管理后台随牌照统一下发，网关本地只读应用；本端点不提供 OEM 写入",
        },
        "network": { "outlets": outlets },
        "storage": {
            "sqlite_path": gateway.cache.sqlite_path,
            "max_size_mb": gateway.cache.max_size_mb.to_string(),
            "retention_days": gateway.cache.retention_days.to_string(),
        },
        "security": {
            "tls_cert_path": gateway.security.tls_cert_path,
            "tls_key_path": gateway.security.tls_key_path,
            "web_auth_enabled": gateway.security.web_auth_enabled,
            "activation_code_set": gateway.licensing.activation_code.is_some(),
            "mgmt_users": users,
        },
        "config_version": state.daemon().config_version().to_string(),
    }))
    .into_response()
}

/// 出口编码 → 字面量（与 /api/outlets 同口径）。
fn encoding_str(encoding: OutletEncoding) -> &'static str {
    match encoding {
        OutletEncoding::Protobuf => "protobuf",
        OutletEncoding::Json => "json",
    }
}

/// 敏感字段脱敏：已配置 → `"<redacted>"`，未配置 → `null`（绝不回明文）。
fn redact_secret(secret: &Option<String>) -> Value {
    match secret {
        Some(_) => Value::String("<redacted>".to_string()),
        None => Value::Null,
    }
}

// ---- GET /api/settings/backups ----

/// `GET /api/settings/backups` → 配置目录内 `config.toml.bak-*` 清单。
///
/// 每项：`{file, size_bytes, mtime_ms}`（计数 / 字节 / mtime 一律字符串——大数
/// 红线）；按文件名升序（名字含 unix 秒备份时刻，升序 = 时间序）。目录不可读 /
/// 未装配 config 路径时返回空列表（空列表合法，不报错）。
pub async fn list_backups(State(state): State<MgmtState>) -> Response {
    let rows = match state.config_path() {
        Some(path) => backup_rows(&path),
        None => Vec::new(),
    };
    Json(Value::Array(rows)).into_response()
}

/// 列出配置目录内 `{file_name}.bak-*` 备份（排除临时文件；读目录失败 = 空表）。
fn backup_rows(config_path: &Path) -> Vec<Value> {
    let dir = config_path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .map_or_else(|| PathBuf::from("."), Path::to_path_buf);
    let file_name = config_path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("config.toml");
    let prefix = format!("{file_name}.bak-");
    let mut rows: Vec<(String, u64, u64)> = match std::fs::read_dir(&dir) {
        Ok(entries) => entries
            .filter_map(|entry| entry.ok())
            .filter(|entry| entry.path().is_file())
            .filter_map(|entry| entry.file_name().to_str().map(ToString::to_string))
            .filter(|name| name.starts_with(&prefix) && !name.contains(".tmp"))
            .filter_map(|name| {
                let meta = std::fs::metadata(dir.join(&name)).ok()?;
                let size = meta.len();
                let mtime_ms = meta
                    .modified()
                    .ok()?
                    .duration_since(UNIX_EPOCH)
                    .map(|d| d.as_millis() as u64)
                    .unwrap_or(0);
                Some((name, size, mtime_ms))
            })
            .collect(),
        Err(_) => Vec::new(),
    };
    rows.sort();
    rows.into_iter()
        .map(|(name, size, mtime_ms)| {
            json!({
                "file": name,
                "size_bytes": size.to_string(),
                "mtime_ms": mtime_ms.to_string(),
            })
        })
        .collect()
}

/// 取最新备份文件名（`{file_name}.bak-*` 按名最大 = 时刻最新；无备份 → None）。
fn newest_backup_name(config_path: &Path) -> Option<String> {
    backup_rows(config_path)
        .pop()
        .and_then(|row| row["file"].as_str().map(ToString::to_string))
}

// ---- PUT /api/settings ----

/// `PUT /api/settings` → 设置持久化（白名单校验 + 写前备份 + 热重载生效）。
///
/// body = 设置组对象（如 `{"basic": {"gateway_id": "gw-2"}, "storage": {...}}`）；
/// 只更新出现的字段。任何未知字段（顶层或组内）→ 400；写失败 fail-closed。
pub async fn put_settings(
    State(state): State<MgmtState>,
    authed: AuthedRole,
    body: Bytes,
) -> Response {
    if let Err(rejection) = authed.ensure(Permission::DeviceWrite) {
        writeapi::audit(
            &state,
            &authed.claims.sub,
            OpsAction::SettingsWrite,
            false,
            OUTCOME_DENIED,
            &rejection.to_string(),
        );
        return rejection.into_response();
    }
    let actor = authed.claims.sub.clone();
    let req: Value = match serde_json::from_slice::<Value>(&body) {
        Ok(value) if value.is_object() => value,
        Ok(_) | Err(_) => {
            writeapi::audit(
                &state,
                &actor,
                OpsAction::SettingsWrite,
                true,
                OUTCOME_BAD_REQUEST,
                "malformed json body",
            );
            return writeapi::validation_error(
                "body",
                "body must be a JSON object of setting groups",
                "object {basic?, storage?, security?}",
            );
        }
    };
    // body 已在上面的解析分支验证为对象；此处不可能是 None（无 unwrap/expect 纪律）。
    let Some(groups) = req.as_object() else {
        return writeapi::internal("unreachable: body verified as JSON object");
    };
    for key in groups.keys() {
        if !SETTING_GROUPS.contains(&key.as_str()) {
            let reason = if key == "oem" {
                "oem branding is read-only (managed by the vendor license backend)".to_string()
            } else if key == "network" {
                "network outlets are managed via /api/outlets and dedicated forwarder endpoints"
                    .to_string()
            } else {
                format!("unknown settings group {key:?}")
            };
            writeapi::audit(
                &state,
                &actor,
                OpsAction::SettingsWrite,
                true,
                OUTCOME_BAD_REQUEST,
                &format!("rejected settings group {key:?}: {reason}"),
            );
            return writeapi::validation_error(
                &format!("body.{key}"),
                &reason,
                &format!("allowed groups: {}", SETTING_GROUPS.join(" | ")),
            );
        }
    }
    if groups.is_empty() {
        writeapi::audit(
            &state,
            &actor,
            OpsAction::SettingsWrite,
            true,
            OUTCOME_BAD_REQUEST,
            "no setting groups provided",
        );
        return writeapi::validation_error(
            "body",
            "no setting groups provided",
            &format!("at least one of: {}", SETTING_GROUPS.join(" | ")),
        );
    }

    let _guard = writeapi::write_guard();
    let mut config = (*state.config()).clone();
    if let Some(group) = groups.get("basic") {
        let Some(obj) = group.as_object() else {
            return bad_group(&actor, &state, "basic");
        };
        if let Err(resp) = reject_unknown_keys("basic", obj, BASIC_FIELDS) {
            return audit_rejected(&state, &actor, resp);
        }
        if let Some(raw) = obj.get("gateway_id") {
            config.gateway.gateway_id = match parse_string_field("basic", "gateway_id", raw) {
                Ok(value) if !value.is_empty() => value,
                Ok(_) => {
                    return audit_validation(
                        &state,
                        &actor,
                        writeapi::validation_error(
                            "basic.gateway_id",
                            "gateway_id must not be empty",
                            "non-empty gateway identifier",
                        ),
                    );
                }
                Err(resp) => return audit_validation(&state, &actor, resp),
            };
        }
    }
    if let Some(group) = groups.get("storage") {
        let Some(obj) = group.as_object() else {
            return bad_group(&actor, &state, "storage");
        };
        if let Err(resp) = reject_unknown_keys("storage", obj, STORAGE_FIELDS) {
            return audit_rejected(&state, &actor, resp);
        }
        if let Some(raw) = obj.get("sqlite_path") {
            match parse_string_field("storage", "sqlite_path", raw) {
                Ok(value) if !value.is_empty() => config.gateway.cache.sqlite_path = value,
                Ok(_) => {
                    return audit_validation(
                        &state,
                        &actor,
                        writeapi::validation_error(
                            "storage.sqlite_path",
                            "sqlite_path must not be empty",
                            "cache database file name (relative to data_dir)",
                        ),
                    );
                }
                Err(resp) => return audit_validation(&state, &actor, resp),
            }
        }
        if let Some(raw) = obj.get("max_size_mb") {
            match parse_u64_field("storage", "max_size_mb", raw) {
                Ok(value) => config.gateway.cache.max_size_mb = value,
                Err(resp) => return audit_validation(&state, &actor, resp),
            }
        }
        if let Some(raw) = obj.get("retention_days") {
            match parse_u64_field("storage", "retention_days", raw) {
                Ok(value) if value <= u32::MAX as u64 => {
                    config.gateway.cache.retention_days = value as u32;
                }
                Ok(_) | Err(_) => {
                    return audit_validation(
                        &state,
                        &actor,
                        writeapi::validation_error(
                            "storage.retention_days",
                            "not a valid u32",
                            "integer days (u32)",
                        ),
                    );
                }
            }
        }
    }
    if let Some(group) = groups.get("security") {
        let Some(obj) = group.as_object() else {
            return bad_group(&actor, &state, "security");
        };
        if let Err(resp) = reject_unknown_keys("security", obj, SECURITY_FIELDS) {
            return audit_rejected(&state, &actor, resp);
        }
        if let Some(raw) = obj.get("web_auth_enabled") {
            match raw.as_bool() {
                Some(value) => config.gateway.security.web_auth_enabled = value,
                None => {
                    return audit_validation(
                        &state,
                        &actor,
                        writeapi::validation_error(
                            "security.web_auth_enabled",
                            "must be a boolean",
                            "true | false",
                        ),
                    );
                }
            }
        }
        for field in ["tls_cert_path", "tls_key_path"] {
            if let Some(raw) = obj.get(field) {
                match parse_opt_path_field("security", field, raw) {
                    Ok(value) => {
                        if field == "tls_cert_path" {
                            config.gateway.security.tls_cert_path = value;
                        } else {
                            config.gateway.security.tls_key_path = value;
                        }
                    }
                    Err(resp) => return audit_validation(&state, &actor, resp),
                }
            }
        }
    }

    let detail = "persist settings via PUT /api/settings".to_string();
    match pages::persist_config(&state, config, &actor, OpsAction::SettingsWrite, &detail) {
        Ok(version) => {
            // 本次写前备份文件名（写锁持有期内取目录最新备份；无备份 → null）。
            let backup = state
                .config_path()
                .and_then(|path| newest_backup_name(&path));
            state.publish(MgmtEvent::ConfigReloaded { version });
            Json(json!({
                "accepted": true,
                "config_version": version.to_string(),
                "backup": backup,
                "note": "写前已自动备份；热重载即时生效",
            }))
            .into_response()
        }
        Err(resp) => resp,
    }
}

/// 400：组值不是对象（fail-closed，不入写路径）。
fn bad_group(actor: &str, state: &MgmtState, group: &str) -> Response {
    audit_validation(
        state,
        actor,
        writeapi::validation_error(
            &format!("body.{group}"),
            &format!("settings group {group:?} must be an object"),
            &format!("object of: {}", SETTING_GROUPS.join(" | ")),
        ),
    )
}

/// 校验失败统一入审计（bad_request）后返回 400 响应。
fn audit_validation(state: &MgmtState, actor: &str, resp: Response) -> Response {
    writeapi::audit(
        state,
        actor,
        OpsAction::SettingsWrite,
        true,
        OUTCOME_BAD_REQUEST,
        "settings body validation failed",
    );
    resp
}

/// 未知字段拒绝同样入审计。
fn audit_rejected(state: &MgmtState, actor: &str, resp: Response) -> Response {
    writeapi::audit(
        state,
        actor,
        OpsAction::SettingsWrite,
        true,
        OUTCOME_BAD_REQUEST,
        "settings body carries unknown fields",
    );
    resp
}

/// 白名单第二层：组内键校验（未知键 → 400，字段带组前缀）。
#[allow(clippy::result_large_err)]
fn reject_unknown_keys(
    group: &str,
    obj: &Map<String, Value>,
    allowed: &[&str],
) -> Result<(), Response> {
    for key in obj.keys() {
        if !allowed.contains(&key.as_str()) {
            return Err(writeapi::validation_error(
                &format!("{group}.{key}"),
                &format!("unknown settings field {group:?}.{key:?}"),
                &format!("allowed fields in {group:?}: {}", allowed.join(" | ")),
            ));
        }
    }
    Ok(())
}

/// 非空字符串字段解析（trim）。
#[allow(clippy::result_large_err)]
fn parse_string_field(group: &str, field: &str, raw: &Value) -> Result<String, Response> {
    match raw.as_str() {
        Some(value) => Ok(value.trim().to_string()),
        None => Err(writeapi::validation_error(
            &format!("{group}.{field}"),
            &format!("must be a string, got {raw}"),
            "string",
        )),
    }
}

/// u64 字段解析（接受 number 或 string——大数红线，string 为推荐形态）。
#[allow(clippy::result_large_err)]
fn parse_u64_field(group: &str, field: &str, raw: &Value) -> Result<u64, Response> {
    let parsed = match raw {
        Value::Number(number) => number.as_u64(),
        Value::String(value) => value.trim().parse::<u64>().ok(),
        _ => None,
    };
    parsed.ok_or_else(|| {
        writeapi::validation_error(
            &format!("{group}.{field}"),
            &format!("not a valid u64: {raw}"),
            "integer >= 0; number or string",
        )
    })
}

/// 可选路径字段解析（`null` = 清空，string = 设置）。
#[allow(clippy::result_large_err)]
fn parse_opt_path_field(group: &str, field: &str, raw: &Value) -> Result<Option<String>, Response> {
    match raw {
        Value::Null => Ok(None),
        Value::String(value) => {
            let trimmed = value.trim();
            if trimmed.is_empty() {
                Ok(None)
            } else {
                Ok(Some(trimmed.to_string()))
            }
        }
        _ => Err(writeapi::validation_error(
            &format!("{group}.{field}"),
            &format!("must be a string or null, got {raw}"),
            "string (PEM file path) | null (clear)",
        )),
    }
}

// ---- 测试 ----

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bootstrap::DaemonShared;
    use crate::config::{ConfigShared, GatewayConfig};
    use crate::mgmt::auth_jwt::{now_unix_secs, sign, Claims};
    use crate::mgmt::rbac::Role;
    use crate::mgmt::remote_ops::{self, runtime_for};
    use std::sync::Arc;
    use std::time::Duration;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::{TcpListener, TcpStream};

    /// 测试种子配置：敏感字段齐备（激活码 / 出口口令 / password_hash），
    /// 用于验证 GET 脱敏 + PUT 部分更新不破坏其它组。
    const SEED_TOML: &str = r#"
[gateway]
gateway_id = "gw-test"
data_dir = "./data"

[gateway.licensing]
cloud_url = "https://lic.example.com"
activation_code = "SECRET-ACTIVATION-XYZ"

[gateway.cache]
sqlite_path = "cache.db"
max_size_mb = 512
retention_days = 7

[gateway.security]
web_auth_enabled = true

[mgmt_auth]

[[mgmt_auth.users]]
name = "admin"
role = "system"
password_hash = "deadbeefcafe"

[[outlets]]
name = "north-1"
broker = "mqtts://broker.local:8883"
topic_prefix = "telemetry"
qos = 1
tls = true
encoding = "protobuf"
username = "dev-user"
password = "PLAINTEXT-PASS"
"#;

    /// 构造绑定临时配置文件的 MgmtState（`bind_path=false` 时故意不绑定
    /// config 路径，验证写端点 fail-closed）。
    fn make_state(dir: &tempfile::TempDir, bind_path: bool) -> (MgmtState, PathBuf) {
        let path = dir.path().join("config.toml");
        std::fs::write(&path, SEED_TOML).expect("seed config");
        let config = Arc::new(GatewayConfig::load(&path).expect("load"));
        let daemon = DaemonShared::new();
        daemon.set_config(Arc::new(ConfigShared::new((*config).clone())));
        let mut state = MgmtState::new(daemon, config);
        if bind_path {
            state = state.with_config_path(&path);
        }
        let state = state;
        remote_ops::install(&state, Arc::new(remote_ops::DenyAllOpsAuthorizer));
        (state, path)
    }

    /// 以 state 的实际签名密钥签发测试 token（对齐 writeapi 测试装配口径）。
    fn token_for(state: &MgmtState, role: Role) -> String {
        let now = now_unix_secs();
        let claims = Claims {
            sub: "ops-admin".to_string(),
            role,
            exp: now + 600,
            iat: now,
            nbf: None,
            jti: "test-jti-settings".to_string(),
        };
        sign(&claims, state.auth().key()).expect("sign test token")
    }

    /// 在 127.0.0.1 随机端口启动 axum 服务（本机回环）。
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

    /// 手写 HTTP 请求（method/path/body/token），3s 超时防挂死。
    async fn http_request(
        port: u16,
        method: &str,
        path: &str,
        body: Option<&str>,
        token: Option<&str>,
    ) -> (u16, String, String) {
        tokio::time::timeout(Duration::from_secs(3), async move {
            let mut stream =
                TcpStream::connect(("127.0.0.1", port)).await.expect("connect");
            let body = body.unwrap_or("");
            let auth = token
                .map(|t| format!("Authorization: Bearer {t}\r\n"))
                .unwrap_or_default();
            let request = if method == "GET" {
                format!("GET {path} HTTP/1.1\r\nHost: 127.0.0.1\r\n{auth}Connection: close\r\n\r\n")
            } else {
                format!(
                    "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1\r\n{auth}Content-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                )
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

    async fn http_put_bearer(
        port: u16,
        path: &str,
        body: &str,
        token: &str,
    ) -> (u16, String, String) {
        http_request(port, "PUT", path, Some(body), Some(token)).await
    }

    /// 从落盘文件重读配置（往返断言用）。
    fn load_config(path: &std::path::Path) -> GatewayConfig {
        GatewayConfig::load(path).expect("reload saved config")
    }

    /// 列出目录内备份文件名（排除临时文件）。
    fn backup_names(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(dir)
            .expect("read_dir")
            .filter_map(|e| e.ok())
            .filter(|e| e.path().is_file())
            .filter_map(|e| e.file_name().to_str().map(ToString::to_string))
            .filter(|n| n.starts_with("config.toml.bak-") && !n.contains(".tmp"))
            .collect();
        names.sort();
        names
    }

    // ---- GET /api/settings ----

    /// QA Happy: 分组视图齐备（五组页签对应字段）+ 脱敏红线（口令/激活码/
    /// password_hash 绝不明文）+ 大数一律字符串（config_version/qos/max_size_mb）。
    #[tokio::test]
    async fn get_settings_grouped_view_redacted_and_string_numbers() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (state, _path) = make_state(&dir, true);
        let port = spawn_server(state.clone()).await;

        let (status, _, body) = http_get(port, "/api/settings").await;
        assert_eq!(status, 200, "{body}");
        let value: Value = serde_json::from_str(&body).expect("json");

        // 基础 / 存储 / 安全分组。
        assert_eq!(value["basic"]["gateway_id"], "gw-test");
        assert_eq!(value["basic"]["data_dir"], "./data");
        assert_eq!(value["storage"]["sqlite_path"], "cache.db");
        assert_eq!(value["storage"]["max_size_mb"], "512");
        assert_eq!(value["storage"]["retention_days"], "7");
        assert_eq!(value["security"]["web_auth_enabled"], true);
        assert_eq!(value["security"]["activation_code_set"], true);

        // 网络（出口）：qos 字符串 + 口令脱敏。
        let outlets = value["network"]["outlets"].as_array().expect("outlets");
        assert_eq!(outlets.len(), 1);
        assert_eq!(outlets[0]["qos"], "1");
        assert_eq!(outlets[0]["password"], "<redacted>");
        assert_eq!(outlets[0]["username"], "dev-user");
        assert_eq!(outlets[0]["encoding"], "protobuf");

        // 安全：mgmt 用户只回 name/role。
        let users = value["security"]["mgmt_users"].as_array().expect("users");
        assert_eq!(users.len(), 1);
        assert_eq!(users[0]["name"], "admin");
        assert_eq!(users[0]["role"], "system");

        // 脱敏红线：敏感原文绝不出现。
        assert!(!body.contains("PLAINTEXT-PASS"), "password leaked: {body}");
        assert!(
            !body.contains("SECRET-ACTIVATION-XYZ"),
            "activation leaked: {body}"
        );
        assert!(
            !body.contains("deadbeefcafe"),
            "password hash leaked: {body}"
        );
        assert!(
            value.get("activation_code").is_none(),
            "activation_code must not appear"
        );
        assert!(
            value["config_version"].is_string(),
            "config_version 大数红线"
        );
    }

    /// QA: GET 读端点保持开放（无 token → 200，与 /api/status 同口径）。
    #[tokio::test]
    async fn get_settings_is_open_read() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (state, _path) = make_state(&dir, true);
        let port = spawn_server(state).await;
        let (status, _, _) = http_get(port, "/api/settings").await;
        assert_eq!(status, 200, "read endpoints stay open (no token)");
    }

    // ---- PUT /api/settings ----

    /// QA Happy: PUT 落盘往返——响应含 config_version/backup；文件重读字段
    /// 齐变；GET 立即反映；写前备份恰生成一份且内容为写前快照。
    #[tokio::test]
    async fn put_settings_persists_versions_and_backs_up() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (state, path) = make_state(&dir, true);
        let before_raw = std::fs::read_to_string(&path).expect("read pre-write");
        let token = token_for(&state, Role::System);
        let port = spawn_server(state.clone()).await;

        let (status, _, body) = http_put_bearer(
            port,
            "/api/settings",
            r#"{
                "basic": {"gateway_id": "gw-renamed"},
                "storage": {"max_size_mb": "1024", "retention_days": 14, "sqlite_path": "vault.db"},
                "security": {"web_auth_enabled": false, "tls_cert_path": "/pem/cert.pem", "tls_key_path": null}
            }"#,
            &token,
        )
        .await;
        assert_eq!(status, 200, "{body}");
        let value: Value = serde_json::from_str(&body).expect("json");
        assert_eq!(value["accepted"], true);
        assert!(
            value["config_version"].is_string(),
            "config_version 大数红线: {value}"
        );
        let backup = value["backup"].as_str().expect("backup name");
        assert!(
            backup.starts_with("config.toml.bak-"),
            "backup name: {backup}"
        );

        // 落盘往返 + 未提及字段保持原值（outlets / mgmt_auth / 激活码不动）。
        let config = load_config(&path);
        assert_eq!(config.gateway.gateway_id, "gw-renamed");
        assert_eq!(config.gateway.cache.max_size_mb, 1024);
        assert_eq!(config.gateway.cache.retention_days, 14);
        assert_eq!(config.gateway.cache.sqlite_path, "vault.db");
        assert!(!config.gateway.security.web_auth_enabled);
        assert_eq!(
            config.gateway.security.tls_cert_path.as_deref(),
            Some("/pem/cert.pem")
        );
        assert_eq!(
            config.gateway.security.tls_key_path, None,
            "null clears the path"
        );
        assert_eq!(config.outlets.len(), 1, "outlets untouched");
        assert_eq!(
            config.outlets[0].password.as_deref(),
            Some("PLAINTEXT-PASS"),
            "redaction is response-only; config keeps the real secret"
        );

        // 备份内容 = 写前快照。
        assert_eq!(backup_names(dir.path()).len(), 1, "exactly one backup");
        let backup_raw = std::fs::read_to_string(dir.path().join(backup)).expect("read backup");
        assert_eq!(
            backup_raw, before_raw,
            "backup must be the pre-write snapshot"
        );

        // GET 立即反映（热快照即时替换）。
        let (status, _, body) = http_get(port, "/api/settings").await;
        assert_eq!(status, 200);
        let value: Value = serde_json::from_str(&body).expect("json");
        assert_eq!(value["basic"]["gateway_id"], "gw-renamed");
        assert_eq!(value["security"]["web_auth_enabled"], false);
    }

    /// QA Error: 白名单——未知顶层组 / 只读组（oem、network）/ 组内未知字段
    /// 一律 400，且**零落盘**（fail-closed）。
    #[tokio::test]
    async fn put_settings_rejects_unknown_and_readonly_fields_without_disk_change() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (state, path) = make_state(&dir, true);
        let before_raw = std::fs::read_to_string(&path).expect("read pre-write");
        let token = token_for(&state, Role::System);
        let port = spawn_server(state.clone()).await;

        // 未知顶层组。
        let (status, _, body) =
            http_put_bearer(port, "/api/settings", r#"{"tz":"UTC+8"}"#, &token).await;
        assert_eq!(status, 400, "{body}");
        let value: Value = serde_json::from_str(&body).expect("json");
        assert_eq!(value["error"], "validation_failed");
        assert_eq!(value["field"], "body.tz");

        // 只读组（oem / network）→ 400 且 reason 指明只读归属。
        for (group, marker) in [("oem", "read-only"), ("network", "outlets")] {
            let body_json = format!(r#"{{"{group}": {{}}}}"#);
            let (status, _, body) =
                http_put_bearer(port, "/api/settings", &body_json, &token).await;
            assert_eq!(status, 400, "{group}: {body}");
            let value: Value = serde_json::from_str(&body).expect("json");
            assert!(
                value["reason"].as_str().expect("reason").contains(marker),
                "{group} reason must explain read-only: {value}"
            );
        }

        // 组内未知字段。
        let (status, _, body) = http_put_bearer(
            port,
            "/api/settings",
            r#"{"basic": {"gateway_id": "x", "brand": "acme"}}"#,
            &token,
        )
        .await;
        assert_eq!(status, 400, "{body}");
        let value: Value = serde_json::from_str(&body).expect("json");
        assert_eq!(value["field"], "basic.brand");

        // 空 body（无任何组）→ 400；类型错误 → 400。
        let (status, _, _) = http_put_bearer(port, "/api/settings", "{}", &token).await;
        assert_eq!(status, 400);
        let (status, _, _) = http_put_bearer(
            port,
            "/api/settings",
            r#"{"basic": {"gateway_id": 42}}"#,
            &token,
        )
        .await;
        assert_eq!(status, 400);
        let (status, _, _) = http_put_bearer(port, "/api/settings", "not-json{{{", &token).await;
        assert_eq!(status, 400);

        // 全部失败路径零落盘（连备份都不该出现）。
        assert_eq!(
            std::fs::read_to_string(&path).expect("reread"),
            before_raw,
            "config must stay untouched on every failure path"
        );
        assert!(backup_names(dir.path()).is_empty(), "no backup on failure");
    }

    /// QA RBAC: ops 角色（不持 device.write）→ 403 + 审计（被拒入环），
    /// 且零落盘。
    #[tokio::test]
    async fn put_settings_denied_for_ops_role_is_403_and_audited() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (state, path) = make_state(&dir, true);
        let ops = token_for(&state, Role::Ops);
        let port = spawn_server(state.clone()).await;

        let (status, _, body) = http_put_bearer(
            port,
            "/api/settings",
            r#"{"basic": {"gateway_id": "hacked"}}"#,
            &ops,
        )
        .await;
        assert_eq!(status, 403, "{body}");
        let value: Value = serde_json::from_str(&body).expect("json");
        assert_eq!(value["error"], "forbidden");

        let denied: Vec<_> = runtime_for(&state)
            .audit_snapshot()
            .into_iter()
            .filter(|e| !e.allowed && e.outcome == OUTCOME_DENIED)
            .collect();
        assert_eq!(denied.len(), 1);
        assert_eq!(denied[0].action, "settings_write");
        assert_eq!(denied[0].actor, "ops-admin");

        let config = load_config(&path);
        assert_eq!(config.gateway.gateway_id, "gw-test", "no disk change");
    }

    /// QA: 未携带 token → 401（身份未建立；不留审计痕）。
    #[tokio::test]
    async fn put_settings_unauthenticated_is_401() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (state, path) = make_state(&dir, true);
        let port = spawn_server(state.clone()).await;

        let (status, _, _) = http_request(
            port,
            "PUT",
            "/api/settings",
            Some(r#"{"basic": {"gateway_id": "x"}}"#),
            None,
        )
        .await;
        assert_eq!(status, 401);
        let config = load_config(&path);
        assert_eq!(config.gateway.gateway_id, "gw-test");
        assert!(runtime_for(&state).audit_snapshot().is_empty());
    }

    /// QA fail-closed: config 路径未装配 → 500，配置快照不变（绝不写未知文件）。
    #[tokio::test]
    async fn put_settings_without_config_path_is_fail_closed_500() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (state, _path) = make_state(&dir, false);
        let token = token_for(&state, Role::System);
        let port = spawn_server(state.clone()).await;

        let (status, _, body) = http_put_bearer(
            port,
            "/api/settings",
            r#"{"basic": {"gateway_id": "gw-2"}}"#,
            &token,
        )
        .await;
        assert_eq!(status, 500, "{body}");
        let value: Value = serde_json::from_str(&body).expect("json");
        assert_eq!(value["error"], "internal");
        assert_eq!(
            state.config().gateway.gateway_id,
            "gw-test",
            "snapshot must stay unchanged"
        );
    }

    // ---- GET /api/settings/backups ----

    /// QA: 备份清单——初始空列表合法；PUT 落盘后清单出现该备份，字段
    /// （file/size_bytes/mtime_ms）齐备且计数一律字符串（大数红线）。
    #[tokio::test]
    async fn backups_list_empty_then_reflects_write() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (state, _path) = make_state(&dir, true);
        let token = token_for(&state, Role::System);
        let port = spawn_server(state.clone()).await;

        // 初始：无备份 → 空列表（合法）。
        let (status, _, body) = http_get(port, "/api/settings/backups").await;
        assert_eq!(status, 200, "{body}");
        let rows: Vec<Value> = serde_json::from_str(&body).expect("array");
        assert!(rows.is_empty(), "no backup yet: {rows:?}");

        // PUT 一次 → 清单出现恰好一条。
        let (status, _, _) = http_put_bearer(
            port,
            "/api/settings",
            r#"{"storage": {"max_size_mb": 256}}"#,
            &token,
        )
        .await;
        assert_eq!(status, 200);

        let (status, _, body) = http_get(port, "/api/settings/backups").await;
        assert_eq!(status, 200);
        let rows: Vec<Value> = serde_json::from_str(&body).expect("array");
        assert_eq!(rows.len(), 1, "one backup after one write: {body}");
        assert!(rows[0]["file"]
            .as_str()
            .expect("file")
            .starts_with("config.toml.bak-"));
        assert!(
            rows[0]["size_bytes"].is_string() && rows[0]["mtime_ms"].is_string(),
            "size/mtime must be strings (大数红线): {rows:?}"
        );
        assert!(
            rows[0]["size_bytes"]
                .as_str()
                .expect("size")
                .parse::<u64>()
                .is_ok(),
            "size parses as u64: {rows:?}"
        );
        assert!(
            rows[0]["mtime_ms"]
                .as_str()
                .expect("mtime")
                .parse::<u64>()
                .is_ok(),
            "mtime parses as u64: {rows:?}"
        );
    }
}

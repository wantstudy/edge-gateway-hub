//! 管理面测试的 HTTP 脚手架（`#[cfg(test)]`，生产构建整体剔除）。
//!
//! 原先整包放在 `mgmt/pages::tests` 内部；现在被 `pages` / `alerts_api` 的
//! 端到端测试共用，提出来是为了**不把同一份 130 行 harness 复制两份**——测试
//! 里出现两份「意思相同但各写一遍」的HTTP 助手，正是我们一路在治理的「跨模块
//! 语义漂移」的小 replica。
//!
//! 所有请求都走真实回环 socket（不 mock axum handler 直调），这样断言的是
//! 「路由 + 鉴权 + handler + 状态」整条链，而不是某一段函数。

use crate::bootstrap::DaemonShared;
use crate::config::{ConfigShared, GatewayConfig};
use crate::mgmt::auth_jwt::{now_unix_secs, sign, Claims};
use crate::mgmt::rbac::Role;
use crate::mgmt::remote_ops;
use crate::mgmt::MgmtState;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

/// 测试种子配置：登记空设备 + 点位设备（端点式地址，southbound 同语义）+ 1 出口。
pub fn seed_toml(broker_port: u16) -> String {
    format!(
        r#"
[gateway]
gateway_id = "gw-test"

[[outlets]]
name = "north-1"
broker = "mqtt://127.0.0.1:{broker_port}"
topic_prefix = "telemetry"
qos = 1
encoding = "json"

[[devices]]
device_id = "dev-empty"
name = "空设备"
protocol = "opcua"

[[points]]
device_id = "dev-01"
point_id = "40001"
protocol = "modbus-tcp"
address = "192.168.1.10:502"
frequency_ms = 100

[[points]]
device_id = "dev-01"
point_id = "40003"
protocol = "modbus-tcp"
address = "192.168.1.10:502"
frequency_ms = 500
"#
    )
}

/// 构造绑定临时配置文件 + 全新 ops runtime 的 MgmtState（独立实例，防并行污染）。
pub fn make_state(dir: &tempfile::TempDir, broker_port: u16) -> (MgmtState, PathBuf) {
    let path = dir.path().join("config.toml");
    std::fs::write(&path, seed_toml(broker_port)).expect("seed config");
    let config = Arc::new(GatewayConfig::load(&path).expect("load"));
    let daemon = DaemonShared::new();
    daemon.set_config(Arc::new(ConfigShared::new((*config).clone())));
    let state = MgmtState::new(daemon, config).with_config_path(&path);
    remote_ops::install(&state, Arc::new(remote_ops::DenyAllOpsAuthorizer));
    (state, path)
}

/// 以 state 的实际签名密钥签发测试 token（对齐 writeapi 测试装配口径）。
pub fn token_for(state: &MgmtState, role: Role) -> String {
    let now = now_unix_secs();
    let claims = Claims {
        sub: "ops-admin".to_string(),
        role: role.as_str().to_string(),
        perms: None,
        exp: now + 600,
        iat: now,
        nbf: None,
        jti: "test-jti-pages".to_string(),
    };
    sign(&claims, state.auth().key()).expect("sign test token")
}

/// 在 127.0.0.1 随机端口启动 axum 服务（本机回环），返回端口。
pub async fn spawn_server(state: MgmtState) -> u16 {
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
pub fn parse_response(raw: &str) -> (u16, String, String) {
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

/// 手写 HTTP 请求（10s 超时——探测类端点含真实网络 IO，预算放宽）。
pub async fn http_request(
    port: u16,
    method: &str,
    path: &str,
    body: Option<&str>,
    token: Option<&str>,
    content_type: &str,
) -> (u16, String, String) {
    tokio::time::timeout(Duration::from_secs(10), async move {
        let mut stream = TcpStream::connect(("127.0.0.1", port))
            .await
            .expect("connect");
        let body = body.unwrap_or("");
        let auth = token
            .map(|t| format!("Authorization: Bearer {t}\r\n"))
            .unwrap_or_default();
        let request = if method == "GET" {
            format!("GET {path} HTTP/1.1\r\nHost: 127.0.0.1\r\n{auth}Connection: close\r\n\r\n")
        } else {
            format!(
                "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1\r\n{auth}Content-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
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

/// 无 token 的裸 GET（用于鉴权断言）。
pub async fn http_get(port: u16, path: &str) -> (u16, String, String) {
    http_request(port, "GET", path, None, None, "text/plain").await
}

/// `POST` + JSON。
pub async fn post_json(port: u16, path: &str, body: &str, token: &str) -> (u16, String, String) {
    http_request(
        port,
        "POST",
        path,
        Some(body),
        Some(token),
        "application/json",
    )
    .await
}

/// `POST` + CSV。
pub async fn post_csv(port: u16, path: &str, body: &str, token: &str) -> (u16, String, String) {
    http_request(port, "POST", path, Some(body), Some(token), "text/csv").await
}

/// `PUT` + JSON。
pub async fn put_json(port: u16, path: &str, body: &str, token: &str) -> (u16, String, String) {
    http_request(
        port,
        "PUT",
        path,
        Some(body),
        Some(token),
        "application/json",
    )
    .await
}

/// `DELETE` + JSON（四要素删除口径）。
pub async fn delete_json(port: u16, path: &str, body: &str, token: &str) -> (u16, String, String) {
    http_request(
        port,
        "DELETE",
        path,
        Some(body),
        Some(token),
        "application/json",
    )
    .await
}

/// 从落盘文件重读配置（往返断言用）。
pub fn load_config(path: &Path) -> GatewayConfig {
    GatewayConfig::load(path).expect("reload saved config")
}

//! task 13 — HTTP 采集驱动（轮询 JSON 接口，hand-rolled HTTP/1.1 客户端）。
//!
//! ## 依赖红线
//! **禁止引入新依赖（不用 reqwest）**：基于 tokio TcpStream 手写 HTTP/1.1 GET
//! （Host/Accept/Connection 头、`\r\n\r\n` 结尾）；响应解析支持
//! **Content-Length** 与 **chunked**（最小实现 [`decode_chunked`]）。
//!
//! ## V1 仅支持 http://，不支持 TLS
//! 红线要求零新增依赖，而 TLS 需引入 rustls 客户端栈（daemon 已有 rustls 依赖，
//! 但为其包一层 HTTP 客户端 TLS 属于 task 13 范围外的握手/证书校验工程量）。
//! V1 明确只接受 `http://` URL，`https://` 一律报错；后续计划：接入
//! tokio-rustls 时增加 `HttpDriverConfig::tls: Option<TlsConfig>` 分支，
//! URL 解析在 [`parse_http_url`] 单点扩展 `https://` 前缀即可。
//!
//! ## 语义
//! - `connect` = 预检：对目标 TCP 连一次即断（不做 HTTP 层校验）；
//! - `read` = 轮询一次（单次 GET）并按各点位 JSON 路径提取多点；
//!   `write` 不支持（采集只读），显式返回 [`DaemonError::ProtocolError`]；
//! - 每次轮询为独立短连接请求（`Connection: close`），无跨请求状态，
//!   故无重连恢复流程（断线即本次轮询失败，下轮重试）。
//!
//! ## Driver trait 的地址约定
//! [`crate::driver::PointAddress`] 是扁平 u32 结构，无法承载任意 JSON 路径字符串，
//! 故约定：`ReadPoint.address` 满足 `area == Some('H')` 且 `start` 为
//! [`HttpDriverConfig::points`] 配置表的下标（用 [`HttpDriver::point_address`]
//! 生成），`count` 固定为 1（JSON 提取无连续长度语义）。
//! [`Driver::read`] 返回的 [`PointSample::value`] 为 f64 的 8 字节小端编码；
//! 提取失败（路径缺失 / 类型错误）按 trait 契约整体返回 Err——
//! 含质量码的完整结果请用 [`HttpDriver::poll`]（返回 [`JsonPointSample`]，
//! 质量码语义复用 mqtt_in.rs 的共享实现）。
//!
//! ## JSON 提取
//! 点分路径提取 / 数值转换 / 质量码逻辑**定义在 mqtt_in.rs（task 14 约定）并 pub**，
//! 本模块直接复用（`crate::driver::mqtt_in::{json_point_sample, JsonPointSample}`），
//! 语义详见该模块文档（路径缺失 / 类型错误 → `Quality::Bad`）。

use std::time::Duration;

use async_trait::async_trait;
use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::time::timeout;

use crate::driver::mqtt_in::{json_point_sample, JsonPointSample};
use crate::driver::{Driver, PointAddress, PointSample, ReadPoint, WritePoint};
use crate::error::{DaemonError, DaemonResult};

// ---- 本模块局部错误 ----

/// HTTP 驱动局部错误（`map_err`/`From` 转 [`DaemonError`]）。
#[derive(Debug, Clone, PartialEq, Eq)]
enum HttpError {
    /// 传输层问题（连接 / 收发 / 超时）→ [`DaemonError::NetworkError`]。
    Network(String),
    /// 协议层问题（URL / 状态码 / 帧 / JSON）→ [`DaemonError::ProtocolError`]。
    Protocol(String),
}

impl From<HttpError> for DaemonError {
    fn from(err: HttpError) -> Self {
        match err {
            HttpError::Network(msg) => DaemonError::NetworkError(msg),
            HttpError::Protocol(msg) => DaemonError::ProtocolError(msg),
        }
    }
}

// ---- URL / 请求报文（纯函数） ----

/// 解析 `http://host[:port]/path?query` URL（V1 仅 http，原因见模块文档）。
///
/// 返回（主机、端口、路径）；省略端口默认 80，省略路径默认 `/`。
///
/// # Errors
/// 非 `http://` 前缀 / 缺主机 / 端口非法 → [`HttpError::Protocol`]。
fn parse_http_url(url: &str) -> Result<(String, u16, String), HttpError> {
    let wrap = |detail: String| HttpError::Protocol(format!("invalid http url {url:?}: {detail}"));
    let rest = url
        .strip_prefix("http://")
        .ok_or_else(|| wrap("only http:// scheme supported in V1 (no TLS)".to_string()))?;
    let (authority, path) = match rest.find('/') {
        Some(i) => (&rest[..i], rest[i..].to_string()),
        None => (rest, "/".to_string()),
    };
    if authority.is_empty() {
        return Err(wrap("missing host".to_string()));
    }
    let (host, port) = match authority.rsplit_once(':') {
        Some((h, p)) => {
            if h.is_empty() {
                return Err(wrap("missing host".to_string()));
            }
            let port: u16 = p.parse().map_err(|_| wrap(format!("invalid port {p:?}")))?;
            (h.to_string(), port)
        }
        None => (authority.to_string(), 80),
    };
    Ok((host, port, path))
}

/// 构建最小 HTTP/1.1 GET 请求报文（`\r\n\r\n` 结尾）。
fn build_get_request(host: &str, port: u16, path: &str, headers: &[(String, String)]) -> String {
    let host_header = if port == 80 {
        host.to_string()
    } else {
        format!("{host}:{port}")
    };
    let mut request = format!(
        "GET {path} HTTP/1.1\r\nHost: {host_header}\r\nAccept: application/json\r\nConnection: close\r\n"
    );
    for (name, value) in headers {
        request.push_str(&format!("{name}: {value}\r\n"));
    }
    request.push_str("\r\n");
    request
}

// ---- 响应解析（纯函数） ----

/// 在字节流中定位 `\r\n` 行尾。
fn find_crlf(data: &[u8]) -> Result<usize, HttpError> {
    data.windows(2)
        .position(|w| w == b"\r\n")
        .ok_or_else(|| HttpError::Protocol("unterminated line (missing CRLF)".to_string()))
}

/// 解码 chunked 传输编码体（最小实现：十六进制块长 + 块数据 + CRLF，
/// 支持 `;chunk-extension` 忽略；终止于 0 长块）。
fn decode_chunked(mut data: &[u8]) -> Result<Vec<u8>, HttpError> {
    let mut out = Vec::new();
    loop {
        let line_end = find_crlf(data)?;
        let line = std::str::from_utf8(&data[..line_end])
            .map_err(|_| HttpError::Protocol("chunk size line is not utf-8".to_string()))?;
        let size_str = line.split(';').next().unwrap_or("").trim();
        let size = usize::from_str_radix(size_str, 16)
            .map_err(|_| HttpError::Protocol(format!("invalid chunk size {size_str:?}")))?;
        data = &data[line_end + 2..];
        if size == 0 {
            // 终止块后的 trailer 不做解析（最小实现），直接结束。
            return Ok(out);
        }
        if data.len() < size + 2 {
            return Err(HttpError::Protocol(format!(
                "truncated chunk: need {size} bytes, got {}",
                data.len()
            )));
        }
        out.extend_from_slice(&data[..size]);
        if &data[size..size + 2] != b"\r\n" {
            return Err(HttpError::Protocol(
                "missing CRLF after chunk data".to_string(),
            ));
        }
        data = &data[size + 2..];
    }
}

/// 解析 HTTP 响应：状态行 + 头 + 体（Content-Length / chunked / EOF 定界）。
///
/// 返回体字节；非 2xx 状态码视为协议错误。
fn parse_http_response(raw: &[u8]) -> Result<Vec<u8>, HttpError> {
    let header_end = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .ok_or_else(|| HttpError::Protocol("missing header terminator \\r\\n\\r\\n".to_string()))?;
    let head = std::str::from_utf8(&raw[..header_end])
        .map_err(|_| HttpError::Protocol("response head is not utf-8".to_string()))?;
    let body = &raw[header_end + 4..];

    // 状态行：HTTP/1.1 200 OK
    let mut lines = head.split("\r\n");
    let status_line = lines
        .next()
        .ok_or_else(|| HttpError::Protocol("empty response head".to_string()))?;
    let status: u16 = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| HttpError::Protocol(format!("malformed status line {status_line:?}")))?;
    if !(200..300).contains(&status) {
        return Err(HttpError::Protocol(format!("http status {status}")));
    }

    // 头字段：大小写不敏感地找 Content-Length / Transfer-Encoding。
    let mut content_length: Option<usize> = None;
    let mut chunked = false;
    for line in lines {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        let name = name.trim().to_ascii_lowercase();
        let value = value.trim();
        if name == "content-length" {
            content_length = value.parse().ok();
        } else if name == "transfer-encoding" && value.to_ascii_lowercase().contains("chunked") {
            chunked = true;
        }
    }

    if chunked {
        decode_chunked(body)
    } else if let Some(len) = content_length {
        if body.len() < len {
            return Err(HttpError::Protocol(format!(
                "truncated body: header declares {len} bytes, got {}",
                body.len()
            )));
        }
        Ok(body[..len].to_vec())
    } else {
        // 无 Content-Length 也非 chunked：Connection: close 下以 EOF 定界。
        Ok(body.to_vec())
    }
}

// ---- 配置 ----

/// 单个 HTTP 点位配置：点位标识 + payload 内 JSON 路径。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpPointConfig {
    /// 点位标识（采样结果回带）。
    pub point_id: String,
    /// JSON 点分路径（如 `data.temperature`、`list.0.value`；语义同 mqtt_in.rs）。
    pub json_path: String,
}

/// HTTP 驱动配置。
#[derive(Debug, Clone)]
pub struct HttpDriverConfig {
    /// 目标 URL（V1 仅 `http://`）。
    pub url: String,
    /// 附加请求头（逐条拼接在默认头之后；Host/Accept/Connection 为默认头）。
    pub headers: Vec<(String, String)>,
    /// 单次请求超时。
    pub timeout: Duration,
    /// 点位配置表（`Driver::read` 以表下标寻址，见模块文档）。
    pub points: Vec<HttpPointConfig>,
}

impl Default for HttpDriverConfig {
    fn default() -> Self {
        Self {
            url: "http://127.0.0.1:8080/data".to_string(),
            headers: Vec::new(),
            timeout: Duration::from_secs(3),
            points: Vec::new(),
        }
    }
}

/// HTTP 采集驱动（每次轮询一个独立 GET 短连接）。
pub struct HttpDriver {
    config: HttpDriverConfig,
}

impl HttpDriver {
    /// 创建驱动。
    pub fn new(config: HttpDriverConfig) -> Self {
        Self { config }
    }

    /// 生成 `Driver::read` 约定的点位地址（`area='H'`，`start`=配置表下标）。
    pub fn point_address(index: usize) -> PointAddress {
        PointAddress {
            db: 0,
            area: Some('H'),
            start: index as u32,
            bit: false,
            bit_index: 0,
        }
    }

    /// 发起一次 GET 并返回全部点位的带质量样本（一轮采集的完整结果）。
    ///
    /// # Errors
    /// URL / 连接 / 超时 / 状态码 / JSON 解析失败 → [`DaemonError`]（映射见局部错误）。
    pub async fn poll(&mut self) -> DaemonResult<Vec<JsonPointSample>> {
        let (host, port, path) = parse_http_url(&self.config.url).map_err(HttpError::from)?;
        let raw = Self::fetch(
            &host,
            port,
            &path,
            &self.config.headers,
            self.config.timeout,
        )
        .await
        .map_err(HttpError::from)?;
        let payload: Value = serde_json::from_slice(&raw)
            .map_err(|e| DaemonError::ProtocolError(format!("http payload json parse: {e}")))?;
        Ok(self
            .config
            .points
            .iter()
            .map(|p| json_point_sample(&p.point_id, &p.json_path, &payload))
            .collect())
    }

    /// 执行一次 GET（连接 → 发送 → 读至 EOF → 解析 → 返回体）。
    async fn fetch(
        host: &str,
        port: u16,
        path: &str,
        headers: &[(String, String)],
        timeout_dur: Duration,
    ) -> Result<Vec<u8>, HttpError> {
        let addr = format!("{host}:{port}");
        let stream = TcpStream::connect(&addr)
            .await
            .map_err(|e| HttpError::Network(format!("http connect {addr}: {e}")))?;
        let request = build_get_request(host, port, path, headers);
        let io = async {
            let mut stream = stream;
            stream
                .write_all(request.as_bytes())
                .await
                .map_err(|e| HttpError::Network(format!("http send request: {e}")))?;
            let mut raw = Vec::new();
            stream
                .read_to_end(&mut raw)
                .await
                .map_err(|e| HttpError::Network(format!("http read response: {e}")))?;
            Ok::<Vec<u8>, HttpError>(raw)
        };
        match timeout(timeout_dur, io).await {
            Ok(result) => {
                // 状态行 / Content-Length / chunked 解析在此收口。
                parse_http_response(&result?)
            }
            Err(_) => Err(HttpError::Network("http request timeout".to_string())),
        }
    }
}

#[async_trait]
impl Driver for HttpDriver {
    /// 预检：对目标 TCP 连接一次即断（不做 HTTP 层校验）。
    async fn connect(&mut self) -> DaemonResult<()> {
        let (host, port, _) = parse_http_url(&self.config.url).map_err(HttpError::from)?;
        let addr = format!("{host}:{port}");
        let stream = TcpStream::connect(&addr)
            .await
            .map_err(|e| DaemonError::NetworkError(format!("http precheck connect {addr}: {e}")))?;
        // 预检连接立即释放。
        drop(stream);
        Ok(())
    }

    /// 轮询一次并按配置表下标提取多点（地址约定见模块文档）。
    async fn read(&mut self, points: &[ReadPoint]) -> DaemonResult<Vec<PointSample>> {
        if points.is_empty() {
            return Ok(Vec::new());
        }
        // 单次 GET 提取全部点位（「轮询一次并提取多点」）。
        let samples = self.poll().await?;
        let mut out = Vec::with_capacity(points.len());
        for p in points {
            if p.address.area != Some('H') {
                return Err(DaemonError::ProtocolError(format!(
                    "http read requires area 'H' point index address, got {:?}",
                    p.address
                )));
            }
            if p.count != 1 {
                return Err(DaemonError::ProtocolError(format!(
                    "http read count must be 1 (no contiguous length semantics), got {}",
                    p.count
                )));
            }
            let index = p.address.start as usize;
            let sample = samples.get(index).ok_or_else(|| {
                DaemonError::ProtocolError(format!(
                    "http point index {index} out of range (configured {} points)",
                    samples.len()
                ))
            })?;
            if sample.quality == protocol_proto::Quality::Good {
                out.push(PointSample {
                    address: p.address.clone(),
                    value: sample.value.to_le_bytes().to_vec(),
                });
            } else {
                // trait 契约：任一点失败整体返回 Err；质量细节可用 poll() 获取。
                return Err(DaemonError::ProtocolError(format!(
                    "http point {} extraction failed: {}",
                    sample.point_id, sample.detail
                )));
            }
        }
        Ok(out)
    }

    /// 不支持写（采集只读），显式报错。
    async fn write(&mut self, _points: &[WritePoint]) -> DaemonResult<()> {
        Err(DaemonError::ProtocolError(
            "http driver does not support write (collection is read-only)".to_string(),
        ))
    }

    /// 无跨请求状态，断开为无操作。
    async fn disconnect(&mut self) -> DaemonResult<()> {
        Ok(())
    }
}

// ---- 测试 ----

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::SocketAddr;

    use crate::driver::mqtt_in::PathError;
    use crate::error::{ERR_NETWORK, ERR_PROTOCOL};
    use protocol_proto::Quality;

    /// 测试用驱动配置（指向 mock 服务器）。
    fn test_config(url: String, points: Vec<HttpPointConfig>) -> HttpDriverConfig {
        HttpDriverConfig {
            url,
            headers: Vec::new(),
            timeout: Duration::from_secs(2),
            points,
        }
    }

    /// mock HTTP 服务器：读到请求头结束即按 handler 回放响应，随后断开
    /// （配合客户端 `Connection: close` 的 read_to_end）。
    async fn spawn_http_mock<F>(handler: F) -> SocketAddr
    where
        F: Fn(&[u8]) -> Vec<u8> + Send + Sync + 'static,
    {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let addr = listener.local_addr().expect("local addr");
        let handler = std::sync::Arc::new(handler);
        tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    return;
                };
                let handler = std::sync::Arc::clone(&handler);
                tokio::spawn(async move {
                    let mut buf = Vec::new();
                    let mut chunk = [0u8; 1024];
                    loop {
                        // 读到请求头结束（\r\n\r\n）为止。
                        match stream.read(&mut chunk).await {
                            Ok(0) | Err(_) => return,
                            Ok(n) => {
                                buf.extend_from_slice(&chunk[..n]);
                                if buf.windows(4).any(|w| w == b"\r\n\r\n") {
                                    break;
                                }
                            }
                        }
                    }
                    let response = handler(&buf);
                    let _ = stream.write_all(&response).await;
                    // Connection: close：写完即断，客户端 read_to_end 收尾。
                });
            }
        });
        addr
    }

    /// 组装 200 OK + Content-Length 响应。
    fn ok_response(body: &str) -> Vec<u8> {
        format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        )
        .into_bytes()
    }

    /// 组装 chunked 编码响应。
    fn chunked_response(body: &[u8]) -> Vec<u8> {
        let mut out = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n".to_vec();
        let mid = body.len() / 2;
        for part in [&body[..mid], &body[mid..]] {
            out.extend_from_slice(format!("{:x}\r\n", part.len()).as_bytes());
            out.extend_from_slice(part);
            out.extend_from_slice(b"\r\n");
        }
        out.extend_from_slice(b"0\r\n\r\n");
        out
    }

    // ---- URL 解析 ----

    /// QA Happy: 默认端口 / 显式端口 / 默认路径 / 带查询串。
    #[test]
    fn parse_http_url_happy() {
        assert_eq!(
            parse_http_url("http://plc.local/data?id=1").expect("ok"),
            ("plc.local".to_string(), 80, "/data?id=1".to_string())
        );
        assert_eq!(
            parse_http_url("http://192.168.1.10:8080/api/v1").expect("ok"),
            ("192.168.1.10".to_string(), 8080, "/api/v1".to_string())
        );
        assert_eq!(
            parse_http_url("http://host").expect("ok"),
            ("host".to_string(), 80, "/".to_string())
        );
    }

    /// QA Error: https / 空主机 / 端口非法 → ProtocolError。
    #[test]
    fn parse_http_url_errors() {
        for url in [
            "https://secure.local/data",
            "ftp://host/x",
            "http:///no-host",
            "http://host:notaport/x",
            "http://host:99999/x",
            "",
        ] {
            let err = parse_http_url(url).expect_err(&format!("must reject {url:?}"));
            assert!(
                matches!(err, HttpError::Protocol(_)),
                "for {url:?}: {err:?}"
            );
        }
    }

    // ---- GET 报文构建 ----

    /// QA: 请求报文 golden（默认头 + 自定义头 + \r\n\r\n 结尾）。
    #[test]
    fn build_get_request_golden() {
        let request = build_get_request(
            "plc.local",
            8080,
            "/api/v1/data",
            &[("X-Token".to_string(), "abc".to_string())],
        );
        assert_eq!(
            request,
            "GET /api/v1/data HTTP/1.1\r\n\
             Host: plc.local:8080\r\n\
             Accept: application/json\r\n\
             Connection: close\r\n\
             X-Token: abc\r\n\
             \r\n"
        );
        // 80 端口省略端口后缀。
        let request = build_get_request("host", 80, "/", &[]);
        assert!(
            request.starts_with("GET / HTTP/1.1\r\nHost: host\r\n"),
            "{request:?}"
        );
        assert!(request.ends_with("\r\n\r\n"), "\\r\\n\\r\\n terminated");
    }

    // ---- 路径提取与数值转换（共享实现位于 mqtt_in.rs，此处回归验证驱动侧语义） ----

    /// QA: 提取 happy path（对象键 / 数组下标 / 嵌套）与错误路径。
    #[test]
    fn extraction_paths() {
        let root: Value = serde_json::from_str(
            r#"{"data":{"temperature":21.5},"list":[{"value":1},{"value":2}]}"#,
        )
        .expect("json");
        assert_eq!(
            json_point_sample("t", "data.temperature", &root),
            JsonPointSample::good("t", 21.5)
        );
        assert_eq!(
            json_point_sample("t", "list.0.value", &root),
            JsonPointSample::good("t", 1.0)
        );
        // 越界 → Bad。
        let oob = json_point_sample("t", "list.9.value", &root);
        assert_eq!(oob.quality, Quality::Bad);
        assert!(oob.detail.contains("out of range"), "{}", oob.detail);
        // 类型错误 → Bad。
        let type_err = json_point_sample("t", "data.temperature.nested", &root);
        assert_eq!(type_err.quality, Quality::Bad);
        assert!(
            type_err.detail.contains("cannot descend"),
            "{}",
            type_err.detail
        );
        // 缺键 → Bad。
        assert_eq!(
            json_point_sample("t", "data.humidity", &root).quality,
            Quality::Bad
        );
        // 空路径 → PathError::Empty（共享错误类型契约）。
        assert_eq!(
            crate::driver::mqtt_in::extract_path(&root, ""),
            Err(PathError::Empty)
        );
    }

    // ---- chunked 解码 ----

    /// QA: chunked golden 解码（多块 + 终止块）。
    #[test]
    fn decode_chunked_golden() {
        let body = b"{\"value\":42}";
        let mut wire = b"4\r\n".to_vec();
        wire.extend_from_slice(&body[..4]);
        wire.extend_from_slice(b"\r\n");
        wire.extend_from_slice(format!("{:x}\r\n", body.len() - 4).as_bytes());
        wire.extend_from_slice(&body[4..]);
        wire.extend_from_slice(b"\r\n0\r\n\r\n");
        assert_eq!(decode_chunked(&wire).expect("decode ok"), body.to_vec());
    }

    /// QA: chunk 扩展（`;ext`）被忽略；非法块长 / 截断报错。
    #[test]
    fn decode_chunked_extension_and_errors() {
        let wire = b"3;foo=bar\r\nabc\r\n0\r\n\r\n";
        assert_eq!(decode_chunked(wire).expect("decode ok"), b"abc".to_vec());

        let bad_size = b"zz\r\nabc\r\n";
        assert!(matches!(
            decode_chunked(bad_size),
            Err(HttpError::Protocol(_))
        ));
        let truncated = b"5\r\nabc";
        assert!(matches!(
            decode_chunked(truncated),
            Err(HttpError::Protocol(_))
        ));
        let unterminated = b"3\r\nabc";
        assert!(matches!(
            decode_chunked(unterminated),
            Err(HttpError::Protocol(_))
        ));
    }

    // ---- 响应解析 ----

    /// QA: Content-Length / 无长度（EOF 定界）/ chunked 三种定界方式。
    #[test]
    fn parse_http_response_body_delimiting() {
        let raw = ok_response("{\"a\":1}");
        assert_eq!(
            parse_http_response(&raw).expect("ok"),
            b"{\"a\":1}".to_vec(),
            "content-length honored"
        );

        let raw = b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\r\nno-length-body";
        assert_eq!(
            parse_http_response(raw).expect("ok"),
            b"no-length-body".to_vec(),
            "eof delimited"
        );

        let raw = chunked_response(b"{\"b\":2}");
        assert_eq!(
            parse_http_response(&raw).expect("ok"),
            b"{\"b\":2}".to_vec(),
            "chunked decoded"
        );
    }

    /// QA Error: 非 2xx / 缺头终止符 / 声明长度截断 → Protocol。
    #[test]
    fn parse_http_response_errors() {
        let raw = b"HTTP/1.1 500 Internal Server Error\r\nContent-Length: 0\r\n\r\n";
        assert!(matches!(
            parse_http_response(raw),
            Err(HttpError::Protocol(_))
        ));
        let raw = b"HTTP/1.1 200 OK\r\nno-terminator";
        assert!(matches!(
            parse_http_response(raw),
            Err(HttpError::Protocol(_))
        ));
        let raw = b"HTTP/1.1 200 OK\r\nContent-Length: 10\r\n\r\nshort";
        assert!(matches!(
            parse_http_response(raw),
            Err(HttpError::Protocol(_))
        ));
        let raw = b"garbage";
        assert!(matches!(
            parse_http_response(raw),
            Err(HttpError::Protocol(_))
        ));
    }

    // ---- mock 服务器端到端 ----

    /// QA Happy: poll 一轮提取多点（Content-Length 响应）。
    #[tokio::test]
    async fn mock_poll_extracts_points() {
        let addr = spawn_http_mock(move |_request| {
            ok_response(r#"{"data":{"temperature":21.5},"humidity":58}"#)
        })
        .await;

        let mut driver = HttpDriver::new(test_config(
            format!("http://{addr}/sensors"),
            vec![
                HttpPointConfig {
                    point_id: "temp".to_string(),
                    json_path: "data.temperature".to_string(),
                },
                HttpPointConfig {
                    point_id: "hum".to_string(),
                    json_path: "humidity".to_string(),
                },
                HttpPointConfig {
                    point_id: "bad".to_string(),
                    json_path: "no.such.path".to_string(),
                },
            ],
        ));
        let samples = driver.poll().await.expect("poll ok");
        assert_eq!(samples.len(), 3);
        assert_eq!(samples[0], JsonPointSample::good("temp", 21.5));
        assert_eq!(samples[1], JsonPointSample::good("hum", 58.0));
        assert_eq!(samples[2].quality, Quality::Bad, "missing path → Bad");
    }

    /// QA Happy: chunked 响应端到端。
    #[tokio::test]
    async fn mock_poll_chunked_response() {
        let addr = spawn_http_mock(move |_request| chunked_response(br#"{"value":42}"#)).await;

        let mut driver = HttpDriver::new(test_config(
            format!("http://{addr}/x"),
            vec![HttpPointConfig {
                point_id: "v".to_string(),
                json_path: "value".to_string(),
            }],
        ));
        let samples = driver.poll().await.expect("poll ok");
        assert_eq!(samples[0], JsonPointSample::good("v", 42.0));
    }

    /// QA Error: 非 2xx → ProtocolError；JSON 非法 → ProtocolError。
    #[tokio::test]
    async fn mock_poll_error_paths() {
        let addr = spawn_http_mock(move |_request| {
            b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\n\r\n".to_vec()
        })
        .await;
        let mut driver = HttpDriver::new(test_config(
            format!("http://{addr}/x"),
            vec![HttpPointConfig {
                point_id: "v".to_string(),
                json_path: "value".to_string(),
            }],
        ));
        let err = driver.poll().await.expect_err("503 must fail");
        assert!(matches!(err, DaemonError::ProtocolError(_)), "{err:?}");
        assert_eq!(err.error_code(), ERR_PROTOCOL);
        assert!(
            err.to_string().contains("503"),
            "message keeps status: {err}"
        );

        let addr = spawn_http_mock(move |_request| ok_response("not-json")).await;
        let mut driver = HttpDriver::new(test_config(format!("http://{addr}/x"), Vec::new()));
        let err = driver.poll().await.expect_err("invalid json must fail");
        assert!(matches!(err, DaemonError::ProtocolError(_)), "{err:?}");
    }

    /// QA Error: 连接拒绝 → NetworkError。
    #[tokio::test]
    async fn connection_refused_maps_to_network_error() {
        let mut driver =
            HttpDriver::new(test_config("http://127.0.0.1:1/x".to_string(), Vec::new()));
        let err = driver.poll().await.expect_err("must fail");
        assert!(matches!(err, DaemonError::NetworkError(_)), "{err:?}");
        assert_eq!(err.error_code(), ERR_NETWORK);
    }

    // ---- Driver trait 行为 ----

    /// QA Happy: connect 预检（TCP 连一次）、read 按下标提取（f64 LE）、
    /// write 显式不支持。
    #[tokio::test]
    async fn driver_trait_read_write_contract() {
        let addr = spawn_http_mock(move |_request| ok_response(r#"{"value":42}"#)).await;

        let mut driver = HttpDriver::new(test_config(
            format!("http://{addr}/x"),
            vec![HttpPointConfig {
                point_id: "v".to_string(),
                json_path: "value".to_string(),
            }],
        ));
        driver.connect().await.expect("precheck connect");

        // 正确下标地址。
        let point = ReadPoint {
            address: HttpDriver::point_address(0),
            count: 1,
        };
        let samples = driver.read(&[point.clone()]).await.expect("read");
        assert_eq!(samples.len(), 1);
        assert_eq!(samples[0].address, point.address, "echoes request address");
        assert_eq!(
            samples[0].value,
            42.0f64.to_le_bytes().to_vec(),
            "f64 little-endian encoded"
        );

        // 提取失败（越界下标 / 坏路径）→ 整体 Err。
        let err = driver
            .read(&[ReadPoint {
                address: HttpDriver::point_address(9),
                count: 1,
            }])
            .await
            .expect_err("index out of range");
        assert!(matches!(err, DaemonError::ProtocolError(_)), "{err:?}");

        // 非法地址区（非 'H'）。
        let err = driver
            .read(&[ReadPoint {
                address: crate::driver::PointAddressParser::parse("D100").expect("addr"),
                count: 1,
            }])
            .await
            .expect_err("area must be 'H'");
        assert!(matches!(err, DaemonError::ProtocolError(_)), "{err:?}");

        // write 显式不支持。
        let err = driver
            .write(&[WritePoint {
                address: HttpDriver::point_address(0),
                value: vec![1],
            }])
            .await
            .expect_err("write unsupported");
        assert!(matches!(err, DaemonError::ProtocolError(_)), "{err:?}");
        assert_eq!(err.error_code(), ERR_PROTOCOL);

        // 断开为无操作。
        driver.disconnect().await.expect("disconnect");
    }

    /// QA Error: https URL 在 connect/read 处即报错（V1 无 TLS）。
    #[tokio::test]
    async fn https_url_rejected() {
        let mut driver = HttpDriver::new(test_config(
            "https://secure.local/x".to_string(),
            Vec::new(),
        ));
        let err = driver.connect().await.expect_err("https rejected");
        assert!(matches!(err, DaemonError::ProtocolError(_)), "{err:?}");
        assert!(
            err.to_string().contains("http://"),
            "message hints scheme restriction: {err}"
        );
    }

    /// 空点位表 read 直通：不发起请求返回空。
    #[tokio::test]
    async fn empty_read_is_noop() {
        let mut driver =
            HttpDriver::new(test_config("http://127.0.0.1:1/x".to_string(), Vec::new()));
        assert!(driver.read(&[]).await.expect("empty read").is_empty());
    }
}

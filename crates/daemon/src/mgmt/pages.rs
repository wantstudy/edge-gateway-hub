//! 页面级管理 API 补齐（web-console real 模式页面契约：mock-data.ts 头注释映射）。
//!
//! ## 端点清单（对齐 `web-console/src/mock/mock-data.ts` 契约注释）
//! - `GET  /api/points/export`            点表 CSV 导出（读，开放）——导出即可当导入模板；
//! - `POST /api/points/import`            点表 CSV 批量导入（`point.write`，仅 system）；
//! - `POST /api/settings/rollback`        回滚到最近 `config.toml.bak-*` 备份（`device.write`，仅 system）；
//! - `POST /api/devices/test`             设备连通性探测（`device.view`，modbus-tcp/rtu 全探测）；
//! - `GET  /api/forwarders`               北向出口列表（读，开放；= `/api/outlets` + `id` 锚点）；
//! - `POST /api/forwarders`               出口登记（`device.write`）——**诚实 501**（写能力未落地）；
//! - `POST /api/forwarders/:id/test`      出口 TCP 可达性探测（`device.view`）；
//! - `GET  /api/rules`                    转发规则（读，开放）——**诚实空态**（无规则引擎数据源）；
//! - `GET  /api/alerts`                   告警（读，开放）——**诚实空态**（无告警引擎数据源）；
//! - `PUT  /api/alerts/rules`             告警规则写（`device.write`）——**诚实 501**；
//! - `GET  /api/license/status`           授权状态快照（读，开放；来自 `LicenseRuntime` watch）。
//!
//! ## CSV 契约（硬契约：校验错误必须「行号 + 原因 + 允许值」；导出即可当导入模板）
//! - 列：`device_id,point_id,protocol,address,frequency_ms`（= `config.rs PointConfig`）；
//! - **行号** = 文件内 1 基物理行号（表头 = 第 1 行）；
//! - 校验失败**整批拒绝**（fail-closed，零落盘），400 返回
//!   `{error: "validation_failed", errors: [{line, reason, allowed}, ...]}`；
//! - 支持最简 RFC4180：逗号分隔、双引号包裹、`""` 转义（手写解析，**零新依赖**）；
//! - modbus 地址接受两种形态（与 southbound 运行期语义一致）：寄存器号
//!   （`40001`，过 `PointAddressParser`）或端点式（`host:port`，southbound
//!   `address` 列的实际语义）——导出再导入因此总是可往返；
//! - XLSX 明确**不支持**（记录为后续缺口，非 CSV 一律按解析失败报 400）。
//!
//! ## 复用与红线
//! - 写路径复用 writeapi 既有链路：全进程写锁（`writeapi::write_guard`）→
//!   校验 → license 配额闸门 → `GatewayConfig::save`（写前备份 + 原子落盘）→
//!   `ConfigShared::replace` 热生效 → mgmt 事件（SSE 三源合流）→ 审计
//!   （内存环 + 持久哈希链，经 `writeapi::audit`）；
//! - 大数红线：`config_version` / 时间戳 / 计数一律字符串编码；
//! - **不伪造数据**：无真实数据源的端点返回诚实空态 / 501，绝不构造演示抖动。

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use axum::body::Bytes;
use axum::extract::{Path as AxumPath, Query, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Json, Response};
use serde_json::{json, Value};
use tokio::net::TcpStream;

use super::rbac::{AuthedRole, Permission};
use super::remote_ops::{
    self, OpsAction, OUTCOME_ACCEPTED, OUTCOME_BAD_REQUEST, OUTCOME_DENIED, OUTCOME_NOT_IMPLEMENTED,
};
use super::writeapi;
use super::{MgmtEvent, MgmtState};
use crate::auth::client::LicenseState;
use crate::config::GatewayConfig;
use crate::driver::modbus::{ModbusConfig, ModbusDriver, ModbusFraming};
use crate::driver::{Driver, PointAddressParser, ReadPoint};

/// CSV 模板表头（导出即模板；导入第 1 行必须含全部列名）。
const POINTS_CSV_HEADER: &str = "device_id,point_id,protocol,address,frequency_ms";
/// 点表导入单批行数上限（防御性上限，防超大请求打爆内存）。
const MAX_IMPORT_ROWS: usize = 10_000;
/// 设备 / 出口探测缺省超时（毫秒）。
const DEFAULT_PROBE_TIMEOUT_MS: u64 = 3_000;
/// 探测超时下限（毫秒）。
const MIN_PROBE_TIMEOUT_MS: u64 = 100;
/// 探测超时上限（毫秒）。
const MAX_PROBE_TIMEOUT_MS: u64 = 10_000;
/// modbus-tcp 探测的缺省探针寄存器（1 基 5 位编址 `40001` = 保持寄存器 0）。
const DEFAULT_PROBE_REGISTER: &str = "40001";
/// modbus 缺省端口（southbound `DEFAULT_SOUTHBOUND_PORT` 同口径）。
const DEFAULT_MODBUS_PORT: u16 = 502;

// ===========================================================================
// 通用辅助
// ===========================================================================

/// 探测超时解析：接受 JSON number 或 string（大数红线），钳制到
/// `[MIN, MAX]`，缺省 [`DEFAULT_PROBE_TIMEOUT_MS`]。
#[allow(clippy::result_large_err)]
fn parse_probe_timeout(raw: Option<&Value>) -> Result<u64, Response> {
    let Some(raw) = raw else {
        return Ok(DEFAULT_PROBE_TIMEOUT_MS);
    };
    let ms = match raw {
        Value::Number(n) => n.as_u64(),
        Value::String(s) => s.trim().parse::<u64>().ok(),
        _ => None,
    };
    let Some(ms) = ms else {
        return Err(writeapi::validation_error(
            "timeout_ms",
            &format!("not a valid u64: {raw}"),
            &format!("integer in [{MIN_PROBE_TIMEOUT_MS}, {MAX_PROBE_TIMEOUT_MS}] (ms); number or string"),
        ));
    };
    Ok(ms.clamp(MIN_PROBE_TIMEOUT_MS, MAX_PROBE_TIMEOUT_MS))
}

/// 解析 `host[:port]` / `[v6][:port]` 为 SocketAddr（DNS 经 tokio 解析；
/// 无端口时补 `default_port`；裸 IPv6 无方括号无法安全补端口 → None）。
async fn resolve_endpoint(host: &str, default_port: u16) -> Option<SocketAddrLike> {
    let trimmed = host.trim();
    let with_port: String = if trimmed.starts_with('[') {
        if trimmed.contains("]:") {
            trimmed.to_string()
        } else {
            format!("{trimmed}:{default_port}")
        }
    } else if trimmed.matches(':').count() == 1 {
        trimmed.to_string() // host:port
    } else if trimmed.contains(':') {
        return None; // 裸 IPv6（多冒号无端口）歧义，拒绝。
    } else {
        format!("{trimmed}:{default_port}") // 纯 host
    };
    // 先绑定再消费：tokio lookup_host 的 Output 类型携带输入借用，
    // match 头部临时会让借用活到函数尾（E0597）；let 绑定使其先于函数尾结束。
    let resolved = tokio::net::lookup_host(with_port.as_str()).await;
    match resolved {
        Ok(mut addrs) => addrs.next().map(SocketAddrLike),
        Err(_) => None,
    }
}

/// [`std::net::SocketAddr`] 的换名包装（保持 resolve_endpoint 私有抽象）。
struct SocketAddrLike(std::net::SocketAddr);

/// 拆解 `scheme://host[:port][/path]` 为 (host, port)。
/// host 保留 IPv6 方括号原样（交由 [`resolve_endpoint`] 处理）。
fn split_broker_url(url: &str) -> Option<(String, Option<u16>)> {
    let (scheme, rest) = url.split_once("://")?;
    if scheme.trim().is_empty() {
        return None;
    }
    let host_port = rest.split('/').next().unwrap_or(rest);
    if host_port.is_empty() {
        return None;
    }
    if let Some(idx) = host_port.rfind(':') {
        let (host, port_raw) = (&host_port[..idx], &host_port[idx + 1..]);
        if !port_raw.is_empty() && port_raw.chars().all(|c| c.is_ascii_digit()) {
            return Some((host.to_string(), port_raw.parse::<u16>().ok()));
        }
        // 尾段非纯数字（如裸 IPv6）→ host 整体返回，端口未知。
        return Some((host_port.to_string(), None));
    }
    Some((host_port.to_string(), None))
}

/// 主机名形态判定（防把 `DB1.DBX0.0` 之类的点位地址误判为端点）：
/// 至少含一个 `.`、或为 `localhost`、或为方括号包裹的 IPv6。
fn looks_like_host(host: &str) -> bool {
    !host.is_empty()
        && (host.contains('.') || host.eq_ignore_ascii_case("localhost") || host.starts_with('['))
}

/// 判定 `host[:port]` 是否为合法端点形态（供 modbus 端点式地址校验）。
fn is_endpoint_shape(raw: &str) -> bool {
    match split_broker_url(&format!("tcp://{raw}")) {
        Some((host, _port)) => looks_like_host(&host),
        None => false,
    }
}

// ===========================================================================
// CSV（手写零依赖解析 / 生成）
// ===========================================================================

/// 单行 CSV 解析（最简 RFC4180：逗号分隔、双引号包裹、`""` 转义；不支持跨行记录）。
fn parse_csv_line(line: &str) -> Result<Vec<String>, String> {
    let mut fields: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut in_quotes = false;
    let mut chars = line.chars().peekable();
    while let Some(ch) = chars.next() {
        if in_quotes {
            if ch == '"' {
                if chars.peek() == Some(&'"') {
                    chars.next();
                    current.push('"');
                } else {
                    in_quotes = false;
                }
            } else {
                current.push(ch);
            }
        } else {
            match ch {
                '"' => {
                    if current.is_empty() {
                        in_quotes = true;
                    } else {
                        return Err("unexpected quote inside unquoted field".to_string());
                    }
                }
                ',' => fields.push(std::mem::take(&mut current)),
                _ => current.push(ch),
            }
        }
    }
    if in_quotes {
        return Err("unterminated quoted field (missing closing quote)".to_string());
    }
    fields.push(current);
    Ok(fields)
}

/// CSV 字段编码：含逗号 / 引号 / 换行的字段用双引号包裹（`""` 转义）。
fn csv_field(value: &str) -> String {
    if value.contains(',') || value.contains('"') || value.contains('\n') || value.contains('\r') {
        format!("\"{}\"", value.replace('"', "\"\""))
    } else {
        value.to_string()
    }
}

/// 由原始表头字段构建「小写列名 → 下标」映射（导入校验与行重建共用）。
fn build_header_map(header_fields: &[String]) -> std::collections::HashMap<String, usize> {
    header_fields
        .iter()
        .enumerate()
        .map(|(idx, name)| (name.trim().to_ascii_lowercase(), idx))
        .collect()
}

/// 单条导入校验错误（「行号 + 原因 + 允许值」硬契约）。
struct ImportError {
    /// 文件内 1 基物理行号（表头 = 1；0 = 请求级错误）。
    line: usize,
    /// 失败原因（人读）。
    reason: String,
    /// 允许值说明（人读）。
    allowed: String,
}

impl ImportError {
    fn new(line: usize, reason: impl Into<String>, allowed: impl Into<String>) -> Self {
        Self {
            line,
            reason: reason.into(),
            allowed: allowed.into(),
        }
    }

    fn to_json(&self) -> Value {
        json!({ "line": self.line.to_string(), "reason": self.reason, "allowed": self.allowed })
    }
}

/// 校验通过的一行导入数据。
struct ImportRow {
    device_id: String,
    point_id: String,
    protocol: String,
    address: String,
    frequency_ms: u64,
}

/// modbus 地址的双重语义校验：寄存器号（`40001`，过 `PointAddressParser`）
/// 或端点式（`host:port`，southbound `address` 列的实际语义）。
/// 其余解析器协议（s7 / mc）仅接受解析器形态；非解析器协议仅非空校验。
/// 返回 `Err((reason, allowed))` 供行级错误报告。
fn check_address(protocol: &str, address: &str) -> Result<(), (String, String)> {
    let trimmed = address.trim();
    if trimmed.is_empty() {
        return Err((
            "address must not be empty".to_string(),
            "non-empty endpoint (host:port) or point address".to_string(),
        ));
    }
    if !writeapi::PROTOCOLS.contains(&protocol) {
        // 调用方已先做协议域校验；此处兜底。
        return Err((
            format!("unknown protocol {protocol:?}"),
            writeapi::PROTOCOLS.join(" | "),
        ));
    }
    let parser_protocols = ["modbus-tcp", "modbus-rtu", "s7", "mc"];
    if !parser_protocols.contains(&protocol) {
        return Ok(()); // opcua / http / mqtt：endpoint / URL 语义，仅非空校验。
    }
    if PointAddressParser::parse(trimmed).is_ok() {
        return Ok(());
    }
    // modbus 允许端点式地址（host:port / host，端口可缺省 = 502）。
    if (protocol == "modbus-tcp" || protocol == "modbus-rtu") && is_endpoint_shape(trimmed) {
        return Ok(());
    }
    Err((
        format!(
            "invalid point address {trimmed:?}: not a register number (e.g. 40001) nor a host:port endpoint"
        ),
        format!(
            "{protocol} address: register number accepted by driver::PointAddressParser \
             (e.g. modbus \"40001\", s7 \"DB1.DBX0.0\", mc \"D100\") or host:port endpoint for modbus"
        ),
    ))
}

/// 导入请求（CSV 文本 + 缺省设备 + 覆盖开关）。
struct ImportRequest {
    csv: String,
    /// 缺省设备：行内 `device_id` 列缺失 / 为空时使用（query `?device_id=` 或 JSON 字段）。
    device_id: String,
    /// `replace=true`：先删除缺省设备的全部既有点位再导入（须显式指定设备）。
    replace: bool,
}

/// 解析导入请求：body 可为裸 CSV 文本（`text/csv`）或 JSON `{"csv": "...", "device_id"?, "replace"?}`；
/// query 参数 `device_id` / `replace` 与 JSON 字段同语义（JSON 优先）。
#[allow(clippy::result_large_err)]
fn parse_import_request(
    params: &std::collections::HashMap<String, String>,
    body: &[u8],
) -> Result<ImportRequest, Response> {
    let mut csv = String::new();
    let mut device_id = params.get("device_id").cloned().unwrap_or_default();
    let mut replace = params.get("replace").map(|v| v == "true").unwrap_or(false);
    if !body.is_empty() {
        // JSON 形态优先识别（JSON 对象须携带 csv 字段；其余按裸 CSV 处理）。
        match serde_json::from_slice::<Value>(body) {
            Ok(value) if value.is_object() => {
                let Some(raw_csv) = value.get("csv").and_then(Value::as_str) else {
                    return Err(writeapi::validation_error(
                        "body",
                        "JSON import body must carry a string field \"csv\"",
                        "{\"csv\": \"device_id,point_id,...\\n...\"} or raw CSV text",
                    ));
                };
                csv = raw_csv.to_string();
                if let Some(id) = value.get("device_id").and_then(Value::as_str) {
                    device_id = id.to_string();
                }
                if let Some(flag) = value.get("replace").and_then(Value::as_bool) {
                    replace = flag;
                }
            }
            Ok(_) => {
                return Err(writeapi::validation_error(
                    "body",
                    "JSON body must be an object",
                    "{\"csv\": \"...\"} or raw CSV text",
                ));
            }
            Err(_) => {
                // 裸 CSV 文本。
                csv = String::from_utf8(body.to_vec()).map_err(|_| {
                    writeapi::validation_error(
                        "body",
                        "CSV body is not valid UTF-8",
                        "UTF-8 encoded CSV text",
                    )
                })?;
            }
        }
    }
    if csv.trim().is_empty() {
        return Err(writeapi::validation_error(
            "csv",
            "csv body is empty",
            "raw CSV text or JSON {\"csv\": \"...\"}",
        ));
    }
    Ok(ImportRequest {
        csv,
        device_id,
        replace,
    })
}

// ===========================================================================
// 写路径（复用 writeapi 既有链路的页面级入口）
// ===========================================================================

/// 页面级配置落盘：license 配额闸门 → 写前备份 + 原子落盘 → 热快照即时替换 →
/// 审计（内存环 + 持久链）。调用方须已持有 [`writeapi::write_guard`] 并完成
/// 全部校验；管理事件由调用方按语义自行发布。
#[allow(clippy::result_large_err)]
fn persist_config(
    state: &MgmtState,
    new_config: GatewayConfig,
    actor: &str,
    action: OpsAction,
    detail: &str,
) -> Result<u64, Response> {
    // 授权配额闸门（免费版 fail-closed，与 writeapi::persist 同口径）。
    if let Some(license) = state.daemon().license_runtime() {
        if let Err(err) = license.enforce_free_limits(&new_config) {
            writeapi::audit(
                state,
                actor,
                action,
                false,
                OUTCOME_BAD_REQUEST,
                &format!("{detail}: rejected by free-edition quota gate: {err}"),
            );
            return Err(writeapi::validation_error(
                "config",
                &format!("free-edition quota rejected: {err}"),
                "free edition: ≤8 devices, modbus-tcp/modbus-rtu only, \
                 frequency_ms ≥ 1000 (activate a license to lift the limits)",
            ));
        }
    }
    let Some(path) = state.config_path() else {
        writeapi::audit(
            state,
            actor,
            action,
            true,
            writeapi::OUTCOME_FAILED,
            "config file path not configured (with_config_path wiring missing); write refused",
        );
        return Err(writeapi::internal(
            "config file path not configured; write refused (fail-closed)",
        ));
    };
    if let Err(err) = new_config.save(&path) {
        writeapi::audit(
            state,
            actor,
            action,
            true,
            writeapi::OUTCOME_FAILED,
            &format!("{detail}: save failed: {err}"),
        );
        return Err(writeapi::internal(&format!("config save failed: {err}")));
    }
    let version = state.daemon().config_shared().replace(new_config);
    writeapi::audit(
        state,
        actor,
        action,
        true,
        OUTCOME_ACCEPTED,
        &format!("{detail}; config_version={version}"),
    );
    Ok(version)
}

// ===========================================================================
// 点表导出 / 批量导入
// ===========================================================================

/// `GET /api/points/export?device_id=` → CSV 点表（读，开放）。
///
/// 列 = `config.rs PointConfig` 平铺；`?device_id=` 过滤单设备（缺省导出全部）。
/// **导出即可当导入模板**（硬契约）：`POST /api/points/import` 接受本端点的输出。
pub async fn points_export(
    State(state): State<MgmtState>,
    Query(params): Query<std::collections::HashMap<String, String>>,
) -> Response {
    let device_filter = params.get("device_id").map(String::as_str).unwrap_or("");
    let config = state.config();
    let mut body = String::from(POINTS_CSV_HEADER);
    body.push('\n');
    for point in &config.points {
        if !device_filter.is_empty() && point.device_id != device_filter {
            continue;
        }
        body.push_str(
            &[
                csv_field(&point.device_id),
                csv_field(&point.point_id),
                csv_field(&point.protocol),
                csv_field(&point.address),
                csv_field(&point.frequency_ms.to_string()),
            ]
            .join(","),
        );
        body.push('\n');
    }
    ([(header::CONTENT_TYPE, "text/csv; charset=utf-8")], body).into_response()
}

/// `POST /api/points/import?device_id=&replace=` → CSV 点表批量导入。
///
/// - 鉴权：`point.write`（仅 system；401/403 由 rbac 层统一编码，被拒入审计）；
/// - 校验错误「行号 + 原因 + 允许值」逐条列出，**任一错误整批拒绝**（零落盘）；
/// - `replace=true`：先删除缺省设备的全部既有点位再导入（须显式指定设备）；
/// - 受理后：写前备份 + 原子落盘 + 热生效 + 逐受影响设备发布 `device_changed`
///   事件（SSE）+ 持久审计。
pub async fn points_import(
    State(state): State<MgmtState>,
    authed: AuthedRole,
    Query(params): Query<std::collections::HashMap<String, String>>,
    body: Bytes,
) -> Response {
    if let Err(rejection) = authed.ensure(Permission::PointWrite) {
        writeapi::audit(
            &state,
            &authed.claims.sub,
            OpsAction::PointImport,
            false,
            OUTCOME_DENIED,
            &rejection.to_string(),
        );
        return rejection.into_response();
    }
    let actor = authed.claims.sub.clone();
    let request = match parse_import_request(&params, body.as_ref()) {
        Ok(request) => request,
        Err(resp) => {
            writeapi::audit(
                &state,
                &actor,
                OpsAction::PointImport,
                true,
                OUTCOME_BAD_REQUEST,
                "malformed import request",
            );
            return resp;
        }
    };

    let _guard = writeapi::write_guard();
    let mut config = (*state.config()).clone();
    let errors = validate_import(&request, &config);
    if !errors.is_empty() {
        let count = errors.len();
        writeapi::audit(
            &state,
            &actor,
            OpsAction::PointImport,
            true,
            OUTCOME_BAD_REQUEST,
            &format!("import rejected: {count} invalid row(s); nothing written (fail-closed)"),
        );
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({
                "error": "validation_failed",
                "message": format!("{count} invalid row(s); nothing was written (fail-closed)"),
                "errors": errors.iter().map(ImportError::to_json).collect::<Vec<Value>>(),
            })),
        )
            .into_response();
    }

    // 校验已通过：重建行集（validate_import 已保证每行合法）。
    let rows = revalidate_rows(&request, &config);
    let replaced = if request.replace {
        let before = config.points.len();
        config.points.retain(|p| p.device_id != request.device_id);
        before - config.points.len()
    } else {
        0
    };
    for row in &rows {
        config.points.push(crate::config::PointConfig {
            device_id: row.device_id.clone(),
            point_id: row.point_id.clone(),
            protocol: row.protocol.clone(),
            address: row.address.clone(),
            frequency_ms: row.frequency_ms,
        });
    }
    let affected: Vec<String> = rows
        .iter()
        .map(|r| r.device_id.clone())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    let detail = format!(
        "import points: imported={}, replaced={}, devices={:?}, replace={}",
        rows.len(),
        replaced,
        affected,
        request.replace
    );
    match persist_config(&state, config, &actor, OpsAction::PointImport, &detail) {
        Ok(version) => {
            for device_id in &affected {
                state.publish(MgmtEvent::DeviceChanged {
                    device_id: device_id.clone(),
                });
            }
            Json(json!({
                "accepted": true,
                "imported": rows.len().to_string(),
                "replaced": replaced.to_string(),
                "devices": affected,
                "config_version": version.to_string(),
            }))
            .into_response()
        }
        Err(resp) => resp,
    }
}

/// 导入校验：返回全部行级错误（空 = 通过）。须在写锁内、对当前配置快照执行。
fn validate_import(request: &ImportRequest, config: &GatewayConfig) -> Vec<ImportError> {
    let mut errors: Vec<ImportError> = Vec::new();

    // 覆盖模式必须有显式目标设备。
    if request.replace && request.device_id.trim().is_empty() {
        errors.push(ImportError::new(
            0,
            "replace=true requires an explicit device_id",
            "device_id (query ?device_id= or JSON field) when replace=true",
        ));
    }

    let mut lines = request.csv.lines().enumerate();
    // 第 1 行：表头（列名 → 下标映射；缺列整批拒绝）。
    let header_map: std::collections::HashMap<String, usize> = match lines.next() {
        Some((_, header_line)) => match parse_csv_line(header_line) {
            Ok(fields) => build_header_map(&fields),
            Err(reason) => {
                errors.push(ImportError::new(
                    1,
                    format!("malformed header: {reason}"),
                    POINTS_CSV_HEADER,
                ));
                std::collections::HashMap::new()
            }
        },
        None => {
            errors.push(ImportError::new(
                1,
                "missing header line",
                POINTS_CSV_HEADER,
            ));
            std::collections::HashMap::new()
        }
    };
    for column in [
        "device_id",
        "point_id",
        "protocol",
        "address",
        "frequency_ms",
    ] {
        if !header_map.contains_key(column) {
            errors.push(ImportError::new(
                1,
                format!("missing column {column:?} in header"),
                POINTS_CSV_HEADER,
            ));
        }
    }
    if !errors.is_empty() {
        return errors;
    }
    let get_col = |fields: &[String], name: &str| -> Option<String> {
        header_map
            .get(name)
            .and_then(|idx| fields.get(*idx))
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
    };

    // 批内去重集合 + 既有 (device, point) 集合（覆盖模式下目标设备既有行将删除，不计入）。
    let mut seen: BTreeSet<(String, String)> = BTreeSet::new();
    let existing: BTreeSet<(String, String)> = config
        .points
        .iter()
        .filter(|p| !(request.replace && p.device_id == request.device_id))
        .map(|p| (p.device_id.clone(), p.point_id.clone()))
        .collect();
    let registered_devices: BTreeSet<String> = config
        .devices
        .iter()
        .map(|d| d.device_id.clone())
        .chain(config.points.iter().map(|p| p.device_id.clone()))
        .collect();

    let mut data_rows = 0usize;
    for (idx, line) in lines {
        let line_no = idx + 1;
        if line.trim().is_empty() {
            continue; // 空行跳过（保留行号计数）。
        }
        data_rows += 1;
        if data_rows > MAX_IMPORT_ROWS {
            errors.push(ImportError::new(
                line_no,
                format!("too many rows (limit {MAX_IMPORT_ROWS})"),
                format!("≤ {MAX_IMPORT_ROWS} data rows per import"),
            ));
            break;
        }
        let fields = match parse_csv_line(line) {
            Ok(fields) => fields,
            Err(reason) => {
                errors.push(ImportError::new(
                    line_no,
                    format!("malformed CSV: {reason}"),
                    "comma-separated fields; quote fields containing commas with double quotes",
                ));
                continue;
            }
        };
        // 覆盖模式：行内 device_id 列忽略，统一回填覆盖目标设备（对齐 mock
        // `replacePointsOfDevice` 语义——「覆盖导入到指定设备」；这使
        // 「导出 → 换设备覆盖导入」的往返无需手改 CSV 的 device_id 列）。
        let device_id = if request.replace {
            request.device_id.clone()
        } else {
            get_col(&fields, "device_id").unwrap_or_else(|| request.device_id.clone())
        };
        if device_id.is_empty() {
            errors.push(ImportError::new(
                line_no,
                "device_id is empty (no column value and no ?device_id= default)",
                "device_id column or ?device_id= query default",
            ));
            continue;
        }
        if request.replace && device_id.is_empty() {
            errors.push(ImportError::new(
                line_no,
                "replace=true requires an explicit device_id (query ?device_id= or JSON field)",
                "device_id default when replace=true",
            ));
            continue;
        }
        if !registered_devices.contains(&device_id) {
            errors.push(ImportError::new(
                line_no,
                format!("unknown device {device_id:?}"),
                "device registered in [[devices]] or having existing [[points]] rows",
            ));
            continue;
        }
        let Some(point_id) = get_col(&fields, "point_id") else {
            errors.push(ImportError::new(
                line_no,
                "point_id is empty",
                "non-empty point identifier",
            ));
            continue;
        };
        let Some(protocol) = get_col(&fields, "protocol") else {
            errors.push(ImportError::new(
                line_no,
                "protocol is empty",
                writeapi::PROTOCOLS.join(" | "),
            ));
            continue;
        };
        if !writeapi::PROTOCOLS.contains(&protocol.as_str()) {
            errors.push(ImportError::new(
                line_no,
                format!("unknown protocol {protocol:?}"),
                writeapi::PROTOCOLS.join(" | "),
            ));
            continue;
        }
        let Some(address) = get_col(&fields, "address") else {
            errors.push(ImportError::new(
                line_no,
                "address is empty",
                "non-empty endpoint (host:port) or point address",
            ));
            continue;
        };
        if let Err((reason, allowed)) = check_address(&protocol, &address) {
            errors.push(ImportError::new(line_no, reason, allowed));
            continue;
        }
        // 频率仅做校验（不合法 → 行错误）；合法行由 revalidate_rows 重建。
        let _frequency_ms = match fields
            .get(header_map["frequency_ms"])
            .map(|s: &String| s.trim())
            .filter(|s| !s.is_empty())
        {
            None => writeapi::DEFAULT_FREQUENCY_MS,
            Some(raw) => match raw.parse::<u64>() {
                Ok(ms) if ms >= writeapi::MIN_FREQUENCY_MS => ms,
                Ok(ms) => {
                    errors.push(ImportError::new(
                        line_no,
                        format!("frequency_ms {ms} is below the minimum collection frequency"),
                        format!(
                            ">= {} (ms); empty = {}",
                            writeapi::MIN_FREQUENCY_MS,
                            writeapi::DEFAULT_FREQUENCY_MS
                        ),
                    ));
                    continue;
                }
                Err(_) => {
                    errors.push(ImportError::new(
                        line_no,
                        format!("frequency_ms {raw:?} is not a valid u64"),
                        format!(
                            "integer >= {} (ms); empty = {}",
                            writeapi::MIN_FREQUENCY_MS,
                            writeapi::DEFAULT_FREQUENCY_MS
                        ),
                    ));
                    continue;
                }
            },
        };
        let key = (device_id.clone(), point_id.clone());
        if !seen.insert(key.clone()) {
            errors.push(ImportError::new(
                line_no,
                format!("duplicate point {device_id:?}/{point_id:?} within the import file"),
                "unique (device_id, point_id) pair within the file",
            ));
            continue;
        }
        if existing.contains(&key) {
            errors.push(ImportError::new(
                line_no,
                format!("point {device_id:?}/{point_id:?} already exists"),
                "unique point id within the device (use replace=true to overwrite the device's point table)",
            ));
            continue;
        }
    }
    errors
}

/// 校验通过后重建行集（与 [`validate_import`] 同规则；调用时已保证零错误）。
fn revalidate_rows(request: &ImportRequest, config: &GatewayConfig) -> Vec<ImportRow> {
    let mut rows = Vec::new();
    let mut lines = request.csv.lines();
    let Ok(header_fields) = lines
        .next()
        .map(parse_csv_line)
        .unwrap_or(Err("empty".into()))
    else {
        return rows;
    };
    let header_map = build_header_map(&header_fields);
    let get_col = |fields: &[String], name: &str| -> String {
        header_map
            .get(name)
            .and_then(|idx| fields.get(*idx))
            .map(|s| s.trim().to_string())
            .unwrap_or_default()
    };
    for line in lines {
        if line.trim().is_empty() {
            continue;
        }
        let Ok(fields) = parse_csv_line(line) else {
            continue;
        };
        // 覆盖模式回填目标设备（与 validate_import 同规则）。
        let device_id = if request.replace {
            request.device_id.clone()
        } else {
            let v = get_col(&fields, "device_id");
            if v.is_empty() {
                request.device_id.clone()
            } else {
                v
            }
        };
        let point_id = get_col(&fields, "point_id");
        let protocol = get_col(&fields, "protocol");
        let address = get_col(&fields, "address");
        let freq_raw = get_col(&fields, "frequency_ms");
        // 设备存在性 / 合法性由 validate_import 保证；此处只做防御性兜底。
        let known = config.devices.iter().any(|d| d.device_id == device_id)
            || config.points.iter().any(|p| p.device_id == device_id);
        if !known || point_id.is_empty() || protocol.is_empty() || address.is_empty() {
            continue;
        }
        let frequency_ms = if freq_raw.is_empty() {
            writeapi::DEFAULT_FREQUENCY_MS
        } else {
            freq_raw.parse().unwrap_or(writeapi::DEFAULT_FREQUENCY_MS)
        };
        rows.push(ImportRow {
            device_id,
            point_id,
            protocol,
            address,
            frequency_ms,
        });
    }
    rows
}

// ===========================================================================
// 配置回滚
// ===========================================================================

/// `POST /api/settings/rollback` → 回滚到最近 `config.toml.bak-*` 备份。
///
/// - 鉴权：`device.write`（仅 system——配置级高危写动作，复用既有语义）；
/// - body（可选）：`{backup?: string, reason?: string}`；缺省取文件名字典序最大的
///   备份（unix 秒定长，字典序 = 时间序；同秒多份 `-N` 序号最大者最新）；
/// - 流程：备份文件 → 解析校验（坏备份 fail-closed 拒绝）→ license 配额闸门 →
///   `GatewayConfig::save`（对当前配置**再做一次写前备份**，回滚自身可逆）→
///   热生效 → `config_reloaded` 事件 → 持久审计；
/// - 无备份 → 404 `{error: "no_backup"}`。
pub async fn settings_rollback(
    State(state): State<MgmtState>,
    authed: AuthedRole,
    body: Bytes,
) -> Response {
    if let Err(rejection) = authed.ensure(Permission::DeviceWrite) {
        writeapi::audit(
            &state,
            &authed.claims.sub,
            OpsAction::SettingsRollback,
            false,
            OUTCOME_DENIED,
            &rejection.to_string(),
        );
        return rejection.into_response();
    }
    let actor = authed.claims.sub.clone();
    #[derive(serde::Deserialize, Default)]
    struct RollbackBody {
        #[serde(default)]
        backup: String,
        #[serde(default)]
        reason: String,
    }
    let req: RollbackBody = if body.is_empty() {
        RollbackBody::default()
    } else {
        match serde_json::from_slice(&body) {
            Ok(v) => v,
            Err(err) => {
                writeapi::audit(
                    &state,
                    &actor,
                    OpsAction::SettingsRollback,
                    true,
                    OUTCOME_BAD_REQUEST,
                    &format!("malformed json body: {err}"),
                );
                return writeapi::validation_error(
                    "body",
                    &format!("malformed JSON: {err}"),
                    "object {backup?, reason?} (both optional)",
                );
            }
        }
    };

    let _guard = writeapi::write_guard();
    let Some(path) = state.config_path() else {
        writeapi::audit(
            &state,
            &actor,
            OpsAction::SettingsRollback,
            true,
            writeapi::OUTCOME_FAILED,
            "config file path not configured (with_config_path wiring missing); rollback refused",
        );
        return writeapi::internal(
            "config file path not configured; rollback refused (fail-closed)",
        );
    };
    let file_name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("config.toml")
        .to_string();
    let prefix = format!("{file_name}.bak-");
    let dir = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .map_or_else(|| PathBuf::from("."), std::borrow::ToOwned::to_owned);

    // 选备份：显式指定（仅接受同目录文件名，防路径穿越）或最新一份。
    let backup_path: std::path::PathBuf = if req.backup.trim().is_empty() {
        let mut backups: Vec<String> = std::fs::read_dir(&dir)
            .map(|entries| {
                entries
                    .filter_map(|e| e.ok())
                    .filter(|e| e.path().is_file())
                    .filter_map(|e| e.file_name().to_str().map(ToString::to_string))
                    .filter(|n| n.starts_with(&prefix) && !n.contains(".tmp"))
                    .collect()
            })
            .unwrap_or_default();
        backups.sort();
        match backups.pop() {
            Some(name) => dir.join(name),
            None => {
                writeapi::audit(
                    &state,
                    &actor,
                    OpsAction::SettingsRollback,
                    true,
                    OUTCOME_BAD_REQUEST,
                    "no backup file found",
                );
                return rollback_no_backup(&file_name);
            }
        }
    } else {
        let name = req.backup.trim();
        let name_ok = !name.contains('/')
            && !name.contains('\\')
            && !name.contains("..")
            && name.starts_with(&prefix)
            && !name.contains(".tmp");
        if !name_ok {
            writeapi::audit(
                &state,
                &actor,
                OpsAction::SettingsRollback,
                true,
                OUTCOME_BAD_REQUEST,
                &format!("invalid backup name {name:?}"),
            );
            return writeapi::validation_error(
                "backup",
                &format!("invalid backup name {name:?}"),
                &format!("file name matching {prefix}* within the config directory"),
            );
        }
        let candidate = dir.join(name);
        if !candidate.is_file() {
            writeapi::audit(
                &state,
                &actor,
                OpsAction::SettingsRollback,
                true,
                OUTCOME_BAD_REQUEST,
                &format!("backup {name:?} not found"),
            );
            return (
                StatusCode::NOT_FOUND,
                Json(json!({
                    "error": "no_backup",
                    "message": format!("backup {name:?} not found in the config directory"),
                })),
            )
                .into_response();
        }
        candidate
    };

    // 读取 + 解析校验（坏备份 fail-closed：绝不写半份配置）。
    let backup_name = backup_path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("")
        .to_string();
    let raw = match std::fs::read_to_string(&backup_path) {
        Ok(raw) => raw,
        Err(err) => {
            writeapi::audit(
                &state,
                &actor,
                OpsAction::SettingsRollback,
                true,
                writeapi::OUTCOME_FAILED,
                &format!("backup {backup_name:?} unreadable: {err}"),
            );
            return writeapi::internal(&format!("backup {backup_name:?} unreadable"));
        }
    };
    let restored = match GatewayConfig::parse(&raw) {
        Ok(config) => config,
        Err(err) => {
            writeapi::audit(
                &state,
                &actor,
                OpsAction::SettingsRollback,
                true,
                writeapi::OUTCOME_FAILED,
                &format!("backup {backup_name:?} not parseable: {err}"),
            );
            return writeapi::internal(&format!(
                "backup {backup_name:?} failed config validation; nothing was written (fail-closed)"
            ));
        }
    };
    let detail = format!(
        "rollback config to backup {backup_name:?}; reason={:?}",
        req.reason
    );
    match persist_config(
        &state,
        restored,
        &actor,
        OpsAction::SettingsRollback,
        &detail,
    ) {
        Ok(version) => {
            state.publish(MgmtEvent::ConfigReloaded { version });
            Json(json!({
                "accepted": true,
                "restored_from": backup_name,
                "config_version": version.to_string(),
                "note": "当前配置已在回滚前再次备份（回滚可逆）；热重载即时生效",
            }))
            .into_response()
        }
        Err(resp) => resp,
    }
}

/// 404 无备份错误体。
fn rollback_no_backup(file_name: &str) -> Response {
    (
        StatusCode::NOT_FOUND,
        Json(json!({
            "error": "no_backup",
            "message": format!("no {file_name}.bak-* backup found; write a config change first (each write creates a pre-write backup)"),
        })),
    )
        .into_response()
}

// ===========================================================================
// 设备 / 出口连通性探测
// ===========================================================================

/// `POST /api/devices/test` → 设备连通性探测（结构化结果，**失败不 500**）。
///
/// - body：`{device_id}`（按既有配置探测：端点取该设备首条点位的 `address`，
///   寄存器取首个可解析为 4xxxx/3xxxx 的 `point_id`，与 southbound 轮询同语义）
///   或 `{protocol, address, register?, slave?}`（登记前探测）；
///   `timeout_ms` 可选（number/string，钳制 100..=10000，缺省 3000）；
/// - 仅 `modbus-tcp` / `modbus-rtu`（RTU-over-TCP）做**全探测**（建连 + 读 1 个
///   保持寄存器，复用 driver 层 `ModbusDriver`，只调用不修改）；其余协议诚实
///   返回 `unsupported_protocol`（无对应结构化探测器，不伪造结果）；
/// - 响应恒 200：`{ok: true, ...}` 或 `{ok: false, error_kind, reason}`
///   （error_kind ∈ unreachable / timeout / protocol_error / no_endpoint /
///   invalid_endpoint / unsupported_protocol）。
pub async fn device_test(
    State(state): State<MgmtState>,
    authed: AuthedRole,
    body: Bytes,
) -> Response {
    if let Err(rejection) = authed.ensure(Permission::DeviceView) {
        remote_ops::runtime_for(&state).record_audit(
            &authed.claims.sub,
            OpsAction::DeviceTest,
            false,
            OUTCOME_DENIED,
            &rejection.to_string(),
        );
        return rejection.into_response();
    }
    let actor = authed.claims.sub.clone();

    #[derive(serde::Deserialize, Default)]
    struct DeviceTestBody {
        #[serde(default)]
        device_id: String,
        #[serde(default)]
        protocol: String,
        #[serde(default)]
        address: String,
        #[serde(default)]
        register: String,
        #[serde(default)]
        slave: Option<Value>,
        #[serde(default)]
        timeout_ms: Option<Value>,
    }
    let req: DeviceTestBody = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(err) => {
            remote_ops::runtime_for(&state).record_audit(
                &actor,
                OpsAction::DeviceTest,
                true,
                OUTCOME_BAD_REQUEST,
                &format!("malformed json body: {err}"),
            );
            return writeapi::validation_error(
                "body",
                &format!("malformed JSON: {err}"),
                "object {device_id} or {protocol, address, register?, slave?, timeout_ms?}",
            );
        }
    };
    let timeout_ms = match parse_probe_timeout(req.timeout_ms.as_ref()) {
        Ok(ms) => ms,
        Err(resp) => {
            remote_ops::runtime_for(&state).record_audit(
                &actor,
                OpsAction::DeviceTest,
                true,
                OUTCOME_BAD_REQUEST,
                "invalid timeout_ms",
            );
            return resp;
        }
    };

    // 目标解析：优先按 device_id 查既有配置，否则用请求体直连参数。
    let config = state.config();
    let (protocol, endpoint_raw, register_raw) = if !req.device_id.trim().is_empty() {
        let device_id = req.device_id.trim();
        let Some(point) = config.points.iter().find(|p| p.device_id == device_id) else {
            let registered = config.devices.iter().any(|d| d.device_id == device_id);
            if registered {
                // 仅登记设备：有协议无端点——诚实报无端点，不猜测地址。
                remote_ops::runtime_for(&state).record_audit(
                    &actor,
                    OpsAction::DeviceTest,
                    true,
                    OUTCOME_ACCEPTED,
                    &format!(
                        "device {device_id:?} is registered but has no points (no endpoint on file)"
                    ),
                );
                return probe_failure_response(
                    "no_endpoint",
                    &format!(
                        "device {device_id:?} is registered but has no point rows; add points first or probe with explicit protocol/address"
                    ),
                );
            }
            remote_ops::runtime_for(&state).record_audit(
                &actor,
                OpsAction::DeviceTest,
                true,
                OUTCOME_BAD_REQUEST,
                &format!("unknown device {device_id:?}"),
            );
            return writeapi::not_found(&format!("device {device_id:?} not found"));
        };
        // 首个可解析为 4xxxx/3xxxx 寄存器的 point_id（southbound 轮询同语义）。
        let register = config
            .points
            .iter()
            .filter(|p| p.device_id == device_id)
            .map(|p| p.point_id.clone())
            .find(|pid| {
                PointAddressParser::parse(pid)
                    .map(|a| a.area == Some('4') || a.area == Some('3'))
                    .unwrap_or(false)
            })
            .unwrap_or_else(|| req.register.clone());
        (point.protocol.clone(), point.address.clone(), register)
    } else {
        (
            req.protocol.trim().to_string(),
            req.address.trim().to_string(),
            req.register.trim().to_string(),
        )
    };

    if protocol != "modbus-tcp" && protocol != "modbus-rtu" {
        remote_ops::runtime_for(&state).record_audit(
            &actor,
            OpsAction::DeviceTest,
            true,
            OUTCOME_ACCEPTED,
            &format!("probe refused: unsupported protocol {protocol:?} (honest limitation)"),
        );
        return probe_failure_response(
            "unsupported_protocol",
            &format!(
                "only modbus-tcp / modbus-rtu (RTU-over-TCP) support structured connectivity probes; \
                 {protocol:?} has no probe driver yet (honest limitation, no fabricated results)"
            ),
        );
    }
    if endpoint_raw.is_empty() {
        return probe_failure_response(
            "no_endpoint",
            "no endpoint address available for the probe",
        );
    }
    let framing = if protocol == "modbus-tcp" {
        ModbusFraming::Tcp
    } else {
        ModbusFraming::Rtu
    };
    let Some(addr) = resolve_endpoint(&endpoint_raw, DEFAULT_MODBUS_PORT).await else {
        return probe_failure_response(
            "invalid_endpoint",
            &format!(
                "cannot resolve endpoint {endpoint_raw:?} (expected host:port or resolvable host)"
            ),
        );
    };
    let register = if register_raw.is_empty() {
        DEFAULT_PROBE_REGISTER.to_string()
    } else {
        register_raw
    };
    let Ok(register_addr) = PointAddressParser::parse(&register) else {
        return probe_failure_response(
            "invalid_endpoint",
            &format!(
                "probe register {register:?} is not a parseable point address (expected 4xxxx/3xxxx)"
            ),
        );
    };
    let slave = match req.slave.as_ref() {
        None if protocol == "modbus-tcp" => 0xFF,
        None => 1u8,
        Some(v) => match v {
            Value::Number(n) => n.as_u64().unwrap_or(0xFF) as u8,
            Value::String(s) => s.trim().parse().unwrap_or(0xFF),
            _ => 0xFF,
        },
    };

    // 全探测：建连 + 读 1 个保持寄存器。总预算覆盖「建连 + 超时 + 一次重连重试」。
    let started = Instant::now();
    let budget = Duration::from_millis(timeout_ms.saturating_mul(2) + 2_000);
    let mut driver = ModbusDriver::new(ModbusConfig {
        addr: addr.0,
        slave,
        framing,
        timeout: Duration::from_millis(timeout_ms),
        reconnector: crate::driver::Reconnector::default(),
    });
    let probe = tokio::time::timeout(budget, async {
        driver.connect().await?;
        driver
            .read(&[ReadPoint {
                address: register_addr,
                count: 1,
            }])
            .await
    })
    .await;
    let elapsed_ms = started.elapsed().as_millis().to_string();
    let (ok_flag, outcome) = match &probe {
        Err(_) => (
            false,
            probe_failure_with_elapsed(
                "timeout",
                &format!("probe to {endpoint_raw:?} exceeded budget after {elapsed_ms} ms"),
                elapsed_ms.clone(),
            ),
        ),
        Ok(Err(err)) => {
            let kind = match err {
                crate::error::DaemonError::NetworkError(_) => "unreachable",
                crate::error::DaemonError::ProtocolError(_) => "protocol_error",
                _ => "error",
            };
            (
                false,
                probe_failure_with_elapsed(kind, &err.to_string(), elapsed_ms.clone()),
            )
        }
        Ok(Ok(samples)) => {
            let bytes = samples.first().map(|s| s.value.len()).unwrap_or(0);
            (
                true,
                Json(json!({
                    "ok": true,
                    "protocol": protocol,
                    "endpoint": endpoint_raw,
                    "register": register,
                    "sample_bytes": bytes.to_string(),
                    "elapsed_ms": elapsed_ms.clone(),
                    "detail": "TCP connect + read 1 holding register succeeded",
                }))
                .into_response(),
            )
        }
    };
    remote_ops::runtime_for(&state).record_audit(
        &actor,
        OpsAction::DeviceTest,
        true,
        OUTCOME_ACCEPTED,
        &format!(
            "device test protocol={protocol} endpoint={endpoint_raw:?} register={register} elapsed_ms={elapsed_ms}: {}",
            if ok_flag { "probe ok" } else { "probe failed (structured)" }
        ),
    );
    outcome
}

/// 探测结构化失败（HTTP 200 + ok=false，绝不 500）。
fn probe_failure_response(error_kind: &str, reason: &str) -> Response {
    Json(json!({
        "ok": false,
        "error_kind": error_kind,
        "reason": reason,
    }))
    .into_response()
}

/// 探测结构化失败（含耗时；大数红线：elapsed_ms 字符串编码）。
fn probe_failure_with_elapsed(error_kind: &str, reason: &str, elapsed_ms: String) -> Response {
    Json(json!({
        "ok": false,
        "error_kind": error_kind,
        "reason": reason,
        "elapsed_ms": elapsed_ms,
    }))
    .into_response()
}

// ===========================================================================
// 北向出口（forwarders = outlets 的页面契约别名 + 探测）
// ===========================================================================

/// `GET /api/forwarders` → 北向出口列表（读，开放）。
///
/// 与 `/api/outlets` 同一数据源与字段，另加 `id` = 出口名（前端 ForwarderRecord
/// 的 id 锚点；出口在 config 中以 `name` 为唯一键）。
pub async fn forwarders_list(State(state): State<MgmtState>) -> Response {
    let rows: Vec<Value> = state
        .config()
        .outlets
        .iter()
        .map(|outlet| {
            let encoding = match outlet.encoding {
                crate::config::OutletEncoding::Protobuf => "protobuf",
                crate::config::OutletEncoding::Json => "json",
            };
            json!({
                "id": outlet.name,
                "name": outlet.name,
                "target": outlet.broker,
                "topic_prefix": outlet.topic_prefix,
                "qos": outlet.qos.to_string(),
                "tls": outlet.tls,
                "encoding": encoding,
            })
        })
        .collect();
    Json(Value::Array(rows)).into_response()
}

/// `POST /api/forwarders` → 出口登记（诚实 501：鉴权与审计管线就绪，
/// 写能力未落地——出口写涉及 TLS 证书字段校验，待北向配置写任务收口）。
pub async fn forwarder_create(
    State(state): State<MgmtState>,
    authed: AuthedRole,
    body: Bytes,
) -> Response {
    not_implemented_write(
        &state,
        authed,
        body.as_ref(),
        OpsAction::ForwarderCreate,
        "forwarder registration via mgmt API is not implemented yet",
        "北向出口登记写接口未落地（TLS 证书字段校验待收口）；鉴权与审计管线已就绪",
    )
    .await
}

/// `POST /api/forwarders/:id/test` → 出口 TCP 可达性探测（`id` = 出口名）。
///
/// **诚实限制**：仅 TCP 建连探测（`mqtts://` 不做 TLS 握手 / MQTT CONNACK），
/// 结果标注 `probe: "tcp_connect"`；未知出口 → 404；不可达 → 200 结构化失败。
pub async fn forwarder_test(
    State(state): State<MgmtState>,
    AxumPath(forwarder_id): AxumPath<String>,
    authed: AuthedRole,
    Query(params): Query<std::collections::HashMap<String, String>>,
) -> Response {
    if let Err(rejection) = authed.ensure(Permission::DeviceView) {
        remote_ops::runtime_for(&state).record_audit(
            &authed.claims.sub,
            OpsAction::ForwarderTest,
            false,
            OUTCOME_DENIED,
            &rejection.to_string(),
        );
        return rejection.into_response();
    }
    let actor = authed.claims.sub.clone();
    let timeout_ms = match params.get("timeout_ms").map(|v| v.trim().parse::<u64>()) {
        Some(Ok(ms)) => ms.clamp(MIN_PROBE_TIMEOUT_MS, MAX_PROBE_TIMEOUT_MS),
        Some(Err(_)) => {
            remote_ops::runtime_for(&state).record_audit(
                &actor,
                OpsAction::ForwarderTest,
                true,
                OUTCOME_BAD_REQUEST,
                "invalid timeout_ms query parameter",
            );
            return writeapi::validation_error(
                "timeout_ms",
                "not a valid u64",
                &format!("integer in [{MIN_PROBE_TIMEOUT_MS}, {MAX_PROBE_TIMEOUT_MS}] (ms)"),
            );
        }
        None => DEFAULT_PROBE_TIMEOUT_MS,
    };

    let outlet = state
        .config()
        .outlets
        .iter()
        .find(|o| o.name == forwarder_id)
        .map(|o| (o.broker.clone(), o.tls));
    let Some((broker, tls)) = outlet else {
        remote_ops::runtime_for(&state).record_audit(
            &actor,
            OpsAction::ForwarderTest,
            true,
            OUTCOME_BAD_REQUEST,
            &format!("unknown forwarder {forwarder_id:?}"),
        );
        return writeapi::not_found(&format!("forwarder {forwarder_id:?} not found"));
    };
    let Some((host, port)) = split_broker_url(&broker) else {
        return probe_failure_response(
            "invalid_target",
            &format!("broker url {broker:?} is not scheme://host[:port]"),
        );
    };
    let port = port.unwrap_or(if tls { 8883 } else { 1883 });
    let Some(addr) = resolve_endpoint(&host, port).await else {
        return probe_failure_response(
            "invalid_target",
            &format!("cannot resolve broker host {host:?}"),
        );
    };
    let started = Instant::now();
    let outcome = match tokio::time::timeout(
        Duration::from_millis(timeout_ms),
        TcpStream::connect(addr.0),
    )
    .await
    {
        Err(_) => probe_failure_with_elapsed(
            "timeout",
            &format!("tcp connect to {host}:{port} exceeded {timeout_ms} ms"),
            started.elapsed().as_millis().to_string(),
        ),
        Ok(Err(err)) => probe_failure_with_elapsed(
            "unreachable",
            &format!("tcp connect to {host}:{port} failed: {err}"),
            started.elapsed().as_millis().to_string(),
        ),
        Ok(Ok(_)) => Json(json!({
            "ok": true,
            "id": forwarder_id,
            "target": broker,
            "probe": "tcp_connect",
            "elapsed_ms": started.elapsed().as_millis().to_string(),
            "note": "TCP reachability only; TLS handshake / MQTT CONNACK are not probed",
        }))
        .into_response(),
    };
    remote_ops::runtime_for(&state).record_audit(
        &actor,
        OpsAction::ForwarderTest,
        true,
        OUTCOME_ACCEPTED,
        &format!(
            "forwarder test id={forwarder_id:?} target={broker:?} elapsed_ms={}: probe finished (structured)",
            started.elapsed().as_millis()
        ),
    );
    outcome
}

// ===========================================================================
// 诚实空态 / 占位端点（无真实数据源，绝不伪造数据）
// ===========================================================================

/// `GET /api/rules` → 转发规则（诚实空态：规则引擎未落地，无真实数据源）。
pub async fn rules_list() -> Response {
    Json(json!({
        "items": [],
        "total": "0",
        "source": "unsupported",
        "reason": "转发规则引擎未落地（数据面为固定管线），无真实数据源；返回诚实空态，不伪造规则",
    }))
    .into_response()
}

/// `GET /api/alerts` → 告警（诚实空态：告警引擎未落地）。
pub async fn alerts_list() -> Response {
    Json(json!({
        "items": [],
        "total": "0",
        "source": "unsupported",
        "reason": "告警引擎未落地（设备离线 / 北向中断等告警事件域尚未建模）；返回诚实空态，不伪造告警",
    }))
    .into_response()
}

/// `PUT /api/alerts/rules` → 告警规则写（诚实 501：鉴权就绪，能力未落地）。
pub async fn alerts_rules_put(
    State(state): State<MgmtState>,
    authed: AuthedRole,
    body: Bytes,
) -> Response {
    not_implemented_write(
        &state,
        authed,
        body.as_ref(),
        OpsAction::AlarmRulesWrite,
        "alarm rule configuration has no backing engine yet",
        "告警引擎未落地，无规则可写；鉴权与审计管线已就绪",
    )
    .await
}

/// 诚实 501 通用路径：鉴权（`device.write`，仅 system）→ 审计（not_implemented）
/// → 501 结构化响应（对齐 remote_ops::collectors 的「管线先于能力」模式）。
async fn not_implemented_write(
    state: &MgmtState,
    authed: AuthedRole,
    body: &[u8],
    action: OpsAction,
    planned: &str,
    reason_cn: &str,
) -> Response {
    if let Err(rejection) = authed.ensure(Permission::DeviceWrite) {
        writeapi::audit(
            state,
            &authed.claims.sub,
            action,
            false,
            OUTCOME_DENIED,
            &rejection.to_string(),
        );
        return rejection.into_response();
    }
    let actor = authed.claims.sub.clone();
    // body 仅做 JSON 结构探测（内容不参与判定；能力未落地无从消费字段）。
    let body_ok = body.is_empty()
        || serde_json::from_slice::<Value>(body)
            .map(|v| v.is_object())
            .unwrap_or(false);
    if !body_ok {
        writeapi::audit(
            state,
            &actor,
            action,
            true,
            OUTCOME_BAD_REQUEST,
            "malformed json body",
        );
        return writeapi::validation_error("body", "malformed JSON", "object (fields reserved)");
    }
    writeapi::audit(
        state,
        &actor,
        action,
        true,
        OUTCOME_NOT_IMPLEMENTED,
        planned,
    );
    (
        StatusCode::NOT_IMPLEMENTED,
        Json(json!({
            "error": "not_implemented",
            "planned": planned,
            "reason": reason_cn,
        })),
    )
        .into_response()
}

// ===========================================================================
// 授权状态
// ===========================================================================

/// `GET /api/license/status` → 授权状态快照（读，开放；来自 `LicenseRuntime` watch）。
///
/// 大数红线：`valid_until` / `remaining_secs` / `remaining_days` 一律字符串。
/// `lease.raw`（签名租约原文）**绝不回显**。runtime 未装配时诚实返回
/// `unlicensed`（fail-closed 展示语义）。
pub async fn license_status(State(state): State<MgmtState>) -> Response {
    let Some(runtime) = state.daemon().license_runtime() else {
        return Json(json!({
            "status": "unlicensed",
            "tier": "",
            "north_forward_allowed": false,
            "note": "license runtime not assembled (fail-closed view)",
        }))
        .into_response();
    };
    let now = crate::mgmt::auth_jwt::now_unix_secs();
    let north_allowed = runtime.north_forward_allowed();
    let mut body = match runtime.state() {
        LicenseState::Unlicensed => json!({
            "status": "unlicensed",
            "tier": "",
        }),
        LicenseState::Trial { days_left } => json!({
            "status": "trial",
            "tier": "trial",
            "remaining_days": days_left.to_string(),
        }),
        LicenseState::Licensed { lease } => json!({
            "status": "active",
            "tier": lease.tier,
            "valid_until": lease.valid_until.to_string(),
            "remaining_secs": lease.remaining_secs(now).to_string(),
            "remaining_days": (lease.remaining_secs(now) / 86_400).to_string(),
        }),
        LicenseState::Grace { lease, days_left } => json!({
            "status": "grace",
            "tier": lease.tier,
            "valid_until": lease.valid_until.to_string(),
            "remaining_secs": lease.remaining_secs(now).to_string(),
            "remaining_days": days_left.to_string(),
        }),
        LicenseState::Degraded { reason } => json!({
            "status": "degraded",
            "tier": "free",
            "degrade_reason": reason,
        }),
    };
    body["north_forward_allowed"] = json!(north_allowed);
    Json(body).into_response()
}

// ===========================================================================
// 测试（沿用 writeapi.rs 测试模式：spawn_server + 手写 HTTP）
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bootstrap::DaemonShared;
    use crate::config::ConfigShared;
    use crate::mgmt::auth_jwt::{now_unix_secs, sign, Claims};
    use crate::mgmt::rbac::Role;
    use crate::mgmt::remote_ops::runtime_for;
    use std::path::PathBuf;
    use std::sync::Arc;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::{TcpListener, TcpStream};

    /// 测试种子配置：登记空设备 + 点位设备（端点式地址，southbound 同语义）+ 1 出口。
    fn seed_toml(broker_port: u16) -> String {
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
    fn make_state(dir: &tempfile::TempDir, broker_port: u16) -> (MgmtState, PathBuf) {
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
    fn token_for(state: &MgmtState, role: Role) -> String {
        let now = now_unix_secs();
        let claims = Claims {
            sub: "ops-admin".to_string(),
            role,
            exp: now + 600,
            iat: now,
            nbf: None,
            jti: "test-jti-pages".to_string(),
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

    /// 手写 HTTP 请求（10s 超时——探测类端点含真实网络 IO，预算放宽）。
    async fn http_request(
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

    async fn http_get(port: u16, path: &str) -> (u16, String, String) {
        http_request(port, "GET", path, None, None, "text/plain").await
    }

    async fn post_json(port: u16, path: &str, body: &str, token: &str) -> (u16, String, String) {
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

    async fn post_csv(port: u16, path: &str, body: &str, token: &str) -> (u16, String, String) {
        http_request(port, "POST", path, Some(body), Some(token), "text/csv").await
    }

    async fn put_json(port: u16, path: &str, body: &str, token: &str) -> (u16, String, String) {
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

    /// 从落盘文件重读配置（往返断言用）。
    fn load_config(path: &std::path::Path) -> GatewayConfig {
        GatewayConfig::load(path).expect("reload saved config")
    }

    /// 最小 Modbus TCP 应答器：收 FC03 读请求 → 回 1 个寄存器（值 0x0102）。
    async fn spawn_modbus_stub() -> u16 {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let port = listener.local_addr().expect("addr").port();
        tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    break;
                };
                tokio::spawn(async move {
                    let mut head = [0u8; 12]; // MBAP(7) + FC03 请求(5)
                    if sock.read_exact(&mut head).await.is_err() {
                        return;
                    }
                    let resp: [u8; 11] = [
                        head[0], head[1], 0x00, 0x00, 0x00, 0x05, head[6], 0x03, 0x02, 0x01, 0x02,
                    ];
                    let _ = sock.write_all(&resp).await;
                    let _ = sock.flush().await;
                });
            }
        });
        port
    }

    // ---- 点表导出 / 导入 ----

    /// QA Happy: 导出即模板——CSV 表头 + 种子行逐字段一致 + text/csv + 过滤。
    #[tokio::test]
    async fn points_export_csv_matches_seed() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (state, _path) = make_state(&dir, 1);
        let port = spawn_server(state).await;

        let (status, headers, body) = http_get(port, "/api/points/export").await;
        assert_eq!(status, 200, "{body}");
        assert!(headers.contains("text/csv"), "headers: {headers}");
        let mut lines = body.lines();
        assert_eq!(lines.next().expect("header"), POINTS_CSV_HEADER);
        let row1 = lines.next().expect("row 1");
        assert_eq!(row1, "dev-01,40001,modbus-tcp,192.168.1.10:502,100");
        let row2 = lines.next().expect("row 2");
        assert_eq!(row2, "dev-01,40003,modbus-tcp,192.168.1.10:502,500");

        // device_id 过滤。
        let (status, _, body) = http_get(port, "/api/points/export?device_id=dev-01").await;
        assert_eq!(status, 200);
        assert_eq!(body.lines().count(), 3, "header + 2 rows: {body}");
        let (status, _, body) = http_get(port, "/api/points/export?device_id=dev-empty").await;
        assert_eq!(status, 200);
        assert_eq!(
            body.lines().count(),
            1,
            "no rows for pointless device: {body}"
        );
    }

    /// QA Happy: 导出 → 导入往返（硬契约「导出即可当导入模板」）——
    /// 寄存器式 point_id + 端点式 address 双形态可往返，落盘 + 读接口即时可见。
    #[tokio::test]
    async fn export_then_import_roundtrip() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (state, path) = make_state(&dir, 1);
        let token = token_for(&state, Role::System);
        let port = spawn_server(state.clone()).await;

        // ① 从 dev-01 导出。
        let (_, _, csv) = http_get(port, "/api/points/export?device_id=dev-01").await;

        // ② 原样导入到 dev-empty（replace=true 覆盖式）。
        let (status, _, body) = post_csv(
            port,
            "/api/points/import?device_id=dev-empty&replace=true",
            &csv,
            &token,
        )
        .await;
        assert_eq!(status, 200, "{body}");
        let value: Value = serde_json::from_str(&body).expect("json");
        assert_eq!(value["accepted"], true);
        assert_eq!(value["imported"], "2");
        assert_eq!(value["replaced"], "0");
        assert!(value["config_version"].is_string(), "大数红线: {value}");

        // ③ 落盘往返 + 读接口立即可见。
        let config = load_config(&path);
        let imported: Vec<_> = config
            .points
            .iter()
            .filter(|p| p.device_id == "dev-empty")
            .collect();
        assert_eq!(imported.len(), 2, "roundtrip rows persisted");
        assert_eq!(imported[0].point_id, "40001");
        assert_eq!(imported[0].address, "192.168.1.10:502");

        let (status, _, body) = http_get(port, "/api/points?device_id=dev-empty").await;
        assert_eq!(status, 200);
        let points: Vec<Value> = serde_json::from_str(&body).expect("array");
        assert_eq!(points.len(), 2, "{body}");
    }

    /// QA 红线: 坏行必须「行号 + 原因 + 允许值」——未知协议（行 3）、
    /// 频率越下限（行 4）、重复点位（行 5）逐条入 errors；**整批拒绝零落盘**。
    #[tokio::test]
    async fn import_bad_rows_report_line_reason_allowed() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (state, path) = make_state(&dir, 1);
        let token = token_for(&state, Role::System);
        let port = spawn_server(state.clone()).await;

        let csv = concat!(
            "device_id,point_id,protocol,address,frequency_ms\n",
            "dev-empty,p_ok,modbus-tcp,40005,200\n",
            "dev-empty,p_bad,magic-bus,40005,200\n",
            "dev-empty,p_fast,modbus-tcp,40006,10\n",
            "dev-01,40001,modbus-tcp,40007,200\n",
        );
        let (status, _, body) = post_csv(port, "/api/points/import", csv, &token).await;
        assert_eq!(status, 400, "{body}");
        let value: Value = serde_json::from_str(&body).expect("json");
        assert_eq!(value["error"], "validation_failed");
        let errors = value["errors"].as_array().expect("errors array");
        // 行 3：未知协议（行号 = 文件物理行号，表头为行 1）。
        let line3 = errors
            .iter()
            .find(|e| e["line"] == "3")
            .expect("line 3 error");
        assert!(
            line3["reason"]
                .as_str()
                .expect("reason")
                .contains("magic-bus"),
            "{line3}"
        );
        assert!(
            line3["allowed"]
                .as_str()
                .expect("allowed")
                .contains("modbus-tcp"),
            "allowed must enumerate protocols: {line3}"
        );
        // 行 4：频率越下限。
        let line4 = errors
            .iter()
            .find(|e| e["line"] == "4")
            .expect("line 4 error");
        assert!(
            line4["allowed"].as_str().expect("allowed").contains("100"),
            "{line4}"
        );
        // 行 5：与既有点位重复。
        let line5 = errors
            .iter()
            .find(|e| e["line"] == "5")
            .expect("line 5 error");
        assert!(
            line5["reason"]
                .as_str()
                .expect("reason")
                .contains("already exists"),
            "{line5}"
        );
        assert_eq!(errors.len(), 3, "exactly the three bad rows: {value}");

        // 整批拒绝：零落盘。
        assert_eq!(load_config(&path).points.len(), 2, "seed points untouched");
    }

    /// QA: 表头缺列（行 1）与空 body → 400。
    #[tokio::test]
    async fn import_header_and_body_validation() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (state, _path) = make_state(&dir, 1);
        let token = token_for(&state, Role::System);
        let port = spawn_server(state.clone()).await;

        // 缺列表头 → 行 1 报错。
        let (status, _, body) = post_csv(
            port,
            "/api/points/import",
            "device_id,point_id\ndev-empty,p1,modbus-tcp",
            &token,
        )
        .await;
        assert_eq!(status, 400, "{body}");
        let value: Value = serde_json::from_str(&body).expect("json");
        let errors = value["errors"].as_array().expect("errors");
        assert!(errors.iter().any(|e| e["line"] == "1"), "{value}");

        // 空 body → 400。
        let (status, _, _) = post_csv(port, "/api/points/import", "", &token).await;
        assert_eq!(status, 400);
    }

    /// QA: JSON 形态导入（{"csv": ...} + device_id 字段）；空 frequency_ms
    /// 列 → 缺省 1000 落盘。
    #[tokio::test]
    async fn import_json_body_default_frequency() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (state, path) = make_state(&dir, 1);
        let token = token_for(&state, Role::System);
        let port = spawn_server(state.clone()).await;

        let json_body = serde_json::json!({
            "device_id": "dev-empty",
            "csv": "device_id,point_id,protocol,address,frequency_ms\n,pt-json,modbus-tcp,40009,\n"
        })
        .to_string();
        let (status, _, body) = post_json(port, "/api/points/import", &json_body, &token).await;
        assert_eq!(status, 200, "{body}");
        let value: Value = serde_json::from_str(&body).expect("json");
        assert_eq!(value["imported"], "1");
        let row = load_config(&path)
            .points
            .into_iter()
            .find(|p| p.point_id == "pt-json")
            .expect("row persisted");
        assert_eq!(row.frequency_ms, 1_000, "empty frequency_ms → default");
        assert_eq!(row.address, "40009");
    }

    /// QA: replace=true 覆盖导入——先删目标设备既有点位（replaced 计数），再写入；
    /// replace=true 缺设备 → 400。
    #[tokio::test]
    async fn import_replace_mode_deletes_then_writes() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (state, path) = make_state(&dir, 1);
        let token = token_for(&state, Role::System);
        let port = spawn_server(state.clone()).await;

        let csv = concat!(
            "device_id,point_id,protocol,address,frequency_ms\n",
            "dev-01,new-point,modbus-tcp,40010,250\n",
        );
        let (status, _, body) = post_csv(
            port,
            "/api/points/import?device_id=dev-01&replace=true",
            csv,
            &token,
        )
        .await;
        assert_eq!(status, 200, "{body}");
        let value: Value = serde_json::from_str(&body).expect("json");
        assert_eq!(value["imported"], "1");
        assert_eq!(value["replaced"], "2", "seed dev-01 had two points");

        let config = load_config(&path);
        let dev01: Vec<_> = config
            .points
            .iter()
            .filter(|p| p.device_id == "dev-01")
            .collect();
        assert_eq!(dev01.len(), 1);
        assert_eq!(dev01[0].point_id, "new-point");
        assert_eq!(dev01[0].frequency_ms, 250);

        // replace=true 缺设备 → 400。
        let (status, _, body) =
            post_csv(port, "/api/points/import?replace=true", csv, &token).await;
        assert_eq!(status, 400, "{body}");
    }

    /// QA RBAC: 无 token → 401；ops（不持 point.write）→ 403 + 审计；
    /// 全部拒绝路径零落盘。
    #[tokio::test]
    async fn import_auth_gates() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (state, path) = make_state(&dir, 1);
        let ops = token_for(&state, Role::Ops);
        let port = spawn_server(state.clone()).await;

        let csv = format!("{}\ndev-empty,p1,modbus-tcp,40009,200\n", POINTS_CSV_HEADER);
        let (status, _, _) = post_csv(port, "/api/points/import", &csv, "").await;
        assert_eq!(status, 401, "missing token must be rejected");

        let (status, _, body) = post_csv(port, "/api/points/import", &csv, &ops).await;
        assert_eq!(status, 403, "{body}");
        let value: Value = serde_json::from_str(&body).expect("json");
        assert_eq!(value["error"], "forbidden");

        assert_eq!(load_config(&path).points.len(), 2, "nothing persisted");
        let denied: Vec<_> = runtime_for(&state)
            .audit_snapshot()
            .into_iter()
            .filter(|e| !e.allowed && e.outcome == OUTCOME_DENIED)
            .collect();
        assert!(
            denied.iter().all(|e| e.action == "point_import"),
            "{denied:?}"
        );
    }

    // ---- 配置回滚 ----

    /// QA Happy: 写操作产生备份 → 回滚 → 配置往返恢复 + config_version 字符串 +
    /// 读接口反映恢复结果 + config_reloaded 事件入历史环。
    #[tokio::test]
    async fn rollback_restores_pre_write_snapshot() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (state, path) = make_state(&dir, 1);
        let token = token_for(&state, Role::System);
        let port = spawn_server(state.clone()).await;

        // ① 写操作（新增设备）→ 产生写前备份。
        let (status, _, body) = post_json(
            port,
            "/api/devices",
            r#"{"id":"dev-9","protocol":"s7"}"#,
            &token,
        )
        .await;
        assert_eq!(status, 200, "{body}");
        assert_eq!(load_config(&path).devices.len(), 2, "dev-9 written");

        // ② 回滚（缺省取最新备份）。
        let (status, _, body) = post_json(
            port,
            "/api/settings/rollback",
            r#"{"reason":"误操作回退"}"#,
            &token,
        )
        .await;
        assert_eq!(status, 200, "{body}");
        let value: Value = serde_json::from_str(&body).expect("json");
        assert_eq!(value["accepted"], true);
        let restored_from = value["restored_from"].as_str().expect("restored_from");
        assert!(
            restored_from.starts_with("config.toml.bak-"),
            "backup name: {restored_from}"
        );
        assert!(value["config_version"].is_string(), "大数红线: {value}");

        // ③ 落盘往返：dev-9 消失，种子设备回归。
        let config = load_config(&path);
        assert!(
            !config.devices.iter().any(|d| d.device_id == "dev-9"),
            "dev-9 rolled back"
        );
        assert!(
            config.devices.iter().any(|d| d.device_id == "dev-empty"),
            "seed device restored"
        );

        // ④ 读接口即时反映。
        let (status, _, body) = http_get(port, "/api/devices").await;
        assert_eq!(status, 200);
        let devices: Vec<Value> = serde_json::from_str(&body).expect("array");
        assert!(
            devices.iter().all(|d| d["id"] != "dev-9"),
            "read side reflects rollback: {body}"
        );

        // ⑤ config_reloaded 事件进入历史环（SSE 三源合流语义）。
        let reloaded: Vec<_> = state
            .history_since(0)
            .into_iter()
            .filter(|e| e.event.type_name() == "config_reloaded")
            .collect();
        assert!(!reloaded.is_empty(), "config_reloaded event published");
    }

    /// QA: 显式 backup 名回滚 + 路径穿越拒绝 + 无备份 404 + 未知备份 404。
    #[tokio::test]
    async fn rollback_backup_selection_and_404() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (state, path) = make_state(&dir, 1);
        let token = token_for(&state, Role::System);
        let port = spawn_server(state.clone()).await;

        // 无写操作 → 无备份 → 404。
        let (status, _, body) = post_json(port, "/api/settings/rollback", "{}", &token).await;
        assert_eq!(status, 404, "{body}");
        let value: Value = serde_json::from_str(&body).expect("json");
        assert_eq!(value["error"], "no_backup");

        // 制造一次写 → 得到备份名。
        let (status, _, _) = post_json(
            port,
            "/api/devices",
            r#"{"id":"dev-9","protocol":"s7"}"#,
            &token,
        )
        .await;
        assert_eq!(status, 200);
        let backup_name = std::fs::read_dir(dir.path())
            .expect("read_dir")
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().to_string())
            .find(|n| n.starts_with("config.toml.bak-") && !n.contains(".tmp"))
            .expect("backup exists");

        // 显式名回滚成功。
        let (status, _, body) = post_json(
            port,
            "/api/settings/rollback",
            &format!(r#"{{"backup":"{backup_name}"}}"#),
            &token,
        )
        .await;
        assert_eq!(status, 200, "{body}");

        // 路径穿越名 → 400。
        let (status, _, _) = post_json(
            port,
            "/api/settings/rollback",
            r#"{"backup":"../config.toml.bak-x"}"#,
            &token,
        )
        .await;
        assert_eq!(status, 400);

        // 不存在的备份名 → 404；配置文件本体完好。
        assert!(path.is_file(), "config file intact");
        let (status, _, _) = post_json(
            port,
            "/api/settings/rollback",
            r#"{"backup":"config.toml.bak-9999999999"}"#,
            &token,
        )
        .await;
        assert_eq!(status, 404);
    }

    /// QA RBAC: 回滚高危动作——无 token 401；ops 403 + 审计；配置零改动。
    #[tokio::test]
    async fn rollback_auth_gates() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (state, path) = make_state(&dir, 1);
        let ops = token_for(&state, Role::Ops);
        let port = spawn_server(state.clone()).await;

        let (status, _, _) = http_request(
            port,
            "POST",
            "/api/settings/rollback",
            Some("{}"),
            None,
            "application/json",
        )
        .await;
        assert_eq!(status, 401);

        let (status, _, body) = post_json(port, "/api/settings/rollback", "{}", &ops).await;
        assert_eq!(status, 403, "{body}");

        // 零改动（无备份产生、设备数不变）。
        assert_eq!(load_config(&path).devices.len(), 1);
        let denied: Vec<_> = runtime_for(&state)
            .audit_snapshot()
            .into_iter()
            .filter(|e| !e.allowed)
            .collect();
        assert!(
            denied.iter().all(|e| e.action == "settings_rollback"),
            "{denied:?}"
        );
    }

    // ---- 设备连通性探测 ----

    /// QA Happy: modbus-tcp 全探测——假应答器建连 + 读 1 寄存器成功，
    /// elapsed_ms / sample_bytes 字符串编码（大数红线）。
    #[tokio::test]
    async fn device_test_modbus_happy_path() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (state, _path) = make_state(&dir, 1);
        let token = token_for(&state, Role::System);
        let port = spawn_server(state.clone()).await;
        let stub_port = spawn_modbus_stub().await;

        let (status, _, body) = post_json(
            port,
            "/api/devices/test",
            &format!(
                r#"{{"protocol":"modbus-tcp","address":"127.0.0.1:{stub_port}","register":"40001","timeout_ms":1000}}"#
            ),
            &token,
        )
        .await;
        assert_eq!(status, 200, "{body}");
        let value: Value = serde_json::from_str(&body).expect("json");
        assert_eq!(value["ok"], true, "{value}");
        assert_eq!(value["protocol"], "modbus-tcp");
        assert_eq!(value["sample_bytes"], "2");
        assert!(value["elapsed_ms"].is_string(), "大数红线: {value}");

        // 审计环有 device_test 记录。
        assert!(runtime_for(&state)
            .audit_snapshot()
            .iter()
            .any(|e| e.action == "device_test" && e.allowed));
    }

    /// QA: 按 device_id 探测（复用配置中的端点与首个可解析寄存器）——
    /// 种子设备指向不可达端点 → 结构化失败（HTTP 200，非 500）。
    #[tokio::test]
    async fn device_test_by_device_id_unreachable_is_structured() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (state, _path) = make_state(&dir, 1);
        let token = token_for(&state, Role::System);
        let port = spawn_server(state.clone()).await;

        let (status, _, body) = post_json(
            port,
            "/api/devices/test",
            r#"{"device_id":"dev-01","timeout_ms":300}"#,
            &token,
        )
        .await;
        assert_eq!(status, 200, "{body}");
        let value: Value = serde_json::from_str(&body).expect("json");
        assert_eq!(value["ok"], false, "{value}");
        assert!(
            ["unreachable", "timeout", "protocol_error", "error"]
                .contains(&value["error_kind"].as_str().expect("kind")),
            "structured failure kind: {value}"
        );
        assert!(
            value["reason"]
                .as_str()
                .expect("reason")
                .contains("192.168.1.10"),
            "reason carries the resolved endpoint: {value}"
        );

        // 未知设备 → 404；仅登记设备（无点位）→ no_endpoint 结构化失败。
        let (status, _, _) =
            post_json(port, "/api/devices/test", r#"{"device_id":"nope"}"#, &token).await;
        assert_eq!(status, 404);
        let (status, _, body) = post_json(
            port,
            "/api/devices/test",
            r#"{"device_id":"dev-empty"}"#,
            &token,
        )
        .await;
        assert_eq!(status, 200, "{body}");
        let value: Value = serde_json::from_str(&body).expect("json");
        assert_eq!(value["error_kind"], "no_endpoint", "{value}");

        // 非 modbus 协议（无结构化探测器）→ 不支持协议的结构化失败（非 500）。
        let (status, _, body) = post_json(
            port,
            "/api/devices/test",
            r#"{"protocol":"s7","address":"DB1.DBX0.0"}"#,
            &token,
        )
        .await;
        assert_eq!(status, 200, "{body}");
        let value: Value = serde_json::from_str(&body).expect("json");
        assert_eq!(value["error_kind"], "unsupported_protocol", "{value}");
    }

    /// QA: 探测鉴权（无 token 401）+ 异常 body 400 + timeout_ms 字符串形态钳制。
    #[tokio::test]
    async fn device_test_auth_and_validation() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (state, _path) = make_state(&dir, 1);
        let port = spawn_server(state.clone()).await;

        let (status, _, _) = http_request(
            port,
            "POST",
            "/api/devices/test",
            Some(r#"{"protocol":"modbus-tcp","address":"127.0.0.1:1"}"#),
            None,
            "application/json",
        )
        .await;
        assert_eq!(status, 401);

        let token = token_for(&state, Role::System);
        let (status, _, body) = post_json(port, "/api/devices/test", "not-json", &token).await;
        assert_eq!(status, 400, "{body}");

        // timeout_ms 越界钳制不报错（number/string 双形态，大数红线）。
        let stub_port = spawn_modbus_stub().await;
        let (status, _, body) = post_json(
            port,
            "/api/devices/test",
            &format!(
                r#"{{"protocol":"modbus-tcp","address":"127.0.0.1:{stub_port}","timeout_ms":"999999"}}"#
            ),
            &token,
        )
        .await;
        assert_eq!(status, 200, "{body}");
    }

    // ---- 北向出口 ----

    /// QA: GET /api/forwarders = outlets 数据源 + id 锚点；出口探测命中/未知/401；
    /// POST /api/forwarders → 诚实 501。
    #[tokio::test]
    async fn forwarders_list_and_tcp_probe() {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let live_port = listener.local_addr().expect("addr").port();
        tokio::spawn(async move { while let Ok((_sock, _)) = listener.accept().await {} });

        let dir = tempfile::tempdir().expect("tempdir");
        let (state, _path) = make_state(&dir, live_port);
        let token = token_for(&state, Role::System);
        let port = spawn_server(state.clone()).await;

        // 列表：id = 出口名。
        let (status, _, body) = http_get(port, "/api/forwarders").await;
        assert_eq!(status, 200);
        let rows: Vec<Value> = serde_json::from_str(&body).expect("array");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["id"], "north-1");
        assert_eq!(rows[0]["encoding"], "json");

        // 探测：命中（TCP 监听中）。
        let (status, _, body) = post_json(port, "/api/forwarders/north-1/test", "{}", &token).await;
        assert_eq!(status, 200, "{body}");
        let value: Value = serde_json::from_str(&body).expect("json");
        assert_eq!(value["ok"], true, "{value}");
        assert_eq!(value["probe"], "tcp_connect");

        // 探测：未知出口 → 404。
        let (status, _, _) = post_json(port, "/api/forwarders/nope/test", "{}", &token).await;
        assert_eq!(status, 404);

        // 探测：无 token → 401。
        let (status, _, _) = http_request(
            port,
            "POST",
            "/api/forwarders/north-1/test",
            Some("{}"),
            None,
            "application/json",
        )
        .await;
        assert_eq!(status, 401);

        // POST /api/forwarders → 诚实 501（写能力未落地）。
        let (status, _, body) = post_json(
            port,
            "/api/forwarders",
            r#"{"name":"x","broker":"mqtt://127.0.0.1:1883"}"#,
            &token,
        )
        .await;
        assert_eq!(status, 501, "{body}");
        let value: Value = serde_json::from_str(&body).expect("json");
        assert_eq!(value["error"], "not_implemented");
    }

    // ---- 诚实空态 / 占位 / 授权状态 / logs 别名 ----

    /// QA: 规则与告警为诚实空态（不伪造数据）；告警规则写 → 501（system）/ 403（ops）；
    /// 授权状态可读；/api/logs 别名自守卫。
    #[tokio::test]
    async fn honest_placeholders_and_license_status() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (state, _path) = make_state(&dir, 1);
        let system = token_for(&state, Role::System);
        let port = spawn_server(state.clone()).await;

        // GET /api/rules：items 空 + source=unsupported。
        let (status, _, body) = http_get(port, "/api/rules").await;
        assert_eq!(status, 200);
        let value: Value = serde_json::from_str(&body).expect("json");
        assert_eq!(value["items"].as_array().expect("items").len(), 0);
        assert_eq!(value["source"], "unsupported");

        // GET /api/alerts：同上。
        let (status, _, body) = http_get(port, "/api/alerts").await;
        assert_eq!(status, 200);
        let value: Value = serde_json::from_str(&body).expect("json");
        assert_eq!(value["source"], "unsupported");

        // PUT /api/alerts/rules：system → 501（鉴权通过、能力未落地）。
        let (status, _, body) =
            put_json(port, "/api/alerts/rules", r#"{"rules":[]}"#, &system).await;
        assert_eq!(status, 501, "{body}");
        let value: Value = serde_json::from_str(&body).expect("json");
        assert_eq!(value["error"], "not_implemented");

        // PUT /api/alerts/rules：ops → 403。
        let ops = token_for(&state, Role::Ops);
        let (status, _, _) = put_json(port, "/api/alerts/rules", r#"{"rules":[]}"#, &ops).await;
        assert_eq!(status, 403);

        // GET /api/license/status：runtime 未装配 → 诚实 unlicensed。
        let (status, _, body) = http_get(port, "/api/license/status").await;
        assert_eq!(status, 200);
        let value: Value = serde_json::from_str(&body).expect("json");
        assert_eq!(value["status"], "unlicensed");
        assert_eq!(value["north_forward_allowed"], false);

        // GET /api/logs：别名路由自守卫（无 token → 401；带 system token → 200）。
        let (status, _, _) = http_get(port, "/api/logs").await;
        assert_eq!(status, 401);
        let (status, _, body) = http_request(
            port,
            "GET",
            "/api/logs?actor=ops-admin",
            None,
            Some(&system),
            "text/plain",
        )
        .await;
        assert_eq!(status, 200, "{body}");
    }
}

//! 北向载荷编码器（计划 task 62）：protobuf（默认）/ JSON 双编码，每路出口独立可选。
//!
//! ## 职责边界
//! - **做**：[`TelemetryBatch`] 与单点 [`DataPoint`] 的 protobuf / JSON 序列化与反序列化；
//!   MQTT5 属性声明（Payload Format Indicator / Content Type）；MQTT 3.1.1 降级约定；
//!   `encoding` 字面量解析（非法值在配置加载期报错）。
//! - **不做**：单位换算 / 死区过滤（= task 15 / 37）、转发规则与触发时机（= task 20/37）、
//!   离线缓存与补发（= task 17/18）、签名（= task 21，签名对象是 [`crate::auth::signing::semantic_hash`]，
//!   与本模块的字节编码**完全解耦**）。
//!
//! ## 关键设计决策
//!
//! 1. **单一数据源**：编码枚举只有 [`Encoding`] 一套，定义在 task 19 的 `north::mqtt`，
//!    本模块只做再导出，禁止另建枚举；`parse_encoding` 直接委派 [`Encoding::parse`]，
//!    非法字面量 → `ConfigError`（码 2000），**不静默回退默认值**。
//! 2. **编码与签名解耦**：签名对象是业务语义哈希（字段排序后逐字段哈希），不含任何
//!    protobuf / JSON 字节序信息。因此「同一份数据两种编码 → 解码 → `semantic_hash`」
//!    必须**逐字节相等**，验签结果也必须相同（见单测
//!    `dual_encoding_semantic_hash_identical_and_signature_verifies`）。
//! 3. **JSON 大整数走字符串**（红线）：JSON number 是 IEEE754 double，
//!    超出 2^53−1 的整数会静默丢精度。规则（[`JSON_SAFE_INTEGER_MAX`]）：
//!    `|v| <= 9_007_199_254_740_991` → JSON number（无损）；超出 → JSON **字符串**。
//!    解码侧两种形态都接受，解码后一律是 `i64`，两条路径语义一致。
//! 4. **bytes 带类型前缀**：`value` / `sig` 输出为 `{"t": <类型>, "b64": <标准 base64>}`，
//!    其中 `t` ∈ `"f64le"`（8 字节小端 IEEE754 双精度，`sample_to_data_point` 产出的数值载荷）
//!    或 `"blob"`（其余长度，未知类型的原始字节）。类型前缀保证接收侧**不会**把 blob
//!    误当作双精度解析。解码以 `b64` 为准，`t` 为提示。
//! 5. **非有限浮点为 `null`**（红线）：`NaN` / `+Infinity` / `-Infinity` 在 JSON 中非法。
//!    当 `value` 恰为这三个值的**规范位模式**（小端 `7ff8000000000000` /
//!    `7ff0000000000000` / `fff0000000000000`）时，输出 `"value": null` 并附带
//!    `"value_f64": "NaN" | "Infinity" | "-Infinity"` 标记，解码可精确还原位模式
//!    （因此语义哈希不变）。**只有这三个规范常量会被判为无效浮点**——
//!    uint64 计数器等整数载荷（哪怕位模式形如 NaN，例如 `u64::MAX` / `i64::MAX`）
//!    一律按 blob 走 base64，绝不丢精度、绝不被误判。
//! 6. **值无效 → quality 降级在两条路径同一处做**：[`sample_to_data_point`] 在
//!    `!value.is_finite()` 时把 quality 强制置为 `BAD`（proto 与 JSON 共用同一函数），
//!    因此不会出现「JSON 改了 quality、protobuf 没改」导致的语义哈希漂移。
//! 7. **确定性**：`编码 → 解码 → 再编码` 两次字节流必须相等（签名解耦的前提）。
//!    JSON 对象字段固定齐全输出（`auth` 缺失时显式 `null`），`value_f64` 仅随 `null` 出现。
//! 8. **零 panic**：所有可失败点（protobuf decode / JSON 解析 / base64 / 整数解析）
//!    收敛为 [`DaemonError::ProtocolError`]（码 1000，北向载荷编解码域），
//!    入参非法（encoding 字面量 / `enc` 字段不匹配）为 `ConfigError`（码 2000）。
//!
//! ## JSON 结构约定（字段名与 protobuf 一一映射，snake_case）
//!
//! ```json
//! {
//!   "enc": "json",
//!   "points": [
//!     {
//!       "device_id": "pump-01",
//!       "point_id": "inlet_temp",
//!       "value": { "t": "f64le", "b64": "AAAAAABCPAA=" },
//!       "unit": "degC",
//!       "ts": "1762999999000000001",
//!       "quality": "GOOD",
//!       "quality_code": 1
//!     }
//!   ],
//!   "ts": 1762999999500000000,
//!   "gateway_id": "gw-001",
//!   "auth": { "mid": "...", "nonce": "...", "ts": "1763000000000000000",
//!             "sig": { "t": "blob", "b64": "..." } }
//! }
//! ```
//!
//! - **大整数**：所有 `int64`（`ts`、`auth.ts`、`quality_code`）走
//!   规则 3（≤2^53−1 用 number，超出用**字符串**）；纳秒时间戳恒为 19 位，恒走字符串。
//! - **bytes**：`value` / `sig` 为 `{"t": ..., "b64": ...}`（规则 4）；非有限浮点为
//!   `null` + `value_f64` 标记（规则 5）。
//! - **quality**：`"GOOD"` / `"UNCERTAIN"` / `"BAD"` / `"SIMULATED"`（与
//!   `Quality::as_str_name()` 一致，未知码值为 `"QUALITY_INVALID"`），
//!   并同时输出 `quality_code`（**权威值**，保证任意 i32 码值精确往返）。
//! - **数值精度**：`value` 是原始字节，JSON 路径不引入任何浮点十进制转换，
//!   因此不存在 double 舍入。
//!
//! ## MQTT 属性与降级
//!
//! - **MQTT 5**：`Payload Format Indicator` = [`payload_format_indicator`]
//!   （protobuf → 0 二进制，JSON → 1 UTF-8）；`Content Type` = [`content_type`]
//!   （`application/x-protobuf` / `application/json`）。
//! - **MQTT 3.1.1 降级**（无属性可用）：payload 外层 `enc` 字段（JSON 输出恒含
//!   `"enc":"json"`；protobuf 为二进制，无该字段）+ **topic 后缀**
//!   （[`v311_topic_suffix`]：protobuf = `""`，json = `"/json"`）。接收侧先按 topic 后缀判定，
//!   再以 `enc` 字段交叉校验，两者冲突即拒绝。

use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use base64::Engine as _;
use prost::Message as _;
use protocol_proto::{AuthBlock, DataPoint, Quality, TelemetryBatch};
use serde_json::{Map, Value};

use crate::error::{DaemonError, DaemonResult};
use crate::north::mqtt::PayloadEncoder;
use crate::pipeline::ProcessedSample;

/// 编码枚举的**唯一定义**在 task 19 的 `north::mqtt`，此处再导出（禁止另建一套）。
pub use crate::north::mqtt::Encoding;

// ---- 常量（默认值与协议约定集中声明） ----

/// JSON number 可无损表达的最大整数（2^53 − 1）。
///
/// 超出该值的 `int64`（纳秒时间戳必然超出）在 JSON 中**必须**编码为字符串。
pub const JSON_SAFE_INTEGER_MAX: i64 = 9_007_199_254_740_991;

/// MQTT5 Content Type：protobuf。
pub const CONTENT_TYPE_PROTOBUF: &str = "application/x-protobuf";
/// MQTT5 Content Type：JSON。
pub const CONTENT_TYPE_JSON: &str = "application/json";

/// MQTT5 Payload Format Indicator：未指定 / 二进制（protobuf）。
pub const PAYLOAD_FORMAT_BINARY: u8 = 0;
/// MQTT5 Payload Format Indicator：UTF-8 文本（JSON）。
pub const PAYLOAD_FORMAT_UTF8: u8 = 1;

/// MQTT 3.1.1 降级 topic 后缀：protobuf（无后缀）。
pub const TOPIC_SUFFIX_PROTOBUF: &str = "";
/// MQTT 3.1.1 降级 topic 后缀：JSON。
pub const TOPIC_SUFFIX_JSON: &str = "/json";

/// payload 外层编码标识字段名（MQTT 3.1.1 无属性时的降级判别）。
pub const ENC_FIELD: &str = "enc";
/// `enc` 字段取值（与 [`Encoding::as_str`] 一致，仅 JSON 载荷写入）。
pub const ENC_VALUE_JSON: &str = "json";

/// bytes 类型前缀：未知类型的原始字节。
pub const VALUE_TYPE_BLOB: &str = "blob";
/// bytes 类型前缀：8 字节小端 IEEE754 双精度。
pub const VALUE_TYPE_F64LE: &str = "f64le";

/// 非有限浮点标记：NaN。
pub const F64_MARKER_NAN: &str = "NaN";
/// 非有限浮点标记：+Infinity。
pub const F64_MARKER_INF: &str = "Infinity";
/// 非有限浮点标记：−Infinity。
pub const F64_MARKER_NEG_INF: &str = "-Infinity";

/// `f64::NAN` 的小端规范位模式（仅该常量被判为无效浮点）。
const NAN_BITS_LE: [u8; 8] = f64::NAN.to_le_bytes();
/// `f64::INFINITY` 的小端规范位模式。
const INF_BITS_LE: [u8; 8] = f64::INFINITY.to_le_bytes();
/// `f64::NEG_INFINITY` 的小端规范位模式。
const NEG_INF_BITS_LE: [u8; 8] = f64::NEG_INFINITY.to_le_bytes();

// ---- 编码器契约 ----

/// 批级编码器（task 19 的 [`crate::north::mqtt::MqttClient`] 按**每路连接**选择实现）。
///
/// 实现必须是确定性的：同一 `batch` 两次编码产出相同字节（签名解耦的前提）。
pub trait BatchEncoder: Send + Sync {
    /// 该编码器产出的编码格式。
    fn encoding(&self) -> Encoding;

    /// 把一个 [`TelemetryBatch`] 序列化为该格式的字节。
    ///
    /// # Errors
    /// 序列化失败返回 [`DaemonError::ProtocolError`]（码 1000）。
    fn encode_batch(&self, batch: &TelemetryBatch) -> DaemonResult<Vec<u8>>;

    /// MQTT5 Content Type：`application/x-protobuf` / `application/json`。
    fn content_type(&self) -> &'static str;

    /// MQTT5 Payload Format Indicator：JSON = 1（UTF-8），Protobuf = 0（二进制）。
    fn payload_format_indicator(&self) -> u8;
}

/// Protobuf 编码器（**默认**）。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ProtobufEncoder;

/// JSON 编码器（每路出口可选；大整数/bytes/非有限浮点约定见模块文档）。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct JsonEncoder;

impl BatchEncoder for ProtobufEncoder {
    fn encoding(&self) -> Encoding {
        Encoding::Protobuf
    }

    fn encode_batch(&self, batch: &TelemetryBatch) -> DaemonResult<Vec<u8>> {
        Ok(batch.encode_to_vec())
    }

    fn content_type(&self) -> &'static str {
        CONTENT_TYPE_PROTOBUF
    }

    fn payload_format_indicator(&self) -> u8 {
        PAYLOAD_FORMAT_BINARY
    }
}

impl BatchEncoder for JsonEncoder {
    fn encoding(&self) -> Encoding {
        Encoding::Json
    }

    fn encode_batch(&self, batch: &TelemetryBatch) -> DaemonResult<Vec<u8>> {
        let value = batch_to_json(batch);
        serde_json::to_vec(&value)
            .map_err(|e| codec_err(format!("serialize batch to json failed: {e}")))
    }

    fn content_type(&self) -> &'static str {
        CONTENT_TYPE_JSON
    }

    fn payload_format_indicator(&self) -> u8 {
        PAYLOAD_FORMAT_UTF8
    }
}

// ---- task 19 单点契约对接 ----

impl PayloadEncoder for ProtobufEncoder {
    fn encoding(&self) -> Encoding {
        Encoding::Protobuf
    }

    fn encode(&self, sample: &ProcessedSample) -> DaemonResult<Vec<u8>> {
        Ok(sample_to_data_point(sample).encode_to_vec())
    }
}

impl PayloadEncoder for JsonEncoder {
    fn encoding(&self) -> Encoding {
        Encoding::Json
    }

    fn encode(&self, sample: &ProcessedSample) -> DaemonResult<Vec<u8>> {
        let value = point_to_json(&sample_to_data_point(sample), true);
        serde_json::to_vec(&value)
            .map_err(|e| codec_err(format!("serialize point to json failed: {e}")))
    }
}

// ---- 工厂与入口 ----

/// 按编码声明取得批级编码器（task 19 每路连接持有一个）。
pub fn encoder_for(enc: Encoding) -> Box<dyn BatchEncoder> {
    match enc {
        Encoding::Protobuf => Box::new(ProtobufEncoder),
        Encoding::Json => Box::new(JsonEncoder),
    }
}

/// 反序列化一个 [`TelemetryBatch`]（`enc` 由调用方按连接配置或 topic 后缀给出）。
///
/// # Errors
/// 截断 / 垃圾字节 → [`DaemonError::ProtocolError`]（码 1000），**不 panic**。
pub fn decode_batch(enc: Encoding, bytes: &[u8]) -> DaemonResult<TelemetryBatch> {
    match enc {
        Encoding::Protobuf => TelemetryBatch::decode(bytes)
            .map_err(|e| codec_err(format!("decode protobuf batch failed: {e}"))),
        Encoding::Json => json_to_batch(bytes),
    }
}

/// 反序列化一个 [`DataPoint`]（与 [`PayloadEncoder::encode`] 对称，单点发布路径）。
///
/// # Errors
/// 非法 / 不匹配字节 → [`DaemonError::ProtocolError`]（码 1000）。
pub fn decode_point(enc: Encoding, bytes: &[u8]) -> DaemonResult<DataPoint> {
    match enc {
        Encoding::Protobuf => DataPoint::decode(bytes)
            .map_err(|e| codec_err(format!("decode protobuf point failed: {e}"))),
        Encoding::Json => json_to_point(bytes),
    }
}

/// MQTT 3.1.1 降级（无 MQTT5 属性）时的 **topic 后缀**约定。
///
/// - [`Encoding::Protobuf`] → `""`（无后缀，默认路径）；
/// - [`Encoding::Json`] → `"/json"`。
///
/// 接收侧判据优先级：topic 后缀 → payload 外层 `enc` 字段；两者冲突即拒绝。
pub fn v311_topic_suffix(enc: Encoding) -> &'static str {
    match enc {
        Encoding::Protobuf => TOPIC_SUFFIX_PROTOBUF,
        Encoding::Json => TOPIC_SUFFIX_JSON,
    }
}

/// 解析 `[[northbound]] encoding = "..."` 字面量。
///
/// # Errors
/// 非法取值 → [`DaemonError::ConfigError`]（码 2000），**不静默回退**到 protobuf。
/// 合法字面量：`protobuf` / `proto`（别名）/ `json`（大小写不敏感）。
pub fn parse_encoding(s: &str) -> DaemonResult<Encoding> {
    Encoding::parse(s)
}

// ---- 语义摘要（验签解耦契约，编码无关） ----

/// 业务语义确定性哈希（北向编解码侧的**唯一入口**）。
///
/// 红线约定：签名/验签**绝不**以 protobuf / JSON 序列化字节为对象，而是以
/// 「语义字段按固定顺序规范化后哈希」的摘要为对象——字段固定顺序
/// （点位按 `(device_id, point_id, ts)` 稳定排序 → 逐点
/// `device_id/point_id/unit/ts/quality/value` → `gateway_id` → `batch_ts`），
/// `i64` 一律十进制字符串，`bytes` 原样写入，`auth` 块不参与（防自指）。
/// 因此**同一业务数据无论走 protobuf 还是 JSON 编码，摘要必须逐字节相等**。
///
/// 单一定义：规范化算法只存在于 `crate::auth::signing::semantic_hash`（task 21），
/// 本函数是该算法对北向编码/验签路径的**委托再导出**，禁止在本模块复制第二份
/// 规范化实现（两份实现必然漂移）。
pub fn semantic_digest(batch: &TelemetryBatch) -> [u8; 32] {
    crate::auth::signing::semantic_hash(batch)
}

/// [`semantic_digest`] 的小写 hex 文本形态（日志 / 平台侧比对用）。
pub fn semantic_digest_hex(batch: &TelemetryBatch) -> String {
    hex::encode(semantic_digest(batch))
}

/// 从一段已编码载荷一步还原语义摘要（decode → digest）。
///
/// 用途：接收侧拿到任意编码的载荷后，无需关心编码格式即可参与验签比对。
///
/// # Errors
/// 解码失败（截断 / 垃圾字节 / JSON 结构非法）→ [`DaemonError::ProtocolError`]
/// 或 `ConfigError`（见 [`decode_batch`]），不 panic。
pub fn digest_of_encoded(enc: Encoding, bytes: &[u8]) -> DaemonResult<[u8; 32]> {
    Ok(semantic_digest(&decode_batch(enc, bytes)?))
}

/// 把一个 [`ProcessedSample`] 归一化为 [`DataPoint`]（protobuf 与 JSON **共用**同一函数）。
///
/// 约定：
/// - `value` = `f64` 的 **8 字节小端**位模式（数值载荷的规范形态）；
/// - `ts` = `collected_ts_ns`（网关采集时刻；schema 只有单一 `ts` 字段，
///   `device_ts_ns` 不在北向 schema 承载范围）；
/// - `!value.is_finite()`（NaN / ±Infinity）→ quality **强制降为 `BAD`**
///   （两条编码路径同一规则，故语义哈希一致；这不是单位换算也不是死区过滤）。
pub fn sample_to_data_point(sample: &ProcessedSample) -> DataPoint {
    let quality = if sample.value.is_finite() {
        sample.quality as i32
    } else {
        Quality::Bad as i32
    };
    DataPoint {
        device_id: sample.device_id.clone(),
        point_id: sample.point_id.clone(),
        value: sample.value.to_le_bytes().to_vec(),
        unit: sample.unit.clone(),
        ts: sample.collected_ts_ns,
        quality,
    }
}

// ---- JSON：编码 ----

/// `TelemetryBatch` → JSON 根对象。
fn batch_to_json(batch: &TelemetryBatch) -> Value {
    let mut root = Map::new();
    root.insert(
        ENC_FIELD.to_string(),
        Value::String(ENC_VALUE_JSON.to_string()),
    );
    root.insert(
        "points".to_string(),
        Value::Array(
            batch
                .points
                .iter()
                .map(|point| point_to_json(point, false))
                .collect(),
        ),
    );
    root.insert("ts".to_string(), json_i64(batch.ts));
    root.insert(
        "gateway_id".to_string(),
        Value::String(batch.gateway_id.clone()),
    );
    root.insert(
        "auth".to_string(),
        match batch.auth.as_ref() {
            Some(auth) => auth_to_json(auth),
            None => Value::Null,
        },
    );
    Value::Object(root)
}

/// `DataPoint` → JSON 对象。`with_enc` 为 `true` 时带外层 `enc` 字段（单点发布路径）。
fn point_to_json(point: &DataPoint, with_enc: bool) -> Value {
    let mut map = Map::new();
    if with_enc {
        map.insert(
            ENC_FIELD.to_string(),
            Value::String(ENC_VALUE_JSON.to_string()),
        );
    }
    map.insert(
        "device_id".to_string(),
        Value::String(point.device_id.clone()),
    );
    map.insert(
        "point_id".to_string(),
        Value::String(point.point_id.clone()),
    );
    match non_finite_marker(&point.value) {
        // 非有限浮点：JSON 里 NaN / ±Infinity 非法 → null + 标记（可精确还原位模式）。
        Some(marker) => {
            map.insert("value".to_string(), Value::Null);
            map.insert("value_f64".to_string(), Value::String(marker.to_string()));
        }
        None => {
            map.insert("value".to_string(), bytes_to_json(&point.value));
        }
    }
    map.insert("unit".to_string(), Value::String(point.unit.clone()));
    map.insert("ts".to_string(), json_i64(point.ts));
    map.insert(
        "quality".to_string(),
        Value::String(quality_name(point.quality).to_string()),
    );
    map.insert(
        "quality_code".to_string(),
        json_i64(i64::from(point.quality)),
    );
    Value::Object(map)
}

/// `AuthBlock` → JSON 对象。
fn auth_to_json(auth: &AuthBlock) -> Value {
    let mut map = Map::new();
    map.insert("mid".to_string(), Value::String(auth.mid.clone()));
    map.insert("nonce".to_string(), Value::String(auth.nonce.clone()));
    map.insert("ts".to_string(), json_i64(auth.ts));
    map.insert("sig".to_string(), bytes_to_json(&auth.sig));
    Value::Object(map)
}

/// bytes → `{"t": <类型前缀>, "b64": <标准 base64>}`。
///
/// 类型前缀：8 字节 → `"f64le"`（数值载荷规范形态），其余 → `"blob"`（未知类型原始字节）。
/// 前缀只作类型提示，解码以 `b64` 为准。
fn bytes_to_json(bytes: &[u8]) -> Value {
    let mut map = Map::new();
    map.insert(
        "t".to_string(),
        Value::String(value_type(bytes).to_string()),
    );
    map.insert(
        "b64".to_string(),
        Value::String(BASE64_STANDARD.encode(bytes)),
    );
    Value::Object(map)
}

/// bytes 的类型前缀（见 [`bytes_to_json`]）。
fn value_type(bytes: &[u8]) -> &'static str {
    if bytes.len() == 8 {
        VALUE_TYPE_F64LE
    } else {
        VALUE_TYPE_BLOB
    }
}

/// 非有限浮点判定：仅当字节**恰好**是 NaN / ±Infinity 的**规范**位模式时返回标记名。
///
/// 非规范 NaN 位模式（例如 `u64::MAX` / `i64::MAX` 这类整数载荷）**不**判为无效浮点，
/// 一律按 blob 走 base64，避免整数计数器被误判为 NaN 而丢失精度。
fn non_finite_marker(bytes: &[u8]) -> Option<&'static str> {
    if bytes == NAN_BITS_LE {
        return Some(F64_MARKER_NAN);
    }
    if bytes == INF_BITS_LE {
        return Some(F64_MARKER_INF);
    }
    if bytes == NEG_INF_BITS_LE {
        return Some(F64_MARKER_NEG_INF);
    }
    None
}

/// i64 → JSON：超出 [`JSON_SAFE_INTEGER_MAX`] 走字符串（防 IEEE754 double 丢精度）。
fn json_i64(value: i64) -> Value {
    if !(-JSON_SAFE_INTEGER_MAX..=JSON_SAFE_INTEGER_MAX).contains(&value) {
        Value::String(value.to_string())
    } else {
        Value::from(value)
    }
}

/// quality i32 → 英文枚举名（与 `Quality::as_str_name()` 一致）；未知码值 → `QUALITY_INVALID`。
fn quality_name(code: i32) -> &'static str {
    match code {
        q if q == Quality::Good as i32 => Quality::Good.as_str_name(),
        q if q == Quality::Uncertain as i32 => Quality::Uncertain.as_str_name(),
        q if q == Quality::Bad as i32 => Quality::Bad.as_str_name(),
        q if q == Quality::Simulated as i32 => Quality::Simulated.as_str_name(),
        q if q == Quality::Unspecified as i32 => Quality::Unspecified.as_str_name(),
        _ => "QUALITY_INVALID",
    }
}

// ---- JSON：解码 ----

/// JSON 字节 → `TelemetryBatch`。
fn json_to_batch(bytes: &[u8]) -> DaemonResult<TelemetryBatch> {
    let root = json_object(bytes, "batch")?;
    require_enc(&root)?;
    let points = match root.get("points") {
        Some(Value::Array(items)) => items
            .iter()
            .map(|item| match item {
                Value::Object(map) => json_to_point_map(map),
                other => Err(codec_err(format!(
                    "field `points`: expected object element, got {other}"
                ))),
            })
            .collect::<DaemonResult<Vec<DataPoint>>>(),
        None => Ok(Vec::new()),
        Some(other) => Err(codec_err(format!(
            "field `points`: expected array, got {other}"
        ))),
    }?;
    let auth = match root.get("auth") {
        None | Some(Value::Null) => None,
        Some(Value::Object(map)) => Some(json_to_auth(map)?),
        Some(other) => {
            return Err(codec_err(format!(
                "field `auth`: expected object or null, got {other}"
            )))
        }
    };
    Ok(TelemetryBatch {
        points,
        ts: i64_from_json(root.get("ts"), "ts")?,
        gateway_id: str_from_json(root.get("gateway_id"), "gateway_id")?,
        auth,
    })
}

/// JSON 字节 → `DataPoint`（单点发布路径）。
fn json_to_point(bytes: &[u8]) -> DaemonResult<DataPoint> {
    let map = json_object(bytes, "point")?;
    require_enc(&map)?;
    json_to_point_map(&map)
}

/// 点对象 → `DataPoint`（批内元素与单点发布共用）。
fn json_to_point_map(map: &Map<String, Value>) -> DaemonResult<DataPoint> {
    Ok(DataPoint {
        device_id: str_from_json(map.get("device_id"), "device_id")?,
        point_id: str_from_json(map.get("point_id"), "point_id")?,
        value: value_from_json(map)?,
        unit: str_from_json(map.get("unit"), "unit")?,
        ts: i64_from_json(map.get("ts"), "ts")?,
        quality: quality_from_json(map.get("quality_code"), map.get("quality"))?,
    })
}

/// 点对象 → `value` 字节：优先 `b64`，`null` 时按 `value_f64` 标记还原规范位模式。
fn value_from_json(map: &Map<String, Value>) -> DaemonResult<Vec<u8>> {
    match map.get("value") {
        None | Some(Value::Null) => match map.get("value_f64").and_then(Value::as_str) {
            Some(marker) => Ok(non_finite_from_marker(marker)?.to_le_bytes().to_vec()),
            None => Ok(Vec::new()),
        },
        Some(Value::Object(obj)) => {
            let b64 = obj
                .get("b64")
                .and_then(Value::as_str)
                .ok_or_else(|| codec_err("field `value.b64`: missing or not a string"))?;
            BASE64_STANDARD
                .decode(b64)
                .map_err(|e| codec_err(format!("field `value.b64`: invalid base64: {e}")))
        }
        Some(other) => Err(codec_err(format!(
            "field `value`: expected object or null, got {other}"
        ))),
    }
}

/// 非有限浮点标记 → `f64`（与 [`non_finite_marker`] 互逆）。
fn non_finite_from_marker(marker: &str) -> DaemonResult<f64> {
    match marker {
        F64_MARKER_NAN => Ok(f64::NAN),
        F64_MARKER_INF => Ok(f64::INFINITY),
        F64_MARKER_NEG_INF => Ok(f64::NEG_INFINITY),
        other => Err(codec_err(format!(
            "field `value_f64`: unknown marker `{other}` \
             (expected `NaN` / `Infinity` / `-Infinity`)"
        ))),
    }
}

/// 授权块对象 → `AuthBlock`。
fn json_to_auth(map: &Map<String, Value>) -> DaemonResult<AuthBlock> {
    let sig = match map.get("sig") {
        None | Some(Value::Null) => Vec::new(),
        Some(Value::Object(obj)) => {
            let b64 = obj
                .get("b64")
                .and_then(Value::as_str)
                .ok_or_else(|| codec_err("field `auth.sig.b64`: missing or not a string"))?;
            BASE64_STANDARD
                .decode(b64)
                .map_err(|e| codec_err(format!("field `auth.sig.b64`: invalid base64: {e}")))?
        }
        Some(other) => {
            return Err(codec_err(format!(
                "field `auth.sig`: expected object or null, got {other}"
            )))
        }
    };
    Ok(AuthBlock {
        mid: str_from_json(map.get("mid"), "auth.mid")?,
        nonce: str_from_json(map.get("nonce"), "auth.nonce")?,
        ts: i64_from_json(map.get("ts"), "auth.ts")?,
        sig,
    })
}

/// 解析 JSON 根对象（非对象 / 非法 JSON → `ProtocolError`，不 panic）。
fn json_object(bytes: &[u8], what: &str) -> DaemonResult<Map<String, Value>> {
    let value: Value = serde_json::from_slice(bytes)
        .map_err(|e| codec_err(format!("parse json {what} failed: {e}")))?;
    match value {
        Value::Object(map) => Ok(map),
        other => Err(codec_err(format!(
            "json {what}: expected object root, got {other}"
        ))),
    }
}

/// 校验外层 `enc` 字段（缺失允许；存在但不为 `json` → `ConfigError` 2000）。
fn require_enc(map: &Map<String, Value>) -> DaemonResult<()> {
    match map.get(ENC_FIELD) {
        None => Ok(()),
        Some(Value::String(s)) if s == ENC_VALUE_JSON => Ok(()),
        Some(other) => Err(DaemonError::ConfigError(format!(
            "northbound payload: `{ENC_FIELD}` is {other}, expected `{ENC_VALUE_JSON}`"
        ))),
    }
}

/// JSON number / string → `i64`；字段缺失视为 proto3 默认值 0。
fn i64_from_json(value: Option<&Value>, field: &str) -> DaemonResult<i64> {
    match value {
        None => Ok(0),
        Some(Value::Number(n)) => n
            .as_i64()
            .ok_or_else(|| codec_err(format!("field `{field}`: number is not an i64 ({n})"))),
        Some(Value::String(s)) => s
            .trim()
            .parse::<i64>()
            .map_err(|e| codec_err(format!("field `{field}`: `{s}` is not an i64: {e}"))),
        Some(other) => Err(codec_err(format!(
            "field `{field}`: expected number or string, got {other}"
        ))),
    }
}

/// JSON string → `String`；字段缺失视为 proto3 默认值空串。
fn str_from_json(value: Option<&Value>, field: &str) -> DaemonResult<String> {
    match value {
        None => Ok(String::new()),
        Some(Value::String(s)) => Ok(s.clone()),
        Some(other) => Err(codec_err(format!(
            "field `{field}`: expected string, got {other}"
        ))),
    }
}

/// quality：优先取 `quality_code`（权威 i32），缺失时按枚举名回退。
fn quality_from_json(code: Option<&Value>, name: Option<&Value>) -> DaemonResult<i32> {
    if let Some(code) = code {
        let raw = i64_from_json(Some(code), "quality_code")?;
        return i32::try_from(raw)
            .map_err(|_| codec_err(format!("field `quality_code`: {raw} out of i32 range")));
    }
    let name = match name.and_then(Value::as_str) {
        Some(name) => name,
        None => return Ok(Quality::Unspecified as i32),
    };
    match Quality::from_str_name(name) {
        Some(quality) => Ok(quality as i32),
        None => Err(codec_err(format!("field `quality`: unknown name `{name}`"))),
    }
}

/// 统一错误构造：北向载荷编解码域（码 1000）。
fn codec_err(message: impl Into<String>) -> DaemonError {
    DaemonError::ProtocolError(format!("northbound payload codec: {}", message.into()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::time::Instant;

    use ed25519_dalek::VerifyingKey;
    use serde_json::Value as JsonValue;

    use crate::auth::machine_id::{FingerprintKey, MachineIdentity, StaticAnchor};
    use crate::auth::signing::{
        semantic_hash, verify_batch, AuthSigner, LicenseGate, StaticKeyProvider,
    };
    use crate::error::ERR_CONFIG;

    /// test-only：固定签名时间戳（纳秒），保证测试可重复。
    const TEST_TS_NS: i64 = 1_763_000_000_000_000_000;

    /// test-only：单元测试专用 Ed25519 私钥种子，**禁止用于真实部署**。
    const TEST_ONLY_KEY_A: [u8; 32] = [0x1au8; 32];

    /// test-only：指纹 HMAC key，**禁止用于真实部署**。
    const TEST_ONLY_FP_KEY: &[u8] = b"TEST_ONLY_encoder_fp_key";

    /// test-only：恒定放行的授权闸门（模拟 Token 有效且未降级）。
    struct AlwaysLicensed;

    impl LicenseGate for AlwaysLicensed {
        fn can_sign(&self) -> bool {
            true
        }
    }

    /// test-only：经 task 3 指纹模块派生 mid（本模块不自造 mid 算法）。
    fn test_mid() -> String {
        let identity = MachineIdentity::new(
            vec![Box::new(StaticAnchor::new(
                "test-anchor",
                Some("gw-encoder-001"),
            ))],
            1,
            FingerprintKey::from_bytes(TEST_ONLY_FP_KEY.to_vec()).expect("fp key non-empty"),
        );
        identity
            .get_machine_fingerprint()
            .expect("quorum ok: 1 usable anchor of 1 required")
    }

    /// test-only：签名器 + 对应公钥。
    fn test_signer() -> (AuthSigner, VerifyingKey) {
        let provider = StaticKeyProvider::new(TEST_ONLY_KEY_A);
        let public_key = provider.verifying_key().expect("verifying key");
        let signer = AuthSigner::new(Arc::new(provider), Arc::new(AlwaysLicensed), test_mid())
            .expect("mid is 64-hex");
        (signer, public_key)
    }

    /// test-only：构造 DataPoint。
    fn point(
        device_id: &str,
        point_id: &str,
        value: Vec<u8>,
        ts: i64,
        quality: Quality,
    ) -> DataPoint {
        DataPoint {
            device_id: device_id.to_string(),
            point_id: point_id.to_string(),
            value,
            unit: "kPa".to_string(),
            ts,
            quality: quality as i32,
        }
    }

    /// test-only：构造 TelemetryBatch（点位故意乱序，验证语义哈希顺序无关）。
    fn sample_batch() -> TelemetryBatch {
        TelemetryBatch {
            points: vec![
                point(
                    "pump-02",
                    "outlet_pressure",
                    42.5_f64.to_le_bytes().to_vec(),
                    1_762_999_999_000_000_002,
                    Quality::Good,
                ),
                point(
                    "pump-01",
                    "inlet_temp",
                    36.6_f64.to_le_bytes().to_vec(),
                    1_762_999_999_000_000_001,
                    Quality::Good,
                ),
                point(
                    "pump-01",
                    "vibration",
                    vec![0x40, 0x49, 0x0f, 0xdb],
                    1_762_999_999_000_000_003,
                    Quality::Uncertain,
                ),
            ],
            ts: 1_762_999_999_500_000_000,
            gateway_id: "gw-encoder-001".to_string(),
            auth: None,
        }
    }

    /// test-only：确定性伪随机字节（LCG，避免引入 rand 依赖）。
    fn pseudo_bytes(len: usize, seed: u64) -> Vec<u8> {
        let mut state: u64 = seed | 1;
        (0..len)
            .map(|_| {
                state = state
                    .wrapping_mul(6_364_136_223_846_793_005)
                    .wrapping_add(1_442_695_040_888_963_407);
                (state >> 33) as u8
            })
            .collect()
    }

    // ---- 1. 双编码语义一致 + 验签一致（核心） ----

    /// QA：同一批次走 protobuf / JSON 两条路径，解码后的 `semantic_hash` 逐字节相等，
    /// 且用 task 21 的 `AuthSigner` 签名后两种编码路径下 `verify_batch` 均通过。
    #[test]
    fn dual_encoding_semantic_hash_identical_and_signature_verifies() {
        let batch = sample_batch();
        let (signer, public_key) = test_signer();

        let pb_bytes = ProtobufEncoder
            .encode_batch(&batch)
            .expect("protobuf encode");
        let json_bytes = JsonEncoder.encode_batch(&batch).expect("json encode");

        let from_pb = decode_batch(Encoding::Protobuf, &pb_bytes).expect("protobuf decode");
        let from_json = decode_batch(Encoding::Json, &json_bytes).expect("json decode");

        let expected = semantic_hash(&batch);
        assert_eq!(
            expected,
            semantic_hash(&from_pb),
            "protobuf 往返后语义哈希必须不变"
        );
        assert_eq!(
            expected,
            semantic_hash(&from_json),
            "JSON 往返后语义哈希必须与 protobuf 路径逐字节相等"
        );
        assert_eq!(
            semantic_hash(&from_pb),
            semantic_hash(&from_json),
            "两条编码路径的语义哈希必须逐字节相等"
        );

        // 签名对象是语义哈希（不含编码字节），故一份签名对两种编码路径都成立。
        let block = signer.sign_batch(&batch, TEST_TS_NS).expect("sign");
        verify_batch(&block, &from_pb, &public_key).expect("protobuf 路径验签通过");
        verify_batch(&block, &from_json, &public_key).expect("json 路径验签通过");

        // 带 auth 块的批次：语义哈希不含 auth，两条路径仍一致。
        let mut signed = batch.clone();
        signer.attach(&mut signed, TEST_TS_NS).expect("attach");
        let signed_pb = decode_batch(
            Encoding::Protobuf,
            &ProtobufEncoder.encode_batch(&signed).expect("encode"),
        )
        .expect("decode");
        let signed_json = decode_batch(
            Encoding::Json,
            &JsonEncoder.encode_batch(&signed).expect("encode"),
        )
        .expect("decode");
        assert_eq!(semantic_hash(&signed_pb), semantic_hash(&signed_json));
        assert_eq!(signed_pb.auth, signed_json.auth, "auth 块必须完整往返");
        verify_batch(&block, &signed_json, &public_key).expect("带 auth 的批次验签通过");
    }

    // ---- 2. 大整数精度 ----

    /// QA：`i64::MAX` 纳秒时间戳与 2^53 以上的 uint64 计数器在 JSON 中无精度丢失。
    #[test]
    fn big_integers_keep_full_precision_in_json() {
        let counters: [u64; 5] = [
            9_007_199_254_740_992,     // 2^53
            9_007_199_254_740_993,     // 2^53 + 1
            1_152_921_504_606_846_976, // 2^60
            i64::MAX as u64,           // 2^63 − 1
            u64::MAX,                  // 2^64 − 1（位模式形如 NaN，仍按 blob 走）
        ];
        let mut points = Vec::new();
        for (index, counter) in counters.iter().enumerate() {
            points.push(point(
                "meter-01",
                &format!("counter_{index}"),
                counter.to_le_bytes().to_vec(),
                i64::MAX,
                Quality::Good,
            ));
        }
        let batch = TelemetryBatch {
            points,
            ts: i64::MAX,
            gateway_id: "gw-encoder-001".to_string(),
            auth: None,
        };

        let json_bytes = JsonEncoder.encode_batch(&batch).expect("json encode");
        let root: JsonValue = serde_json::from_slice(&json_bytes).expect("valid json");
        // ts 必须是字符串，且字面量与 i64::MAX 完全一致（无任何 double 舍入）。
        assert!(root["ts"].is_string(), "纳秒时间戳必须是 JSON 字符串");
        assert_eq!(root["ts"].as_str().expect("ts str"), "9223372036854775807");

        let decoded = decode_batch(Encoding::Json, &json_bytes).expect("json decode");
        let from_pb = decode_batch(
            Encoding::Protobuf,
            &ProtobufEncoder.encode_batch(&batch).expect("pb encode"),
        )
        .expect("pb decode");

        assert_eq!(decoded.ts, i64::MAX);
        assert_eq!(decoded.ts, from_pb.ts);
        assert_eq!(semantic_hash(&decoded), semantic_hash(&from_pb));
        for (index, counter) in counters.iter().enumerate() {
            assert_eq!(
                decoded.points[index].value,
                counter.to_le_bytes().to_vec(),
                "uint64 计数器 {counter} 必须逐字节无损往返"
            );
            assert_eq!(decoded.points[index].ts, i64::MAX);
        }
    }

    // ---- 3. 阈值边界 ----

    /// QA：2^53−1 仍用 JSON number（无损），+1 必须退化为字符串。
    #[test]
    fn safe_integer_threshold_boundary() {
        let at_max = JsonEncoder
            .encode_batch(&TelemetryBatch {
                points: Vec::new(),
                ts: JSON_SAFE_INTEGER_MAX,
                gateway_id: "gw".to_string(),
                auth: None,
            })
            .expect("encode");
        let root: JsonValue = serde_json::from_slice(&at_max).expect("json");
        assert!(
            root["ts"].is_number(),
            "2^53-1 未超出阈值，应仍为 JSON number，实际 {0}",
            root["ts"]
        );
        assert_eq!(root["ts"].as_i64(), Some(JSON_SAFE_INTEGER_MAX));

        let over = JsonEncoder
            .encode_batch(&TelemetryBatch {
                points: Vec::new(),
                ts: JSON_SAFE_INTEGER_MAX + 1,
                gateway_id: "gw".to_string(),
                auth: None,
            })
            .expect("encode");
        let root: JsonValue = serde_json::from_slice(&over).expect("json");
        assert!(
            root["ts"].is_string(),
            "2^53 超出阈值，必须是 JSON 字符串，实际 {0}",
            root["ts"]
        );
        assert_eq!(root["ts"].as_str(), Some("9007199254740992"));
        assert_eq!(
            decode_batch(Encoding::Json, &over).expect("decode").ts,
            JSON_SAFE_INTEGER_MAX + 1
        );

        // 负向边界同理。
        let neg = JsonEncoder
            .encode_batch(&TelemetryBatch {
                points: Vec::new(),
                ts: -(JSON_SAFE_INTEGER_MAX + 1),
                gateway_id: "gw".to_string(),
                auth: None,
            })
            .expect("encode");
        let root: JsonValue = serde_json::from_slice(&neg).expect("json");
        assert!(root["ts"].is_string(), "−2^53 必须是字符串");
        assert_eq!(root["ts"].as_str(), Some("-9007199254740992"));
    }

    // ---- 4. NaN / ±Infinity ----

    /// QA：非有限浮点在 JSON 中为 `null` + `value_f64` 标记，quality 降为 BAD，
    /// 输出仍是合法 JSON，decode 回来不 panic 且与 protobuf 路径语义一致。
    #[test]
    fn non_finite_values_encode_as_null_with_quality_marker() {
        let samples: [(&str, f64, &'static str); 3] = [
            ("nan", f64::NAN, F64_MARKER_NAN),
            ("pos_inf", f64::INFINITY, F64_MARKER_INF),
            ("neg_inf", f64::NEG_INFINITY, F64_MARKER_NEG_INF),
        ];
        for (name, value, marker) in samples {
            let sample = ProcessedSample {
                device_id: "dev-1".to_string(),
                point_id: name.to_string(),
                value,
                unit: "kPa".to_string(),
                device_ts_ns: None,
                collected_ts_ns: 1_700_000_000_000_000_000,
                quality: Quality::Good,
            };
            let converted = sample_to_data_point(&sample);
            assert_eq!(
                converted.quality,
                Quality::Bad as i32,
                "{name}: 非有限值必须把 quality 降为 BAD"
            );

            let batch = TelemetryBatch {
                points: vec![converted],
                ts: 1_700_000_000_000_000_000,
                gateway_id: "gw".to_string(),
                auth: None,
            };
            let json_bytes = JsonEncoder.encode_batch(&batch).expect("json encode");
            // 合法 JSON（NaN/Infinity 字面量会让严格解析器报错）。
            let text = String::from_utf8(json_bytes.clone()).expect("utf-8");
            let root: JsonValue = serde_json::from_str::<JsonValue>(&text)
                .unwrap_or_else(|e| panic!("{name}: 输出必须是合法 JSON: {e}"));

            assert!(
                root["points"][0]["value"].is_null(),
                "{name}: value 必须为 null"
            );
            assert_eq!(root["points"][0]["value_f64"].as_str(), Some(marker));
            assert_eq!(root["points"][0]["quality"].as_str(), Some("BAD"));
            assert_eq!(root["points"][0]["quality_code"].as_i64(), Some(3));

            let decoded = decode_batch(Encoding::Json, &json_bytes)
                .unwrap_or_else(|e| panic!("{name}: decode 不得 panic: {e}"));
            assert_eq!(decoded.points[0].value, value.to_le_bytes().to_vec());
            assert_eq!(decoded.points[0].quality, Quality::Bad as i32);

            let from_pb = decode_batch(
                Encoding::Protobuf,
                &ProtobufEncoder.encode_batch(&batch).expect("pb encode"),
            )
            .expect("pb decode");
            assert_eq!(
                semantic_hash(&decoded),
                semantic_hash(&from_pb),
                "{name}: 两条编码路径语义必须一致"
            );
        }
    }

    // ---- 5. bytes / base64 往返 ----

    /// QA：任意字节（含 8 字节整数位模式）经 base64 往返逐字节相等。
    #[test]
    fn bytes_roundtrip_via_base64() {
        let cases: Vec<Vec<u8>> = vec![
            Vec::new(),
            vec![0x00],
            vec![0xde, 0xad, 0xbe, 0xef],
            pseudo_bytes(8, 0x2545_f491),
            pseudo_bytes(13, 0x9e37_79b9),
            pseudo_bytes(64, 0x1234_5678),
            u64::MAX.to_le_bytes().to_vec(),
        ];
        let mut points = Vec::new();
        for (index, bytes) in cases.iter().enumerate() {
            points.push(point(
                "dev",
                &format!("blob_{index}"),
                bytes.clone(),
                1_700_000_000_000_000_000 + index as i64,
                Quality::Good,
            ));
        }
        let batch = TelemetryBatch {
            points,
            ts: 1_700_000_000_000_000_000,
            gateway_id: "gw".to_string(),
            auth: None,
        };

        let json_bytes = JsonEncoder.encode_batch(&batch).expect("encode");
        let root: JsonValue = serde_json::from_slice(&json_bytes).expect("json");
        assert_eq!(
            root["points"][1]["value"]["t"].as_str(),
            Some(VALUE_TYPE_BLOB),
            "非 8 字节载荷的类型前缀必须是 blob"
        );
        assert_eq!(
            root["points"][3]["value"]["t"].as_str(),
            Some(VALUE_TYPE_F64LE),
            "8 字节数值载荷的类型前缀必须是 f64le"
        );
        assert_eq!(
            root["points"][1]["value"]["b64"].as_str(),
            Some(BASE64_STANDARD.encode(&cases[1]).as_str())
        );

        let decoded = decode_batch(Encoding::Json, &json_bytes).expect("decode");
        for (index, bytes) in cases.iter().enumerate() {
            assert_eq!(
                &decoded.points[index].value, bytes,
                "case {index} 往返不一致"
            );
        }
        let from_pb = decode_batch(
            Encoding::Protobuf,
            &ProtobufEncoder.encode_batch(&batch).expect("pb encode"),
        )
        .expect("pb decode");
        assert_eq!(semantic_hash(&decoded), semantic_hash(&from_pb));
    }

    // ---- 6. MQTT5 属性 ----

    /// QA：MQTT5 Payload Format Indicator 与 Content Type 声明正确。
    #[test]
    fn mqtt5_properties_are_declared() {
        assert_eq!(ProtobufEncoder.payload_format_indicator(), 0);
        assert_eq!(JsonEncoder.payload_format_indicator(), 1);
        assert_eq!(ProtobufEncoder.content_type(), "application/x-protobuf");
        assert_eq!(JsonEncoder.content_type(), "application/json");

        let encoders: [Box<dyn BatchEncoder>; 2] =
            [Box::new(ProtobufEncoder), Box::new(JsonEncoder)];
        for encoder in &encoders {
            match encoder.encoding() {
                Encoding::Protobuf => {
                    assert_eq!(encoder.payload_format_indicator(), PAYLOAD_FORMAT_BINARY);
                    assert_eq!(encoder.content_type(), CONTENT_TYPE_PROTOBUF);
                }
                Encoding::Json => {
                    assert_eq!(encoder.payload_format_indicator(), PAYLOAD_FORMAT_UTF8);
                    assert_eq!(encoder.content_type(), CONTENT_TYPE_JSON);
                }
            }
        }
    }

    // ---- 7. MQTT 3.1.1 降级 ----

    /// QA：降级 topic 后缀在两种编码下可区分；`enc` 字段参与交叉校验。
    #[test]
    fn v311_topic_suffix_distinguishes_encodings() {
        assert_eq!(v311_topic_suffix(Encoding::Protobuf), "");
        assert_eq!(v311_topic_suffix(Encoding::Json), "/json");
        assert_ne!(
            v311_topic_suffix(Encoding::Protobuf),
            v311_topic_suffix(Encoding::Json)
        );

        // JSON 载荷外层恒带 enc 字段（protobuf 为二进制，无该字段）。
        let json_bytes = JsonEncoder.encode_batch(&sample_batch()).expect("encode");
        let root: JsonValue = serde_json::from_slice(&json_bytes).expect("json");
        assert_eq!(root["enc"].as_str(), Some("json"));

        // enc 字段与声明冲突 → ConfigError（2000），不静默接受。
        let mut bad = root.clone();
        bad["enc"] = JsonValue::String("yaml".to_string());
        let bad_bytes = serde_json::to_vec(&bad).expect("serialize");
        let err = decode_batch(Encoding::Json, &bad_bytes).expect_err("enc 冲突必须报错");
        assert_eq!(err.error_code(), ERR_CONFIG);
    }

    // ---- 8. 非法 encoding ----

    /// QA：非法 encoding 字面量 → ConfigError(2000)，不静默回退默认值。
    #[test]
    fn parse_encoding_rejects_unknown_values() {
        for bad in ["yaml", "", "  ", "JSON5", "cbor"] {
            let err = parse_encoding(bad).expect_err("非法 encoding 必须报错");
            assert_eq!(
                err.error_code(),
                ERR_CONFIG,
                "`{bad}` 必须是 ConfigError(2000)"
            );
        }
        assert_eq!(parse_encoding("protobuf").expect("ok"), Encoding::Protobuf);
        assert_eq!(parse_encoding("proto").expect("ok"), Encoding::Protobuf);
        assert_eq!(parse_encoding("JSON").expect("ok"), Encoding::Json);
        assert_eq!(
            Encoding::default(),
            Encoding::Protobuf,
            "默认必须是 protobuf"
        );
    }

    // ---- 9. decode 错误路径 ----

    /// QA：截断 / 垃圾字节 → DaemonError，不 panic。
    #[test]
    fn decode_batch_errors_on_garbage_without_panic() {
        let pb = ProtobufEncoder
            .encode_batch(&sample_batch())
            .expect("encode");
        let json = JsonEncoder.encode_batch(&sample_batch()).expect("encode");

        // protobuf：截断的标志性字节（0xff 不是合法 field key）。
        let err = decode_batch(Encoding::Protobuf, &[0xff, 0xff, 0xff]).expect_err("must fail");
        assert_eq!(err.error_code(), 1000);
        // protobuf：合法前缀但被截断。
        let err = decode_batch(Encoding::Protobuf, &pb[..pb.len() - 1]).expect_err("must fail");
        assert_eq!(err.error_code(), 1000);
        // json：合法 JSON 但被截断（不是合法 JSON）。
        let err = decode_batch(Encoding::Json, &json[..json.len() / 2]).expect_err("must fail");
        assert!(
            err.error_code() == 1000 || err.error_code() == ERR_CONFIG,
            "截断 JSON 应收敛为 DaemonError，实际码 {}",
            err.error_code()
        );

        // json：非 JSON / 空 / 数组根 / 字段类型错误。
        for garbage in [
            &b"not json at all"[..],
            &b""[..],
            &b"[]"[..],
            &br#"{"points": "oops"}"#[..],
            &br#"{"points":[{"value":{"b64":"!!!not-base64!!!"}}]}"#[..],
            &br#"{"points":[{"value":null,"value_f64":"None"}]}"#[..],
        ] {
            let err = decode_batch(Encoding::Json, garbage).expect_err("must fail");
            assert!(
                err.error_code() == 1000 || err.error_code() == ERR_CONFIG,
                "垃圾字节应收敛为 DaemonError，实际码 {}",
                err.error_code()
            );
        }
        // 单点路径同样不 panic。
        assert!(decode_point(Encoding::Json, b"{").is_err());
        assert!(decode_point(Encoding::Protobuf, &[0xff]).is_err());
    }

    // ---- 10. 往返确定性 ----

    /// QA：编码 → 解码 → 再编码，两次字节流相等（签名解耦的前提）。
    #[test]
    fn roundtrip_is_deterministic() {
        let batch = sample_batch();
        for enc in [Encoding::Protobuf, Encoding::Json] {
            let encoder = encoder_for(enc);
            let first = encoder.encode_batch(&batch).expect("encode 1");
            let decoded = decode_batch(enc, &first).expect("decode");
            let second = encoder.encode_batch(&decoded).expect("encode 2");
            assert_eq!(first, second, "{enc} 编码不确定（二次编码字节流不一致）");
            assert_eq!(
                semantic_hash(&batch),
                semantic_hash(&decoded),
                "{enc} 往返后语义哈希漂移"
            );
        }
    }

    // ---- 11. 与 task 19 的 PayloadEncoder 对接 ----

    /// QA：`PayloadEncoder` 实现声明的 encoding 与批级端点声明一致，
    /// 且 `publish_sample` 可直接注入。
    #[test]
    fn payload_encoder_matches_endpoint_declaration() {
        let sample = ProcessedSample {
            device_id: "dev-1".to_string(),
            point_id: "p1".to_string(),
            value: 36.5,
            unit: "kPa".to_string(),
            device_ts_ns: None,
            collected_ts_ns: 1_700_000_000_000_000_000,
            quality: Quality::Good,
        };

        let pb_bytes = PayloadEncoder::encode(&ProtobufEncoder, &sample).expect("pb encode");
        assert_eq!(
            PayloadEncoder::encoding(&ProtobufEncoder),
            Encoding::Protobuf
        );
        assert_eq!(BatchEncoder::encoding(&ProtobufEncoder), Encoding::Protobuf);
        let pb_point = decode_point(Encoding::Protobuf, &pb_bytes).expect("pb decode point");
        assert_eq!(pb_point, sample_to_data_point(&sample));
        assert_eq!(
            f64::from_le_bytes(pb_point.value.as_slice().try_into().expect("8 bytes")),
            36.5
        );

        let json_bytes = PayloadEncoder::encode(&JsonEncoder, &sample).expect("json encode");
        assert_eq!(PayloadEncoder::encoding(&JsonEncoder), Encoding::Json);
        assert_eq!(BatchEncoder::encoding(&JsonEncoder), Encoding::Json);
        let root: JsonValue = serde_json::from_slice(&json_bytes).expect("valid json");
        assert_eq!(root["enc"].as_str(), Some("json"));
        assert_eq!(root["quality"].as_str(), Some("GOOD"));
        let json_point = decode_point(Encoding::Json, &json_bytes).expect("json decode point");
        assert_eq!(json_point, sample_to_data_point(&sample));

        // 编码器可被 task 19 的 `&dyn PayloadEncoder` 直接注入。
        let injected: &dyn PayloadEncoder = &JsonEncoder;
        assert_eq!(injected.encoding(), Encoding::Json);
        assert_eq!(injected.encode(&sample).expect("encode"), json_bytes);

        // 非有限值经单点路径同样降级为 BAD（与批级路径同一规则）。
        let bad_sample = ProcessedSample {
            value: f64::NAN,
            ..sample
        };
        let bad_json = PayloadEncoder::encode(&JsonEncoder, &bad_sample).expect("encode");
        let bad_root: JsonValue = serde_json::from_slice(&bad_json).expect("valid json");
        assert!(bad_root["value"].is_null());
        assert_eq!(bad_root["quality"].as_str(), Some("BAD"));
        assert_eq!(
            decode_point(Encoding::Json, &bad_json)
                .expect("decode")
                .quality,
            Quality::Bad as i32
        );
    }

    /// QA：`encoder_for` 返回的编码器与声明一致（task 19 按每路连接选择）。
    #[test]
    fn encoder_for_matches_declaration() {
        assert_eq!(
            encoder_for(Encoding::Protobuf).encoding(),
            Encoding::Protobuf
        );
        assert_eq!(encoder_for(Encoding::Json).encoding(), Encoding::Json);
        assert_eq!(
            encoder_for(Encoding::default()).content_type(),
            CONTENT_TYPE_PROTOBUF
        );
    }

    // ---- 12. 语义摘要（验签解耦契约） ----

    /// QA：语义摘要与编码无关——同一批次（含 NaN / ±Inf / `u64::MAX` / `i64::MAX`
    /// 边界点位）经 protobuf 与 JSON 两条编码路径往返后，`semantic_digest` 逐字节相等。
    #[test]
    fn semantic_digest_is_identical_across_encodings_at_boundaries() {
        // 非有限浮点：经 sample_to_data_point 归一化（quality 强制 BAD，两条路径同一规则）。
        let mut points = Vec::new();
        for (name, value) in [
            ("nan", f64::NAN),
            ("pos_inf", f64::INFINITY),
            ("neg_inf", f64::NEG_INFINITY),
        ] {
            points.push(sample_to_data_point(&ProcessedSample {
                device_id: "dev-1".to_string(),
                point_id: name.to_string(),
                value,
                unit: "kPa".to_string(),
                device_ts_ns: None,
                collected_ts_ns: i64::MAX,
                quality: Quality::Good,
            }));
        }
        // u64::MAX 计数器（位模式形如 NaN，必须按 blob 走，绝不被误判）。
        points.push(point(
            "meter-01",
            "counter_u64max",
            u64::MAX.to_le_bytes().to_vec(),
            i64::MAX,
            Quality::Good,
        ));
        // 正常数值点。
        points.push(point(
            "pump-01",
            "inlet_temp",
            36.6_f64.to_le_bytes().to_vec(),
            1_762_999_999_000_000_001,
            Quality::Good,
        ));
        // 点位故意乱序（语义哈希按 (device_id, point_id, ts) 排序，与顺序无关）。
        points.reverse();
        let batch = TelemetryBatch {
            points,
            ts: i64::MAX,
            gateway_id: "gw-digest-001".to_string(),
            auth: None,
        };

        let expected = semantic_digest(&batch);
        let from_pb = digest_of_encoded(
            Encoding::Protobuf,
            &ProtobufEncoder.encode_batch(&batch).expect("pb encode"),
        )
        .expect("pb decode");
        let from_json = digest_of_encoded(
            Encoding::Json,
            &JsonEncoder.encode_batch(&batch).expect("json encode"),
        )
        .expect("json decode");

        assert_eq!(expected, from_pb, "protobuf 往返后语义摘要必须不变");
        assert_eq!(expected, from_json, "JSON 往返后语义摘要必须与原批次一致");
        assert_eq!(from_pb, from_json, "两条编码路径的摘要必须逐字节相等");
        assert_eq!(
            semantic_digest_hex(&batch).len(),
            64,
            "hex 摘要必须是 64 字符"
        );
    }

    /// QA：语义摘要的确定性与字段敏感性——同一批次两次摘要相同；`auth` 块
    /// 不参与摘要（防自指）；任一语义字段（gateway_id / value / ts）被篡改即漂移。
    #[test]
    fn semantic_digest_is_deterministic_and_auth_agnostic() {
        let batch = sample_batch();
        let baseline = semantic_digest(&batch);

        // 确定性：同一批次重复计算结果一致。
        assert_eq!(baseline, semantic_digest(&batch));

        // auth 不参与摘要：附加签名块后摘要不变（验签对象不含签名块本身）。
        let (signer, _public_key) = test_signer();
        let mut signed = batch.clone();
        signer.attach(&mut signed, TEST_TS_NS).expect("attach");
        assert!(
            signed.auth.is_some(),
            "前置条件：attach 后必须真的带了 auth 块"
        );
        assert_eq!(
            baseline,
            semantic_digest(&signed),
            "auth 块不得影响语义摘要"
        );

        // 字段敏感性：篡改 gateway_id / value / 点位 ts 任一项，摘要必须漂移。
        let mut tampered = batch.clone();
        tampered.gateway_id = "gw-tampered".to_string();
        assert_ne!(baseline, semantic_digest(&tampered), "gateway_id 敏感");

        let mut tampered = batch.clone();
        tampered.points[0].value = 43.0_f64.to_le_bytes().to_vec();
        assert_ne!(baseline, semantic_digest(&tampered), "value 敏感");

        let mut tampered = batch.clone();
        tampered.points[0].ts += 1;
        assert_ne!(baseline, semantic_digest(&tampered), "点位 ts 敏感");

        // 顺序无关性：打乱点位顺序后摘要不变。
        let mut shuffled = batch.clone();
        shuffled.points.reverse();
        assert_eq!(
            baseline,
            semantic_digest(&shuffled),
            "点位顺序不得影响语义摘要"
        );
    }

    // ---- 13. golden bytes（protobuf 线格式冻结） ----

    /// test-only：golden 批次（字段取值刻意选在 varint/base64 可手算验证的位置）：
    /// - 批次 `ts` = 2^56（varint：`80 80 80 80 80 80 80 80 01`，且超出 2^53−1 → JSON 字符串）；
    /// - 点位 `ts` = 2^56 + 1（varint：`81 80 80 80 80 80 80 80 01`）；
    /// - `value` = 36.5（f64 = 0x4042400000000000，小端 = `00 00 00 00 00 40 42 40`）。
    fn golden_batch() -> TelemetryBatch {
        TelemetryBatch {
            points: vec![DataPoint {
                device_id: "d1".to_string(),
                point_id: "p1".to_string(),
                value: 36.5_f64.to_le_bytes().to_vec(),
                unit: "u".to_string(),
                ts: (1u64 << 56) as i64 + 1,
                quality: Quality::Good as i32,
            }],
            ts: (1u64 << 56) as i64,
            gateway_id: "g1".to_string(),
            auth: None,
        }
    }

    /// QA：protobuf 编码字节流**冻结**为 golden 十六进制（跨版本防漂移；
    /// wire 格式由 prost 按字段号顺序编码，golden 手工推导可逐字段核对）。
    #[test]
    fn golden_protobuf_batch_bytes() {
        // DataPoint（字段号顺序）：1=device_id 2=point_id 3=value 4=unit 5=ts 6=quality。
        const GOLDEN_POINT_HEX: &str = concat!(
            "0a026431",                             // 1: "d1"
            "12027031",                             // 2: "p1"
            "1a080000000000404240",                 // 3: value = 36.5 f64le
            "220175",                               // 4: "u"
            "28818080808080808001",                 // 5: ts = 2^56+1 (varint)
            "3001",                                 // 6: quality = GOOD
        );
        // TelemetryBatch：1=points(len 0x21) 2=ts(varint 2^56) 3=gateway_id。
        // （`concat!` 只接受字面量，const 片段用 `format!` 拼接。）
        let golden_batch_hex = format!(
            "0a21{GOLDEN_POINT_HEX}108080808080808080011a026731"
        );

        // 单点 golden。
        let dp = sample_to_data_point(&ProcessedSample {
            device_id: "d1".to_string(),
            point_id: "p1".to_string(),
            value: 36.5,
            unit: "u".to_string(),
            device_ts_ns: None,
            collected_ts_ns: (1u64 << 56) as i64 + 1,
            quality: Quality::Good,
        });
        assert_eq!(
            hex::encode(dp.encode_to_vec()),
            GOLDEN_POINT_HEX,
            "DataPoint wire 格式漂移"
        );

        // 批级 golden。
        let batch = golden_batch();
        assert_eq!(
            hex::encode(ProtobufEncoder.encode_batch(&batch).expect("encode")),
            golden_batch_hex,
            "TelemetryBatch wire 格式漂移"
        );

        // golden 字节可解码回等值结构（冻结的同时保证自洽）。
        let bytes = hex::decode(&golden_batch_hex).expect("golden hex");
        let decoded = decode_batch(Encoding::Protobuf, &bytes).expect("decode golden");
        assert_eq!(decoded.points, batch.points);
        assert_eq!(decoded.ts, batch.ts);
        assert_eq!(decoded.gateway_id, batch.gateway_id);
        assert!(decoded.auth.is_none());
    }

    // ---- 14. golden json（文本格式冻结） ----

    /// QA：JSON 编码文本**冻结**为 golden 字符串（serde_json Map 键序为字典序、
    /// 字段恒齐全输出，故文本字节级确定）。同时覆盖大整数字符串化与 base64 golden。
    #[test]
    fn golden_json_batch_text() {
        const GOLDEN_JSON: &str = concat!(
            r#"{"auth":null,"enc":"json","gateway_id":"g1","points":[{"device_id":"d1",""#,
            r#"point_id":"p1","quality":"GOOD","quality_code":1,"ts":"72057594037927937","#,
            r#""unit":"u","#,
            r#""value":{"b64":"AAAAAABAQkA=","t":"f64le"}}],"#,
            r#""ts":"72057594037927936"}"#,
        );

        let batch = golden_batch();
        let text = String::from_utf8(JsonEncoder.encode_batch(&batch).expect("encode"))
            .expect("utf-8");
        assert_eq!(text, GOLDEN_JSON, "JSON 文本格式漂移");

        // 单点路径（带外层 enc 字段）同一 golden 点位。
        let dp = sample_to_data_point(&ProcessedSample {
            device_id: "d1".to_string(),
            point_id: "p1".to_string(),
            value: 36.5,
            unit: "u".to_string(),
            device_ts_ns: None,
            collected_ts_ns: (1u64 << 56) as i64 + 1,
            quality: Quality::Good,
        });
        let point_text =
            String::from_utf8(JsonEncoder.encode_batch(&TelemetryBatch {
                points: vec![dp],
                ts: 0,
                gateway_id: String::new(),
                auth: None,
            })
            .expect("encode"))
            .expect("utf-8");
        assert!(
            point_text.contains(r#""ts":"72057594037927937""#),
            "点内大整数 ts 必须字符串化: {point_text}"
        );
        assert!(
            point_text.contains(r#""value":{"b64":"AAAAAABAQkA=","t":"f64le"}"#),
            "value base64 golden 必须一致: {point_text}"
        );

        // golden 文本可解码回等值结构（ts 从字符串精确还原 2^56 / 2^56+1）。
        let decoded = decode_batch(Encoding::Json, GOLDEN_JSON.as_bytes()).expect("decode golden");
        assert_eq!(decoded.points, batch.points);
        assert_eq!(decoded.ts, batch.ts);
        assert_eq!(decoded.gateway_id, batch.gateway_id);
    }

    // ---- 15. 性能基线（task 39 压测对比用） ----

    /// 性能基线：`cargo test -p daemon encoder -- --ignored --nocapture`。
    ///
    /// 输出：protobuf / JSON 的字节数与序列化 / 反序列化耗时（µs），供 task 39 对比。
    #[test]
    #[ignore = "性能基线，非功能门禁；需 --ignored 手动运行"]
    fn perf_baseline() {
        const POINTS: usize = 1_000;
        const ITERATIONS: u32 = 50;

        let mut points = Vec::with_capacity(POINTS);
        for index in 0..POINTS {
            let value = (index as f64) * 1.5 + 0.25;
            points.push(point(
                &format!("dev-{}", index % 16),
                &format!("point_{index}"),
                value.to_le_bytes().to_vec(),
                1_762_999_999_000_000_000 + index as i64,
                Quality::Good,
            ));
        }
        let batch = TelemetryBatch {
            points,
            ts: 1_762_999_999_500_000_000,
            gateway_id: "gw-perf-001".to_string(),
            auth: None,
        };

        let pb_bytes = ProtobufEncoder.encode_batch(&batch).expect("pb encode");
        let json_bytes = JsonEncoder.encode_batch(&batch).expect("json encode");

        // 序列化耗时。
        let start = Instant::now();
        for _ in 0..ITERATIONS {
            let _ = ProtobufEncoder.encode_batch(&batch).expect("pb encode");
        }
        let pb_encode_us = start.elapsed().as_secs_f64() * 1e6 / f64::from(ITERATIONS);

        let start = Instant::now();
        for _ in 0..ITERATIONS {
            let _ = JsonEncoder.encode_batch(&batch).expect("json encode");
        }
        let json_encode_us = start.elapsed().as_secs_f64() * 1e6 / f64::from(ITERATIONS);

        // 反序列化耗时。
        let start = Instant::now();
        for _ in 0..ITERATIONS {
            let _ = decode_batch(Encoding::Protobuf, &pb_bytes).expect("pb decode");
        }
        let pb_decode_us = start.elapsed().as_secs_f64() * 1e6 / f64::from(ITERATIONS);

        let start = Instant::now();
        for _ in 0..ITERATIONS {
            let _ = decode_batch(Encoding::Json, &json_bytes).expect("json decode");
        }
        let json_decode_us = start.elapsed().as_secs_f64() * 1e6 / f64::from(ITERATIONS);

        let ratio = json_bytes.len() as f64 / pb_bytes.len() as f64;
        println!("perf_baseline: points={POINTS} iterations={ITERATIONS}");
        println!(
            "  size:     protobuf={} B  json={} B  ratio={ratio:.2}x  delta={} B",
            pb_bytes.len(),
            json_bytes.len(),
            json_bytes.len() as i64 - pb_bytes.len() as i64
        );
        println!(
            "  encode:   protobuf={pb_encode_us:.1} µs/op  json={json_encode_us:.1} µs/op  ratio={:.2}x",
            json_encode_us / pb_encode_us
        );
        println!(
            "  decode:   protobuf={pb_decode_us:.1} µs/op  json={json_decode_us:.1} µs/op  ratio={:.2}x",
            json_decode_us / pb_decode_us
        );
        println!(
            "  per-point: protobuf={:.1} B/pt  json={:.1} B/pt",
            pb_bytes.len() as f64 / POINTS as f64,
            json_bytes.len() as f64 / POINTS as f64
        );

        // 基线自检：性能可以慢，但语义必须不漂移。
        assert_eq!(
            semantic_hash(&decode_batch(Encoding::Json, &json_bytes).expect("decode")),
            semantic_hash(&decode_batch(Encoding::Protobuf, &pb_bytes).expect("decode"))
        );
    }
}

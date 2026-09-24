//! task 14 — 第三方 MQTT 接入驱动（rumqttc 订阅 → 点位映射）。
//!
//! ## 职责边界
//! - **做**：topic 匹配（精确 / `+` 单层 / `#` 多层，纯函数 [`topic_matches`]）；
//!   JSON payload 按点分路径提取 → 数值 → [`JsonPointSample`]（含质量码）；
//!   eventloop 消费循环 [`run_loop`]（QoS1 订阅、断线按 [`Reconnector`] 指数退避
//!   等待后继续 poll —— rumqttc 语义：poll 出错后继续 poll 会自动重连，
//!   与 north/mqtt.rs 的结论一致）。
//! - **不做**：北向发布（north/mqtt.rs）；TLS 配置面（rumqttc Transport 留待后续，
//!   默认明文 1883 口接入第三方 broker）；点位映射的持久化。
//!
//! ## 为什么不实现 `Driver` trait
//! [`crate::driver::Driver`] 是**拉模型**（调用方定时 `read`），而第三方 MQTT 是
//! **推模型**（broker 主动推送）。本模块以「连接 + 事件循环 + 回调」的订阅侧形态
//! 提供能力，由调度器/运行时挂接 `on_sample` 回调，不套用 poll 语义。
//!
//! ## 网络层的可测试性（trait 注入）
//! eventloop 消费循环通过 [`MqttEventSource`] trait 注入事件源：生产环境用
//! rumqttc `EventLoop` 的适配实现，测试用 mock 回放事件序列（含错误事件），
//! **单测不做任何真实网络连接**（`connect()` 使用的 broker 地址也只在生产装配时给出）。
//!
//! ## JSON 提取（与 http.rs 相同语义）
//! 提取纯函数（[`extract_path`] / [`json_to_f64`] / [`json_point_sample`] /
//! [`JsonPointSample`]）按任务约定**定义在本模块并 pub**，供 http.rs（task 13）
//! 复用——同一套点分路径（`data.temperature`、数组下标 `list.0.value`）与质量码
//! 语义，避免两份实现漂移。数值转换失败 / 路径缺失 / payload 非 JSON 均产出
//! `Quality::Bad`（参考 pipeline.rs 的 Quality 用法：NaN 值不被死区吞掉，
//! 有效性由 quality 表达，故 Bad 样本 value 取 NaN 而非伪装 0）。

use std::time::Duration;

use async_trait::async_trait;
use protocol_proto::Quality;
use rumqttc::{AsyncClient, Event, EventLoop, MqttOptions, Packet, QoS};
use serde_json::Value;

use crate::driver::Reconnector;
use crate::error::{DaemonError, DaemonResult};

/// rumqttc 事件轮询结果（0.25 无 `rumqttc::Result` 别名，此处显式定义）。
pub(crate) type EventOutcome = Result<Event, rumqttc::ConnectionError>;

// ---- JSON 路径提取（本模块 pub，供 http.rs 复用） ----

/// 点分路径导航错误。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PathError {
    /// 空路径。
    Empty,
    /// 空分段（如 `a..b`）。
    EmptySegment {
        /// 段序号（0 基）。
        index: usize,
    },
    /// 对象缺键。
    MissingKey {
        /// 缺失的键名。
        key: String,
    },
    /// 下标段不是非负整数（数组容器下）。
    InvalidIndex {
        /// 原始段内容。
        segment: String,
    },
    /// 数组下标越界。
    IndexOutOfRange {
        /// 请求的下标。
        index: usize,
        /// 数组实际长度。
        len: usize,
    },
    /// 以标量为中间节点继续下钻。
    NotNavigable {
        /// 当前段。
        segment: String,
        /// 实际 JSON 类型名（null/bool/number/string）。
        actual: &'static str,
    },
}

impl std::fmt::Display for PathError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PathError::Empty => write!(f, "empty path"),
            PathError::EmptySegment { index } => write!(f, "empty segment at #{index}"),
            PathError::MissingKey { key } => write!(f, "missing key {key:?}"),
            PathError::InvalidIndex { segment } => {
                write!(
                    f,
                    "array index must be a non-negative integer, got {segment:?}"
                )
            }
            PathError::IndexOutOfRange { index, len } => {
                write!(f, "array index {index} out of range (len={len})")
            }
            PathError::NotNavigable { segment, actual } => {
                write!(f, "cannot descend into {actual} at segment {segment:?}")
            }
        }
    }
}

/// JSON 类型名（错误消息用）。
fn json_kind(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "bool",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

/// 按点分路径提取 JSON 值（纯函数）。
///
/// 语法：`.` 分隔；段为对象键或（数组容器下的）十进制下标，如
/// `data.temperature`、`list.0.value`。对象容器下数字段按**键**查找
/// （兼容 `{"1": …}` 的数字键对象）；数组容器下按**下标**查找。
///
/// # Errors
/// 空路径 / 空分段 / 缺键 / 下标非法或越界 / 以标量为中间节点 → [`PathError`]。
pub fn extract_path<'a>(root: &'a Value, path: &str) -> Result<&'a Value, PathError> {
    if path.trim().is_empty() {
        return Err(PathError::Empty);
    }
    let mut current = root;
    for (i, segment) in path.split('.').enumerate() {
        if segment.is_empty() {
            return Err(PathError::EmptySegment { index: i });
        }
        match current {
            Value::Object(map) => {
                current = map.get(segment).ok_or_else(|| PathError::MissingKey {
                    key: segment.to_string(),
                })?;
            }
            Value::Array(arr) => {
                let index: usize = segment.parse().map_err(|_| PathError::InvalidIndex {
                    segment: segment.to_string(),
                })?;
                current = arr.get(index).ok_or(PathError::IndexOutOfRange {
                    index,
                    len: arr.len(),
                })?;
            }
            other => {
                return Err(PathError::NotNavigable {
                    segment: segment.to_string(),
                    actual: json_kind(other),
                });
            }
        }
    }
    Ok(current)
}

/// JSON 值 → f64（纯函数）。
///
/// 规则：Number 直接转；String 尝试按 f64 解析（trim 首尾空白）；
/// 其余类型（bool/null/array/object）视为转换失败。
///
/// # Errors
/// 类型不支持或字符串非数值时返回原因描述。
pub fn json_to_f64(value: &Value) -> Result<f64, String> {
    match value {
        Value::Number(n) => n
            .as_f64()
            .ok_or_else(|| "number out of f64 range".to_string()),
        Value::String(s) => s
            .trim()
            .parse::<f64>()
            .map_err(|_| format!("string {s:?} is not numeric")),
        other => Err(format!(
            "unsupported json type {} for numeric conversion",
            json_kind(other)
        )),
    }
}

/// JSON 点位采样结果（http.rs 与本模块共用的提取产物）。
#[derive(Debug, Clone, PartialEq)]
pub struct JsonPointSample {
    /// 点位标识（映射配置的 `point_id`）。
    pub point_id: String,
    /// 数值（提取成功时为 JSON 数值；失败时为 NaN，有效性以 `quality` 为准）。
    pub value: f64,
    /// 质量码：成功 `Good`；路径缺失 / 类型错误 / payload 非 JSON → `Bad`。
    pub quality: Quality,
    /// 失败原因（成功时为空串；日志与诊断用）。
    pub detail: String,
}

impl JsonPointSample {
    /// 成功样本。
    pub fn good(point_id: &str, value: f64) -> Self {
        Self {
            point_id: point_id.to_string(),
            value,
            quality: Quality::Good,
            detail: String::new(),
        }
    }

    /// 失败样本（value 取 NaN：NaN 不被死区吞掉，有效性由 quality 表达）。
    pub fn bad(point_id: &str, detail: String) -> Self {
        Self {
            point_id: point_id.to_string(),
            value: f64::NAN,
            quality: Quality::Bad,
            detail,
        }
    }
}

/// 一步到位：按路径从 JSON 根提取单点样本（路径缺失 / 类型错误 → `Quality::Bad`）。
pub fn json_point_sample(point_id: &str, json_path: &str, root: &Value) -> JsonPointSample {
    match extract_path(root, json_path) {
        Err(e) => JsonPointSample::bad(point_id, format!("json path {json_path:?}: {e}")),
        Ok(v) => match json_to_f64(v) {
            Ok(n) => JsonPointSample::good(point_id, n),
            Err(e) => JsonPointSample::bad(point_id, format!("json path {json_path:?}: {e}")),
        },
    }
}

// ---- topic 匹配 ----

/// MQTT topic 通配匹配（纯函数，MQTT 3.1.1 语义）。
///
/// 规则：
/// - `+` 恰好匹配一层（可为空层，如 `a/+/c` 匹配 `a//c`）；
/// - `#` 匹配剩余全部层（含零层：`sport/#` 匹配 `sport`），且必须是模式的最后一层；
/// - 通配符必须独占一层（`a#` / `s+port` 为非法模式，不匹配任何主题）；
/// - 区分大小写；层数必须严格对应。
pub fn topic_matches(pattern: &str, actual: &str) -> bool {
    let pattern_levels: Vec<&str> = pattern.split('/').collect();
    let actual_levels: Vec<&str> = actual.split('/').collect();
    let mut i = 0usize;
    for (idx, seg) in pattern_levels.iter().enumerate() {
        match *seg {
            "#" => {
                // '#' 必须独占一层且位于末尾（split 已保证独占层，这里校验末尾）。
                if idx != pattern_levels.len() - 1 {
                    return false;
                }
                // 匹配剩余全部层（含零层）。
                return true;
            }
            "+" => {
                if i >= actual_levels.len() {
                    return false;
                }
                i += 1;
            }
            literal => {
                // 通配符必须独占一层：层内出现 '+'/'#' 即非法模式。
                if literal.contains('#') || literal.contains('+') {
                    return false;
                }
                if i >= actual_levels.len() || actual_levels[i] != literal {
                    return false;
                }
                i += 1;
            }
        }
    }
    i == actual_levels.len()
}

// ---- 配置 ----

/// 单条 topic 订阅映射：主题模式 + payload 内 JSON 路径 + 目标点位。
#[derive(Debug, Clone)]
pub struct MqttTopicMapping {
    /// topic 订阅模式（支持 `+` / `#` 通配）。
    pub topic: String,
    /// payload JSON 点分路径（如 `data.temperature`；空串视为整包数值）。
    pub json_path: String,
    /// 目标点位标识。
    pub point_id: String,
}

/// 第三方 MQTT 接入配置。
#[derive(Debug, Clone)]
pub struct MqttInConfig {
    /// broker 主机。
    pub host: String,
    /// broker 端口（明文 1883；TLS 留待后续）。
    pub port: u16,
    /// client id（建议固定，配合 QoS1 会话恢复）。
    pub client_id: String,
    /// 用户名（可选）。
    pub username: Option<String>,
    /// 密码（可选）。
    pub password: Option<String>,
    /// keep alive 间隔。
    pub keep_alive: Duration,
    /// topic 订阅映射列表。
    pub topics: Vec<MqttTopicMapping>,
}

impl Default for MqttInConfig {
    fn default() -> Self {
        Self {
            host: "127.0.0.1".to_string(),
            port: 1883,
            client_id: "iot-daq-mqtt-in".to_string(),
            username: None,
            password: None,
            keep_alive: Duration::from_secs(30),
            topics: Vec::new(),
        }
    }
}

// ---- 事件源注入（网络层可测试性） ----

/// MQTT 事件源抽象：将 rumqttc `EventLoop::poll` 注入化，测试用 mock 回放事件。
#[async_trait]
pub trait MqttEventSource: Send {
    /// 取下一个事件。
    /// 返回 `None` 表示事件流结束（**仅测试回放哨兵**；真实 `EventLoop` 恒为 `Some`）。
    async fn poll(&mut self) -> Option<EventOutcome>;
}

#[async_trait]
impl MqttEventSource for EventLoop {
    async fn poll(&mut self) -> Option<EventOutcome> {
        Some(EventLoop::poll(self).await)
    }
}

// ---- 驱动 ----

/// 第三方 MQTT 接入驱动（订阅侧）。
pub struct MqttInDriver {
    config: MqttInConfig,
    /// (client, eventloop) 会话；`connect()` 成功后持有。
    session: Option<(AsyncClient, EventLoop)>,
    reconnector: Reconnector,
}

impl MqttInDriver {
    /// 创建驱动。
    pub fn new(config: MqttInConfig) -> Self {
        let reconnector = Reconnector::default();
        Self {
            config,
            session: None,
            reconnector,
        }
    }

    /// 当前重连退避状态（只读；测试断言用）。
    pub fn reconnector(&self) -> &Reconnector {
        &self.reconnector
    }

    /// 建立会话并按配置发出 QoS1 订阅。
    ///
    /// rumqttc 0.25 无 `AsyncClient::connect`（north/mqtt.rs 同用法）：`new` 仅建立
    /// 内存通道，**实际 TCP 连接在 eventloop 首次 `poll()` 时发起**；连接/订阅的
    /// 结果由 [`run_loop`] 消费事件时观察（错误按退避继续 poll 自动重连）。
    ///
    /// # Errors
    /// 订阅请求入队失败（会话通道已关闭等）→ [`DaemonError::ProtocolError`]。
    pub async fn connect(&mut self) -> DaemonResult<()> {
        let mut opts = MqttOptions::new(
            self.config.client_id.as_str(),
            self.config.host.as_str(),
            self.config.port,
        );
        opts.set_keep_alive(self.config.keep_alive);
        if let (Some(user), Some(pass)) = (&self.config.username, &self.config.password) {
            opts.set_credentials(user.clone(), pass.clone());
        }
        let (client, eventloop) = AsyncClient::new(opts, 10);
        for mapping in &self.config.topics {
            client
                .subscribe(mapping.topic.as_str(), QoS::AtLeastOnce)
                .await
                .map_err(|e| {
                    DaemonError::ProtocolError(format!(
                        "mqtt-in subscribe {:?}: {e}",
                        mapping.topic
                    ))
                })?;
        }
        self.session = Some((client, eventloop));
        self.reconnector.reset();
        Ok(())
    }

    /// 取走 eventloop（供注入 [`run_loop`]）。
    ///
    /// 生产装配顺序：`connect()` → `take_event_loop()` → `run_loop(...)`。
    /// 取走后本实例不再持有连接（client 随 session 一并释放；
    /// 订阅请求已入 eventloop 队列，由 run_loop 的首次 poll 发出）。
    pub fn take_event_loop(&mut self) -> Option<EventLoop> {
        self.session.take().map(|(_, eventloop)| eventloop)
    }
}

/// eventloop 消费循环：QoS1 订阅消费 + payload 映射 + 断线退避。
///
/// - 收到 `Publish`：按 [`topic_matches`] 找出所有匹配映射，逐条
///   [`json_point_sample`] 提取并以 `on_sample` 回调产出；
///   payload 整包非 JSON 时为每条匹配映射产出 `Quality::Bad` 样本（不 panic、不断流）；
/// - 其他事件（ConnAck/SubAck/PingResp/Outgoing…）忽略；
/// - poll 出错：按 [`Reconnector`] 指数退避 `sleep` 后继续 poll
///   （rumqttc 语义：继续 poll 会自动重连）；
/// - 事件流结束（`None`，测试哨兵）时返回最终退避器供断言。
pub async fn run_loop<S, F>(
    mut source: S,
    mappings: &[MqttTopicMapping],
    mut on_sample: F,
    mut backoff: Reconnector,
) -> Reconnector
where
    S: MqttEventSource,
    F: FnMut(JsonPointSample),
{
    loop {
        match source.poll().await {
            Some(Ok(Event::Incoming(Packet::Publish(publish)))) => {
                let matched: Vec<&MqttTopicMapping> = mappings
                    .iter()
                    .filter(|m| topic_matches(&m.topic, &publish.topic))
                    .collect();
                match serde_json::from_slice::<Value>(publish.payload.as_ref()) {
                    Ok(root) => {
                        for mapping in matched {
                            let sample =
                                json_point_sample(&mapping.point_id, &mapping.json_path, &root);
                            on_sample(sample);
                        }
                    }
                    Err(e) => {
                        // 整包非 JSON：全部匹配映射产出 Bad 样本，链路不断。
                        for mapping in matched {
                            on_sample(JsonPointSample::bad(
                                &mapping.point_id,
                                format!("payload json parse: {e}"),
                            ));
                        }
                    }
                }
            }
            Some(Ok(_)) => {}
            Some(Err(err)) => {
                tracing::warn!(
                    error = %err,
                    "mqtt-in eventloop error; backing off before next poll"
                );
                tokio::time::sleep(backoff.next_delay()).await;
            }
            None => return backoff,
        }
    }
}

// ---- 测试（全部纯函数 / mock 注入，禁真实网络） ----

#[cfg(test)]
mod tests {
    use super::*;
    use std::io;
    use std::sync::{Arc, Mutex};

    use rumqttc::{ConnectionError, Outgoing, Publish};

    /// 组装测试用 Incoming Publish 事件。
    #[allow(clippy::result_large_err)] // 刻意镜像生产 EventOutcome 别名；Err 为 rumqttc 大类型
    fn publish_event(topic: &str, payload: &str) -> EventOutcome {
        Ok(Event::Incoming(Packet::Publish(Publish::new(
            topic,
            QoS::AtLeastOnce,
            payload.as_bytes().to_vec(),
        ))))
    }

    /// 组装测试用错误事件（模拟断线）。
    #[allow(clippy::result_large_err)] // 刻意镜像生产 EventOutcome 别名；Err 为 rumqttc 大类型
    fn error_event() -> EventOutcome {
        Err(ConnectionError::Io(io::Error::new(
            io::ErrorKind::ConnectionReset,
            "mock reset",
        )))
    }

    /// mock 事件源：按序回放，耗尽后返回 None（run_loop 据此返回）。
    struct MockSource {
        events: Vec<EventOutcome>,
    }

    #[async_trait]
    impl MqttEventSource for MockSource {
        async fn poll(&mut self) -> Option<EventOutcome> {
            if self.events.is_empty() {
                None
            } else {
                Some(self.events.remove(0))
            }
        }
    }

    // ---- topic_matches 全场景 ----

    /// QA: 精确匹配 / 大小写敏感 / 层数严格。
    #[test]
    fn topic_matches_exact() {
        assert!(topic_matches("sensor/temp", "sensor/temp"));
        assert!(!topic_matches("sensor/temp", "sensor/hum"));
        assert!(
            !topic_matches("Sensor/temp", "sensor/temp"),
            "case-sensitive"
        );
        assert!(
            !topic_matches("sensor/temp", "sensor/temp/x"),
            "no extra levels"
        );
        assert!(
            !topic_matches("sensor/temp/x", "sensor/temp"),
            "no missing levels"
        );
        assert!(
            !topic_matches("sensor", "sensortemp"),
            "whole level match only"
        );
    }

    /// QA: `+` 单层通配（含空层）。
    #[test]
    fn topic_matches_single_level_wildcard() {
        assert!(topic_matches("sensor/+", "sensor/temp"));
        assert!(
            !topic_matches("sensor/+", "sensor"),
            "+ needs its own level"
        );
        assert!(
            !topic_matches("sensor/+", "sensor/temp/x"),
            "+ is exactly one level"
        );
        assert!(topic_matches("+/temp", "sensor/temp"));
        assert!(topic_matches("a/+/c", "a//c"), "+ matches empty level");
        assert!(topic_matches("sensor/+/temp", "sensor/line1/temp"));
        assert!(!topic_matches("sensor/+/temp", "sensor/line1/line2/temp"));
    }

    /// QA: `#` 多层通配（含零层；必须位于末尾）。
    #[test]
    fn topic_matches_multi_level_wildcard() {
        assert!(topic_matches("sensor/#", "sensor/temp/x"));
        assert!(
            topic_matches("sensor/#", "sensor"),
            "# also matches zero levels"
        );
        assert!(topic_matches("#", "a/b/c"));
        assert!(topic_matches("#", "sensor"));
        assert!(!topic_matches("a/#/c", "a/x/c"), "# must be the last level");
        assert!(!topic_matches("#/temp", "a/temp"), "# only at tail");
    }

    /// QA: 通配符必须独占一层（非法模式不匹配任何主题）。
    #[test]
    fn topic_matches_rejects_inline_wildcards() {
        assert!(!topic_matches("sensor#", "sensor/temp"));
        assert!(!topic_matches("sen+or/temp", "sensor/temp"));
        assert!(!topic_matches("a+/c", "ax/c"));
    }

    // ---- JSON 提取（http.rs 共用语义） ----

    /// QA Happy: 点分路径 / 数组下标 / 嵌套组合 / 数字键对象。
    #[test]
    fn extract_path_happy() {
        let root: Value = serde_json::from_str(
            r#"{"data":{"temperature":21.5},"list":[{"value":1},{"value":2}],"1":"numeric-key"}"#,
        )
        .expect("json");
        assert_eq!(
            extract_path(&root, "data.temperature").expect("ok"),
            &Value::from(21.5)
        );
        assert_eq!(
            extract_path(&root, "list.1.value").expect("ok"),
            &Value::from(2)
        );
        assert_eq!(
            extract_path(&root, "1").expect("ok"),
            &Value::from("numeric-key"),
            "object numeric key looked up by key"
        );
    }

    /// QA Error: 空路径 / 空分段 / 缺键 / 下标越界或非法 / 标量下钻。
    #[test]
    fn extract_path_errors() {
        let root: Value = serde_json::from_str(r#"{"data":{"t":1.0},"list":[10,20],"s":"scalar"}"#)
            .expect("json");

        assert_eq!(extract_path(&root, "").unwrap_err(), PathError::Empty);
        assert!(matches!(
            extract_path(&root, "data..t"),
            Err(PathError::EmptySegment { index: 1 })
        ));
        assert!(matches!(
            extract_path(&root, "data.missing"),
            Err(PathError::MissingKey { key }) if key == "missing"
        ));
        assert!(matches!(
            extract_path(&root, "list.5"),
            Err(PathError::IndexOutOfRange { index: 5, len: 2 })
        ));
        assert!(matches!(
            extract_path(&root, "list.x"),
            Err(PathError::InvalidIndex { .. })
        ));
        assert!(matches!(
            extract_path(&root, "s.0"),
            Err(PathError::NotNavigable {
                actual: "string",
                ..
            })
        ));
    }

    /// QA: json_to_f64 数值 / 字符串数值 / 各类失败路径。
    #[test]
    fn json_to_f64_cases() {
        assert_eq!(json_to_f64(&Value::from(42)).expect("int ok"), 42.0);
        assert_eq!(json_to_f64(&Value::from(1.5)).expect("float ok"), 1.5);
        assert_eq!(
            json_to_f64(&Value::from(" 3.25 ")).expect("numeric string ok"),
            3.25
        );
        assert!(
            json_to_f64(&Value::from("abc")).is_err(),
            "non-numeric string"
        );
        assert!(json_to_f64(&Value::Bool(true)).is_err(), "bool not numeric");
        assert!(json_to_f64(&Value::Null).is_err(), "null not numeric");
        assert!(
            json_to_f64(&serde_json::json!([1])).is_err(),
            "array not numeric"
        );
    }

    /// QA: json_point_sample 成功 → Good；路径缺失 / 类型错误 → Bad（value=NaN）。
    #[test]
    fn json_point_sample_quality_semantics() {
        let root: Value = serde_json::from_str(r#"{"value":21.5,"s":"oops"}"#).expect("json");

        let good = json_point_sample("t1", "value", &root);
        assert_eq!(good, JsonPointSample::good("t1", 21.5));
        assert_eq!(good.quality, Quality::Good);

        let missing = json_point_sample("t1", "nope.value", &root);
        assert_eq!(missing.quality, Quality::Bad);
        assert!(missing.value.is_nan(), "bad sample value is NaN");
        assert!(missing.detail.contains("nope.value"), "detail keeps path");

        let type_err = json_point_sample("t1", "s", &root);
        assert_eq!(type_err.quality, Quality::Bad);
        assert!(
            type_err.detail.contains("not numeric"),
            "{}",
            type_err.detail
        );
    }

    // ---- payload 映射 ----

    /// QA: 映射产物字段正确（point_id 透传、value 提取）。
    #[test]
    fn map_payload_by_mapping() {
        let root: Value = serde_json::from_str(r#"{"data":{"pv":36.5}}"#).expect("json");
        let sample = json_point_sample("p_temp", "data.pv", &root);
        assert_eq!(sample.point_id, "p_temp");
        assert_eq!(sample.value, 36.5);
        assert_eq!(sample.quality, Quality::Good);
    }

    // ---- run_loop（mock 事件源回放，不做真实连接） ----

    /// QA Happy: 通配/精确映射命中、非匹配主题忽略、非 JSON payload → Bad、
    /// 错误事件走退避且不中断、事件流结束返回退避器。
    #[tokio::test]
    async fn run_loop_replays_events_and_maps_payloads() {
        let mappings = vec![
            MqttTopicMapping {
                topic: "plant/+/temp".to_string(),
                json_path: "value".to_string(),
                point_id: "t1".to_string(),
            },
            MqttTopicMapping {
                topic: "plant/#".to_string(),
                json_path: "value".to_string(),
                point_id: "all".to_string(),
            },
        ];
        let source = MockSource {
            events: vec![
                // 1. 命中两条映射：t1 + all。
                publish_event("plant/line1/temp", r#"{"value":21.5}"#),
                // 2. 其他事件（Outgoing）忽略。
                Ok(Event::Outgoing(Outgoing::PingReq)),
                // 3. 非匹配主题忽略。
                publish_event("other/topic", r#"{"value":1}"#),
                // 4. 整包非 JSON：命中两条映射各产出一个 Bad。
                publish_event("plant/line2/temp", "not-json"),
                // 5. 断线错误：退避后继续（下面事件仍被消费）。
                error_event(),
                // 6. 重连恢复后的事件：继续产出 Good。
                publish_event("plant/line1/temp", r#"{"value":22.5}"#),
            ],
        };

        let samples: Arc<Mutex<Vec<JsonPointSample>>> = Arc::default();
        let sink = Arc::clone(&samples);
        let backoff = Reconnector::new(Duration::from_millis(1), Duration::from_millis(2), 2);
        let mut returned = run_loop(
            source,
            &mappings,
            move |sample| sink.lock().expect("sink lock").push(sample),
            backoff,
        )
        .await;

        let got = samples.lock().expect("sink lock");
        // 事件 1：两条 Good。
        assert_eq!(got[0], JsonPointSample::good("t1", 21.5));
        assert_eq!(got[1], JsonPointSample::good("all", 21.5));
        // 事件 3：无样本（非匹配主题）。
        // 事件 4：两条 Bad（非 JSON payload）。
        assert_eq!(got[2].point_id, "t1");
        assert_eq!(got[2].quality, Quality::Bad);
        assert!(got[2].value.is_nan());
        assert!(got[2].detail.contains("json parse"), "{}", got[2].detail);
        assert_eq!(got[3].point_id, "all");
        assert_eq!(got[3].quality, Quality::Bad);
        // 事件 6：重连后继续 Good。
        assert_eq!(got[4], JsonPointSample::good("t1", 22.5));
        assert_eq!(got[5], JsonPointSample::good("all", 22.5));
        assert_eq!(got.len(), 6, "no extra samples");

        // 退避推进断言：错误事件消耗一次 next_delay（initial=1ms）→ 下次为 2ms。
        assert_eq!(
            returned.next_delay(),
            Duration::from_millis(2),
            "error event advanced backoff"
        );
    }

    /// QA: 无匹配映射的配置不产生任何样本。
    #[tokio::test]
    async fn run_loop_empty_mappings_produce_nothing() {
        let source = MockSource {
            events: vec![publish_event("a/b", r#"{"value":1}"#), error_event()],
        };
        let samples: Arc<Mutex<Vec<JsonPointSample>>> = Arc::default();
        let sink = Arc::clone(&samples);
        let backoff = Reconnector::new(Duration::from_millis(1), Duration::from_millis(1), 2);
        let mut returned = run_loop(
            source,
            &[],
            move |sample| sink.lock().expect("sink lock").push(sample),
            backoff,
        )
        .await;
        assert!(
            samples.lock().expect("sink lock").is_empty(),
            "no mappings → no samples"
        );
        // 错误事件仍推进退避（回到 initial 附近：1ms 消耗一次后仍为 1ms）。
        assert_eq!(returned.next_delay(), Duration::from_millis(1));
    }
}

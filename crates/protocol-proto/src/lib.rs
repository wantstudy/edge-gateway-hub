//! `protocol-proto` — 北向遥测 Protobuf schema 与生成代码。
//!
//! schema 唯一来源：`proto/telemetry.proto`（方案 a：prost-build + protoc-bin-vendored，
//! 构建期生成，见 `build.rs`）。消息：`TelemetryBatch` / `DataPoint` / `AuthBlock`。
//!
//! JSON 编码约定（与 task 62 双编码器对齐，详见 .proto 头注释）：
//! - `int64`（纳秒时间戳 / uint64 计数器）在 JSON 中**必须编码为字符串**
//!   （超出 2^53-1 会被 IEEE754 double 静默丢精度）；
//! - `bytes`（value / sig）编码为 base64；
//! - `f64` 的 NaN / ±Infinity 编码为 `null`，有效性借 `quality` 表达；
//! - 字段名统一 snake_case，Protobuf 与 JSON 语义一一对应；
//! - `quality` 枚举输出英文枚举名（GOOD/UNCERTAIN/BAD/SIMULATED），界面层才转中文。

/// prost 生成命名空间（源：proto/telemetry.proto，package `iotdaq.telemetry`）。
pub mod pb {
    include!(concat!(env!("OUT_DIR"), "/iotdaq.telemetry.rs"));
}

pub use pb::{AuthBlock, DataPoint, TelemetryBatch};

/// 质量码枚举提升到 crate 根，业务侧直接引用；JSON 路径输出英文枚举名。
pub use pb::data_point::Quality;

#[cfg(test)]
mod tests {
    use super::Quality;

    /// 枚举 JSON 名与 schema 约定 5 一致（防生成器重命名导致漂移）。
    #[test]
    fn quality_enum_json_names() {
        assert_eq!(Quality::Unspecified.as_str_name(), "QUALITY_UNSPECIFIED");
        assert_eq!(Quality::Good.as_str_name(), "GOOD");
        assert_eq!(Quality::Uncertain.as_str_name(), "UNCERTAIN");
        assert_eq!(Quality::Bad.as_str_name(), "BAD");
        assert_eq!(Quality::Simulated.as_str_name(), "SIMULATED");
    }
}

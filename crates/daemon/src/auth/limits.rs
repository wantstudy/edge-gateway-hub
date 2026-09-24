//! task 24 — 免费基础版限制逻辑（8 设备 / ≥1s / 无北向转发 / 无 OTA）。
//!
//! ## 设计要点
//! - **纯策略校验模块**：输入 [`GatewayConfig`] + licensed 标志，输出**全部**违规项
//!   （聚合、不短路）；licensed = true（已激活授权）时不限制，恒返回空。
//! - 配置统计口径（与 `config.rs` task 4 版字段对齐）：
//!   - **设备数**：`[[points]]` 平铺行去重后的 `device_id` 数；
//!   - **最小采集间隔**：每设备各点位 `frequency_ms` 的最小值，任一设备最小值
//!     < [`FREE_MIN_POLL_INTERVAL_MS`] 即违规（逐设备上报）；
//!   - **北向转发**：`[[outlets]]` 非空即视为启用北向转发；
//!   - **OTA**：当前 `GatewayConfig`（`config.rs` task 4 版）**尚无 OTA 配置字段**
//!     （`SecuritySection` 仅含 `tls_cert_path` / `tls_key_path` / `web_auth_enabled`），
//!     [`LimitViolation::OtaEnabled`] 为**预留变体**，OTA 配置字段接入后在
//!     [`validate`] 中聚合——本阶段恒不产出该违规。
//! - **零 IO、零 panic**：只读配置切片，返回违规向量；违规判定顺序稳定
//!   （BTreeMap 排序遍历），便于测试与日志比对。
//! - 违规处置策略由调用方决定（本模块只报告：如 `ExpiredDegrade` 场景按本清单
//!   拒绝超额设备接入 / 抬升采集频率 / 关闭北向出口），**不停用本地采集**。

use std::collections::{BTreeMap, BTreeSet};

use crate::config::GatewayConfig;

/// 免费版最大南向设备数（task 24 契约常量）。
pub const FREE_MAX_DEVICES: usize = 8;

/// 免费版最小采集间隔毫秒（task 24 契约常量；< 1000ms 视为违规）。
pub const FREE_MIN_POLL_INTERVAL_MS: u64 = 1_000;

/// 免费版是否允许北向转发（task 24 契约常量：不允许）。
pub const FREE_NORTH_FORWARD: bool = false;

/// 免费版是否允许 OTA（task 24 契约常量：不允许）。
pub const FREE_OTA_ALLOWED: bool = false;

/// 免费基础版限制参数集（常量聚合视图，供调用方展示 / 文案使用）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FreeEditionLimits {
    /// 最大南向设备数。
    pub max_devices: usize,
    /// 最小采集间隔（毫秒）。
    pub min_poll_interval_ms: u64,
    /// 是否允许北向转发。
    pub north_forward: bool,
    /// 是否允许 OTA。
    pub ota_allowed: bool,
}

impl FreeEditionLimits {
    /// 以契约常量构造限制参数集。
    pub const fn new() -> Self {
        Self {
            max_devices: FREE_MAX_DEVICES,
            min_poll_interval_ms: FREE_MIN_POLL_INTERVAL_MS,
            north_forward: FREE_NORTH_FORWARD,
            ota_allowed: FREE_OTA_ALLOWED,
        }
    }
}

impl Default for FreeEditionLimits {
    fn default() -> Self {
        Self::new()
    }
}

/// 单项限制违规（聚合返回，不短路）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LimitViolation {
    /// 南向设备数超免费版配额。
    TooManyDevices {
        /// 实际设备数（去重后）。
        count: usize,
    },
    /// 某设备采集间隔快于免费版下限（逐设备上报）。
    IntervalTooFast {
        /// 违规设备标识。
        device_id: String,
        /// 该设备各点位最小采集间隔（毫秒）。
        interval_ms: u64,
    },
    /// 免费版启用了北向转发（`[[outlets]]` 非空）。
    NorthEnabled,
    /// 免费版启用了 OTA（预留：待 `GatewayConfig` 接入 OTA 配置字段后产出）。
    OtaEnabled,
}

/// 免费版限制校验：聚合**全部**违规项，不短路。
///
/// - `licensed = true`：已激活授权，不做任何限制，恒返回空；
/// - `licensed = false`：按 [`FreeEditionLimits::new`] 逐项检查设备数 / 采集间隔 /
///   北向转发（OTA 待配置字段接入后启用）。
pub fn validate(config: &GatewayConfig, licensed: bool) -> Vec<LimitViolation> {
    if licensed {
        // 授权有效：不做任何限制。
        return Vec::new();
    }

    let limits = FreeEditionLimits::new();
    let mut violations: Vec<LimitViolation> = Vec::new();

    // 1) 设备数：[[points]] 平铺行去重 device_id（BTreeSet 保判定顺序稳定）。
    let mut devices: BTreeSet<&str> = BTreeSet::new();
    for point in &config.points {
        devices.insert(point.device_id.as_str());
    }
    if devices.len() > limits.max_devices {
        violations.push(LimitViolation::TooManyDevices {
            count: devices.len(),
        });
    }

    // 2) 采集间隔：逐设备取各点位 frequency_ms 最小值，< 下限即违规（逐设备上报）。
    let mut min_interval_by_device: BTreeMap<&str, u64> = BTreeMap::new();
    for point in &config.points {
        let entry = min_interval_by_device
            .entry(point.device_id.as_str())
            .or_insert(point.frequency_ms);
        if point.frequency_ms < *entry {
            *entry = point.frequency_ms;
        }
    }
    for (device_id, interval_ms) in &min_interval_by_device {
        if *interval_ms < limits.min_poll_interval_ms {
            violations.push(LimitViolation::IntervalTooFast {
                device_id: (*device_id).to_string(),
                interval_ms: *interval_ms,
            });
        }
    }

    // 3) 北向转发：[[outlets]] 非空即视为启用。
    if !limits.north_forward && !config.outlets.is_empty() {
        violations.push(LimitViolation::NorthEnabled);
    }

    // 4) OTA：GatewayConfig（config.rs task 4 版）暂无 OTA 配置字段（SecuritySection
    //    仅含 tls_cert_path / tls_key_path / web_auth_enabled），故本阶段无法从配置
    //    统计 OTA 开关；OtaEnabled 为预留变体，字段接入后在此聚合，本阶段恒不产出。

    violations
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{GatewaySection, OutletConfig, PointConfig};

    /// 构造带 N 台设备（每台 1 点位、指定间隔）的配置。
    fn config_with_devices(device_count: usize, frequency_ms: u64) -> GatewayConfig {
        let points = (0..device_count)
            .map(|i| PointConfig {
                device_id: format!("dev-{i:03}"),
                point_id: format!("p_{i:03}"),
                protocol: "modbus-tcp".to_string(),
                address: format!("192.168.1.{i}:502"),
                frequency_ms,
            })
            .collect();
        GatewayConfig {
            gateway: GatewaySection::default(),
            outlets: Vec::new(),
            points,
        }
    }

    /// 构造带一路北向出口的配置。
    fn config_with_outlet(mut config: GatewayConfig) -> GatewayConfig {
        config.outlets.push(OutletConfig {
            name: "north-1".to_string(),
            broker: "mqtts://broker.local:8883".to_string(),
            topic_prefix: "telemetry".to_string(),
            qos: 1,
            tls: true,
            encoding: crate::config::OutletEncoding::Protobuf,
        });
        config
    }

    /// QA：契约常量与设计口径一致（防漂移）。
    #[test]
    fn free_limits_constants_match_design() {
        let limits = FreeEditionLimits::new();
        assert_eq!(limits.max_devices, 8);
        assert_eq!(limits.min_poll_interval_ms, 1_000);
        assert!(!limits.north_forward);
        assert!(!limits.ota_allowed);
        // Default 与 new 一致。
        assert_eq!(FreeEditionLimits::default(), limits);
    }

    /// QA：licensed = true 时任何配置都不限制（返回空）。
    #[test]
    fn licensed_bypasses_all_limits() {
        // 20 台设备 + 1ms 间隔 + 北向出口 —— 全部超限，但授权有效 ⇒ 空。
        let mut config = config_with_devices(20, 1);
        config = config_with_outlet(config);
        assert!(validate(&config, true).is_empty());

        // 空配置同样返回空。
        assert!(validate(&GatewayConfig::default(), true).is_empty());
    }

    /// QA：边界 —— 恰好 8 台设备（间隔合规、无北向）⇒ 无违规。
    #[test]
    fn exactly_8_devices_is_allowed() {
        let config = config_with_devices(FREE_MAX_DEVICES, FREE_MIN_POLL_INTERVAL_MS);
        assert!(validate(&config, false).is_empty(), "恰好 8 台必须放行");
    }

    /// QA：9 台设备 ⇒ TooManyDevices{count:9}。
    #[test]
    fn nine_devices_violates_quota() {
        let config = config_with_devices(9, FREE_MIN_POLL_INTERVAL_MS);
        let violations = validate(&config, false);
        assert_eq!(
            violations,
            vec![LimitViolation::TooManyDevices { count: 9 }],
            "9 台仅设备数违规，不得误报间隔/北向"
        );
    }

    /// QA：边界 —— 间隔恰好 1000ms 合规；999ms 违规。
    #[test]
    fn interval_boundary_1000ms() {
        let at_limit = config_with_devices(1, FREE_MIN_POLL_INTERVAL_MS);
        assert!(validate(&at_limit, false).is_empty(), "恰好 1000ms 必须放行");

        let too_fast = config_with_devices(1, FREE_MIN_POLL_INTERVAL_MS - 1);
        assert_eq!(
            validate(&too_fast, false),
            vec![LimitViolation::IntervalTooFast {
                device_id: "dev-000".to_string(),
                interval_ms: FREE_MIN_POLL_INTERVAL_MS - 1
            }]
        );
    }

    /// QA：同设备多点位取最小频率判定（一台快点位即违规该设备）。
    #[test]
    fn per_device_min_interval_is_judged() {
        let mut config = config_with_devices(1, 5_000);
        config.points.push(PointConfig {
            device_id: "dev-000".to_string(),
            point_id: "p_fast".to_string(),
            protocol: "modbus-tcp".to_string(),
            address: "192.168.1.0:502".to_string(),
            frequency_ms: 500,
        });
        assert_eq!(
            validate(&config, false),
            vec![LimitViolation::IntervalTooFast {
                device_id: "dev-000".to_string(),
                interval_ms: 500
            }],
            "设备最小频率 500ms < 1000ms ⇒ 违规"
        );
    }

    /// QA：北向转发 —— outlets 非空 ⇒ NorthEnabled；空 ⇒ 无违规。
    #[test]
    fn north_forward_violation() {
        let clean = config_with_devices(1, FREE_MIN_POLL_INTERVAL_MS);
        assert!(validate(&clean, false).is_empty());

        let with_outlet = config_with_outlet(clean);
        assert_eq!(validate(&with_outlet, false), vec![LimitViolation::NorthEnabled]);
    }

    /// QA：多类违规聚合（不短路）—— 超额设备 + 快间隔 + 北向同时上报。
    #[test]
    fn aggregates_all_violations() {
        // 10 台设备，每台 2 点位：一台合规间隔 + 一台快间隔；外加北向出口。
        let mut config = config_with_devices(10, 2_000);
        config.points.push(PointConfig {
            device_id: "dev-000".to_string(),
            point_id: "p_fast".to_string(),
            protocol: "opcua".to_string(),
            address: "opc.tcp://10.0.0.1:4840".to_string(),
            frequency_ms: 100,
        });
        let config = config_with_outlet(config);

        let violations = validate(&config, false);
        assert!(
            violations.contains(&LimitViolation::TooManyDevices { count: 10 }),
            "须含设备数违规: {violations:?}"
        );
        assert!(
            violations.contains(&LimitViolation::IntervalTooFast {
                device_id: "dev-000".to_string(),
                interval_ms: 100
            }),
            "须含快间隔违规: {violations:?}"
        );
        assert!(
            violations.contains(&LimitViolation::NorthEnabled),
            "须含北向违规: {violations:?}"
        );
        // 10 台中仅 dev-000 违规间隔 ⇒ 间隔违规恰 1 条，共 3 条，聚合不短路。
        assert_eq!(
            violations.len(),
            3,
            "聚合数必须恰为 3（1 超额 + 1 快间隔 + 1 北向）: {violations:?}"
        );
    }

    /// QA：空配置（全默认）⇒ 无违规。
    #[test]
    fn empty_config_has_no_violations() {
        let config = GatewayConfig::default();
        assert!(validate(&config, false).is_empty());
    }

    /// QA：判定顺序稳定 —— 相同配置多次校验结果逐项一致。
    #[test]
    fn violation_order_is_stable() {
        let config = config_with_devices(12, 500);
        let first = validate(&config, false);
        let second = validate(&config, false);
        assert_eq!(first, second, "同一配置两次校验顺序与内容必须一致");
        // 12 台每台单点位 500ms ⇒ 全部 12 台间隔违规（排序按 device_id 升序，BTreeMap）。
        assert_eq!(first.len(), 1 + 12, "1 超额 + 12 快间隔");
        assert!(first[0] == LimitViolation::TooManyDevices { count: 12 });
    }
}

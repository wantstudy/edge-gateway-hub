//! 数据处理内核（计划 task 15，Wave 2b）。
//!
//! **边界声明**（与计划一致，越界即返工）：
//! - 只做**逐点、无规则的本地处理**：点位映射 → 单位换算 → 死区过滤 → 时间戳统一；
//! - **不做**规则引擎（规则解析 / 匹配 / 动作编排 = task 20/37）；
//! - **不做**数据类型解码与质量码语义规范化（= task 53），`quality` **原值透传**；
//! - **不做**公式求值（= task 70），但必须保证**单位换算先于公式**——本层输出即工程单位值。
//!
//! 数据流位置：驱动 [`crate::driver::PointSample`]（原始字节）→ task 53 解码为数值 + quality
//! → 本层 [`RawSample`]（已解码数值）→ [`ProcessedSample`]（工程单位值）
//! → task 62 编码为 `DataPoint` 字节 / JSON。
//!
//! ## 关键设计决策
//!
//! 1. **换算在死区之前**：死区阈值是工程单位语义，必须在换算后的值上判定
//!    （否则 Pa→kPa 时阈值会被放大 1000 倍，过滤行为与配置不符）。
//! 2. **死区以「上次输出值」为基准**（非上次输入值）：符合 QA 场景
//!    `36.49 / 36.50 / 36.51`（阈值 0.1）→ 仅 1 次输出。
//! 3. **quality 变化强制输出**：死区不得吞掉质量状态跃变（如 GOOD→BAD）。
//!    本层只比较相等性、不解释 quality 含义，属「透传」而非「语义规范化」。
//! 4. **时钟可注入**：`process_at` 显式传入采集时间戳，单测可确定性断言；
//!    `process` 走系统时钟。设备原始时间戳（若有）原样保留，不做覆盖。
//! 5. **死区阈值按 `<` 判定**：差值恰好等于阈值即输出（`>=` 阈值视为有效变化）。
//!    `NaN` 永不被死区吞掉（比较恒为 false），由 quality 表达其有效性。

use std::collections::HashMap;
use std::time::{SystemTime, UNIX_EPOCH};

use protocol_proto::Quality;

use crate::error::{DaemonError, DaemonResult};

// ---- 配置 ----

/// 点位处理配置（点位表的一行处理视图）。
#[derive(Debug, Clone, PartialEq)]
pub struct PointConfig {
    /// 源点位标识（驱动侧地址 / 标识，如 `"40001"`）。
    pub source_id: String,
    /// 目标点位标识（点位表主键，北向 `point_id`，对外不暴露内部 id）。
    pub point_id: String,
    /// 设备标识（北向 `device_id`）。
    pub device_id: String,
    /// 换算后的工程单位（如 `"kPa"`）。
    pub unit: String,
    /// 换算系数：`out = in * scale + offset`（默认 `1.0`）。
    pub scale: f64,
    /// 换算偏移（默认 `0.0`）。
    pub offset: f64,
    /// 绝对死区阈值（**工程单位**，≥ 0；`0.0` = 不过滤）。
    pub deadband: f64,
}

impl PointConfig {
    /// 直通配置：不换算、不过滤（保留原单位）。
    pub fn passthrough(source_id: &str, point_id: &str, device_id: &str, unit: &str) -> Self {
        Self {
            source_id: source_id.to_string(),
            point_id: point_id.to_string(),
            device_id: device_id.to_string(),
            unit: unit.to_string(),
            scale: 1.0,
            offset: 0.0,
            deadband: 0.0,
        }
    }

    /// 配置自检：`new` 时逐条校验，非法即 [`DaemonError::ConfigError`]（错误码 2000）。
    fn validate(&self) -> DaemonResult<()> {
        let bad = |field: &str| {
            DaemonError::ConfigError(format!("point config: {field} must not be empty"))
        };
        if self.source_id.is_empty() {
            return Err(bad("source_id"));
        }
        if self.point_id.is_empty() {
            return Err(bad("point_id"));
        }
        if self.device_id.is_empty() {
            return Err(bad("device_id"));
        }
        if !self.scale.is_finite() {
            return Err(DaemonError::ConfigError(format!(
                "point config {}: scale must be finite, got {}",
                self.point_id, self.scale
            )));
        }
        if !self.offset.is_finite() {
            return Err(DaemonError::ConfigError(format!(
                "point config {}: offset must be finite, got {}",
                self.point_id, self.offset
            )));
        }
        if !self.deadband.is_finite() || self.deadband < 0.0 {
            return Err(DaemonError::ConfigError(format!(
                "point config {}: deadband must be finite and >= 0, got {}",
                self.point_id, self.deadband
            )));
        }
        Ok(())
    }
}

// ---- 输入 / 输出 ----

/// 处理输入：已解码的数值采样（解码与 quality 语义归 task 53，本层不解释）。
#[derive(Debug, Clone, PartialEq)]
pub struct RawSample {
    /// 源点位标识（与 [`PointConfig::source_id`] 对应）。
    pub source_id: String,
    /// 已解码数值（原始工程单位换算前的量值）。
    pub value: f64,
    /// 质量码（**原值透传**）。
    pub quality: Quality,
    /// 设备侧原始时间戳（Unix 纳秒）；设备不提供时为 `None`。
    pub device_ts_ns: Option<i64>,
}

/// 处理输出：工程单位值 + 双时间戳（可直接供 task 62 编码为 `DataPoint`）。
#[derive(Debug, Clone, PartialEq)]
pub struct ProcessedSample {
    /// 设备标识。
    pub device_id: String,
    /// 点位标识。
    pub point_id: String,
    /// 换算后的工程单位值。
    pub value: f64,
    /// 工程单位。
    pub unit: String,
    /// 设备原始时间戳（若有，原样保留）。
    pub device_ts_ns: Option<i64>,
    /// 网关采集时间戳（Unix 纳秒）。
    pub collected_ts_ns: i64,
    /// 质量码（原值透传）。
    pub quality: Quality,
}

// ---- 处理器 ----

/// 逐点数据处理器（有状态：持有死区基准）。
///
/// 状态仅包含「各点位上次输出值」，点位配置热重载 / 点位删除时调用 [`Self::reset`] 清除，
/// 避免旧基准污染新配置（阈值变化后首帧应按「首次」输出）。
#[derive(Debug, Default)]
pub struct DataProcessor {
    configs: HashMap<String, PointConfig>,
    last_reported: HashMap<String, (f64, Quality)>,
}

impl DataProcessor {
    /// 按点位配置构建；重复 `source_id` 或非法配置返回 [`DaemonError::ConfigError`]。
    pub fn new(configs: Vec<PointConfig>) -> DaemonResult<Self> {
        let mut map = HashMap::with_capacity(configs.len());
        for cfg in configs {
            cfg.validate()?;
            let source = cfg.source_id.clone();
            if map.insert(cfg.source_id.clone(), cfg).is_some() {
                return Err(DaemonError::ConfigError(format!(
                    "duplicate source_id {source:?} in point configs (one source maps to one point)"
                )));
            }
        }
        Ok(Self {
            configs: map,
            last_reported: HashMap::new(),
        })
    }

    /// 处理一帧（采集时间戳取系统时钟）。
    pub fn process(&mut self, sample: RawSample) -> DaemonResult<Option<ProcessedSample>> {
        let now = SystemTime::now().duration_since(UNIX_EPOCH).map_err(|e| {
            DaemonError::ConfigError(format!("system clock before unix epoch: {e}"))
        })?;
        let collected = i64::try_from(now.as_secs())
            .unwrap_or(i64::MAX)
            .saturating_mul(1_000_000_000)
            .saturating_add(i64::from(now.subsec_nanos()));
        self.process_at(sample, collected)
    }

    /// 处理一帧并显式指定采集时间戳（单测 / 回放用）。
    ///
    /// 返回 `Ok(None)` 表示被死区过滤（不产出）；`Err` 表示配置缺失等需调用方决策的问题。
    pub fn process_at(
        &mut self,
        sample: RawSample,
        collected_ts_ns: i64,
    ) -> DaemonResult<Option<ProcessedSample>> {
        let cfg = self.configs.get(&sample.source_id).ok_or_else(|| {
            DaemonError::ConfigError(format!(
                "no point config for source {:?} (point table missing this source)",
                sample.source_id
            ))
        })?;

        // 1) 单位换算：必须先于死区判定与公式求值（task 70 前置）。
        let value = sample.value * cfg.scale + cfg.offset;

        // 2) 死区过滤：以「上次输出值」为基准；quality 变化强制输出。
        if let Some((last_value, last_quality)) = self.last_reported.get(&cfg.point_id) {
            let within_deadband = (value - *last_value).abs() < cfg.deadband;
            if *last_quality == sample.quality && within_deadband {
                return Ok(None);
            }
        }

        let out = ProcessedSample {
            device_id: cfg.device_id.clone(),
            point_id: cfg.point_id.clone(),
            value,
            unit: cfg.unit.clone(),
            device_ts_ns: sample.device_ts_ns,
            collected_ts_ns,
            quality: sample.quality,
        };
        self.last_reported
            .insert(cfg.point_id.clone(), (value, sample.quality));
        Ok(Some(out))
    }

    /// 清除某点位的死区基准（配置热重载 / 点位删除后调用）。
    pub fn reset(&mut self, point_id: &str) {
        self.last_reported.remove(point_id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const T0: i64 = 1_700_000_000_000_000_000;

    fn cfg_pa_to_kpa() -> PointConfig {
        PointConfig {
            source_id: "40001".to_string(),
            point_id: "line1_m1_pressure".to_string(),
            device_id: "line1_m1".to_string(),
            unit: "kPa".to_string(),
            scale: 0.001,
            offset: 0.0,
            deadband: 0.0,
        }
    }

    fn raw(source_id: &str, value: f64) -> RawSample {
        RawSample {
            source_id: source_id.to_string(),
            value,
            quality: Quality::Good,
            device_ts_ns: None,
        }
    }

    // ---- QA 场景 ----

    /// Happy: 输入 1000 Pa → 换算为 1 kPa（计划 QA 场景 1）。
    #[test]
    fn unit_conversion_pa_to_kpa() {
        let mut p = DataProcessor::new(vec![cfg_pa_to_kpa()]).expect("config");
        let out = p
            .process_at(raw("40001", 1000.0), T0)
            .expect("ok")
            .expect("not filtered");
        assert_eq!(out.value, 1.0);
        assert_eq!(out.unit, "kPa");
        assert_eq!(out.point_id, "line1_m1_pressure");
    }

    /// 换算同时支持系数与偏移（如 ℃ → ℉：100 → 212）。
    #[test]
    fn unit_conversion_scale_and_offset() {
        let mut p = DataProcessor::new(vec![PointConfig {
            source_id: "t1".into(),
            point_id: "temp_f".into(),
            device_id: "m1".into(),
            unit: "°F".into(),
            scale: 1.8,
            offset: 32.0,
            deadband: 0.0,
        }])
        .expect("config");
        let out = p
            .process_at(raw("t1", 100.0), T0)
            .expect("ok")
            .expect("out");
        assert!((out.value - 212.0).abs() < 1e-9, "got {}", out.value);
    }

    /// Error: 36.49 / 36.50 / 36.51，死区 0.1 → 仅 1 次输出（计划 QA 场景 2）。
    #[test]
    fn deadband_suppresses_small_changes() {
        let mut p = DataProcessor::new(vec![PointConfig {
            source_id: "t".into(),
            point_id: "temp".into(),
            device_id: "m1".into(),
            unit: "°C".into(),
            scale: 1.0,
            offset: 0.0,
            deadband: 0.1,
        }])
        .expect("config");

        let mut emitted = 0;
        for v in [36.49, 36.50, 36.51] {
            if p.process_at(raw("t", v), T0).expect("ok").is_some() {
                emitted += 1;
            }
        }
        assert_eq!(emitted, 1, "only the first sample passes the deadband");
    }

    /// 变化超过阈值则输出（第二帧 36.70 与基准 36.49 差 0.21 > 0.1）。
    #[test]
    fn deadband_passes_when_threshold_exceeded() {
        let mut p = DataProcessor::new(vec![PointConfig {
            source_id: "t".into(),
            point_id: "temp".into(),
            device_id: "m1".into(),
            unit: "°C".into(),
            scale: 1.0,
            offset: 0.0,
            deadband: 0.1,
        }])
        .expect("config");

        assert!(p.process_at(raw("t", 36.49), T0).expect("ok").is_some());
        let out = p.process_at(raw("t", 36.70), T0).expect("ok");
        assert!(out.is_some(), "0.21 > 0.1 must pass");
        assert!((out.expect("out").value - 36.70).abs() < 1e-9);
    }

    /// 差值恰好等于阈值 → 输出（判定为 `<`，阈值本身算有效变化）。
    #[test]
    fn deadband_boundary_value_is_emitted() {
        let mut p = DataProcessor::new(vec![PointConfig {
            source_id: "t".into(),
            point_id: "temp".into(),
            device_id: "m1".into(),
            unit: "°C".into(),
            scale: 1.0,
            offset: 0.0,
            deadband: 0.1,
        }])
        .expect("config");
        assert!(p.process_at(raw("t", 36.50), T0).expect("ok").is_some());
        assert!(
            p.process_at(raw("t", 36.60), T0).expect("ok").is_some(),
            "delta == deadband is emitted"
        );
    }

    /// 死区不得吞掉质量状态跃变（GOOD → BAD，值不变）。
    #[test]
    fn quality_change_forces_output_within_deadband() {
        let mut p = DataProcessor::new(vec![PointConfig {
            source_id: "t".into(),
            point_id: "temp".into(),
            device_id: "m1".into(),
            unit: "°C".into(),
            scale: 1.0,
            offset: 0.0,
            deadband: 0.5,
        }])
        .expect("config");

        assert!(p.process_at(raw("t", 36.5), T0).expect("ok").is_some());
        let bad = RawSample {
            source_id: "t".into(),
            value: 36.5,
            quality: Quality::Bad,
            device_ts_ns: None,
        };
        let out = p
            .process_at(bad, T0)
            .expect("ok")
            .expect("quality change emits");
        assert_eq!(out.quality, Quality::Bad, "quality passed through verbatim");
    }

    /// **顺序验证**：死区在换算之后判定（阈值是工程单位语义）。
    /// 1000 / 1001 Pa → 1.000 / 1.001 kPa，死区 0.01 kPa：
    /// 若死区在换算前判定（差 1 Pa > 0.01）会误输出，换算后（差 0.001 < 0.01）应过滤。
    #[test]
    fn deadband_is_evaluated_after_conversion() {
        let mut p = DataProcessor::new(vec![PointConfig {
            source_id: "40001".into(),
            point_id: "press".into(),
            device_id: "m1".into(),
            unit: "kPa".into(),
            scale: 0.001,
            offset: 0.0,
            deadband: 0.01,
        }])
        .expect("config");

        assert!(p
            .process_at(raw("40001", 1000.0), T0)
            .expect("ok")
            .is_some());
        assert!(
            p.process_at(raw("40001", 1001.0), T0)
                .expect("ok")
                .is_none(),
            "1 Pa delta = 0.001 kPa < 0.01 kPa deadband → filtered"
        );
    }

    /// NaN 不被死区吞掉（比较恒 false），有效性由 quality 表达。
    #[test]
    fn nan_is_not_suppressed() {
        let mut p = DataProcessor::new(vec![PointConfig {
            source_id: "t".into(),
            point_id: "temp".into(),
            device_id: "m1".into(),
            unit: "°C".into(),
            scale: 1.0,
            offset: 0.0,
            deadband: 1e9,
        }])
        .expect("config");
        assert!(p.process_at(raw("t", 1.0), T0).expect("ok").is_some());
        let nan = RawSample {
            source_id: "t".into(),
            value: f64::NAN,
            quality: Quality::Bad,
            device_ts_ns: None,
        };
        assert!(p.process_at(nan, T0).expect("ok").is_some());
    }

    /// 点位映射：源标识 → 目标 point_id + device_id（对外不暴露内部 id）。
    #[test]
    fn point_mapping_source_to_target() {
        let mut p = DataProcessor::new(vec![cfg_pa_to_kpa()]).expect("config");
        let out = p
            .process_at(raw("40001", 2500.0), T0)
            .expect("ok")
            .expect("out");
        assert_eq!(out.point_id, "line1_m1_pressure");
        assert_eq!(out.device_id, "line1_m1");
        assert_eq!(out.value, 2.5);
    }

    /// 时间戳统一：保留设备原始时间戳 + 打上采集时间戳（互不覆盖）。
    #[test]
    fn timestamps_keep_device_and_collected() {
        let mut p = DataProcessor::new(vec![cfg_pa_to_kpa()]).expect("config");
        let s = RawSample {
            source_id: "40001".into(),
            value: 1000.0,
            quality: Quality::Good,
            device_ts_ns: Some(1_699_999_999_000_000_000),
        };
        let out = p.process_at(s, T0).expect("ok").expect("out");
        assert_eq!(out.device_ts_ns, Some(1_699_999_999_000_000_000));
        assert_eq!(out.collected_ts_ns, T0);
    }

    /// 未配置的源点位 → ConfigError（由调用方决定跳过或告警，不静默丢弃）。
    #[test]
    fn unknown_source_is_config_error() {
        let mut p = DataProcessor::new(vec![cfg_pa_to_kpa()]).expect("config");
        let err = p
            .process_at(raw("99999", 1.0), T0)
            .expect_err("unknown source must not be silently dropped");
        assert!(
            matches!(err, DaemonError::ConfigError(_)),
            "expected ConfigError, got {err:?}"
        );
    }

    /// `reset` 清除死区基准（配置热重载后首帧按「首次」输出）。
    #[test]
    fn reset_clears_deadband_baseline() {
        let mut p = DataProcessor::new(vec![PointConfig {
            source_id: "t".into(),
            point_id: "temp".into(),
            device_id: "m1".into(),
            unit: "°C".into(),
            scale: 1.0,
            offset: 0.0,
            deadband: 0.5,
        }])
        .expect("config");
        assert!(p.process_at(raw("t", 36.5), T0).expect("ok").is_some());
        assert!(p.process_at(raw("t", 36.6), T0).expect("ok").is_none());
        p.reset("temp");
        assert!(
            p.process_at(raw("t", 36.6), T0).expect("ok").is_some(),
            "after reset the same value is re-emitted"
        );
    }

    // ---- 配置校验 ----

    /// 非法配置在 `new` 阶段被拒（死区负值 / 空字段 / 非有限系数 / 重复源）。
    #[test]
    fn invalid_configs_are_rejected() {
        let cases: Vec<PointConfig> = vec![
            PointConfig {
                deadband: -0.1,
                ..cfg_pa_to_kpa()
            },
            PointConfig {
                point_id: String::new(),
                ..cfg_pa_to_kpa()
            },
            PointConfig {
                scale: f64::NAN,
                ..cfg_pa_to_kpa()
            },
            PointConfig {
                source_id: String::new(),
                ..cfg_pa_to_kpa()
            },
            PointConfig {
                offset: f64::INFINITY,
                ..cfg_pa_to_kpa()
            },
        ];
        for cfg in cases {
            let err = DataProcessor::new(vec![cfg]).expect_err("invalid config must be rejected");
            assert!(
                matches!(err, DaemonError::ConfigError(_)),
                "expected ConfigError, got {err:?}"
            );
        }

        let dup = DataProcessor::new(vec![cfg_pa_to_kpa(), cfg_pa_to_kpa()]);
        assert!(dup.is_err(), "duplicate source_id must be rejected");
    }

    /// 直通配置：不换算、不过滤。
    #[test]
    fn passthrough_config_keeps_raw_value() {
        let cfg = PointConfig::passthrough("40001", "p", "m1", "°C");
        assert_eq!(cfg.scale, 1.0);
        assert_eq!(cfg.offset, 0.0);
        assert_eq!(cfg.deadband, 0.0);
        let mut p = DataProcessor::new(vec![cfg]).expect("config");
        assert!(p.process_at(raw("40001", 12.0), T0).expect("ok").is_some());
        assert!(
            p.process_at(raw("40001", 12.000_001), T0)
                .expect("ok")
                .is_some(),
            "deadband 0 means no filtering"
        );
    }
}

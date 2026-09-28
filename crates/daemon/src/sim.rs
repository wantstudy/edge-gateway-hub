//! 点位模拟源（`sim_*` 配置的**真实执行体**）。
//!
//! # 为什么存在（诚实降级红线）
//!
//! `PointConfig` 的 `sim_enabled` / `sim_mode` / `sim_min` / `sim_max` / `sim_dec`
//! 承担「没有南向设备时也能验证采集 → 北向全链路」的职责，管理页面上是一个可
//! 拨动的开关。在此之前这几个字段**只被 schema 解析与写接口回显，没有任何执行
//! 体消费**：打开仿真后南向仍发真实请求（现场表现为「开了仿真还是连不上设备」）。
//! 这属于「UI 有开关、后端无能力」的假能力，本模块补齐执行体。
//!
//! # 语义（与前端文案严格一致）
//!
//! - `sim_enabled = true` → 该点位**不发起任何南向读**，样本由本模块合成；
//!   质量码 [`Quality::Good`]，`device_ts_ns = None`（与真实读同口径：采集时刻
//!   由采集侧承载，设备不提供时间戳）。
//! - `sim_mode`：`random`（min~max 均匀随机，缺省）| `fixed`（恒定 `min`）|
//!   `ramp`（`min → max` 线性循环，100 拍一周期）。**未知值一律拒绝** —— 该组
//!   轮询显式报 `ConfigError`，绝不静默退回真实读（否则「仿真」会变成撒谎）。
//! - `sim_min` / `sim_max` 缺省 `0.0` / `100.0`；`min > max` 或非有限值 = 配置错误。
//! - `sim_dec` 缺省 2（小数位，上限 [`MAX_SIM_DEC`]）。
//!
//! # 确定性（可复现）
//!
//! 取值由 `splitmix64(seed(point_id) ^ tick)` 派生，**无内部可变状态**：同一
//! (点位, 拍号) 恒得同一值，便于单测与现场问题复现；也不引入任何 RNG 依赖
//! （依赖红线：保持纯 Rust 零新增）。

/// 模拟值小数位上限（防 `10^dec` 溢出为 `inf`）。
pub const MAX_SIM_DEC: u32 = 6;
/// `sim_min` 缺省下限。
pub const DEFAULT_SIM_MIN: f64 = 0.0;
/// `sim_max` 缺省上限。
pub const DEFAULT_SIM_MAX: f64 = 100.0;
/// `sim_dec` 缺省小数位。
pub const DEFAULT_SIM_DEC: u32 = 2;
/// `ramp` 模式一圈的拍数（确定性周期）。
const RAMP_PERIOD_TICKS: u64 = 100;

/// 模拟波形。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SimMode {
    /// min~max 均匀随机（缺省）。
    Random,
    /// 恒定 `min`。
    Fixed,
    /// `min → max` 线性循环。
    Ramp,
}

impl SimMode {
    /// 解析配置字面量（大小写不敏感）。
    ///
    /// `None` / 空串 = 缺省 [`SimMode::Random`]；**未知值返回 `None`**，调用方必须
    /// 据此显式报错（不得退回真实读）。
    #[must_use]
    pub fn parse(raw: Option<&str>) -> Option<Self> {
        match raw.map(str::trim).filter(|text| !text.is_empty()) {
            None => Some(SimMode::Random),
            Some(text) => match text.to_ascii_lowercase().as_str() {
                "random" => Some(SimMode::Random),
                "fixed" => Some(SimMode::Fixed),
                "ramp" => Some(SimMode::Ramp),
                _ => None,
            },
        }
    }

    /// 规范字面量（日志 / 审计口径）。
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            SimMode::Random => "random",
            SimMode::Fixed => "fixed",
            SimMode::Ramp => "ramp",
        }
    }
}

/// 单点位的模拟规格（已校验）。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SimSpec {
    pub mode: SimMode,
    pub min: f64,
    pub max: f64,
    pub dec: u32,
}

impl SimSpec {
    /// 由点位配置字段构造；`Err` = **配置非法**（调用方须显式报错）。
    ///
    /// # Errors
    /// 未知 `sim_mode` 或数值区间非法（`min > max` / 非有限值）。
    pub fn from_fields(
        sim_mode: Option<&str>,
        sim_min: Option<f64>,
        sim_max: Option<f64>,
        sim_dec: Option<u32>,
    ) -> Result<Self, String> {
        let mode = SimMode::parse(sim_mode).ok_or_else(|| {
            format!(
                "unknown sim_mode {:?} (supported: random | fixed | ramp)",
                sim_mode.unwrap_or("")
            )
        })?;
        let min = sim_min.unwrap_or(DEFAULT_SIM_MIN);
        let max = sim_max.unwrap_or(DEFAULT_SIM_MAX);
        if !min.is_finite() || !max.is_finite() {
            return Err(format!(
                "sim_min / sim_max must be finite numbers (got min={min}, max={max})"
            ));
        }
        if min > max {
            return Err(format!("sim_min ({min}) must not exceed sim_max ({max})"));
        }
        Ok(Self {
            mode,
            min,
            max,
            dec: sim_dec.unwrap_or(DEFAULT_SIM_DEC).min(MAX_SIM_DEC),
        })
    }

    /// 第 `tick` 拍的模拟值（确定性；`seed` 由 [`seed_for`] 提供）。
    #[must_use]
    pub fn value_at(&self, seed: u64, tick: u64) -> f64 {
        let raw = match self.mode {
            SimMode::Fixed => self.min,
            SimMode::Random => {
                // 高 53 位 → [0,1) 均匀；每拍独立哈希，无状态可复现。
                let hashed = splitmix64(seed ^ tick.wrapping_mul(0x9E37_79B9_7F4A_7C15));
                let unit = (hashed >> 11) as f64 / (1u64 << 53) as f64;
                self.min + unit * (self.max - self.min)
            }
            SimMode::Ramp => {
                let unit = (tick % RAMP_PERIOD_TICKS) as f64 / RAMP_PERIOD_TICKS as f64;
                self.min + unit * (self.max - self.min)
            }
        };
        round_to_dec(raw, self.dec)
    }
}

/// 点位标识 → 稳定种子（FNV-1a 64；同一 `point_id` 跨进程重启同种子）。
#[must_use]
pub fn seed_for(point_id: &str) -> u64 {
    let mut hash: u64 = 0xCBF2_9CE4_8422_2325;
    for byte in point_id.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01B3);
    }
    hash
}

/// splitmix64（无依赖、确定性 PRNG 的一步）。
fn splitmix64(seed: u64) -> u64 {
    let x = seed.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut z = x;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// 按小数位四舍五入（`dec` 已在上游截到 [`MAX_SIM_DEC`]；非有限值原样返回）。
fn round_to_dec(value: f64, dec: u32) -> f64 {
    if !value.is_finite() {
        return value;
    }
    let factor = 10f64.powi(dec.min(MAX_SIM_DEC) as i32);
    (value * factor).round() / factor
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mode_parse_defaults_and_rejects_unknown() {
        assert_eq!(SimMode::parse(None), Some(SimMode::Random));
        assert_eq!(SimMode::parse(Some("")), Some(SimMode::Random));
        assert_eq!(SimMode::parse(Some("  RANDOM ")), Some(SimMode::Random));
        assert_eq!(SimMode::parse(Some("fixed")), Some(SimMode::Fixed));
        assert_eq!(SimMode::parse(Some("Ramp")), Some(SimMode::Ramp));
        // 未知波形必须被拒绝（调用方据此报错，不得退回真实读）。
        assert_eq!(SimMode::parse(Some("sine")), None);
        assert_eq!(SimMode::parse(Some("1")), None);
    }

    #[test]
    fn spec_defaults_match_documented_values() {
        let spec = SimSpec::from_fields(None, None, None, None).expect("defaults are valid");
        assert_eq!(spec.mode, SimMode::Random);
        assert_eq!(spec.min, DEFAULT_SIM_MIN);
        assert_eq!(spec.max, DEFAULT_SIM_MAX);
        assert_eq!(spec.dec, DEFAULT_SIM_DEC);
    }

    #[test]
    fn spec_rejects_inverted_or_non_finite_range() {
        assert!(SimSpec::from_fields(Some("random"), Some(10.0), Some(5.0), None).is_err());
        assert!(SimSpec::from_fields(Some("random"), Some(f64::NAN), Some(5.0), None).is_err());
        assert!(
            SimSpec::from_fields(Some("random"), Some(0.0), Some(f64::INFINITY), None).is_err()
        );
        // 相等区间合法（恒定值）。
        assert!(SimSpec::from_fields(Some("fixed"), Some(3.0), Some(3.0), None).is_ok());
    }

    #[test]
    fn spec_caps_sim_dec_at_max() {
        let spec =
            SimSpec::from_fields(Some("fixed"), Some(1.0), Some(1.0), Some(99)).expect("valid");
        assert_eq!(spec.dec, MAX_SIM_DEC);
    }

    #[test]
    fn fixed_mode_is_constant_min() {
        let spec =
            SimSpec::from_fields(Some("fixed"), Some(12.345), Some(99.0), Some(2)).expect("valid");
        let seed = seed_for("40001");
        for tick in 0..8 {
            assert_eq!(
                spec.value_at(seed, tick),
                12.35,
                "fixed must round min to dec"
            );
        }
    }

    #[test]
    fn random_mode_is_bounded_and_deterministic() {
        let spec =
            SimSpec::from_fields(Some("random"), Some(-5.0), Some(5.0), Some(3)).expect("valid");
        let seed = seed_for("40001");
        let first: Vec<f64> = (0..32).map(|t| spec.value_at(seed, t)).collect();
        // 确定性：同 seed 同 tick 恒同值。
        let again: Vec<f64> = (0..32).map(|t| spec.value_at(seed, t)).collect();
        assert_eq!(first, again, "same (point, tick) must yield the same value");
        // 边界闭合。
        for v in &first {
            assert!((-5.0..=5.0).contains(v), "value {v} out of [min,max]");
        }
        // 不同点位种子应产生不同序列（避免所有点同波形）。
        let other: Vec<f64> = (0..32)
            .map(|t| spec.value_at(seed_for("40002"), t))
            .collect();
        assert_ne!(first, other, "distinct points must not share one waveform");
        // 序列不应恒定（否则 random 退化成 fixed）。
        assert!(first.windows(2).any(|w| w[0] != w[1]));
    }

    #[test]
    fn ramp_mode_cycles_between_bounds() {
        let spec =
            SimSpec::from_fields(Some("ramp"), Some(0.0), Some(10.0), Some(1)).expect("valid");
        let seed = seed_for("40001");
        assert_eq!(spec.value_at(seed, 0), 0.0);
        assert_eq!(spec.value_at(seed, 50), 5.0);
        assert_eq!(spec.value_at(seed, 99), 9.9);
        // 周期回绕。
        assert_eq!(spec.value_at(seed, 100), 0.0);
    }

    #[test]
    fn seed_for_is_stable_and_distinguishes_points() {
        assert_eq!(seed_for("40001"), seed_for("40001"));
        assert_ne!(seed_for("40001"), seed_for("40002"));
        assert_ne!(seed_for(""), seed_for("a"));
    }

    #[test]
    fn round_to_dec_handles_non_finite() {
        assert!(round_to_dec(f64::NAN, 2).is_nan());
        assert_eq!(round_to_dec(f64::INFINITY, 2), f64::INFINITY);
        assert_eq!(round_to_dec(1.005, 2), 1.0); // 浮点表示决定的下取整，仅锁定行为
        assert_eq!(round_to_dec(2.5, 0), 3.0);
    }
}

//! 统一数据类型解码器与质量码规范（计划 task 53，Wave 7）。
//!
//! **边界声明**（与计划一致，越界即返工）：
//! - 本模块只做「原始寄存器字节 → 强类型值 + 质量码」的**纯函数解码**，
//!   不做采集调度（task 16）、不做单位换算与死区（task 15）、不做公式求值（task 70）；
//! - 驱动侧**不得**自带 quality 语义；各驱动只把原生状态码交给
//!   [`Quality::from_driver_fault`] 归一（映射表集中在本模块）；
//! - task 15（`pipeline.rs`）目前只透传 `quality` 原值，改造时调用本模块
//!   [`Quality::to_wire`] 做规范化，本模块不反向依赖 pipeline。
//!
//! ## 解码模型
//!
//! 1. **寄存器视角**：输入 `bytes` 为**传输顺序**的寄存器负载，每个 16 位寄存器在线路上
//!    为大端（Modbus 惯例）。设寄存器序列 `R0, R1, ...`，每个寄存器含高字节 / 低字节。
//! 2. **字节序组合**（32 位四种，64 位同构外推）：
//!
//!    | 变体 | 32 位字节序 | 64 位字节序 | 含义 |
//!    |---|---|---|---|
//!    | [`ByteOrder::Abcd`] | `ABCD` | `ABCDEFGH` | 原样（大端，S7 默认） |
//!    | [`ByteOrder::Cdab`] | `CDAB` | `CDABGHEF` | 相邻 16 位寄存器互换 |
//!    | [`ByteOrder::Badc`] | `BADC` | `BADCFEHG` | 寄存器内高低字节互换 |
//!    | [`ByteOrder::Dcba`] | `DCBA` | `HGFEDCBA` | 整体逆序（小端，MC 默认） |
//!
//!    单一寄存器（2 字节）时 `CDAB` 无意义，退化为原样（不做越界访问）。
//! 3. **位号约定**：`bit_index` 以「按字节序组合后的整数」的 **LSB 为 0** 编号，
//!    布尔取 1 位、`bits` 从 `bit_index` 起取 `bit_width` 位。
//! 4. **缩放因子**：`scaled = raw * scale + offset`，只在数值类型上生效；
//!    原始值仍以 [`DecodedValue::Uint`] 保留 `u64` 全精度（不先降精度到 `f64`）。
//!
//! ## 质量码规范
//!
//! [`Quality`] 为统一质量码。`severity()` 数值越大越差，代表「信息缺失程度」递增：
//! `Good < Uncertain < OutOfRange < CalcFailed < Bad < Timeout < CommError`。
//! **多输入取最差**（[`Quality::worst`] / [`Quality::worst_of`]）是本规范的核心继承规则：
//! 派生量（task 70 公式点、聚合点）的质量 = 所有输入质量的 `worst()`。
//!
//! `CalcFailed` 语义（本任务定义，task 70 只使用）：计算点因**输入缺失 / 输入超时 /
//! 输入非数值 / 除零 / 计算结果为 NaN 或 ±Inf** 时置该质量码；它不是通信故障，
//! 而是「输入齐但算不出来」，因此优于 `Timeout` / `CommError`、劣于 `OutOfRange`。

use protocol_proto::Quality as WireQuality;

use crate::error::{DaemonError, DaemonResult};

/// 解码器支持的单个寄存器负载上限（64 位 = 4 个寄存器 = 8 字节）。
const MAX_BYTES: usize = 8;

/// IEEE754 double 可精确表示的整数上限 `2^53 - 1`；超出必须在 JSON 路径编码为字符串。
const I64_SAFE_MAX: i64 = 9_007_199_254_740_991;
/// 与 [`I64_SAFE_MAX`] 对应的无符号上限。
const U64_SAFE_MAX: u64 = 9_007_199_254_740_991;

// ---- 数据类型 ----

/// 点位数据类型（计划 task 53：bool / int8-64 / uint8-64 / float32 / float64 / string / bitset）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DataType {
    /// 布尔（从寄存器按位提取）。
    Bool,
    /// 有符号 8 位。
    Int8,
    /// 有符号 16 位。
    Int16,
    /// 有符号 32 位（跨 2 个寄存器）。
    Int32,
    /// 有符号 64 位（跨 4 个寄存器）。
    Int64,
    /// 无符号 8 位。
    Uint8,
    /// 无符号 16 位（单寄存器）。
    Uint16,
    /// 无符号 32 位（跨 2 个寄存器）。
    Uint32,
    /// 无符号 64 位计数器（跨 4 个寄存器，JSON 须编码为字符串）。
    Uint64,
    /// IEEE754 单精度（跨 2 个寄存器）。
    Float32,
    /// IEEE754 双精度（跨 4 个寄存器）。
    Float64,
    /// 字符串（定长 / 结束符 / 整段，见 [`StringMode`]）。
    Text,
    /// 位串：从 `bit_index` 起取 `bit_width` 位构成无符号整数。
    Bits,
}

impl DataType {
    /// 定长数值类型的字节宽度；`Bool` / `Bits` / `Text` 返回 `None`（长度由位号或
    /// [`StringMode`] 决定）。
    pub const fn fixed_size(self) -> Option<usize> {
        match self {
            Self::Int8 | Self::Uint8 => Some(1),
            Self::Int16 | Self::Uint16 => Some(2),
            Self::Int32 | Self::Uint32 | Self::Float32 => Some(4),
            Self::Int64 | Self::Uint64 | Self::Float64 => Some(8),
            Self::Bool | Self::Bits | Self::Text => None,
        }
    }

    /// 是否为有符号整数类型（决定符号扩展行为）。
    pub const fn is_signed_int(self) -> bool {
        matches!(self, Self::Int8 | Self::Int16 | Self::Int32 | Self::Int64)
    }

    /// 是否为无符号整数类型（含 `Bits`）。
    pub const fn is_unsigned_int(self) -> bool {
        matches!(
            self,
            Self::Uint8 | Self::Uint16 | Self::Uint32 | Self::Uint64 | Self::Bits
        )
    }

    /// 是否为 IEEE754 浮点类型。
    pub const fn is_float(self) -> bool {
        matches!(self, Self::Float32 | Self::Float64)
    }
}

// ---- 字节序 ----

/// 字节序组合（Modbus 32 位四大易错组合 + 64 位同构外推）。
///
/// 命名取自「以 ABCD 表示 4 个字节的传输顺序」的工业惯例；64 位以 8 字节序列
/// `ABCDEFGH` 同理外推（见模块文档表格）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ByteOrder {
    /// `ABCD` / `ABCDEFGH`：原样大端（S7 默认）。
    #[default]
    Abcd,
    /// `CDAB` / `CDABGHEF`：相邻 16 位寄存器互换。
    Cdab,
    /// `BADC` / `BADCFEHG`：每个寄存器内高低字节互换。
    Badc,
    /// `DCBA` / `HGFEDCBA`：整体逆序小端（MC 默认）。
    Dcba,
}

impl ByteOrder {
    /// S7（西门子）默认字节序：大端。
    pub const fn s7() -> Self {
        Self::Abcd
    }

    /// MC（三菱 Melsec）默认字节序：小端。
    pub const fn mc() -> Self {
        Self::Dcba
    }

    /// 英文配置名（`ABCD` / `CDAB` / `BADC` / `DCBA`，大小写与空格不敏感）。
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Abcd => "ABCD",
            Self::Cdab => "CDAB",
            Self::Badc => "BADC",
            Self::Dcba => "DCBA",
        }
    }

    /// 从配置名解析；无法识别返回 `None`（由调用方转成配置错误）。
    pub fn from_name(name: &str) -> Option<Self> {
        match name.trim().to_ascii_uppercase().as_str() {
            "ABCD" | "BIG" | "BIG_ENDIAN" => Some(Self::Abcd),
            "CDAB" | "BIG_WORD_SWAP" => Some(Self::Cdab),
            "BADC" | "BIG_BYTE_SWAP" => Some(Self::Badc),
            "DCBA" | "LITTLE" | "LITTLE_ENDIAN" => Some(Self::Dcba),
            _ => None,
        }
    }
}

// ---- 字符串 ----

/// 字符串截取模式（定长 / 结束符 / 整段）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StringMode {
    /// 定长：取前 `n` 字节（不足则取全部）。
    Fixed(u16),
    /// 结束符：截取到首个等于给定字节的位置（不含该字节）。
    Terminator(u8),
    /// 整段：使用全部负载字节。
    All,
}

/// 字符串解码细则。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StringSpec {
    /// 截取模式。
    pub mode: StringMode,
    /// 是否剥离尾部 NUL（`0x00`）填充（定长字符串常见）。
    pub trim_nul: bool,
    /// 非法 UTF-8 是否容错（以 U+FFFD 替换）而非报错。
    pub lossy: bool,
}

impl Default for StringSpec {
    fn default() -> Self {
        Self {
            mode: StringMode::Terminator(0),
            trim_nul: true,
            lossy: false,
        }
    }
}

// ---- 解码规格 ----

/// 点位解码规格（点位表的一行解码视图）。
#[derive(Debug, Clone, PartialEq)]
pub struct DecodeSpec {
    /// 数据类型。
    pub data_type: DataType,
    /// 字节序组合（默认 [`ByteOrder::Abcd`]）。
    pub byte_order: ByteOrder,
    /// 缩放系数：`scaled = raw * scale + offset`（默认 `1.0`）。
    pub scale: f64,
    /// 缩放偏移（默认 `0.0`）。
    pub offset: f64,
    /// 位号（布尔取该位；位串从该位起取），LSB = 0，取值 0-63。
    pub bit_index: u8,
    /// 位串宽度（1-64，默认 16；仅 [`DataType::Bits`] 使用）。
    pub bit_width: u8,
    /// 字符串细则（仅 [`DataType::Text`] 使用）。
    pub string: StringSpec,
    /// 工程量有效范围（闭区间）；越界置 [`Quality::OutOfRange`]，`None` 表示不校验。
    pub valid_range: Option<(f64, f64)>,
}

impl Default for DecodeSpec {
    fn default() -> Self {
        Self {
            data_type: DataType::Uint16,
            byte_order: ByteOrder::Abcd,
            scale: 1.0,
            offset: 0.0,
            bit_index: 0,
            bit_width: 16,
            string: StringSpec::default(),
            valid_range: None,
        }
    }
}

impl DecodeSpec {
    /// 以数据类型 + 字节序构建（其余取默认值）。
    pub fn new(data_type: DataType, byte_order: ByteOrder) -> Self {
        Self {
            data_type,
            byte_order,
            ..Self::default()
        }
    }

    /// 设置缩放因子，返回自身（链式构建）。
    pub fn with_scale_offset(mut self, scale: f64, offset: f64) -> Self {
        self.scale = scale;
        self.offset = offset;
        self
    }

    /// 设置位号与位宽，返回自身（链式构建）。
    pub fn with_bits(mut self, bit_index: u8, bit_width: u8) -> Self {
        self.bit_index = bit_index;
        self.bit_width = bit_width;
        self
    }

    /// 设置工程量有效范围，返回自身（链式构建）。
    pub fn with_valid_range(mut self, min: f64, max: f64) -> Self {
        self.valid_range = Some((min, max));
        self
    }

    /// 配置自检：非法即 [`DaemonError::ConfigError`]（错误码 2000）。
    pub fn validate(&self) -> DaemonResult<()> {
        if !self.scale.is_finite() || !self.offset.is_finite() {
            return Err(DaemonError::ConfigError(format!(
                "decode spec: scale/offset must be finite, got scale={} offset={}",
                self.scale, self.offset
            )));
        }
        if let Some((min, max)) = self.valid_range {
            if !min.is_finite() || !max.is_finite() || min > max {
                return Err(DaemonError::ConfigError(format!(
                    "decode spec: valid_range must be finite and min <= max, got [{min}, {max}]"
                )));
            }
        }
        if self.bit_index > 63 {
            return Err(DaemonError::ConfigError(format!(
                "decode spec: bit_index must be 0-63, got {}",
                self.bit_index
            )));
        }
        if self.bit_width == 0 || self.bit_width > 64 {
            return Err(DaemonError::ConfigError(format!(
                "decode spec: bit_width must be 1-64, got {}",
                self.bit_width
            )));
        }
        if let StringMode::Fixed(len) = self.string.mode {
            if len == 0 {
                return Err(DaemonError::ConfigError(
                    "decode spec: fixed string length must be >= 1".to_string(),
                ));
            }
        }
        if matches!(self.data_type, DataType::Bits) {
            let end = u32::from(self.bit_index).saturating_add(u32::from(self.bit_width));
            if end > 64 {
                return Err(DaemonError::ConfigError(format!(
                    "decode spec: bit_index {} + bit_width {} exceeds 64 bits",
                    self.bit_index, self.bit_width
                )));
            }
        }
        Ok(())
    }
}

// ---- 解码结果 ----

/// 解码后的强类型值（保持 `u64` / `i64` 全精度，不先降精度到 `f64`）。
#[derive(Debug, Clone, PartialEq)]
pub enum DecodedValue {
    /// 布尔（位提取结果）。
    Bool(bool),
    /// 有符号整数。
    Int(i64),
    /// 无符号整数（含 64 位计数器，全精度）。
    Uint(u64),
    /// IEEE754 浮点（float32 已提升为 `f64`）。
    Float(f64),
    /// 字符串。
    Text(String),
    /// 位串（提取出的无符号整数）。
    Bits(u64),
}

impl DecodedValue {
    /// 是否可参与数值运算（布尔 / 整数 / 浮点 / 位串；文本为 `false`）。
    pub fn is_numeric(&self) -> bool {
        !matches!(self, Self::Text(_))
    }

    /// 转为 `f64`（供 task 15 死区与换算使用）；文本为 `NaN`。
    ///
    /// 注意：`u64` 超过 `2^53 - 1` 时存在精度损失，故 [`Self::Uint`] 的原始值
    /// 必须同时保留（JSON 路径用 `json_number()` 编码为字符串）。
    pub fn as_f64(&self) -> f64 {
        match self {
            Self::Bool(b) => {
                if *b {
                    1.0
                } else {
                    0.0
                }
            }
            Self::Int(v) => *v as f64,
            Self::Uint(v) | Self::Bits(v) => *v as f64,
            Self::Float(v) => *v,
            Self::Text(_) => f64::NAN,
        }
    }

    /// JSON 数值字面量（红线：`uint64` 计数器与超过 `2^53 - 1` 的整数**编码为字符串**；
    /// 非有限浮点编码为 `null`）。文本类型返回 `None`（由调用方按字符串字段编码）。
    pub fn json_number(&self) -> Option<String> {
        match self {
            Self::Text(_) => None,
            Self::Bool(b) => Some(b.to_string()),
            Self::Int(v) => Some(if *v < -I64_SAFE_MAX || *v > I64_SAFE_MAX {
                format!("\"{v}\"")
            } else {
                v.to_string()
            }),
            Self::Uint(v) | Self::Bits(v) => Some(if *v > U64_SAFE_MAX {
                format!("\"{v}\"")
            } else {
                v.to_string()
            }),
            Self::Float(v) => Some(if v.is_finite() {
                v.to_string()
            } else {
                "null".to_string()
            }),
        }
    }
}

/// 解码输出：原始值 + 工程量值 + 质量码（可直接供 task 15 / task 62 使用）。
#[derive(Debug, Clone, PartialEq)]
pub struct DecodedSample {
    /// 原始解码值（`u64` 计数器保持全精度）。
    pub value: DecodedValue,
    /// 应用 `scale` / `offset` 后的工程量值；文本与非数值类型为 `NaN`。
    pub scaled: f64,
    /// 统一质量码。
    pub quality: Quality,
}

// ---- 解码器 ----

/// 统一数值解码器（无状态纯解码，可并发共享）。
#[derive(Debug, Clone, PartialEq)]
pub struct ValueDecoder {
    spec: DecodeSpec,
}

impl ValueDecoder {
    /// 按规格构建；非法规格返回 [`DaemonError::ConfigError`]。
    pub fn new(spec: DecodeSpec) -> DaemonResult<Self> {
        spec.validate()?;
        Ok(Self { spec })
    }

    /// 解码规格（只读）。
    pub const fn spec(&self) -> &DecodeSpec {
        &self.spec
    }

    /// 解码一段寄存器负载。
    ///
    /// # Errors
    /// - 负载长度不足、奇数长度（多字节数值）、位访问超过 8 字节、字符串非法 UTF-8
    ///   → [`DaemonError::ProtocolError`]（错误码 1000）；
    /// - 这些是**结构性问题**（点位配置与设备返回不匹配），须由调用方决策（告警 / 跳过），
    ///   不静默降级为 `quality = bad` 以免掩盖配置错误。
    pub fn decode(&self, bytes: &[u8]) -> DaemonResult<DecodedSample> {
        let value = match self.spec.data_type {
            DataType::Bool => DecodedValue::Bool(self.bit(bytes)?),
            DataType::Bits => DecodedValue::Bits(self.bits(bytes)?),
            DataType::Text => DecodedValue::Text(self.text(bytes)?),
            _ => self.number(bytes)?,
        };

        let scaled = if value.is_numeric() {
            value.as_f64() * self.spec.scale + self.spec.offset
        } else {
            f64::NAN
        };

        // 质量判定：① 非有限值 → Bad；② 越界 → OutOfRange（取最差，不覆盖更差者）。
        let mut quality = Quality::Good;
        if value.is_numeric() && !scaled.is_finite() {
            quality = Quality::Bad;
        }
        if let Some((min, max)) = self.spec.valid_range {
            // 仅对**数值**量做范围判定：文本 / 布尔 / 位串的工程量为 `NaN`，
            // 而 `!(min..=max).contains(&NaN)` 恒为真——若不判数值性，任何配置了
            // `valid_range` 的非数值点位都会被**永久**误判为 `OutOfRange`。
            if value.is_numeric() && !(min..=max).contains(&scaled) {
                quality = quality.worst(Quality::OutOfRange);
            }
        }

        Ok(DecodedSample {
            value,
            scaled,
            quality,
        })
    }

    /// 定长数值解码（跨寄存器拼接 + 符号扩展 + IEEE754 位 reinterpret）。
    fn number(&self, bytes: &[u8]) -> DaemonResult<DecodedValue> {
        let need = self.spec.data_type.fixed_size().ok_or_else(|| {
            DaemonError::ConfigError(format!(
                "decode spec: {:?} is not a fixed-size numeric type",
                self.spec.data_type
            ))
        })?;
        if bytes.len() < need {
            return Err(DaemonError::ProtocolError(format!(
                "decode {:?}: need {need} bytes, got {}",
                self.spec.data_type,
                bytes.len()
            )));
        }
        // 多字节数值不允许奇数长度（寄存器为 16 位，奇数说明配置/回包不匹配）。
        // 用 `% 2 == 1` 而非 `is_multiple_of`（后者稳定于 1.87，高于本仓库 MSRV 1.85）。
        if need > 1 && bytes.len() % 2 == 1 {
            return Err(DaemonError::ProtocolError(format!(
                "decode {:?}: odd payload length {} (registers are 16-bit)",
                self.spec.data_type,
                bytes.len()
            )));
        }

        let slice = &bytes[..need];
        let (ordered, len) = reorder(self.spec.byte_order, slice);
        let raw = be_u64(&ordered, len);

        let value = match self.spec.data_type {
            DataType::Int8 | DataType::Int16 | DataType::Int32 | DataType::Int64 => {
                DecodedValue::Int(sign_extend(raw, len))
            }
            DataType::Uint8 | DataType::Uint16 | DataType::Uint32 | DataType::Uint64 => {
                DecodedValue::Uint(raw)
            }
            DataType::Float32 => {
                let bits = (raw & 0xFFFF_FFFF) as u32;
                DecodedValue::Float(f64::from(f32::from_bits(bits)))
            }
            DataType::Float64 => DecodedValue::Float(f64::from_bits(raw)),
            // Bool / Bits / Text 已在 decode 中分流，此处不可达。
            DataType::Bool | DataType::Bits | DataType::Text => {
                return Err(DaemonError::ConfigError(format!(
                    "decode spec: {:?} must not go through numeric decoding",
                    self.spec.data_type
                )))
            }
        };
        Ok(value)
    }

    /// 按位取布尔：以组合后整数的 LSB 为 0 编号。
    fn bit(&self, bytes: &[u8]) -> DaemonResult<bool> {
        let word = self.bit_word(bytes)?;
        Ok((word >> self.spec.bit_index) & 1 == 1)
    }

    /// 按位宽取位串。
    fn bits(&self, bytes: &[u8]) -> DaemonResult<u64> {
        let word = self.bit_word(bytes)?;
        let mask = if self.spec.bit_width >= 64 {
            u64::MAX
        } else {
            (1u64 << u32::from(self.spec.bit_width)) - 1
        };
        Ok((word >> u32::from(self.spec.bit_index)) & mask)
    }

    /// 位访问的字缓冲（至多 8 字节，按字节序组合后的大端整数）。
    fn bit_word(&self, bytes: &[u8]) -> DaemonResult<u64> {
        if bytes.is_empty() {
            return Err(DaemonError::ProtocolError(
                "decode bit: empty payload".to_string(),
            ));
        }
        if bytes.len() > MAX_BYTES {
            return Err(DaemonError::ProtocolError(format!(
                "decode bit: at most {MAX_BYTES} bytes supported, got {}",
                bytes.len()
            )));
        }
        let (ordered, len) = reorder(self.spec.byte_order, bytes);
        Ok(be_u64(&ordered, len))
    }

    /// 字符串解码（定长 / 结束符 / 整段 + NUL 剥离 + UTF-8 校验）。
    fn text(&self, bytes: &[u8]) -> DaemonResult<String> {
        let mut slice: &[u8] = match self.spec.string.mode {
            StringMode::Fixed(len) => {
                let end = usize::from(len).min(bytes.len());
                &bytes[..end]
            }
            StringMode::Terminator(term) => bytes
                .iter()
                .position(|b| *b == term)
                .map_or(bytes, |pos| &bytes[..pos]),
            StringMode::All => bytes,
        };

        if self.spec.string.trim_nul {
            let end = slice.iter().rposition(|b| *b != 0).map_or(0, |pos| pos + 1);
            slice = &slice[..end];
        }

        match std::str::from_utf8(slice) {
            Ok(s) => Ok(s.to_string()),
            Err(_) if self.spec.string.lossy => Ok(String::from_utf8_lossy(slice).into_owned()),
            Err(e) => Err(DaemonError::ProtocolError(format!(
                "decode string: invalid utf-8 payload ({e})"
            ))),
        }
    }
}

/// 按字节序组合重排至多 [`MAX_BYTES`] 字节，返回定长缓冲与实际长度。
///
/// 不做裸索引：`CDAB` / `BADC` 在缺少配对寄存器时退化为原样（单寄存器场景）。
fn reorder(order: ByteOrder, bytes: &[u8]) -> ([u8; MAX_BYTES], usize) {
    let len = bytes.len().min(MAX_BYTES);
    let mut out = [0u8; MAX_BYTES];
    for (k, slot) in out.iter_mut().enumerate().take(len) {
        let src = match order {
            ByteOrder::Abcd => k,
            // 寄存器内高低字节互换；末字节无配对时原样保留。
            ByteOrder::Badc => {
                let pair_lo = (k / 2) * 2 + 1;
                if pair_lo < len {
                    (k / 2) * 2 + (1 - k % 2)
                } else {
                    k
                }
            }
            // 相邻 16 位寄存器互换；无配对寄存器时原样保留。
            ByteOrder::Cdab => {
                let other = ((k / 2) ^ 1) * 2 + (k % 2);
                if other < len {
                    other
                } else {
                    k
                }
            }
            ByteOrder::Dcba => len - 1 - k,
        };
        // src 由上界推导保证落在 0..len 内；仍用 get 兜底，杜绝越界 panic。
        *slot = bytes.get(src).copied().unwrap_or(0);
    }
    (out, len)
}

/// 定长缓冲按大端组装为 `u64`（`len` ≤ [`MAX_BYTES`]）。
fn be_u64(ordered: &[u8; MAX_BYTES], len: usize) -> u64 {
    let mut v = 0u64;
    for b in ordered.iter().take(len) {
        v = (v << 8) | u64::from(*b);
    }
    v
}

/// 按字节宽度做符号扩展（`bytes` ∈ 1..=8）。
fn sign_extend(raw: u64, bytes: usize) -> i64 {
    let bits = u32::try_from(bytes.min(MAX_BYTES) * 8).unwrap_or(64);
    let shift = 64u32.saturating_sub(bits);
    (raw.wrapping_shl(shift) as i64).wrapping_shr(shift)
}

// ---- 质量码 ----

/// OPC UA StatusCode severity 掩码（bit30-31）。
const OPCUA_SEVERITY_MASK: u32 = 0xC000_0000;
/// OPC UA `Good` severity（含 0x00000000 / GoodClamped 0x00300000 等）。
const OPCUA_SEVERITY_GOOD: u32 = 0x0000_0000;
/// OPC UA `Uncertain` severity。
const OPCUA_SEVERITY_UNCERTAIN: u32 = 0x4000_0000;

/// OPC UA `Bad_Timeout`（操作超时）。
pub const OPCUA_BAD_TIMEOUT: u32 = 0x800A_0000;
/// OPC UA `Bad_CommunicationError`（底层通信错误）。
pub const OPCUA_BAD_COMMUNICATION_ERROR: u32 = 0x8005_0000;
/// OPC UA `Bad_NoCommunication`（无通信）。
pub const OPCUA_BAD_NO_COMMUNICATION: u32 = 0x8031_0000;
/// OPC UA `Bad_WaitingForInitialData`（等待首帧数据）。
pub const OPCUA_BAD_WAITING_FOR_INITIAL_DATA: u32 = 0x8032_0000;
/// OPC UA `Bad_OutOfRange`（写入值 / 读数超出量程）。
pub const OPCUA_BAD_OUT_OF_RANGE: u32 = 0x803C_0000;

/// Modbus 异常码：非法功能。
pub const MODBUS_EXC_ILLEGAL_FUNCTION: u8 = 0x01;
/// Modbus 异常码：非法数据地址。
pub const MODBUS_EXC_ILLEGAL_DATA_ADDRESS: u8 = 0x02;
/// Modbus 异常码：非法数据值。
pub const MODBUS_EXC_ILLEGAL_DATA_VALUE: u8 = 0x03;
/// Modbus 异常码：从站设备故障。
pub const MODBUS_EXC_SLAVE_FAILURE: u8 = 0x04;
/// Modbus 异常码：确认（已接受，需长时操作）。
pub const MODBUS_EXC_ACKNOWLEDGE: u8 = 0x05;
/// Modbus 异常码：从站设备忙。
pub const MODBUS_EXC_SLAVE_BUSY: u8 = 0x06;
/// Modbus 异常码：网关路径不可用。
pub const MODBUS_EXC_GATEWAY_PATH_UNAVAILABLE: u8 = 0x0A;
/// Modbus 异常码：网关目标设备无响应。
pub const MODBUS_EXC_GATEWAY_NO_RESPONSE: u8 = 0x0B;

/// S7 返回码：无错误。
pub const S7_RETURN_OK: u8 = 0x00;
/// S7 返回码：硬件错误。
pub const S7_RETURN_HARDWARE_ERROR: u8 = 0x01;
/// S7 返回码：对象不允许访问。
pub const S7_RETURN_ACCESS_DENIED: u8 = 0x03;
/// S7 返回码：地址无效。
pub const S7_RETURN_INVALID_ADDRESS: u8 = 0x05;
/// S7 返回码：数据类型不支持。
pub const S7_RETURN_TYPE_NOT_SUPPORTED: u8 = 0x06;
/// S7 返回码：对象不存在。
pub const S7_RETURN_OBJECT_NOT_EXIST: u8 = 0x0A;

/// MC（Melsec）结束码：正常完成。
pub const MC_END_OK: u16 = 0x0000;
/// MC 结束码：指定的软元件范围超出允许范围。
pub const MC_END_RANGE_OUT: u16 = 0x4031;
/// MC 结束码：不允许读取。
pub const MC_END_READ_NOT_ALLOWED: u16 = 0xC051;
/// MC 结束码：不允许写入。
pub const MC_END_WRITE_NOT_ALLOWED: u16 = 0xC056;
/// MC 结束码：请求报文错误。
pub const MC_END_BAD_REQUEST: u16 = 0xC058;
/// MC 结束码：命令 / 子命令指定错误。
pub const MC_END_COMMAND_ERROR: u16 = 0xC059;
/// MC 结束码：软元件指定错误。
pub const MC_END_DEVICE_ERROR: u16 = 0xC05B;
/// MC 结束码：软元件地址越界。
pub const MC_END_DEVICE_OFFSET: u16 = 0xC05C;
/// MC 结束码：目标 CPU 不可执行（STOP / 未解锁）。
pub const MC_END_CANNOT_EXEC: u16 = 0xC05F;
/// MC 结束码：位指定错误。
pub const MC_END_BIT_SPEC: u16 = 0xC060;

/// 统一质量码（计划 task 53）。
///
/// `severity()` 越大越差，代表「信息缺失程度」递增；**多输入取最差**是唯一继承规则。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Quality {
    /// 良好：值有效且可信。
    Good,
    /// 不确定：值可用但可信度下降（如设备自校准中、OPC UA Uncertain）。
    Uncertain,
    /// 超出工程量程：值已读到但不落在有效范围内。
    OutOfRange,
    /// **计算失败**（task 70 使用）：计算点因输入缺失 / 输入超时 / 输入非数值 /
    /// 除零 / 结果为 NaN 或 ±Inf 而不可用。语义由本任务定义，70 只做使用。
    CalcFailed,
    /// 通用坏值：值不可用（兜底，如 IEEE754 NaN / ±Inf、从站设备故障）。
    Bad,
    /// 超时：请求未在超时时间内返回（含 Modbus 传输超时、网关目标无响应）。
    Timeout,
    /// 通信故障：链路 / 协议层失败（连接断开、异常码、S7/MC 结束码非 0）。
    CommError,
}

impl Quality {
    /// 严重度（越大越差）：`Good=0 … CommError=6`。
    pub const fn severity(self) -> u8 {
        match self {
            Self::Good => 0,
            Self::Uncertain => 1,
            Self::OutOfRange => 2,
            Self::CalcFailed => 3,
            Self::Bad => 4,
            Self::Timeout => 5,
            Self::CommError => 6,
        }
    }

    /// **最差质量继承**：两者中取更差者，相等时保留 `self`（结果确定、与顺序无关）。
    pub fn worst(self, other: Self) -> Self {
        if other.severity() > self.severity() {
            other
        } else {
            self
        }
    }

    /// 多输入取最差（派生量 / 聚合点用）；空输入视为 [`Self::Good`]。
    pub fn worst_of<I: IntoIterator<Item = Self>>(iter: I) -> Self {
        iter.into_iter().fold(Self::Good, Self::worst)
    }

    /// 是否良好。
    pub const fn is_good(self) -> bool {
        matches!(self, Self::Good)
    }

    /// 是否可用于计算（良好或不确定；坏值与通信故障均不可用）。
    pub const fn is_acceptable(self) -> bool {
        matches!(self, Self::Good | Self::Uncertain)
    }

    /// 本次是否**取到了值**（超时与通信故障表示根本没有数值）。
    pub const fn has_value(self) -> bool {
        !matches!(self, Self::Timeout | Self::CommError)
    }

    /// JSON / 日志用的枚举名（`GOOD` / `CALC_FAILED` / …）。
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Good => "GOOD",
            Self::Uncertain => "UNCERTAIN",
            Self::OutOfRange => "OUT_OF_RANGE",
            Self::CalcFailed => "CALC_FAILED",
            Self::Bad => "BAD",
            Self::Timeout => "TIMEOUT",
            Self::CommError => "COMM_ERROR",
        }
    }

    /// 从枚举名解析；无法识别返回 `None`。
    pub fn from_name(name: &str) -> Option<Self> {
        match name.trim().to_ascii_uppercase().as_str() {
            "GOOD" => Some(Self::Good),
            "UNCERTAIN" => Some(Self::Uncertain),
            "OUT_OF_RANGE" => Some(Self::OutOfRange),
            "CALC_FAILED" => Some(Self::CalcFailed),
            "BAD" => Some(Self::Bad),
            "TIMEOUT" => Some(Self::Timeout),
            "COMM_ERROR" => Some(Self::CommError),
            _ => None,
        }
    }

    /// 映射为北向 Protobuf 质量码（task 62 编码用）：
    /// `Good → GOOD`、`Uncertain → UNCERTAIN`、其余（含 `Timeout` / `CommError` /
    /// `CalcFailed` / `OutOfRange`）→ `BAD`。
    pub const fn to_wire(self) -> WireQuality {
        match self {
            Self::Good => WireQuality::Good,
            Self::Uncertain => WireQuality::Uncertain,
            _ => WireQuality::Bad,
        }
    }

    /// 从北向 Protobuf 质量码反向归一（日志 / 回放用；`Simulated` 视为有效值 `Good`，
    /// `Unspecified` 视为未初始化 → `Uncertain`）。
    pub const fn from_wire(wire: WireQuality) -> Self {
        match wire {
            WireQuality::Good | WireQuality::Simulated => Self::Good,
            WireQuality::Uncertain | WireQuality::Unspecified => Self::Uncertain,
            WireQuality::Bad => Self::Bad,
        }
    }

    /// OPC UA StatusCode → 统一质量码：先按已知码精确映射，再按 severity 位兜底
    /// （`0x0000_0000` → Good、`0x4000_0000` → Uncertain、`0x8000_0000` → Bad）。
    pub const fn from_opc_ua_status(code: u32) -> Self {
        match code {
            OPCUA_BAD_TIMEOUT | OPCUA_BAD_WAITING_FOR_INITIAL_DATA => Self::Timeout,
            OPCUA_BAD_COMMUNICATION_ERROR | OPCUA_BAD_NO_COMMUNICATION => Self::CommError,
            OPCUA_BAD_OUT_OF_RANGE => Self::OutOfRange,
            _ => match code & OPCUA_SEVERITY_MASK {
                OPCUA_SEVERITY_GOOD => Self::Good,
                OPCUA_SEVERITY_UNCERTAIN => Self::Uncertain,
                _ => Self::Bad,
            },
        }
    }

    /// Modbus 异常码 → 统一质量码（**Modbus 传输超时无异常码**，由
    /// [`DriverFault::Timeout`] 或 [`Self::Timeout`] 直接表达）。
    pub const fn from_modbus_exception(code: u8) -> Self {
        match code {
            // 已接受但需长时操作：值尚未确定，不算故障。
            MODBUS_EXC_ACKNOWLEDGE => Self::Uncertain,
            // 从站设备故障：设备回了但值是坏的。
            MODBUS_EXC_SLAVE_FAILURE => Self::Bad,
            // 网关目标设备无响应 → 超时语义。
            MODBUS_EXC_GATEWAY_NO_RESPONSE => Self::Timeout,
            // 非法功能 / 地址 / 值、设备忙、网关路径不可用 → 通信层失败。
            MODBUS_EXC_ILLEGAL_FUNCTION
            | MODBUS_EXC_ILLEGAL_DATA_ADDRESS
            | MODBUS_EXC_ILLEGAL_DATA_VALUE
            | MODBUS_EXC_SLAVE_BUSY
            | MODBUS_EXC_GATEWAY_PATH_UNAVAILABLE => Self::CommError,
            _ => Self::CommError,
        }
    }

    /// S7 返回码 → 统一质量码（`0x00` 正常；硬件错误视为坏值；其余为非零返回码 → 通信故障）。
    pub const fn from_s7_return_code(code: u8) -> Self {
        match code {
            S7_RETURN_OK => Self::Good,
            S7_RETURN_HARDWARE_ERROR => Self::Bad,
            S7_RETURN_ACCESS_DENIED
            | S7_RETURN_INVALID_ADDRESS
            | S7_RETURN_TYPE_NOT_SUPPORTED
            | S7_RETURN_OBJECT_NOT_EXIST => Self::CommError,
            _ => Self::CommError,
        }
    }

    /// MC（Melsec）结束码 → 统一质量码（`0x0000` 正常；软元件范围越界 → 超量程；
    /// 其余非零结束码 → 通信故障）。
    pub const fn from_mc_end_code(code: u16) -> Self {
        match code {
            MC_END_OK => Self::Good,
            MC_END_RANGE_OUT => Self::OutOfRange,
            MC_END_READ_NOT_ALLOWED
            | MC_END_WRITE_NOT_ALLOWED
            | MC_END_BAD_REQUEST
            | MC_END_COMMAND_ERROR
            | MC_END_DEVICE_ERROR
            | MC_END_DEVICE_OFFSET
            | MC_END_CANNOT_EXEC
            | MC_END_BIT_SPEC => Self::CommError,
            _ => Self::CommError,
        }
    }

    /// **跨驱动统一入口**：各驱动把原生状态码包装为 [`DriverFault`] 后调用本方法，
    /// 驱动内不得自带 quality 语义（计划 Must NOT do）。
    pub const fn from_driver_fault(fault: DriverFault) -> Self {
        match fault {
            DriverFault::Ok => Self::Good,
            DriverFault::Timeout => Self::Timeout,
            DriverFault::ModbusException(code) => Self::from_modbus_exception(code),
            DriverFault::OpcUa(code) => Self::from_opc_ua_status(code),
            DriverFault::S7(code) => Self::from_s7_return_code(code),
            DriverFault::Mc(code) => Self::from_mc_end_code(code),
        }
    }
}

/// 驱动故障原语：各驱动把自身原生状态码包装为此枚举后交给
/// [`Quality::from_driver_fault`] 归一（**映射表集中在 codec，不散落在驱动里**）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DriverFault {
    /// 无故障（正常完成）。
    Ok,
    /// 传输超时（请求未在超时时间内返回；Modbus 超时无异常码，由驱动自行判定）。
    Timeout,
    /// Modbus 异常响应码（0x01-0x0B）。
    ModbusException(u8),
    /// OPC UA StatusCode（32 位）。
    OpcUa(u32),
    /// S7 返回码。
    S7(u8),
    /// MC（Melsec）结束码。
    Mc(u16),
}

impl DriverFault {
    /// 归一为统一质量码（委托 [`Quality::from_driver_fault`]）。
    pub const fn quality(self) -> Quality {
        Quality::from_driver_fault(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 便捷构造：数据类型 + 字节序。
    fn dec(data_type: DataType, order: ByteOrder) -> ValueDecoder {
        ValueDecoder::new(DecodeSpec::new(data_type, order)).expect("spec")
    }

    /// 便捷取工程量值。
    fn f32_of(order: ByteOrder, bytes: &[u8]) -> f64 {
        dec(DataType::Float32, order)
            .decode(bytes)
            .expect("decode")
            .scaled
    }

    // ---- QA 场景 ----

    /// **QA Happy**：寄存器 `[0x41C8_0000]` 按 ABCD 解 float32 → 25.0。
    #[test]
    fn float32_abcd_decodes_25() {
        let regs = [0x41u8, 0xC8, 0x00, 0x00];
        let out = dec(DataType::Float32, ByteOrder::Abcd)
            .decode(&regs)
            .expect("decode");
        assert_eq!(out.scaled, 25.0);
        assert_eq!(out.quality, Quality::Good);
        assert_eq!(out.value, DecodedValue::Float(25.0));
    }

    /// **QA Happy**：同一数据按 CDAB 得到**不同**的预期值（证明组合确实生效）；
    /// 反向向量：寄存器 `[0x0000_41C8]` 按 CDAB 才是 25.0。
    #[test]
    fn float32_cdab_differs_from_abcd() {
        let regs = [0x41u8, 0xC8, 0x00, 0x00];

        // 同一份数据：ABCD = 25.0，CDAB 交换相邻寄存器 → 0x000041C8（极小非规格化数）。
        let cdab = dec(DataType::Float32, ByteOrder::Cdab)
            .decode(&regs)
            .expect("decode");
        assert_ne!(cdab.scaled, 25.0);
        assert!(
            cdab.scaled > 0.0 && cdab.scaled < 1e-30,
            "CDAB of 0x41C80000 must be a tiny denormal, got {}",
            cdab.scaled
        );
        // 以无符号视图固化精确位模式，避免浮点断言含糊。
        let raw = dec(DataType::Uint32, ByteOrder::Cdab)
            .decode(&regs)
            .expect("decode");
        assert_eq!(raw.value, DecodedValue::Uint(0x0000_41C8));

        // 反向向量：要按 CDAB 解出 25.0，线路字节必须是 00 00 41 C8。
        let swapped = [0x00u8, 0x00, 0x41, 0xC8];
        assert_eq!(f32_of(ByteOrder::Cdab, &swapped), 25.0);
        assert_ne!(f32_of(ByteOrder::Abcd, &swapped), 25.0);
    }

    /// **QA Error**：用错误字节序解 float32 → 结果明显异常（反例固化）。
    #[test]
    fn float32_wrong_byte_order_is_obviously_abnormal() {
        let regs = [0x41u8, 0xC8, 0x00, 0x00];
        let truth = 25.0f64;

        // BADC（寄存器内字节互换）→ 0xC8410000 → -197632.0
        let badc = f32_of(ByteOrder::Badc, &regs);
        assert!((badc + 197_632.0).abs() < 1e-6, "BADC got {badc}");
        assert!((badc - truth).abs() > 1e3, "BADC must be far from 25.0");

        // DCBA（整体逆序）→ 0x0000C841 → 极小非规格化数（与真值差 ~25）
        let dcba = f32_of(ByteOrder::Dcba, &regs);
        assert!(dcba.abs() < 1e-30, "DCBA got {dcba}");
        assert_ne!(dcba, truth);
        assert!((dcba - truth).abs() > 24.0, "DCBA must be far from 25.0");

        // 四种字节序互不相同（组合确实生效，不是同一条代码路径）。
        let all = [
            f32_of(ByteOrder::Abcd, &regs),
            f32_of(ByteOrder::Cdab, &regs),
            f32_of(ByteOrder::Badc, &regs),
            f32_of(ByteOrder::Dcba, &regs),
        ];
        for i in 0..all.len() {
            for j in (i + 1)..all.len() {
                assert_ne!(all[i], all[j], "byte orders {i}/{j} collapsed: {all:?}");
            }
        }
    }

    /// 32 位四种字节序的精确位模式（uint32 视图，最强证据）。
    #[test]
    fn all_four_byte_orders_are_distinct() {
        let regs = [0x01u8, 0x02, 0x03, 0x04];
        let expect = [
            (ByteOrder::Abcd, 0x0102_0304u64), // ABCD
            (ByteOrder::Cdab, 0x0304_0102u64), // CDAB
            (ByteOrder::Badc, 0x0201_0403u64), // BADC
            (ByteOrder::Dcba, 0x0403_0201u64), // DCBA
        ];
        for (order, want) in expect {
            let out = dec(DataType::Uint32, order).decode(&regs).expect("decode");
            assert_eq!(out.value, DecodedValue::Uint(want), "order {order:?}");
        }
        // 名称往返与 S7/MC 默认序。
        assert_eq!(
            ByteOrder::from_name(" cdab ").expect("name"),
            ByteOrder::Cdab
        );
        assert_eq!(ByteOrder::s7(), ByteOrder::Abcd);
        assert_eq!(ByteOrder::mc(), ByteOrder::Dcba);
        assert_eq!(ByteOrder::Abcd.as_str(), "ABCD");
    }

    /// float64 已知真值向量往返（含 DCBA 逆序解码）。
    #[test]
    fn float64_true_value_vectors_roundtrip() {
        let vectors: [(f64, u64); 3] = [
            (1.0, 0x3FF0_0000_0000_0000),
            (-2.5, 0xC004_0000_0000_0000),
            (std::f64::consts::PI, 0x4009_21FB_5444_2D18),
        ];
        for (truth, bits) in vectors {
            let be = bits.to_be_bytes();
            assert_eq!(f32_of_x64(ByteOrder::Abcd, &be), truth, "bits {bits:#018X}");

            let mut le = be;
            le.reverse();
            assert_eq!(f32_of_x64(ByteOrder::Dcba, &le), truth, "bits {bits:#018X}");
        }
    }

    /// 整数类型：符号扩展 + 字节序敏感性 + 长度不足报错。
    #[test]
    fn integer_types_sign_extension_and_errors() {
        // 8 位：字节序无关。
        assert_eq!(
            dec(DataType::Int8, ByteOrder::Abcd)
                .decode(&[0xFF])
                .expect("decode")
                .value,
            DecodedValue::Int(-1)
        );
        assert_eq!(
            dec(DataType::Uint8, ByteOrder::Dcba)
                .decode(&[0xFF])
                .expect("decode")
                .value,
            DecodedValue::Uint(255)
        );

        // 16 位：0xFFFE → -2（ABCD）；DCBA 逆序 → 0xFEFF → -257。
        assert_eq!(
            dec(DataType::Int16, ByteOrder::Abcd)
                .decode(&[0xFF, 0xFE])
                .expect("decode")
                .value,
            DecodedValue::Int(-2)
        );
        assert_eq!(
            dec(DataType::Int16, ByteOrder::Dcba)
                .decode(&[0xFF, 0xFE])
                .expect("decode")
                .value,
            DecodedValue::Int(-257)
        );

        // 32 / 64 位全 1 → -1（符号扩展正确）。
        assert_eq!(
            dec(DataType::Int32, ByteOrder::Abcd)
                .decode(&[0xFF; 4])
                .expect("decode")
                .value,
            DecodedValue::Int(-1)
        );
        assert_eq!(
            dec(DataType::Int64, ByteOrder::Abcd)
                .decode(&[0xFF; 8])
                .expect("decode")
                .value,
            DecodedValue::Int(-1)
        );

        // 长度不足 / 奇数长度 → ProtocolError（不静默降级）。
        let short = dec(DataType::Int32, ByteOrder::Abcd).decode(&[0x00, 0x01]);
        assert!(
            matches!(short, Err(DaemonError::ProtocolError(_))),
            "got {short:?}"
        );
        let odd = dec(DataType::Int32, ByteOrder::Abcd).decode(&[0x00, 0x01, 0x02]);
        assert!(
            matches!(odd, Err(DaemonError::ProtocolError(_))),
            "got {odd:?}"
        );
    }

    /// uint64 计数器保持全精度，且 JSON 数值字面量为字符串（IEEE754 2^53-1 红线）。
    #[test]
    fn uint64_counter_keeps_precision_and_json_string() {
        let counter = 0x0123_4567_89AB_CDEFu64;
        let out = dec(DataType::Uint64, ByteOrder::Abcd)
            .decode(&counter.to_be_bytes())
            .expect("decode");
        assert_eq!(out.value, DecodedValue::Uint(counter));
        // 超过 2^53-1 → JSON 必须是字符串。
        assert_eq!(
            out.value.json_number(),
            Some("\"81985529216486895\"".to_string())
        );

        let max = dec(DataType::Uint64, ByteOrder::Abcd)
            .decode(&[0xFF; 8])
            .expect("decode");
        assert_eq!(max.value, DecodedValue::Uint(u64::MAX));
        assert_eq!(
            max.value.json_number(),
            Some("\"18446744073709551615\"".to_string())
        );
        // 未超过 2^53-1 的小整数仍编码为数值。
        assert_eq!(DecodedValue::Uint(42).json_number(), Some("42".to_string()));
        assert_eq!(
            DecodedValue::Float(f64::NAN).json_number(),
            Some("null".to_string())
        );
        assert_eq!(DecodedValue::Text("x".to_string()).json_number(), None);
    }

    /// 缩放因子（scale / offset）产生工程量值，原始值不被破坏。
    #[test]
    fn scale_offset_range_and_nan_quality() {
        let decoder = ValueDecoder::new(
            DecodeSpec::new(DataType::Uint16, ByteOrder::Abcd).with_scale_offset(0.1, -4.0),
        )
        .expect("spec");
        let out = decoder.decode(&[0x04, 0xD2]).expect("decode"); // 1234
        assert_eq!(out.value, DecodedValue::Uint(1234));
        assert!((out.scaled - 119.4).abs() < 1e-9, "got {}", out.scaled);

        // 越界 → OutOfRange（工程量语义）。
        let ranged = ValueDecoder::new(
            DecodeSpec::new(DataType::Uint16, ByteOrder::Abcd).with_valid_range(0.0, 100.0),
        )
        .expect("spec");
        assert_eq!(
            ranged.decode(&[0x00, 0x32]).expect("decode").quality,
            Quality::Good
        ); // 50
        assert_eq!(
            ranged.decode(&[0x04, 0xD2]).expect("decode").quality,
            Quality::OutOfRange
        ); // 1234

        // NaN / ±Inf → Bad（有效性由 quality 表达）。
        let nan = dec(DataType::Float32, ByteOrder::Abcd)
            .decode(&[0x7F, 0xC0, 0x00, 0x00])
            .expect("decode");
        assert!(nan.scaled.is_nan());
        assert_eq!(nan.quality, Quality::Bad);
    }

    /// 位（bool）与位串（bitset）提取：位号以组合后整数 LSB 为 0。
    #[test]
    fn bit_and_bitset_extraction() {
        // 单寄存器值 0x0005：bit0=1, bit1=0, bit2=1（位号以寄存器整数 LSB 为 0）。
        let regs = [0x00u8, 0b0000_0101];
        let bool_at = |idx: u8| {
            ValueDecoder::new(DecodeSpec::new(DataType::Bool, ByteOrder::Abcd).with_bits(idx, 1))
                .expect("spec")
                .decode(&regs)
                .expect("decode")
                .value
        };
        assert_eq!(bool_at(0), DecodedValue::Bool(true));
        assert_eq!(bool_at(1), DecodedValue::Bool(false));
        assert_eq!(bool_at(2), DecodedValue::Bool(true));
        assert_eq!(bool_at(8), DecodedValue::Bool(false));

        // 位串：从 bit0 取 8 位 → 0x05；从 bit4 取 4 位 → 0x00。
        let bits_at = |idx: u8, width: u8| -> u64 {
            match ValueDecoder::new(
                DecodeSpec::new(DataType::Bits, ByteOrder::Abcd).with_bits(idx, width),
            )
            .expect("spec")
            .decode(&regs)
            .expect("decode")
            .value
            {
                DecodedValue::Bits(v) => v,
                other => panic!("unexpected {other:?}"),
            }
        };
        assert_eq!(bits_at(0, 8), 0x05);
        assert_eq!(bits_at(4, 4), 0x00);

        // 非法位配置在构造期被拒（bit_index + bit_width > 64、bit_width = 0）。
        let bad =
            ValueDecoder::new(DecodeSpec::new(DataType::Bits, ByteOrder::Abcd).with_bits(60, 8));
        assert!(matches!(bad, Err(DaemonError::ConfigError(_))));
    }

    /// 字符串：定长 / 结束符 / NUL 剥离 / 非法 UTF-8 报错与容错。
    #[test]
    fn string_fixed_terminator_and_invalid_utf8() {
        let payload = b"HELLO\0\0\0";

        // 结束符（默认 0x00）+ NUL 剥离。
        let term = ValueDecoder::new(DecodeSpec {
            data_type: DataType::Text,
            ..DecodeSpec::default()
        })
        .expect("spec");
        assert_eq!(
            term.decode(payload).expect("decode").value,
            DecodedValue::Text("HELLO".to_string())
        );

        // 定长 3 字节 → "HEL"。
        let fixed = ValueDecoder::new(DecodeSpec {
            data_type: DataType::Text,
            string: StringSpec {
                mode: StringMode::Fixed(3),
                ..StringSpec::default()
            },
            ..DecodeSpec::default()
        })
        .expect("spec");
        assert_eq!(
            fixed.decode(payload).expect("decode").value,
            DecodedValue::Text("HEL".to_string())
        );

        // 非法 UTF-8：默认报错（ProtocolError），lossy 模式容错。
        let invalid = [0x41u8, 0xFF, 0x42];
        let strict = ValueDecoder::new(DecodeSpec {
            data_type: DataType::Text,
            string: StringSpec {
                mode: StringMode::All,
                trim_nul: false,
                lossy: false,
            },
            ..DecodeSpec::default()
        })
        .expect("spec");
        assert!(matches!(
            strict.decode(&invalid),
            Err(DaemonError::ProtocolError(_))
        ));

        let lossy = ValueDecoder::new(DecodeSpec {
            data_type: DataType::Text,
            string: StringSpec {
                mode: StringMode::All,
                trim_nul: false,
                lossy: true,
            },
            ..DecodeSpec::default()
        })
        .expect("spec");
        assert_eq!(
            lossy.decode(&invalid).expect("decode").value,
            DecodedValue::Text("A\u{FFFD}B".to_string())
        );
    }

    /// **QA Error**：Modbus 超时 → `timeout` 而非 `good`。
    #[test]
    fn modbus_timeout_maps_to_timeout_not_good() {
        let q = Quality::from_driver_fault(DriverFault::Timeout);
        assert_eq!(q, Quality::Timeout);
        assert_ne!(q, Quality::Good);
        assert!(!q.is_good());
        assert!(!q.has_value(), "timeout means no value was obtained");
        // 北向 wire 层归一为 BAD（Protobuf 只有 5 个枚举值）。
        assert_eq!(q.to_wire(), WireQuality::Bad);

        // 网关目标设备无响应（异常码 0x0B）同样映射为 Timeout。
        assert_eq!(
            Quality::from_driver_fault(DriverFault::ModbusException(
                MODBUS_EXC_GATEWAY_NO_RESPONSE
            )),
            Quality::Timeout
        );
    }

    /// 跨驱动映射表：OPC UA StatusCode / Modbus 异常码 / S7 / MC。
    #[test]
    fn cross_driver_fault_mapping_table() {
        // OPC UA：精确码 + severity 兜底。
        assert_eq!(
            Quality::from_driver_fault(DriverFault::OpcUa(OPCUA_BAD_TIMEOUT)),
            Quality::Timeout
        );
        assert_eq!(
            Quality::from_driver_fault(DriverFault::OpcUa(OPCUA_BAD_COMMUNICATION_ERROR)),
            Quality::CommError
        );
        assert_eq!(
            Quality::from_driver_fault(DriverFault::OpcUa(OPCUA_BAD_OUT_OF_RANGE)),
            Quality::OutOfRange
        );
        assert_eq!(
            Quality::from_driver_fault(DriverFault::OpcUa(0x0000_0000)),
            Quality::Good
        );
        assert_eq!(
            Quality::from_driver_fault(DriverFault::OpcUa(0x4000_0000)),
            Quality::Uncertain
        );
        assert_eq!(
            Quality::from_driver_fault(DriverFault::OpcUa(0x8000_0000)),
            Quality::Bad
        );

        // Modbus 异常码。
        assert_eq!(
            Quality::from_driver_fault(DriverFault::ModbusException(
                MODBUS_EXC_ILLEGAL_DATA_ADDRESS
            )),
            Quality::CommError
        );
        assert_eq!(
            Quality::from_driver_fault(DriverFault::ModbusException(MODBUS_EXC_SLAVE_FAILURE)),
            Quality::Bad
        );
        assert_eq!(
            Quality::from_driver_fault(DriverFault::ModbusException(MODBUS_EXC_ACKNOWLEDGE)),
            Quality::Uncertain
        );

        // S7：0x00 正常 / 硬件错误坏值 / 其余非零 → 通信故障。
        assert_eq!(
            Quality::from_driver_fault(DriverFault::S7(S7_RETURN_OK)),
            Quality::Good
        );
        assert_eq!(
            Quality::from_driver_fault(DriverFault::S7(S7_RETURN_OBJECT_NOT_EXIST)),
            Quality::CommError
        );
        assert_eq!(
            Quality::from_driver_fault(DriverFault::S7(S7_RETURN_HARDWARE_ERROR)),
            Quality::Bad
        );

        // MC：0x0000 正常 / 软元件范围越界 → 超量程 / 其余非零 → 通信故障。
        assert_eq!(
            Quality::from_driver_fault(DriverFault::Mc(MC_END_OK)),
            Quality::Good
        );
        assert_eq!(
            Quality::from_driver_fault(DriverFault::Mc(MC_END_RANGE_OUT)),
            Quality::OutOfRange
        );
        assert_eq!(
            Quality::from_driver_fault(DriverFault::Mc(MC_END_READ_NOT_ALLOWED)),
            Quality::CommError
        );

        // 无故障 → Good。
        assert_eq!(DriverFault::Ok.quality(), Quality::Good);
    }

    /// **最差质量继承**（多输入取最差）：派生点 / 聚合点的核心规则，含 `calc_failed`。
    #[test]
    fn worst_quality_inheritance() {
        assert_eq!(Quality::Good.worst(Quality::Good), Quality::Good);
        assert_eq!(Quality::Good.worst(Quality::CommError), Quality::CommError);
        assert_eq!(Quality::CommError.worst(Quality::Good), Quality::CommError);
        assert_eq!(Quality::Uncertain.worst(Quality::Bad), Quality::Bad);
        assert_eq!(
            Quality::Timeout.worst(Quality::CommError),
            Quality::CommError
        );
        // calc_failed 优于 timeout / comm_error，劣于 out_of_range。
        assert_eq!(
            Quality::CalcFailed.worst(Quality::Timeout),
            Quality::Timeout
        );
        assert_eq!(
            Quality::OutOfRange.worst(Quality::CalcFailed),
            Quality::CalcFailed
        );

        // 多输入：任一超时即整体超时；顺序无关（结果确定）。
        let inputs = [
            Quality::Good,
            Quality::Uncertain,
            Quality::Timeout,
            Quality::Good,
        ];
        assert_eq!(Quality::worst_of(inputs), Quality::Timeout);
        assert_eq!(
            Quality::worst_of(inputs.into_iter().rev()),
            Quality::Timeout
        );
        assert_eq!(Quality::worst_of([]), Quality::Good);

        // 公式点：输入 bad + 输入 good → 至少 bad；再叠加计算失败 → CalcFailed。
        let input_worst = Quality::worst_of([Quality::Bad, Quality::Good]);
        assert_eq!(input_worst, Quality::Bad);
        assert_eq!(input_worst.worst(Quality::CalcFailed), Quality::Bad);
        assert_eq!(
            Quality::worst_of([Quality::Good, Quality::Good]).worst(Quality::CalcFailed),
            Quality::CalcFailed,
            "clean inputs + failed evaluation = calc_failed"
        );

        // 可用性判定。
        assert!(Quality::Good.is_acceptable());
        assert!(Quality::Uncertain.is_acceptable());
        assert!(!Quality::Bad.is_acceptable());
        assert!(Quality::Bad.has_value());
        assert!(!Quality::CommError.has_value());
    }

    /// 质量码名称与北向 wire 枚举往返。
    #[test]
    fn quality_names_and_wire_roundtrip() {
        let all = [
            (Quality::Good, "GOOD"),
            (Quality::Uncertain, "UNCERTAIN"),
            (Quality::OutOfRange, "OUT_OF_RANGE"),
            (Quality::CalcFailed, "CALC_FAILED"),
            (Quality::Bad, "BAD"),
            (Quality::Timeout, "TIMEOUT"),
            (Quality::CommError, "COMM_ERROR"),
        ];
        for (q, name) in all {
            assert_eq!(q.as_str(), name);
            assert_eq!(Quality::from_name(name).expect("name"), q);
            assert_eq!(Quality::from_name(&name.to_lowercase()).expect("name"), q);
        }
        assert_eq!(Quality::from_name("nope"), None);

        // wire 映射：只有 Good/Uncertain 保留，其余归一为 Bad。
        assert_eq!(Quality::Good.to_wire(), WireQuality::Good);
        assert_eq!(Quality::Uncertain.to_wire(), WireQuality::Uncertain);
        for q in [
            Quality::Bad,
            Quality::Timeout,
            Quality::CommError,
            Quality::OutOfRange,
            Quality::CalcFailed,
        ] {
            assert_eq!(q.to_wire(), WireQuality::Bad, "{q:?}");
        }
        assert_eq!(Quality::from_wire(WireQuality::Bad), Quality::Bad);
        assert_eq!(Quality::from_wire(WireQuality::Good), Quality::Good);
        assert_eq!(Quality::from_wire(WireQuality::Simulated), Quality::Good);
        assert_eq!(
            Quality::from_wire(WireQuality::Unspecified),
            Quality::Uncertain
        );
    }

    /// float64 解码辅助（避免与 float32 同名混淆）。
    fn f32_of_x64(order: ByteOrder, bytes: &[u8]) -> f64 {
        dec(DataType::Float64, order)
            .decode(bytes)
            .expect("decode")
            .scaled
    }

    // ================= QA 独立验收（task 53 边界 / 反例） =================

    /// 按完整规格构造解码器。
    fn dec_full(spec: DecodeSpec) -> ValueDecoder {
        ValueDecoder::new(spec).expect("spec")
    }

    /// QA 边界：空负载 / 长度不足 / 多字节奇数长度一律 ProtocolError；
    /// 位访问不受「寄存器偶数」约束（该约束只针对定长数值）。
    #[test]
    fn qa_empty_odd_and_undersized_payloads_are_rejected() {
        for dt in [
            DataType::Bool,
            DataType::Bits,
            DataType::Int16,
            DataType::Uint16,
            DataType::Int32,
            DataType::Uint32,
            DataType::Float32,
            DataType::Float64,
        ] {
            let err = dec(dt, ByteOrder::Abcd)
                .decode(&[])
                .expect_err("empty payload must fail");
            assert_eq!(err.error_code(), crate::error::ERR_PROTOCOL, "{dt:?}");
        }
        // 定长数值：长度不足（3 < 4）。
        assert!(dec(DataType::Uint32, ByteOrder::Abcd)
            .decode(&[1, 2, 3])
            .is_err());
        // 多字节数值：奇数长度（5 字节 / 9 字节）必须显式报错，不静默截断或补零。
        let odd = dec(DataType::Int32, ByteOrder::Abcd)
            .decode(&[1, 2, 3, 4, 5])
            .expect_err("odd length must fail");
        assert!(
            odd.to_string().contains("odd payload length"),
            "actual: {odd}"
        );
        assert!(dec(DataType::Float64, ByteOrder::Dcba)
            .decode(&[0; 9])
            .is_err());
        // 8 位定长类型只取首字节，多余字节不构成错误。
        let out = dec(DataType::Uint8, ByteOrder::Abcd)
            .decode(&[7, 9, 9])
            .expect("u8");
        assert_eq!(out.value, DecodedValue::Uint(7));
        // 位串：奇数长度合法（3 字节 = 0x123456，取低 12 位 = 0x456）。
        let bits = dec_full(DecodeSpec::new(DataType::Bits, ByteOrder::Abcd).with_bits(0, 12));
        assert_eq!(
            bits.decode(&[0x12, 0x34, 0x56]).expect("bits").value,
            DecodedValue::Bits(0x456)
        );
    }

    /// QA 边界：单寄存器（2 字节）时 `CDAB` 无配对寄存器 → 退化原样；
    /// `BADC` 是「寄存器内字节互换」本义，两字节仍互换（二者语义不同，勿混）。
    #[test]
    fn qa_single_register_cdab_degrades_but_badc_swaps() {
        let bytes = [0x12u8, 0x34];
        let cdab = dec(DataType::Uint16, ByteOrder::Cdab)
            .decode(&bytes)
            .expect("cdab");
        assert_eq!(
            cdab.value,
            DecodedValue::Uint(0x1234),
            "CDAB 单寄存器退化原样"
        );
        let badc = dec(DataType::Uint16, ByteOrder::Badc)
            .decode(&bytes)
            .expect("badc");
        assert_eq!(
            badc.value,
            DecodedValue::Uint(0x3412),
            "BADC 两字节仍做寄存器内互换"
        );
        // 3 字节奇数位访问遇 DCBA 逆序：末字节无配对也不越界。
        let bits = dec_full(DecodeSpec::new(DataType::Bits, ByteOrder::Dcba).with_bits(0, 64));
        assert_eq!(
            bits.decode(&[1, 2, 3]).expect("bits").value,
            DecodedValue::Bits(0x03_02_01)
        );
    }

    /// QA 大数红线：`u64` 计数器全精度保留；超 `2^53 - 1` 必须编码为 JSON 字符串。
    #[test]
    fn qa_u64_precision_and_json_string_boundary() {
        let out = dec(DataType::Uint64, ByteOrder::Abcd)
            .decode(&u64::MAX.to_be_bytes())
            .expect("u64");
        assert_eq!(out.value, DecodedValue::Uint(u64::MAX), "原始值不得降精度");
        assert_eq!(
            out.value.json_number().as_deref(),
            Some("\"18446744073709551615\"")
        );
        assert_eq!(
            out.value.as_f64(),
            u64::MAX as f64,
            "as_f64 允许损失（已文档化）"
        );

        const SAFE: u64 = 9_007_199_254_740_991;
        assert_eq!(
            DecodedValue::Uint(SAFE).json_number().as_deref(),
            Some("9007199254740991")
        );
        assert_eq!(
            DecodedValue::Uint(SAFE + 1).json_number().as_deref(),
            Some("\"9007199254740992\"")
        );
        assert_eq!(
            DecodedValue::Int(-(SAFE as i64)).json_number().as_deref(),
            Some("-9007199254740991")
        );
        assert_eq!(
            DecodedValue::Int(-(SAFE as i64) - 1)
                .json_number()
                .as_deref(),
            Some("\"-9007199254740992\"")
        );
        assert_eq!(
            DecodedValue::Int(i64::MIN).json_number().as_deref(),
            Some("\"-9223372036854775808\"")
        );
        assert_eq!(
            DecodedValue::Bool(true).json_number().as_deref(),
            Some("true")
        );
        assert_eq!(DecodedValue::Text("x".to_string()).json_number(), None);
    }

    /// QA 边界：负零（有限 → Good，符号可保留）、NaN / ±Inf（非有限 → Bad + JSON `null`）。
    #[test]
    fn qa_negative_zero_and_non_finite_floats() {
        const NEG_ZERO_BITS: u64 = 0x8000_0000_0000_0000;
        let plain = dec(DataType::Float64, ByteOrder::Abcd)
            .decode(&NEG_ZERO_BITS.to_be_bytes())
            .expect("f64");
        assert_eq!(plain.value, DecodedValue::Float(-0.0));
        // `-0.0 * 1.0 + 0.0` 在 IEEE754 下变回 `+0.0`（加法把符号吃掉）。
        assert_eq!(plain.scaled, 0.0);
        assert_eq!(plain.quality, Quality::Good, "负零是有限值");

        // offset = -0.0 时符号得以保留（-0.0 + -0.0 = -0.0）。
        let kept = dec_full(
            DecodeSpec::new(DataType::Float64, ByteOrder::Abcd).with_scale_offset(1.0, -0.0),
        )
        .decode(&NEG_ZERO_BITS.to_be_bytes())
        .expect("f64");
        assert!(kept.scaled.is_sign_negative(), "scaled={}", kept.scaled);

        for bits in [
            0x7FF0_0000_0000_0000u64, // +Inf
            0xFFF0_0000_0000_0000,    // -Inf
            0x7FF8_0000_0000_0000,    // NaN
        ] {
            let out = dec(DataType::Float64, ByteOrder::Abcd)
                .decode(&bits.to_be_bytes())
                .expect("f64");
            assert_eq!(out.quality, Quality::Bad, "bits {bits:#018X}");
            assert!(!out.scaled.is_finite());
            assert_eq!(
                out.value.json_number().as_deref(),
                Some("null"),
                "非有限浮点必须编为 null（JSON 无 Inf/NaN 字面量）"
            );
        }
    }

    /// QA 边界：`scale = 0`（输出恒为 offset）与缩放溢出（→ Inf ⇒ Bad）。
    #[test]
    fn qa_scale_zero_and_overflow() {
        let zero_scale = dec_full(
            DecodeSpec::new(DataType::Uint16, ByteOrder::Abcd).with_scale_offset(0.0, 5.5),
        );
        let out = zero_scale.decode(&[0x03, 0xE8]).expect("decode");
        assert_eq!(out.value, DecodedValue::Uint(1000));
        assert_eq!(out.scaled, 5.5, "scale=0 ⇒ 工程量恒为 offset");
        assert_eq!(out.quality, Quality::Good);

        let huge = dec_full(
            DecodeSpec::new(DataType::Uint16, ByteOrder::Abcd).with_scale_offset(f64::MAX, 0.0),
        );
        let out = huge.decode(&[0x00, 0x64]).expect("decode");
        assert!(out.scaled.is_infinite(), "100 × f64::MAX 必须溢出");
        assert_eq!(out.quality, Quality::Bad);
        assert_eq!(
            out.value.json_number().as_deref(),
            Some("100"),
            "原始值不被缩放污染"
        );
    }

    /// QA 边界：`valid_range` 对文本点位（工程量为 NaN）的判定；`worst` 顺序无关。
    #[test]
    fn qa_valid_range_on_text_and_worst_is_order_independent() {
        let mut spec = DecodeSpec::new(DataType::Text, ByteOrder::Abcd);
        spec.string = StringSpec {
            mode: StringMode::All,
            trim_nul: false,
            lossy: false,
        };
        spec.valid_range = Some((0.0, 100.0));
        let out = dec_full(spec).decode(b"HI").expect("decode");
        assert_eq!(out.value, DecodedValue::Text("HI".to_string()));
        assert!(out.scaled.is_nan());
        assert_eq!(
            out.quality,
            Quality::Good,
            "非数值点位不做数值范围判定：文本工程量为 NaN，配了 valid_range 也不得恒报越界（修复验收报告 P2）"
        );

        let all = [
            Quality::Good,
            Quality::Uncertain,
            Quality::OutOfRange,
            Quality::CalcFailed,
            Quality::Bad,
            Quality::Timeout,
            Quality::CommError,
        ];
        assert_eq!(Quality::worst_of(all), Quality::CommError);
        assert_eq!(
            Quality::worst_of(all.iter().rev().copied()),
            Quality::CommError
        );
        for a in all {
            for b in all {
                assert_eq!(a.worst(b), b.worst(a), "{a:?}/{b:?} 不满足交换律");
                assert_eq!(a.worst(b).severity(), a.severity().max(b.severity()));
            }
        }
        assert_eq!(
            Quality::worst_of(std::iter::empty::<Quality>()),
            Quality::Good,
            "空输入 = Good"
        );
    }

    /// QA 反例：字符串非法 UTF-8——`lossy=false` 报错、`lossy=true` 以 U+FFFD 替换；
    /// 定长 + 剥 NUL 组合。
    #[test]
    fn qa_invalid_utf8_strict_and_lossy() {
        let truncated = [0xE4u8, 0xBD]; // 3 字节序列被截断
        let mut strict = DecodeSpec::new(DataType::Text, ByteOrder::Abcd);
        strict.string = StringSpec {
            mode: StringMode::All,
            trim_nul: false,
            lossy: false,
        };
        let err = dec_full(strict)
            .decode(&truncated)
            .expect_err("strict must fail");
        assert_eq!(err.error_code(), crate::error::ERR_PROTOCOL);

        let mut lossy = DecodeSpec::new(DataType::Text, ByteOrder::Abcd);
        lossy.string = StringSpec {
            mode: StringMode::All,
            trim_nul: false,
            lossy: true,
        };
        let out = dec_full(lossy).decode(&truncated).expect("lossy");
        assert_eq!(out.value, DecodedValue::Text("\u{FFFD}".to_string()));
        assert_eq!(out.quality, Quality::Good, "文本不参与数值质量判定");

        let mut fixed = DecodeSpec::new(DataType::Text, ByteOrder::Abcd);
        fixed.string = StringSpec {
            mode: StringMode::Fixed(8),
            trim_nul: true,
            lossy: false,
        };
        let out = dec_full(fixed).decode(&[b'A', b'B', 0, 0]).expect("fixed");
        assert_eq!(out.value, DecodedValue::Text("AB".to_string()));
    }
}

//! task 39（联调压测）· 北向**双编码性能基线**与**公式求值开销基线**。
//!
//! 本文件是 task 39 在 `daemon` crate 侧的**集成测试**（`tests/`，只依赖 `daemon` 的公开 API），
//! 产出两项交付基线，不修改任何产品源码：
//!
//! 1. **双编码语义等价 + 性能基线**（`dual_encoding_semantics_identical` /
//!    `dual_encoding_scale_baseline`）：同一 [`TelemetryBatch`] 走 protobuf / JSON 两条
//!    编码路径，解码后必须语义相等，且平台侧用于验签的语义摘要
//!    （[`semantic_digest`]）必须**逐字节相等**（编码与签名解耦的红线）；同时测量两者的
//!    字节数、编码 / 解码耗时，为「JSON 模式推荐设备上限」交付物提供原始数据。
//! 2. **公式求值开销基线**（`formula_overhead_baseline`）：0 / 50 / 200 个计算点下，每采集
//!    周期 [`FormulaEngine::eval_cycle`] 的墙钟耗时（cycles/ms 与每点增量代理），并证明
//!    新增计算点不会污染物理点结果。
//! 3. **AST 只编译一次**（`formula_ast_compiled_once`）：用 [`FormulaEngine::compile_count`]
//!    这一**直接可观测量**断言 N 周期求值期间表达式解析次数不增长（非间接代理）。
//! 4. **失败 → 质量码语义**（`formula_failure_quality_semantics`）：除零 / 缺失（超时）输入 /
//!    结果为 NaN 一律收敛为 `CalcFailed`（severity 3），且合并逻辑**不得误用 `Bad`** 掩盖它。
//!
//! 运行（带输出）：`cargo test -p daemon --test encoding_formula_baseline -- --nocapture`

use std::time::Instant;

use daemon::codec::Quality as UnifiedQuality;
use daemon::formula::{
    CalcFailure, DerivedPointConfig, EvalMode, FormulaEngine, InputValue, PointValues,
};
use daemon::north::encoder::{
    decode_batch, sample_to_data_point, semantic_digest, BatchEncoder, Encoding, JsonEncoder,
    ProtobufEncoder, F64_MARKER_INF, F64_MARKER_NAN, JSON_SAFE_INTEGER_MAX, VALUE_TYPE_BLOB,
    VALUE_TYPE_F64LE,
};
use daemon::pipeline::ProcessedSample;
use protocol_proto::{DataPoint, Quality as WireQuality, TelemetryBatch};

/// 19 位纳秒时间戳，恒大于 [`JSON_SAFE_INTEGER_MAX`]（2^53−1）→ JSON 路径必须字符串化。
const BIG_TS_NS: i64 = 1_763_000_000_000_000_001;

/// 固定物理点数量（公式基线用）。
const PHYSICAL: usize = 8;

/// 构造一个 `ProcessedSample`（统一 `kPa` / 无设备时间戳）。
fn sample(device_id: &str, point_id: &str, value: f64, ts_ns: i64) -> ProcessedSample {
    ProcessedSample {
        device_id: device_id.to_string(),
        point_id: point_id.to_string(),
        value,
        unit: "kPa".to_string(),
        device_ts_ns: None,
        collected_ts_ns: ts_ns,
        quality: WireQuality::Good,
    }
}

/// 边界批次：同时含「超 2^53 的 uint64 计数器」「NaN」「+Inf」「blob」「普通浮点」，
/// 外加超 2^53 的纳秒时间戳——覆盖双编码差异最大的全部取值域。
fn boundary_batch() -> TelemetryBatch {
    // uint64 计数器 = 2^53（> JSON_SAFE_INTEGER_MAX），其小端位模式非 NaN/±Inf 规范常量，
    // 因此 JSON 走 base64（f64le 前缀）而非 null 标记，可逐字节无损往返。
    let counter: u64 = (JSON_SAFE_INTEGER_MAX as u64) + 1;

    let counter_point = DataPoint {
        device_id: "meter-01".to_string(),
        point_id: "counter".to_string(),
        value: counter.to_le_bytes().to_vec(),
        unit: "count".to_string(),
        ts: BIG_TS_NS,
        quality: WireQuality::Good as i32,
    };
    // 非 8 字节负载 → `blob` 前缀（未知类型原始字节）。
    let blob_point = DataPoint {
        device_id: "meter-01".to_string(),
        point_id: "blob".to_string(),
        value: vec![0xde, 0xad, 0xbe, 0xef, 0x01],
        unit: "raw".to_string(),
        ts: BIG_TS_NS,
        quality: WireQuality::Good as i32,
    };

    // NaN / +Inf 经 `sample_to_data_point` 归一：value 位模式保留，quality 强制降为 BAD
    // （protobuf / JSON 共用同一函数，故两条编码路径质量码一致）。
    let normal = sample_to_data_point(&sample("pump-01", "inlet_temp", 36.6, BIG_TS_NS));
    let nan = sample_to_data_point(&sample("pump-01", "nan_sensor", f64::NAN, BIG_TS_NS));
    let inf = sample_to_data_point(&sample("pump-01", "inf_sensor", f64::INFINITY, BIG_TS_NS));

    TelemetryBatch {
        points: vec![normal, nan, inf, counter_point, blob_point],
        ts: BIG_TS_NS,
        gateway_id: "gw-baseline-001".to_string(),
        auth: None,
    }
}

// ---- 1. 双编码语义一致（含大整数 / NaN / ±Inf / blob 边界） ----

/// QA：同一批次（含 2^53 计数器、NaN、+Inf、blob、纳秒时间戳）经 protobuf / JSON 两条编码
/// 路径往返后**语义逐字段相等**，且 `semantic_digest` 逐字节相等（验签解耦）。
/// 另断言 JSON 路径把超 2^53 的整数编码为**字符串**（无精度丢失）、NaN/±Inf 编码为
/// `null` + `value_f64` 标记并降级为 BAD。
#[test]
fn dual_encoding_semantics_identical() {
    let batch = boundary_batch();
    let counter: u64 = (JSON_SAFE_INTEGER_MAX as u64) + 1;

    let pb_bytes = ProtobufEncoder
        .encode_batch(&batch)
        .expect("protobuf encode");
    let json_bytes = JsonEncoder.encode_batch(&batch).expect("json encode");

    let from_pb = decode_batch(Encoding::Protobuf, &pb_bytes).expect("protobuf decode");
    let from_json = decode_batch(Encoding::Json, &json_bytes).expect("json decode");

    // 语义等价：两条路径都能无损还原原批次（逐点 / 逐字段 / 质量码）。
    assert_eq!(from_pb.points, batch.points, "protobuf 往返逐点不等");
    assert_eq!(from_json.points, batch.points, "JSON 往返逐点不等");
    assert_eq!(from_pb.ts, batch.ts);
    assert_eq!(from_json.ts, batch.ts);
    assert_eq!(from_pb.gateway_id, batch.gateway_id);
    assert_eq!(from_json.gateway_id, batch.gateway_id);
    assert_eq!(from_pb.auth, from_json.auth);

    // 编码无关的语义摘要：protobuf 与 JSON 必须逐字节相等。
    assert_eq!(
        semantic_digest(&batch),
        semantic_digest(&from_pb),
        "protobuf 往返后语义摘要漂移"
    );
    assert_eq!(
        semantic_digest(&from_pb),
        semantic_digest(&from_json),
        "protobuf 与 JSON 两条编码路径的语义摘要必须逐字节相等"
    );

    // ---- JSON 结构断言 ----
    let root: serde_json::Value =
        serde_json::from_slice(&json_bytes).expect("output is valid json");

    // 超 2^53 的纳秒时间戳（批次 + 每个点位）必须字符串化，且字面量与 i64 完全一致。
    let big_ts_text = BIG_TS_NS.to_string();
    assert!(
        root["ts"].is_string(),
        "批次纳秒时间戳必须是 JSON 字符串，实际 {}",
        root["ts"]
    );
    assert_eq!(root["ts"].as_str(), Some(big_ts_text.as_str()));

    let json_points = root["points"].as_array().expect("points is an array");
    assert_eq!(json_points.len(), batch.points.len());
    for (idx, point) in json_points.iter().enumerate() {
        assert!(
            point["ts"].is_string(),
            "point[{idx}] 纳秒时间戳必须字符串化，实际 {}",
            point["ts"]
        );
        assert_eq!(point["ts"].as_str(), Some(big_ts_text.as_str()));
    }

    // 点位顺序与输入一致（JSON array 保序）：0 普通 / 1 NaN / 2 +Inf / 3 计数器 / 4 blob。
    assert_eq!(json_points[0]["point_id"].as_str(), Some("inlet_temp"));
    assert_eq!(json_points[1]["point_id"].as_str(), Some("nan_sensor"));
    assert_eq!(json_points[2]["point_id"].as_str(), Some("inf_sensor"));
    assert_eq!(json_points[3]["point_id"].as_str(), Some("counter"));
    assert_eq!(json_points[4]["point_id"].as_str(), Some("blob"));

    // NaN → value=null + value_f64="NaN" + 质量码 BAD(3)。
    assert!(
        json_points[1]["value"].is_null(),
        "NaN 的 value 必须为 null"
    );
    assert_eq!(json_points[1]["value_f64"].as_str(), Some(F64_MARKER_NAN));
    assert_eq!(
        json_points[1]["quality_code"].as_i64(),
        Some(i64::from(WireQuality::Bad as i32))
    );
    // +Inf → value=null + value_f64="Infinity" + 质量码 BAD(3)。
    assert!(
        json_points[2]["value"].is_null(),
        "+Inf 的 value 必须为 null"
    );
    assert_eq!(json_points[2]["value_f64"].as_str(), Some(F64_MARKER_INF));
    assert_eq!(
        json_points[2]["quality_code"].as_i64(),
        Some(i64::from(WireQuality::Bad as i32))
    );

    // bytes 类型前缀：8 字节数值载荷 → f64le；非 8 字节 → blob。
    assert_eq!(
        json_points[3]["value"]["t"].as_str(),
        Some(VALUE_TYPE_F64LE),
        "计数器为 8 字节数值载荷，前缀必须是 f64le"
    );
    assert_eq!(
        json_points[4]["value"]["t"].as_str(),
        Some(VALUE_TYPE_BLOB),
        "非 8 字节载荷前缀必须是 blob"
    );

    // 解码侧还原：计数器逐字节无损；NaN 位模式精确还原且质量码 BAD。
    let decoded_counter = from_json
        .points
        .iter()
        .find(|p| p.point_id == "counter")
        .expect("counter point present");
    assert_eq!(
        decoded_counter.value,
        counter.to_le_bytes().to_vec(),
        "uint64 计数器必须逐字节无损（JSON base64 路径）"
    );
    let decoded_nan = from_json
        .points
        .iter()
        .find(|p| p.point_id == "nan_sensor")
        .expect("nan point present");
    assert_eq!(decoded_nan.value, f64::NAN.to_le_bytes().to_vec());
    assert_eq!(decoded_nan.quality, WireQuality::Bad as i32);

    println!(
        "dual_encoding_semantics_identical: points={} protobuf={} B json={} B (json/proto={:.2}x) \
         digest={}",
        batch.points.len(),
        pb_bytes.len(),
        json_bytes.len(),
        json_bytes.len() as f64 / pb_bytes.len() as f64,
        hex32(&semantic_digest(&from_json)),
    );
}

// ---- 2. 规模化双编码性能基线 ----

/// 双编码**规模化性能基线**：200 点真实规模批次，测量并打印 protobuf vs JSON 的字节数、
/// 编码 / 解码耗时。
///
/// **「JSON 模式推荐设备上限」交付基线**：JSON 相对 protobuf 的**体积膨胀比**与
/// **编解码耗时比**即为压缩带宽 / CPU 预算约束——在固定网关吞吐预算下，
/// `可承载点位上限 ≈ protobuf 上限 / 体积膨胀比`（体积受带宽约束）且需同时满足
/// `CPU = 点位 × JSON 单点耗时` 的预算。本测试输出的 `json/proto` 比例即该交付物的原始系数。
#[test]
fn dual_encoding_scale_baseline() {
    const POINTS: usize = 200;
    const ITER: u32 = 50;

    let mut points = Vec::with_capacity(POINTS);
    for index in 0..POINTS {
        let value = 20.0 + (index as f64) * 0.5;
        points.push(sample_to_data_point(&sample(
            &format!("dev-{}", index % 16),
            &format!("point_{index}"),
            value,
            BIG_TS_NS + index as i64,
        )));
    }
    let batch = TelemetryBatch {
        points,
        ts: BIG_TS_NS,
        gateway_id: "gw-scale-001".to_string(),
        auth: None,
    };

    let pb_bytes = ProtobufEncoder
        .encode_batch(&batch)
        .expect("protobuf encode");
    let json_bytes = JsonEncoder.encode_batch(&batch).expect("json encode");

    // 编码耗时。
    let start = Instant::now();
    for _ in 0..ITER {
        let _ = ProtobufEncoder
            .encode_batch(&batch)
            .expect("protobuf encode");
    }
    let pb_encode_us = start.elapsed().as_secs_f64() * 1e6 / f64::from(ITER);

    let start = Instant::now();
    for _ in 0..ITER {
        let _ = JsonEncoder.encode_batch(&batch).expect("json encode");
    }
    let json_encode_us = start.elapsed().as_secs_f64() * 1e6 / f64::from(ITER);

    // 解码耗时。
    let start = Instant::now();
    for _ in 0..ITER {
        let _ = decode_batch(Encoding::Protobuf, &pb_bytes).expect("protobuf decode");
    }
    let pb_decode_us = start.elapsed().as_secs_f64() * 1e6 / f64::from(ITER);

    let start = Instant::now();
    for _ in 0..ITER {
        let _ = decode_batch(Encoding::Json, &json_bytes).expect("json decode");
    }
    let json_decode_us = start.elapsed().as_secs_f64() * 1e6 / f64::from(ITER);

    // 语义自检：规模路径同样不得漂移。
    let from_pb = decode_batch(Encoding::Protobuf, &pb_bytes).expect("protobuf decode");
    let from_json = decode_batch(Encoding::Json, &json_bytes).expect("json decode");
    assert_eq!(from_pb.points, batch.points, "protobuf 规模往返逐点不等");
    assert_eq!(from_json.points, batch.points, "JSON 规模往返逐点不等");
    assert_eq!(
        semantic_digest(&from_pb),
        semantic_digest(&from_json),
        "规模化批次两条编码路径语义摘要必须相等"
    );

    let size_ratio = json_bytes.len() as f64 / pb_bytes.len() as f64;
    println!("dual_encoding_scale_baseline: points={POINTS} iterations={ITER}");
    println!(
        "  size:   protobuf={} B json={} B ratio={size_ratio:.2}x delta={} B",
        pb_bytes.len(),
        json_bytes.len(),
        json_bytes.len() as i64 - pb_bytes.len() as i64
    );
    println!(
        "  encode: protobuf={pb_encode_us:.1} µs/op json={json_encode_us:.1} µs/op ratio={:.2}x",
        json_encode_us / pb_encode_us
    );
    println!(
        "  decode: protobuf={pb_decode_us:.1} µs/op json={json_decode_us:.1} µs/op ratio={:.2}x",
        json_decode_us / pb_decode_us
    );
    println!(
        "  per-point: protobuf={:.1} B/pt json={:.1} B/pt",
        pb_bytes.len() as f64 / POINTS as f64,
        json_bytes.len() as f64 / POINTS as f64
    );
}

// ---- 3. 公式求值开销基线 ----

/// 固定物理点输入集（8 个良好数值点）。
fn physical_inputs() -> PointValues {
    let mut map = PointValues::new();
    for index in 0..PHYSICAL {
        map.insert(
            format!("PHY_{index}"),
            InputValue::good(10.0 + index as f64),
        );
    }
    map
}

/// 构建含 `calc_points` 个计算点的引擎；每个计算点 `R_i = [PHY_{i%8}] * 2 + i`，
/// 触发模式设为 [`EvalMode::Periodic`]（每周期都求值，才能量到真实求值开销，而非跳过路径）。
fn derived_level(calc_points: usize) -> FormulaEngine {
    let configs: Vec<DerivedPointConfig> = (0..calc_points)
        .map(|index| {
            let mut cfg = DerivedPointConfig::new(
                &format!("R_{index}"),
                &format!("[PHY_{}] * 2 + {index}", index % PHYSICAL),
            );
            cfg.unit = "kW".to_string();
            cfg.eval_mode = EvalMode::Periodic;
            cfg
        })
        .collect();
    FormulaEngine::new(configs).expect("engine builds")
}

/// `R_i` 的期望值：`PHY_{i%8} * 2 + i`。
fn expected_derived(index: usize) -> f64 {
    (10.0 + (index % PHYSICAL) as f64) * 2.0 + index as f64
}

/// 公式求值**开销基线**：0 / 50 / 200 计算点下，100 周期的 `eval_cycle` 墙钟耗时，
/// 输出每周期耗时、cycles/ms 与每点增量代理；并断言各层级结果正确、且新增计算点不会
/// 污染物理点结果（入参物理点表在求值前后逐字段不变）。
#[test]
fn formula_overhead_baseline() {
    const CYCLES: u32 = 100;

    let inputs = physical_inputs();

    for &level in &[0usize, 50, 200] {
        let mut engine = derived_level(level);
        let inputs_before = inputs.clone();

        let start = Instant::now();
        for cycle in 0..CYCLES {
            let ts = BIG_TS_NS + i64::from(cycle) * 1_000_000_000;
            let _ = engine.eval_cycle(&inputs, ts);
        }
        let elapsed = start.elapsed();

        let total_us = elapsed.as_secs_f64() * 1e6;
        let per_cycle_us = total_us / f64::from(CYCLES);
        let cycles_per_ms = f64::from(CYCLES) / (elapsed.as_secs_f64() * 1e3);
        let per_point_us = if level == 0 {
            0.0
        } else {
            per_cycle_us / level as f64
        };
        println!(
            "formula_overhead_baseline: level={level:>3} cycles={CYCLES} total={total_us:.1} µs \
             per_cycle={per_cycle_us:.2} µs ({cycles_per_ms:.0} cycles/ms) per_point={per_point_us:.3} µs"
        );

        // 入参物理点表不得被求值改动（物理点结果不受计算点数量影响）。
        assert_eq!(
            inputs, inputs_before,
            "eval_cycle 不得改动入参物理点表（level={level}）"
        );
        assert!(
            engine.order().len() == level,
            "拓扑序长度应等于计算点数（level={level}）"
        );

        // 额外跑一周期（不计时）用于正确性断言。
        let results = engine.eval_cycle(&inputs, BIG_TS_NS + i64::from(CYCLES) * 1_000_000_000);
        if level == 0 {
            assert!(results.is_empty(), "0 计算点应无派生结果");
            continue;
        }
        assert_eq!(results.len(), level, "派生结果数应等于计算点数");
        for index in 0..level {
            let id = format!("R_{index}");
            let result = results
                .iter()
                .find(|r| r.point_id == id)
                .unwrap_or_else(|| panic!("level={level} 缺少派生结果 {id}"));
            assert!(!result.failed, "{id} 不应失败: {:?}", result.reason);
            assert_eq!(
                result.value,
                Some(expected_derived(index)),
                "{id} 值错误（新增计算点污染了结果？）"
            );
            assert_eq!(result.quality, UnifiedQuality::Good, "{id} 质量码错误");
        }
    }
}

// ---- 4. AST 只编译一次（直接可观测量） ----

/// 断言表达式 AST **只解析 / 编译一次**，多周期求值期间不重新解析。
///
/// 本仓库在 `formula.rs` 提供了**直接可观测量** [`FormulaEngine::compile_count`]
/// （「实际解析次数」，`formula.rs` 的 `compile_cached` 仅按表达式文本做编译缓存），
/// 因此本测试做**直接断言**（非间接代理）：构造后解析次数 == distinct 表达式数，
/// 且 N 周期求值后该计数**不增长**。
#[test]
fn formula_ast_compiled_once() {
    const POINTS: usize = 20;
    const CYCLES: u32 = 100;

    let inputs = physical_inputs();

    let configs: Vec<DerivedPointConfig> = (0..POINTS)
        .map(|index| {
            DerivedPointConfig::new(&format!("R_{index}"), &format!("[PHY_0] * 2 + {index}"))
        })
        .collect();
    let mut engine = FormulaEngine::new(configs).expect("engine builds");

    // 直接可观测量：distinct 表达式数 == 编译次数（20 条不同表达式 → 20 次）。
    assert_eq!(
        engine.compile_count(),
        POINTS,
        "20 条不同表达式应恰好编译 {POINTS} 次"
    );
    assert!(
        engine.program("R_0").is_some(),
        "编译产物（AST）必须已缓存，供求值期复用"
    );

    let compiled_once = engine.compile_count();
    for cycle in 0..CYCLES {
        let ts = BIG_TS_NS + i64::from(cycle) * 1_000_000_000;
        let _ = engine.eval_cycle(&inputs, ts);
    }
    assert_eq!(
        engine.compile_count(),
        compiled_once,
        "N 周期求值后解析次数不得增长（AST 必须只编译一次并在求值期复用）：\
         before={compiled_once} after={}",
        engine.compile_count()
    );

    // 去重：10 个计算点共用同一表达式 → 只编译 1 次，且求值不触发重新解析。
    let shared: Vec<DerivedPointConfig> = (0..10)
        .map(|index| DerivedPointConfig::new(&format!("S_{index}"), "[PHY_0] + 1"))
        .collect();
    let mut shared_engine = FormulaEngine::new(shared).expect("engine builds");
    assert_eq!(
        shared_engine.compile_count(),
        1,
        "同一表达式应共享编译产物（编译缓存按表达式文本去重）"
    );
    for cycle in 0..CYCLES {
        let ts = BIG_TS_NS + i64::from(cycle) * 1_000_000_000;
        let _ = shared_engine.eval_cycle(&inputs, ts);
    }
    assert_eq!(
        shared_engine.compile_count(),
        1,
        "求值不得触发重新解析（共享表达式仍只编译 1 次）"
    );

    println!(
        "formula_ast_compiled_once: distinct_exprs={POINTS} compiled={compiled_once} \
         shared_expr_points=10 compiled=1 （直接经 FormulaEngine::compile_count 断言）"
    );
}

// ---- 5. 失败 → 质量码语义 ----

/// 断言除零 / 缺失（超时）输入 / 结果为 NaN 一律收敛为 `CalcFailed`（severity 3，
/// `CalcFailed=3 < Bad=4 < Timeout=5`），并验证 `Quality::worst` 合并语义：
/// **失败合并不得误用 `Bad`**（否则严重度更低的 `CalcFailed` 会被永久掩盖）；
/// 干净输入 + 计算失败必须恰为 `CalcFailed`（而非 `Bad`）。
#[test]
fn formula_failure_quality_semantics() {
    // 严重度序（来自 codec::Quality::severity）。这是「失败合并不得用 Bad」的前提。
    assert_eq!(UnifiedQuality::CalcFailed.severity(), 3);
    assert!(
        UnifiedQuality::CalcFailed.severity() < UnifiedQuality::Bad.severity(),
        "CalcFailed 必须优于 Bad"
    );
    assert!(
        UnifiedQuality::Bad.severity() < UnifiedQuality::Timeout.severity(),
        "Bad 必须优于 Timeout"
    );

    let mut engine = FormulaEngine::new(vec![
        DerivedPointConfig::new("R_DivZero", "[A] / [B]"),
        DerivedPointConfig::new("R_Missing", "[A] + [MISSING]"),
        DerivedPointConfig::new("R_NaN", "sqrt([NEG])"),
        DerivedPointConfig::new("R_InheritBad", "[A] + [C]"),
    ])
    .expect("engine builds");

    let mut inputs = PointValues::new();
    inputs.insert("A".to_string(), InputValue::good(4.0));
    // 除零：B = 0。
    inputs.insert("B".to_string(), InputValue::good(0.0));
    // 缺失 / 超时输入：携带 Timeout 仅用于入口甄别与回显，**不进 worst()**。
    inputs.insert(
        "MISSING".to_string(),
        InputValue::Missing(UnifiedQuality::Timeout),
    );
    // 结果为 NaN：负数开方 → non_finite。
    inputs.insert("NEG".to_string(), InputValue::good(-4.0));
    // 继承最差输入质量：C 为 Bad 数值点。
    inputs.insert(
        "C".to_string(),
        InputValue::Numeric(1.0, UnifiedQuality::Bad),
    );

    let results = engine.eval_cycle(&inputs, BIG_TS_NS);
    let get = |id: &str| {
        results
            .iter()
            .find(|r| r.point_id == id)
            .unwrap_or_else(|| panic!("缺少派生结果 {id}"))
    };

    // 除零 → CalcFailed(3)，绝不能被 Bad(4) 掩盖。
    let div = get("R_DivZero");
    assert!(div.failed, "除零必须标记 failed");
    assert_eq!(div.reason, Some(CalcFailure::DivideByZero));
    assert_eq!(div.quality, UnifiedQuality::CalcFailed);
    assert_eq!(
        div.quality.severity(),
        3,
        "必须恰为 CalcFailed(3)；若合并误用 Bad 会得到 4"
    );

    // 缺失（且携带 Timeout）输入 → CalcFailed；携带的 Timeout 不被继承。
    let miss = get("R_Missing");
    assert!(miss.failed, "缺失输入必须标记 failed");
    assert_eq!(miss.reason, Some(CalcFailure::MissingInput));
    assert_eq!(
        miss.quality,
        UnifiedQuality::CalcFailed,
        "缺失输入的 Timeout 仅供回显，不得进 worst() 掩盖 CalcFailed"
    );
    assert_eq!(miss.quality.severity(), 3);

    // 结果为 NaN → CalcFailed。
    let nan = get("R_NaN");
    assert!(nan.failed, "NaN 结果必须标记 failed");
    assert_eq!(nan.reason, Some(CalcFailure::NonFinite));
    assert_eq!(nan.quality, UnifiedQuality::CalcFailed);
    assert_eq!(nan.quality.severity(), 3);

    // 继承最差输入质量（未失败）：Bad 输入 → 派生点 Bad，值仍正确。
    let inherit = get("R_InheritBad");
    assert!(!inherit.failed, "数值输入不应失败");
    assert_eq!(inherit.value, Some(5.0), "4 + 1 = 5");
    assert_eq!(inherit.quality, UnifiedQuality::Bad);

    // Quality::worst 合并语义：干净输入 + 计算失败 = CalcFailed（不被掩盖）；更差者胜、顺序无关。
    assert_eq!(
        UnifiedQuality::worst(UnifiedQuality::Good, UnifiedQuality::CalcFailed),
        UnifiedQuality::CalcFailed,
        "干净输入 + 失败必须浮出 CalcFailed"
    );
    assert_eq!(
        UnifiedQuality::worst(UnifiedQuality::CalcFailed, UnifiedQuality::Good),
        UnifiedQuality::CalcFailed,
        "worst 与顺序无关"
    );
    assert_eq!(
        UnifiedQuality::worst(UnifiedQuality::CalcFailed, UnifiedQuality::Bad),
        UnifiedQuality::Bad,
        "Bad(4) 劣于 CalcFailed(3)，合并取更差者 = Bad"
    );
    assert_eq!(
        UnifiedQuality::worst_of([UnifiedQuality::Good, UnifiedQuality::Good]),
        UnifiedQuality::Good
    );

    println!(
        "formula_failure_quality_semantics: div_zero={:?} missing={:?} nan={:?} inherit_bad={:?} \
         all_failures_are_calc_failed(3)=true",
        div.quality, miss.quality, nan.quality, inherit.quality
    );
}

/// `[u8; 32]` 的小写 hex（避免为测试引入额外依赖；`hex` 虽为依赖但此处仅用于可读输出）。
fn hex32(bytes: &[u8; 32]) -> String {
    let mut out = String::with_capacity(64);
    for byte in bytes {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

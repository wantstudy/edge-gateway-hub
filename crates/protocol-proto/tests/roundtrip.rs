//! Protobuf 往返测试（计划 task 2 QA 场景）。

use prost::Message;
use protocol_proto::pb::data_point::Quality;
use protocol_proto::{AuthBlock, DataPoint, TelemetryBatch};

/// QA: 构建 TelemetryBatch 含 3 个 DataPoint → 序列化 → 反序列化 → 所有字段一致。
#[test]
fn telemetry_batch_roundtrip_with_three_points() {
    let batch = TelemetryBatch {
        points: vec![
            DataPoint {
                device_id: "dev-01".into(),
                point_id: "p_temp".into(),
                value: vec![0x40, 0x59, 0x00, 0x00],
                unit: "\u{00b0}C".into(),
                ts: 1_700_000_000_123_456_789, // 纳秒级，> 2^53-1
                quality: Quality::Good as i32,
            },
            DataPoint {
                device_id: "dev-01".into(),
                point_id: "p_pressure".into(),
                value: vec![0x01, 0x02],
                unit: "kPa".into(),
                ts: 1_700_000_000_000_000_001,
                quality: Quality::Uncertain as i32,
            },
            DataPoint {
                device_id: "dev-02".into(),
                point_id: "p_sim".into(),
                value: vec![0xFF],
                unit: String::new(),
                ts: 0,
                quality: Quality::Simulated as i32,
            },
        ],
        ts: 1_700_000_001_999_999_999,
        gateway_id: "gw-alpha".into(),
        auth: Some(AuthBlock {
            mid: "a".repeat(64),
            nonce: "nonce-1".into(),
            ts: 1_700_000_001_999_999_999,
            sig: vec![0xAB; 64],
        }),
    };

    let encoded = batch.encode_to_vec();
    let decoded = TelemetryBatch::decode(encoded.as_slice()).expect("decode must succeed");

    assert_eq!(decoded.points.len(), 3);
    for (a, b) in batch.points.iter().zip(decoded.points.iter()) {
        assert_eq!(a.device_id, b.device_id);
        assert_eq!(a.point_id, b.point_id);
        assert_eq!(a.value, b.value);
        assert_eq!(a.unit, b.unit);
        assert_eq!(a.ts, b.ts);
        assert_eq!(a.quality, b.quality);
    }
    assert_eq!(batch.ts, decoded.ts);
    assert_eq!(batch.gateway_id, decoded.gateway_id);

    let auth = decoded.auth.expect("auth block must survive roundtrip");
    let orig = batch.auth.as_ref().expect("auth block was set");
    assert_eq!(auth.mid, orig.mid);
    assert_eq!(auth.nonce, orig.nonce);
    assert_eq!(auth.ts, orig.ts);
    assert_eq!(auth.sig, orig.sig);
}

/// QA: AuthBlock 结构正确 —— mid/nonce 为字符串、ts 为 int64、sig 为 bytes。
#[test]
fn auth_block_structure() {
    let block = AuthBlock {
        mid: "fingerprint-hex-64".to_string(),
        nonce: "0123456789abcdef".to_string(),
        ts: 1_700_000_000_000_000_000_i64,
        sig: vec![1u8, 2, 3, 4],
    };

    assert_eq!(block.mid, "fingerprint-hex-64");
    assert_eq!(block.nonce, "0123456789abcdef");
    assert_eq!(block.ts, 1_700_000_000_000_000_000_i64);
    assert_eq!(block.sig, vec![1u8, 2, 3, 4]);

    let decoded = AuthBlock::decode(block.encode_to_vec().as_slice()).expect("decode");
    assert_eq!(decoded, block);
}

/// int64 纳秒时间戳超过 2^53-1 时，protobuf int64 往返无损
/// —— 这正是 JSON 路径必须将 int64 编码为字符串的原因（schema 头注释约定 1）。
#[test]
fn int64_ts_beyond_2p53_roundtrip_exact() {
    let ts = 9_007_199_254_740_993_i64; // 2^53 + 1
    let msg = DataPoint {
        ts,
        ..DataPoint::default()
    };
    let decoded = DataPoint::decode(msg.encode_to_vec().as_slice()).expect("decode");
    assert_eq!(decoded.ts, ts, "int64 must survive exactly beyond 2^53-1");
}

/// quality 枚举编码值往返保持（含 BAD 与未初始化占位）。
#[test]
fn quality_enum_values_survive_roundtrip() {
    let cases = [
        (0, Quality::Unspecified),
        (1, Quality::Good),
        (2, Quality::Uncertain),
        (3, Quality::Bad),
        (4, Quality::Simulated),
    ];
    for (wire, variant) in cases {
        let msg = DataPoint {
            quality: variant as i32,
            ..DataPoint::default()
        };
        let decoded = DataPoint::decode(msg.encode_to_vec().as_slice()).expect("decode");
        assert_eq!(decoded.quality, wire);
    }
}

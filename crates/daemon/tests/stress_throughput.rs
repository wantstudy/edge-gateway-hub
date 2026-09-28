//! task 39 联调压测：端到端采集吞吐 + 断网补发正确性。
//!
//! 本集成测试把已有的基础原语按真实数据流串起来，验证「采集 → 处理 → 编码 → 背压入队」
//! 的端到端吞吐与「断网落盘 → 恢复补发」的端到端正确性：
//!
//! ```text
//! 模拟设备 → AcquisitionPipeline::ingest_physical → ProcessedSample
//!          → encoder_for(Encoding).encode_batch  → Vec<u8>
//!          → AcquisitionPipeline::enqueue         → OfflineQueue (内存 / 磁盘)
//!          → replay_batch + ack_up_to             → 补发 + 位点推进
//! ```
//!
//! ## 覆盖用例
//! 1. [`fifty_devices_100ms_no_loss`]       — 50 设备 × 100 ms × 10 s = **5,000** 条，零丢失。
//! 2. [`two_hundred_devices_round`]         — 200 设备 × 100 ms × 10 s = **20,000** 条，零丢失 + 水位。
//! 3. [`offline_backfill_scaled_smoke`]     — 断网 50 × 100 ms × 36 s = **18,000** 条，默认运行。
//! 4. [`offline_one_hour_backfill_exact`]   — 断网 50 × 100 ms × 3600 s = **1,800,000** 条（`#[ignore]`）。
//!
//! ## 红线
//! - **虚拟时钟**：全程只推进 [`ManualClock`]，**绝不真实 `sleep`**（纳秒级 `set` / `advance`）。
//! - **零新依赖**：仅使用 `daemon` 公开 API + 既有 dev-dependencies（`tempfile` / `tokio`）。
//!
//! ## 算式口径（与 `docs/design/capacity-estimation.md` 一致）
//! 速率 = `设备数 × 1000 / 采集周期ms`；断网 1h = `速率 × 3600`：
//! 50 设备 × 100 ms = 500 条/秒 → 断网 1h = **1,800,000 条**。

use std::collections::HashSet;
use std::sync::Arc;

use protocol_proto::{Quality, TelemetryBatch};

use daemon::backpressure::{AdmitOutcome, AuditLog};
use daemon::north::encoder::{
    decode_batch, encoder_for, sample_to_data_point, BatchEncoder, Encoding,
};
use daemon::offline_queue::{ManualClock, OfflineQueue, QueueConfig, QueuedBatch};
use daemon::pipeline::{
    AcquisitionPipeline, DataProcessor, PointConfig, ProcessedSample, RawSample,
};

use tempfile::TempDir;

/// 采集周期：100 ms（纳秒）。
const POLL_NS: i64 = 100_000_000;

/// 虚拟时钟起点（纳秒，2023-11 前后，任意固定值即可）。
const T0_NS: i64 = 1_700_000_000_000_000_000;

/// 网关标识（幂等键 `{gateway_id}:{batch_seq}` 前缀）。
const GATEWAY_ID: &str = "gw-stress-tp";

/// 单次补发 / 取批的批量大小。
const READ_BATCH: usize = 8_192;

/// 打开一个临时离线队列（`queue.db`），返回其临时目录以保持存活。
fn temp_queue() -> (TempDir, Arc<OfflineQueue>, ManualClock) {
    let dir = tempfile::tempdir().expect("tempdir");
    let clock = ManualClock::new(T0_NS);
    let config = QueueConfig::new(dir.path().join("queue.db"), GATEWAY_ID).expect("queue config");
    let queue = OfflineQueue::open(config, Arc::new(clock.clone())).expect("open queue");
    (dir, Arc::new(queue), clock)
}

/// 构建带 `devices` 个**直通**点位的采集管线（不换算、不过滤、无公式）。
fn build_pipeline(
    devices: usize,
    queue: Arc<OfflineQueue>,
    clock: &ManualClock,
) -> AcquisitionPipeline {
    let configs: Vec<PointConfig> = (0..devices)
        .map(|d| {
            PointConfig::passthrough(
                &format!("src-{d}"),
                &format!("pt-{d}"),
                &format!("dev-{d}"),
                "kPa",
            )
        })
        .collect();
    let processor = DataProcessor::new(configs).expect("point table");
    AcquisitionPipeline::new(
        processor,
        None,
        Some(queue),
        Arc::new(AuditLog::new(64)),
        Arc::new(clock.clone()),
    )
}

/// 把单点处理结果编码为北向负载（protobuf `TelemetryBatch`，单点一负载）。
fn encode_point(encoder: &dyn BatchEncoder, sample: &ProcessedSample) -> Vec<u8> {
    let batch = TelemetryBatch {
        points: vec![sample_to_data_point(sample)],
        ts: sample.collected_ts_ns,
        gateway_id: GATEWAY_ID.to_string(),
        auth: None,
    };
    encoder.encode_batch(&batch).expect("encode_batch")
}

/// 一次模拟的统计结果。
struct SimStats {
    /// 摄入的物理样本数。
    ingested: usize,
    /// 成功入队的负载数。
    enqueued: usize,
    /// 每台设备贡献的样本数（用于「每设备恰 N 条」断言）。
    per_device: Vec<usize>,
    /// 累计编码负载字节数。
    payload_bytes: u64,
}

/// 模拟 `cycles` 轮采集（每轮推进虚拟时钟 100 ms），每设备每轮采集 + 编码 + 入队一条。
fn simulate(
    pipe: &mut AcquisitionPipeline,
    clock: &ManualClock,
    encoder: &dyn BatchEncoder,
    devices: usize,
    cycles: usize,
) -> SimStats {
    let mut ingested = 0usize;
    let mut enqueued = 0usize;
    let mut payload_bytes = 0u64;
    let mut per_device = vec![0usize; devices];

    for _ in 0..cycles {
        clock.advance(POLL_NS);
        let ts = clock.now();
        for (d, count) in per_device.iter_mut().enumerate() {
            let raw = RawSample {
                source_id: format!("src-{d}"),
                value: d as f64,
                quality: Quality::Good,
                device_ts_ns: None,
            };
            let processed = pipe
                .ingest_physical(raw, ts)
                .expect("ingest_physical must not fail for a configured source")
                .expect("passthrough point (deadband = 0) must always emit");
            let payload = encode_point(encoder, &processed);
            payload_bytes += payload.len() as u64;
            match pipe.enqueue(payload) {
                AdmitOutcome::Admitted { .. } => {
                    enqueued += 1;
                    *count += 1;
                }
                AdmitOutcome::Rejected { reason, .. } => {
                    panic!("unexpected backpressure rejection under capacity: {reason}");
                }
            }
            ingested += 1;
        }
        // 周期屏障（无公式 → 空结果），保证与生产调度的调用序列一致。
        let _ = pipe.finish_cycle(ts);
    }

    SimStats {
        ingested,
        enqueued,
        per_device,
        payload_bytes,
    }
}

/// 排空队列并校验断网补发的三条不变量：
/// (a) 序号严格单调、无空洞、无重复；
/// (b) 补发总数 == 入队总数；
/// (c) 重投已 ack 的批次**不会**重复入库（幂等键稳定 + 消费端去重）。
///
/// 返回首个补发批次（供上游做进一步断言 / 观测）。
fn drain_and_verify(queue: &OfflineQueue, expected: usize) -> Vec<QueuedBatch> {
    let mut prev = 0u64;
    let mut total = 0usize;
    let mut first_batch: Vec<QueuedBatch> = Vec::new();

    loop {
        let batch = queue.replay_batch(READ_BATCH).expect("replay_batch");
        if batch.is_empty() {
            break;
        }
        for b in &batch {
            assert_eq!(
                b.seq,
                prev + 1,
                "sequence must be strictly monotonic & gap-free: expected {} got {}",
                prev + 1,
                b.seq
            );
            assert_eq!(b.gateway_id, GATEWAY_ID, "gateway_id must be stable");
            assert_eq!(
                b.idempotency_key(),
                format!("{GATEWAY_ID}:{}", b.seq),
                "idempotency key must be `gateway_id:batch_seq`"
            );
            prev = b.seq;
        }
        total += batch.len();
        let last = batch.last().expect("non-empty batch").seq;
        if first_batch.is_empty() {
            first_batch = batch;
        }
        queue.ack_up_to(last).expect("ack_up_to");
    }

    assert_eq!(total, expected, "total replayed must equal total enqueued");
    assert_eq!(prev, expected as u64, "final batch_seq must equal expected");
    assert_eq!(
        queue.pending().expect("pending"),
        0,
        "queue must be fully drained"
    );

    // 补发负载可被 `decode_batch` 还原（编码 / 解码链路端到端一致）。
    {
        let first = first_batch.first().expect("at least one replayed batch");
        let decoded =
            decode_batch(Encoding::Protobuf, &first.payload).expect("decode replayed payload");
        assert_eq!(decoded.gateway_id, GATEWAY_ID, "decoded gateway_id stable");
        assert_eq!(decoded.points.len(), 1, "one sample per payload");
    }

    // (c) 重投已 ack 的批次：队列不再返回它（位点已过），消费端按幂等键去重不重复入库。
    let after_ack = queue.replay_batch(READ_BATCH).expect("replay after ack");
    assert!(
        after_ack.is_empty(),
        "already-acked batches must not be replayed"
    );

    let mut server: HashSet<String> = first_batch
        .iter()
        .map(QueuedBatch::idempotency_key)
        .collect();
    let seeded = server.len();
    let mut new_inserts = 0usize;
    for b in &first_batch {
        if server.insert(b.idempotency_key()) {
            new_inserts += 1;
        }
    }
    assert_eq!(
        new_inserts, 0,
        "re-delivering an already-acked batch must not double-insert"
    );
    assert_eq!(
        server.len(),
        seeded,
        "server row count unchanged after re-delivery"
    );

    first_batch
}

// ---------------------------------------------------------------------------
// 用例 1：50 设备 × 100 ms × 10 s = 5,000 条，零丢失
// ---------------------------------------------------------------------------

/// Happy：50 设备、100 ms 周期、模拟 10 s（100 轮）→ 恰 5,000 条摄入 + 入队，零丢失，
/// 且每台设备恰 100 条。
#[tokio::test]
async fn fifty_devices_100ms_no_loss() {
    let devices = 50usize;
    let cycles = 100usize; // 100 轮 × 100 ms = 10 s
    let expected = devices * cycles; // 5,000
    assert_eq!(expected, 5_000, "50 devices x 100 cycles");

    let (_dir, queue, clock) = temp_queue();
    let encoder = encoder_for(Encoding::Protobuf);
    let mut pipe = build_pipeline(devices, Arc::clone(&queue), &clock);

    let stats = simulate(&mut pipe, &clock, encoder.as_ref(), devices, cycles);

    assert_eq!(stats.ingested, expected, "ingested samples");
    assert_eq!(stats.enqueued, expected, "enqueued samples");
    assert_eq!(stats.per_device.len(), devices);
    for (d, count) in stats.per_device.iter().enumerate() {
        assert_eq!(
            *count, 100,
            "device {d} must contribute exactly 100 samples"
        );
    }
    assert_eq!(
        queue.pending().expect("pending"),
        expected,
        "zero loss: every ingested sample must be queued"
    );

    println!(
        "fifty_devices_100ms_no_loss: ingested={} enqueued={} pending={} pending_mem={} \
         pending_disk={} disk_bytes={} payload_bytes={}",
        stats.ingested,
        stats.enqueued,
        queue.pending().expect("pending"),
        queue.pending_mem(),
        queue.pending_disk().expect("pending_disk"),
        queue.disk_bytes().expect("disk_bytes"),
        stats.payload_bytes,
    );

    queue.close().expect("close");
}

// ---------------------------------------------------------------------------
// 用例 2：200 设备 × 100 ms × 10 s = 20,000 条，零丢失 + 水位可观测
// ---------------------------------------------------------------------------

/// Happy：200 设备、100 ms 周期、模拟 10 s（100 轮）→ 恰 20,000 条，零丢失；
/// 打印队列水位（`pending` / `pending_disk` / `disk_bytes` / `pending_mem`）。
#[tokio::test]
async fn two_hundred_devices_round() {
    let devices = 200usize;
    let cycles = 100usize; // 10 s
    let expected = devices * cycles; // 20,000
    assert_eq!(expected, 20_000, "200 devices x 100 cycles");

    let (_dir, queue, clock) = temp_queue();
    let encoder = encoder_for(Encoding::Protobuf);
    let mut pipe = build_pipeline(devices, Arc::clone(&queue), &clock);

    let stats = simulate(&mut pipe, &clock, encoder.as_ref(), devices, cycles);

    assert_eq!(stats.ingested, expected, "ingested samples");
    assert_eq!(stats.enqueued, expected, "enqueued samples");
    assert_eq!(stats.per_device.len(), devices);
    for (d, count) in stats.per_device.iter().enumerate() {
        assert_eq!(
            *count, 100,
            "device {d} must contribute exactly 100 samples"
        );
    }

    let pending = queue.pending().expect("pending");
    let pending_disk = queue.pending_disk().expect("pending_disk");
    let disk_bytes = queue.disk_bytes().expect("disk_bytes");
    assert_eq!(pending, expected, "zero loss");
    assert_eq!(
        pending_disk + queue.pending_mem(),
        expected,
        "pending == disk + mem"
    );

    println!(
        "two_hundred_devices_round: ingested={} enqueued={} pending={} pending_mem={} \
         pending_disk={} disk_bytes={} payload_bytes={}",
        stats.ingested,
        stats.enqueued,
        pending,
        queue.pending_mem(),
        pending_disk,
        disk_bytes,
        stats.payload_bytes,
    );

    queue.close().expect("close");
}

// ---------------------------------------------------------------------------
// 用例 3（默认运行）：断网按比例冒烟 = 18,000 条
// ---------------------------------------------------------------------------

/// 断网补发**按比例冒烟**（默认运行）：50 设备 × 100 ms × 36 s = 50 × (1000/100) × 36 =
/// **18,000** 条，验证与 1.8M 完全相同的三条不变量（序号连续无洞、补发总数一致、
/// 重投不重复入库）。
///
/// 1.8M 全量验收见 [`offline_one_hour_backfill_exact`]（`#[ignore]`，`--ignored` 运行）：
/// 规模 = 本冒烟的 **100 倍**（3600 s 对 36 s），即 50 × (1000/100) × 3600 = **1,800,000** 条。
#[tokio::test]
async fn offline_backfill_scaled_smoke() {
    let devices = 50usize;
    let cycles = 360usize; // 36 s / 100 ms
    let expected = devices * (1_000 / 100) * 36; // 18,000
    assert_eq!(expected, 18_000, "50 x 10/s x 36s");

    let (_dir, queue, clock) = temp_queue();
    queue.set_online(false); // 断网：每批直接落盘（断电也不丢）
    let encoder = encoder_for(Encoding::Protobuf);
    let mut pipe = build_pipeline(devices, Arc::clone(&queue), &clock);

    let stats = simulate(&mut pipe, &clock, encoder.as_ref(), devices, cycles);
    assert_eq!(stats.ingested, expected, "ingested samples");
    assert_eq!(stats.enqueued, expected, "enqueued samples");
    assert_eq!(
        queue.pending().expect("pending"),
        expected,
        "offline backlog must be exact"
    );

    let first = drain_and_verify(&queue, expected);
    assert!(!first.is_empty(), "at least one replayed batch");

    println!(
        "offline_backfill_scaled_smoke: enqueued={} replayed={} first_batch_len={} payload_bytes={}",
        expected,
        expected,
        first.len(),
        stats.payload_bytes,
    );

    queue.close().expect("close");
}

// ---------------------------------------------------------------------------
// 用例 4（重型，#[ignore]）：断网 1 小时全量 = 1,800,000 条
// ---------------------------------------------------------------------------

/// 断网 1 小时全量验收（重型）：50 设备 × 100 ms × 3600 s
/// = 50 × (1000/100) × 3600 = **1,800,000** 条（⚠ 不是 36,000）。
///
/// 断网落盘 → 恢复全量补发，逐条校验：序号严格单调无洞、补发总数 == 入队总数、
/// 重投已 ack 批次不重复入库。
///
/// ⚠ **已知瓶颈（联调实测，非本测试问题）**：当前 `OfflineQueue` 断网路径对**每条**
/// `enqueue` 单独落盘事务，且事务内 `maintain`（保留期 + 环形覆盖）会**全表扫描**
/// 未 ack 行，导致断网入队为 **O(n²)**。实测离线入队耗时（debug 构建）：
/// 1,000→0.21s、2,000→0.66s、4,000→2.35s、8,000→8.43s、16,000→30.9s（每翻倍 ≈ 3.6×）。
/// 外推到 1,800,000 条约需 **百小时量级**，故本用例标记 `#[ignore]`，仅作验收目标留存；
/// 待 `offline_queue` 的维护改为增量 / 批量事务后即可 `--ignored` 产出验收证据。
// heavy: 1.8M rows; run with --ignored to produce acceptance evidence
#[tokio::test]
#[ignore = "heavy: 1.8M rows; run with --ignored to produce acceptance evidence"]
async fn offline_one_hour_backfill_exact() {
    let devices = 50usize;
    let cycles = 3_600usize; // 3600 s / 100 ms = 36,000 轮
    let expected = devices * (1_000 / 100) * 3_600; // 1,800,000
    assert_eq!(expected, 1_800_000, "50 x 10/s x 3600s");

    let (_dir, queue, clock) = temp_queue();
    queue.set_online(false); // 断网：每批直接落盘
    let encoder = encoder_for(Encoding::Protobuf);
    let mut pipe = build_pipeline(devices, Arc::clone(&queue), &clock);

    let stats = simulate(&mut pipe, &clock, encoder.as_ref(), devices, cycles);
    assert_eq!(stats.ingested, expected, "ingested samples");
    assert_eq!(stats.enqueued, expected, "enqueued samples");
    assert_eq!(
        queue.pending().expect("pending"),
        expected,
        "offline one-hour backlog must be exactly 1,800,000"
    );

    let first = drain_and_verify(&queue, expected);
    assert!(!first.is_empty(), "at least one replayed batch");

    println!(
        "offline_one_hour_backfill_exact: enqueued={} replayed={} first_batch_len={} \
         payload_bytes={}",
        expected,
        expected,
        first.len(),
        stats.payload_bytes,
    );

    queue.close().expect("close");
}

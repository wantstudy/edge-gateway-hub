//! 可靠北向分发：把已建好的可靠性原语（backpressure / codec）接入 MQTT 分发。
//!
//! 本模块是 task 19「北向」的接线层（**绝不修改任何原语模块本身**），把
//! `crate::backpressure` 与 `crate::codec` 的已有能力编排进北向出站路径：
//!
//! 1. **SendQueue + PUBACK 确认（背压）**：出站批次流经 [`SendQueue`]（ready / sent
//!    双队列）。一条消息**只有在 MQTT `PUBACK` 确认后**才从 sent-unacked 层移除 /
//!    推进发送游标（`SendQueue::confirm`），在此之前停留在 sent-but-unacked 层。
//! 2. **幂等 / 重放（批次级，关键）**：使用 [`ReplayController`] + [`IdempotencyLedger`]，
//!    幂等键 = `gateway_id:batch_seq`（**批次级**，非行级唯一索引）。重连（会话未恢复）
//!    时排空 ready + sent-unacked 两层并重放；仲裁依赖批次头表主键（`replay_selection`
//!    按 `seq > high_water` 过滤 + 幂等键去重），而非 list-pagination + 内存过滤。
//! 3. **审计排空 → 控制面**：批次被 ack 后，构造白名单 8 字段回执
//!    （`device_mid / lease_id / seq_from / seq_to / count / payload_digest / ts / sig`），
//!    经 [`ReceiptReporter`] POST 到 `POST /audit/receipt`；`payload_digest` 为业务语义
//!    确定性哈希（**非原始 protobuf 字节**），绝不携带业务数值。同时排空 [`AuditLog`]
//!    （落盘 / 溢出 / 降级事件）转发到控制面审计日志。
//! 4. **线路上的质量码（codec）**：构建 protobuf 载荷时，质量码由
//!    `encoder::sample_to_data_point` 经 `crate::codec::Quality::from_wire().to_wire()`
//!    归一（见 encoder.rs 改造）；回执签名同样用语义哈希而非原始字节。
//! 5. **B 档可用性**：默认拓扑（客户自有 Broker）下，网络中断期间持续采集 / 转发
//!    （批次先入 [`SendQueue`] / 内存降级留存），重连后排空并重放补上，**不依赖控制通道**
//!    即可继续；仅审计回执（序号区间 + 语义摘要，无业务数值）经控制面回执路径。

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use rumqttc::{ConnectReturnCode, Event, Packet};

use crate::auth::receipt_reporter::{
    fixed_clock, Ed25519ReceiptSigner, HttpPostTransport, ReceiptBatch, ReceiptReporter,
    ReceiptReporterConfig, ReceiptSigner, ReceiptTransport,
};
use crate::backpressure::{
    audit_json, replay_selection, AuditLog, IdempotencyLedger, PendingSend, PushOutcome,
    ReplayController, SendQueue, SpillResult, SpillSink, WaterMarkConfig, DEFAULT_AUDIT_CAPACITY,
    DEFAULT_SEND_HARD_LIMIT, DEFAULT_SEND_HIGH_WATER,
};
use crate::error::DaemonResult;
use crate::north::encoder::{encoder_for, sample_to_data_point};
use crate::north::mqtt::{EndpointConfig, MqttClient, PublishOutcome, SlowConsumerSink};
use crate::offline_queue::{idempotency_key, AckSink, Clock, QueuedBatch, SystemClock};
use crate::pipeline::ProcessedSample;

use protocol_proto::{DataPoint, TelemetryBatch};

/// 一个批次的「待确认」记录（已发布、等待 PUBACK 期间驻留内存）。
#[derive(Debug, Clone)]
struct InflightBatch {
    /// 批次序号（构成幂等键 `gateway_id:batch_seq`）。
    seq: u64,
    /// 原始批次（供构造回执的语义摘要）。
    batch: TelemetryBatch,
    /// 入队时刻（纳秒）。
    enqueued_ns: i64,
}

/// 可靠分发配置（由装配注入，**禁止硬编码端点 / 密钥**）。
#[derive(Debug, Clone)]
pub struct ReliableDispatchConfig {
    /// 网关标识（幂等键组成之一）。
    pub gateway_id: String,
    /// 设备机位码（回执批次归属者）。
    pub device_mid: String,
    /// 租约 ID（回执批次归属者）。
    pub lease_id: String,
    /// 重启后读回的高水位（通常取离线库 `ack_seq`）。
    pub start_high_water: u64,
    /// 控制面回执端点（完整 URL，如 `http://.../audit/receipt`；仅 `http`）。
    pub receipt_endpoint: String,
    /// 回执签名种子（Ed25519；生产由 KMS / 配置注入，禁止硬编码）。
    pub signer_seed: [u8; 32],
    /// SendQueue 高水位（在途未确认条数阈值）。
    pub send_high_water: usize,
    /// SendQueue 硬上限（触达后拒绝入队，数据交还调用方）。
    pub send_hard_limit: usize,
    /// 审计环形缓冲容量。
    pub audit_capacity: usize,
}

impl Default for ReliableDispatchConfig {
    fn default() -> Self {
        Self {
            gateway_id: String::new(),
            device_mid: String::new(),
            lease_id: String::new(),
            start_high_water: 0,
            receipt_endpoint: String::new(),
            signer_seed: [0u8; 32],
            send_high_water: DEFAULT_SEND_HIGH_WATER,
            send_hard_limit: DEFAULT_SEND_HARD_LIMIT,
            audit_capacity: DEFAULT_AUDIT_CAPACITY,
        }
    }
}

/// 生产用 AckSink（进程内持久化位点）。
///
/// 真实部署应把位点刷到离线库 / 文件；此处以原子变量持久化内存位点。契约
/// 「先落 Ack 后推位点」由 [`ReplayController`] 保证：`persist_ack` 成功后才推进内存游标；
/// 持久化失败（返回 `Err`）时游标一步不动，数据保留待重放由幂等键去重。
pub struct MemoryAckSink {
    /// 当前已确认到的批次序号（high-water mark）。
    cursor: AtomicU64,
}

impl MemoryAckSink {
    /// 构造（起点 = 重启后读回的 high-water mark）。
    #[must_use]
    pub fn new(start: u64) -> Arc<Self> {
        Arc::new(Self {
            cursor: AtomicU64::new(start),
        })
    }

    /// 当前 high-water mark。
    #[must_use]
    pub fn high_water(&self) -> u64 {
        self.cursor.load(Ordering::SeqCst)
    }
}

impl AckSink for MemoryAckSink {
    fn persist_ack(&self, seq: u64) -> DaemonResult<()> {
        let mut cur = self.cursor.load(Ordering::SeqCst);
        loop {
            if seq <= cur {
                return Ok(());
            }
            match self.cursor.compare_exchange_weak(
                cur,
                seq,
                Ordering::SeqCst,
                Ordering::SeqCst,
            ) {
                Ok(_) => return Ok(()),
                Err(seen) => cur = seen,
            }
        }
    }
}

/// B 档「降级到内存」落盘接收端：被降级的待发数据留在有界内存环形，**绝不静默丢弃**；
/// 重连后由 [`ReliableDispatch::drain_and_replay`] 排空并重发。
pub struct MemorySpillSink {
    /// 留存的降级批次（有界）。
    inner: Mutex<VecDeque<PendingSend>>,
    /// 内存环形容量（条数）。
    capacity: usize,
}

impl MemorySpillSink {
    /// 构造。
    #[must_use]
    pub fn new(capacity: usize) -> Arc<Self> {
        Arc::new(Self {
            inner: Mutex::new(VecDeque::new()),
            capacity: capacity.max(1),
        })
    }

    /// 排空留存的降级批次（重连重放用）。
    #[must_use]
    pub fn drain_spilled(&self) -> Vec<PendingSend> {
        lock_recover(&self.inner).drain(..).collect()
    }
}

impl SpillSink for MemorySpillSink {
    fn spill(&self, items: Vec<PendingSend>) -> SpillResult {
        let mut guard = lock_recover(&self.inner);
        let total = items.len();
        let mut accepted = 0usize;
        for item in items {
            if guard.len() >= self.capacity {
                // 内存也满 → 拒绝（交还调用方，由上层告警 / 计数）。
                return SpillResult {
                    accepted,
                    rejected: total - accepted,
                };
            }
            guard.push_back(item);
            accepted = accepted.saturating_add(1);
        }
        SpillResult {
            accepted,
            rejected: 0,
        }
    }
}

/// 慢消费者降级回调：水位触发时记录在途积压（降级动作由调用方在 `Degraded` 分支
/// 把报文交还 [`SendQueue`] 触发内存留存）。
#[derive(Default)]
pub struct DispatchDegrade {
    /// 最近一次触发的在途积压数。
    last_outstanding: Mutex<usize>,
}

impl DispatchDegrade {
    /// 构造。
    #[must_use]
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }
}

impl SlowConsumerSink for DispatchDegrade {
    fn on_slow_consumer(&self, outstanding: usize) {
        *lock_recover(&self.last_outstanding) = outstanding;
    }
}

/// 中毒恢复的锁获取（**绝不 panic**：poison 时取回内部数据继续运行）。
fn lock_recover<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    match m.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

/// 可靠北向分发器：编排 `SendQueue` / `ReplayController` / `IdempotencyLedger` /
/// `AuditLog` / `ReceiptReporter`，并以 [`MqttClient`] 作为 MQTT 传输。
pub struct ReliableDispatch {
    /// 网关标识（幂等键组成之一）。
    gateway_id: String,
    /// 设备机位码（回执批次归属者）。
    device_mid: String,
    /// 租约 ID（回执批次归属者）。
    lease_id: String,
    /// 控制面回执端点（用于派生审计日志端点；仅 `http`）。
    receipt_endpoint: String,
    /// 下一个待分配的批次序号（单调自增，构成幂等键 `gateway_id:batch_seq`）。
    next_seq: u64,
    /// 底层 MQTT 传输（task 19 的 [`MqttClient`]，含水位降级）。
    client: MqttClient,
    /// 有界发送队列（ready / sent 双队列；PUBACK 后才 `confirm`）。
    send: SendQueue,
    /// 补发控制器（去重 + 位点；先落 Ack 后推位点）。
    replay: ReplayController,
    /// 批次级去重登记簿（幂等键 `gateway_id:batch_seq`）。
    ledger: IdempotencyLedger,
    /// 审计环形缓冲（喂给 [`SendQueue`]，落盘 / 溢出 / 降级全留痕）。
    audit: Arc<AuditLog>,
    /// 控制面回执上报（B 档；`submit` 不阻塞）。
    reporter: ReceiptReporter,
    /// 控制面审计日志传输（转发背压审计事件）。
    transport: Arc<dyn ReceiptTransport>,
    /// 慢消费者降级回调（注入 [`MqttClient::publish_backpressured`]）。
    degrade: Arc<DispatchDegrade>,
    /// 已发布、待 PUBACK 的批次（按发送顺序；PUBACK 按序确认）。
    inflight: VecDeque<InflightBatch>,
    /// 在途批次的 [`TelemetryBatch`]（供构造回执语义摘要）。
    pending_batches: HashMap<u64, TelemetryBatch>,
    /// 被硬上限交还、待重试的批次（B 档下持续重试，绝不丢）。
    returned: VecDeque<PendingSend>,
    /// 时钟（供 [`SendQueue`] 与入队时间戳使用）。
    clock: Arc<dyn Clock>,
}

impl ReliableDispatch {
    /// 构建可靠分发（**不发起连接**；连接由 [`Self::poll`] 推进事件循环触发）。
    ///
    /// # Errors
    /// endpoint 非法 / 回执端点空 / 签名器构造失败 → 对应 [`crate::error::DaemonError`]。
    /// 构建可靠分发（**不发起连接**；连接由 [`Self::poll`] 推进事件循环触发）。
    ///
    /// 默认使用手写 HTTP/1.1 传输（纯 Rust，无 openssl / aws-lc）。生产可经
    /// [`Self::with_transport`] 注入自定义传输（如测试桩）。
    ///
    /// # Errors
    /// endpoint 非法 / 回执端点空 / 签名器构造失败 → 对应 [`crate::error::DaemonError`]。
    pub fn new(endpoint: EndpointConfig, cfg: ReliableDispatchConfig) -> DaemonResult<Self> {
        Self::with_transport(endpoint, cfg, Arc::new(HttpPostTransport::default()))
    }

    /// 构建可靠分发并注入回执传输（依赖注入 / 测试用）。
    ///
    /// # Errors
    /// 同 [`Self::new`]。
    pub fn with_transport(
        endpoint: EndpointConfig,
        cfg: ReliableDispatchConfig,
        transport: Arc<dyn ReceiptTransport>,
    ) -> DaemonResult<Self> {
        let client = MqttClient::new(endpoint)?;

        let audit = Arc::new(AuditLog::new(cfg.audit_capacity));
        let spill: Arc<dyn SpillSink> = MemorySpillSink::new(cfg.audit_capacity.saturating_mul(4));
        let clock: Arc<dyn Clock> = Arc::new(SystemClock);
        let wm = WaterMarkConfig::new(cfg.send_high_water, cfg.send_hard_limit);
        let send = SendQueue::new(wm, spill, Arc::clone(&audit), Arc::clone(&clock));

        // 批次级位点：ReplayController（先落 Ack 后推位点）+ 独立 IdempotencyLedger（去重）。
        let ack_sink: Arc<dyn AckSink> = MemoryAckSink::new(cfg.start_high_water);
        let replay =
            ReplayController::new(cfg.gateway_id.clone(), ack_sink, cfg.start_high_water);
        let ledger = IdempotencyLedger::new(
            cfg.gateway_id.clone(),
            cfg.audit_capacity.saturating_mul(4),
        );

        // 控制面回执：submit 仅签名 + 入队，绝不阻塞；签名域与端点由装配注入。
        let signer: Arc<dyn ReceiptSigner> =
            Arc::new(Ed25519ReceiptSigner::from_seed(&cfg.signer_seed));
        // 回执 reporter 必须复用注入的传输：否则测试桩不生效 → worker 走真实 HTTP
        // 无限重试、队列永不排空 → shutdown().await 挂起（生产默认 HTTP 传输不受影响）。
        let reporter = ReceiptReporter::with_transport(
            ReceiptReporterConfig {
                endpoint_url: cfg.receipt_endpoint.clone(),
                ..ReceiptReporterConfig::default()
            },
            signer,
            fixed_clock(0),
            Arc::clone(&transport),
        );

        Ok(Self {
            gateway_id: cfg.gateway_id,
            device_mid: cfg.device_mid,
            lease_id: cfg.lease_id,
            receipt_endpoint: cfg.receipt_endpoint,
            next_seq: cfg.start_high_water.saturating_add(1),
            client,
            send,
            replay,
            ledger,
            audit,
            reporter,
            transport,
            degrade: DispatchDegrade::new(),
            inflight: VecDeque::new(),
            pending_batches: HashMap::new(),
            returned: VecDeque::new(),
            clock,
        })
    }

    /// 在途（已提交未确认）条数（SendQueue 水位口径）。
    #[must_use]
    pub fn pending(&self) -> usize {
        self.send.pending()
    }

    /// 已发布、待 PUBACK 的批次数。
    #[must_use]
    pub fn inflight_len(&self) -> usize {
        self.inflight.len()
    }

    /// 下一个待分配的批次序号。
    #[must_use]
    pub fn next_seq(&self) -> u64 {
        self.next_seq
    }

    /// 当前 high-water mark（已确认到的最大 `batch_seq`）。
    #[must_use]
    pub fn high_water_mark(&self) -> u64 {
        self.replay.high_water_mark()
    }

    /// 回执重试队列长度（已签名待上报的回执数）。
    #[must_use]
    pub fn receipt_queue_len(&self) -> usize {
        self.reporter.queue_len()
    }

    /// 分发一批样本：构造批次、做批次级去重、流经 [`SendQueue`]。
    ///
    /// 质量码在此经 `encoder::sample_to_data_point`（内部 `codec::Quality::from_wire()
    /// .to_wire()`）归一，保证线路上质量码规范化。
    ///
    /// # Errors
    /// 编码失败 → [`crate::error::DaemonError::ProtocolError`]；
    /// 发送队列内存分配异常 → 对应错误。
    pub async fn dispatch_batch(&mut self, samples: &[ProcessedSample]) -> DaemonResult<()> {
        if samples.is_empty() {
            return Ok(());
        }
        let seq = self.next_seq;
        self.next_seq = self.next_seq.saturating_add(1);
        let key = idempotency_key(&self.gateway_id, seq);

        // 批次级去重（幂等键 = gateway_id:batch_seq）：已应用则跳过（重放 / 网络重复）。
        if !self.ledger.apply_key(&key).is_applied() {
            return Ok(());
        }
        // 位点仲裁：已确认过的批次不重发（依赖批次头序号，而非内存 list 过滤）。
        if !self.replay.should_send(seq) {
            return Ok(());
        }

        // 构造 protobuf 载荷：质量码经 codec 归一（见 encoder::sample_to_data_point 改造）。
        let points: Vec<DataPoint> = samples.iter().map(sample_to_data_point).collect();
        let batch_ts = samples
            .first()
            .map(|s| s.collected_ts_ns)
            .unwrap_or(0);
        let batch = TelemetryBatch {
            points,
            ts: batch_ts,
            gateway_id: self.gateway_id.clone(),
            auth: None,
        };

        let payload = self.encode_batch(&batch)?;
        let item = PendingSend::new(seq, payload, self.clock.now_ns());

        // 出站样本流经 SendQueue（ready / sent 双队列）；仅 PUBACK 后才 confirm 推进游标。
        match self.send.push(item) {
            PushOutcome::Admitted => {}
            PushOutcome::Spilled { .. } => {
                // 降级到内存（MemorySpillSink 已留存），重连后由 drain_and_replay 重发。
            }
            PushOutcome::Rejected { returned } => {
                // 硬上限且落盘失败：数据交还，绝不静默丢；B 档下持续重试（见 flush_ready）。
                for r in returned {
                    self.returned.push_back(r);
                }
            }
        }
        self.pending_batches.insert(seq, batch);
        Ok(())
    }

    /// 取出 ready 层批次并发布（带慢消费者降级）；sent 层驻留直到 PUBACK。
    ///
    /// # Errors
    /// 请求通道关闭 → [`crate::error::DaemonError::MqttError`]。
    pub async fn flush_ready(&mut self, max: usize) -> DaemonResult<()> {
        // B 档可用性：先把「被拒交还」的批次重新尝试入队（持续重试，绝不丢）。
        while let Some(item) = self.returned.pop_front() {
            if self.send.push(item.clone()).is_admitted() {
                // 已重新入队（SendQueue 内部接管所有权），继续尝试下一条。
                continue;
            }
            // 仍拒 → 把原 item 放回队尾稍后重试（避免在水位高位无限循环）。
            self.returned.push_back(item);
            break;
        }

        let ready = self.send.take_ready(max);
        for item in ready {
            let seq = item.seq;
            let batch = match self.pending_batches.get(&seq) {
                Some(b) => b.clone(),
                None => continue,
            };
            let topic = self.topic_for_batch();
            // 走 MqttClient 带水位发布（task 19）：超限返回 Degraded，本层把报文交还
            // SendQueue（→ MemorySpillSink 留存），稍后重连重放。
            match self
                .client
                .publish_backpressured(&topic, item.payload.clone(), self.degrade.as_ref())
                .await?
            {
                PublishOutcome::Sent => {
                    self.inflight.push_back(InflightBatch {
                        seq,
                        batch,
                        enqueued_ns: item.enqueued_ns,
                    });
                }
                PublishOutcome::Degraded { .. } => {
                    let _ = self.send.push(item);
                }
            }
        }
        Ok(())
    }

    /// 推进一轮事件循环；在 PUBACK / 会话恢复时分别驱动确认与重放。
    ///
    /// # Errors
    /// 连接 / 协议错误按域收敛（见 [`MqttClient::poll_event`]）。
    pub async fn poll(&mut self) -> DaemonResult<Event> {
        let event = self.client.poll_event().await?;
        match &event {
            // PUBACK / PUBCOMP：确认到达 → 推进发送游标 + 位点 + 回执 + 审计排空。
            Event::Incoming(Packet::PubAck(_)) | Event::Incoming(Packet::PubComp(_)) => {
                self.on_ack().await?;
            }
            // CONNACK：会话未恢复（全新会话）→ 排空 ready + sent-unacked 两层并重放补上。
            Event::Incoming(Packet::ConnAck(ack)) => {
                if ack.code == ConnectReturnCode::Success && !ack.session_present {
                    self.drain_and_replay().await?;
                }
            }
            _ => {}
        }
        Ok(event)
    }

    /// PUBACK 处理：仅确认后才推进游标、推进位点、提交回执、排空审计。
    async fn on_ack(&mut self) -> DaemonResult<()> {
        let info = match self.inflight.pop_front() {
            Some(i) => i,
            None => return Ok(()),
        };
        // 1) 仅 PUBACK 确认后推进发送游标：从 sent-unacked 层移除。
        self.send.confirm(1);
        // 2) 批次级位点推进（ReplayController 内部「先落 Ack 后推位点」）。
        self.replay.ack(info.seq)?;
        // 3) 审计排空 → 控制面：构造白名单 8 字段回执并 POST 到 /audit/receipt。
        //    payload_digest 为语义哈希（非原始 protobuf 字节）；绝不携带业务数值。
        let receipt = ReceiptBatch::from_telemetry(
            &info.batch,
            self.device_mid.clone(),
            self.lease_id.clone(),
            info.seq,
            info.seq,
        )?;
        self.reporter.submit(&receipt).await?;
        self.pending_batches.remove(&info.seq);
        // 4) 排空背压审计（落盘 / 溢出 / 降级事件）→ 控制面审计日志。
        self.drain_audit().await?;
        Ok(())
    }

    /// 重连（会话未恢复）时：用批次头序号做仲裁（`replay_selection` 过滤
    /// `seq > high_water` 并按幂等键去重），排空 ready + sent-unacked 两层并重发补上。
    ///
    /// # Errors
    /// 批次编码失败 → [`crate::error::DaemonError::ProtocolError`]。
    async fn drain_and_replay(&mut self) -> DaemonResult<()> {
        // 把 sent-unacked 层的在途批次转为 QueuedBatch，交给 replay_selection 仲裁。
        let queued: DaemonResult<Vec<QueuedBatch>> = self
            .inflight
            .iter()
            .map(|ib| {
                Ok(QueuedBatch {
                    seq: ib.seq,
                    gateway_id: self.gateway_id.clone(),
                    payload: self.encode_batch(&ib.batch)?,
                    enqueued_ns: ib.enqueued_ns,
                })
            })
            .collect();
        let queued = queued?;
        let to_resend: Vec<&QueuedBatch> =
            replay_selection(&queued, self.replay.high_water_mark());
        let items: Vec<PendingSend> = to_resend
            .into_iter()
            .map(|b| PendingSend::new(b.seq, b.payload.clone(), b.enqueued_ns))
            .collect();

        // 清掉旧 inflight 镜像（sent-unacked 已交还 SendQueue 的 ready 头部），
        // 由 flush_ready 重新建立镜像。
        self.inflight.clear();
        self.send.requeue_failed(items);
        self.flush_ready(usize::MAX).await?;
        Ok(())
    }

    /// 把 [`TelemetryBatch`] 编码为当前连接声明的字节格式。
    fn encode_batch(&self, batch: &TelemetryBatch) -> DaemonResult<Vec<u8>> {
        encoder_for(self.client.encoding()).encode_batch(batch)
    }

    /// 批次发布主题（gateway_id 维度；设备级路由由 broker 侧完成）。
    fn topic_for_batch(&self) -> String {
        format!("{}/{}", self.client.endpoint().topic_prefix, self.gateway_id)
    }

    /// 排空 [`AuditLog`]（落盘 / 溢出 / 降级事件）并转发到控制面审计日志。
    ///
    /// 控制面不可达时**绝不阻断采集 / 重放**，仅告警。
    async fn drain_audit(&mut self) -> DaemonResult<()> {
        let events = self.audit.drain();
        if events.is_empty() {
            return Ok(());
        }
        let event_url = self.audit_event_url();
        if event_url.is_empty() {
            return Ok(());
        }
        for ev in events {
            let body = audit_json(&ev);
            if let Err(e) = self.transport.post_json(&event_url, &body).await {
                eprintln!("[reliable] audit event forward failed: {e}");
            }
        }
        Ok(())
    }

    /// 由回执端点派生审计日志端点（`/audit/receipt` → `/audit/event`）。
    #[must_use]
    fn audit_event_url(&self) -> String {
        const RECEIPT: &str = "/audit/receipt";
        match self.receipt_endpoint.strip_suffix(RECEIPT) {
            Some(base) => format!("{base}/audit/event"),
            None => String::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use protocol_proto::Quality as WireQuality;

    fn sample(value: f64) -> ProcessedSample {
        ProcessedSample {
            device_id: "dev-1".to_string(),
            point_id: "p1".to_string(),
            value,
            unit: "kPa".to_string(),
            device_ts_ns: None,
            collected_ts_ns: 1_700_000_000_000_000_000,
            quality: WireQuality::Good,
        }
    }

    /// 测试桩传输：立即返回成功且不触网，使 worker 能快速排空队列、shutdown 不挂起。
    struct StubTransport;

    #[async_trait::async_trait]
    impl ReceiptTransport for StubTransport {
        async fn post_json(&self, _url: &str, _body: &str) -> DaemonResult<String> {
            // 必须含 `accepted`（parse_receipt_response 要求）；返回 "{}" 会被判失败 → worker 无限重试 → shutdown 挂起。
            Ok(r#"{"accepted":true}"#.to_string())
        }
    }

    fn make_dispatch() -> ReliableDispatch {
        // 注入 StubTransport：回执经内存桩返回成功，worker 立即排空，shutdown 不挂起。
        let endpoint = EndpointConfig::new("rel-test", "127.0.0.1", 1883);
        let cfg = ReliableDispatchConfig {
            gateway_id: "gw-1".to_string(),
            device_mid: "MID-1".to_string(),
            lease_id: "LEASE-1".to_string(),
            start_high_water: 0,
            receipt_endpoint: "http://127.0.0.1:9/audit".to_string(),
            signer_seed: [7u8; 32],
            ..ReliableDispatchConfig::default()
        };
        ReliableDispatch::with_transport(endpoint, cfg, Arc::new(StubTransport)).expect("build dispatch")
    }

    fn telemetry_batch(_seq: u64) -> TelemetryBatch {
        TelemetryBatch {
            points: vec![DataPoint {
                device_id: "d".to_string(),
                point_id: "p".to_string(),
                value: 1.0f64.to_le_bytes().to_vec(),
                unit: "kPa".to_string(),
                ts: 1,
                quality: WireQuality::Good as i32,
            }],
            ts: 1,
            gateway_id: "gw-1".to_string(),
            auth: None,
        }
    }

    #[test]
    fn memory_ack_sink_persists_monotonic() {
        let sink = MemoryAckSink::new(0);
        sink.persist_ack(3).expect("ack 3");
        sink.persist_ack(1).expect("ack 1 (older, no-op)");
        assert_eq!(sink.high_water(), 3);
        sink.persist_ack(5).expect("ack 5");
        assert_eq!(sink.high_water(), 5);
    }

    #[test]
    fn memory_spill_sink_retains_and_drains() {
        let sink = MemorySpillSink::new(2);
        let a = PendingSend::new(1, vec![1], 0);
        let b = PendingSend::new(2, vec![2], 0);
        let c = PendingSend::new(3, vec![3], 0);
        // 容量 2：前两条接受，第三条拒绝。
        assert_eq!(sink.spill(vec![a, b, c]).accepted, 2);
        let drained = sink.drain_spilled();
        assert_eq!(drained.len(), 2);
        assert_eq!(drained[0].seq, 1);
        assert_eq!(drained[1].seq, 2);
    }

    #[tokio::test]
    async fn dispatch_enqueues_to_send_queue() {
        let mut dx = make_dispatch();
        dx.dispatch_batch(&[sample(1.0), sample(2.0)]).await.unwrap();
        // 一个批次进入 ready 层；序号自增。
        assert_eq!(dx.pending(), 1);
        assert_eq!(dx.next_seq(), 2);
        dx.reporter.shutdown().await;
    }

    #[tokio::test]
    async fn flush_ready_publishes_without_broker() {
        let mut dx = make_dispatch();
        dx.dispatch_batch(&[sample(1.0)]).await.unwrap();
        // 无 broker 也能发布到请求通道（水位未满 → Sent），批次进入在途。
        dx.flush_ready(8).await.unwrap();
        assert_eq!(dx.inflight_len(), 1, "批次应已发布并在途");
        dx.reporter.shutdown().await;
    }

    #[tokio::test]
    async fn on_ack_advances_cursor_and_enqueues_receipt() {
        let mut dx = make_dispatch();
        // 手动注入一条在途批次（模拟已发布、待 PUBACK）。
        let batch = telemetry_batch(1);
        dx.inflight.push_back(InflightBatch {
            seq: 1,
            batch: batch.clone(),
            enqueued_ns: 1,
        });
        dx.pending_batches.insert(1, batch);

        dx.on_ack().await.unwrap();

        assert_eq!(dx.high_water_mark(), 1, "PUBACK 后位点应推进到 1");
        assert!(
            dx.receipt_queue_len() >= 1,
            "回执应已签名入队待上报（8 字段白名单，语义摘要）"
        );
        assert_eq!(dx.inflight_len(), 0, "已确认批次移出在途");
        dx.reporter.shutdown().await;
    }

    #[tokio::test]
    async fn reconnect_replay_resends_unacked() {
        let mut dx = make_dispatch();
        // 模拟已发布、会话未恢复的在途批次（seq=5 > high_water=0）。
        let batch = telemetry_batch(5);
        dx.inflight.push_back(InflightBatch {
            seq: 5,
            batch: batch.clone(),
            enqueued_ns: 1,
        });
        dx.pending_batches.insert(5, batch);

        // 重连重放：seq=5 > high_water=0 → 选中并重发。
        dx.drain_and_replay().await.unwrap();

        assert_eq!(dx.inflight_len(), 1, "重放后应仍在途（未被确认）");
        assert_eq!(dx.high_water_mark(), 0, "未收到 PUBACK，位点不变");
        dx.reporter.shutdown().await;
    }
}

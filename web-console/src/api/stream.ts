/**
 * @file stream.ts
 * @module web-console/api/stream
 * @description 实时遥测 SSE 客户端（**原生 `EventSource`，零新增依赖**）。
 *
 * ── 对齐后端契约（crates/daemon/src/mgmt/mod.rs `GET /api/stream`）────────────
 *  · SSE（`text/event-stream`），事件名 `telemetry`，`data` 为一帧 JSON：
 *    `{ enc, gateway_id, ts: <纳秒字符串>, auth, points: [{ device_id, point_id,
 *    value: <number|null 已解码>, unit, ts: <纳秒字符串>, quality: "GOOD"|…,
 *    quality_code: <number|null> }] }`；
 *  · 鉴权：`Authorization: Bearer <jwt>` 头 **或** `?token=<jwt>` 查询参数。
 *    **EventSource 无法自定义请求头 → 必须用 `?token=`**；
 *  · 心跳由服务端 KeepAlive（15s）承担，客户端无需处理。
 *
 * ── 重连与 401 处置设计 ──────────────────────────────────────────────────────
 *  · **网络抖动 / 服务重启**：`EventSource` 原生自动重连（`readyState=CONNECTING`），
 *    本模块不做额外干预，仅把状态翻回 `connecting` 供顶栏指示「链路降级」；
 *  · **401（token 失效）**：原生 `error` 事件不带 HTTP 状态码，且 `EventSource`
 *    对 401 也会无休止重试 → 这里在首次 error 时发一个**一次性探测请求**
 *    （`fetch('/api/stream')` 只读响应状态码，不消费流体）：401 → 关闭流 +
 *    走 `client.handleUnauthorized()`（清 token / 通知监听器 / 跳登录页），
 *    与 HTTP 层 401 语义完全一致；非 401 → 交给原生重连。
 *    探测以 `probeInFlight` 去重，避免重连风暴期间打爆后端。
 *
 * ── 大数红线 ────────────────────────────────────────────────────────────────
 * 帧 `ts` 与点 `ts` 为**纳秒字符串**，一律原样透传存储/展示，绝不 `parseInt`
 * （2^53 精度陷阱）；仅 `quality_code`（范围安全的 JSON number）按数值保留。
 *
 * ── 节流契约 ────────────────────────────────────────────────────────────────
 * 本模块只做「最新帧落缓存」（高频写入无渲染副作用）；页面层的 1s 节流 tick
 * 从缓存读取 —— 上游推送频率与渲染频率解耦（设计系统 §4.2 硬约束）。
 */
import { reactive } from 'vue';
import { getStoredToken, handleUnauthorized } from './client';

/** SSE 帧内单点形状（与后端 `LivePoint` 字段名一一对应，蛇形命名直通）。 */
export interface TelemetryPointFrame {
  /** 设备 id */
  readonly device_id: string;
  /** 点位 id（后端配置的 point_id，如 `p_temp`） */
  readonly point_id: string;
  /** 已解码数值（不可用为 null；其余类型一律按 null 兜底展示） */
  readonly value: number | null;
  /** 工程单位 */
  readonly unit: string;
  /** 采样时间戳（**纳秒字符串**，透传展示） */
  readonly ts: string;
  /** 质量枚举名（`GOOD` / `UNCERTAIN` / `OUT_OF_RANGE` / `CALC_FAILED` / `BAD` / `TIMEOUT` / `COMM_ERROR`） */
  readonly quality: string;
  /** 质量码整数值（范围安全，数值保留；缺失为 null） */
  readonly quality_code: number | null;
}

/** SSE `telemetry` 帧形状（与后端 `LiveTelemetry` 对齐）。 */
export interface TelemetryFrame {
  /** 编码标识（恒 `json`） */
  readonly enc: string;
  /** 网关标识 */
  readonly gateway_id: string;
  /** 批次时间戳（**纳秒字符串**） */
  readonly ts: string;
  /** 签名块（北向无签名时为 null） */
  readonly auth: unknown;
  /** 本批次点位数组 */
  readonly points: readonly TelemetryPointFrame[];
}

/** SSE 通道状态。 */
export type StreamStatus =
  /** 未启动（未登录 / 已关闭） */
  | 'idle'
  /** 连接中（首次连接或断线自动重连） */
  | 'connecting'
  /** 已连接（至少收到一次 open） */
  | 'open'
  /** 鉴权失败（token 失效，已停止重连并触发 401 处置） */
  | 'unauthorized';

/** 单点最新快照（页面对齐 `device_id/point_id` 读取）。 */
export interface TelemetryPointSnapshot {
  /** 最新值（不可用为 null） */
  value: number | null;
  /** 工程单位 */
  unit: string;
  /** 采样时间戳（**纳秒字符串**，透传展示） */
  ts: string;
  /** 质量枚举名（wire 原文，如 `GOOD`） */
  quality: string;
  /** 质量码整数（缺失为 null） */
  qualityCode: number | null;
  /** 客户端收到该帧的时刻（毫秒，仅用于陈旧判定，非业务时间戳） */
  receivedAtMs: number;
}

/** SSE 通道运行状态（响应式；页面 / 顶栏连接指示消费）。 */
export const streamStatus = reactive<{
  /** 通道状态 */
  value: StreamStatus;
  /** 已收到的 telemetry 帧数（自连接起累计） */
  framesReceived: number;
  /** 最近一帧到达时刻（毫秒；0 = 尚未收到帧） */
  lastFrameAtMs: number;
  /** 最近一帧的批次 ts（纳秒字符串原文透传；空串 = 尚未收到帧） */
  lastFrameTs: string;
}>({
  value: 'idle',
  framesReceived: 0,
  lastFrameAtMs: 0,
  lastFrameTs: '',
});

/**
 * 逐点最新快照表（响应式 Map）。
 *
 * key = `${device_id}/${point_id}`（对齐前端 real 模式点位：
 * `PointRecord.deviceId` 来自 `/api/points` 的 `device_id`，
 * `PointRecord.id` 来自 `point_id`，见 repo.mapPoint）。
 */
export const pointSnapshots = reactive(new Map<string, TelemetryPointSnapshot>());

/** 组装逐点快照 key（页面与流客户端共用的唯一口径）。 */
export function snapshotKey(deviceId: string, pointId: string): string {
  return `${deviceId}/${pointId}`;
}

/** 前端质量枚举（与 `model.ts` 的 `DataQuality` 同形；此处独立声明避免反向依赖页面层）。 */
export type StreamDataQuality = 'Good' | 'Uncertain' | 'Bad' | 'CalcFailed' | 'Timeout';

/**
 * 后端质量枚举名 → 前端 `DataQuality` 映射。
 *
 * 后端权威映射（codec.rs `Quality::as_str` / `from_name`）：
 * `GOOD / UNCERTAIN / OUT_OF_RANGE / CALC_FAILED / BAD / TIMEOUT / COMM_ERROR`。
 * 前端枚举只有 5 个取值：`OUT_OF_RANGE` 归 `Uncertain`（可接受语义），
 * `COMM_ERROR` 归 `Bad`（不可用语义），无法识别的一律保守归 `Bad`。
 */
const WIRE_QUALITY_MAP: Readonly<Record<string, StreamDataQuality>> = Object.freeze({
  GOOD: 'Good',
  UNCERTAIN: 'Uncertain',
  OUT_OF_RANGE: 'Uncertain',
  CALC_FAILED: 'CalcFailed',
  BAD: 'Bad',
  TIMEOUT: 'Timeout',
  COMM_ERROR: 'Bad',
});

/** 质量枚举名 → 前端 `DataQuality`（未识别值保守归 `Bad`）。 */
export function wireQualityToDataQuality(wire: string): StreamDataQuality {
  return WIRE_QUALITY_MAP[wire.trim().toUpperCase()] ?? 'Bad';
}

// ---------------------------------------------------------------------------
// 连接生命周期
// ---------------------------------------------------------------------------

/** 当前 EventSource 句柄（null = 未连接）。 */
let es: EventSource | null = null;

/** 401 探测去重标记（重连风暴期间只发一个探测请求）。 */
let probeInFlight = false;

/**
 * 建立 SSE 连接（已持有 token 时生效；幂等，重复调用直接返回）。
 *
 * 未登录时不建连（保持 idle，登录成功后由生命周期 watch 再触发）。
 */
export function connectStream(): void {
  if (es) {
    return; // 幂等：已在连接 / 重连中
  }
  const token = getStoredToken();
  if (!token) {
    return; // 未登录：保持 idle，登录成功后由生命周期 watch 再触发
  }
  streamStatus.value = 'connecting';
  // EventSource 无法带 header → 鉴权走 ?token= 查询参数（后端显式支持）
  es = new EventSource(`/api/stream?token=${encodeURIComponent(token)}`);

  es.addEventListener('telemetry', (event: Event) => {
    // 自定义事件名不在 EventSourceEventMap 中，回调形参按基类 Event 收敛后取 data
    const data = (event as MessageEvent<string>).data;
    let frame: TelemetryFrame;
    try {
      frame = JSON.parse(data) as TelemetryFrame;
    } catch {
      return; // 坏帧丢弃，不影响连接
    }
    if (!frame || !Array.isArray(frame.points)) {
      return;
    }
    const nowMs = Date.now();
    for (const point of frame.points) {
      if (!point || typeof point.device_id !== 'string' || typeof point.point_id !== 'string') {
        continue;
      }
      if (!point.device_id || !point.point_id) {
        continue; // 缺 key 的点无法对齐，丢弃
      }
      pointSnapshots.set(snapshotKey(point.device_id, point.point_id), {
        value: typeof point.value === 'number' && Number.isFinite(point.value) ? point.value : null,
        unit: typeof point.unit === 'string' ? point.unit : '',
        ts: typeof point.ts === 'string' ? point.ts : '', // 纳秒字符串透传，绝不 parseInt
        quality: typeof point.quality === 'string' ? point.quality : 'BAD',
        qualityCode:
          typeof point.quality_code === 'number' && Number.isFinite(point.quality_code)
            ? point.quality_code
            : null,
        receivedAtMs: nowMs,
      });
    }
    streamStatus.framesReceived += 1;
    streamStatus.lastFrameAtMs = nowMs;
    streamStatus.lastFrameTs = typeof frame.ts === 'string' ? frame.ts : '';
  });

  es.onopen = () => {
    streamStatus.value = 'open';
  };

  es.onerror = () => {
    if (!es) {
      return;
    }
    // EventSource 会原生自动重连；这里只同步状态 + 做一次性 401 探测
    if (streamStatus.value === 'open') {
      streamStatus.value = 'connecting';
    }
    void probeUnauthorizedOnce();
  };
}

/**
 * 一次性探测：确认 error 是否源于 401（token 失效）。
 *
 * 原生 `error` 事件不带状态码，而 EventSource 对 401 也会无限重试 —— 用
 * `fetch` 只读响应头（服务端对坏 token 立即回 401；好 token 回 200 + 流头），
 * 401 → 关闭流并走与 HTTP 层一致的未授权处置；其余情况交给原生重连。
 */
async function probeUnauthorizedOnce(): Promise<void> {
  if (probeInFlight) {
    return;
  }
  probeInFlight = true;
  try {
    const token = getStoredToken();
    const res = await fetch('/api/stream', {
      headers: token ? { Authorization: `Bearer ${token}` } : {},
    });
    if (res.status === 401) {
      streamStatus.value = 'unauthorized';
      closeStream();
      handleUnauthorized(); // 清 token + 通知监听器 + 跳登录页（与 apiRequest 401 同语义）
      return;
    }
    // 非 401（含 200）：网络抖动 / 瞬时错误，EventSource 原生重连接管；
    // 探测连接立即放弃（不消费流体，避免与服务端长连接重复）。
    try {
      await res.body?.cancel();
    } catch {
      /* 放弃失败不影响主流程 */
    }
  } catch {
    /* fetch 本身网络失败：同样交给 EventSource 原生重连 */
  } finally {
    probeInFlight = false;
  }
}

/** 关闭 SSE 连接（登出 / 组件卸载等场景；幂等）。已判定的 unauthorized 状态保留。 */
export function closeStream(): void {
  if (es) {
    es.close();
    es = null;
  }
  if (streamStatus.value !== 'unauthorized') {
    streamStatus.value = 'idle';
  }
}

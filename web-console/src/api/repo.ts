/**
 * @file repo.ts
 * @module web-console/api/repo
 * @description 统一数据层：`VITE_API_MODE` 分流 mock / real，页面层唯一数据入口。
 *
 * ── 设计（对齐任务约束「减少逐页改动面」）────────────────────────────────────
 *  · **导出与 `mock-data.ts` 同名同形**：`export *` 透传类型 / 常量 / 快照，
 *    本模块再导出同名的 `repo`（局部导出优先于 `export *`，TS 合法）；
 *    页面只需把 `from '../mock/mock-data'` 换成 `from '@/api/repo'`，渲染逻辑零改动。
 *  · **mock 模式（默认）**：`repo` 即 mock-data 仓库本身（追加 `ops` 运维动作），
 *    完全不发网络请求。
 *  · **real 模式**：启动 / 登录后 `preloadRealData()` 并行拉取
 *    `/api/devices · /api/points · /api/outlets · /api/status · /api/events`
 *    并缓存；读取型方法优先返回真实缓存，**任一接口失败 / 为空则回退 mock 数据**
 *    （任务约束「容差由 fallback 兜底」）；写操作（后端契约未含设备/点位写接口）
 *    落本地覆盖层（overlay），语义与 mock 一致，供本地端到端走通页面流程。
 *  · **大数红线**：`totalForwardedRecords` 等大数字段走字符串直通（`pickStr`），
 *    绝不 parseFloat；仅「小值业务计数」经 `pickNum` 安全转换。
 *  · **不改 mock-data.ts 本身**（它是契约与 fallback）。
 */
import {
  PROTOCOL_OPTIONS,
  repo as mockRepo,
  type AlarmRecord,
  type AlarmState,
  type AuditEntry,
  type DeviceDraft,
  type DeviceRecord,
  type Encoding,
  type ForwarderRecord,
  type GatewayInfo,
  type Paged,
  type PointDraft,
  type PointRecord,
  type ProtocolType,
} from '../mock/mock-data';

// 透传 mock-data 的类型 / 常量 / 快照（`repo` 由下方局部导出遮蔽，属 ES 模块规范行为）
export * from '../mock/mock-data';

import { API_MODE, apiRequest } from './client';

// 再导出模式常量，页面可统一从本模块取用
export { API_MODE } from './client';

// ===========================================================================
// 宽容取值工具（后端字段以 mock-data 形状为契约；缺失 / 类型漂移时回默认值）
// ===========================================================================

/** 取字符串字段：string 直通；有限数字转字符串；其余回默认。 */
function pickStr(src: Record<string, unknown>, key: string, dflt: string): string {
  const v = src[key];
  if (typeof v === 'string') {
    return v;
  }
  if (typeof v === 'number' && Number.isFinite(v)) {
    return String(v);
  }
  return dflt;
}

/** 取小值数字字段：number 直通；可安全转换的数字字符串转换；其余回默认。
 *
 *  ⚠️ 仅用于「小值业务计数」（≤ 四位数，远离 2^53）；大数字段一律走 `pickStr`。
 */
function pickNum(src: Record<string, unknown>, key: string, dflt: number): number {
  const v = src[key];
  if (typeof v === 'number' && Number.isFinite(v)) {
    return v;
  }
  if (typeof v === 'string' && v.trim() !== '' && Number.isFinite(Number(v))) {
    return Number(v);
  }
  return dflt;
}

/** 取布尔字段。 */
function pickBool(src: Record<string, unknown>, key: string, dflt: boolean): boolean {
  const v = src[key];
  if (typeof v === 'boolean') {
    return v;
  }
  if (v === 'true') {
    return true;
  }
  if (v === 'false') {
    return false;
  }
  return dflt;
}

/** 当前时间格式化为 `YYYY-MM-DD HH:mm:ss`（与 mock-data 的约定一致）。 */
function nowText(): string {
  const d = new Date();
  const p = (n: number): string => String(n).padStart(2, '0');
  return `${d.getFullYear()}-${p(d.getMonth() + 1)}-${p(d.getDate())} ${p(d.getHours())}:${p(d.getMinutes())}:${p(d.getSeconds())}`;
}

// ===========================================================================
// real 模式：缓存 + 本地覆盖层
// ===========================================================================

/** 真实后端缓存（preload 后填充；字段形状 = mock-data 前端镜像）。 */
interface RealCache {
  /** 是否已完成 preload（无论成败） */
  loaded: boolean;
  devices: DeviceRecord[];
  points: PointRecord[];
  outlets: ForwarderRecord[];
  status: GatewayInfo | null;
  events: AuditEntry[];
  alarms: AlarmRecord[];
}

/** 缓存初始值（全部为空 → 各读取方法回退 mock）。 */
const realCache: RealCache = {
  loaded: false,
  devices: [],
  points: [],
  outlets: [],
  status: null,
  events: [],
  alarms: [],
};

/** 本地覆盖层：real 模式下承载后端契约未覆盖的写操作（语义与 mock 一致）。 */
const overlay = {
  newDevices: [] as DeviceRecord[],
  deletedDeviceIds: new Set<string>(),
  newPoints: [] as PointRecord[],
  deletedPointIds: new Set<string>(),
  outletEncoding: new Map<string, Encoding>(),
  alarmStates: new Map<string, { state: AlarmState; note: string; ackedBy: string }>(),
  localAudit: [] as AuditEntry[],
};

/** 追加本地审计（覆盖层写操作的强制副作用，与 mock 行为对齐）。 */
function pushLocalAudit(entry: {
  actor: string;
  actorType: string;
  action: string;
  entityType: string;
  entityLabel: string;
  entityId: string;
  detail: string;
  result: string;
}): void {
  overlay.localAudit.unshift({
    id: `log-local-${Date.now()}-${overlay.localAudit.length}`,
    ts: nowText(),
    ip: '127.0.0.1',
    ...entry,
  });
}

// ---------------------------------------------------------------------------
// 后端响应 → 前端镜像（宽容映射）
// ---------------------------------------------------------------------------

/** `/api/devices` 行 → DeviceRecord。 */
function mapDevice(raw: Record<string, unknown>, idx: number): DeviceRecord {
  const protocol = pickStr(raw, 'protocol', 'modbus-tcp') as ProtocolType;
  return {
    id: pickStr(raw, 'id', `dev-real-${idx}`),
    name: pickStr(raw, 'name', `设备-${idx + 1}`),
    protocol,
    protocolLabel: pickStr(raw, 'protocolLabel', PROTOCOL_OPTIONS.find((p) => p.value === protocol)?.label ?? protocol),
    connectionSummary: pickStr(raw, 'connectionSummary', '—'),
    status: pickStr(raw, 'status', 'offline') as DeviceRecord['status'],
    intervalMs: pickNum(raw, 'intervalMs', 1000),
    timeoutMs: pickNum(raw, 'timeoutMs', 3000),
    retryTimes: pickNum(raw, 'retryTimes', 3),
    pointCount: pickNum(raw, 'pointCount', 0),
    lastSampleAt: pickStr(raw, 'lastSampleAt', '—'),
    successRate: pickNum(raw, 'successRate', 0),
    failStreak: pickNum(raw, 'failStreak', 0),
    offlineText: pickStr(raw, 'offlineText', ''),
    createdAt: pickStr(raw, 'createdAt', '—'),
  };
}

/** `/api/points` 行 → PointRecord。 */
function mapPoint(raw: Record<string, unknown>, idx: number): PointRecord {
  const value = raw.value === null || raw.value === undefined ? null : pickNum(raw, 'value', 0);
  const quality = pickStr(raw, 'quality', 'Good') as PointRecord['quality'];
  return {
    id: pickStr(raw, 'id', `pt-real-${idx}`),
    deviceId: pickStr(raw, 'deviceId', pickStr(raw, 'device_id', '')),
    deviceName: pickStr(raw, 'deviceName', pickStr(raw, 'device_name', '—')),
    name: pickStr(raw, 'name', `点位-${idx + 1}`),
    pointType: pickStr(raw, 'pointType', 'physical') as PointRecord['pointType'],
    address: pickStr(raw, 'address', '—'),
    dataType: pickStr(raw, 'dataType', pickStr(raw, 'data_type', 'float32')),
    byteOrder: pickStr(raw, 'byteOrder', pickStr(raw, 'byte_order', '—')) as PointRecord['byteOrder'],
    unit: pickStr(raw, 'unit', ''),
    deadband: pickNum(raw, 'deadband', 0),
    targetKey: pickStr(raw, 'targetKey', pickStr(raw, 'target_key', '')),
    quality,
    value,
    valueText: pickStr(raw, 'valueText', value === null ? '——' : String(value)),
    formula: typeof raw.formula === 'string' ? raw.formula : null,
    updatedAt: pickStr(raw, 'updatedAt', pickStr(raw, 'updated_at', '—')),
    stale: pickBool(raw, 'stale', false),
  };
}

/** `/api/outlets` 行 → ForwarderRecord（北向出口）。 */
function mapForwarder(raw: Record<string, unknown>, idx: number): ForwarderRecord {
  const qosRaw = pickNum(raw, 'qos', 1);
  const encoding = pickStr(raw, 'encoding', 'protobuf');
  return {
    id: pickStr(raw, 'id', `fwd-real-${idx}`),
    name: pickStr(raw, 'name', `出口-${idx + 1}`),
    brokerUrl: pickStr(raw, 'brokerUrl', pickStr(raw, 'broker_url', '—')),
    transportSecurity: pickStr(raw, 'transportSecurity', pickStr(raw, 'transport_security', '—')),
    certStatusText: pickStr(raw, 'certStatusText', '—'),
    clientId: pickStr(raw, 'clientId', pickStr(raw, 'client_id', '')),
    qos: (qosRaw === 0 || qosRaw === 1 || qosRaw === 2 ? qosRaw : 1) as ForwarderRecord['qos'],
    retained: pickBool(raw, 'retained', false),
    topicTemplate: pickStr(raw, 'topicTemplate', pickStr(raw, 'topic_template', '')),
    encoding: (encoding === 'json' ? 'json' : 'protobuf') as Encoding,
    status: pickStr(raw, 'status', 'disconnected') as ForwarderRecord['status'],
    connectedForText: pickStr(raw, 'connectedForText', '—'),
    coveredDevices: pickNum(raw, 'coveredDevices', 0),
    recommendedDeviceLimit: pickNum(raw, 'recommendedDeviceLimit', 0),
    lastConsistencyCheckAt: pickStr(raw, 'lastConsistencyCheckAt', '—'),
    enabled: pickBool(raw, 'enabled', true),
  };
}

const ALARM_LEVEL_LABELS: Readonly<Record<string, string>> = {
  critical: '严重',
  major: '重要',
  minor: '次要',
  warning: '提示',
};

const ALARM_STATE_LABELS: Readonly<Record<string, string>> = {
  open: '待处理',
  acking: '已确认',
  resolved: '已恢复',
};

/** `/api/events` 行 → AuditEntry（日志与审计页）。 */
function mapEventAudit(raw: Record<string, unknown>, idx: number): AuditEntry {
  return {
    id: pickStr(raw, 'id', `evt-${idx}`),
    ts: pickStr(raw, 'ts', pickStr(raw, 'time', '—')),
    actor: pickStr(raw, 'actor', 'system'),
    actorType: pickStr(raw, 'actorType', pickStr(raw, 'actor_type', 'system')),
    action: pickStr(raw, 'action', '—'),
    entityType: pickStr(raw, 'entityType', pickStr(raw, 'entity_type', 'system')),
    entityLabel: pickStr(raw, 'entityLabel', pickStr(raw, 'entity_label', '系统')),
    entityId: pickStr(raw, 'entityId', pickStr(raw, 'entity_id', '—')),
    detail: pickStr(raw, 'detail', '—'),
    ip: pickStr(raw, 'ip', '127.0.0.1'),
    result: pickStr(raw, 'result', 'success'),
  };
}

/** `/api/events` 行 → AlarmRecord；不具备告警形状（无 level 且无 title）时返回 null。 */
function mapEventAlarm(raw: Record<string, unknown>, idx: number): AlarmRecord | null {
  const level = pickStr(raw, 'level', '');
  const title = pickStr(raw, 'title', '');
  if (!level && !title) {
    return null;
  }
  const state = pickStr(raw, 'state', 'open');
  return {
    id: pickStr(raw, 'id', `evt-al-${idx}`),
    level: (['critical', 'major', 'minor', 'warning'].includes(level) ? level : 'minor') as AlarmRecord['level'],
    levelLabel: ALARM_LEVEL_LABELS[level] ?? '次要',
    sourceType: pickStr(raw, 'sourceType', pickStr(raw, 'source_type', 'system')),
    sourceLabel: pickStr(raw, 'sourceLabel', pickStr(raw, 'source', '—')),
    title: title || '—',
    detail: pickStr(raw, 'detail', ''),
    firstSeenAt: pickStr(raw, 'firstSeenAt', pickStr(raw, 'first_seen_at', '—')),
    lastSeenAt: pickStr(raw, 'lastSeenAt', pickStr(raw, 'last_seen_at', '—')),
    count: pickNum(raw, 'count', 1),
    state: (['open', 'acking', 'resolved'].includes(state) ? state : 'open') as AlarmRecord['state'],
    stateLabel: ALARM_STATE_LABELS[state] ?? '待处理',
    ackedBy: pickStr(raw, 'ackedBy', pickStr(raw, 'acked_by', '')),
    note: pickStr(raw, 'note', ''),
  };
}

/** `/api/status` → GatewayInfo。
 *
 *  大数红线：`totalForwardedRecords` 走 `pickStr` 字符串直通；
 *  其余均为小值业务计数，经 `pickNum` 安全转换（后端即使返回字符串也不丢精度）。
 */
function mapStatus(raw: Record<string, unknown>): GatewayInfo {
  const dflt = mockRepo.getGateway();
  return {
    name: pickStr(raw, 'name', dflt.name),
    machineCode: pickStr(raw, 'machineCode', dflt.machineCode),
    deployMode: pickStr(raw, 'deployMode', dflt.deployMode),
    version: pickStr(raw, 'version', dflt.version),
    hostname: pickStr(raw, 'hostname', dflt.hostname),
    manageUrl: pickStr(raw, 'manageUrl', dflt.manageUrl),
    port: pickNum(raw, 'port', dflt.port),
    startedAt: pickStr(raw, 'startedAt', dflt.startedAt),
    uptimeText: pickStr(raw, 'uptimeText', dflt.uptimeText),
    deviceCount: pickNum(raw, 'deviceCount', dflt.deviceCount),
    onlineCount: pickNum(raw, 'onlineCount', dflt.onlineCount),
    pointCount: pickNum(raw, 'pointCount', dflt.pointCount),
    failedPointCount: pickNum(raw, 'failedPointCount', dflt.failedPointCount),
    sampleRatePerSec: pickNum(raw, 'sampleRatePerSec', dflt.sampleRatePerSec),
    forwardRatePerSec: pickNum(raw, 'forwardRatePerSec', dflt.forwardRatePerSec),
    queueUsedGb: pickNum(raw, 'queueUsedGb', dflt.queueUsedGb),
    queueCapacityGb: pickNum(raw, 'queueCapacityGb', dflt.queueCapacityGb),
    queueDrainDays: pickNum(raw, 'queueDrainDays', dflt.queueDrainDays),
    totalForwardedRecords: pickStr(raw, 'totalForwardedRecords', dflt.totalForwardedRecords),
  };
}

// ---------------------------------------------------------------------------
// preload（登录成功后 / 带token刷新时调用一次）
// ---------------------------------------------------------------------------

/** 拉取设备清单（失败保持空缓存 → 读取方法回退 mock）。 */
async function fetchDevices(): Promise<void> {
  try {
    const raw = await apiRequest<unknown[]>('/api/devices');
    realCache.devices = Array.isArray(raw) ? raw.map((row, i) => mapDevice(asRecord(row), i)) : [];
  } catch {
    realCache.devices = [];
  }
}

/** 拉取点位清单（可选 `device_id` 过滤；失败保持空缓存）。 */
async function fetchPoints(deviceId: string): Promise<void> {
  try {
    const qs = deviceId ? `?device_id=${encodeURIComponent(deviceId)}` : '';
    const raw = await apiRequest<unknown[]>(`/api/points${qs}`);
    realCache.points = Array.isArray(raw) ? raw.map((row, i) => mapPoint(asRecord(row), i)) : [];
  } catch {
    realCache.points = [];
  }
}

/** 拉取北向出口清单。 */
async function fetchOutlets(): Promise<void> {
  try {
    const raw = await apiRequest<unknown[]>('/api/outlets');
    realCache.outlets = Array.isArray(raw) ? raw.map((row, i) => mapForwarder(asRecord(row), i)) : [];
  } catch {
    realCache.outlets = [];
  }
}

/** 拉取网关状态。 */
async function fetchStatus(): Promise<void> {
  try {
    const raw = await apiRequest<Record<string, unknown>>('/api/status');
    realCache.status = mapStatus(asRecord(raw));
  } catch {
    realCache.status = null;
  }
}

/** 拉取事件流（宽容映射为审计条目 + 告警两类形状）。 */
async function fetchEvents(): Promise<void> {
  try {
    const raw = await apiRequest<unknown[]>('/api/events');
    const rows = Array.isArray(raw) ? raw.map(asRecord) : [];
    realCache.events = rows.map((row, i) => mapEventAudit(row, i));
    realCache.alarms = rows.map((row, i) => mapEventAlarm(row, i)).filter((a): a is AlarmRecord => a !== null);
  } catch {
    realCache.events = [];
    realCache.alarms = [];
  }
}

/** 把 unknown 收敛为 Record（数组 / 非对象回空对象）。 */
function asRecord(value: unknown): Record<string, unknown> {
  return value !== null && typeof value === 'object' && !Array.isArray(value)
    ? (value as Record<string, unknown>)
    : {};
}

/**
 * 预取真实数据（real 模式专用；mock 模式直接返回 false）。
 *
 * 五路请求 `allSettled` 并行：任一失败只影响自身缓存（回退 mock），不影响其余。
 *
 * @returns 全部成功返回 true（仅供日志 / 调试，页面无需关心）
 */
export async function preloadRealData(): Promise<boolean> {
  if (API_MODE !== 'real') {
    return false;
  }
  const results = await Promise.allSettled([fetchDevices(), fetchPoints(''), fetchOutlets(), fetchStatus(), fetchEvents()]);
  realCache.loaded = true;
  const okCount = results.filter((r) => r.status === 'fulfilled').length;
  if (okCount < results.length) {
    // 容差：部分失败已由各读取方法回退 mock，这里仅打印便于联调排障
    console.warn(`[web-console] preloadRealData：${okCount}/${results.length} 接口成功，失败分组已回退 mock 数据`);
  }
  return okCount === results.length;
}

// ---------------------------------------------------------------------------
// 生效视图（真实缓存 + 覆盖层合成；缓存为空时回退 mock 数据）
// ---------------------------------------------------------------------------

/** 生效设备清单。 */
function effectiveDevices(): DeviceRecord[] {
  const base = realCache.devices.length > 0 ? realCache.devices : mockRepo.allDevices();
  const kept = base.filter((d) => !overlay.deletedDeviceIds.has(d.id));
  return [...overlay.newDevices, ...kept];
}

/** 生效点位清单。 */
function effectivePoints(): PointRecord[] {
  const base = realCache.points.length > 0 ? realCache.points : mockRepo.allPoints();
  const kept = base.filter((p) => !overlay.deletedPointIds.has(p.id));
  return [...overlay.newPoints, ...kept];
}

/** 生效北向出口清单（含编码覆盖）。 */
function effectiveOutlets(): ForwarderRecord[] {
  const base = realCache.outlets.length > 0 ? realCache.outlets : mockRepo.allForwarders();
  return base.map((f) => {
    const enc = overlay.outletEncoding.get(f.id);
    return enc ? { ...f, encoding: enc } : f;
  });
}

/** 生效告警清单（含处置状态覆盖）。 */
function effectiveAlarms(): AlarmRecord[] {
  const base = realCache.alarms.length > 0 ? realCache.alarms : mockRepo.allAlarms();
  return base.map((a) => {
    const o = overlay.alarmStates.get(a.id);
    if (!o) {
      return a;
    }
    return { ...a, state: o.state, stateLabel: ALARM_STATE_LABELS[o.state] ?? a.stateLabel, note: o.note, ackedBy: o.ackedBy };
  });
}

/** 生效审计清单（本地覆盖层在前 + 运维日志 + 事件流）。 */
function effectiveAudit(): AuditEntry[] {
  const base = realCache.events.length > 0 ? realCache.events : mockRepo.queryAudit({ actorType: '', action: '', entityType: '', result: '', page: 1, pageSize: 100000 }).items;
  return [...overlay.localAudit, ...base];
}

/** 通用分页。 */
function paginate<T>(items: T[], page: number, pageSize: number): Paged<T> {
  const start = (page - 1) * pageSize;
  return { items: items.slice(start, start + pageSize), total: items.length, page };
}

/** 刷新某设备的点位计数（覆盖层与缓存中的设备对象就地更新）。 */
function refreshPointCount(deviceId: string): void {
  const target = overlay.newDevices.find((d) => d.id === deviceId) ?? realCache.devices.find((d) => d.id === deviceId);
  if (target) {
    target.pointCount = effectivePoints().filter((p) => p.deviceId === deviceId).length;
  }
}

// ===========================================================================
// real 模式仓库实现（与 mock repo 同签名；未列出的方法继承 mock 行为）
// ===========================================================================

/** real 模式仓库（读：缓存+覆盖层；写：覆盖层；契约外方法回退 mock）。 */
function buildRealRepo(): typeof mockRepo {
  return {
    ...mockRepo,

    // ---------- 网关信息 ----------
    getGateway(): GatewayInfo {
      return realCache.status ?? mockRepo.getGateway();
    },

    // ---------- 设备（读） ----------
    queryDevices(query: { status: string; protocol: string; keyword: string; page: number; pageSize: number }): Paged<DeviceRecord> {
      const kw = query.keyword.trim().toLowerCase();
      const filtered = effectiveDevices().filter((d) => {
        if (query.status && d.status !== query.status) {
          return false;
        }
        if (query.protocol && d.protocol !== query.protocol) {
          return false;
        }
        if (kw) {
          const haystack = `${d.name} ${d.connectionSummary} ${d.protocolLabel}`.toLowerCase();
          if (!haystack.includes(kw)) {
            return false;
          }
        }
        return true;
      });
      return paginate(filtered, query.page, query.pageSize);
    },

    allDevices(): DeviceRecord[] {
      return effectiveDevices();
    },

    getDevice(id: string): DeviceRecord | null {
      return effectiveDevices().find((d) => d.id === id) ?? null;
    },

    // ---------- 点位（读） ----------
    queryPoints(query: { deviceId: string; pointType: string; quality: string; keyword: string; page: number; pageSize: number }): Paged<PointRecord> {
      const kw = query.keyword.trim().toLowerCase();
      const filtered = effectivePoints().filter((p) => {
        if (query.deviceId && p.deviceId !== query.deviceId) {
          return false;
        }
        if (query.pointType && p.pointType !== query.pointType) {
          return false;
        }
        if (query.quality && p.quality !== query.quality) {
          return false;
        }
        if (kw) {
          const haystack = `${p.name} ${p.targetKey} ${p.address}`.toLowerCase();
          if (!haystack.includes(kw)) {
            return false;
          }
        }
        return true;
      });
      return paginate(filtered, query.page, query.pageSize);
    },

    allPoints(): PointRecord[] {
      return effectivePoints();
    },

    pointsOfDevice(deviceId: string): PointRecord[] {
      return effectivePoints().filter((p) => p.deviceId === deviceId);
    },

    // ---------- 北向出口（读 + 编码覆盖） ----------
    allForwarders(): ForwarderRecord[] {
      return effectiveOutlets();
    },

    getForwarder(id: string): ForwarderRecord | null {
      return effectiveOutlets().find((f) => f.id === id) ?? null;
    },

    setForwarderEncoding(input: { id: string; encoding: Encoding; actor: string }): boolean {
      const target = effectiveOutlets().find((f) => f.id === input.id);
      if (!target) {
        return false;
      }
      overlay.outletEncoding.set(input.id, input.encoding);
      pushLocalAudit({
        actor: input.actor,
        actorType: 'human',
        action: '修改北向编码',
        entityType: 'forwarder',
        entityLabel: '北向出口',
        entityId: target.id,
        detail: `${target.name}：${target.encoding} → ${input.encoding}（本地覆盖层，后端契约未含写接口）`,
        result: 'success',
      });
      return true;
    },

    // ---------- 告警（读 + 处置覆盖） ----------
    allAlarms(): AlarmRecord[] {
      return effectiveAlarms();
    },

    resolveAlarm(input: { id: string; state: AlarmState; note: string; actor: string }): boolean {
      const target = effectiveAlarms().find((a) => a.id === input.id);
      if (!target) {
        return false;
      }
      overlay.alarmStates.set(input.id, { state: input.state, note: input.note, ackedBy: input.actor });
      pushLocalAudit({
        actor: input.actor,
        actorType: 'human',
        action: '处置告警',
        entityType: 'alarm',
        entityLabel: '告警',
        entityId: target.id,
        detail: `${target.title} → ${ALARM_STATE_LABELS[input.state] ?? input.state} · ${input.note}`,
        result: 'success',
      });
      return true;
    },

    // ---------- 审计（读） ----------
    queryAudit(query: { actorType: string; action: string; entityType: string; result: string; page: number; pageSize: number }): Paged<AuditEntry> {
      const filtered = effectiveAudit().filter((log) => {
        if (query.actorType && log.actorType !== query.actorType) {
          return false;
        }
        if (query.action && !log.action.includes(query.action)) {
          return false;
        }
        if (query.entityType && log.entityType !== query.entityType) {
          return false;
        }
        if (query.result && log.result !== query.result) {
          return false;
        }
        return true;
      });
      return paginate(filtered, query.page, query.pageSize);
    },

    // ---------- 设备写操作（覆盖层；后端契约未含设备写接口） ----------
    createDevice(input: DeviceDraft): DeviceRecord {
      const id = `dev-${String(Date.now()).slice(-6)}`;
      const protocolLabel = PROTOCOL_OPTIONS.find((p) => p.value === input.protocol)?.label ?? input.protocol;
      const record: DeviceRecord = {
        id,
        name: input.name.trim(),
        protocol: input.protocol,
        protocolLabel,
        connectionSummary: input.connectionSummary.trim(),
        status: 'offline',
        intervalMs: input.intervalMs,
        timeoutMs: input.timeoutMs,
        retryTimes: input.retryTimes,
        pointCount: 0,
        lastSampleAt: '—',
        successRate: 0,
        failStreak: 0,
        offlineText: '尚未接入',
        createdAt: nowText(),
      };
      overlay.newDevices.push(record);
      pushLocalAudit({
        actor: input.actor,
        actorType: 'human',
        action: '新增设备',
        entityType: 'device',
        entityLabel: '设备',
        entityId: id,
        detail: `${record.name} · ${protocolLabel} · ${record.connectionSummary}（本地覆盖层）`,
        result: 'success',
      });
      return { ...record };
    },

    deleteDevice(input: { id: string; actor: string }): boolean {
      const removed = effectiveDevices().find((d) => d.id === input.id);
      if (!removed) {
        return false;
      }
      const ni = overlay.newDevices.findIndex((d) => d.id === input.id);
      if (ni >= 0) {
        overlay.newDevices.splice(ni, 1);
      } else {
        overlay.deletedDeviceIds.add(input.id);
      }
      // 级联删除其下点位（与 mock 语义一致）
      for (const p of effectivePoints()) {
        if (p.deviceId === input.id) {
          const pi = overlay.newPoints.findIndex((x) => x.id === p.id);
          if (pi >= 0) {
            overlay.newPoints.splice(pi, 1);
          } else {
            overlay.deletedPointIds.add(p.id);
          }
        }
      }
      pushLocalAudit({
        actor: input.actor,
        actorType: 'human',
        action: '删除设备',
        entityType: 'device',
        entityLabel: '设备',
        entityId: input.id,
        detail: `${removed.name}（含其下全部点位一并删除；本地覆盖层）`,
        result: 'success',
      });
      return true;
    },

    // ---------- 点位写操作（覆盖层） ----------
    createPoint(input: PointDraft): PointRecord {
      const id = `pt-${String(Date.now()).slice(-7)}-${effectivePoints().length}`;
      const device = effectiveDevices().find((d) => d.id === input.deviceId);
      const isDerived = input.pointType === 'derived';
      const record: PointRecord = {
        id,
        deviceId: input.deviceId,
        deviceName: device?.name ?? '—',
        name: input.name.trim(),
        pointType: input.pointType,
        address: isDerived ? '—' : input.address.trim(),
        dataType: input.dataType,
        byteOrder: isDerived ? '—' : input.byteOrder,
        unit: input.unit,
        deadband: input.deadband,
        targetKey: input.targetKey.trim(),
        quality: 'Good',
        value: isDerived ? null : 0,
        valueText: isDerived ? '—' : '0',
        formula: isDerived ? (input.formula ?? '') : null,
        updatedAt: nowText(),
        stale: false,
      };
      overlay.newPoints.push(record);
      refreshPointCount(input.deviceId);
      pushLocalAudit({
        actor: input.actor ?? '',
        actorType: 'human',
        action: '新增点位',
        entityType: 'point',
        entityLabel: '点位',
        entityId: id,
        detail: `${record.deviceName} / ${record.name} · ${record.targetKey}（本地覆盖层）`,
        result: 'success',
      });
      return { ...record };
    },

    deletePoint(input: { id: string; actor: string }): boolean {
      const removed = effectivePoints().find((p) => p.id === input.id);
      if (!removed) {
        return false;
      }
      const ni = overlay.newPoints.findIndex((p) => p.id === input.id);
      if (ni >= 0) {
        overlay.newPoints.splice(ni, 1);
      } else {
        overlay.deletedPointIds.add(input.id);
      }
      refreshPointCount(removed.deviceId);
      pushLocalAudit({
        actor: input.actor,
        actorType: 'human',
        action: '删除点位',
        entityType: 'point',
        entityLabel: '点位',
        entityId: input.id,
        detail: `${removed.deviceName} / ${removed.name}（本地覆盖层）`,
        result: 'success',
      });
      return true;
    },

    replacePointsOfDevice(input: { deviceId: string; rows: PointDraft[]; actor: string }): number {
      // 先删旧点
      for (const p of effectivePoints()) {
        if (p.deviceId === input.deviceId) {
          const pi = overlay.newPoints.findIndex((x) => x.id === p.id);
          if (pi >= 0) {
            overlay.newPoints.splice(pi, 1);
          } else {
            overlay.deletedPointIds.add(p.id);
          }
        }
      }
      // 再写入新点
      const device = effectiveDevices().find((d) => d.id === input.deviceId);
      let count = 0;
      for (const row of input.rows) {
        const isDerived = row.pointType === 'derived';
        const id = `pt-${String(Date.now()).slice(-7)}-${count}`;
        overlay.newPoints.push({
          id,
          deviceId: input.deviceId,
          deviceName: device?.name ?? '—',
          name: row.name.trim(),
          pointType: row.pointType,
          address: isDerived ? '—' : row.address.trim(),
          dataType: row.dataType,
          byteOrder: isDerived ? '—' : row.byteOrder,
          unit: row.unit,
          deadband: row.deadband,
          targetKey: row.targetKey.trim(),
          quality: 'Good',
          value: isDerived ? null : 0,
          valueText: isDerived ? '—' : '0',
          formula: isDerived ? (row.formula ?? '') : null,
          updatedAt: nowText(),
          stale: false,
        });
        count += 1;
      }
      refreshPointCount(input.deviceId);
      pushLocalAudit({
        actor: input.actor,
        actorType: 'human',
        action: '导入点表',
        entityType: 'point',
        entityLabel: '点位',
        entityId: input.deviceId,
        detail: `${device?.name ?? input.deviceId}：覆盖导入 ${count} 个点位（本地覆盖层）`,
        result: 'success',
      });
      return count;
    },
  };
}

// ===========================================================================
// 运维动作（ops）：real → 真实接口；mock → 模拟成功 + 本地审计
// ===========================================================================

/** 运维动作 API（repo.ops.*）。 */
export interface OpsApi {
  /** 触发网关重启（real：`POST /api/ops/restart`；危险操作二次确认由页面层完成）。 */
  restart(actor: string): Promise<{ ok: boolean; message: string }>;
  /** 新建采集器（real：`POST /api/ops/collectors`）。 */
  createCollector(input: { name: string; deviceId?: string; actor: string }): Promise<{ ok: boolean; message: string }>;
  /** 拉取运行日志（real：`GET /api/ops/logs`，结果并入审计清单）。 */
  logs(): Promise<AuditEntry[]>;
  /** 健康检查（real：`GET /api/health`；403 表示权限不足）。 */
  health(): Promise<{ ok: boolean; message: string }>;
}

/** 运维动作实现（两种模式都可用）。 */
function buildOps(): OpsApi {
  return {
    async restart(actor: string): Promise<{ ok: boolean; message: string }> {
      if (API_MODE !== 'real') {
        pushLocalAudit({
          actor,
          actorType: 'human',
          action: '重启网关',
          entityType: 'system',
          entityLabel: '系统',
          entityId: 'gateway',
          detail: 'mock 模式：模拟重启成功',
          result: 'success',
        });
        return { ok: true, message: '重启指令已下发（mock 模拟）。' };
      }
      try {
        await apiRequest<unknown>('/api/ops/restart', { method: 'POST' });
        return { ok: true, message: '重启指令已下发，网关将在数秒内重启。' };
      } catch (cause) {
        if (cause instanceof Error && 'status' in cause && (cause as { status: number }).status === 403) {
          return { ok: false, message: '权限不足（403），当前角色不可执行重启。' };
        }
        return { ok: false, message: cause instanceof Error ? cause.message : '重启指令下发失败。' };
      }
    },

    async createCollector(input: { name: string; deviceId?: string; actor: string }): Promise<{ ok: boolean; message: string }> {
      if (API_MODE !== 'real') {
        pushLocalAudit({
          actor: input.actor,
          actorType: 'human',
          action: '新建采集器',
          entityType: 'device',
          entityLabel: '采集器',
          entityId: input.name,
          detail: `mock 模式：模拟创建采集器 ${input.name}`,
          result: 'success',
        });
        return { ok: true, message: '采集器已创建（mock 模拟）。' };
      }
      try {
        await apiRequest<unknown>('/api/ops/collectors', {
          method: 'POST',
          body: JSON.stringify({ name: input.name, device_id: input.deviceId ?? '' }),
        });
        return { ok: true, message: `采集器 ${input.name} 已创建。` };
      } catch (cause) {
        if (cause instanceof Error && 'status' in cause && (cause as { status: number }).status === 403) {
          return { ok: false, message: '权限不足（403），当前角色不可创建采集器。' };
        }
        return { ok: false, message: cause instanceof Error ? cause.message : '采集器创建失败。' };
      }
    },

    async logs(): Promise<AuditEntry[]> {
      if (API_MODE !== 'real') {
        return mockRepo.queryAudit({ actorType: '', action: '', entityType: '', result: '', page: 1, pageSize: 100 }).items;
      }
      try {
        const raw = await apiRequest<unknown[]>('/api/ops/logs');
        const entries = (Array.isArray(raw) ? raw : []).map((row, i) => mapEventAudit(asRecord(row), i));
        realCache.events = [...entries, ...realCache.events];
        return entries;
      } catch {
        return [];
      }
    },

    async health(): Promise<{ ok: boolean; message: string }> {
      if (API_MODE !== 'real') {
        return { ok: true, message: 'mock 模式：服务正常（模拟）。' };
      }
      try {
        await apiRequest<unknown>('/api/health');
        return { ok: true, message: '网关服务正常。' };
      } catch (cause) {
        if (cause instanceof Error && 'status' in cause && (cause as { status: number }).status === 403) {
          return { ok: false, message: '权限不足（403）。' };
        }
        return { ok: false, message: cause instanceof Error ? cause.message : '健康检查失败。' };
      }
    },
  };
}

// ===========================================================================
// 统一导出
// ===========================================================================

/** 页面层唯一数据入口：mock 模式 = mock 仓库；real 模式 = 真实缓存 + 覆盖层。 */
const baseRepo: typeof mockRepo = API_MODE === 'real' ? buildRealRepo() : mockRepo;

/** 最终仓库（含 ops 运维动作；mock 模式下 ops 为模拟实现）。 */
export const repo: typeof mockRepo & { ops: OpsApi } = { ...baseRepo, ops: buildOps() };

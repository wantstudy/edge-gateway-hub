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
 *    `/api/devices · /api/points（逐设备）· /api/outlets · /api/overview · /api/events`
 *    并缓存；读取型方法优先返回真实缓存，**任一接口失败 / 为空则回退 mock 数据**
 *    （任务约束「容差由 fallback 兜底」）；写操作（后端契约未含设备/点位写接口）
 *    落本地覆盖层（overlay），语义与 mock 一致，供本地端到端走通页面流程。
 *  · **大数红线**：`totalForwardedRecords` 等大数字段走字符串直通（`pickStr`），
 *    绝不 parseFloat；仅「小值业务计数」经 `pickNum` 安全转换。
 *  · **不改 mock-data.ts 本身**（它是契约与 fallback）。
 */
import {
  DEFAULT_ACTOR,
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
  type MockLicense,
  type Paged,
  type PointDraft,
  type PointRecord,
  type ProtocolType,
  type RuleRecord,
} from '../mock/mock-data';

// 透传 mock-data 的类型 / 常量 / 快照（`repo` 由下方局部导出遮蔽，属 ES 模块规范行为）
export * from '../mock/mock-data';

import { ref, type Ref } from 'vue';
import { API_MODE, ApiError, apiRequest, apiRequestText } from './client';

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
  /** 审计条目（唯一数据源：`GET /api/audit`——有限 JSON；**不是** `/api/events` 无限 SSE 流） */
  events: AuditEntry[];
  /** 告警（`GET /api/alerts`；后端无告警引擎 → 诚实空态，绝不回退 mock 造假） */
  alarms: AlarmRecord[];
  /** 转发规则（`GET /api/rules`；后端无规则引擎 → 诚实空态） */
  rules: RuleRecord[];
  /** 授权状态（`GET /api/license/status` 原始快照；全字段字符串透传） */
  license: LicenseStatusSnapshot | null;
  /** 各数据源的「不可得原因」（诚实空态 / 501 / 503 / 网络失败；空串 = 正常） */
  notices: Record<NoticeKey, string>;
}

/** 可解释提示的键（对应一处真实数据源）。 */
export type NoticeKey = 'audit' | 'alerts' | 'rules' | 'license' | 'forwarders' | 'points';

/** 缓存初始值（全部为空 → 各读取方法回退 mock / 诚实空态）。 */
const realCache: RealCache = {
  loaded: false,
  devices: [],
  points: [],
  outlets: [],
  status: null,
  events: [],
  alarms: [],
  rules: [],
  license: null,
  notices: { audit: '', alerts: '', rules: '', license: '', forwarders: '', points: '' },
};

/**
 * 缓存版本号（`dataVersion` 的源）。
 *
 * real 模式下 preload 在**后台**跑，挂载 / 登录不再等待它；缓存填充完成后
 * 本值自增，页面可用 `watch(dataVersion, reload)` 由响应式驱动刷新
 * （总览页等带 tick 的页面无需额外接线）。
 */
let cacheVersion = 0;

/** 读取当前缓存版本号（配合 `dataVersion` 使用）。 */
export function getCacheVersion(): number {
  return cacheVersion;
}

/**
 * 缓存版本（响应式）：real 模式后台 preload 完成后自增。
 *
 * 页面可 `watch(dataVersion, reload)` 实现「缓存填充后自动刷新」，
 * 从而不必在挂载 / 登录路径上阻塞等待网络。
 */
export const dataVersion: Ref<number> = ref(0);

/** 自增缓存版本（后台 preload 每完成一批分组调用一次）。 */
function bumpCacheVersion(): void {
  cacheVersion += 1;
  dataVersion.value = cacheVersion;
}

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

/** `/api/points` 行 → PointRecord。
 *
 * 后端行形状（蛇形命名）：`{ device_id, point_id, protocol, address, frequency_ms }`。
 * `id` 取后端 `point_id`（无则回退 `id` 字段）——这是实时遥测流（`/api/stream`）
 * 逐点 key（`${device_id}/${point_id}`）的对齐锚点，**不可改名**。
 */
function mapPoint(raw: Record<string, unknown>, idx: number): PointRecord {
  const value = raw.value === null || raw.value === undefined ? null : pickNum(raw, 'value', 0);
  const quality = pickStr(raw, 'quality', 'Good') as PointRecord['quality'];
  return {
    id: pickStr(raw, 'id', pickStr(raw, 'point_id', `pt-real-${idx}`)),
    deviceId: pickStr(raw, 'deviceId', pickStr(raw, 'device_id', '')),
    deviceName: pickStr(raw, 'deviceName', pickStr(raw, 'device_name', '—')),
    name: pickStr(raw, 'name', pickStr(raw, 'point_id', `点位-${idx + 1}`)),
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
    // 后端 /api/forwarders 实测契约（2026-09-25 live 联调）：
    // target / qos(数字字符串) / tls(bool) / topic_prefix；mock 与旧字段名保留为回退。
    brokerUrl: pickStr(raw, 'target', pickStr(raw, 'brokerUrl', pickStr(raw, 'broker_url', '—'))),
    transportSecurity:
      typeof raw.tls === 'boolean'
        ? raw.tls
          ? 'TLS'
          : 'TCP'
        : pickStr(raw, 'transportSecurity', pickStr(raw, 'transport_security', '—')),
    certStatusText: pickStr(raw, 'certStatusText', '—'),
    clientId: pickStr(raw, 'clientId', pickStr(raw, 'client_id', '')),
    qos: (qosRaw === 0 || qosRaw === 1 || qosRaw === 2 ? qosRaw : 1) as ForwarderRecord['qos'],
    retained: pickBool(raw, 'retained', false),
    topicTemplate: pickStr(raw, 'topicTemplate', pickStr(raw, 'topic_prefix', pickStr(raw, 'topic_template', ''))),
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

/** 后端审计事件类型字面量 → 中文动作名（未知字面量原样透传，不猜译）。 */
const AUDIT_EVENT_LABELS: Readonly<Record<string, string>> = {
  login: '登录成功',
  login_failed: '登录失败',
  config_change: '配置变更',
  authz_failed: '鉴权拒绝',
  trial_expired: '试用期到期',
  audit_read: '审计查询',
  audit_export: '审计导出',
};

/** 后端审计 outcome → 前端 `AuditEntry.result` 域（success / denied / failed）。 */
const AUDIT_OUTCOME_MAP: Readonly<Record<string, string>> = {
  accepted: 'success',
  denied: 'denied',
  bad_request: 'failed',
  failed: 'failed',
};

/**
 * `/api/audit` 行 → AuditEntry（复用 `mapEventAudit` 的统一映射）。
 *
 * 后端行形状：`{seq, ts_ns, actor, event, outcome, detail, prev_hash, entry_hash}`
 * （`seq` / `ts_ns` 为字符串编码的大数，红线要求透传）。这里先做「字段名与量纲
 * 归一化」，再交给 `mapEventAudit` 落前端形状，保证只有一条映射路径。
 */
function mapAuditRow(raw: Record<string, unknown>, idx: number): AuditEntry {
  const event = pickStr(raw, 'event', '');
  const outcome = pickStr(raw, 'outcome', '');
  return mapEventAudit(
    {
      // 大数红线：seq 为链内序号字符串，原样作为主键，绝不 parseInt。
      id: pickStr(raw, 'seq', `audit-${idx}`),
      ts: formatNsText(pickStr(raw, 'ts_ns', '')),
      actor: pickStr(raw, 'actor', 'system'),
      actorType: 'human',
      action: AUDIT_EVENT_LABELS[event] ?? (event || '—'),
      entityType: 'system',
      entityLabel: '系统',
      entityId: event || '—',
      detail: pickStr(raw, 'detail', '—'),
      // 后端审计记录不含来源 IP：诚实留空占位，不伪造地址。
      ip: '',
      result: AUDIT_OUTCOME_MAP[outcome] ?? (outcome || 'success'),
    },
    idx,
  );
}

/**
 * UTC 纳秒字符串 → `YYYY-MM-DD HH:mm:ss`。
 *
 * 大数红线：`ts_ns`（≈1.7e18）超出 `Number` 安全整数区间，故**只截取秒段**
 * （去掉末 9 位）再格式化，全程不把整串转成数字，避免精度损失与误用。
 */
function formatNsText(value: string): string {
  const trimmed = value.trim();
  if (!/^\d+$/.test(trimmed)) {
    return trimmed || '—';
  }
  const secsText = trimmed.length > 9 ? trimmed.slice(0, -9) : '0';
  const secs = Number(secsText);
  if (!Number.isFinite(secs) || secs <= 0) {
    return '—';
  }
  return formatDateTimeMs(secs * 1000);
}

/** `/api/rules` 行 → RuleRecord（后端当前恒空；保留宽容映射防未来字段漂移）。 */
function mapRule(raw: Record<string, unknown>, idx: number): RuleRecord {
  return {
    id: pickStr(raw, 'id', `rule-${idx}`),
    name: pickStr(raw, 'name', `规则-${idx + 1}`),
    forwarderId: pickStr(raw, 'forwarderId', pickStr(raw, 'forwarder_id', '')),
    forwarderName: pickStr(raw, 'forwarderName', pickStr(raw, 'forwarder_name', '—')),
    condition: pickStr(raw, 'condition', '—'),
    action: pickStr(raw, 'action', '—'),
    hitCount: pickNum(raw, 'hitCount', pickNum(raw, 'hit_count', 0)),
    priority: pickNum(raw, 'priority', 0),
    enabled: pickBool(raw, 'enabled', true),
    lastHitAt: pickStr(raw, 'lastHitAt', pickStr(raw, 'last_hit_at', '—')),
  };
}

/** 授权状态快照（`GET /api/license/status` 原始形状；**全字段字符串**，绝不数值化）。 */
export interface LicenseStatusSnapshot {
  /** 状态字面量：`unlicensed` / `trial` / `active` / `grace` / `degraded` */
  status: string;
  /** 档位（`free` / `standard` / `pro` …；未授权为空串） */
  tier: string;
  /** 租约有效至（epoch 秒字符串） */
  validUntil: string;
  /** 剩余秒数（字符串） */
  remainingSecs: string;
  /** 剩余天数（字符串） */
  remainingDays: string;
  /** 降级原因（未降级为空串） */
  degradeReason: string;
  /** 北向转发是否放行（免费版 / 未授权为 false） */
  northForwardAllowed: boolean;
  /** 后端附注（如 runtime 未装配说明） */
  note: string;
}

/**
 * 统一失败描述（**不静默吞错**）：给出「原因 + 恢复路径」。
 *
 * 501 = 能力未落地；503 = 依赖未装配（如审计库）；403 = 权限不足；
 * 400 = 参数非法（附后端 `message`）；0 = 网络 / 超时。
 */
function describeFailure(cause: unknown, subject: string): string {
  if (cause instanceof ApiError) {
    const detail = extractErrorMessage(cause.body);
    switch (cause.status) {
      case 501:
        return `${subject}：后端能力未落地（501）${detail}。恢复路径：等待该能力上线，当前不可用功能请勿依赖。`;
      case 503:
        return `${subject}：后端依赖未装配（503）${detail}。恢复路径：检查 daemon 启动装配（如审计库挂载）后重试。`;
      case 403:
        return `${subject}：权限不足（403）${detail}。恢复路径：使用具备相应权限的账号登录。`;
      case 404:
        return `${subject}：资源不存在（404）${detail}。`;
      case 400:
        return `${subject}：请求参数非法（400）${detail}。`;
      case 401:
        return `${subject}：登录已失效（401），请重新登录。`;
      default:
        return `${subject}：请求失败（HTTP ${cause.status}）${detail}。`;
    }
  }
  return `${subject}：${cause instanceof Error ? cause.message : String(cause)}`;
}

/** 从后端错误体取人读消息（`{message}` / `{error}` 优先，其次原文）。 */
function extractErrorMessage(body: unknown): string {
  const rec = asRecord(body);
  const message = pickStr(rec, 'message', '');
  if (message) {
    return `——${message}`;
  }
  const error = pickStr(rec, 'error', '');
  if (error) {
    return `——${error}`;
  }
  return typeof body === 'string' && body ? `——${body}` : '';
}

/** epoch 秒 / 毫秒字符串 → `YYYY-MM-DD HH:mm:ss`（非纯时间戳则原样透传）。
 *
 * `/api/overview` 的 `startedAt` 当前为 epoch 秒字符串（小值，远低于 2^53），
 * 为对齐 mock 契约的展示格式做一次**无精度损失**的格式化；若后端未来直接
 * 返回人类可读时间字符串，则不匹配纯数字形态、原样透传（宽容兼容）。
 */
function formatEpochText(value: string): string {
  const trimmed = value.trim();
  if (/^\d{10}$/.test(trimmed)) {
    return formatDateTimeMs(Number(trimmed) * 1000);
  }
  if (/^\d{13}$/.test(trimmed)) {
    return formatDateTimeMs(Number(trimmed));
  }
  return value;
}

/** 毫秒时间戳 → `YYYY-MM-DD HH:mm:ss`。 */
function formatDateTimeMs(ms: number): string {
  const d = new Date(ms);
  const p = (n: number): string => String(n).padStart(2, '0');
  return `${d.getFullYear()}-${p(d.getMonth() + 1)}-${p(d.getDate())} ${p(d.getHours())}:${p(d.getMinutes())}:${p(d.getSeconds())}`;
}

/** `/api/overview` → GatewayInfo。
 *
 * 大数红线：`totalForwardedRecords` 走 `pickStr` 字符串直通；
 * 后端其余计数字段当前也以字符串返回（JSON 大数红线），均为小值业务计数，
 * 经 `pickNum` 安全转换后再参与页面运算/格式化（无精度损失）。
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
    startedAt: formatEpochText(pickStr(raw, 'startedAt', dflt.startedAt)),
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

/** 按设备拉取点位并并入总表（单设备失败只跳过自身，不影响其余设备）。 */
async function fetchPointsOfDevice(deviceId: string, offset: number): Promise<PointRecord[]> {
  const qs = `?device_id=${encodeURIComponent(deviceId)}`;
  const raw = await apiRequest<unknown[]>(`/api/points${qs}`);
  return Array.isArray(raw) ? raw.map((row, i) => mapPoint(asRecord(row), offset + i)) : [];
}

/**
 * 拉取全量点位：`/api/points` 需按 `device_id` 过滤（缺省返回空数组），
 * 因此对各设备（须先由 `fetchDevices()` 填充清单）**并行**拉取点位后合并。
 */
async function fetchAllPoints(): Promise<void> {
  try {
    if (realCache.devices.length === 0) {
      realCache.points = [];
      return; // 设备清单为空 / 不可得：回退 mock 点位
    }
    const settled = await Promise.allSettled(
      realCache.devices.map((d, i) => fetchPointsOfDevice(d.id, i * 1000)),
    );
    const merged: PointRecord[] = [];
    for (const result of settled) {
      if (result.status === 'fulfilled') {
        merged.push(...result.value);
      }
    }
    realCache.points = merged;
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

/** 拉取网关信息（`GET /api/overview`，无需 token；real 模式网关信息唯一来源）。 */
async function fetchOverview(): Promise<void> {
  try {
    const raw = await apiRequest<Record<string, unknown>>('/api/overview');
    realCache.status = mapStatus(asRecord(raw));
  } catch {
    realCache.status = null;
  }
}

/**
 * 拉取审计条目：`GET /api/audit`（**有限 JSON**，RBAC `audit.view`）。
 *
 * ⚠️ D-01 根因修复点：旧实现读 `/api/events`——那是**无限 SSE 事件流**，
 * `res.text()` 永不 resolve，导致 preload 永不 settle（刷新白屏 / 登录卡死）。
 * 审计页的正确数据源是本端点；SSE 通道由 `stream.ts` 用 EventSource 消费。
 *
 * 失败语义（不静默吞错）：503 = 审计库未装配、400 = 查询参数非法、403 = 无
 * `audit.view` 权限——原因写入 `realCache.notices.audit` 供页面给出恢复路径。
 */
async function fetchAudit(): Promise<void> {
  try {
    const raw = await apiRequest<Record<string, unknown>>('/api/audit?limit=200');
    const rows = Array.isArray(raw.rows) ? raw.rows.map(asRecord) : [];
    realCache.events = rows.map((row, i) => mapAuditRow(row, i));
    realCache.notices.audit = '';
  } catch (cause) {
    realCache.events = [];
    realCache.notices.audit = describeFailure(cause, '审计日志');
  }
}

/**
 * 拉取告警：`GET /api/alerts`（后端无告警引擎 → `{items:[],source:"unsupported",reason}`）。
 *
 * **诚实空态**：real 模式下告警清单恒取本端点结果，**绝不回退 mock 告警**、
 * 也绝不从审计行伪造告警（后端 `reason` 原样透传给页面）。
 */
async function fetchAlerts(): Promise<void> {
  try {
    const raw = await apiRequest<Record<string, unknown>>('/api/alerts');
    const rows = Array.isArray(raw.items) ? raw.items.map(asRecord) : [];
    realCache.alarms = rows.map((row, i) => mapEventAlarm(row, i)).filter((a): a is AlarmRecord => a !== null);
    realCache.notices.alerts =
      raw.source === 'unsupported' ? pickStr(raw, 'reason', '后端告警引擎未落地，当前无真实告警数据源') : '';
  } catch (cause) {
    realCache.alarms = [];
    realCache.notices.alerts = describeFailure(cause, '告警');
  }
}

/** 拉取转发规则：`GET /api/rules`（后端无规则引擎 → 诚实空态）。 */
async function fetchRules(): Promise<void> {
  try {
    const raw = await apiRequest<Record<string, unknown>>('/api/rules');
    const rows = Array.isArray(raw.items) ? raw.items.map(asRecord) : [];
    realCache.rules = rows.map((row, i) => mapRule(row, i));
    realCache.notices.rules =
      raw.source === 'unsupported' ? pickStr(raw, 'reason', '后端规则引擎未落地，当前无真实规则数据源') : '';
  } catch (cause) {
    realCache.rules = [];
    realCache.notices.rules = describeFailure(cause, '转发规则');
  }
}

/**
 * 拉取北向出口：`GET /api/forwarders`（`id` = 出口名，前端 id 锚点）；
 * 不可得时落回 `GET /api/outlets`（同一数据源与字段）。
 */
async function fetchForwarders(): Promise<void> {
  try {
    const raw = await apiRequest<unknown[]>('/api/forwarders');
    const rows = Array.isArray(raw) ? raw.map((row, i) => mapForwarder(asRecord(row), i)) : [];
    realCache.outlets = rows;
    realCache.notices.forwarders = '';
  } catch (cause) {
    await fetchOutlets();
    realCache.notices.forwarders = describeFailure(cause, '北向出口');
  }
}

/** 拉取授权状态：`GET /api/license/status`（全字段字符串；未装配诚实返回 `unlicensed`）。 */
async function fetchLicense(): Promise<void> {
  try {
    const raw = await apiRequest<Record<string, unknown>>('/api/license/status');
    realCache.license = {
      status: pickStr(raw, 'status', 'unlicensed'),
      tier: pickStr(raw, 'tier', ''),
      validUntil: pickStr(raw, 'valid_until', ''),
      remainingSecs: pickStr(raw, 'remaining_secs', ''),
      remainingDays: pickStr(raw, 'remaining_days', ''),
      degradeReason: pickStr(raw, 'degrade_reason', ''),
      northForwardAllowed: pickBool(raw, 'north_forward_allowed', false),
      note: pickStr(raw, 'note', ''),
    };
    realCache.notices.license = '';
  } catch (cause) {
    realCache.license = null;
    realCache.notices.license = describeFailure(cause, '授权状态');
  }
}

/** 把 unknown 收敛为 Record（数组 / 非对象回空对象）。 */
function asRecord(value: unknown): Record<string, unknown> {
  return value !== null && typeof value === 'object' && !Array.isArray(value)
    ? (value as Record<string, unknown>)
    : {};
}

/** preload 整体超时护栏（毫秒）：超时后按**已成功分组**继续，不再阻塞调用方。 */
const PRELOAD_TIMEOUT_MS = 8000;

/**
 * 预取真实数据（real 模式专用；mock 模式直接返回 false）。
 *
 * ── D-01 修复要点 ────────────────────────────────────────────────────────────
 *  1. **不再拉取 `/api/events`**：那是无限 SSE 流（`res.text()` 永不 resolve），
 *     旧实现把它放进 `allSettled` 导致本函数永不 settle → 刷新白屏 / 登录卡死。
 *     审计改走有限 JSON 的 `GET /api/audit`；SSE 由 `stream.ts` 的 EventSource 消费。
 *  2. **整体 8s 超时护栏**：`Promise.race` 兜底，超时即以已成功分组继续
 *     （仍在飞行的分组会在后台继续填缓存，完成后自增 `dataVersion`）。
 *     单请求另有 client.ts 的 15s AbortController 超时。
 *
 * 分组并行 `allSettled`：任一失败只影响自身缓存（回退 mock / 诚实空态）。
 *
 * @returns 全部成功返回 true（仅供日志 / 调试，页面无需关心）
 */
export async function preloadRealData(): Promise<boolean> {
  if (API_MODE !== 'real') {
    return false;
  }
  const groups: Promise<void>[] = [
    fetchDevicesAndPoints(),
    fetchForwarders(),
    fetchOverview(),
    fetchAudit(),
    fetchAlerts(),
    fetchRules(),
    fetchLicense(),
  ];
  const raced = await Promise.race([
    Promise.allSettled(groups).then((results) => ({ results, timedOut: false })),
    new Promise<{ results: PromiseSettledResult<void>[] | null; timedOut: boolean }>((resolve) => {
      setTimeout(() => resolve({ results: null, timedOut: true }), PRELOAD_TIMEOUT_MS);
    }),
  ]);

  realCache.loaded = true;
  bumpCacheVersion();

  if (raced.timedOut || raced.results === null) {
    // 超时：已成功的分组已生效，未完成的分组在后台继续填缓存（各自 try/catch 兜底）
    console.warn(`[web-console] preloadRealData：超过 ${PRELOAD_TIMEOUT_MS} ms 未全部完成，按已成功分组继续`);
    // 后台补填完成后再自增一次版本，供页面响应式刷新
    void Promise.allSettled(groups).then(() => bumpCacheVersion());
    return false;
  }

  const okCount = raced.results.filter((r) => r.status === 'fulfilled').length;
  if (okCount < raced.results.length) {
    // 容差：部分失败已由各读取方法回退 mock / 诚实空态，这里仅打印便于联调排障
    console.warn(`[web-console] preloadRealData：${okCount}/${raced.results.length} 分组成功，失败分组已回退`);
  }
  return okCount === raced.results.length;
}

/** 设备清单 + 逐设备点位（先设备后点位，两段串行；组内各自容错）。 */
async function fetchDevicesAndPoints(): Promise<void> {
  await fetchDevices();
  await fetchAllPoints();
}

/**
 * 低频轮询网关信息（real 模式专用；OverviewPage 的 1s tick 每 5 拍触发一次）。
 *
 * 实时遥测帧（`/api/stream` 的 `LiveTelemetry`）只含逐点数值、不含累计计数器
 * （`totalForwardedRecords` 等），因此网关级统计由本方法按 5s 节奏刷新。
 */
export async function refreshOverview(): Promise<void> {
  if (API_MODE !== 'real') {
    return;
  }
  await fetchOverview();
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

/**
 * 生效告警清单（含处置状态覆盖）。
 *
 * **诚实空态（D-01 修复项）**：real 模式恒取 `GET /api/alerts` 的结果
 * （后端无告警引擎 → 空数组），**绝不回退 mock 告警**、也不从审计行伪造告警；
 * 原因见 `realCache.notices.alerts`。
 */
function effectiveAlarms(): AlarmRecord[] {
  const base = realCache.alarms;
  return base.map((a) => {
    const o = overlay.alarmStates.get(a.id);
    if (!o) {
      return a;
    }
    return { ...a, state: o.state, stateLabel: ALARM_STATE_LABELS[o.state] ?? a.stateLabel, note: o.note, ackedBy: o.ackedBy };
  });
}

/** 生效审计清单（本地覆盖层在前 + `GET /api/audit` 真实条目）。 */
function effectiveAudit(): AuditEntry[] {
  const base =
    realCache.events.length > 0
      ? realCache.events
      : mockRepo.queryAudit({ actorType: '', action: '', entityType: '', result: '', page: 1, pageSize: 100000 }).items;
  return [...overlay.localAudit, ...base];
}

/** 生效转发规则清单（real 模式取 `GET /api/rules`，后端无规则引擎 → 诚实空态）。 */
function effectiveRules(): RuleRecord[] {
  return realCache.rules;
}

/**
 * 生效授权状态：以 `GET /api/license/status` 的真实快照覆盖 mock 快照的
 * 「有真实数据源」字段；纯展示字段（校验档位 / 心跳时间 / 能力矩阵）无后端
 * 数据源，沿用 mock 契约值并**不做任何真实性暗示**。
 */
function effectiveLicense(degradedView: boolean): MockLicense {
  const base = degradedView ? mockRepo.getLicenseDegraded() : mockRepo.getLicense();
  const real = realCache.license;
  if (!real) {
    return base;
  }
  return {
    ...base,
    status: mapLicenseStatus(real.status),
    tierId: real.tier || base.tierId,
    tierName: LICENSE_TIER_LABELS[real.tier] ?? (real.tier || base.tierName),
    remainingDays: pickCountFromText(real.remainingDays),
    remainingText: licenseRemainingText(real),
    validUntil: real.validUntil ? formatEpochText(real.validUntil) : real.status === 'unlicensed' ? '未授权' : base.validUntil,
    degradeReason: real.degradeReason || (real.status === 'degraded' ? '授权降级（后端未提供原因）' : ''),
    onExpireText: real.northForwardAllowed
      ? '授权有效：北向转发正常放行'
      : '北向转发已停用，本地采集继续（恢复路径：完成授权激活）',
  };
}

/** `GET /api/license/status` 状态字面量 → 前端 `LicenseStatus` 域。 */
function mapLicenseStatus(status: string): MockLicense['status'] {
  switch (status) {
    case 'active':
      return 'active';
    case 'trial':
      return 'trial';
    case 'grace':
    case 'degraded':
      return 'grace';
    default:
      // unlicensed / 未知字面量：fail-closed 展示为「已停用」
      return 'stopped';
  }
}

/** 档位 id → 中文名（未知档位原样透传）。 */
const LICENSE_TIER_LABELS: Readonly<Record<string, string>> = {
  free: '免费版',
  trial: '试用版',
  standard: '标准版',
  pro: '专业版',
};

/** 剩余天数字符串 → 小值计数（非法 / 缺失 → 0，绝不 parseFloat 大数）。 */
function pickCountFromText(text: string): number {
  const trimmed = text.trim();
  if (!/^\d{1,6}$/.test(trimmed)) {
    return 0;
  }
  return Number(trimmed);
}

/** 剩余时长文本（优先剩余秒数；缺失退回天数；都缺失给「—」）。 */
function licenseRemainingText(real: LicenseStatusSnapshot): string {
  const days = pickCountFromText(real.remainingDays);
  if (days > 0) {
    return `${days} 天`;
  }
  const secs = pickCountFromText(real.remainingSecs);
  if (secs > 0) {
    const d = Math.floor(secs / 86_400);
    const h = Math.floor((secs % 86_400) / 3_600);
    return d > 0 ? `${d} 天 ${h} 小时` : `${h} 小时`;
  }
  return real.status === 'unlicensed' ? '未授权' : '—';
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

    // ---------- 转发规则（读；`GET /api/rules` 诚实空态） ----------
    allRules(): RuleRecord[] {
      return effectiveRules();
    },

    // ---------- 告警（读 + 处置覆盖；`GET /api/alerts` 诚实空态） ----------
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

    // ---------- 授权状态（读；真实快照覆盖 `GET /api/license/status`） ----------
    getLicense(): MockLicense {
      return effectiveLicense(false);
    },

    getLicenseDegraded(): MockLicense {
      return effectiveLicense(true);
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
// 动作型端点（actions）：真实写 / 探测能力
//   POST /api/devices/test · POST /api/points/import · GET /api/points/export
//   GET /api/forwarders · POST /api/forwarders/{id}/test · GET /api/license/status
//   POST /api/forwarders（501）· PUT /api/alerts/rules（501）
// ===========================================================================

/** 结构化探测结果（`POST /api/devices/test` / `POST /api/forwarders/{id}/test`）。 */
export interface ProbeResult {
  /** 探测是否成功 */
  ok: boolean;
  /** 结构化失败类型（unreachable / timeout / protocol_error / no_endpoint / invalid_endpoint / unsupported_protocol / not_found / …） */
  errorKind: string;
  /** 后端原始原因（人读） */
  reason: string;
  /** 耗时（毫秒，字符串透传；失败时可能为空串） */
  elapsedMs: string;
  /** 面向用户的可解释提示（原因 + 恢复路径），可直接展示 */
  message: string;
}

/** 设备探测入参。 */
export interface DeviceTestInput {
  /** 已登记设备：按配置内首条点位的端点 + 首个 4xxxx/3xxxx 寄存器探测 */
  deviceId?: string;
  /** 登记前探测：协议（仅 modbus-tcp / modbus-rtu 支持全探测） */
  protocol?: string;
  /** 登记前探测：端点 `host[:port]` */
  address?: string;
  /** 探针寄存器（缺省 `40001`） */
  register?: string;
  /** 从站号（字符串 / 数字均可，透传） */
  slave?: string | number;
  /** 超时（毫秒，后端钳制 100..=10000） */
  timeoutMs?: string | number;
}

/** 点表导入结果（含逐行校验错误；失败零落盘）。 */
export interface ImportResult {
  ok: boolean;
  /** 成功导入行数（字符串透传） */
  imported: string;
  /** 被覆盖替换的旧行数（字符串透传） */
  replaced: string;
  /** 受影响的设备 id 列表 */
  devices: string[];
  /** 新配置版本（字符串，大数红线） */
  configVersion: string;
  /** 行级错误（`{line, reason, allowed}`；`line` 为文件内 1 基物理行号字符串） */
  errors: ImportRowError[];
  /** 面向用户的可解释提示（原因 + 恢复路径） */
  message: string;
}

/** 导入行级错误（后端「行号 + 原因 + 允许值」硬契约）。 */
export interface ImportRowError {
  /** 物理行号（表头 = 1；0 = 请求级错误）；字符串透传 */
  line: string;
  /** 失败原因 */
  reason: string;
  /** 允许值说明 */
  allowed: string;
}

/** 点表导出结果。 */
export interface ExportResult {
  ok: boolean;
  /** CSV 原文（表头 `device_id,point_id,protocol,address,frequency_ms`，可直接当导入模板） */
  csv: string;
  /** 建议文件名 */
  fileName: string;
  /** 数据行数（不含表头） */
  rowCount: number;
  message: string;
}

/** 动作型端点 API（`repo.actions.*`）。 */
export interface ActionApi {
  /** 设备连通性探测（结构化失败，绝不 500）。 */
  testDevice(input: DeviceTestInput): Promise<ProbeResult>;
  /** 点表 CSV 批量导入（坏行整批拒绝，逐行返回 `{line, reason, allowed}`）。 */
  importPoints(input: { csv: string; deviceId?: string; replace?: boolean }): Promise<ImportResult>;
  /** 点表 CSV 导出（`?device_id=` 过滤；导出即可当导入模板）。 */
  exportPoints(deviceId?: string): Promise<ExportResult>;
  /** 导出并触发浏览器下载（导出端点 + Blob 落盘）。 */
  downloadPointsCsv(deviceId?: string): Promise<ExportResult>;
  /** 北向出口列表（`GET /api/forwarders`；`id` = 出口名）。 */
  listForwarders(): Promise<ForwarderRecord[]>;
  /** 出口 TCP 可达性探测（`POST /api/forwarders/{id}/test`，未知出口 404）。 */
  testForwarder(id: string, timeoutMs?: string | number): Promise<ProbeResult>;
  /** 授权状态原始快照（全字段字符串；runtime 未装配 → `unlicensed`）。 */
  licenseStatus(): Promise<LicenseStatusSnapshot>;
  /** 出口登记（后端诚实 501；返回可解释提示，不静默吞错）。 */
  createForwarder(input: { name: string; broker: string; actor: string }): Promise<{ ok: boolean; message: string }>;
  /** 告警规则写（后端诚实 501；返回可解释提示）。 */
  saveAlarmRules(actor: string): Promise<{ ok: boolean; message: string }>;
  /** 各数据源的「不可得原因」（诚实空态 / 501 / 503 / 网络失败；空串 = 正常）。 */
  notices(): Record<NoticeKey, string>;
}

/** 从后端响应构造探测结果（`ok` 由调用方按语义给出）。 */
function probeFromResponse(raw: Record<string, unknown>, okFallback: boolean): ProbeResult {
  const ok = typeof raw.ok === 'boolean' ? raw.ok : okFallback;
  const errorKind = pickStr(raw, 'error_kind', ok ? '' : 'error');
  const reason = pickStr(raw, 'reason', ok ? pickStr(raw, 'detail', '') : '');
  const elapsedMs = pickStr(raw, 'elapsed_ms', '');
  return {
    ok,
    errorKind,
    reason,
    elapsedMs,
    message: ok
      ? `探测成功${elapsedMs ? `（${elapsedMs} ms）` : ''}`
      : `${PROBE_ERROR_HINTS[errorKind] ?? `探测失败（${errorKind || '未知原因'}）`}${reason ? `：${reason}` : ''}`,
  };
}

/** 探测失败类型 → 可解释提示（含恢复路径）。 */
const PROBE_ERROR_HINTS: Readonly<Record<string, string>> = {
  unreachable: '目标不可达（TCP 建连被拒 / 网络不通）。恢复路径：确认设备 IP 与端口、网线及防火墙策略后重试',
  timeout: '探测超时。恢复路径：增大超时或排查目标设备响应',
  protocol_error: '协议层错误（设备已连上但应答非法）。恢复路径：核对从站号 / 寄存器地址与设备协议配置',
  no_endpoint: '无可用探测端点。恢复路径：先为该设备登记点位（或在表单里显式填写协议与地址）再探测',
  invalid_endpoint: '端点无法解析。恢复路径：地址须为 host:port 或可解析主机名',
  unsupported_protocol:
    '该协议暂不支持结构化探测（后端仅实现 modbus-tcp / modbus-rtu）。恢复路径：改用设备自带的协议工具验证，或改登记为 modbus 协议后再探测',
  invalid_target: '出口目标地址无法解析。恢复路径：broker 地址须为 scheme://host[:port]',
  not_found: '目标不存在（后端 404）。恢复路径：刷新列表后重试',
};

/** 由结构化失败响应体 / ApiError 构造探测结果。 */
function probeFailure(cause: unknown): ProbeResult {
  if (cause instanceof ApiError) {
    const detail = extractErrorMessage(cause.body);
    const kind = cause.status === 404 ? 'not_found' : cause.status === 403 ? 'denied' : cause.status === 401 ? 'unauthorized' : `http_${cause.status}`;
    return {
      ok: false,
      errorKind: kind,
      reason: detail.replace(/^——/, ''),
      elapsedMs: '',
      message: describeFailure(cause, '探测'),
    };
  }
  return {
    ok: false,
    errorKind: 'error',
    reason: cause instanceof Error ? cause.message : String(cause),
    elapsedMs: '',
    message: `探测失败：${cause instanceof Error ? cause.message : String(cause)}`,
  };
}

/** 触发浏览器下载（Blob + 临时 `<a download>`；零依赖）。 */
function triggerDownload(fileName: string, content: string, mime: string): void {
  const blob = new Blob([content], { type: `${mime};charset=utf-8` });
  const url = URL.createObjectURL(blob);
  const anchor = document.createElement('a');
  anchor.href = url;
  anchor.download = fileName;
  anchor.style.display = 'none';
  document.body.appendChild(anchor);
  anchor.click();
  document.body.removeChild(anchor);
  URL.revokeObjectURL(url);
}

/** 点表 CSV 导出实现（real：`GET /api/points/export`；mock：本地演示 CSV）。 */
async function exportPointsImpl(deviceId?: string): Promise<ExportResult> {
  if (API_MODE !== 'real') {
    const header = 'device_id,point_id,protocol,address,frequency_ms';
    const rows = mockRepo.allPoints().filter((p) => !deviceId || p.deviceId === deviceId);
    const csv = [header, ...rows.map((p) => `${p.deviceId},${p.name},modbus-tcp,${p.address},1000`)].join('\n');
    return {
      ok: true,
      csv,
      fileName: `points${deviceId ? `-${deviceId}` : ''}.csv`,
      rowCount: rows.length,
      message: `mock 模式：已生成 ${rows.length} 行点表 CSV。`,
    };
  }
  const qs = deviceId ? `?device_id=${encodeURIComponent(deviceId)}` : '';
  try {
    const csv = await apiRequestText(`/api/points/export${qs}`);
    const rowCount = Math.max(0, csv.split(/\r?\n/).filter((l) => l.trim() !== '').length - 1);
    return {
      ok: true,
      csv,
      fileName: `points${deviceId ? `-${deviceId}` : ''}.csv`,
      rowCount,
      message: `已导出 ${rowCount} 行点表（可直接当导入模板）。`,
    };
  } catch (cause) {
    realCache.notices.points = describeFailure(cause, '点表导出');
    return { ok: false, csv: '', fileName: '', rowCount: 0, message: describeFailure(cause, '点表导出') };
  }
}

/** 动作型端点实现（real → 真实端点；mock → 模拟成功 + 本地审计，行为零回归）。 */
function buildActions(): ActionApi {
  return {
    async testDevice(input: DeviceTestInput): Promise<ProbeResult> {
      if (API_MODE !== 'real') {
        pushLocalAudit({
          actor: DEFAULT_ACTOR,
          actorType: 'human',
          action: '设备连通性探测',
          entityType: 'device',
          entityLabel: '设备',
          entityId: input.deviceId ?? input.address ?? '—',
          detail: `mock 模式：模拟探测成功（${input.protocol ?? '按设备配置'}）`,
          result: 'success',
        });
        return { ok: true, errorKind: '', reason: '', elapsedMs: '12', message: 'mock 模式：模拟探测成功（12 ms）。' };
      }
      try {
        const body: Record<string, unknown> = {};
        if (input.deviceId) {
          body.device_id = input.deviceId;
        }
        if (input.protocol) {
          body.protocol = input.protocol;
        }
        if (input.address) {
          body.address = input.address;
        }
        if (input.register) {
          body.register = input.register;
        }
        if (input.slave !== undefined && input.slave !== '') {
          body.slave = input.slave;
        }
        if (input.timeoutMs !== undefined && input.timeoutMs !== '') {
          body.timeout_ms = input.timeoutMs;
        }
        const raw = await apiRequest<Record<string, unknown>>('/api/devices/test', {
          method: 'POST',
          body: JSON.stringify(body),
        });
        return probeFromResponse(asRecord(raw), false);
      } catch (cause) {
        return probeFailure(cause);
      }
    },

    async importPoints(input: { csv: string; deviceId?: string; replace?: boolean }): Promise<ImportResult> {
      if (API_MODE !== 'real') {
        // mock 模式：按 CSV 行数模拟导入（保留既有前端流程语义，零回归）
        const dataRows = input.csv
          .split(/\r?\n/)
          .slice(1)
          .filter((line) => line.trim() !== '').length;
        pushLocalAudit({
          actor: DEFAULT_ACTOR,
          actorType: 'human',
          action: '导入点表',
          entityType: 'point',
          entityLabel: '点位',
          entityId: input.deviceId ?? '—',
          detail: `mock 模式：模拟导入 ${dataRows} 行`,
          result: 'success',
        });
        return {
          ok: true,
          imported: String(dataRows),
          replaced: '0',
          devices: input.deviceId ? [input.deviceId] : [],
          configVersion: '',
          errors: [],
          message: `mock 模式：模拟导入 ${dataRows} 行。`,
        };
      }
      const qs = new URLSearchParams();
      if (input.deviceId) {
        qs.set('device_id', input.deviceId);
      }
      if (input.replace) {
        qs.set('replace', 'true');
      }
      const query = qs.toString();
      try {
        const raw = await apiRequest<Record<string, unknown>>(`/api/points/import${query ? `?${query}` : ''}`, {
          method: 'POST',
          body: JSON.stringify({ csv: input.csv, device_id: input.deviceId ?? '', replace: Boolean(input.replace) }),
        });
        // 导入成功 → 立即使缓存可见（后端已热生效，前端缓存同步刷新）
        void fetchAllPoints().then(() => bumpCacheVersion());
        const imported = pickStr(raw, 'imported', '0');
        return {
          ok: true,
          imported,
          replaced: pickStr(raw, 'replaced', '0'),
          devices: Array.isArray(raw.devices) ? raw.devices.map((d) => String(d)) : [],
          configVersion: pickStr(raw, 'config_version', ''),
          errors: [],
          message: `导入成功：${imported} 行已落盘并热生效。`,
        };
      } catch (cause) {
        const errors = extractImportErrors(cause);
        if (errors.length > 0) {
          return {
            ok: false,
            imported: '0',
            replaced: '0',
            devices: [],
            configVersion: '',
            errors,
            message: `导入被整批拒绝（零落盘）：${errors.length} 行不合法，请按「行号 + 原因 + 允许值」修正后重试。`,
          };
        }
        return {
          ok: false,
          imported: '0',
          replaced: '0',
          devices: [],
          configVersion: '',
          errors: [],
          message: describeFailure(cause, '点表导入'),
        };
      }
    },

    exportPoints(deviceId?: string): Promise<ExportResult> {
      return exportPointsImpl(deviceId);
    },

    async downloadPointsCsv(deviceId?: string): Promise<ExportResult> {
      const result = await exportPointsImpl(deviceId);
      if (result.ok) {
        triggerDownload(result.fileName, result.csv, 'text/csv');
      }
      return result;
    },

    async listForwarders(): Promise<ForwarderRecord[]> {
      if (API_MODE !== 'real') {
        return mockRepo.allForwarders();
      }
      try {
        const raw = await apiRequest<unknown[]>('/api/forwarders');
        return Array.isArray(raw) ? raw.map((row, i) => mapForwarder(asRecord(row), i)) : [];
      } catch (cause) {
        realCache.notices.forwarders = describeFailure(cause, '北向出口');
        return [];
      }
    },

    async testForwarder(id: string, timeoutMs?: string | number): Promise<ProbeResult> {
      if (API_MODE !== 'real') {
        pushLocalAudit({
          actor: DEFAULT_ACTOR,
          actorType: 'human',
          action: '出口连通性探测',
          entityType: 'forwarder',
          entityLabel: '北向出口',
          entityId: id,
          detail: 'mock 模式：模拟出口探测成功',
          result: 'success',
        });
        return { ok: true, errorKind: '', reason: '', elapsedMs: '8', message: 'mock 模式：模拟出口探测成功（8 ms）。' };
      }
      const qs = timeoutMs !== undefined && timeoutMs !== '' ? `?timeout_ms=${encodeURIComponent(String(timeoutMs))}` : '';
      try {
        const raw = await apiRequest<Record<string, unknown>>(`/api/forwarders/${encodeURIComponent(id)}/test${qs}`, {
          method: 'POST',
        });
        const result = probeFromResponse(asRecord(raw), false);
        // 诚实限制：后端仅做 TCP 建连，不做 TLS 握手 / MQTT CONNACK
        return result.ok
          ? { ...result, message: `${result.message}（仅 TCP 建连探测：未校验 TLS 握手与 MQTT CONNACK）` }
          : result;
      } catch (cause) {
        return probeFailure(cause);
      }
    },

    async licenseStatus(): Promise<LicenseStatusSnapshot> {
      if (API_MODE !== 'real') {
        const mock = mockRepo.getLicense();
        return {
          status: mock.status === 'active' ? 'active' : mock.status === 'trial' ? 'trial' : mock.status === 'grace' ? 'grace' : 'unlicensed',
          tier: mock.tierId,
          validUntil: '',
          remainingSecs: '',
          remainingDays: String(mock.remainingDays),
          degradeReason: mock.degradeReason,
          northForwardAllowed: mock.status === 'active' || mock.status === 'trial',
          note: 'mock 模式：授权状态为演示数据',
        };
      }
      if (realCache.license) {
        return realCache.license;
      }
      await fetchLicense();
      return (
        realCache.license ?? {
          status: 'unlicensed',
          tier: '',
          validUntil: '',
          remainingSecs: '',
          remainingDays: '',
          degradeReason: '',
          northForwardAllowed: false,
          note: realCache.notices.license || '授权状态不可得（runtime 未装配 / 请求失败）',
        }
      );
    },

    async createForwarder(input: { name: string; broker: string; actor: string }): Promise<{ ok: boolean; message: string }> {
      if (API_MODE !== 'real') {
        pushLocalAudit({
          actor: input.actor,
          actorType: 'human',
          action: '新增北向出口',
          entityType: 'forwarder',
          entityLabel: '北向出口',
          entityId: input.name,
          detail: `mock 模式：模拟新增出口 ${input.name}`,
          result: 'success',
        });
        return { ok: true, message: '出口已新增（mock 模拟）。' };
      }
      try {
        await apiRequest<unknown>('/api/forwarders', {
          method: 'POST',
          body: JSON.stringify({ name: input.name, broker: input.broker }),
        });
        void fetchForwarders().then(() => bumpCacheVersion());
        return { ok: true, message: `出口 ${input.name} 已登记。` };
      } catch (cause) {
        return { ok: false, message: describeFailure(cause, '北向出口登记') };
      }
    },

    async saveAlarmRules(actor: string): Promise<{ ok: boolean; message: string }> {
      if (API_MODE !== 'real') {
        pushLocalAudit({
          actor,
          actorType: 'human',
          action: '保存告警规则',
          entityType: 'alarm',
          entityLabel: '告警规则',
          entityId: 'alarm-rules',
          detail: 'mock 模式：模拟保存告警规则',
          result: 'success',
        });
        return { ok: true, message: '告警规则已保存（mock 模拟）。' };
      }
      try {
        await apiRequest<unknown>('/api/alerts/rules', { method: 'PUT', body: JSON.stringify({ rules: [] }) });
        return { ok: true, message: '告警规则已保存。' };
      } catch (cause) {
        return { ok: false, message: describeFailure(cause, '告警规则保存') };
      }
    },

    notices(): Record<NoticeKey, string> {
      return { ...realCache.notices };
    },
  };
}

/** 从 400 响应体提取逐行导入错误（`{line, reason, allowed}`，行号字符串透传）。 */
function extractImportErrors(cause: unknown): ImportRowError[] {
  if (!(cause instanceof ApiError)) {
    return [];
  }
  const body = asRecord(cause.body);
  const rawErrors = Array.isArray(body.errors) ? body.errors : [];
  return rawErrors.map((item) => {
    const row = asRecord(item);
    return {
      // 行号是物理行号（可含 `0` = 请求级错误）：字符串透传，绝不 parseInt。
      line: pickStr(row, 'line', '0'),
      reason: pickStr(row, 'reason', '原因未给出'),
      allowed: pickStr(row, 'allowed', ''),
    };
  });
}

// ===========================================================================
// 统一导出
// ===========================================================================

/** 页面层唯一数据入口：mock 模式 = mock 仓库；real 模式 = 真实缓存 + 覆盖层。 */
const baseRepo: typeof mockRepo = API_MODE === 'real' ? buildRealRepo() : mockRepo;

/**
 * 最终仓库（含 `ops` 运维动作与 `actions` 动作型端点；mock 模式下二者均为模拟实现）。
 */
export const repo: typeof mockRepo & { ops: OpsApi; actions: ActionApi } = {
  ...baseRepo,
  ops: buildOps(),
  actions: buildActions(),
};

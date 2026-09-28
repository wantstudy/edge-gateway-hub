/**
 * @file repo.ts
 * @module web-console/api/repo
 * @description 统一数据层：**只走真实后端**（mock 模式已废除，见 `client.ts`）。
 *
 * ── 定位 ────────────────────────────────────────────────────────────────────
 *  · **对外符号与历史契约兼容**：继续 `export * from './model'` 透传全部类型与
 *    展示常量（`PROTOCOL_OPTIONS` / `DEFAULT_ACTOR` / `type DeviceRecord` …），
 *    页面 `from '@/api/repo'` 的导入路径与符号名不变。
 *  · **读**：启动 / 登录后 `preloadRealData()` 并行拉取
 *    `/api/devices · /api/points（逐设备）· /api/forwarders · /api/overview ·
 *    /api/audit · /api/alerts · /api/rules · /api/license/status` 并缓存。
 *    缓存为空 / 请求失败 = **诚实空态**，失败原因写入 `realCache.notices.*`，
 *    **绝不回退任何演示数据**。
 *  · **写**：一律真实 HTTP（`POST/PUT/DELETE /api/devices`、
 *    `POST/PUT/DELETE /api/points`、`POST /api/points/import` 等），返回结构化
 *    `WriteResult`（`ok:false` 必带真实原因），成功后 `refresh()` 重新拉真实数据
 *    并自增 `dataVersion`。**已删除本地覆盖层（overlay）与本地假写记录**。
 *  · **大数红线**：`totalForwardedRecords` 等大数字段走字符串直通（`pickStr`），
 *    绝不 parseFloat；仅「小值业务计数」经 `pickNum` 安全转换。
 *  · **mock 模式已废除**：仓库不再有 `mockRepo` 分支；`API_MODE` 恒为 `'real'`。
 */
import { ref, type Ref } from 'vue';
import { formatEmbeddedTimestamps, formatNanoTimestampText, formatTimestampText } from '@/utils/time';
import { ApiError, apiRequest, apiRequestText } from './client';
import {
  DEFAULT_ACTOR,
  PROTOCOL_OPTIONS,
  type AccountRecord,
  type AlarmRecord,
  type AlarmState,
  type AuditEntry,
  type DeviceDraft,
  type DeviceGroup,
  type DeviceRecord,
  type Encoding,
  type ForwarderRecord,
  type GatewayInfo,
  type MockLicense,
  type Paged,
  type PermissionRecord,
  type PointDraft,
  type PointRecord,
  type ProtocolType,
  type RoleRecord,
  type RuleRecord,
  type WriteResult,
} from './model';

// 透传 model 的类型 / 常量 / 授权基线（`repo` 由下方局部导出，属 ES 模块规范行为）
export * from './model';

// 再导出模式常量（页面可统一从本模块取用；恒为 'real'）
export { API_MODE } from './client';

// ===========================================================================
// 宽容取值工具（后端字段以 model 形状为契约；缺失 / 类型漂移时回诚实默认值）
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

/**
 * 读 `autostart_guaranteed` 对象：`{value, provenance, hint}`。
 *
 * 后端未上报 / 形状不符 → 回落 `{value:'', provenance:'unknown', hint:''}`：
 * `provenance:'unknown'` 是**诚实值**，页面据此说「不可得」，不冒充 detected。
 */
function readGuarantee(raw: unknown): AutostartGuarantee {
  const rec = asRecord(raw);
  return {
    value: pickStr(rec, 'value', ''),
    provenance: pickStr(rec, 'provenance', 'unknown'),
    hint: pickStr(rec, 'hint', ''),
  };
}

/** 字段是否「后端已上报」（既不缺失、也非 null）。 */
function present(src: Record<string, unknown>, key: string): boolean {
  const v = src[key];
  return v !== undefined && v !== null;
}

/** 两个键名任一已上报（兼容 camelCase / snake_case）。 */
function presentAny(src: Record<string, unknown>, camel: string, snake: string): boolean {
  return present(src, camel) || present(src, snake);
}

/** 取第一个已上报的键（camel 优先），缺省回空串。 */
function pickEither(src: Record<string, unknown>, camel: string, snake: string): string {
  return pickStr(src, camel, pickStr(src, snake, ''));
}

/** 把 unknown 收敛为 Record（数组 / 非对象回空对象）。 */
function asRecord(value: unknown): Record<string, unknown> {
  return value !== null && typeof value === 'object' && !Array.isArray(value)
    ? (value as Record<string, unknown>)
    : {};
}

/** epoch 秒 / 毫秒字符串 → `YYYY-MM-DD HH:mm:ss`。
 *
 * 数字形态统一交给共享展示工具：只接受 10 位秒 / 13 位毫秒；uint64、纳秒串及
 * 其他数字长度统一回空态，绝不把原始时间戳透传到页面。已是人类可读文本时保留。
 */
function formatEpochText(value: string): string {
  const trimmed = value.trim();
  return /^\d+$/.test(trimmed) ? formatTimestampText(trimmed) : trimmed || '—';
}

/**
 * UTC 纳秒字符串 → `YYYY-MM-DD HH:mm:ss`。
 *
 * 大数红线：绝不把纳秒整串转为 Number，只截取 10 位秒段交给共享展示工具。
 */
function formatNsText(value: string): string {
  return formatNanoTimestampText(value);
}

// ===========================================================================
// 真实后端缓存（唯一数据源；空 = 诚实空态）
// ===========================================================================

/** 数据源键（对应一处真实接口）。 */
export type NoticeKey =
  | 'devices'
  | 'points'
  | 'forwarders'
  | 'audit'
  | 'alerts'
  | 'rules'
  | 'license'
  | 'groups'
  | 'roles'
  | 'permissions'
  | 'accounts';

/** 全部数据源键（构造 notices 初值用）。 */
const NOTICE_KEYS: readonly NoticeKey[] = [
  'devices',
  'points',
  'forwarders',
  'audit',
  'alerts',
  'rules',
  'license',
  'groups',
  'roles',
  'permissions',
  'accounts',
];

/** 真实后端缓存（preload / 页面按需填充；字段形状 = model 前端镜像）。 */
interface RealCache {
  /** 是否已完成一次 preload（无论成败） */
  loaded: boolean;
  devices: DeviceRecord[];
  points: PointRecord[];
  outlets: ForwarderRecord[];
  status: GatewayInfo | null;
  /** 审计条目（`GET /api/audit`——有限 JSON；**不是** `/api/events` 无限 SSE 流） */
  events: AuditEntry[];
  /** 告警（`GET /api/alerts`；后端无告警引擎 → 诚实空态） */
  alarms: AlarmRecord[];
  /** 转发规则（`GET /api/rules`；后端无规则引擎 → 诚实空态） */
  rules: RuleRecord[];
  /** 授权状态（`GET /api/license/status` 原始快照；全字段字符串透传） */
  license: LicenseStatusSnapshot | null;
  /** 设备分组（`GET /api/groups`；后端未落地 → 诚实空态） */
  groups: DeviceGroup[];
  /** 角色（`GET /api/roles`；后端未落地 → 诚实空态） */
  roles: RoleRecord[];
  /** 权限清单（`GET /api/permissions`；后端未落地 → 诚实空态） */
  permissions: PermissionRecord[];
  /** 账号（`GET /api/accounts`；后端未落地 → 诚实空态） */
  accounts: AccountRecord[];
  /** 各数据源的「不可得原因」（诚实空态 / 404 / 501 / 503 / 网络失败；空串 = 正常） */
  notices: Record<NoticeKey, string>;
}

/** 缓存初始值（全部为空 → 各读取方法返回诚实空态）。 */
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
  groups: [],
  roles: [],
  permissions: [],
  accounts: [],
  notices: Object.fromEntries(NOTICE_KEYS.map((k) => [k, ''])) as Record<NoticeKey, string>,
};

/** 测试专用：暴露缓存引用以便单元测试注入事件（不参与任何生产逻辑；tree-shake 友好）。 */
export const __testCache = realCache;

/** 缓存版本号（`dataVersion` 的源）。 */
let cacheVersion = 0;

/** 读取当前缓存版本号（配合 `dataVersion` 使用）。 */
export function getCacheVersion(): number {
  return cacheVersion;
}

/**
 * 缓存版本（响应式）：preload / 刷新 / 写操作完成后自增。
 *
 * 页面可 `watch(dataVersion, reload)` 实现「数据变更后自动刷新」。
 */
export const dataVersion: Ref<number> = ref(0);

/** 自增缓存版本。 */
function bumpCacheVersion(): void {
  cacheVersion += 1;
  dataVersion.value = cacheVersion;
}

/** 统一失败描述（**不静默吞错**）：给出「原因 + 恢复路径」。 */
function describeFailure(cause: unknown, subject: string): string {
  if (cause instanceof ApiError) {
    const detail = extractErrorMessage(cause.body);
    switch (cause.status) {
      case 501:
        return `${subject}：该功能暂未开放（501）${detail}。请升级网关后使用。`;
      case 503:
        return `${subject}：网关服务暂不可用（503）${detail}。请稍后重试，若持续出现请检查网关运行状态。`;
      case 404:
        return `${subject}：网关暂不支持该功能（404）${detail}。请升级网关后重试。`;
      case 403:
        return `${subject}：权限不足（403）${detail}。恢复路径：使用具备相应权限的账号登录。`;
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

/**
 * 从后端错误体取人读消息（`{message}` / `{reason}` / `{error}` 优先，其次原文）。
 *
 * 同时透传后端 `hint` 恢复路径：存在非空 `hint` 时追加 ` 恢复路径：<hint>`，
 * 把后端已算好的「怎么修」一并给到用户（如预绑定冲突的换机工单 / 解绑旧机）。
 * `hint` 为空串时视为不存在；无任何主消息时仅回 `——恢复路径：<hint>`。
 */
export function extractErrorMessage(body: unknown): string {
  const rec = asRecord(body);
  const hint = pickStr(rec, 'hint', '');
  const suffix = hint ? ` 恢复路径：${hint}` : '';
  const message = pickStr(rec, 'message', '');
  if (message) {
    return `——${message}${suffix}`;
  }
  const reason = pickStr(rec, 'reason', '');
  if (reason) {
    return `——${reason}${suffix}`;
  }
  const error = pickStr(rec, 'error', '');
  if (error) {
    return `——${error}${suffix}`;
  }
  if (hint) {
    return `——恢复路径：${hint}`;
  }
  return typeof body === 'string' && body ? `——${body}` : '';
}

/** 从 ApiError 错误体提取字段级校验原因（writeapi validation_error：{error,field,reason}）。 */
function validationReason(cause: unknown): string {
  const body = (cause as { body?: unknown }).body;
  if (body && typeof body === 'object') {
    const rec = asRecord(body);
    const field = typeof rec['field'] === 'string' ? rec['field'] : '';
    const reason = typeof rec['reason'] === 'string' ? rec['reason'] : '';
    if (field || reason) {
      return `${field ? `${field} —— ` : ''}${reason || '字段校验失败'}`;
    }
  }
  return '';
}

/** 构造失败结果（**绝不假装成功**）。 */
function fail<T = void>(message: string): WriteResult<T> {
  return { ok: false, message };
}

/**
 * 更新执行结果 → 面向用户的人读消息。
 *
 * 诚实红线：`accepted:true` 只代表后端**已受理**（下载 + 验签通过），
 * `applied` 当前恒为 false —— 版本替换由宿主安装器在**重启时**完成。
 * 因此本函数**绝不**出现「成功」字样；`supported:false` / `accepted:false` 时
 * 把后端 `reason` 原文并入消息（那是面向用户的真实原因，不自行编造覆盖）。
 */
function describeApplyResult(result: UpdateApplyResult): string {
  if (!result.supported) {
    return `更新未执行：${result.reason || '后端尚未具备执行更新的能力。'}`;
  }
  if (!result.accepted) {
    return `更新未受理：${result.reason || '后端拒绝了本次更新请求。'}`;
  }
  const target = result.targetVersion || '新版本';
  if (result.applied) {
    return `更新已应用：${result.currentVersion || '当前版本'} → ${target}。`;
  }
  return (
    `更新包已受理：后端已完成下载并通过签名校验（目标 ${target}）；` +
    '重启网关后由宿主安装器完成替换，替换结果须以重启后的实际版本号为准（本次尚未应用）。'
  );
}

/** 后端角色（`dev`/`system` 等）→ 设备 id 生成用的 ASCII slug。 */
function slug(value: string, fallback: string): string {
  const ascii = value
    .trim()
    .toLowerCase()
    .replace(/[^a-z0-9]+/g, '-')
    .replace(/^-+|-+$/g, '');
  return ascii || fallback;
}

// ---------------------------------------------------------------------------
// 后端响应 → 前端镜像（宽容映射；缺失字段一律诚实默认，绝不臆造）
// ---------------------------------------------------------------------------

/**
 * `/api/devices` 行 → DeviceRecord。
 *
 * 兼容两种字段命名（camelCase 前端契约 / snake_case 后端 wire），并消费本次新增
 * 字段：`status`、`last_sample_at`、`success_rate`、`fail_streak`、`point_count`、
 * `group_id`。**缺字段即诚实留空 / 最保守取值**，并把「后端未上报」写入 `unknownText`。
 */
function mapDevice(raw: Record<string, unknown>, idx: number): DeviceRecord {
  const protocol = pickStr(raw, 'protocol', 'modbus-tcp') as ProtocolType;

  // 状态三态：后端未上报 → 按最保守 'offline' 展示，并标注未知。
  const hasStatus = typeof raw['status'] === 'string' && raw['status'].trim() !== '';
  const statusRaw = hasStatus ? raw['status'] as string : 'offline';
  const status = (['online', 'offline', 'error'].includes(statusRaw) ? statusRaw : 'offline') as DeviceRecord['status'];

  // 新增字段缺失即列为「后端未上报」（不伪造 successRate / failStreak / pointCount）。
  const missing: string[] = [];
  if (!hasStatus) {
    missing.push('status');
  }
  if (!presentAny(raw, 'lastSampleAt', 'last_sample_at')) {
    missing.push('last_sample_at');
  }
  if (!presentAny(raw, 'successRate', 'success_rate')) {
    missing.push('success_rate');
  }
  if (!presentAny(raw, 'failStreak', 'fail_streak')) {
    missing.push('fail_streak');
  }
  if (!presentAny(raw, 'pointCount', 'point_count')) {
    missing.push('point_count');
  }

  const lastSampleRaw = pickEither(raw, 'lastSampleAt', 'last_sample_at');
  const offlineRaw = pickEither(raw, 'offlineText', 'offline_text');

  return {
    id: pickStr(raw, 'id', `dev-real-${idx}`),
    name: pickStr(raw, 'name', pickStr(raw, 'id', `设备-${idx + 1}`)),
    protocol,
    protocolLabel: pickStr(raw, 'protocolLabel', PROTOCOL_OPTIONS.find((p) => p.value === protocol)?.label ?? protocol),
    // 后端当前不上报连接摘要：诚实留占位符，不臆造端点。
    connectionSummary: pickStr(raw, 'connectionSummary', pickStr(raw, 'connection_summary', '—')),
    status,
    unknownText: missing.length > 0 ? `后端未上报：${missing.join(' / ')}` : '',
    intervalMs: pickNum(raw, 'intervalMs', pickNum(raw, 'poll_interval_ms', 0)),
    timeoutMs: pickNum(raw, 'timeoutMs', 0),
    retryTimes: pickNum(raw, 'retryTimes', 0),
    pointCount: pickNum(raw, 'pointCount', pickNum(raw, 'point_count', 0)),
    groupId: pickEither(raw, 'groupId', 'group_id'),
    lastSampleAt: lastSampleRaw ? formatEpochText(lastSampleRaw) : '—',
    successRate: pickNum(raw, 'successRate', pickNum(raw, 'success_rate', 0)),
    failStreak: pickNum(raw, 'failStreak', pickNum(raw, 'fail_streak', 0)),
    offlineText: hasStatus ? offlineRaw : '状态未知（后端未上报）',
    createdAt: formatEpochText(pickEither(raw, 'createdAt', 'created_at')),
  };
}

/**
 * `/api/points` 行 → PointRecord。
 *
 * 后端行形状（蛇形命名）：`{device_id, point_id, protocol, address, frequency_ms,
 * name?, data_type?, byte_order?, unit?, deadband?, target_key?, point_type?,
 * push_enabled?, formula?}`。`id` 取 `point_id`（无则回退 `id`）——这是实时遥测流
 * （`/api/stream`）逐点 key（`${device_id}/${point_id}`）的对齐锚点，**不可改名**。
 */
function mapPoint(raw: Record<string, unknown>, idx: number): PointRecord {
  const value = raw['value'] === null || raw['value'] === undefined ? null : pickNum(raw, 'value', 0);
  const hasQuality = typeof raw['quality'] === 'string' && raw['quality'].trim() !== '';
  // 质量：后端 /api/points 不上报实时质量 → 有值才可能 Good，否则保守 Uncertain（不冒充健康）。
  const quality = (hasQuality ? raw['quality'] as string : value === null ? 'Uncertain' : 'Good') as PointRecord['quality'];
  const pointId = pickEither(raw, 'pointId', 'point_id');
  const address = pickStr(raw, 'address', '');
  const pointType = pickStr(raw, 'pointType', pickStr(raw, 'point_type', 'physical')) as PointRecord['pointType'];
  return {
    id: pickEither(raw, 'id', 'point_id') || `pt-real-${idx}`,
    deviceId: pickEither(raw, 'deviceId', 'device_id'),
    deviceName: pickStr(raw, 'deviceName', pickStr(raw, 'device_name', '—')),
    name: pickStr(raw, 'name', pointId || `点位-${idx + 1}`),
    pointType,
    address: address || '—',
    dataType: pickStr(raw, 'dataType', pickStr(raw, 'data_type', '—')),
    byteOrder: pickStr(raw, 'byteOrder', pickStr(raw, 'byte_order', '—')) as PointRecord['byteOrder'],
    unit: pickEither(raw, 'unit', 'unit'),
    deadband: pickNum(raw, 'deadband', 0),
    targetKey: pickEither(raw, 'targetKey', 'target_key'),
    // 推送开关缺省「开」（需求 6：点位默认参与北向推送）；后端显式回 false 仍为关。
    pushEnabled: pickBool(raw, 'pushEnabled', pickBool(raw, 'push_enabled', true)),
    quality,
    value,
    valueText: pickStr(raw, 'valueText', value === null ? '——' : String(value)),
    formula: typeof raw['formula'] === 'string' ? raw['formula'] : null,
    updatedAt: formatEpochText(pickEither(raw, 'updatedAt', 'updated_at')),
    stale: pickBool(raw, 'stale', false),
  };
}

/** `/api/forwarders` 行 → ForwarderRecord（北向出口）。 */
function mapForwarder(raw: Record<string, unknown>, idx: number): ForwarderRecord {
  const qosRaw = pickNum(raw, 'qos', 1);
  const encoding = pickStr(raw, 'encoding', 'protobuf');
  return {
    id: pickStr(raw, 'id', `fwd-real-${idx}`),
    name: pickStr(raw, 'name', `出口-${idx + 1}`),
    // 后端 /api/forwarders 契约：target / qos(数字字符串) / tls(bool) / topic_prefix。
    brokerUrl: pickStr(raw, 'target', pickStr(raw, 'brokerUrl', pickStr(raw, 'broker_url', '—'))),
    transportSecurity:
      typeof raw['tls'] === 'boolean' ? (raw['tls'] ? 'TLS' : 'TCP') : pickStr(raw, 'transportSecurity', '—'),
    certStatusText: pickStr(raw, 'certStatusText', '—'),
    clientId: pickStr(raw, 'clientId', pickStr(raw, 'client_id', '')),
    qos: (qosRaw === 0 || qosRaw === 1 || qosRaw === 2 ? qosRaw : 1) as ForwarderRecord['qos'],
    retained: pickBool(raw, 'retained', false),
    topicTemplate: pickStr(raw, 'topicTemplate', pickStr(raw, 'topic_prefix', '')),
    encoding: (encoding === 'json' ? 'json' : 'protobuf') as Encoding,
    status: pickStr(raw, 'status', 'disconnected') as ForwarderRecord['status'],
    connectedForText: pickStr(raw, 'connectedForText', '—'),
    coveredDevices: pickNum(raw, 'coveredDevices', 0),
    recommendedDeviceLimit: pickNum(raw, 'recommendedDeviceLimit', 0),
    lastConsistencyCheckAt: formatEpochText(pickEither(raw, 'lastConsistencyCheckAt', 'last_consistency_check_at')),
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

/** 任意事件行 → AuditEntry（统一映射路径）。 */
function mapEventAudit(raw: Record<string, unknown>, idx: number): AuditEntry {
  return {
    id: pickStr(raw, 'id', `evt-${idx}`),
    ts: formatEpochText(pickStr(raw, 'ts', pickStr(raw, 'time', ''))),
    actor: pickStr(raw, 'actor', 'system'),
    actorType: pickStr(raw, 'actorType', pickStr(raw, 'actor_type', 'system')),
    action: pickStr(raw, 'action', '—'),
    entityType: pickStr(raw, 'entityType', pickStr(raw, 'entity_type', 'system')),
    entityLabel: pickStr(raw, 'entityLabel', pickStr(raw, 'entity_label', '系统')),
    entityId: pickStr(raw, 'entityId', pickStr(raw, 'entity_id', '—')),
    // 审计 detail 是后端自由文本，可能内嵌带 epoch 的标识符（如 `dev-1790381277499`）——
    // 统一把可判定的内嵌时间戳格式化为可读文本，绝不让原始时间戳上屏。
    detail: formatEmbeddedTimestamps(pickStr(raw, 'detail', '—')),
    ip: pickStr(raw, 'ip', ''),
    result: pickStr(raw, 'result', 'success'),
  };
}

/**
 * `/api/audit` 行 → AuditEntry。
 *
 * 后端行形状：`{seq, ts_ns, actor, event, outcome, detail}`（`seq` / `ts_ns` 为字符串
 * 编码的大数，红线要求透传）。这里先做「字段名与量纲归一化」，再交给 `mapEventAudit`。
 */
function mapAuditRow(raw: Record<string, unknown>, idx: number): AuditEntry {
  const event = pickStr(raw, 'event', '');
  const outcome = pickStr(raw, 'outcome', '');
  return mapEventAudit(
    {
      // 大数红线：seq 为链内序号字符串，原样作为主键，绝不 parseInt。
      id: pickStr(raw, 'seq', `audit-${idx}`),
      ts: formatNsText(pickStr(raw, 'ts_ns', pickStr(raw, 'ts', ''))),
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

/** `/api/alerts` 行 → AlarmRecord；不具备告警形状（无 level 且无 title）时返回 null。 */
function mapEventAlarm(raw: Record<string, unknown>, idx: number): AlarmRecord | null {
  const level = pickStr(raw, 'level', '');
  const title = pickStr(raw, 'title', '');
  if (!level && !title) {
    return null;
  }
  const state = pickStr(raw, 'state', 'open');
  return {
    id: pickStr(raw, 'id', `al-${idx}`),
    level: (['critical', 'major', 'minor', 'warning'].includes(level) ? level : 'minor') as AlarmRecord['level'],
    levelLabel: ALARM_LEVEL_LABELS[level] ?? '次要',
    sourceType: pickStr(raw, 'sourceType', pickStr(raw, 'source_type', 'system')),
    // 后端 `/api/alerts` 的权威字段是 `source_label`（`source` 不存在）——逐个核对字段名
    // 后改为在此读取，否则该列恒为「—」（历史上把 note / 未知字段名拼进回退链的同类问题）。
    sourceLabel: pickEither(raw, 'sourceLabel', 'source_label') || '—',
    title: title || '—',
    detail: pickStr(raw, 'detail', ''),
    firstSeenAt: formatEpochText(pickStr(raw, 'firstSeenAt', pickStr(raw, 'first_seen_at', ''))),
    lastSeenAt: formatEpochText(pickStr(raw, 'lastSeenAt', pickStr(raw, 'last_seen_at', ''))),
    count: pickNum(raw, 'count', 1),
    state: (['open', 'acking', 'resolved'].includes(state) ? state : 'open') as AlarmRecord['state'],
    stateLabel: ALARM_STATE_LABELS[state] ?? '待处理',
    ackedBy: pickStr(raw, 'ackedBy', pickStr(raw, 'acked_by', '')),
    note: pickStr(raw, 'note', ''),
  };
}

/** `/api/rules` 行 → RuleRecord（后端当前恒空；保留宽容映射防未来字段漂移）。 */
function mapRule(raw: Record<string, unknown>, idx: number): RuleRecord {
  return {
    id: pickStr(raw, 'id', `rule-${idx}`),
    name: pickStr(raw, 'name', `规则-${idx + 1}`),
    forwarderId: pickEither(raw, 'forwarderId', 'forwarder_id'),
    forwarderName: pickStr(raw, 'forwarderName', pickStr(raw, 'forwarder_name', '—')),
    condition: pickStr(raw, 'condition', '—'),
    action: pickStr(raw, 'action', '—'),
    hitCount: pickNum(raw, 'hitCount', pickNum(raw, 'hit_count', 0)),
    priority: pickNum(raw, 'priority', 0),
    enabled: pickBool(raw, 'enabled', true),
    lastHitAt: formatEpochText(pickEither(raw, 'lastHitAt', 'last_hit_at')),
  };
}

// ===========================================================================
// 转发规则：结构化条件契约（对齐 daemon `crates/daemon/src/rules.rs` 的
// Condition / Action 模型 —— 写接口必须下发结构化 JSON 对象，**禁止**字符串）
// ===========================================================================

/** 条件比较运算符（后端 `CmpOp` 的 snake_case wire 值）。 */
export type RuleCmpOp = 'gt' | 'ge' | 'lt' | 'le' | 'eq' | 'ne';

/** 条件比较值：JSON number 或 string（后端 `ConditionValue` 为 untagged，两者均合法）。 */
export type RuleConditionValue = number | string;

/** 结构化条件（后端 `Condition` 的 JSON 形状；`when` 恒真 = 下发 `null`，不得下发空 and）。 */
export type RuleCondition =
  | { kind: 'cmp'; field: string; op: RuleCmpOp; value: RuleConditionValue }
  | { kind: 'and'; conditions: RuleCondition[] }
  | { kind: 'or'; conditions: RuleCondition[] }
  | { kind: 'not'; condition: RuleCondition };

/** 结构化动作（后端 `Action` 的 JSON 形状）。 */
export type RuleAction = { kind: 'publish'; topic: string } | { kind: 'remap'; fields: Record<string, string> };

/** 转发规则完整记录：`RuleRecord` + 编辑回显所需的结构化字段。 */
export interface RuleDetail extends RuleRecord {
  /** 结构化条件；`null` = 恒真（后端未下发时亦为 null）。 */
  when: RuleCondition | null;
  /** 结构化动作列表。 */
  actions: RuleAction[];
  /** SELECT 字段白名单（空 = 输出全部字段）。 */
  select: string[];
  /** 依赖规则 id 列表（wire: depends_on）。 */
  dependsOn: string[];
}

/** 规则清单读取结果（`notice` 非空 = 数据源不可得的真实原因，调用方须原样展示）。 */
export interface RuleListResult {
  items: RuleDetail[];
  notice: string;
}

/**
 * 条件值输入 → 下发值（**大数红线**）。
 *
 *  · 只有整体匹配十进制数值字面量（可选负号 / 小数）才转 JSON number；
 *    `1e30` / `0x10` / 空串等一律按字符串下发（不硬转，绝不 parseInt）；
 *  · 整数必须 `Number.isSafeInteger`，超出安全整数范围一律按字符串下发。
 */
export function coerceRuleConditionValue(raw: string): RuleConditionValue {
  const text = raw.trim();
  if (!/^-?\d+(\.\d+)?$/.test(text)) {
    return text;
  }
  const num = Number(text);
  if (!Number.isFinite(num)) {
    return text;
  }
  if (Number.isInteger(num) && !Number.isSafeInteger(num)) {
    return text;
  }
  return num;
}

/** 把 unknown 收敛为 RuleCondition（形状不合法回 null，由调用方决定回显策略）。 */
function asRuleCondition(value: unknown): RuleCondition | null {
  if (
    value !== null &&
    typeof value === 'object' &&
    !Array.isArray(value) &&
    typeof (value as { kind?: unknown }).kind === 'string'
  ) {
    return value as RuleCondition;
  }
  return null;
}

/** 把 unknown 收敛为 RuleAction[]（非对象 / 缺 kind 的项丢弃）。 */
function asRuleActions(value: unknown): RuleAction[] {
  if (!Array.isArray(value)) {
    return [];
  }
  return value.filter(
    (item): item is RuleAction =>
      item !== null &&
      typeof item === 'object' &&
      !Array.isArray(item) &&
      typeof (item as { kind?: unknown }).kind === 'string',
  );
}

/** 结构化条件 → 人读描述（列表展示兜底；后端已给 condition 描述串时不用它）。 */
export function describeRuleCondition(when: RuleCondition | null): string {
  if (!when) {
    return '全部数据（无条件过滤）';
  }
  if (when.kind === 'cmp') {
    const opLabel: Readonly<Record<RuleCmpOp, string>> = { gt: '>', ge: '>=', lt: '<', le: '<=', eq: '=', ne: '≠' };
    return `${when.field} ${opLabel[when.op]} ${String(when.value)}`;
  }
  if (when.kind === 'not') {
    return `非（${describeRuleCondition(when.condition)}）`;
  }
  const inner = when.conditions.map(describeRuleCondition).join(when.kind === 'and' ? ' 且 ' : ' 或 ');
  return when.conditions.length > 1 ? `(${inner})` : inner;
}

/** 结构化动作 → 人读描述（列表展示兜底）。 */
export function describeRuleActions(actions: RuleAction[]): string {
  if (actions.length === 0) {
    return '—';
  }
  return actions
    .map((a) => (a.kind === 'publish' ? `发布到 ${a.topic}` : `重映射 ${Object.keys(a.fields).join(' / ')}`))
    .join('；');
}

/** `/api/rules` 行 → RuleDetail（宽容映射；`condition` / `action` 描述串缺失时由结构化字段兜底生成）。 */
function mapRuleDetail(raw: Record<string, unknown>, idx: number): RuleDetail {
  const when = raw['when'] === undefined ? null : asRuleCondition(raw['when']);
  const actions = asRuleActions(raw['actions']);
  const selectRaw = raw['select'];
  const dependsRaw = raw['depends_on'] ?? raw['dependsOn'];
  return {
    ...mapRule(raw, idx),
    condition: pickStr(raw, 'condition', describeRuleCondition(when)),
    action: pickStr(raw, 'action', describeRuleActions(actions)),
    when,
    actions,
    select: Array.isArray(selectRaw) ? selectRaw.filter((s): s is string => typeof s === 'string') : [],
    dependsOn: Array.isArray(dependsRaw) ? dependsRaw.filter((s): s is string => typeof s === 'string') : [],
  };
}

/** `/api/groups` 行 → DeviceGroup。 */
function mapGroup(raw: Record<string, unknown>, idx: number): DeviceGroup {
  return {
    id: pickStr(raw, 'id', pickStr(raw, 'group_id', `group-${idx}`)),
    name: pickStr(raw, 'name', `分组-${idx + 1}`),
    deviceCount: pickNum(raw, 'deviceCount', pickNum(raw, 'device_count', 0)),
    isDefault: pickBool(raw, 'isDefault', pickBool(raw, 'is_default', false)),
  };
}

/** `/api/roles` 行 → RoleRecord（permissions 为字符串数组，非字符串项丢弃）。 */
function mapRole(raw: Record<string, unknown>, idx: number): RoleRecord {
  const permsRaw = raw['permissions'];
  const permissions = Array.isArray(permsRaw) ? permsRaw.filter((p): p is string => typeof p === 'string') : [];
  return {
    id: pickStr(raw, 'id', pickStr(raw, 'role_id', `role-${idx}`)),
    name: pickStr(raw, 'name', `角色-${idx + 1}`),
    permissions,
    builtin: pickBool(raw, 'builtin', false),
    accountCount: pickNum(raw, 'accountCount', pickNum(raw, 'account_count', 0)),
  };
}

/** `/api/permissions` 行 → PermissionRecord。 */
function mapPermission(raw: Record<string, unknown>, idx: number): PermissionRecord {
  return {
    id: pickStr(raw, 'id', `perm-${idx}`),
    label: pickStr(raw, 'label', pickStr(raw, 'id', `权限-${idx + 1}`)),
    group: pickStr(raw, 'group', pickStr(raw, 'group_label', '其他')),
    scope: pickStr(raw, 'scope', ''),
  };
}

/** `/api/accounts` 行 → AccountRecord。 */
function mapAccount(raw: Record<string, unknown>, idx: number): AccountRecord {
  const lastLoginRaw = pickEither(raw, 'lastLoginAt', 'last_login_at');
  const createdRaw = pickEither(raw, 'createdAt', 'created_at');
  return {
    account: pickStr(raw, 'account', pickStr(raw, 'username', `acct-${idx}`)),
    role: pickStr(raw, 'role', ''),
    status: pickStr(raw, 'status', ''),
    lastLoginAt: lastLoginRaw ? formatEpochText(lastLoginRaw) : '—',
    createdAt: createdRaw ? formatEpochText(createdRaw) : '—',
  };
}

/** `/api/overview` → GatewayInfo。 */
function mapStatus(raw: Record<string, unknown>): GatewayInfo {
  const dflt = HONEST_EMPTY_GATEWAY;
  return {
    name: pickStr(raw, 'name', dflt.name),
    machineCode: pickStr(raw, 'machineCode', dflt.machineCode),
    deployMode: pickStr(raw, 'deployMode', dflt.deployMode),
    version: pickStr(raw, 'version', dflt.version),
    hostname: pickStr(raw, 'hostname', dflt.hostname),
    manageUrl: pickStr(raw, 'manageUrl', dflt.manageUrl),
    port: pickNum(raw, 'port', dflt.port),
    startedAt: formatEpochText(pickEither(raw, 'startedAt', 'started_at') || dflt.startedAt),
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

/** 后端诚实空网关（`getGateway` 空缓存 / `mapStatus` 缺字段兜底）——绝不掺演示值。 */
const HONEST_EMPTY_GATEWAY: GatewayInfo = {
  name: '—',
  machineCode: '—',
  deployMode: '—',
  version: '—',
  hostname: '—',
  manageUrl: '—',
  port: 0,
  startedAt: '—',
  uptimeText: '—',
  deviceCount: 0,
  onlineCount: 0,
  pointCount: 0,
  failedPointCount: 0,
  sampleRatePerSec: 0,
  forwardRatePerSec: 0,
  queueUsedGb: 0,
  queueCapacityGb: 0,
  queueDrainDays: 0,
  totalForwardedRecords: '0',
};

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

/** 剩余时长文本（优先剩余天数；缺失退回秒数；都缺失给「—」）。 */
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

/**
 * 生效授权状态：以 `GET /api/license/status` 的真实快照覆盖诚实空值基线。
 *
 * **无任何演示数据**：未取到真实快照时返回诚实空值（status=`stopped`）；
 * 纯展示字段（校验档位 / 心跳时间 / 能力矩阵）后端无数据源，诚实留 `—` / 空。
 */
function effectiveLicense(degradedView: boolean): MockLicense {
  const base = degradedView ? LICENSE_DEGRADED_BASELINE : LICENSE_ACTIVE_BASELINE;
  const real = realCache.license;
  if (!real) {
    return base;
  }
  return {
    ...base,
    status: mapLicenseStatus(real.status),
    tierId: real.tier,
    tierName: LICENSE_TIER_LABELS[real.tier] ?? (real.tier || '—'),
    remainingDays: pickCountFromText(real.remainingDays),
    remainingText: licenseRemainingText(real),
    validUntil: real.validUntil ? formatEpochText(real.validUntil) : real.status === 'unlicensed' ? '未授权' : '—',
    degradeReason: real.degradeReason || (real.status === 'degraded' ? '授权降级（后端未提供原因）' : ''),
    onExpireText: real.northForwardAllowed
      ? '授权有效：北向转发正常放行'
      : '北向转发已停用，本地采集继续（恢复路径：完成授权激活）',
  };
}

/** 诚实空值授权基线（活动视图）。 */
const LICENSE_ACTIVE_BASELINE: MockLicense = {
  status: 'stopped',
  tierId: '',
  tierName: '—',
  grade: '',
  remainingDays: 0,
  remainingText: '—',
  validUntil: '—',
  lastHeartbeatAt: '—',
  nextHeartbeatAt: '—',
  anchorSources: '—',
  degradeReason: '',
  onExpireText: '—',
  capabilities: [],
};

/** 诚实空值授权基线（降级视图）。 */
const LICENSE_DEGRADED_BASELINE: MockLicense = {
  ...LICENSE_ACTIVE_BASELINE,
  status: 'grace',
};

// ---------------------------------------------------------------------------
// 各接口拉取（失败 → 空缓存 + notices 记录真实原因）
// ---------------------------------------------------------------------------

/** 拉取设备清单。 */
async function fetchDevices(): Promise<void> {
  try {
    const raw = await apiRequest<unknown[]>('/api/devices');
    realCache.devices = Array.isArray(raw) ? raw.map((row, i) => mapDevice(asRecord(row), i)) : [];
    realCache.notices.devices = '';
  } catch (cause) {
    realCache.devices = [];
    realCache.notices.devices = describeFailure(cause, '设备清单');
  }
}

/** 按设备拉取点位（`/api/points` 需 `device_id` 过滤；缺省返回空数组）。 */
async function fetchPointsOfDevice(deviceId: string, offset: number): Promise<PointRecord[]> {
  const qs = `?device_id=${encodeURIComponent(deviceId)}`;
  const raw = await apiRequest<unknown[]>(`/api/points${qs}`);
  return Array.isArray(raw) ? raw.map((row, i) => mapPoint(asRecord(row), offset + i)) : [];
}

/** 拉取全量点位：对各设备并行拉取后合并。 */
async function fetchAllPoints(): Promise<void> {
  try {
    if (realCache.devices.length === 0) {
      realCache.points = [];
      realCache.notices.points = '';
      return; // 设备清单为空：无点位可拉（非错误）
    }
    const settled = await Promise.allSettled(
      realCache.devices.map((d, i) => fetchPointsOfDevice(d.id, i * 1000)),
    );
    const merged: PointRecord[] = [];
    let firstError = '';
    for (const result of settled) {
      if (result.status === 'fulfilled') {
        merged.push(...result.value);
      } else if (!firstError) {
        firstError = describeFailure(result.reason, '点位清单');
      }
    }
    realCache.points = merged;
    realCache.notices.points = firstError;
  } catch (cause) {
    realCache.points = [];
    realCache.notices.points = describeFailure(cause, '点位清单');
  }
}

/** 拉取设备清单 + 逐设备点位。 */
async function fetchDevicesAndPoints(): Promise<void> {
  await fetchDevices();
  await fetchAllPoints();
}

/** 拉取北向出口清单（`GET /api/forwarders`，`id` = 出口名）。 */
async function fetchForwarders(): Promise<void> {
  try {
    const raw = await apiRequest<unknown[]>('/api/forwarders');
    realCache.outlets = Array.isArray(raw) ? raw.map((row, i) => mapForwarder(asRecord(row), i)) : [];
    realCache.notices.forwarders = '';
  } catch (cause) {
    realCache.outlets = [];
    realCache.notices.forwarders = describeFailure(cause, '北向出口');
  }
}

/** 拉取网关信息（`GET /api/overview`）。 */
async function fetchOverview(): Promise<void> {
  try {
    const raw = await apiRequest<Record<string, unknown>>('/api/overview');
    realCache.status = mapStatus(asRecord(raw));
  } catch (cause) {
    realCache.status = null;
    realCache.notices.devices = realCache.notices.devices || describeFailure(cause, '网关信息');
  }
}

/**
 * 拉取审计条目：`GET /api/audit`（有限 JSON，RBAC `audit.view`）。
 *
 * ⚠️ 审计页的正确数据源是本端点；`/api/events` 是**无限 SSE 事件流**，绝不放 preload。
 */
async function fetchAudit(): Promise<void> {
  try {
    const raw = await apiRequest<Record<string, unknown>>('/api/audit?limit=200');
    const rows = Array.isArray(raw['rows']) ? raw['rows'].map(asRecord) : [];
    realCache.events = rows.map((row, i) => mapAuditRow(row, i));
    realCache.notices.audit = '';
  } catch (cause) {
    realCache.events = [];
    realCache.notices.audit = describeFailure(cause, '审计日志');
  }
}

/** 拉取告警：`GET /api/alerts`（后端无告警引擎 → `source:"unsupported"` 诚实空态）。 */
async function fetchAlerts(): Promise<void> {
  try {
    const raw = await apiRequest<Record<string, unknown>>('/api/alerts');
    const rows = Array.isArray(raw['items']) ? raw['items'].map(asRecord) : [];
    realCache.alarms = rows.map((row, i) => mapEventAlarm(row, i)).filter((a): a is AlarmRecord => a !== null);
    realCache.notices.alerts =
      raw['source'] === 'unsupported' ? pickStr(raw, 'reason', '后端告警引擎未落地，当前无真实告警数据源') : '';
  } catch (cause) {
    realCache.alarms = [];
    realCache.notices.alerts = describeFailure(cause, '告警');
  }
}

/** 拉取转发规则：`GET /api/rules`（后端无规则引擎 → 诚实空态）。 */
async function fetchRules(): Promise<void> {
  try {
    const raw = await apiRequest<Record<string, unknown>>('/api/rules');
    const rows = Array.isArray(raw['items']) ? raw['items'].map(asRecord) : [];
    realCache.rules = rows.map((row, i) => mapRule(row, i));
    realCache.notices.rules =
      raw['source'] === 'unsupported' ? pickStr(raw, 'reason', '后端规则引擎未落地，当前无真实规则数据源') : '';
  } catch (cause) {
    realCache.rules = [];
    realCache.notices.rules = describeFailure(cause, '转发规则');
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

/** preload 整体超时护栏（毫秒）：超时后按**已成功分组**继续，不再阻塞调用方。 */
const PRELOAD_TIMEOUT_MS = 8000;

/**
 * 预取真实数据（启动 / 登录后调用）。
 *
 * 分组并行 `allSettled`：任一失败只影响自身缓存（诚实空态 + notices 记录原因）。
 *
 * @returns 全部成功返回 true（仅供日志 / 调试，页面无需关心）
 */
/** 启动预载单次尝试（原 `preloadRealData` 主体；读操作幂等，重试无副作用）。 */
async function preloadOnce(): Promise<boolean> {
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
    console.warn(`[web-console] preloadRealData：超过 ${PRELOAD_TIMEOUT_MS} ms 未全部完成，按已成功分组继续`);
    void Promise.allSettled(groups).then(() => bumpCacheVersion());
    return false;
  }

  const okCount = raced.results.filter((r) => r.status === 'fulfilled').length;
  if (okCount < raced.results.length) {
    console.warn(`[web-console] preloadRealData：${okCount}/${raced.results.length} 分组成功，失败分组已按诚实空态处理`);
  }
  return okCount === raced.results.length;
}

/** 启动预载重试退避（指数）：1s → 2s → 4s，共三次重试。 */
const PRELOAD_RETRY_DELAYS_MS: readonly number[] = [1_000, 2_000, 4_000];

function sleep(ms: number): Promise<void> {
  return new Promise((resolve) => setTimeout(resolve, ms));
}

/**
 * 启动预载（含指数退避重试）。
 *
 * daemon 与 vite dev server 同时冷启时，首轮请求可能整批落在 daemon 就绪之前
 * （诚实空态 + notices）。这里对「未全部分组成功」的结果按 1s / 2s / 4s 退避重取，
 * 全部成功即提前返回；三次重试后仍未全成功则如实返回 false（页面侧经 notices
 * 展示失败原因，绝不回退演示数据）。
 */
export async function preloadRealData(): Promise<boolean> {
  let result = await preloadOnce();
  for (const delay of PRELOAD_RETRY_DELAYS_MS) {
    if (result) {
      return result;
    }
    await sleep(delay);
    result = await preloadOnce();
  }
  return result;
}

/**
 * 全量刷新真实缓存（写操作成功后调用；重新拉取 + 自增 `dataVersion`）。
 *
 * **禁止本地拼装**：写操作一律在真实接口返回后由本方法重取真相。
 */
export async function refresh(): Promise<void> {
  await Promise.allSettled([
    fetchDevicesAndPoints(),
    fetchForwarders(),
    fetchOverview(),
    fetchAudit(),
    fetchAlerts(),
    fetchRules(),
    fetchLicense(),
  ]);
  realCache.loaded = true;
  bumpCacheVersion();
}

/** 低频轮询网关信息（OverviewPage 的 1s tick 每 5 拍触发一次）。 */
export async function refreshOverview(): Promise<void> {
  await fetchOverview();
  if (realCache.status) {
    bumpCacheVersion();
  }
}

/**
 * 仅重取告警清单并 bump `dataVersion`。
 *
 * 批量处置（`resolveAlarm(..., { refetch: false })` 逐条下发）后在循环结束处
 * **统一调用一次**，替代逐条内部重取（N 条处置 = 1 次清单 GET，而非 N 次）。
 */
export async function refreshAlerts(): Promise<void> {
  await fetchAlerts();
  bumpCacheVersion();
}

/** 通用分页。 */
function paginate<T>(items: T[], page: number, pageSize: number): Paged<T> {
  return { items: items.slice((page - 1) * pageSize, (page - 1) * pageSize + pageSize), total: items.length, page };
}

// ===========================================================================
// 运维动作（ops）
// ===========================================================================

/** 运维动作 API（repo.ops.*）。 */
export interface OpsApi {
  /** 触发网关重启（`POST /api/ops/restart`）。 */
  restart(
    input: { actor: string } & WriteMeta,
  ): Promise<{ ok: boolean; message: string }>;
  /** 触发网关停止（`POST /api/ops/stop`）。 */
  stop(
    input: { actor: string } & WriteMeta,
  ): Promise<{ ok: boolean; message: string }>;
  /** 新建采集器（`POST /api/ops/collectors`；后端 body 契约 `{actor, action:"pause"|"resume"}`，
   *  方法名沿用历史符号，语义实为「暂停 / 恢复采集器组」）。 */
  createCollector(input: { actor: string; action: 'pause' | 'resume' }): Promise<{ ok: boolean; message: string }>;
  /** 拉取运行日志（`GET /api/ops/logs`，结果并入审计清单）。
   *  后端 `actor` 为**必填审计字段**（谁拉取了日志），此处统一取 `DEFAULT_ACTOR`
   *  并与账号类方法同口径（`input.actor ?? DEFAULT_ACTOR`）。 */
  logs(): Promise<AuditEntry[]>;
  /** 健康检查（`GET /api/health`）。 */
  health(): Promise<{ ok: boolean; message: string }>;
  /** 自检清单（`GET /api/diagnostics/selfcheck`，后端已落地 7 项真实检查）。 */
  selfCheck(): Promise<SelfCheckReport>;
  /** 更新检查（`GET /api/updates/check`；**实测仅 GET**，POST → 405）。 */
  checkUpdates(): Promise<UpdateCheckInfo>;
  /**
   * 执行系统更新（`POST /api/updates/apply`；**危险操作**，三字段硬契约）。
   *
   * body **必须**是彼此独立的 `{reason, note, confirm}` 三字段（`note` **绝不**
   * 并入 `reason`）；后端出现未知字段 / 任一字段 trim 后为空 → 400
   * `validation_failed`（fail-closed，不进入执行路径）。
   * 请求失败（400/401/403/404/501/503）**绝不**假装成功：返回结构化结果 + 人读消息。
   */
  applyUpdate(input: { reason: string; note: string; confirm: string }): Promise<UpdateApplyOutcome>;
  /** 开机自启状态（`GET /api/service/autostart`；写入未实现时 `writeSupported:false` 原样保留）。 */
  autostartStatus(): Promise<AutostartStatus>;
  /** 设置开机自启（`PUT /api/service/autostart`；后端未上线 → 诚实 405/404 失败）。 */
  setAutostart(input: { enable: boolean; reason?: string; note?: string }): Promise<{ ok: boolean; message: string }>;
}

/** `GET /api/diagnostics/selfcheck` 单条检查项。 */
export interface SelfCheckItem {
  /** 检查项标识（`config_writable` / `scheduler` / `alarm_engine` / `license` / `audit_logger` / `machine_code` / `clock`）。 */
  name: string;
  /**
   * 该项是否通过。
   * ⚠️ 语义边界：后端按「该检查可执行且无致命异常」判定，与业务是否就绪**不是一回事**
   * —— 例如 `license.ok=true` 只代表授权探查跑通，`detail.north_forward_allowed` 才说明北向是否放行。
   * 页面必须同时渲染 `detail`，禁止只用 `ok` 断言业务状态。
   */
  ok: boolean;
  /** 明细；逐项结构不同，原样保留交由页面做可读化渲染（空对象 = 后端未给明细）。 */
  detail: Record<string, unknown> | null;
}

/** `GET /api/diagnostics/selfcheck` 响应镜像。 */
export interface SelfCheckReport {
  /** 自检时间（毫秒时间戳，**字符串** —— uint64 红线，绝不 parseInt）。 */
  checkedAt: string;
  /** 整体是否通过（后端口径：全部检查项可执行）。 */
  ok: boolean;
  checks: SelfCheckItem[];
}

/** `GET /api/updates/check` 响应的前端镜像（camelCase 透传）。 */
export interface UpdateCheckInfo {
  /** 后端是否具备检查能力（未配置升级源 → false + `reason` 说明）。 */
  checkSupported: boolean;
  currentVersion: string;
  updateAvailable: boolean;
  /** 可升级版本；后端未配置升级源时为 JSON null → 诚实映射为空串。 */
  availableVersion: string;
  source: string;
  reason: string;
}

/** `POST /api/updates/apply` 响应的前端镜像（camelCase 透传）。 */
export interface UpdateApplyResult {
  /** 后端是否**具备执行更新**的能力（未配置升级源 / 能力未接线 → false）。 */
  supported: boolean;
  /** 本次请求是否被后端**受理**（HTTP 200 且通过三要素校验）；≠ 已升级完成。 */
  accepted: boolean;
  /** 是否已真正完成版本替换；**当前后端恒 false** —— 绝不据此声称「已升级」。 */
  applied: boolean;
  currentVersion: string;
  /** 目标版本；后端未给出时为 JSON null → 诚实映射为空串。 */
  targetVersion: string;
  source: string;
  sourceConfigured: boolean;
  /** 面向用户的原因 / 说明（`supported:false` / `accepted:false` 时照原文展示）。 */
  reason: string;
}

/**
 * 更新执行结果（结构化 + 人读消息）。
 *
 * ⚠️ 诚实边界：`accepted:true` 只代表**请求被受理**（后端完成下载 + 验签），
 * `applied` 当前恒为 false —— 版本替换由宿主安装器在**重启时**完成，
 * 因此 `message` **绝不**出现「成功」字样，结果一律以重启后的实际版本号为准。
 */
export interface UpdateApplyOutcome {
  /** 后端是否受理本次更新请求（HTTP 200 且 `accepted=true`）。 */
  accepted: boolean;
  /** 后端是否已真正完成版本替换（当前恒 false）。 */
  applied: boolean;
  /** 后端原始结构化结果；请求失败（非 200 / 网络失败）时为 `null`。 */
  result: UpdateApplyResult | null;
  /** 面向用户的人读消息（含后端 `reason` 原文；禁用「成功」措辞）。 */
  message: string;
}

/** `GET /api/service/autostart` 响应的前端镜像。 */
export interface AutostartStatus {
  supported: boolean;
  /** 是否已注册自启；后端非 Windows 平台回 JSON null → **原样保留 null**（不猜）。 */
  registered: boolean | null;
  command: string;
  source: string;
  queryError: string;
  /** 后端诚实声明（当前 false = 自启写入未实现）；repo 原样保留，绝不冒充可写。 */
  writeSupported: boolean;
  writeReason: string;
  /** 自启注册目标（如 systemd unit / shell 命令标识）；后端按需返回，可能缺省。 */
  target?: string | null;
  /** 注册目标形态；后端按实际守护类型返回，可能缺省。 */
  targetKind?: 'shell' | 'daemon' | null;
  /** 部署形态：`windows-service` / `windows-desktop` / `linux-systemd` / `docker`；证据不足为 `unknown`。 */
  form: string;
  /** 自启托管方：`registry-run` / `systemd` / `container-orchestrator`。 */
  managedBy: string;
  /**
   * 自启保障声明。容器形态 `provenance` 恒为 `declared`——网关在容器内**测不到**
   * compose 的 restart 策略与宿主 docker.service 状态，只回显宿主声明注入的值。
   */
  autostartGuaranteed: AutostartGuarantee;
}

/** 自启保障声明（`autostart_guaranteed`）。 */
export interface AutostartGuarantee {
  /** 保障值：容器形态 = compose 注入的 restart 策略声明值；systemd 形态为空串。 */
  value: string;
  /** `declared` = 宿主声明值 / `detected` = 真实探测 / `unknown` = 不可得。 */
  provenance: string;
  /** 面向用户的说明与指引（诚实：不承诺网关能改写宿主）。 */
  hint: string;
}

/** `GET|PUT /api/settings/backup-policy` 的备份策略。
 *
 * `retentionCount` / `intervalMin` 为**字符串数字**（大数红线：透传，禁 parseInt）。
 */
export interface BackupPolicy {
  autoBeforeWrite: boolean;
  retentionCount: string;
  intervalMin: string;
  source: string;
}

/** ops 写动作公共实现（restart / stop 同一后端契约）。 */
async function postOpsAction(
  path: string,
  input: { actor: string } & WriteMeta,
  okMessage: string,
): Promise<{ ok: boolean; message: string }> {
  try {
    await apiRequest<unknown>(path, {
      method: 'POST',
      // 危险运维指令：reason（原因枚举）/ note（补充说明）/ confirm（回显）分别原样入审计。
      body: JSON.stringify(withReason({ actor: input.actor }, input)),
    });
    return { ok: true, message: okMessage };
  } catch (cause) {
    if (cause instanceof ApiError && cause.status === 403) {
      return { ok: false, message: '权限不足（403），当前角色不可执行该操作。' };
    }
    if (cause instanceof ApiError && cause.status === 400) {
      return {
        ok: false,
        message:
          '网关拒绝（400）：网关标识（gateway id）回显校验未通过。页面取到的标识可能与网关运行期配置不一致，请刷新页面后重试。',
      };
    }
    return { ok: false, message: describeFailure(cause, '运维指令') };
  }
}

/** 运维动作实现。 */
function buildOps(): OpsApi {
  return {
    async restart(input): Promise<{ ok: boolean; message: string }> {
      return postOpsAction(
        '/api/ops/restart',
        input,
        '重启指令已下发，网关将优雅停机并由 Supervisor / 服务管理器拉起。',
      );
    },

    async stop(input): Promise<{ ok: boolean; message: string }> {
      return postOpsAction(
        '/api/ops/stop',
        input,
        '停止指令已下发，网关将优雅停机（是否再次拉起由 Supervisor / 服务管理器决定，不承诺自动重启）。',
      );
    },

    async createCollector(input): Promise<{ ok: boolean; message: string }> {
      try {
        // 后端 `CollectorsBody`：`{actor（必填非空）, action: "pause"|"resume"}`，
        // 其余字段被忽略 —— 旧 `{name, device_id}` 形状必 400，已对齐。
        await apiRequest<unknown>('/api/ops/collectors', {
          method: 'POST',
          body: JSON.stringify({ actor: input.actor, action: input.action }),
        });
        return { ok: true, message: input.action === 'pause' ? '采集器组已暂停。' : '采集器组已恢复。' };
      } catch (cause) {
        return { ok: false, message: describeFailure(cause, '采集器操作') };
      }
    },

    async logs(): Promise<AuditEntry[]> {
      try {
        // 后端 `/api/ops/logs` 的 `actor` 为必填审计字段（缺失 → 400），
        // 用 `DEFAULT_ACTOR` 与账号类写方法保持同一口径。
        const raw = await apiRequest<unknown[]>(
          `/api/ops/logs?actor=${encodeURIComponent(DEFAULT_ACTOR)}`,
        );
        const entries = (Array.isArray(raw) ? raw : []).map((row, i) => mapEventAudit(asRecord(row), i));
        realCache.events = [...entries, ...realCache.events];
        return entries;
      } catch (cause) {
        realCache.notices.audit = describeFailure(cause, '运行日志');
        return [];
      }
    },

    async health(): Promise<{ ok: boolean; message: string }> {
      try {
        await apiRequest<unknown>('/api/health');
        return { ok: true, message: '网关服务正常。' };
      } catch (cause) {
        if (cause instanceof ApiError && cause.status === 403) {
          return { ok: false, message: '权限不足（403）。' };
        }
        return { ok: false, message: describeFailure(cause, '健康检查') };
      }
    },

    async selfCheck(): Promise<SelfCheckReport> {
      // wire（be-rules B 组 + 真机 2026-09-26 实测）：GET /api/diagnostics/selfcheck
      // → `{ ok, checked_at, checks:[{ name, ok, detail }] }`，7 项真实检查。
      // ⚠️ 绝不 catch 成空清单 —— 那是「假空态」，会把「接口失败」渲染成「尚未自检」，
      //    与诚实降级红线冲突。失败必须向上抛，由页面呈现真实原因。
      const raw = await apiRequest<Record<string, unknown>>('/api/diagnostics/selfcheck');
      const rows = Array.isArray(raw['checks']) ? (raw['checks'] as unknown[]) : [];
      return {
        // `checked_at` 是毫秒时间戳字符串（JSON 大数红线），绝不做数值转换。
        checkedAt: pickStr(raw, 'checkedAt', pickStr(raw, 'checked_at', '')),
        ok: pickBool(raw, 'ok', false),
        checks: rows.map((row) => {
          const r = asRecord(row);
          const detail = asRecord(r['detail']);
          return {
            name: pickStr(r, 'name', ''),
            ok: pickBool(r, 'ok', false),
            detail: Object.keys(detail).length > 0 ? detail : null,
          };
        }),
      };
    },

    async checkUpdates(): Promise<UpdateCheckInfo> {
      // wire（ops_api.rs + 真机 2026-09-26 实测）：GET only（POST → 405）；未配置
      // 升级源 → `check_supported:false` + `reason`，照实透传，绝不伪造可升级。
      try {
        const raw = await apiRequest<Record<string, unknown>>('/api/updates/check');
        return {
          checkSupported: pickBool(raw, 'checkSupported', pickBool(raw, 'check_supported', false)),
          currentVersion: pickStr(raw, 'currentVersion', pickStr(raw, 'current_version', '')),
          updateAvailable: pickBool(raw, 'updateAvailable', pickBool(raw, 'update_available', false)),
          // 后端 `available_version: null` → 空串（诚实「无可升级版本」，不臆造）。
          availableVersion: pickStr(raw, 'availableVersion', pickStr(raw, 'available_version', '')),
          source: pickStr(raw, 'source', ''),
          reason: pickStr(raw, 'reason', ''),
        };
      } catch (cause) {
        return {
          checkSupported: false,
          currentVersion: '',
          updateAvailable: false,
          availableVersion: '',
          source: '',
          reason: describeFailure(cause, '更新检查'),
        };
      }
    },

    async applyUpdate(input): Promise<UpdateApplyOutcome> {
      // wire（ops_api.rs:148 硬契约）：POST /api/updates/apply，body **恰好**
      // `{reason, note, confirm}` 三个彼此独立的字段（`note` 绝不并入 `reason`）；
      // 出现未知字段 / 任一字段 trim 后为空 → 400 `validation_failed`。
      // 失败一律返回结构化结果 + 人读消息，**绝不**假装成功。
      try {
        const raw = await apiRequest<Record<string, unknown>>('/api/updates/apply', {
          method: 'POST',
          body: JSON.stringify({ reason: input.reason, note: input.note, confirm: input.confirm }),
        });
        const result: UpdateApplyResult = {
          supported: pickBool(raw, 'supported', false),
          accepted: pickBool(raw, 'accepted', false),
          applied: pickBool(raw, 'applied', false),
          currentVersion: pickStr(raw, 'currentVersion', pickStr(raw, 'current_version', '')),
          // 后端 `target_version: null` → 空串（诚实「目标未知」，不臆造版本号）。
          targetVersion: pickStr(raw, 'targetVersion', pickStr(raw, 'target_version', '')),
          source: pickStr(raw, 'source', ''),
          sourceConfigured: pickBool(raw, 'sourceConfigured', pickBool(raw, 'source_configured', false)),
          reason: pickStr(raw, 'reason', ''),
        };
        return {
          accepted: result.accepted,
          applied: result.applied,
          result,
          message: describeApplyResult(result),
        };
      } catch (cause) {
        if (cause instanceof ApiError && (cause.status === 404 || cause.status === 501)) {
          return {
            accepted: false,
            applied: false,
            result: null,
            message: '后端更新执行接口尚未上线（POST /api/updates/apply 未部署），本次未做任何变更。',
          };
        }
        if (cause instanceof ApiError && cause.status === 403) {
          return {
            accepted: false,
            applied: false,
            result: null,
            message: '权限不足（403）：当前角色不可执行系统更新。',
          };
        }
        if (cause instanceof ApiError && cause.status === 400) {
          // 后端 400 体形如 `{error:"validation_failed", message:<detail>}`：
          // 先取字段级原因，再取人读消息（去掉 `——` 前缀），最后兜底诚实说明。
          const detail = validationReason(cause) || extractErrorMessage(cause.body).replace(/^——/, '');
          return {
            accepted: false,
            applied: false,
            result: null,
            message:
              `网关拒绝（400）：${detail || '更新请求三要素（reason / note / confirm）校验未通过'}。` +
              '本次未做任何变更。',
          };
        }
        return { accepted: false, applied: false, result: null, message: describeFailure(cause, '系统更新') };
      }
    },

    async autostartStatus(): Promise<AutostartStatus> {
      // wire（ops_api.rs）：Windows 真实现（reg query HKCU Run）；非 Windows /
      // 查询失败 → `registered:null`；写入未实现 → `write_supported:false` + 原因。
      // 形态字段（form / managed_by / autostart_guaranteed）为后端派生，缺失一律回落
      // 空值 / unknown，绝不猜平台。
      try {
        const raw = await apiRequest<Record<string, unknown>>('/api/service/autostart');
        const registeredRaw = raw['registered'];
        return {
          supported: pickBool(raw, 'supported', false),
          // 后端 JSON null（非 Windows / 未查询）→ 原样保留 null，不猜布尔。
          registered: typeof registeredRaw === 'boolean' ? registeredRaw : null,
          command: pickStr(raw, 'command', ''),
          source: pickStr(raw, 'source', ''),
          queryError: pickStr(raw, 'queryError', pickStr(raw, 'query_error', '')),
          writeSupported: pickBool(raw, 'writeSupported', pickBool(raw, 'write_supported', false)),
          writeReason: pickStr(raw, 'writeReason', pickStr(raw, 'write_reason', '')),
          form: pickStr(raw, 'form', 'unknown'),
          managedBy: pickStr(raw, 'managedBy', pickStr(raw, 'managed_by', '')),
          autostartGuaranteed: readGuarantee(raw['autostartGuaranteed'] ?? raw['autostart_guaranteed']),
        };
      } catch (cause) {
        return {
          supported: false,
          registered: null,
          command: '',
          source: '',
          queryError: describeFailure(cause, '自启状态'),
          writeSupported: false,
          writeReason: '',
          form: 'unknown',
          managedBy: '',
          autostartGuaranteed: { value: '', provenance: 'unknown', hint: '' },
        };
      }
    },

    async setAutostart(input): Promise<{ ok: boolean; message: string }> {
      try {
        const body = withReason({ enable: input.enable }, input);
        await apiRequest<unknown>('/api/service/autostart', { method: 'PUT', body: JSON.stringify(body) });
        return { ok: true, message: input.enable ? '开机自启已启用。' : '开机自启已关闭。' };
      } catch (cause) {
        if (cause instanceof ApiError && (cause.status === 404 || cause.status === 405)) {
          return {
            ok: false,
            message: '后端尚未提供自启写接口（PUT /api/service/autostart 未落地，真机实测 405），本次未做任何变更。',
          };
        }
        // 501 = 后端明确「当前部署形态不支持由网关改写自启」（非 Windows 恒此分支）。
        // 优先透传后端 hint（说明到底由谁托管），不给「请升级网关」这类误导文案。
        if (cause instanceof ApiError && cause.status === 501) {
          // 501 响应体带 `hint`：说明该形态下自启到底由谁托管（容器编排 / 宿主 systemd）。
          const hint = pickStr(asRecord(cause.body), 'hint', '');
          return {
            ok: false,
            message: hint
              ? `当前部署形态不支持由网关改写自启：${hint}本次未做任何变更。`
              : '当前部署形态不支持由网关改写自启（自启由宿主托管），本次未做任何变更。',
          };
        }
        return { ok: false, message: describeFailure(cause, '设置开机自启') };
      }
    },
  };
}

// ===========================================================================
// 设置读写（settings）
// ===========================================================================

/** 备份清单行（`GET /api/settings/backups`；字节 / mtime 一律字符串——大数红线）。 */
export interface SettingsBackupRow {
  file: string;
  sizeBytes: string;
  mtimeMs: string;
}

/** 北向出口只读行（settings.network.outlets；password 已脱敏：`<redacted>` 或空）。 */
export interface SettingsOutletRow {
  id: string;
  name: string;
  broker: string;
  topicPrefix: string;
  qos: string;
  tls: boolean;
  encoding: string;
  username: string;
  /** 后端已脱敏："<redacted>"（已配置）或 ""（未配置）；前端不得尝试还原明文 */
  password: string;
}

/** `GET /api/settings` 只读视图。 */
export interface SettingsView {
  basic: { gatewayId: string; dataDir: string };
  oem: { managedBy: string; note: string };
  network: { outlets: SettingsOutletRow[] };
  storage: { sqlitePath: string; maxSizeMb: string; retentionDays: string };
  security: {
    tlsCertPath: string;
    tlsKeyPath: string;
    webAuthEnabled: boolean;
    activationCodeSet: boolean;
    mgmtUsers: { name: string; role: string }[];
  };
  configVersion: string;
}

/** 设置写动作 API（repo.settings.*）。 */
export interface SettingsApi {
  get(): Promise<{ ok: boolean; message: string; view: SettingsView | null }>;
  update(input: {
    actor: string;
    basic?: { gatewayId: string };
    storage?: { sqlitePath?: string; maxSizeMb?: string; retentionDays?: string };
    security?: { webAuthEnabled?: boolean; tlsCertPath?: string; tlsKeyPath?: string };
  }): Promise<{ ok: boolean; message: string; configVersion?: string; backup?: string }>;
  backups(): Promise<{ ok: boolean; message: string; rows: SettingsBackupRow[] }>;
  /** 备份策略读取（`GET /api/settings/backup-policy`；未上线 → ok:false + policy:null）。 */
  getBackupPolicy(): Promise<{ ok: boolean; message: string; policy: BackupPolicy | null }>;
  /** 备份策略更新（`PUT /api/settings/backup-policy`；patch 只发改动的字段，reason/note 独立下发）。 */
  putBackupPolicy(patch: {
    autoBeforeWrite?: boolean;
    retentionCount?: string;
    intervalMin?: string;
    reason?: string;
    note?: string;
  }): Promise<{ ok: boolean; message: string; policy?: BackupPolicy }>;
  /** 配置回滚。
   *
   * `reason`（原因枚举原文）与 `note`（补充说明）为**两个独立字段**，本层原样下发到
   * 请求 body（`withReason` 同口径），**绝不做字符串拼接**。
   */
  rollback(input: { actor: string; reason: string; note?: string; backup?: string }): Promise<{
    ok: boolean;
    message: string;
    restoredFrom?: string;
    version?: string;
  }>;
}

/** 出口行映射（snake_case → camelCase；password 保持脱敏原样）。 */
function mapSettingsOutlet(row: Record<string, unknown>, index: number): SettingsOutletRow {
  return {
    id: pickStr(row, 'id', `outlet-${index}`),
    name: pickStr(row, 'name', ''),
    broker: pickStr(row, 'broker', ''),
    topicPrefix: pickStr(row, 'topic_prefix', ''),
    qos: pickStr(row, 'qos', '0'),
    tls: pickBool(row, 'tls', false),
    encoding: pickStr(row, 'encoding', 'json'),
    username: pickStr(row, 'username', ''),
    password: pickStr(row, 'password', ''),
  };
}

/** 设置视图映射（全部字段按 wire 形状显式挑取；计数/容量字符串直通）。 */
function mapSettingsView(raw: Record<string, unknown>): SettingsView {
  const basic = asRecord(raw['basic'] ?? {});
  const oem = asRecord(raw['oem'] ?? {});
  const network = asRecord(raw['network'] ?? {});
  const storage = asRecord(raw['storage'] ?? {});
  const security = asRecord(raw['security'] ?? {});
  const outlets = Array.isArray(network['outlets']) ? network['outlets'] : [];
  const users = Array.isArray(security['mgmt_users']) ? security['mgmt_users'] : [];
  return {
    basic: { gatewayId: pickStr(basic, 'gateway_id', ''), dataDir: pickStr(basic, 'data_dir', '') },
    oem: { managedBy: pickStr(oem, 'managed_by', ''), note: pickStr(oem, 'note', '') },
    network: { outlets: outlets.map((row, i) => mapSettingsOutlet(asRecord(row), i)) },
    storage: {
      sqlitePath: pickStr(storage, 'sqlite_path', ''),
      maxSizeMb: pickStr(storage, 'max_size_mb', ''),
      retentionDays: pickStr(storage, 'retention_days', ''),
    },
    security: {
      tlsCertPath: pickStr(security, 'tls_cert_path', ''),
      tlsKeyPath: pickStr(security, 'tls_key_path', ''),
      webAuthEnabled: pickBool(security, 'web_auth_enabled', false),
      activationCodeSet: pickBool(security, 'activation_code_set', false),
      mgmtUsers: users.map((row) => {
        const rec = asRecord(row);
        return { name: pickStr(rec, 'name', ''), role: pickStr(rec, 'role', '') };
      }),
    },
    configVersion: pickStr(raw, 'config_version', ''),
  };
}

/** 设置读写实现。 */
function buildSettings(): SettingsApi {
  return {
    async get(): Promise<{ ok: boolean; message: string; view: SettingsView | null }> {
      try {
        const raw = await apiRequest<Record<string, unknown>>('/api/settings');
        return { ok: true, message: 'ok', view: mapSettingsView(asRecord(raw)) };
      } catch (cause) {
        return { ok: false, message: describeFailure(cause, '设置读取'), view: null };
      }
    },

    async update(input): Promise<{ ok: boolean; message: string; configVersion?: string; backup?: string }> {
      // 组装白名单 body（camelCase → snake_case；只发出现的字段，未知键后端 400）。
      const body: Record<string, unknown> = {};
      if (input.basic) {
        body['basic'] = { gateway_id: input.basic.gatewayId };
      }
      if (input.storage) {
        const storage: Record<string, unknown> = {};
        if (input.storage.sqlitePath !== undefined) storage['sqlite_path'] = input.storage.sqlitePath;
        if (input.storage.maxSizeMb !== undefined) storage['max_size_mb'] = input.storage.maxSizeMb;
        if (input.storage.retentionDays !== undefined) storage['retention_days'] = input.storage.retentionDays;
        body['storage'] = storage;
      }
      if (input.security) {
        const security: Record<string, unknown> = {};
        if (input.security.webAuthEnabled !== undefined) security['web_auth_enabled'] = input.security.webAuthEnabled;
        if (input.security.tlsCertPath !== undefined) security['tls_cert_path'] = input.security.tlsCertPath;
        if (input.security.tlsKeyPath !== undefined) security['tls_key_path'] = input.security.tlsKeyPath;
        body['security'] = security;
      }
      try {
        const raw = await apiRequest<Record<string, unknown>>('/api/settings', {
          method: 'PUT',
          body: JSON.stringify(body),
        });
        const configVersion = typeof raw['config_version'] === 'string' ? raw['config_version'] : undefined;
        const backup = typeof raw['backup'] === 'string' ? raw['backup'] : undefined;
        return { ok: true, message: '设置已保存：写前自动备份，热重载即时生效。', configVersion, backup };
      } catch (cause) {
        if (cause instanceof ApiError && cause.status === 403) {
          return { ok: false, message: '权限不足（403）：设置写入仅限 system 角色。' };
        }
        if (cause instanceof ApiError && cause.status === 400) {
          const reason = validationReason(cause);
          return { ok: false, message: `网关拒绝（400）：${reason || '字段校验失败，请检查输入。'}` };
        }
        return { ok: false, message: describeFailure(cause, '设置保存') };
      }
    },

    async backups(): Promise<{ ok: boolean; message: string; rows: SettingsBackupRow[] }> {
      try {
        const raw = await apiRequest<unknown[]>('/api/settings/backups');
        const rows = (Array.isArray(raw) ? raw : []).map((row, i) => {
          const rec = asRecord(row);
          return {
            file: pickStr(rec, 'file', `bak-${i}`),
            sizeBytes: pickStr(rec, 'size_bytes', '0'),
            mtimeMs: pickStr(rec, 'mtime_ms', '0'),
          };
        });
        return { ok: true, message: 'ok', rows };
      } catch (cause) {
        return { ok: false, message: describeFailure(cause, '备份清单读取'), rows: [] };
      }
    },

    async rollback(input): Promise<{ ok: boolean; message: string; restoredFrom?: string; version?: string }> {
      try {
        // 危险三要素：reason / note / backup 各自独立入 body（后端 `RollbackBody` 现只
        // 解析 backup / reason，`note` 暂被忽略——见 lead → Rust agent 的转交项）。
        const body: Record<string, unknown> = { reason: input.reason };
        if (input.note !== undefined) {
          body['note'] = input.note;
        }
        if (input.backup !== undefined) {
          body['backup'] = input.backup;
        }
        const raw = await apiRequest<Record<string, unknown>>('/api/settings/rollback', {
          method: 'POST',
          body: JSON.stringify(body),
        });
        const restoredFrom = typeof raw['restored_from'] === 'string' ? raw['restored_from'] : '';
        const version = typeof raw['config_version'] === 'string' ? raw['config_version'] : '';
        return {
          ok: true,
          message: '回滚成功：当前配置已在回滚前自动再备份（可逆），热重载即时生效。',
          restoredFrom: restoredFrom || undefined,
          version: version || undefined,
        };
      } catch (cause) {
        if (cause instanceof ApiError && cause.status === 403) {
          return { ok: false, message: '权限不足（403），配置回滚仅限 system 角色执行。' };
        }
        if (cause instanceof ApiError && cause.status === 404) {
          return {
            ok: false,
            message:
              '网关无可回滚备份（404 no_backup）：config 目录内不存在 config.toml.bak-* 备份。每次配置写入前网关才会生成写前备份。',
          };
        }
        if (cause instanceof ApiError && cause.status === 400) {
          const reason = validationReason(cause);
          return { ok: false, message: `网关拒绝（400）：${reason || '备份名校验失败。'}` };
        }
        return { ok: false, message: describeFailure(cause, '配置回滚') };
      }
    },

    async getBackupPolicy(): Promise<{ ok: boolean; message: string; policy: BackupPolicy | null }> {
      try {
        // wire：{auto_before_write, retention_count, interval_min, source}；两个计数
        // 为**字符串数字**（大数红线）——pickStr 透传，绝不 parseInt。
        const raw = await apiRequest<Record<string, unknown>>('/api/settings/backup-policy');
        return {
          ok: true,
          message: 'ok',
          policy: {
            autoBeforeWrite: pickBool(raw, 'autoBeforeWrite', pickBool(raw, 'auto_before_write', false)),
            retentionCount: pickStr(raw, 'retentionCount', pickStr(raw, 'retention_count', '')),
            intervalMin: pickStr(raw, 'intervalMin', pickStr(raw, 'interval_min', '')),
            source: pickStr(raw, 'source', ''),
          },
        };
      } catch (cause) {
        if (cause instanceof ApiError && cause.status === 404) {
          return { ok: false, message: '后端备份策略端点尚未上线（GET /api/settings/backup-policy 404）。', policy: null };
        }
        return { ok: false, message: describeFailure(cause, '备份策略读取'), policy: null };
      }
    },

    async putBackupPolicy(patch): Promise<{ ok: boolean; message: string; policy?: BackupPolicy }> {
      // 白名单 body：只发改动的字段（snake_case）+ reason / note 独立下发。
      const body: Record<string, unknown> = {};
      if (patch.autoBeforeWrite !== undefined) {
        body['auto_before_write'] = patch.autoBeforeWrite;
      }
      if (patch.retentionCount !== undefined) {
        body['retention_count'] = patch.retentionCount; // 字符串数字透传
      }
      if (patch.intervalMin !== undefined) {
        body['interval_min'] = patch.intervalMin;
      }
      try {
        const raw = await apiRequest<Record<string, unknown>>('/api/settings/backup-policy', {
          method: 'PUT',
          body: JSON.stringify(withReason(body, patch)),
        });
        return {
          ok: true,
          message: '备份策略已保存。',
          policy: {
            autoBeforeWrite: pickBool(raw, 'autoBeforeWrite', pickBool(raw, 'auto_before_write', patch.autoBeforeWrite ?? false)),
            retentionCount: pickStr(raw, 'retentionCount', pickStr(raw, 'retention_count', patch.retentionCount ?? '')),
            intervalMin: pickStr(raw, 'intervalMin', pickStr(raw, 'interval_min', patch.intervalMin ?? '')),
            source: pickStr(raw, 'source', ''),
          },
        };
      } catch (cause) {
        if (cause instanceof ApiError && (cause.status === 404 || cause.status === 405)) {
          return { ok: false, message: '后端备份策略写端点尚未上线（PUT /api/settings/backup-policy 未部署），本次未做任何变更。' };
        }
        if (cause instanceof ApiError && cause.status === 400) {
          const reason = validationReason(cause);
          return { ok: false, message: `网关拒绝（400）：${reason || '备份策略字段校验失败。'}` };
        }
        return { ok: false, message: describeFailure(cause, '备份策略保存') };
      }
    },
  };
}

// ===========================================================================
// 动作型端点（actions）：真实写 / 探测能力
// ===========================================================================

/** 结构化探测结果（`POST /api/devices/test` / `POST /api/forwarders/{id}/test`）。 */
export interface ProbeResult {
  ok: boolean;
  errorKind: string;
  reason: string;
  elapsedMs: string;
  message: string;
}

/** 设备探测入参。 */
export interface DeviceTestInput {
  deviceId?: string;
  protocol?: string;
  address?: string;
  register?: string;
  slave?: string | number;
  timeoutMs?: string | number;
}

/** 点表导入结果（含逐行校验错误；失败零落盘）。 */
export interface ImportResult {
  ok: boolean;
  imported: string;
  replaced: string;
  devices: string[];
  configVersion: string;
  errors: ImportRowError[];
  message: string;
}

/** 导入行级错误（后端「行号 + 原因 + 允许值」硬契约）。 */
export interface ImportRowError {
  line: string;
  reason: string;
  allowed: string;
}

/** 点表导出结果。 */
export interface ExportResult {
  ok: boolean;
  csv: string;
  fileName: string;
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
  /** 导出并触发浏览器下载。 */
  downloadPointsCsv(deviceId?: string): Promise<ExportResult>;
  /** 北向出口列表（`GET /api/forwarders`；`id` = 出口名）。 */
  listForwarders(): Promise<ForwarderRecord[]>;
  /** 出口 TCP 可达性探测（`POST /api/forwarders/{id}/test`，未知出口 404）。 */
  testForwarder(id: string, timeoutMs?: string | number): Promise<ProbeResult>;
  /** 授权状态原始快照（全字段字符串；runtime 未装配 → `unlicensed`）。 */
  licenseStatus(): Promise<LicenseStatusSnapshot>;
  /** 出口登记（后端诚实 501；返回可解释提示，不静默吞错）。 */
  createForwarder(input: { name: string; broker: string; actor: string }): Promise<{ ok: boolean; message: string }>;
  /** 告警规则整体保存（`PUT /api/alerts/rules`；后端「携带即校验」：reason/note/confirm
   *  有任一携带就整组校验，`confirm` 回显目标 = 将被覆盖的全部规则名）。 */
  saveAlarmRules(input: {
    rules: unknown[];
    reason?: string;
    note?: string;
    confirm?: string;
    actor?: string;
  }): Promise<{ ok: boolean; message: string }>;
  /** 授权激活（`POST /api/license/activate`，body `{code, reason}`；未上线 → 诚实失败）。 */
  activateLicense(input: { code: string; reason?: string; actor?: string }): Promise<{ ok: boolean; message: string }>;
  /** 兼容别名（委托 `activateLicense`）。 */
  activate(input: { code: string; actor: string }): Promise<{ ok: boolean; message: string }>;
  /** 各数据源的「不可得原因」（诚实空态 / 404 / 501 / 503 / 网络失败；空串 = 正常）。 */
  notices(): Record<NoticeKey, string>;
}

/** 从后端响应构造探测结果。 */
function probeFromResponse(raw: Record<string, unknown>, okFallback: boolean): ProbeResult {
  const ok = typeof raw['ok'] === 'boolean' ? raw['ok'] : okFallback;
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
    const kind =
      cause.status === 404 ? 'not_found' : cause.status === 403 ? 'denied' : cause.status === 401 ? 'unauthorized' : `http_${cause.status}`;
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

/** 点表 CSV 导出实现（`GET /api/points/export`）。 */
async function exportPointsImpl(deviceId?: string): Promise<ExportResult> {
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
    const message = describeFailure(cause, '点表导出');
    realCache.notices.points = message;
    return { ok: false, csv: '', fileName: '', rowCount: 0, message };
  }
}

/** 从 400 响应体提取逐行导入错误（`{line, reason, allowed}`，行号字符串透传）。 */
function extractImportErrors(cause: unknown): ImportRowError[] {
  if (!(cause instanceof ApiError)) {
    return [];
  }
  const body = asRecord(cause.body);
  const rawErrors = Array.isArray(body['errors']) ? body['errors'] : [];
  return rawErrors.map((item) => {
    const row = asRecord(item);
    return {
      line: pickStr(row, 'line', '0'),
      reason: pickStr(row, 'reason', '原因未给出'),
      allowed: pickStr(row, 'allowed', ''),
    };
  });
}

/** 新激活码格式：`IOT-2026-` + 3 段各 4 位 + 末段 2 位。
 *
 * 字符集**显式枚举** `[ACD-HJ-NP-Z23456789]`，刻意不用否定区间——`[A-Z2-9AC-HJ-NP-Z]`
 * 里外层的 `A-Z` 会把 `B` / `I` / `O` 一并收回去，否定区间形同虚设（2026-09-27 修正）。
 * 取值集合 = 23 个字母（无 `B` / `I` / `O`）+ 8 个数字（无 `0` / `1`）= 31 个，与
 * licensing-server `service::CODE_ALPHABET` 逐字符相等。 */
const ACT_CODE_RE = /^IOT-\d{4}-[ACD-HJ-NP-Z23456789]{4}-[ACD-HJ-NP-Z23456789]{4}-[ACD-HJ-NP-Z23456789]{4}-[ACD-HJ-NP-Z23456789]{2}$/;

/** 旧激活码格式 `IOTDAQ-XXXX-XXXX-XXXX-XXXX`：2026-09-27 之前发放的存量码仍在库中，
 * 后端按码值查库校验（不做格式重算），因此旧码**不做作废**，仍可正常激活；此处仅
 * 在成功路径上给出「已废弃、建议换发新码」的结构化提示。 */
const ACT_CODE_LEGACY_RE = /^IOTDAQ-[A-Z0-9]{4}-[A-Z0-9]{4}-[A-Z0-9]{4}-[A-Z0-9]{4}$/;

/** 授权激活真实现（`POST /api/license/activate`，body `{code, reason}`）。
 *
 * `buildActions().activateLicense`（typed 主入口）与顶层 `repo.activate`（LicensePage
 * 兼容入口，`repo.ts` buildWrites 展开）共用。真实判定在后端：激活码格式前端只做
 * 入口校验（形如 `IOT-2026-XXXX-XXXX-XXXX-XX`，另兼容旧 `IOTDAQ-` 格式）；
 * 成功即重取授权快照（页面经 dataVersion 感知）；失败（含端点未上线，真机实测 404）
 * 一律结构化失败，**绝不让格式校验通过冒充激活成功**。
 */
async function doLicenseActivate(input: { code: string; reason?: string }): Promise<{ ok: boolean; message: string }> {
  const normalized = input.code.trim().toUpperCase();
  if (!normalized) {
    return { ok: false, message: '请输入激活码' };
  }
  const legacyShape = ACT_CODE_LEGACY_RE.test(normalized);
  if (!ACT_CODE_RE.test(normalized) && !legacyShape) {
    return { ok: false, message: '激活码格式不正确，应形如 IOT-2026-XXXX-XXXX-XXXX-XX' };
  }
  try {
    await apiRequest<unknown>('/api/license/activate', {
      method: 'POST',
      body: JSON.stringify({ code: normalized, reason: input.reason ?? '' }),
    });
    await fetchLicense(); // 成功后重取真实授权快照
    return {
      ok: true,
      message: legacyShape
        ? '激活码已提交并生效（旧格式 IOTDAQ- 已废弃，建议向管理员重新发放 IOT-2026- 格式新码）。'
        : '激活码已提交并生效。',
    };
  } catch (cause) {
    if (cause instanceof ApiError && (cause.status === 404 || cause.status === 501)) {
      return { ok: false, message: '后端激活接口尚未上线（POST /api/license/activate 未部署），本次未提交激活码。' };
    }
    if (cause instanceof ApiError && cause.status === 400) {
      const reason = validationReason(cause);
      return { ok: false, message: `网关拒绝（400）：${reason || '激活码校验失败。'}` };
    }
    if (cause instanceof ApiError && cause.status === 403) {
      return { ok: false, message: '权限不足（403）：当前角色不可执行授权激活。' };
    }
    return { ok: false, message: describeFailure(cause, '授权激活') };
  }
}

/** 动作型端点实现。 */
function buildActions(): ActionApi {
  return {
    async testDevice(input: DeviceTestInput): Promise<ProbeResult> {
      try {
        const body: Record<string, unknown> = {};
        if (input.deviceId) {
          body['device_id'] = input.deviceId;
        }
        if (input.protocol) {
          body['protocol'] = input.protocol;
        }
        if (input.address) {
          body['address'] = input.address;
        }
        if (input.register) {
          body['register'] = input.register;
        }
        if (input.slave !== undefined && input.slave !== '') {
          body['slave'] = input.slave;
        }
        if (input.timeoutMs !== undefined && input.timeoutMs !== '') {
          body['timeout_ms'] = input.timeoutMs;
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
        // 导入成功 → 立即刷新真实缓存
        void refresh();
        const imported = pickStr(raw, 'imported', '0');
        return {
          ok: true,
          imported,
          replaced: pickStr(raw, 'replaced', '0'),
          devices: Array.isArray(raw['devices']) ? (raw['devices'] as unknown[]).map((d) => String(d)) : [],
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
      try {
        const raw = await apiRequest<unknown[]>('/api/forwarders');
        return Array.isArray(raw) ? raw.map((row, i) => mapForwarder(asRecord(row), i)) : [];
      } catch (cause) {
        realCache.notices.forwarders = describeFailure(cause, '北向出口');
        return [];
      }
    },

    async testForwarder(id: string, timeoutMs?: string | number): Promise<ProbeResult> {
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
          note: realCache.notices.license || '授权状态暂不可得（授权模块未就绪或请求失败）',
        }
      );
    },

    async createForwarder(input: { name: string; broker: string; actor: string }): Promise<{ ok: boolean; message: string }> {
      try {
        await apiRequest<unknown>('/api/forwarders', {
          method: 'POST',
          body: JSON.stringify({ name: input.name, broker: input.broker }),
        });
        await refresh();
        return { ok: true, message: `出口 ${input.name} 已登记。` };
      } catch (cause) {
        return { ok: false, message: describeFailure(cause, '北向出口登记') };
      }
    },

    async saveAlarmRules(input): Promise<{ ok: boolean; message: string }> {
      try {
        // 后端 `RulesPutBody{rules, reason?, note?, confirm?}`（携带即校验）：
        // reason / note / confirm 独立字段原样下发；不携带则只发 rules（普通保存）。
        await apiRequest<unknown>('/api/alerts/rules', {
          method: 'PUT',
          body: JSON.stringify(withReason({ rules: input.rules }, input)),
        });
        return { ok: true, message: '告警规则已保存。' };
      } catch (cause) {
        return { ok: false, message: describeFailure(cause, `告警规则保存（actor=${input.actor ?? ''}）`) };
      }
    },

    /** 授权激活（`POST /api/license/activate`；真实现 `doLicenseActivate`，未上线 → 诚实失败）。 */
    activateLicense(input: { code: string; reason?: string; actor?: string }): Promise<{ ok: boolean; message: string }> {
      return doLicenseActivate(input);
    },

    /** 兼容别名（委托 `activateLicense`）。 */
    activate(input: { code: string; actor: string }): Promise<{ ok: boolean; message: string }> {
      return doLicenseActivate({ code: input.code });
    },

    notices(): Record<NoticeKey, string> {
      return { ...realCache.notices };
    },
  };
}

// ===========================================================================
// 写操作（设备 / 点位）：真实 HTTP + 刷新，绝不本地拼装
// ===========================================================================

/** 生成设备标识（后端 `POST /api/devices` 要求非空唯一 id）。 */
function deviceIdFrom(input: DeviceDraft): string {
  const explicit = input.id?.trim();
  if (explicit) {
    return explicit;
  }
  const base = slug(input.name, 'gw');
  return `dev-${base}-${String(Date.now()).slice(-6)}`;
}

/** 点位协议：取所属设备的协议（后端 `POST /api/points` 需要）；未知回退 modbus-tcp。 */
function protocolOfDevice(deviceId: string): ProtocolType {
  const device = realCache.devices.find((d) => d.id === deviceId);
  return device?.protocol ?? 'modbus-tcp';
}

/** 点位频率：取所属设备的采集间隔（未知回退 1000ms）。 */
function frequencyOfDevice(deviceId: string): number {
  const device = realCache.devices.find((d) => d.id === deviceId);
  return device && device.intervalMs > 0 ? device.intervalMs : 1000;
}

/** 生成点位标识（后端 point_id：物理点取地址，计算点取目标点名）。 */
function pointIdFrom(input: PointDraft): string {
  const addr = input.address.trim();
  if (input.pointType !== 'derived' && addr && addr !== '—') {
    return addr;
  }
  return input.targetKey.trim() || `pt-${String(Date.now()).slice(-6)}`;
}

/** 点位写请求体（含本次新增字段：name / data_type / byte_order / unit / deadband /
 *  target_key / point_type / push_enabled / formula / endpoint）。 */
function pointBody(input: PointDraft, deviceId: string, pointId: string): Record<string, unknown> {
  const isDerived = input.pointType === 'derived';
  const body: Record<string, unknown> = {
    device_id: deviceId,
    point_id: pointId,
    protocol: protocolOfDevice(deviceId),
    address: isDerived ? '' : input.address.trim(),
    frequency_ms: String(frequencyOfDevice(deviceId)),
    name: input.name.trim(),
    data_type: input.dataType,
    byte_order: isDerived ? '—' : input.byteOrder,
    unit: input.unit,
    deadband: input.deadband,
    target_key: input.targetKey.trim(),
    point_type: input.pointType,
    push_enabled: input.pushEnabled ?? true,
    formula: isDerived ? (input.formula ?? '') : '',
  };
  // 设备接入端点（V1 正例键，`address` 退为别名）：UI 有值才透传，缺省保持
  // address 别名语义（后端 `endpoint` 缺省回退 `address`，行为不变）。
  if (input.endpoint !== undefined && input.endpoint.trim() !== '') {
    body['endpoint'] = input.endpoint.trim();
  }
  return body;
}

/** 从 400 错误体构建设备/点位写失败的可解释原因。 */
function writeFailure(cause: unknown, subject: string): WriteResult<never> {
  if (cause instanceof ApiError && cause.status === 400) {
    const reason = validationReason(cause);
    return fail<never>(`网关拒绝（400）：${reason || extractErrorMessage(cause.body).replace(/^——/, '') || '参数校验失败'}`);
  }
  return fail<never>(describeFailure(cause, subject));
}

/**
 * 危险写操作的通用可选元信息（**两段独立输入，禁止拼接**）。
 *
 *  · `reason`  —— 危险弹窗的「原因」枚举原文；
 *  · `note`    —— 危险弹窗的「补充说明」原文（≥10 字，独立字段）；
 *  · `confirm` —— 二次确认值。**各域 wire 口径不同，以后端执行点为准**：
 *       - 角色 / 账号 / 转发规则 / 告警规则 → 对象**全名原文**（后端 trim + 大小写
 *         不敏感精确回显匹配，不匹配 400 `confirm_mismatch`）；
 *       - 运维 restart / stop → 当前 **gateway_id** 原文（同上回显匹配）；
 *       - **分组删除例外 → 恒为布尔 `true`**（`groups.rs` 只认 `Some(true)`，
 *         repo 层无条件归一，见 `buildGroups().remove`）。
 *
 * 三者在 repo 层一律**原样下发**到请求 body，供后端写结构化审计；唯一例外即
 * 上条分组删除的布尔归一。
 */
export interface WriteMeta {
  reason?: string;
  note?: string;
  confirm?: string;
}

/**
 * 把可选 `reason` / `note` / `confirm` **原样写入**写请求 body（有值才带，绝不 trim、
 * 绝不拼接）。
 *
 * 危险写操作红线：后端需要 `reason`（原因枚举）、`note`（补充说明）、`confirm`
 * （对象名二次校验回显）分别入审计；前端只负责忠实下发。
 */
function withReason(body: Record<string, unknown>, input: WriteMeta): Record<string, unknown> {
  if (input.reason !== undefined) {
    body['reason'] = input.reason;
  }
  if (input.note !== undefined) {
    body['note'] = input.note;
  }
  if (input.confirm !== undefined) {
    body['confirm'] = input.confirm;
  }
  return body;
}

/** 写操作实现（真实 HTTP；成功后 `refresh()`）。 */
function buildWrites() {
  return {
    // ---------- 设备 ----------
    async createDevice(input: DeviceDraft): Promise<WriteResult<DeviceRecord>> {
      const id = deviceIdFrom(input);
      try {
        const body: Record<string, unknown> = {
          id,
          name: input.name.trim(),
          protocol: input.protocol,
          enabled: true,
        };
        if (input.groupId) {
          body['group_id'] = input.groupId;
        }
        await apiRequest<unknown>('/api/devices', { method: 'POST', body: JSON.stringify(body) });
        await refresh();
        const created = realCache.devices.find((d) => d.id === id) ?? null;
        return { ok: true, message: `设备「${input.name.trim()}」已登记（id=${id}）。`, data: created ?? undefined };
      } catch (cause) {
        return writeFailure(cause, '新增设备');
      }
    },

    async updateDevice(input: DeviceDraft & { id: string }): Promise<WriteResult<DeviceRecord>> {
      const id = input.id.trim();
      if (!id) {
        return fail('缺少设备 id，无法更新。');
      }
      try {
        const body: Record<string, unknown> = {};
        if (input.name.trim()) {
          body['name'] = input.name.trim();
        }
        if (input.protocol) {
          body['protocol'] = input.protocol;
        }
        if (input.groupId !== undefined) {
          body['group_id'] = input.groupId;
        }
        await apiRequest<unknown>(`/api/devices/${encodeURIComponent(id)}`, {
          method: 'PUT',
          body: JSON.stringify(body),
        });
        await refresh();
        const updated = realCache.devices.find((d) => d.id === id) ?? null;
        return { ok: true, message: `设备 ${id} 已更新。`, data: updated ?? undefined };
      } catch (cause) {
        return writeFailure(cause, '更新设备');
      }
    },

    /**
     * 删除设备：`DELETE /api/devices/:id`（P0-8 危险操作三要素）。
     *
     * 以后端执行点 `crates/daemon/src/mgmt/writeapi.rs::device_delete` 为准：
     * · **body 必填 JSON 对象** `{reason, note, confirm}`（空 body / 非对象 → 400
     *   `validation_failed`），故本函数无条件下发，绝不省略 body；
     * · `reason` 必填非空；`note` 非空则 ≥10 字（`MIN_NOTE_CHARS`）；
     * · `confirm` 须回显**设备名原文**（`trim()` + 大小写不敏感精确匹配）；
     * · `cascade=true`：设备仍有点位时后端默认 400 `device_has_points`（fail-closed），
     *   页面影响清单已明示「连同点位一并删除」，故默认显式级联以与 UI 承诺一致。
     */
    async deleteDevice(input: {
      id: string;
      /** 设备名（仅用于成功回执文案；`confirm` 比对由页面与后端各自完成） */
      name?: string;
      cascade?: boolean;
      actor: string;
    } & WriteMeta): Promise<WriteResult> {
      const id = input.id.trim();
      if (!id) {
        return fail('缺少设备 id，无法删除。');
      }
      try {
        const query = input.cascade === false ? '' : '?cascade=true';
        const body = withReason({ id, actor: input.actor ?? DEFAULT_ACTOR }, input);
        const raw = await apiRequest<Record<string, unknown>>(
          `/api/devices/${encodeURIComponent(id)}${query}`,
          { method: 'DELETE', body: JSON.stringify(body) },
        );
        await refresh();
        const removed = pickStr(raw, 'deleted_points', '');
        return {
          ok: true,
          message: `设备「${input.name || id}」已删除${removed !== '' ? `（含 ${removed} 个点位）` : ''}。`,
        };
      } catch (cause) {
        return writeFailure(cause, '删除设备');
      }
    },

    // ---------- 点位 ----------
    async createPoint(input: PointDraft): Promise<WriteResult<PointRecord>> {
      const pointId = pointIdFrom(input);
      try {
        await apiRequest<unknown>('/api/points', {
          method: 'POST',
          body: JSON.stringify(pointBody(input, input.deviceId, pointId)),
        });
        await refresh();
        const created = realCache.points.find((p) => p.deviceId === input.deviceId && p.id === pointId) ?? null;
        return { ok: true, message: `点位「${input.name.trim()}」已新增（id=${pointId}）。`, data: created ?? undefined };
      } catch (cause) {
        return writeFailure(cause, '新增点位');
      }
    },

    async updatePoint(input: PointDraft & { id: string }): Promise<WriteResult<PointRecord>> {
      const pointId = input.id.trim();
      if (!pointId) {
        return fail('缺少点位 id，无法更新。');
      }
      const body = pointBody(input, input.deviceId, pointId);
      delete body['device_id'];
      delete body['point_id'];
      try {
        await apiRequest<unknown>(
          `/api/points/${encodeURIComponent(input.deviceId)}/${encodeURIComponent(pointId)}`,
          { method: 'PUT', body: JSON.stringify(body) },
        );
        await refresh();
        const updated = realCache.points.find((p) => p.deviceId === input.deviceId && p.id === pointId) ?? null;
        return { ok: true, message: `点位 ${pointId} 已更新。`, data: updated ?? undefined };
      } catch (cause) {
        return writeFailure(cause, '更新点位');
      }
    },

    /**
     * 删除点位：`DELETE /api/points/:deviceId/:pointId`（P0-8 危险操作三要素）。
     *
     * 以后端执行点 `crates/daemon/src/mgmt/writeapi.rs::point_delete` 为准：
     * · **body 必填 JSON 对象** `{reason, note, confirm}`（空 body / 非对象 → 400
     *   `validation_failed`），故本函数无条件下发，绝不省略 body；
     * · `reason` 必填非空；`note` 非空则 ≥10 字（`MIN_NOTE_CHARS`）；
     * · `confirm` 须回显**点位 id 原文**（`trim()` + 大小写不敏感精确匹配），
     *   后端比对目标是 URL 上的 `point_id`，不匹配 → 400 `confirm_mismatch`；
     * · 后端先校验再 `config.points.remove`，**落盘前拒绝，无半改状态**；
     * · 成功回 `{accepted, deleted, device_id, point_id, config_version}`。
     *
     * ⚠️ 本函数**不做任何归一 / 兜底**：`withReason` 只原样透传，比对口径一律以后端为准。
     */
    async deletePoint(input: { id: string; actor: string } & WriteMeta): Promise<WriteResult> {
      const target = realCache.points.find((p) => p.id === input.id);
      if (!target) {
        return fail(`点位 ${input.id} 不在当前缓存中，无法确定所属设备；请刷新后重试。`);
      }
      try {
        const body = withReason({ actor: input.actor ?? DEFAULT_ACTOR }, input);
        await apiRequest<unknown>(
          `/api/points/${encodeURIComponent(target.deviceId)}/${encodeURIComponent(input.id)}`,
          { method: 'DELETE', body: JSON.stringify(body) },
        );
        await refresh();
        return { ok: true, message: `点位 ${input.id} 已删除。` };
      } catch (cause) {
        return writeFailure(cause, '删除点位');
      }
    },

    async replacePointsOfDevice(input: { deviceId: string; rows: PointDraft[]; actor: string }): Promise<WriteResult<{ imported: number }>> {
      const protocol = protocolOfDevice(input.deviceId);
      const frequency = String(frequencyOfDevice(input.deviceId));
      const header = 'device_id,point_id,protocol,address,frequency_ms,push';
      const lines = [header];
      for (const row of input.rows) {
        const isDerived = row.pointType === 'derived';
        const pid = pointIdFrom(row);
        const addr = isDerived ? '' : row.address.trim();
        const csvCell = (v: string): string => (/[",\n]/.test(v) ? `"${v.replace(/"/g, '""')}"` : v);
        lines.push(
          [input.deviceId, pid, protocol, csvCell(addr), frequency, row.pushEnabled ?? true ? '1' : '0'].join(','),
        );
      }
      const csv = lines.join('\n');
      try {
        const raw = await apiRequest<Record<string, unknown>>(
          `/api/points/import?device_id=${encodeURIComponent(input.deviceId)}&replace=true`,
          { method: 'POST', body: JSON.stringify({ csv, device_id: input.deviceId, replace: true }) },
        );
        await refresh();
        const imported = pickCountFromText(pickStr(raw, 'imported', String(input.rows.length)));
        return {
          ok: true,
          message: `已覆盖导入 ${imported} 个点位并热生效。`,
          data: { imported },
        };
      } catch (cause) {
        const errors = extractImportErrors(cause);
        if (errors.length > 0) {
          const first = errors[0];
          return fail(
            `导入被整批拒绝（零落盘）：${errors.length} 行不合法；首条 第 ${first.line} 行 —— ${first.reason}${
              first.allowed ? `（允许值：${first.allowed}）` : ''
            }`,
          );
        }
        return writeFailure(cause, '覆盖导入点表');
      }
    },

    // ---------- 北向出口编码（后端尚无独立写接口）----------
    async setForwarderEncoding(input: { id: string; encoding: Encoding; actor: string }): Promise<WriteResult> {
      try {
        await apiRequest<unknown>(`/api/forwarders/${encodeURIComponent(input.id)}`, {
          method: 'PUT',
          body: JSON.stringify({ encoding: input.encoding }),
        });
        await refresh();
        return { ok: true, message: `出口 ${input.id} 编码已更新为 ${input.encoding}。` };
      } catch (cause) {
        if (cause instanceof ApiError && (cause.status === 404 || cause.status === 405)) {
          return fail(`后端未提供北向出口写接口（${input.id}）：出口编码修改能力未落地，本次未做任何变更。`);
        }
        return writeFailure(cause, '修改北向编码');
      }
    },

    // ---------- 转发规则启停（`repo.rules.setEnabled`，见 `RuleApi.setEnabled`）----------

    // ---------- 告警处置（真实 `POST /api/alerts/:id/ack`）----------
    /**
     * 告警处置（`open` → `acking` / `resolved`）。
     *
     * wire（`alerts_api.rs AckBody`）：`{state, note, reason, confirm}` —— 四要素
     * **强制**、fail-closed：`confirm` 必须 = **告警 id 原文**（trim + 大小写不敏感
     * 精确匹配，空串同样算不匹配 → 400 `confirm_mismatch`）。repo 持有 id，调用方
     * 漏传 `confirm` 时归一为 `input.id`（与分组删除布尔 true 同类的 repo 收口）；
     * `reason` / `note` 原样透传、两字段独立不拼接。成功后重取告警真实状态。
     */
    async resolveAlarm(
      input: {
        id: string;
        state: AlarmState;
        note: string;
        actor: string;
        reason?: string;
        confirm?: string;
      },
      /**
       * `refetch`（缺省 true）：成功后重取 `GET /api/alerts` 并 **bump `dataVersion`**
       * （`watch(dataVersion)` 的页面由此自动刷新；此前只重取缓存不 bump，watch 不触发）。
       * 批量处置请逐条传 `{ refetch: false }`，循环结束后统一调一次 `refreshAlerts()`，
       * 避免 N 条处置 = N 次清单 GET。
       */
      opts?: { refetch?: boolean },
    ): Promise<WriteResult> {
      const target = input.state === 'acking' || input.state === 'resolved' ? input.state : '';
      if (!target) {
        return fail(`未知的处置状态 "${String(input.state)}"（允许 "acking" | "resolved"）。`);
      }
      try {
        await apiRequest<unknown>(`/api/alerts/${encodeURIComponent(input.id)}/ack`, {
          method: 'POST',
          body: JSON.stringify({
            state: target,
            note: input.note,
            reason: input.reason ?? '',
            confirm: input.confirm ?? input.id,
          }),
        });
        if (opts?.refetch ?? true) {
          await fetchAlerts(); // 处置结论在告警内存环，重取真实状态（禁止本地拼装）
          bumpCacheVersion(); // 通知 watch(dataVersion) 的页面（fetch* 本身不 bump）
        }
        return { ok: true, message: `告警 ${input.id} 已置为 ${target === 'acking' ? '处置中' : '已处置'}。` };
      } catch (cause) {
        if (cause instanceof ApiError && cause.status === 403) {
          return fail('权限不足（403）：当前角色不可执行告警处置（需 device.write）。');
        }
        return writeFailure(cause, '告警处置');
      }
    },

    // ---------- 授权触点（客户端仅展示 + 提交，真实判定在后端）----------
    /** 兼容入口：LicensePage 走顶层 `repo.activate(...)`（真实现见 `doLicenseActivate`）。 */
    async activate(input: { code: string; actor: string }): Promise<{ ok: boolean; message: string }> {
      return doLicenseActivate({ code: input.code });
    },

    async submitTransferRequest(input: {
      oldMachineCode: string;
      newMachineCode: string;
      reason: string;
      contact: string;
      actor: string;
    }): Promise<{ ok: boolean; ticketId: string; message: string }> {
      if (!input.reason.trim()) {
        return { ok: false, ticketId: '', message: '请填写换机原因' };
      }
      return {
        ok: false,
        ticketId: '',
        message: '后端未提供换机申请接口（未落地），本次未提交申请。',
      };
    },

    /**
     * 记录「复制机器码」前端动作。
     *
     * 后端无对应审计端点，故**不伪造审计条目**；保留该符号以兼容既有页面调用。
     */
    logCopyMachineCode(input: { actor: string }): void {
      void input;
    },
  };
}

// ===========================================================================
// 命名空间：分组 / 角色 / 权限 / 账号（后端未落地时诚实空态 + notices）
// ===========================================================================

/** 分组 / 角色 / 账号写接口。
 *
 * 危险写操作（删除 / 角色改权 / 口令重置等）遵循项目红线「二次确认 + 原因必填 +
 * 补充说明 + 写审计」：`reason`（原因枚举原文）、`note`（补充说明原文，与 reason
 * **独立不拼接**）、`confirm` 为**可选**入参，有值时由 repo **原样下发到请求
 * body**（后端据此写结构化审计），不作任何吞并。
 *
 * ⚠️ 唯一例外：`GroupApi.remove` 的 `confirm` 由 repo **无条件归一为布尔 `true`**
 * （后端 `DELETE /api/groups/:id` 的硬契约，见 `buildGroups().remove` 注释），
 * 调用方传入的字符串 confirm 不会到达 wire。
 */
export interface GroupApi {
  list(): Promise<DeviceGroup[]>;
  create(input: { name: string; actor?: string } & WriteMeta): Promise<WriteResult<DeviceGroup>>;
  update(input: { id: string; name: string; actor?: string } & WriteMeta): Promise<WriteResult<DeviceGroup>>;
  remove(input: { id: string; actor?: string } & WriteMeta): Promise<WriteResult>;
}

export interface RoleApi {
  list(): Promise<RoleRecord[]>;
  create(input: { name: string; permissions: string[]; actor?: string } & WriteMeta): Promise<WriteResult<RoleRecord>>;
  update(input: {
    id: string;
    name?: string;
    permissions?: string[];
    actor?: string;
  } & WriteMeta): Promise<WriteResult<RoleRecord>>;
  remove(input: { id: string; actor?: string } & WriteMeta): Promise<WriteResult>;
}

export interface AccountApi {
  list(): Promise<AccountRecord[]>;
  create(input: {
    account: string;
    role: string;
    password: string;
    actor?: string;
  } & WriteMeta): Promise<WriteResult<AccountRecord>>;
  update(input: {
    account: string;
    role?: string;
    status?: string;
    actor?: string;
  } & WriteMeta): Promise<WriteResult<AccountRecord>>;
  remove(input: { account: string; actor?: string } & WriteMeta): Promise<WriteResult>;
  resetPassword(input: { account: string; newPassword: string; actor?: string } & WriteMeta): Promise<WriteResult>;
}

/** 拉取设备分组清单（失败 → 空 + notices）。 */
async function loadGroups(): Promise<DeviceGroup[]> {
  try {
    const raw = await apiRequest<unknown[]>('/api/groups');
    realCache.groups = Array.isArray(raw) ? raw.map((row, i) => mapGroup(asRecord(row), i)) : [];
    realCache.notices.groups = '';
  } catch (cause) {
    realCache.groups = [];
    realCache.notices.groups = describeFailure(cause, '设备分组');
  }
  return realCache.groups;
}

/** 拉取角色清单（失败 → 空 + notices）。 */
async function loadRoles(): Promise<RoleRecord[]> {
  try {
    const raw = await apiRequest<unknown[]>('/api/roles');
    realCache.roles = Array.isArray(raw) ? raw.map((row, i) => mapRole(asRecord(row), i)) : [];
    realCache.notices.roles = '';
  } catch (cause) {
    realCache.roles = [];
    realCache.notices.roles = describeFailure(cause, '角色清单');
  }
  return realCache.roles;
}

/** 拉取账号清单（失败 → 空 + notices）。 */
async function loadAccounts(): Promise<AccountRecord[]> {
  try {
    const raw = await apiRequest<unknown[]>('/api/accounts');
    realCache.accounts = Array.isArray(raw) ? raw.map((row, i) => mapAccount(asRecord(row), i)) : [];
    realCache.notices.accounts = '';
  } catch (cause) {
    realCache.accounts = [];
    realCache.notices.accounts = describeFailure(cause, '账号清单');
  }
  return realCache.accounts;
}

/** 分组读写实现。 */
function buildGroups(): GroupApi {
  return {
    list(): Promise<DeviceGroup[]> {
      return loadGroups();
    },

    async create(input): Promise<WriteResult<DeviceGroup>> {
      try {
        const raw = await apiRequest<Record<string, unknown>>('/api/groups', {
          method: 'POST',
          body: JSON.stringify(withReason({ name: input.name, actor: input.actor ?? DEFAULT_ACTOR }, input)),
        });
        await loadGroups();
        return { ok: true, message: `分组「${input.name}」已创建。`, data: mapGroup(asRecord(raw), 0) };
      } catch (cause) {
        return writeFailure(cause, '新增分组');
      }
    },

    async update(input): Promise<WriteResult<DeviceGroup>> {
      try {
        const raw = await apiRequest<Record<string, unknown>>(`/api/groups/${encodeURIComponent(input.id)}`, {
          method: 'PUT',
          body: JSON.stringify(withReason({ name: input.name, actor: input.actor ?? DEFAULT_ACTOR }, input)),
        });
        await loadGroups();
        return { ok: true, message: `分组 ${input.id} 已更新。`, data: mapGroup(asRecord(raw), 0) };
      } catch (cause) {
        return writeFailure(cause, '更新分组');
      }
    },

    async remove(input): Promise<WriteResult> {
      try {
        const body = withReason({ id: input.id, actor: input.actor ?? DEFAULT_ACTOR }, input);
        // ⚠️ 分组删除的 wire 契约（`groups.rs GroupDeleteBody`）：`confirm` 是**布尔
        // `true`**（「显式确认」语义，后端对 `confirm=false` 也显式拒绝），**不是**
        // 角色 / 账号 / 规则域的「对象全名原文」。team-lead 裁决：以后端执行点为准，
        // 前端弹窗继续要求输入组名原文作为交互层拦截，repo 在此**无条件归一**为
        // `confirm: true` 下发——调用方传不传、传什么，都不影响 wire 值。
        // 勿按旧长期记忆条目「分组域 = 全名原文」回改本行（那会让删除 100% 400）。
        body['confirm'] = true;
        await apiRequest<unknown>(`/api/groups/${encodeURIComponent(input.id)}`, {
          method: 'DELETE',
          body: JSON.stringify(body),
        });
        await loadGroups();
        return { ok: true, message: `分组 ${input.id} 已删除。` };
      } catch (cause) {
        return writeFailure(cause, '删除分组');
      }
    },
  };
}

/** 角色读写实现。 */
function buildRoles(): RoleApi {
  return {
    list(): Promise<RoleRecord[]> {
      return loadRoles();
    },

    async create(input): Promise<WriteResult<RoleRecord>> {
      try {
        const raw = await apiRequest<Record<string, unknown>>('/api/roles', {
          method: 'POST',
          body: JSON.stringify(
            withReason({ name: input.name, permissions: input.permissions, actor: input.actor ?? DEFAULT_ACTOR }, input),
          ),
        });
        await loadRoles();
        return { ok: true, message: `角色「${input.name}」已创建。`, data: mapRole(asRecord(raw), 0) };
      } catch (cause) {
        return writeFailure(cause, '新增角色');
      }
    },

    async update(input): Promise<WriteResult<RoleRecord>> {
      const body: Record<string, unknown> = { actor: input.actor ?? DEFAULT_ACTOR };
      if (input.name !== undefined) {
        body['name'] = input.name;
      }
      if (input.permissions !== undefined) {
        body['permissions'] = input.permissions;
      }
      try {
        const raw = await apiRequest<Record<string, unknown>>(`/api/roles/${encodeURIComponent(input.id)}`, {
          method: 'PUT',
          body: JSON.stringify(withReason(body, input)),
        });
        await loadRoles();
        return { ok: true, message: `角色 ${input.id} 已更新。`, data: mapRole(asRecord(raw), 0) };
      } catch (cause) {
        return writeFailure(cause, '更新角色');
      }
    },

    async remove(input): Promise<WriteResult> {
      try {
        await apiRequest<unknown>(`/api/roles/${encodeURIComponent(input.id)}`, {
          method: 'DELETE',
          body: JSON.stringify(withReason({ id: input.id, actor: input.actor ?? DEFAULT_ACTOR }, input)),
        });
        await loadRoles();
        return { ok: true, message: `角色 ${input.id} 已删除。` };
      } catch (cause) {
        return writeFailure(cause, '删除角色');
      }
    },
  };
}

/** 账号读写实现。 */
function buildAccounts(): AccountApi {
  return {
    list(): Promise<AccountRecord[]> {
      return loadAccounts();
    },

    async create(input): Promise<WriteResult<AccountRecord>> {
      try {
        const raw = await apiRequest<Record<string, unknown>>('/api/accounts', {
          method: 'POST',
          body: JSON.stringify(
            withReason(
              {
                account: input.account,
                role: input.role,
                password: input.password,
                actor: input.actor ?? DEFAULT_ACTOR,
              },
              input,
            ),
          ),
        });
        await loadAccounts();
        return { ok: true, message: `账号「${input.account}」已创建。`, data: mapAccount(asRecord(raw), 0) };
      } catch (cause) {
        return writeFailure(cause, '新增账号');
      }
    },

    async update(input): Promise<WriteResult<AccountRecord>> {
      const body: Record<string, unknown> = { actor: input.actor ?? DEFAULT_ACTOR };
      if (input.role !== undefined) {
        body['role'] = input.role;
      }
      if (input.status !== undefined) {
        body['status'] = input.status;
      }
      try {
        const raw = await apiRequest<Record<string, unknown>>(`/api/accounts/${encodeURIComponent(input.account)}`, {
          method: 'PUT',
          body: JSON.stringify(withReason(body, input)),
        });
        await loadAccounts();
        return { ok: true, message: `账号 ${input.account} 已更新。`, data: mapAccount(asRecord(raw), 0) };
      } catch (cause) {
        return writeFailure(cause, '更新账号');
      }
    },

    async remove(input): Promise<WriteResult> {
      try {
        await apiRequest<unknown>(`/api/accounts/${encodeURIComponent(input.account)}`, {
          method: 'DELETE',
          body: JSON.stringify(withReason({ account: input.account, actor: input.actor ?? DEFAULT_ACTOR }, input)),
        });
        await loadAccounts();
        return { ok: true, message: `账号 ${input.account} 已删除。` };
      } catch (cause) {
        return writeFailure(cause, '删除账号');
      }
    },

    async resetPassword(input): Promise<WriteResult> {
      try {
        // 复用账号更新端点（PUT /api/accounts/:account）携带 password 字段。
        await apiRequest<unknown>(`/api/accounts/${encodeURIComponent(input.account)}`, {
          method: 'PUT',
          body: JSON.stringify(
            withReason({ password: input.newPassword, actor: input.actor ?? DEFAULT_ACTOR }, input),
          ),
        });
        return { ok: true, message: `账号 ${input.account} 口令已重置。` };
      } catch (cause) {
        return writeFailure(cause, '重置口令');
      }
    },
  };
}

// ===========================================================================
// 转发规则读写（/api/rules 系列 —— 与告警规则端点 /api/alerts/rules 无关）
// ===========================================================================

/** 转发规则新增 / 编辑入参（冻结契约：`POST /api/rules` 与 `PUT /api/rules/:id` body 的可选子集）。 */
export type RuleUpsertInput = {
  /** 规则 id（新增缺省时由 repo 生成；编辑时即路径参数）。 */
  id?: string;
  name?: string;
  /** 生效出口 id（wire: `forwarder_id`）。 */
  forwarderId?: string;
  enabled?: boolean;
  /** 优先级（数字越小越先匹配）。 */
  priority?: number;
  /** 结构化条件对象；0 条件 = `null`（恒真），**不得**下发空 and / 条件字符串。 */
  when?: RuleCondition | null;
  /** 结构化动作列表（至少一个；引擎要求规则必须产出动作）。 */
  actions?: RuleAction[];
  /** SELECT 字段白名单（留空 = 输出全部字段）。 */
  select?: string[];
  /** 依赖规则 id（wire: `depends_on`）。 */
  dependsOn?: string[];
} & WriteMeta;

/** 转发规则读写 API（`repo.rules.*`；写端点 `/api/rules` 系列）。 */
export interface RuleApi {
  /** 拉取规则清单（`GET /api/rules`；兼容数组与 `{items, source, reason}` 两种形状）。 */
  list(): Promise<RuleListResult>;
  /** 新增：`POST /api/rules`（body 含结构化 `when` / `actions` + `reason` / `note` / `confirm` 三独立字段）。 */
  create(input: RuleUpsertInput): Promise<WriteResult<RuleDetail>>;
  /** 编辑（可选子集部分更新）：`PUT /api/rules/:id`。 */
  update(input: RuleUpsertInput): Promise<WriteResult<RuleDetail>>;
  /** 删除：`DELETE /api/rules/:id`（body `{reason, note, confirm}`）。 */
  remove(input: { id: string } & WriteMeta): Promise<WriteResult>;
  /** 启停：`PUT /api/rules/:id`（body `{enabled, reason, note, confirm}`）。
   *
   * `actor` 仅作调用方标注（`WriteMeta` 三字段之外），与 `reason` / `note` / `confirm` 各自独立。
   */
  setEnabled(input: { id: string; enabled: boolean; actor?: string } & WriteMeta): Promise<WriteResult>;
}

/** 生成规则 id（后端要求非空唯一 id）：`rule-<名称slug>-<时间尾数>`。 */
function ruleIdFrom(name: string): string {
  return `${slug(name, 'rule')}-${String(Date.now()).slice(-6)}`;
}

/** 由入参构造规则写请求 body（snake_case wire 字段 + reason / note / confirm 三独立字段原样下发）。 */
function ruleUpsertBody(input: RuleUpsertInput): Record<string, unknown> {
  const body: Record<string, unknown> = {};
  if (input.id !== undefined) {
    body['id'] = input.id;
  }
  if (input.name !== undefined) {
    body['name'] = input.name;
  }
  if (input.forwarderId !== undefined) {
    body['forwarder_id'] = input.forwarderId;
  }
  if (input.enabled !== undefined) {
    body['enabled'] = input.enabled;
  }
  if (input.priority !== undefined) {
    body['priority'] = input.priority;
  }
  if (input.when !== undefined) {
    body['when'] = input.when;
  }
  if (input.actions !== undefined) {
    body['actions'] = input.actions;
  }
  if (input.select !== undefined) {
    body['select'] = input.select;
  }
  if (input.dependsOn !== undefined) {
    body['depends_on'] = input.dependsOn;
  }
  return withReason(body, input);
}

/** 转发规则读写实现。 */
function buildRules(): RuleApi {
  return {
    async list(): Promise<RuleListResult> {
      try {
        const raw = await apiRequest<unknown>('/api/rules');
        let rows: unknown[] = [];
        let notice = '';
        if (Array.isArray(raw)) {
          rows = raw;
        } else {
          const obj = asRecord(raw);
          const arr = obj['items'];
          rows = Array.isArray(arr) ? arr : [];
          const source = pickStr(obj, 'source', '');
          if (rows.length === 0 && source && source !== 'ok') {
            notice = pickStr(obj, 'reason', '规则数据源不可得，已按空列表展示。');
          }
        }
        return { items: rows.map((row, i) => mapRuleDetail(asRecord(row), i)), notice };
      } catch (cause) {
        return { items: [], notice: describeFailure(cause, '读取转发规则') };
      }
    },

    async create(input): Promise<WriteResult<RuleDetail>> {
      const id = input.id?.trim() || ruleIdFrom(input.name ?? '');
      try {
        const raw = await apiRequest<Record<string, unknown>>('/api/rules', {
          method: 'POST',
          body: JSON.stringify(ruleUpsertBody({ ...input, id })),
        });
        await refresh();
        return {
          ok: true,
          message: `规则「${input.name ?? id}」已创建。`,
          data: mapRuleDetail(asRecord(raw), 0),
        };
      } catch (cause) {
        return writeFailure(cause, `创建转发规则「${input.name ?? id}」`);
      }
    },

    async update(input): Promise<WriteResult<RuleDetail>> {
      const id = input.id?.trim() ?? '';
      if (!id) {
        return fail('更新转发规则：缺少规则 id，请刷新后重试。');
      }
      try {
        const raw = await apiRequest<Record<string, unknown>>(`/api/rules/${encodeURIComponent(id)}`, {
          method: 'PUT',
          body: JSON.stringify(ruleUpsertBody(input)),
        });
        await refresh();
        return {
          ok: true,
          message: `规则「${input.name ?? id}」已更新。`,
          data: mapRuleDetail(asRecord(raw), 0),
        };
      } catch (cause) {
        return writeFailure(cause, `更新转发规则「${input.name ?? id}」`);
      }
    },

    async remove(input): Promise<WriteResult> {
      try {
        await apiRequest<unknown>(`/api/rules/${encodeURIComponent(input.id)}`, {
          method: 'DELETE',
          body: JSON.stringify(withReason({}, input)),
        });
        await refresh();
        return { ok: true, message: `规则 ${input.id} 已删除。` };
      } catch (cause) {
        return writeFailure(cause, `删除转发规则 ${input.id}`);
      }
    },

    async setEnabled(input): Promise<WriteResult> {
      try {
        await apiRequest<unknown>(`/api/rules/${encodeURIComponent(input.id)}`, {
          method: 'PUT',
          body: JSON.stringify(withReason({ enabled: input.enabled }, input)),
        });
        await refresh();
        return { ok: true, message: `规则 ${input.id} 已${input.enabled ? '启用' : '停用'}。` };
      } catch (cause) {
        return writeFailure(cause, `转发规则 ${input.id} ${input.enabled ? '启用' : '停用'}`);
      }
    },
  };
}

/** 转发规则 API 实例（`repo.rules` 与顶层 `setRuleEnabled` 共用同一实现）。 */
const rulesApi: RuleApi = buildRules();

// ===========================================================================
// 统一导出
// ===========================================================================

/** 读取型基础仓库（真实缓存为唯一数据源；空缓存 = 诚实空态）。 */
function buildRepo() {
  const writes = buildWrites();
  return {
    // ---------- 网关信息 ----------
    getGateway(): GatewayInfo {
      return realCache.status ?? HONEST_EMPTY_GATEWAY;
    },

    // ---------- 设备（读） ----------
    queryDevices(query: { status: string; protocol: string; keyword: string; page: number; pageSize: number }): Paged<DeviceRecord> {
      const kw = query.keyword.trim().toLowerCase();
      const filtered = realCache.devices.filter((d) => {
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
      return realCache.devices;
    },

    getDevice(id: string): DeviceRecord | null {
      return realCache.devices.find((d) => d.id === id) ?? null;
    },

    // ---------- 点位（读） ----------
    queryPoints(query: {
      deviceId: string;
      pointType: string;
      quality: string;
      keyword: string;
      page: number;
      pageSize: number;
    }): Paged<PointRecord> {
      const kw = query.keyword.trim().toLowerCase();
      const filtered = realCache.points.filter((p) => {
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
      return realCache.points;
    },

    pointsOfDevice(deviceId: string): PointRecord[] {
      return realCache.points.filter((p) => p.deviceId === deviceId);
    },

    // ---------- 北向出口（读） ----------
    allForwarders(): ForwarderRecord[] {
      return realCache.outlets;
    },

    getForwarder(id: string): ForwarderRecord | null {
      return realCache.outlets.find((f) => f.id === id) ?? null;
    },

    // ---------- 转发规则（读） ----------
    allRules(): RuleRecord[] {
      return realCache.rules;
    },

    // ---------- 告警（读） ----------
    allAlarms(): AlarmRecord[] {
      return realCache.alarms;
    },

    // ---------- 审计（读） ----------
    queryAudit(query: {
      actorType: string;
      actor: string;
      action: string;
      entityType: string;
      result: string;
      from: string;
      to: string;
      page: number;
      pageSize: number;
    }): Paged<AuditEntry> {
      const actor = (query.actor || '').trim();
      const from = (query.from || '').trim();
      const to = (query.to || '').trim();
      const filtered = realCache.events.filter((log) => {
        if (query.actorType && log.actorType !== query.actorType) {
          return false;
        }
        if (actor && !log.actor.includes(actor)) {
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
        if (from && log.ts < from) {
          return false;
        }
        if (to && log.ts > to) {
          return false;
        }
        return true;
      });
      return paginate(filtered, query.page, query.pageSize);
    },

    // ---------- 授权（读） ----------
    getLicense(): MockLicense {
      return effectiveLicense(false);
    },

    getLicenseDegraded(): MockLicense {
      return effectiveLicense(true);
    },

    // ---------- 写操作（真实 HTTP + 刷新） ----------
    createDevice: writes.createDevice,
    updateDevice: writes.updateDevice,
    deleteDevice: writes.deleteDevice,
    createPoint: writes.createPoint,
    updatePoint: writes.updatePoint,
    deletePoint: writes.deletePoint,
    replacePointsOfDevice: writes.replacePointsOfDevice,
    setForwarderEncoding: writes.setForwarderEncoding,
    setRuleEnabled: rulesApi.setEnabled,
    resolveAlarm: writes.resolveAlarm,
    activate: writes.activate,
    submitTransferRequest: writes.submitTransferRequest,
    logCopyMachineCode: writes.logCopyMachineCode,
  };
}

/** 页面层唯一数据入口（含 `ops` / `settings` / `actions` 与分组 / 角色 / 账号命名空间）。 */
export const repo = {
  ...buildRepo(),
  ops: buildOps(),
  settings: buildSettings(),
  actions: buildActions(),
  groups: buildGroups(),
  roles: buildRoles(),
  accounts: buildAccounts(),
  /** 转发规则读写（结构化条件契约；写端点 `/api/rules` 系列）。 */
  rules: rulesApi,
  /** 权限清单（`GET /api/permissions`；后端未落地 → 诚实空态）。 */
  async permissions(): Promise<PermissionRecord[]> {
    try {
      const raw = await apiRequest<unknown[]>('/api/permissions');
      realCache.permissions = Array.isArray(raw) ? raw.map((row, i) => mapPermission(asRecord(row), i)) : [];
      realCache.notices.permissions = '';
    } catch (cause) {
      realCache.permissions = [];
      realCache.notices.permissions = describeFailure(cause, '权限清单');
    }
    return realCache.permissions;
  },
};

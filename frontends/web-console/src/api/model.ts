/**
 * @file model.ts
 * @module web-console/api/model
 * @description 网关控制台**领域模型与展示常量**（原演示数据模块的类型 / 常量
 * 迁出后的事实源）。
 *
 * ── 定位 ────────────────────────────────────────────────────────────────────
 * 本模块**只有类型定义、枚举常量与 UI 选项**，**不含任何演示数据**。
 * 数据一律由 `api/repo.ts` 从真实后端接口拉取。
 * 原先演示数据模块里的示例数组（设备 / 点位 / 出口 / 规则 / 告警 / 审计）
 * 已随该模块一并删除；本模块只保留「契约形状」。
 *
 * ── 契约：字段形状对齐网关管理 API ───────────────────────────────────────────
 *   · 网关信息    ↔ `GET /api/overview`
 *   · 设备        ↔ `GET/POST/PUT/DELETE /api/devices`
 *   · 设备分组    ↔ `GET/POST/PUT/DELETE /api/groups`
 *   · 点位        ↔ `GET/POST/PUT/DELETE /api/points`、`POST /api/points/import`
 *   · 北向出口    ↔ `GET/POST /api/forwarders`、`POST /api/forwarders/{id}/test`
 *   · 告警 / 规则 ↔ `GET /api/alerts`、`GET /api/rules`、`PUT /api/alerts/rules`
 *   · 审计日志    ↔ `GET /api/audit`、`GET /api/logs`
 *   · 授权状态    ↔ `GET /api/license/status`
 *   · 角色 / 权限 / 账号 ↔ `GET|POST /api/roles`、`GET /api/permissions`、`GET|POST /api/accounts`
 *
 * ── 红线：大整数一律用 `string`（IEEE754 精度陷阱）──────────────────────────
 * JS `number` 安全整数上限为 2^53−1 = 9_007_199_254_740_991。以下字段必须 `string`：
 *   · 纳秒/毫秒时间戳、序列号、累计计数器（`seq*`、`totalRecords`、纳秒 ts）；
 *   · 机器码、点位 id、设备 id、Topic 模板中的大数占位。
 * 本模块现状：
 *   · `machineCode` / `id` / `deviceId` / `pointId` / `topicTemplate` → `string` ✅
 *   · `totalRecords` / `seqFrom` / `seqTo` / `tsNs`（越界风险字段）→ `string` ✅
 *   · `onlineCount` / `failedPointCount` / `deviceCount` / 分页 `page·pageSize·total`
 *     → `number`，均为**小值业务计数**（≤ 四位数），远离 2^53，**无需改 string** ✅
 *
 * ── 时间格式约定 ─────────────────────────────────────────────────────────────
 * 所有时间字段统一为本地字符串 `'YYYY-MM-DD HH:mm:ss'`；日期型为 `'YYYY-MM-DD'`。
 * 后端返回 epoch 秒 / 毫秒字符串时，由 `repo.ts` 归一化为上述格式（无精度损失）。
 */

// ===========================================================================
// 枚举与字面量类型
// ===========================================================================

/** 协议类型（`ui-gateway-console.md` §3.2）。 */
export type ProtocolType = 'modbus-tcp' | 'modbus-rtu' | 'opc-ua' | 's7' | 'mc' | 'http' | 'mqtt';

/** 设备在线状态（后端 `GET /api/devices` 的 `status` 字段三态）。 */
export type DeviceStatus = 'online' | 'offline' | 'error';

/** 点位类型：物理点（来自现场）/ 计算点（公式派生）。 */
export type PointType = 'physical' | 'derived';

/** 采集质量（与 `ui-kit/status-map.ts` 的 Good/Bad 语义对齐）。 */
export type DataQuality = 'Good' | 'Uncertain' | 'Bad' | 'CalcFailed' | 'Timeout';

/** 字节序（导入校验的允许值集合）。 */
export type ByteOrder = 'AB CD' | 'CD AB' | 'BA DC' | 'DC BA';

/** 北向出口连接状态。 */
export type ForwarderStatus = 'connected' | 'disconnected' | 'error';

/** 数据编码（每路出口独立单选的取值）。 */
export type Encoding = 'protobuf' | 'json';

/** 告警级别（按严重度降序排列的取值）。 */
export type AlarmLevel = 'critical' | 'major' | 'minor' | 'warning';

/** 告警处置状态。 */
export type AlarmState = 'open' | 'acking' | 'resolved';

/** 授权档位状态。 */
export type LicenseStatus = 'active' | 'trial' | 'grace' | 'stopped';

// ===========================================================================
// 接口（前端镜像）
// ===========================================================================

/** 网关基本信息（`GET /api/overview` 的前端镜像）。 */
export interface GatewayInfo {
  /** 网关名称（设备名） */
  name: string;
  /** 机器码（完整值，展示时用 `formatMachineCode` 格式化） */
  machineCode: string;
  /** 部署形态（native / docker / container） */
  deployMode: string;
  /** 版本（含 build 短哈希） */
  version: string;
  /** 主机名 */
  hostname: string;
  /** 管理地址（本机浏览器访问） */
  manageUrl: string;
  /** 监听端口 */
  port: number;
  /** 启动时间（YYYY-MM-DD HH:mm:ss） */
  startedAt: string;
  /** 运行时长描述 */
  uptimeText: string;
  /** 设备总数 */
  deviceCount: number;
  /** 在线设备数 */
  onlineCount: number;
  /** 点位总数 */
  pointCount: number;
  /** 采集失败点位数 */
  failedPointCount: number;
  /** 采集总频率（点/秒） */
  sampleRatePerSec: number;
  /** 北向转发速率（条/秒） */
  forwardRatePerSec: number;
  /** 磁盘队列水位（GB） */
  queueUsedGb: number;
  /** 磁盘队列容量（GB） */
  queueCapacityGb: number;
  /** 按当前速率可续传天数（现场最关心的换算值） */
  queueDrainDays: number;
  /** 累计转发记录数（**string**：可能超 2^53） */
  totalForwardedRecords: string;
}

/** 设备记录（`/api/devices` 的前端镜像）。 */
export interface DeviceRecord {
  /** 主键 */
  id: string;
  /** 设备名称 */
  name: string;
  /** 协议类型 */
  protocol: ProtocolType;
  /** 协议中文名（列表展示） */
  protocolLabel: string;
  /** 连接摘要（如 `192.168.10.31:502 · 从站 1` 或 `COM3 · 9600 8E1`） */
  connectionSummary: string;
  /** 在线状态（后端未上报时为 `offline`，并在 `unknownText` 标注「未知」） */
  status: DeviceStatus;
  /**
   * 状态不可知时的诚实提示（空串 = 后端已上报真实状态）。
   *
   * 后端 `GET /api/devices` 未返回 `status` 字段时，前端**不猜**在线与否：
   * 状态按最保守的 `offline` 展示，并把原因写进本字段，供页面显式标注「状态未知」。
   */
  unknownText: string;
  /** 采集频率（ms） */
  intervalMs: number;
  /** 超时（ms） */
  timeoutMs: number;
  /** 重试次数 */
  retryTimes: number;
  /** 点位数量 */
  pointCount: number;
  /** 所属分组 id（后端未上报 / 未分组为空串） */
  groupId: string;
  /** 最近成功采集时间（YYYY-MM-DD HH:mm:ss；后端未上报为 `—`） */
  lastSampleAt: string;
  /** 连接成功率（%）；后端未上报为 0 且 `unknownText` 说明 */
  successRate: number;
  /** 连续失败次数 */
  failStreak: number;
  /** 离线时长描述（在线为空串） */
  offlineText: string;
  /** 创建时间 */
  createdAt: string;
}

/** 点位记录（`/api/points` 的前端镜像；含计算点）。 */
export interface PointRecord {
  /** 主键 */
  id: string;
  /** 所属设备 id */
  deviceId: string;
  /** 所属设备名（冗余便于展示与过滤） */
  deviceName: string;
  /** 点位名称（中文） */
  name: string;
  /** 点位类型：物理 / 计算 */
  pointType: PointType;
  /** 地址（物理点如 `DB1.0`；计算点为 `—`） */
  address: string;
  /** 数据类型（float32 / uint32 / int16 …） */
  dataType: string;
  /** 字节序（计算点为 `—`） */
  byteOrder: ByteOrder | '—';
  /** 工程单位 */
  unit: string;
  /** 死区 */
  deadband: number;
  /** 北向目标点名（Topic 变量名） */
  targetKey: string;
  /** 是否参与北向推送（后端 `push_enabled`） */
  pushEnabled: boolean;
  /** 当前质量 */
  quality: DataQuality;
  /** 当前值（数值型；不可用时为 `null`） */
  value: number | null;
  /** 数值展示文本（含单位不可用时的 `——`） */
  valueText: string;
  /** 公式表达式（仅计算点；物理点为 `null`） */
  formula: string | null;
  /** 最近更新时间（YYYY-MM-DD HH:mm:ss） */
  updatedAt: string;
  /** 数据是否陈旧（>1s 未更新） */
  stale: boolean;
}

/** 新增设备表单草稿（`repo.createDevice` 入参）。
 *
 * 前端只采集连接配置；**授权判定与连通性校验在网关 Rust 侧**，本类型不含任何授权字段。
 * `id` 缺省时由 repo 依据名称生成稳定标识（后端 `POST /api/devices` 要求非空唯一 id）。
 */
export interface DeviceDraft {
  /** 设备标识（可选；缺省自动生成） */
  id?: string;
  /** 设备名称 */
  name: string;
  /** 协议类型 */
  protocol: ProtocolType;
  /** 连接摘要（如 `192.168.10.31:102 · 机架 0 槽 1`） */
  connectionSummary: string;
  /** 采集频率（ms） */
  intervalMs: number;
  /** 超时（ms） */
  timeoutMs: number;
  /** 重试次数 */
  retryTimes: number;
  /** 所属分组 id（可选；未分组留空） */
  groupId?: string;
  /** 操作者 */
  actor: string;
}

/** 点位表单草稿（`repo.createPoint` / `repo.replacePointsOfDevice` 入参）。 */
export interface PointDraft {
  /** 所属设备 id */
  deviceId: string;
  /** 点位名称（中文） */
  name: string;
  /** 点位类型：physical / derived */
  pointType: PointType;
  /** 地址（物理点必填；计算点为空） */
  address: string;
  /**
   * 设备接入端点（host:port / 串口 / URL；V1 正例字段）。
   *
   * 后端 `PointCreateBody` / `PointUpdateBody` 的正例键是 `endpoint`（`address`
   * 保留为别名）。UI 有值时由 repo 透传到 `endpoint`；缺省不带上（走 address
   * 别名语义，行为不变）。
   */
  endpoint?: string;
  /** 数据类型（float32 / uint32 …） */
  dataType: string;
  /** 字节序（物理点必填；计算点为 `—`） */
  byteOrder: ByteOrder | '—';
  /** 工程单位 */
  unit: string;
  /** 死区 */
  deadband: number;
  /** 北向目标点名（Topic 变量名，需唯一） */
  targetKey: string;
  /** 是否参与北向推送（可选；缺省 true） */
  pushEnabled?: boolean;
  /** 公式表达式（仅计算点） */
  formula?: string | null;
  /** 操作者 */
  actor?: string;
}

/** 北向出口记录（`/api/forwarders` 的前端镜像）。 */
export interface ForwarderRecord {
  /** 主键 */
  id: string;
  /** 出口名称（如 `客户 EMQX（生产）`） */
  name: string;
  /** Broker 地址（含协议前缀） */
  brokerUrl: string;
  /** 传输安全（如 `TLS` / `mTLS` / `无`） */
  transportSecurity: string;
  /** 证书状态描述 */
  certStatusText: string;
  /** 客户端 ID */
  clientId: string;
  /** QoS */
  qos: 0 | 1 | 2;
  /** 是否保留消息 */
  retained: boolean;
  /** Topic 模板（如 `factory/line1/${device}/${point}`） */
  topicTemplate: string;
  /** 编码（**每路出口独立单选**，默认 protobuf） */
  encoding: Encoding;
  /** 连接状态 */
  status: ForwarderStatus;
  /** 持续连接时长描述 */
  connectedForText: string;
  /** 覆盖设备数（当前） */
  coveredDevices: number;
  /** 该编码的推荐设备上限（JSON 更低） */
  recommendedDeviceLimit: number;
  /** 上次编码一致性自检时间（YYYY-MM-DD HH:mm:ss） */
  lastConsistencyCheckAt: string;
  /** 是否启用 */
  enabled: boolean;
}

/** 转发规则记录。 */
export interface RuleRecord {
  /** 主键 */
  id: string;
  /** 规则名称 */
  name: string;
  /** 生效出口 id */
  forwarderId: string;
  /** 生效出口名（冗余展示） */
  forwarderName: string;
  /** 过滤条件描述（如 `quality = Good`） */
  condition: string;
  /** 动作描述（如 `转发 / 丢弃 / 告警`） */
  action: string;
  /** 命中次数 */
  hitCount: number;
  /** 优先级（数字越小越先匹配） */
  priority: number;
  /** 是否启用 */
  enabled: boolean;
  /** 最近命中时间 */
  lastHitAt: string;
}

/** 告警记录（`/api/alerts` 的前端镜像）。 */
export interface AlarmRecord {
  /** 主键 */
  id: string;
  /** 级别 */
  level: AlarmLevel;
  /** 级别中文 */
  levelLabel: string;
  /** 来源类型（device / forwarder / license / system） */
  sourceType: string;
  /** 来源对象名 */
  sourceLabel: string;
  /** 告警标题 */
  title: string;
  /** 告警详情 */
  detail: string;
  /** 首次触发时间 */
  firstSeenAt: string;
  /** 最近触发时间 */
  lastSeenAt: string;
  /** 触发次数 */
  count: number;
  /** 处置状态 */
  state: AlarmState;
  /** 处置状态中文 */
  stateLabel: string;
  /** 处置人（未处置为空串） */
  ackedBy: string;
  /** 处置说明 */
  note: string;
}

/** 审计日志条目（`/api/audit` 的前端镜像）。 */
export interface AuditEntry {
  /** 主键 */
  id: string;
  /** 时间（YYYY-MM-DD HH:mm:ss） */
  ts: string;
  /** 操作者 */
  actor: string;
  /** 操作者类型（human / system） */
  actorType: string;
  /** 动作 */
  action: string;
  /** 对象类型（device / point / forwarder / license / user / system） */
  entityType: string;
  /** 对象类型中文 */
  entityLabel: string;
  /** 对象标识 */
  entityId: string;
  /** 明细 */
  detail: string;
  /** 来源 IP */
  ip: string;
  /** 结果（success / denied / failed） */
  result: string;
}

/** 授权状态快照（`GET /api/license/status` 的前端镜像）。 */
export interface MockLicense {
  /** 档位状态 */
  status: LicenseStatus;
  /** 档位 id（free / standard / pro） */
  tierId: string;
  /** 档位中文名 */
  tierName: string;
  /** 校验档位（A / B / C） */
  grade: string;
  /** 剩余天数（active 时有效） */
  remainingDays: number;
  /** 剩余时长文本（试用 / 宽限期用，如 `2 天 14 小时`） */
  remainingText: string;
  /** 租约有效至（YYYY-MM-DD HH:mm:ss） */
  validUntil: string;
  /** 上次心跳时间 */
  lastHeartbeatAt: string;
  /** 下次心跳时间 */
  nextHeartbeatAt: string;
  /** 机器码锚点来源（只读） */
  anchorSources: string;
  /** 降级原因（未降级为空串） */
  degradeReason: string;
  /** 到期后行为说明 */
  onExpireText: string;
  /** 能力矩阵（用于「了解差异」弹窗） */
  capabilities: readonly LicenseCapability[];
}

/** 档位能力项。 */
export interface LicenseCapability {
  /** 能力名 */
  readonly name: string;
  /** 当前档位是否具备 */
  readonly included: boolean;
  /** 说明 */
  readonly note: string;
}

/** 设备分组（`/api/groups` 的前端镜像）。 */
export interface DeviceGroup {
  /** 主键 */
  id: string;
  /** 分组名称 */
  name: string;
  /** 分组内设备数 */
  deviceCount: number;
  /** 是否为默认分组（不可删除） */
  isDefault: boolean;
}

/** 角色记录（`/api/roles` 的前端镜像）。 */
export interface RoleRecord {
  /** 主键 */
  id: string;
  /** 角色名称 */
  name: string;
  /** 权限 id 列表 */
  permissions: string[];
  /** 是否内置角色（内置角色不可删除） */
  builtin: boolean;
  /** 关联账号数 */
  accountCount: number;
}

/** 权限清单项（`/api/permissions` 的前端镜像）。 */
export interface PermissionRecord {
  /** 权限 id（如 `device.write`） */
  id: string;
  /** 中文名 */
  label: string;
  /** 所属分组（如 `数据接入`） */
  group: string;
  /**
   * 权限所属「端」（`gateway` = 网关侧 / `licensing` = 厂商侧）。
   *
   * 后端 `GET /api/permissions` 默认只返回网关侧权限，此字段供前端**兜底过滤**：
   * 即便后端误把厂商侧权限透出，网关控制台也不渲染。缺省（老后端未返回）为空串。
   */
  scope?: string;
}

/** 账号记录（`/api/accounts` 的前端镜像）。 */
export interface AccountRecord {
  /** 账号名（主键） */
  account: string;
  /** 角色 id */
  role: string;
  /** 账号状态（如 `active` / `disabled`） */
  status: string;
  /** 最近登录时间（YYYY-MM-DD HH:mm:ss；从未登录为 `—`） */
  lastLoginAt: string;
  /** 创建时间 */
  createdAt: string;
}

/** 分页结果。 */
export interface Paged<T> {
  /** 当前页数据 */
  items: T[];
  /** 总条数 */
  total: number;
  /** 当前页 */
  page: number;
}

/**
 * 写操作结构化结果（**诚实契约**）。
 *
 * `ok === false` 时 `message` 必为可解释的真实原因（后端 400/403/501/503 原文或网络错误），
 * 绝不出现「请求失败却返回 ok:true」的兜底。
 */
export interface WriteResult<T = void> {
  /** 是否真正落库成功 */
  ok: boolean;
  /** 面向上层的结果 / 失败原因（可直接展示） */
  message: string;
  /** 成功时可携带后端归一化后的记录 */
  data?: T;
}

// ===========================================================================
// 展示常量（原演示数据模块的枚举选项；非演示数据）
// ===========================================================================

/** 协议选项（新增设备表单动态切换用）。 */
export const PROTOCOL_OPTIONS: readonly { value: ProtocolType; label: string }[] = [
  { value: 'modbus-tcp', label: 'Modbus TCP' },
  { value: 'modbus-rtu', label: 'Modbus RTU' },
  { value: 'opc-ua', label: 'OPC UA' },
  { value: 's7', label: '西门子 S7' },
  { value: 'mc', label: '三菱 MC' },
  { value: 'http', label: 'HTTP' },
  { value: 'mqtt', label: '第三方 MQTT' },
];

/** 数据类型选项（点表导入的允许值参考）。 */
export const DATA_TYPE_OPTIONS: readonly string[] = [
  'int16',
  'uint16',
  'int32',
  'uint32',
  'float32',
  'float64',
  'bool',
  'string',
];

/** 字节序允许值（点表导入校验的 `允许值` 提示用）。 */
export const BYTE_ORDER_OPTIONS: readonly ByteOrder[] = ['AB CD', 'CD AB', 'BA DC', 'DC BA'];

/** 质量枚举。 */
export const QUALITY_OPTIONS: readonly DataQuality[] = ['Good', 'Uncertain', 'Bad', 'CalcFailed', 'Timeout'];

/** 公式函数白名单（前端即时提示用；**权威校验在网关侧**）。 */
export const FORMULA_FUNCTIONS: readonly string[] = [
  'abs',
  'ceil',
  'floor',
  'round',
  'clamp',
  'min',
  'max',
  'sqrt',
  'pow',
  'exp',
  'log',
  'log10',
  'if',
  'prev',
  'delta',
  'rate',
  'hold',
  'quality',
];

/** 导入点表的失败策略。 */
export const IMPORT_FAIL_POLICIES: readonly { value: string; label: string }[] = [
  { value: 'keep-last', label: '保留上次有效值并标记质量（默认）' },
  { value: 'empty', label: '置空' },
  { value: 'skip', label: '不输出' },
];

/** 当前操作者显示名兜底（登录后由 session 提供真实显示名）。 */
export const DEFAULT_ACTOR = '张工';

// ===========================================================================
// 授权展示快照（诚实空值基线；真实值由 `GET /api/license/status` 覆盖）
// ===========================================================================

/**
 * 授权状态「诚实空值」基线。
 *
 * **不是演示数据**：所有字段为空 / `—` / 0，`status` 取最保守的 `stopped`；
 * 页面在真实快照到达前（preload 前）展示本基线，绝不展示任何虚构档位或剩余天数。
 * 真实值由 `repo.getLicense()` 从 `GET /api/license/status` 映射覆盖。
 */
export const licenseSnapshot: MockLicense = {
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

/**
 * 授权状态「降级展示」诚实空值基线（供降级横幅 / 授权页降级视图使用）。
 *
 * 真实降级判定在后端心跳校验；前端不做任何判定，仅展示真实快照映射结果。
 */
export const licenseDegradedSnapshot: MockLicense = {
  status: 'grace',
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

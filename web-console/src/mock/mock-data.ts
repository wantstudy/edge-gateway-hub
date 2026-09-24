/**
 * @file mock-data.ts
 * @module web-console/mock/mock-data
 * @description 客户端网关控制台演示数据集（in-memory repository）。
 *
 * ── 定位 ────────────────────────────────────────────────────────────────────
 * 本模块是**客户端视角**的数据源，与 `admin-console/src/mock/mock-data.ts`
 * **完全独立**：admin 侧是「厂商租户 / 激活码生命周期」视角，本侧是「这一台网关
 * 自己」的视角（本机设备、本机点位、本机北向出口、本机告警与审计）。
 * **不要复用 admin-console 的 mock**，两侧实体语义不同。
 *
 * ── 契约：字段形状对齐网关管理 API（`ui-gateway-console.md` §5）──────────────
 * 实体形状是各接口响应体的**前端镜像**，字段名逐字段可对应，以便后续把
 * `mock 仓库` 换成 `HTTP 客户端` 时页面层零改动：
 *   · 网关信息    ↔ `GET /api/overview`
 *   · 设备        ↔ `GET/POST/PUT/DELETE /api/devices`、`POST /api/devices/test`
 *   · 点位        ↔ `GET/POST /api/points`、`POST /api/points/import`、`GET /api/points/export`
 *   · 北向出口    ↔ `GET/POST /api/forwarders`、`POST /api/forwarders/{id}/test`
 *   · 告警        ↔ `GET /api/alerts`、`PUT /api/alerts/rules`
 *   · 审计日志    ↔ `GET /api/logs`、`GET /api/audit`
 *   · 授权状态    ↔ `GET /api/license/status`
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
 *
 * ── 行为规则 ─────────────────────────────────────────────────────────────────
 *  · 写操作只改本模块内存数组，并**深拷贝**返回，绝不让页面层拿到引用后误改真相；
 *  · 所有写操作落审计日志（actor / action / entity / detail / result）；
 *  · **授权相关**：客户端只有「复制机器码 / 输入激活码 / 申请换机」三个触点，
 *    本模块**不提供**任何 `revoke` / `unbind` / `resetTrial` 方法（红线 2）。
 */

// ===========================================================================
// 类型定义
// ===========================================================================

/** 协议类型（`ui-gateway-console.md` §3.2）。 */
export type ProtocolType = 'modbus-tcp' | 'modbus-rtu' | 'opc-ua' | 's7' | 'mc' | 'http' | 'mqtt';

/** 设备在线状态。 */
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
  /** 在线状态 */
  status: DeviceStatus;
  /** 采集频率（ms） */
  intervalMs: number;
  /** 超时（ms） */
  timeoutMs: number;
  /** 重试次数 */
  retryTimes: number;
  /** 点位数量 */
  pointCount: number;
  /** 最近成功采集时间（YYYY-MM-DD HH:mm:ss） */
  lastSampleAt: string;
  /** 连接成功率（%） */
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
 */
export interface DeviceDraft {
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
  /** 操作者 */
  actor: string;
}

/** 点位表单草稿（`repo.createPoint` / `replacePointsOfDevice` 入参）。 */
export interface PointDraft {
  /** 所属设备 id */
  deviceId: string;
  /** 点位名称（中文） */
  name: string;
  /** 点位类型：physical / derived */
  pointType: PointType;
  /** 地址（物理点必填；计算点为空） */
  address: string;
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

// ===========================================================================
// 演示数据
// ===========================================================================

/** 网关基本信息（本机视角）。 */
const gateway: GatewayInfo = {
  name: '线1-网关-01',
  machineCode: '8F3A-91C2-7D04-5BE6',
  deployMode: 'native',
  version: '1.0.0 (build a91f3c2)',
  hostname: 'iotdaq-line1-01',
  manageUrl: 'https://192.168.10.5:8080',
  port: 8080,
  startedAt: '2026-09-20 08:12:41',
  uptimeText: '3 天 4 小时',
  deviceCount: 26,
  onlineCount: 24,
  pointCount: 1284,
  failedPointCount: 12,
  sampleRatePerSec: 486,
  forwardRatePerSec: 486,
  queueUsedGb: 3.2,
  queueCapacityGb: 10,
  queueDrainDays: 4.1,
  totalForwardedRecords: '1849200331',
};

/** 设备清单（含异常行：离线 / 采集失败）。 */
const devices: DeviceRecord[] = [
  {
    id: 'dev-001',
    name: '1#注塑机',
    protocol: 's7',
    protocolLabel: '西门子 S7',
    connectionSummary: '192.168.10.31:102 · 机架 0 槽 1',
    status: 'online',
    intervalMs: 1000,
    timeoutMs: 3000,
    retryTimes: 3,
    pointCount: 128,
    lastSampleAt: '2026-09-23 12:31:40',
    successRate: 98,
    failStreak: 0,
    offlineText: '',
    createdAt: '2026-09-12 14:40:02',
  },
  {
    id: 'dev-002',
    name: '电表 A 相',
    protocol: 'modbus-rtu',
    protocolLabel: 'Modbus RTU',
    connectionSummary: 'COM3 · 9600 8E1 · 从站 1',
    status: 'error',
    intervalMs: 1000,
    timeoutMs: 3000,
    retryTimes: 3,
    pointCount: 42,
    lastSampleAt: '2026-09-23 12:31:28',
    successRate: 71,
    failStreak: 12,
    offlineText: '',
    createdAt: '2026-09-12 15:02:11',
  },
  {
    id: 'dev-003',
    name: '冷水机组 #3',
    protocol: 'modbus-rtu',
    protocolLabel: 'Modbus RTU',
    connectionSummary: 'COM3 · 9600 8E1 · 从站 3',
    status: 'offline',
    intervalMs: 1000,
    timeoutMs: 3000,
    retryTimes: 3,
    pointCount: 36,
    lastSampleAt: '2026-09-23 12:28:12',
    successRate: 0,
    failStreak: 187,
    offlineText: '离线 3 分钟',
    createdAt: '2026-09-13 09:20:45',
  },
  {
    id: 'dev-004',
    name: '2#注塑机',
    protocol: 's7',
    protocolLabel: '西门子 S7',
    connectionSummary: '192.168.10.32:102 · 机架 0 槽 1',
    status: 'online',
    intervalMs: 1000,
    timeoutMs: 3000,
    retryTimes: 3,
    pointCount: 128,
    lastSampleAt: '2026-09-23 12:31:39',
    successRate: 99,
    failStreak: 0,
    offlineText: '',
    createdAt: '2026-09-13 10:11:07',
  },
  {
    id: 'dev-005',
    name: '空压机控制器',
    protocol: 'modbus-tcp',
    protocolLabel: 'Modbus TCP',
    connectionSummary: '192.168.10.41:502 · 从站 1',
    status: 'online',
    intervalMs: 2000,
    timeoutMs: 3000,
    retryTimes: 2,
    pointCount: 24,
    lastSampleAt: '2026-09-23 12:31:38',
    successRate: 97,
    failStreak: 0,
    offlineText: '',
    createdAt: '2026-09-14 08:55:30',
  },
  {
    id: 'dev-006',
    name: '车间温湿度计',
    protocol: 'modbus-rtu',
    protocolLabel: 'Modbus RTU',
    connectionSummary: 'COM4 · 19200 8N1 · 从站 2',
    status: 'online',
    intervalMs: 5000,
    timeoutMs: 3000,
    retryTimes: 2,
    pointCount: 4,
    lastSampleAt: '2026-09-23 12:31:35',
    successRate: 99,
    failStreak: 0,
    offlineText: '',
    createdAt: '2026-09-14 09:30:12',
  },
  {
    id: 'dev-007',
    name: '贴标机 PLC',
    protocol: 'mc',
    protocolLabel: '三菱 MC',
    connectionSummary: '192.168.10.51:6000 · 网络 0 站 1（3E 帧）',
    status: 'online',
    intervalMs: 1000,
    timeoutMs: 3000,
    retryTimes: 3,
    pointCount: 56,
    lastSampleAt: '2026-09-23 12:31:40',
    successRate: 96,
    failStreak: 0,
    offlineText: '',
    createdAt: '2026-09-15 13:44:21',
  },
  {
    id: 'dev-008',
    name: '能耗表（第三方）',
    protocol: 'mqtt',
    protocolLabel: '第三方 MQTT',
    connectionSummary: 'mqtt://10.0.8.77:1883 · topic energy/#',
    status: 'online',
    intervalMs: 10000,
    timeoutMs: 5000,
    retryTimes: 2,
    pointCount: 16,
    lastSampleAt: '2026-09-23 12:31:30',
    successRate: 94,
    failStreak: 0,
    offlineText: '',
    createdAt: '2026-09-16 11:20:00',
  },
];

/**
 * 点位清单（物理点 + 计算点混排，与设计 §3.3 的列一一对应）。
 *
 * 直接消费约定（后代页面请照此读取）：
 *  · 过滤某设备的点：`points.filter((p) => p.deviceId === deviceId)`
 *  · 物理点与计算点区分：`p.pointType === 'physical' | 'derived'`
 *  · 计算点的 `address` / `byteOrder` 为 `—`，`formula` 非空
 *  · 陈旧数据判定：`p.stale === true`（整行转灰）
 *  · 质量异常的竖条：`p.quality !== 'Good'`
 */
const points: PointRecord[] = [
  {
    id: 'pt-001',
    deviceId: 'dev-001',
    deviceName: '1#注塑机',
    name: '料筒温度1',
    pointType: 'physical',
    address: 'DB1.0',
    dataType: 'float32',
    byteOrder: 'AB CD',
    unit: '℃',
    deadband: 0.5,
    targetKey: 'T_Barrel1',
    quality: 'Good',
    value: 214.6,
    valueText: '214.6 ℃',
    formula: null,
    updatedAt: '2026-09-23 12:31:40',
    stale: false,
  },
  {
    id: 'pt-002',
    deviceId: 'dev-001',
    deviceName: '1#注塑机',
    name: '注射压力',
    pointType: 'physical',
    address: 'DB1.4',
    dataType: 'float32',
    byteOrder: 'AB CD',
    unit: 'MPa',
    deadband: 0.01,
    targetKey: 'P_Inj',
    quality: 'Good',
    value: 86.12,
    valueText: '86.12 MPa',
    formula: null,
    updatedAt: '2026-09-23 12:31:40',
    stale: false,
  },
  {
    id: 'pt-003',
    deviceId: 'dev-001',
    deviceName: '1#注塑机',
    name: '累计产量',
    pointType: 'physical',
    address: 'DB2.0',
    dataType: 'uint32',
    byteOrder: 'CD AB',
    unit: '件',
    deadband: 0,
    targetKey: 'Q_Total',
    quality: 'Uncertain',
    value: 128450,
    valueText: '128450 件',
    formula: null,
    updatedAt: '2026-09-23 12:31:39',
    stale: false,
  },
  {
    id: 'pt-004',
    deviceId: 'dev-001',
    deviceName: '1#注塑机',
    name: '单位能耗',
    pointType: 'derived',
    address: '—',
    dataType: 'float32',
    byteOrder: '—',
    unit: 'kWh',
    deadband: 0.01,
    targetKey: 'E_Unit',
    quality: 'Good',
    value: 5.12,
    valueText: '5.12 kWh',
    formula: '([P_Inj] * [T_Barrel1]) / 1000 * 0.2778',
    updatedAt: '2026-09-23 12:31:40',
    stale: false,
  },
  {
    id: 'pt-005',
    deviceId: 'dev-001',
    deviceName: '1#注塑机',
    name: '偏差率',
    pointType: 'derived',
    address: '—',
    dataType: 'float32',
    byteOrder: '—',
    unit: '%',
    deadband: 0.1,
    targetKey: 'D_Rate',
    quality: 'CalcFailed',
    value: null,
    valueText: '——',
    formula: 'abs(([T_Barrel1] - 210) / 210) * 100',
    updatedAt: '2026-09-23 12:31:38',
    stale: false,
  },
  {
    id: 'pt-006',
    deviceId: 'dev-002',
    deviceName: '电表 A 相',
    name: 'A 相电压',
    pointType: 'physical',
    address: '40001',
    dataType: 'float32',
    byteOrder: 'AB CD',
    unit: 'V',
    deadband: 0.5,
    targetKey: 'V_A',
    quality: 'Good',
    value: 220.4,
    valueText: '220.4 V',
    formula: null,
    updatedAt: '2026-09-23 12:31:39',
    stale: false,
  },
  {
    id: 'pt-007',
    deviceId: 'dev-002',
    deviceName: '电表 A 相',
    name: 'A 相电流',
    pointType: 'physical',
    address: '40003',
    dataType: 'float32',
    byteOrder: 'AB CD',
    unit: 'A',
    deadband: 0.01,
    targetKey: 'I_A',
    quality: 'Bad',
    value: null,
    valueText: '——',
    formula: null,
    updatedAt: '2026-09-23 12:30:58',
    stale: true,
  },
  {
    id: 'pt-008',
    deviceId: 'dev-002',
    deviceName: '电表 A 相',
    name: '有功功率',
    pointType: 'physical',
    address: '40005',
    dataType: 'float32',
    byteOrder: 'CD AB',
    unit: 'kW',
    deadband: 0.01,
    targetKey: 'P_Act',
    quality: 'Good',
    value: 12.86,
    valueText: '12.86 kW',
    formula: null,
    updatedAt: '2026-09-23 12:31:39',
    stale: false,
  },
  {
    id: 'pt-009',
    deviceId: 'dev-003',
    deviceName: '冷水机组 #3',
    name: '出水温度',
    pointType: 'physical',
    address: '1',
    dataType: 'int16',
    byteOrder: 'AB CD',
    unit: '℃',
    deadband: 0.2,
    targetKey: 'T_Outlet',
    quality: 'Timeout',
    value: null,
    valueText: '——',
    formula: null,
    updatedAt: '2026-09-23 12:28:12',
    stale: true,
  },
  {
    id: 'pt-010',
    deviceId: 'dev-003',
    deviceName: '冷水机组 #3',
    name: '回水温度',
    pointType: 'physical',
    address: '2',
    dataType: 'int16',
    byteOrder: 'AB CD',
    unit: '℃',
    deadband: 0.2,
    targetKey: 'T_Inlet',
    quality: 'Timeout',
    value: null,
    valueText: '——',
    formula: null,
    updatedAt: '2026-09-23 12:28:12',
    stale: true,
  },
  {
    id: 'pt-011',
    deviceId: 'dev-004',
    deviceName: '2#注塑机',
    name: '料筒温度1',
    pointType: 'physical',
    address: 'DB1.0',
    dataType: 'float32',
    byteOrder: 'AB CD',
    unit: '℃',
    deadband: 0.5,
    targetKey: 'T_Barrel1',
    quality: 'Good',
    value: 209.8,
    valueText: '209.8 ℃',
    formula: null,
    updatedAt: '2026-09-23 12:31:39',
    stale: false,
  },
  {
    id: 'pt-012',
    deviceId: 'dev-004',
    deviceName: '2#注塑机',
    name: '注射压力',
    pointType: 'physical',
    address: 'DB1.4',
    dataType: 'float32',
    byteOrder: 'AB CD',
    unit: 'MPa',
    deadband: 0.01,
    targetKey: 'P_Inj',
    quality: 'Good',
    value: 84.07,
    valueText: '84.07 MPa',
    formula: null,
    updatedAt: '2026-09-23 12:31:39',
    stale: false,
  },
  {
    id: 'pt-013',
    deviceId: 'dev-005',
    deviceName: '空压机控制器',
    name: '排气压力',
    pointType: 'physical',
    address: '40010',
    dataType: 'float32',
    byteOrder: 'AB CD',
    unit: 'MPa',
    deadband: 0.01,
    targetKey: 'P_Exh',
    quality: 'Good',
    value: 0.72,
    valueText: '0.72 MPa',
    formula: null,
    updatedAt: '2026-09-23 12:31:38',
    stale: false,
  },
  {
    id: 'pt-014',
    deviceId: 'dev-006',
    deviceName: '车间温湿度计',
    name: '车间温度',
    pointType: 'physical',
    address: '0',
    dataType: 'int16',
    byteOrder: 'AB CD',
    unit: '℃',
    deadband: 0.1,
    targetKey: 'T_Shop',
    quality: 'Good',
    value: 26.4,
    valueText: '26.4 ℃',
    formula: null,
    updatedAt: '2026-09-23 12:31:35',
    stale: false,
  },
  {
    id: 'pt-015',
    deviceId: 'dev-006',
    deviceName: '车间温湿度计',
    name: '车间湿度',
    pointType: 'physical',
    address: '1',
    dataType: 'uint16',
    byteOrder: 'AB CD',
    unit: '%RH',
    deadband: 0.5,
    targetKey: 'RH_Shop',
    quality: 'Good',
    value: 58.2,
    valueText: '58.2 %RH',
    formula: null,
    updatedAt: '2026-09-23 12:31:35',
    stale: false,
  },
  {
    id: 'pt-016',
    deviceId: 'dev-007',
    deviceName: '贴标机 PLC',
    name: '节拍',
    pointType: 'physical',
    address: 'D100',
    dataType: 'uint16',
    byteOrder: 'CD AB',
    unit: 's',
    deadband: 0.1,
    targetKey: 'T_Cycle',
    quality: 'Good',
    value: 3.8,
    valueText: '3.8 s',
    formula: null,
    updatedAt: '2026-09-23 12:31:40',
    stale: false,
  },
  {
    id: 'pt-017',
    deviceId: 'dev-007',
    deviceName: '贴标机 PLC',
    name: '合格率',
    pointType: 'derived',
    address: '—',
    dataType: 'float32',
    byteOrder: '—',
    unit: '%',
    deadband: 0.1,
    targetKey: 'R_Yield',
    quality: 'Good',
    value: 99.2,
    valueText: '99.2 %',
    formula: '([Q_Ok] / ([Q_Ok] + [Q_Ng])) * 100',
    updatedAt: '2026-09-23 12:31:40',
    stale: false,
  },
  {
    id: 'pt-018',
    deviceId: 'dev-008',
    deviceName: '能耗表（第三方）',
    name: '总用电量',
    pointType: 'physical',
    address: 'energy/total',
    dataType: 'float64',
    byteOrder: '—',
    unit: 'kWh',
    deadband: 0.01,
    targetKey: 'E_Total',
    quality: 'Good',
    value: 184920.33,
    valueText: '184920.33 kWh',
    formula: null,
    updatedAt: '2026-09-23 12:31:30',
    stale: false,
  },
];

/** 北向出口清单（每路编码独立，见设计 §3.4）。 */
const forwarders: ForwarderRecord[] = [
  {
    id: 'fwd-001',
    name: '客户 EMQX（生产）',
    brokerUrl: 'mqtts://emqx.factory.local:8883',
    transportSecurity: 'TLS · mTLS 证书已配置',
    certStatusText: '已配置（有效期至 2027-03-01）',
    clientId: 'iotdaq-line1-01',
    qos: 1,
    retained: false,
    topicTemplate: 'factory/line1/${device}/${point}',
    encoding: 'protobuf',
    status: 'connected',
    connectedForText: '持续 6h12m',
    coveredDevices: 26,
    recommendedDeviceLimit: 500,
    lastConsistencyCheckAt: '2026-09-23 11:02:00',
    enabled: true,
  },
  {
    id: 'fwd-002',
    name: '客户 MES（兼容）',
    brokerUrl: 'mqtt://10.0.8.21:1883',
    transportSecurity: '无（内网）',
    certStatusText: '不适用',
    clientId: 'iotdaq-line1-mes',
    qos: 1,
    retained: false,
    topicTemplate: 'mes/${device}/${point}',
    encoding: 'json',
    status: 'connected',
    connectedForText: '持续 6h12m',
    coveredDevices: 26,
    recommendedDeviceLimit: 120,
    lastConsistencyCheckAt: '2026-09-23 11:02:00',
    enabled: true,
  },
  {
    id: 'fwd-003',
    name: '备用 MQTT（灾备）',
    brokerUrl: 'mqtts://backup.emqx.factory.local:8883',
    transportSecurity: 'TLS',
    certStatusText: '已配置（有效期至 2027-03-01）',
    clientId: 'iotdaq-line1-bak',
    qos: 0,
    retained: true,
    topicTemplate: 'backup/${device}/${point}',
    encoding: 'protobuf',
    status: 'disconnected',
    connectedForText: '—',
    coveredDevices: 0,
    recommendedDeviceLimit: 500,
    lastConsistencyCheckAt: '2026-09-20 08:30:00',
    enabled: false,
  },
];

/** 转发规则清单。 */
const rules: RuleRecord[] = [
  {
    id: 'rule-001',
    name: '质量过滤：仅转发 Good',
    forwarderId: 'fwd-001',
    forwarderName: '客户 EMQX（生产）',
    condition: 'quality = Good',
    action: '转发；其余丢弃并记一次统计',
    hitCount: 1849200331,
    priority: 1,
    enabled: true,
    lastHitAt: '2026-09-23 12:31:40',
  },
  {
    id: 'rule-002',
    name: '变化值优先',
    forwarderId: 'fwd-001',
    forwarderName: '客户 EMQX（生产）',
    condition: 'abs(delta) >= deadband',
    action: '转发（死区抑制）',
    hitCount: 128450,
    priority: 2,
    enabled: true,
    lastHitAt: '2026-09-23 12:31:40',
  },
  {
    id: 'rule-003',
    name: '温度越限告警',
    forwarderId: 'fwd-002',
    forwarderName: '客户 MES（兼容）',
    condition: '[T_Barrel1] > 240',
    action: '告警（不发北向）',
    hitCount: 0,
    priority: 3,
    enabled: true,
    lastHitAt: '—',
  },
  {
    id: 'rule-004',
    name: '能耗表降频',
    forwarderId: 'fwd-002',
    forwarderName: '客户 MES（兼容）',
    condition: 'device = 能耗表（第三方）',
    action: '降频至 60s 转发一次',
    hitCount: 3021,
    priority: 4,
    enabled: false,
    lastHitAt: '2026-09-21 17:02:11',
  },
];

/** 告警清单（按严重度排序来源）。 */
const alarms: AlarmRecord[] = [
  {
    id: 'al-001',
    level: 'critical',
    levelLabel: '严重',
    sourceType: 'device',
    sourceLabel: '冷水机组 #3',
    title: '设备离线',
    detail: 'Modbus RTU COM3 从站 3 连续 187 次无响应，已判定离线 3 分钟。',
    firstSeenAt: '2026-09-23 12:28:12',
    lastSeenAt: '2026-09-23 12:31:40',
    count: 1,
    state: 'open',
    stateLabel: '待处理',
    ackedBy: '',
    note: '',
  },
  {
    id: 'al-002',
    level: 'major',
    levelLabel: '重要',
    sourceType: 'device',
    sourceLabel: '电表 A 相',
    title: '采集失败',
    detail: '点位「A 相电流」连续 12 次读取失败（40003），质量置 Bad。',
    firstSeenAt: '2026-09-23 12:28:40',
    lastSeenAt: '2026-09-23 12:31:38',
    count: 12,
    state: 'open',
    stateLabel: '待处理',
    ackedBy: '',
    note: '',
  },
  {
    id: 'al-003',
    level: 'major',
    levelLabel: '重要',
    sourceType: 'forwarder',
    sourceLabel: '客户 EMQX（生产）',
    title: '北向出口不可达',
    detail: '出口 1 连接中断 42s，期间数据转入磁盘队列（3.2 GB，可续传 ≈4.1 天），本地采集继续。',
    firstSeenAt: '2026-09-23 11:48:02',
    lastSeenAt: '2026-09-23 11:48:44',
    count: 1,
    state: 'resolved',
    stateLabel: '已恢复',
    ackedBy: '张工',
    note: '网络抖动，已自动重连并补发完成。',
  },
  {
    id: 'al-004',
    level: 'warning',
    levelLabel: '提示',
    sourceType: 'license',
    sourceLabel: '授权',
    title: '试用剩余 2 天 14 小时',
    detail: '到期后降级为免费基础版（8 设备 / ≥1s / 无北向转发 / 无 OTA）。',
    firstSeenAt: '2026-09-21 10:00:00',
    lastSeenAt: '2026-09-23 12:00:00',
    count: 1,
    state: 'acking',
    stateLabel: '已确认',
    ackedBy: '张工',
    note: '已联系采购走合同流程。',
  },
  {
    id: 'al-005',
    level: 'minor',
    levelLabel: '次要',
    sourceType: 'system',
    sourceLabel: '磁盘队列',
    title: '队列水位偏高',
    detail: '磁盘队列 3.2 / 10 GB（32%），按当前速率可续传 ≈4.1 天。',
    firstSeenAt: '2026-09-23 09:12:00',
    lastSeenAt: '2026-09-23 12:31:00',
    count: 1,
    state: 'open',
    stateLabel: '待处理',
    ackedBy: '',
    note: '',
  },
];

/** 审计日志（含一条 403 拒绝记录，证明前端门控 + Rust 侧拒绝双层）。 */
const auditLogs: AuditEntry[] = [
  {
    id: 'log-001',
    ts: '2026-09-23 12:31:40',
    actor: 'system',
    actorType: 'system',
    action: '北向补发完成',
    entityType: 'forwarder',
    entityLabel: '北向出口',
    entityId: 'fwd-001',
    detail: '补发 128,400 / 1,800,000 条（≈6 分钟）',
    ip: '127.0.0.1',
    result: 'success',
  },
  {
    id: 'log-002',
    ts: '2026-09-23 11:44:02',
    actor: '张工',
    actorType: 'human',
    action: '修改点位',
    entityType: 'point',
    entityLabel: '点位',
    entityId: 'pt-004',
    detail: '单位能耗公式变更（历史数据不重算）',
    ip: '192.168.10.5',
    result: 'success',
  },
  {
    id: 'log-003',
    ts: '2026-09-23 11:20:31',
    actor: '张工',
    actorType: 'human',
    action: '测试连接',
    entityType: 'device',
    entityLabel: '设备',
    entityId: 'dev-001',
    detail: '读取 DB1.0 = 214.6，耗时 42 ms',
    ip: '192.168.10.5',
    result: 'success',
  },
  {
    id: 'log-004',
    ts: '2026-09-23 10:12:30',
    actor: '李工',
    actorType: 'human',
    action: '处置告警',
    entityType: 'alarm',
    entityLabel: '告警',
    entityId: 'al-004',
    detail: '确认「试用剩余 2 天」，备注：已联系采购',
    ip: '192.168.10.8',
    result: 'success',
  },
  {
    id: 'log-005',
    ts: '2026-09-23 09:40:07',
    actor: '访客',
    actorType: 'human',
    action: '尝试修改北向编码',
    entityType: 'forwarder',
    entityLabel: '北向出口',
    entityId: 'fwd-001',
    detail: '—',
    ip: '192.168.10.31',
    result: 'denied',
  },
  {
    id: 'log-006',
    ts: '2026-09-23 09:02:11',
    actor: '张工',
    actorType: 'human',
    action: '导入点表',
    entityType: 'point',
    entityLabel: '点位',
    entityId: 'batch-20260923',
    detail: '有效 128 行 · 错误 2 行 · 警告 3 行（仅导入有效行）',
    ip: '192.168.10.5',
    result: 'success',
  },
  {
    id: 'log-007',
    ts: '2026-09-23 08:12:41',
    actor: 'system',
    actorType: 'system',
    action: '服务启动',
    entityType: 'system',
    entityLabel: '系统',
    entityId: 'gateway',
    detail: '版本 1.0.0 (build a91f3c2) · 加载 26 设备 / 1284 点位',
    ip: '127.0.0.1',
    result: 'success',
  },
];

/** 授权状态快照（`GET /api/license/status`）。 */
export const licenseSnapshot: MockLicense = {
  status: 'active',
  tierId: 'standard',
  tierName: '标准版',
  grade: 'B',
  remainingDays: 362,
  remainingText: '362 天',
  validUntil: '2027-09-20 12:31:00',
  lastHeartbeatAt: '2026-09-23 12:25:11',
  nextHeartbeatAt: '2026-09-23 18:25:11',
  anchorSources: '宿主板 UUID 3F0A… · 宿主物理网卡 MAC 00:E0:4C:…（已签名）',
  degradeReason: '',
  onExpireText: '到期后降级为免费基础版（Modbus 8 设备 / ≥1s / 无北向转发 / 无 OTA）。',
  capabilities: [
    { name: '最大设备数', included: true, note: '标准版 64 台（基础版 8 台）' },
    { name: '最小采集间隔', included: true, note: '100 ms（基础版 ≥1000 ms）' },
    { name: '北向转发', included: true, note: '多路出口 + protobuf/JSON 编码' },
    { name: 'OTA 升级通道', included: true, note: '在线升级可用' },
    { name: '磁盘断点续传', included: true, note: '10 GB 队列' },
  ],
};

/**
 * 授权状态「已降级」演示快照（供后代页面演示降级横幅）。
 *
 * 与 `licenseSnapshot` 并存，页面可按需切换以验证降级表现（§4.4 三种降级场景）。
 * 注意：这是**展示用**快照，真实降级判定在 Rust 侧。
 */
export const licenseDegradedSnapshot: MockLicense = {
  status: 'grace',
  tierId: 'free',
  tierName: '免费基础版',
  grade: 'C',
  remainingDays: 0,
  remainingText: '6 天',
  validUntil: '2026-09-23 12:31:00',
  lastHeartbeatAt: '2026-09-16 12:25:11',
  nextHeartbeatAt: '—',
  anchorSources: '宿主板 UUID 3F0A… · 宿主物理网卡 MAC 00:E0:4C:…（已签名）',
  degradeReason: '云端心跳超期（>7 天），北向转发已停用；本地采集继续。',
  onExpireText: '宽限期内恢复网络并完成心跳即可自动恢复标准版能力。',
  capabilities: [
    { name: '最大设备数', included: false, note: '当前 26 台，超基础版上限 8 台，仅前 8 台可采集' },
    { name: '最小采集间隔', included: false, note: '已强制 ≥1000 ms' },
    { name: '北向转发', included: false, note: '已停用' },
    { name: 'OTA 升级通道', included: false, note: '已停用' },
    { name: '磁盘断点续传', included: true, note: '10 GB 队列' },
  ],
};

// ===========================================================================
// 仓库 API（返回**拷贝**，避免调用方直接改写内部数组）
// ===========================================================================

/** 深拷贝辅助（结构化克隆，保证嵌套数组不被外部改写）。 */
function clone<T>(value: T): T {
  return JSON.parse(JSON.stringify(value)) as T;
}

/** 当前时间格式化为 `YYYY-MM-DD HH:mm:ss`。 */
function now(): string {
  const d = new Date();
  const p = (n: number): string => String(n).padStart(2, '0');
  return `${d.getFullYear()}-${p(d.getMonth() + 1)}-${p(d.getDate())} ${p(d.getHours())}:${p(d.getMinutes())}:${p(d.getSeconds())}`;
}

/** 追加审计日志（所有写操作的强制副作用）。 */
function pushAudit(entry: Omit<AuditEntry, 'id' | 'ts' | 'ip'> & { ip?: string }): void {
  auditLogs.unshift({
    id: `log-${Date.now()}-${auditLogs.length}`,
    ts: now(),
    ip: entry.ip ?? '192.168.10.5',
    ...entry,
  });
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

/** 仓库对外 API（页面层唯一数据入口）。 */
export const repo = {
  // ---------- 网关信息 ----------
  /** 网关基本信息。 */
  getGateway(): GatewayInfo {
    return clone(gateway);
  },

  // ---------- 设备 ----------
  /**
   * 按条件查询设备（分页）。
   *
   * @param query.status 状态筛选（'' = 全部）
   * @param query.protocol 协议筛选（'' = 全部）
   * @param query.keyword 关键字（匹配名称 / 连接摘要）
   * @param query.page 页码（从 1 开始）
   * @param query.pageSize 每页条数
   */
  queryDevices(query: {
    status: string;
    protocol: string;
    keyword: string;
    page: number;
    pageSize: number;
  }): Paged<DeviceRecord> {
    const kw = query.keyword.trim().toLowerCase();
    const filtered = devices.filter((d) => {
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
    const start = (query.page - 1) * query.pageSize;
    return { items: clone(filtered.slice(start, start + query.pageSize)), total: filtered.length, page: query.page };
  },

  /** 全部设备（不分页；供下拉 / 联动消费）。 */
  allDevices(): DeviceRecord[] {
    return clone(devices);
  },

  /** 按 id 取设备（不存在返回 null）。 */
  getDevice(id: string): DeviceRecord | null {
    const found = devices.find((d) => d.id === id);
    return found ? clone(found) : null;
  },

  // ---------- 点位 ----------
  /**
   * 按条件查询点位（分页）。
   *
   * @param query.deviceId 设备筛选（'' = 全部）
   * @param query.pointType 类型筛选（'' = 全部；physical / derived）
   * @param query.quality 质量筛选（'' = 全部）
   * @param query.keyword 关键字（匹配点位名 / 目标点 / 地址）
   * @param query.page 页码（从 1 开始）
   * @param query.pageSize 每页条数
   */
  queryPoints(query: {
    deviceId: string;
    pointType: string;
    quality: string;
    keyword: string;
    page: number;
    pageSize: number;
  }): Paged<PointRecord> {
    const kw = query.keyword.trim().toLowerCase();
    const filtered = points.filter((p) => {
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
    const start = (query.page - 1) * query.pageSize;
    return { items: clone(filtered.slice(start, start + query.pageSize)), total: filtered.length, page: query.page };
  },

  /** 全部点位（不分页；供实时监控与北向字段过滤消费）。 */
  allPoints(): PointRecord[] {
    return clone(points);
  },

  /** 某设备的全部点位（实时监控常用）。 */
  pointsOfDevice(deviceId: string): PointRecord[] {
    return clone(points.filter((p) => p.deviceId === deviceId));
  },

  // ---------- 北向出口 ----------
  /** 全部北向出口。 */
  allForwarders(): ForwarderRecord[] {
    return clone(forwarders);
  },

  /** 按 id 取出口。 */
  getForwarder(id: string): ForwarderRecord | null {
    const found = forwarders.find((f) => f.id === id);
    return found ? clone(found) : null;
  },

  /**
   * 更新出口编码（**每路出口独立**）。
   *
   * JSON 编码时前端应内联展示精度与性能提示（设计 §3.4 要点），本方法只改数据。
   */
  setForwarderEncoding(input: { id: string; encoding: Encoding; actor: string }): boolean {
    const target = forwarders.find((f) => f.id === input.id);
    if (!target) {
      return false;
    }
    const before = target.encoding;
    target.encoding = input.encoding;
    pushAudit({
      actor: input.actor,
      actorType: 'human',
      action: '修改北向编码',
      entityType: 'forwarder',
      entityLabel: '北向出口',
      entityId: target.id,
      detail: `${target.name}：${before} → ${input.encoding}`,
      result: 'success',
    });
    return true;
  },

  // ---------- 转发规则 ----------
  /** 全部转发规则。 */
  allRules(): RuleRecord[] {
    return clone(rules);
  },

  /** 切换规则启用状态。 */
  setRuleEnabled(input: { id: string; enabled: boolean; actor: string }): boolean {
    const target = rules.find((r) => r.id === input.id);
    if (!target) {
      return false;
    }
    target.enabled = input.enabled;
    pushAudit({
      actor: input.actor,
      actorType: 'human',
      action: input.enabled ? '启用转发规则' : '停用转发规则',
      entityType: 'forwarder',
      entityLabel: '转发规则',
      entityId: target.id,
      detail: `${target.name} → ${input.enabled ? '启用' : '停用'}`,
      result: 'success',
    });
    return true;
  },

  // ---------- 告警 ----------
  /** 全部告警（调用方自行按严重度排序）。 */
  allAlarms(): AlarmRecord[] {
    return clone(alarms);
  },

  /** 处置告警（仅状态 + 备注，**不改变授权**）。 */
  resolveAlarm(input: { id: string; state: AlarmState; note: string; actor: string }): boolean {
    const target = alarms.find((a) => a.id === input.id);
    if (!target) {
      return false;
    }
    target.state = input.state;
    target.stateLabel = input.state === 'open' ? '待处理' : input.state === 'acking' ? '已确认' : '已恢复';
    target.note = input.note;
    target.ackedBy = input.actor;
    pushAudit({
      actor: input.actor,
      actorType: 'human',
      action: '处置告警',
      entityType: 'alarm',
      entityLabel: '告警',
      entityId: target.id,
      detail: `${target.title} → ${target.stateLabel} · ${input.note}`,
      result: 'success',
    });
    return true;
  },

  // ---------- 审计 ----------
  /**
   * 按条件查询审计日志（分页）。
   *
   * @param query.actorType 操作者类型筛选（'' = 全部）
   * @param query.action 动作关键字
   * @param query.entityType 对象类型筛选（'' = 全部）
   * @param query.result 结果筛选（'' = 全部）
   * @param query.page 页码
   * @param query.pageSize 每页条数
   */
  queryAudit(query: {
    actorType: string;
    action: string;
    entityType: string;
    result: string;
    page: number;
    pageSize: number;
  }): Paged<AuditEntry> {
    const filtered = auditLogs.filter((log) => {
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
    const start = (query.page - 1) * query.pageSize;
    return { items: clone(filtered.slice(start, start + query.pageSize)), total: filtered.length, page: query.page };
  },

  // ---------- 授权（**只有展示 + 三个合法触点**）----------
  /** 授权状态快照。 */
  getLicense(): MockLicense {
    return clone(licenseSnapshot);
  },

  /**
   * 演示用：切换到「已降级」快照（验证降级横幅）。
   * 真实系统中该状态由 Rust 侧心跳校验决定，前端不可写入。
   */
  getLicenseDegraded(): MockLicense {
    return clone(licenseDegradedSnapshot);
  },

  /**
   * 提交激活码（客户端合法触点 2）。
   *
   * 只做本地校验 + 记录审计，**不做任何授权判定**（判定在 Rust 侧）。
   */
  activate(input: { code: string; actor: string }): { ok: boolean; message: string } {
    const normalized = input.code.trim().toUpperCase();
    if (!normalized) {
      return { ok: false, message: '请输入激活码' };
    }
    if (!/^IOT-\d{4}-[A-Z0-9]{4}-[A-Z0-9]{4}-[A-Z0-9]{4}-[A-Z0-9]{2}$/.test(normalized)) {
      return { ok: false, message: '激活码格式不正确，应形如 IOT-2026-XXXX-XXXX-XXXX-XX' };
    }
    pushAudit({
      actor: input.actor,
      actorType: 'human',
      action: '提交激活码',
      entityType: 'license',
      entityLabel: '授权',
      entityId: gateway.machineCode,
      detail: `激活码 ${normalized.slice(0, 9)}****（真实校验在网关侧）`,
      result: 'success',
    });
    return { ok: true, message: '激活请求已提交，网关将在 3 秒内完成联网校验。' };
  },

  /**
   * 提交换机申请（客户端合法触点 3）。
   *
   * **仅提交申请**：本模块**无**废弃 / 解绑 / 重置试用的任何方法（红线 2）。
   * 厂商在后台废弃原激活码并重新发放，客户再用新码激活。
   */
  submitTransferRequest(input: {
    oldMachineCode: string;
    newMachineCode: string;
    reason: string;
    contact: string;
    actor: string;
  }): { ok: boolean; ticketId: string; message: string } {
    if (!input.reason.trim()) {
      return { ok: false, ticketId: '', message: '请填写换机原因' };
    }
    const ticketId = `RV-${new Date().getFullYear()}-${String(Date.now()).slice(-6)}`;
    pushAudit({
      actor: input.actor,
      actorType: 'human',
      action: '提交换机申请',
      entityType: 'license',
      entityLabel: '授权',
      entityId: ticketId,
      detail: `原机器码 ${input.oldMachineCode} → 新机器码 ${input.newMachineCode || '（未检测）'} · 原因：${input.reason}`,
      result: 'success',
    });
    return {
      ok: true,
      ticketId,
      message: `申请已受理，受理编号 ${ticketId}。厂商将在 1 个工作日内废弃原激活码并重新发放。`,
    };
  },

  /** 记录前端侧审计（如「复制机器码」，由授权页复制按钮触发）。 */
  logCopyMachineCode(input: { actor: string }): void {
    pushAudit({
      actor: input.actor,
      actorType: 'human',
      action: '复制机器码',
      entityType: 'license',
      entityLabel: '授权',
      entityId: gateway.machineCode,
      detail: '客户端授权触点 1：复制机器码用于联系厂商',
      result: 'success',
    });
  },

  // ---------- 设备写操作（原型：内存态；真实系统落库 + 联网校验） ----------
  /**
   * 新增设备（生成 id + 写审计 + 初始离线条）。
   *
   * 真实系统中该写操作会触发网关侧连接探测与授权校验；原型仅维护内存态。
   * 前端**不**在此做任何授权判定（判定在 Rust 侧）。
   */
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
      createdAt: now(),
    };
    devices.push(record);
    pushAudit({
      actor: input.actor,
      actorType: 'human',
      action: '新增设备',
      entityType: 'device',
      entityLabel: '设备',
      entityId: id,
      detail: `${record.name} · ${protocolLabel} · ${record.connectionSummary}`,
      result: 'success',
    });
    return clone(record);
  },

  /** 删除设备（级联删除其下点位；二次确认已在页面层完成）。 */
  deleteDevice(input: { id: string; actor: string }): boolean {
    const idx = devices.findIndex((d) => d.id === input.id);
    if (idx < 0) {
      return false;
    }
    const removed = devices[idx];
    for (let i = points.length - 1; i >= 0; i -= 1) {
      if (points[i].deviceId === input.id) {
        points.splice(i, 1);
      }
    }
    devices.splice(idx, 1);
    pushAudit({
      actor: input.actor,
      actorType: 'human',
      action: '删除设备',
      entityType: 'device',
      entityLabel: '设备',
      entityId: input.id,
      detail: `${removed.name}（含其下全部点位一并删除）`,
      result: 'success',
    });
    return true;
  },

  // ---------- 点位写操作 ----------
  /** 新增单点点位（自动回写所属设备 pointCount）。 */
  createPoint(input: PointDraft): PointRecord {
    const id = `pt-${String(Date.now()).slice(-7)}-${points.length}`;
    const device = devices.find((d) => d.id === input.deviceId);
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
      updatedAt: now(),
      stale: false,
    };
    points.push(record);
    if (device) {
      device.pointCount = points.filter((p) => p.deviceId === device.id).length;
    }
    pushAudit({
      actor: input.actor ?? '',
      actorType: 'human',
      action: '新增点位',
      entityType: 'point',
      entityLabel: '点位',
      entityId: id,
      detail: `${record.deviceName} / ${record.name} · ${record.targetKey}`,
      result: 'success',
    });
    return clone(record);
  },

  /** 删除点位（回写所属设备 pointCount）。 */
  deletePoint(input: { id: string; actor: string }): boolean {
    const idx = points.findIndex((p) => p.id === input.id);
    if (idx < 0) {
      return false;
    }
    const removed = points[idx];
    points.splice(idx, 1);
    const device = devices.find((d) => d.id === removed.deviceId);
    if (device) {
      device.pointCount = points.filter((p) => p.deviceId === device.id).length;
    }
    pushAudit({
      actor: input.actor,
      actorType: 'human',
      action: '删除点位',
      entityType: 'point',
      entityLabel: '点位',
      entityId: input.id,
      detail: `${removed.deviceName} / ${removed.name}`,
      result: 'success',
    });
    return true;
  },

  /**
   * 批量覆盖导入某设备的点表（先删旧点，再写入新点）。
   *
   * 行号 + 原因 + 允许值校验在页面层完成；此处只做落库 + 计数 + 审计。
   */
  replacePointsOfDevice(input: { deviceId: string; rows: PointDraft[]; actor: string }): number {
    for (let i = points.length - 1; i >= 0; i -= 1) {
      if (points[i].deviceId === input.deviceId) {
        points.splice(i, 1);
      }
    }
    const device = devices.find((d) => d.id === input.deviceId);
    let count = 0;
    for (const row of input.rows) {
      const isDerived = row.pointType === 'derived';
      const id = `pt-${String(Date.now()).slice(-7)}-${count}`;
      points.push({
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
        updatedAt: now(),
        stale: false,
      });
      count += 1;
    }
    if (device) {
      device.pointCount = points.filter((p) => p.deviceId === device.id).length;
    }
    pushAudit({
      actor: input.actor,
      actorType: 'human',
      action: '导入点表',
      entityType: 'point',
      entityLabel: '点位',
      entityId: input.deviceId,
      detail: `${device?.name ?? input.deviceId}：覆盖导入 ${count} 个点位`,
      result: 'success',
    });
    return count;
  },
};

// ===========================================================================
// 供页面层直接消费的枚举 / 选项常量
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

/** 当前操作者显示名（演示用，登录后由 session 提供）。 */
export const DEFAULT_ACTOR = '张工';

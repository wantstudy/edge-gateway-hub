/**
 * @file mock-data.ts
 * @module admin-console/mock/mock-data
 * @description 演示数据集与写操作模拟仓库（in-memory repository）。
 *
 * 定位：本应用是**可构建、可交互、可验证**的原型交付。真实数据来自 `/admin/*` 管理 API
 * （见 `docs/design/licensing-api.md` §2），仓库层的接口形状严格按该契约建模，
 * 以便后续只需把 `mock 仓库` 换成 `HTTP 客户端`，页面层零改动。
 *
 * ── 契约：与 licensing-api.md §2 的字段对齐 ──────────────────────────────────
 * 本模块的实体形状是 §2 响应体的**前端镜像**，字段名/语义保持逐字段可对应：
 *   · 激活码  ↔ §2.4 `issued → bound → revoked → reissued`，含 `reissued_from` 溯源链；
 *   · 设备    ↔ §2.5（机器码摘要 / 心跳 / 回执连续性 / 授权租约状态）；
 *   · 回执异常 ↔ §2.6（类型 / 次数 / 处置状态）；
 *   · 租户策略 ↔ §2.7（默认档位、心跳间隔、离线宽限、回执要求）；
 *   · 签名密钥 ↔ §2.8（kid / 算法 / 轮换与退役计划）；
 *   · 审计日志 ↔ §2.9（actor / action / entity / detail / ip / result）。
 * 时间字段统一用 `'YYYY-MM-DD HH:mm:ss'` 本地字符串；ID/码用**字符串**（见下）。
 *
 * ── 红线：大整数一律用 `string`（IEEE754 精度陷阱）─────────────────────────────
 * JS 的 `number` 是双精度浮点，安全整数上限 `Number.MAX_SAFE_INTEGER = 2^53-1`
 * = 9_007_199_254_740_991。任何可能超过它的**身份/序号/时间戳（毫秒/纳秒）/
 * 长整型计数器**都必须以 `string` 承载，否则会静默丢精度（如末位被抹平）。
 * 本模块现状核查（B1）：
 *   · `validUntil` / `validFrom` / `createdAt` / `ts` / `lastHeartbeatAt` … → `string` ✅
 *   · `code` / `machineCode` / `orderId` / `id` / `kid` → `string` ✅
 *   · `count` / `gapCount` / `deviceCount` / `licensedCount` / 分页 `page·pageSize·total`
 *     → `number`，但均为**小值业务计数**（≤ 三位数），远离 2^53，**无需改 string** ✅
 *   · 本仓库**不存在** `seq_from` / `seq_to` / `issued_at`（毫秒）等大整型字段。
 *   ⇒ 结论：当前无任何 `number` 字段需要改为 `string`（0 处迁移）。
 *   后续若接入真实 API 出现 `issued_at`（unix ms）、`seq_*`（回执序号）等字段，
 *   务必在本层就接成 `string`，不要把精度问题留给渲染层。
 *
 * ── 行为规则 ─────────────────────────────────────────────────────────────────
 *  - 激活码：状态机 issued → bound → revoked → reissued，且 `reissued_from` 溯源链
 *  - 废弃：**立即失效**（不只是标记），原设备租约作废，形成时间线节点与审计日志
 *  - 重发：**不自动解绑原设备**，可预绑定新机器码或留待首次激活绑定
 *  - 所有写操作必须落审计日志（actor / action / entity / detail / ip / result）
 *  - 写操作只改本模块内存数组 + 深拷贝返回，**绝不**让页面层拿到引用后误改真相
 */

/** 校验档位。 */
export type Grade = 'A' | 'B' | 'C';

/** 部署形态。 */
export type DeployMode = 'native' | 'docker';

/** 激活码状态（与 licensing-api.md §2.4 对齐）。 */
export type CodeStatus = 'issued' | 'bound' | 'revoked' | 'reissued';

/** 生命周期动作。 */
export type LifecycleAction = '发放' | '绑定设备' | '废弃' | '重发' | '再次绑定';

/** 时间线节点（按时间升序）。 */
export interface LifecycleNode {
  /** 时间（YYYY-MM-DD HH:mm:ss） */
  time: string;
  /** 动作 */
  action: LifecycleAction;
  /** 色调（已发生 = 实心；未发生 = 灰） */
  tone: 'ok' | 'warn' | 'danger' | 'info' | 'unknown';
  /** 关联对象（设备摘要 / 新码） */
  target?: string;
  /** 操作者 */
  operator?: string;
  /** 原因（高危操作必有） */
  reason?: string;
  /** 明细 */
  detail?: string;
}

/** 激活码记录。 */
export interface CodeRecord {
  /** 主键 */
  id: string;
  /** 激活码原文（列表页掩码显示，详情页按权限揭示） */
  code: string;
  /** 状态 */
  status: CodeStatus;
  /** 租户 */
  tenant: string;
  /** 套餐 */
  tier: string;
  /** 生效起始 */
  validFrom: string;
  /** 到期日（试用为相对描述） */
  validUntil: string;
  /** 来源订单号 */
  orderId: string;
  /** 创建时间 */
  createdAt: string;
  /** 绑定设备摘要（掩码态展示） */
  boundDeviceSummary: string | null;
  /** 绑定设备名 */
  boundDeviceName: string | null;
  /** 预绑定机器码（未激活时可能存在） */
  prebindMachineCode: string | null;
  /** 溯源：由哪个源码重发而来 */
  reissuedFrom: string | null;
  /** 溯源：是否已重发为新码 */
  reissuedTo: string | null;
  /** 生命周期时间线 */
  timeline: LifecycleNode[];
  /** 回执连续性摘要 */
  receiptContinuity: string;
  /** 备注 */
  note: string;
}

/** 设备记录。 */
export interface DeviceRecord {
  /** 主键 */
  id: string;
  /** 租户 */
  tenant: string;
  /** 设备名 */
  name: string;
  /** 机器码完整值 */
  machineCode: string;
  /** 机器码摘要（掩码展示） */
  machineSummary: string;
  /** 套餐 */
  tier: string;
  /** 部署形态 */
  deployMode: DeployMode;
  /** 镜像 digest（docker 才有） */
  imageDigest: string | null;
  /** 校验档位 */
  grade: Grade;
  /** 授权状态（active / trial / revoked_lease 等） */
  licenseStatus: string;
  /** 最近心跳时间 */
  lastHeartbeatAt: string;
  /** 回执健康状态 */
  receiptStatus: string;
  /**
   * 回执跳空区间数（**字符串口径**）。
   * 红线 4：后端 uint64 计数在 JSON 路径是字符串，经 Number() 会在 2^53-1 处静默舍入，
   * 因此记录层保留后端原始十进制串；无跳空时为 `''`。
   */
  gapCount: string;
  /** 关联激活码 id */
  boundCodeId: string | null;
  /** 关联激活码掩码显示 */
  boundCodeMasked: string | null;
  /** 客户端版本 */
  clientVersion: string;
  /** 首次激活时间 */
  firstActivatedAt: string;
  /** 人工备注（不改变授权） */
  anomalyNote: string;
}

/** 租户记录。 */
export interface TenantRecord {
  /** 主键 */
  id: string;
  /** 租户名 */
  name: string;
  /** 设备数 */
  deviceCount: number;
  /** 已授权数 */
  licensedCount: number;
  /** 默认档位 */
  defaultGrade: Grade;
  /** 默认套餐 */
  defaultTier: string;
  /** 心跳间隔 */
  heartbeatInterval: string;
  /** 离线宽限 */
  offlineGrace: string;
  /** 是否强制回执 */
  receiptRequired: boolean;
  /** 联系人 */
  contact: string;
  /** 侧边状态（用于列表内联编辑：策略是否启用） */
  enabled: boolean;
}

/** 回执异常记录。 */
export interface ReceiptAnomaly {
  /** 主键 */
  id: string;
  /** 设备摘要 */
  deviceSummary: string;
  /** 租户 */
  tenant: string;
  /** 异常类型（对应 STATUS_MAP 的 receipt_*） */
  type: string;
  /** 明细 */
  detail: string;
  /** 首次出现时间 */
  firstSeen: string;
  /** 次数 */
  count: number;
  /** 处置状态 */
  disposition: string;
  /** 是否已核实（用于切换处置状态按钮文案） */
  verified: boolean;
  /** 备注 */
  note: string;
}

/** 换机工单。 */
export interface TransferTicket {
  /** 受理编号 */
  id: string;
  /** 租户 */
  tenant: string;
  /** 原机器码摘要 */
  oldMachineSummary: string;
  /** 原机器码 */
  oldMachineCode: string;
  /** 新机器码（可能未提供） */
  newMachineCode: string;
  /** 原激活码 id */
  sourceCodeId: string;
  /** 原因 */
  reason: string;
  /** 提交时间 */
  submittedAt: string;
  /** 处置状态 */
  status: string;
  /** 处理说明 */
  resolution: string;
}

/** 审计日志条目。 */
export interface AuditEntry {
  /** 主键 */
  id: string;
  /** 时间 */
  ts: string;
  /** 操作者 */
  actor: string;
  /** 操作者类型（human / system） */
  actorType: string;
  /** 动作 */
  action: string;
  /** 对象类型 */
  entityType: string;
  /** 对象标识 */
  entityId: string;
  /** 对象类型中文（表格展示） */
  entityLabel: string;
  /** 原因 / 备注 */
  detail: string;
  /** 来源 IP */
  ip: string;
  /** 结果 */
  result: string;
}

/** 签名密钥。 */
export interface SigningKey {
  /** key id */
  kid: string;
  /** 算法 */
  algorithm: string;
  /** 状态（对应 STATUS_MAP 的 key_*） */
  status: string;
  /** 启用时间 */
  activatedAt: string;
  /** 退役时间 */
  retiredAt: string;
  /** 计划退役时间 */
  plannedRetireAt: string;
  /** 用途 */
  usage: string;
  /** 公钥指纹 */
  fingerprint: string;
}

/** 管理员账号。 */
export interface AdminUser {
  /** 账号 */
  account: string;
  /** 姓名 */
  name: string;
  /** 角色 */
  role: string;
  /** 状态（对应 STATUS_MAP 的 user_*） */
  status: string;
  /** 最近登录 */
  lastLoginAt: string;
}

// ---------------------------------------------------------------------------
// 演示数据
// ---------------------------------------------------------------------------

/** 激活码列表（4 种状态齐备 + 一条试用）。 */
const codes: CodeRecord[] = [
  {
    id: 'c-001',
    code: 'IOT-2026-8C3F-1234-ABCD-A1',
    status: 'bound',
    tenant: '华中智造集团',
    tier: '专业版',
    validFrom: '2026-09-12',
    validUntil: '2027-09-23',
    orderId: 'SO-2026-0912',
    createdAt: '2026-09-12 10:02:11',
    boundDeviceSummary: '6f2a9b31c1',
    boundDeviceName: '线1-网关-01',
    prebindMachineCode: null,
    reissuedFrom: null,
    reissuedTo: null,
    timeline: [
      { time: '2026-09-12 10:02:11', action: '发放', tone: 'ok', operator: '李工（运营）', detail: '备注：合同首期 · 有效期 12 个月' },
      { time: '2026-09-12 14:31:07', action: '绑定设备', tone: 'ok', target: '6f2a…c1（线1-网关-01，native）', detail: '宿主锚点指纹校验通过' },
    ],
    receiptContinuity: '近 24h 区间连续，无跳空',
    note: '',
  },
  {
    id: 'c-002',
    code: 'IOT-2026-8C3F-5678-EFGH-B7',
    status: 'issued',
    tenant: '华中智造集团',
    tier: '专业版',
    validFrom: '2026-09-12',
    validUntil: '2027-09-23',
    orderId: 'SO-2026-0913',
    createdAt: '2026-09-12 15:40:00',
    boundDeviceSummary: null,
    boundDeviceName: null,
    prebindMachineCode: null,
    reissuedFrom: null,
    reissuedTo: null,
    timeline: [{ time: '2026-09-12 15:40:00', action: '发放', tone: 'ok', operator: '李工（运营）', detail: '未预绑定机器码，等待客户首次激活' }],
    receiptContinuity: '尚未激活，无回执',
    note: '待客户安装后激活',
  },
  {
    id: 'c-003',
    code: 'IOT-2025-1D90-WXYZ-3KLM-3K',
    status: 'revoked',
    tenant: '华南包装',
    tier: '专业版',
    validFrom: '2025-09-01',
    validUntil: '2026-09-01',
    orderId: 'SO-2025-0441',
    createdAt: '2025-09-01 09:10:00',
    boundDeviceSummary: 'a19c4e7218',
    boundDeviceName: '包装1线-网关',
    prebindMachineCode: null,
    reissuedFrom: null,
    reissuedTo: 'c-004',
    timeline: [
      { time: '2025-09-01 09:10:00', action: '发放', tone: 'ok', operator: '李工（运营）' },
      { time: '2025-09-01 11:20:31', action: '绑定设备', tone: 'ok', target: 'a19c…41（包装1线-网关，native）' },
      {
        time: '2026-09-23 11:40:12',
        action: '废弃',
        tone: 'danger',
        operator: '李工（运营）',
        reason: '客户更换硬件',
        detail: '补充说明：主板返修后机器码变化，客户已寄回原主板',
      },
      {
        time: '2026-09-23 11:44:02',
        action: '重发',
        tone: 'warn',
        target: '新码 IOT-2026-77A2-…-Q4（继承 tier 专业版，有效期顺延 12 个月）',
        operator: '李工（运营）',
      },
    ],
    receiptContinuity: '废弃后停止回执',
    note: '',
  },
  {
    id: 'c-004',
    code: 'IOT-2026-77A2-3456-MNOP-Q4',
    status: 'reissued',
    tenant: '华南包装',
    tier: '专业版',
    validFrom: '2026-09-23',
    validUntil: '2027-06-30',
    orderId: 'SO-2026-0923',
    createdAt: '2026-09-23 11:44:02',
    boundDeviceSummary: 'b83d5f90a2',
    boundDeviceName: '包装2线-网关',
    prebindMachineCode: 'C1D2-4E5F-6A7B-8C9D',
    reissuedFrom: 'c-003',
    reissuedTo: null,
    timeline: [
      {
        time: '2026-09-23 11:44:02',
        action: '重发',
        tone: 'warn',
        target: '源码 IOT-2025-1D90-…-3K',
        operator: '李工（运营）',
        detail: '预绑定机器码 C1D2-4E5F-6A7B-8C9D · 继承 tier 专业版',
      },
      { time: '2026-09-23 12:10:44', action: '再次绑定', tone: 'ok', target: 'b83d…92（包装2线-网关，docker）' },
    ],
    receiptContinuity: '近 24h 区间连续，无跳空',
    note: '',
  },
  {
    id: 'c-005',
    code: 'IOT-2026-3F19-7890-QRST-M2',
    status: 'bound',
    tenant: '西南制药',
    tier: '免费版',
    validFrom: '2026-09-19',
    validUntil: '2026-09-26',
    orderId: 'SO-2026-1001',
    createdAt: '2026-09-19 08:30:00',
    boundDeviceSummary: 'c7e10a44b3',
    boundDeviceName: '制粒车间-网关',
    prebindMachineCode: null,
    reissuedFrom: null,
    reissuedTo: null,
    timeline: [
      { time: '2026-09-19 08:30:00', action: '发放', tone: 'ok', operator: '王工（运营）', detail: '试用 7 天' },
      { time: '2026-09-19 09:05:19', action: '绑定设备', tone: 'ok', target: 'c7e1…0a（制粒车间-网关，native）' },
    ],
    receiptContinuity: 'C 档仅心跳，无回执',
    note: '试用中，剩余 2 天',
  },
];

/** 设备列表（含异常行）。 */
const devices: DeviceRecord[] = [
  {
    id: 'd-001',
    tenant: '华中智造集团',
    name: '线1-网关-01',
    machineCode: '6F2A9B31C1D4407E',
    machineSummary: '6f2a9b31c1',
    tier: '专业版',
    deployMode: 'native',
    imageDigest: null,
    grade: 'B',
    licenseStatus: 'active',
    lastHeartbeatAt: '2026-09-23 12:25:11',
    receiptStatus: 'receipt_ok',
    gapCount: '',
    boundCodeId: 'c-001',
    boundCodeMasked: 'IOTDAQ-****-****-****-A1',
    clientVersion: 'v1.0.0 (a91f3c2)',
    firstActivatedAt: '2026-09-12 14:31:07',
    anomalyNote: '',
  },
  {
    id: 'd-002',
    tenant: '华南包装',
    name: '包装2线-网关',
    machineCode: 'B83D5F90A2C41D77',
    machineSummary: 'b83d5f90a2',
    tier: '专业版',
    deployMode: 'docker',
    imageDigest: 'sha256:6b1f0c9d2a7e',
    grade: 'B',
    licenseStatus: 'active',
    lastHeartbeatAt: '2026-09-23 12:19:40',
    receiptStatus: 'receipt_gap',
    gapCount: '2',
    boundCodeId: 'c-004',
    boundCodeMasked: 'IOTDAQ-****-****-****-Q4',
    clientVersion: 'v1.0.0 (a91f3c2)',
    firstActivatedAt: '2026-09-23 12:10:44',
    anomalyNote: '',
  },
  {
    id: 'd-003',
    tenant: '西南制药',
    name: '制粒车间-网关',
    machineCode: 'C7E10A44B35D2981',
    machineSummary: 'c7e10a44b3',
    tier: '免费版',
    deployMode: 'native',
    imageDigest: null,
    grade: 'C',
    licenseStatus: 'trial',
    lastHeartbeatAt: '2026-09-23 11:58:02',
    receiptStatus: 'receipt_na',
    gapCount: '',
    boundCodeId: 'c-005',
    boundCodeMasked: 'IOTDAQ-****-****-****-M2',
    clientVersion: 'v1.0.0 (a91f3c2)',
    firstActivatedAt: '2026-09-19 09:05:19',
    anomalyNote: '',
  },
  {
    id: 'd-004',
    tenant: '华南包装',
    name: '包装1线-网关',
    machineCode: 'A19C4E7218B3F65D',
    machineSummary: 'a19c4e7218',
    tier: '专业版',
    deployMode: 'native',
    imageDigest: null,
    grade: 'B',
    licenseStatus: 'revoked_lease',
    lastHeartbeatAt: '2026-09-22 10:02:18',
    receiptStatus: 'receipt_missing',
    gapCount: '',
    boundCodeId: 'c-003',
    boundCodeMasked: 'IOTDAQ-****-****-****-3K',
    clientVersion: 'v0.9.4 (b21c8f1)',
    firstActivatedAt: '2025-09-01 11:20:31',
    anomalyNote: '客户已寄回主板，等待重装',
  },
  {
    id: 'd-005',
    tenant: '华中智造集团',
    name: '线2-网关-01（容器）',
    machineCode: '3D70F5C29B1E48A6',
    machineSummary: '3d70f5c29b',
    tier: '专业版',
    deployMode: 'docker',
    imageDigest: 'sha256:9c4a1b7e3f20',
    grade: 'B',
    licenseStatus: 'active',
    lastHeartbeatAt: '2026-09-23 12:26:33',
    receiptStatus: 'receipt_ok',
    gapCount: '',
    boundCodeId: null,
    boundCodeMasked: null,
    clientVersion: 'v1.0.0 (a91f3c2)',
    firstActivatedAt: '2026-07-04 16:22:10',
    anomalyNote: '',
  },
];

/** 租户列表。 */
const tenants: TenantRecord[] = [
  {
    id: 't-001',
    name: '华中智造集团',
    deviceCount: 128,
    licensedCount: 124,
    defaultGrade: 'B',
    defaultTier: '专业版',
    heartbeatInterval: '24 小时',
    offlineGrace: '7 天',
    receiptRequired: true,
    contact: '张工 138****0000',
    enabled: true,
  },
  {
    id: 't-002',
    name: '华南包装',
    deviceCount: 86,
    licensedCount: 80,
    defaultGrade: 'B',
    defaultTier: '专业版',
    heartbeatInterval: '24 小时',
    offlineGrace: '7 天',
    receiptRequired: true,
    contact: '李经理 139****1111',
    enabled: true,
  },
  {
    id: 't-003',
    name: '西南制药',
    deviceCount: 12,
    licensedCount: 3,
    defaultGrade: 'C',
    defaultTier: '免费版',
    heartbeatInterval: '12 小时',
    offlineGrace: '3 天',
    receiptRequired: false,
    contact: '王主任 137****2222',
    enabled: true,
  },
  {
    id: 't-004',
    name: '特种材料（大客户）',
    deviceCount: 46,
    licensedCount: 46,
    defaultGrade: 'A',
    defaultTier: '专业版',
    heartbeatInterval: '1 小时',
    offlineGrace: '14 天',
    receiptRequired: true,
    contact: '刘总 136****3333',
    enabled: false,
  },
];

/** 回执异常列表。 */
const anomalies: ReceiptAnomaly[] = [
  {
    id: 'a-001',
    deviceSummary: 'a19c…41',
    tenant: '华南包装',
    type: 'receipt_missing',
    detail: '超 26h 未上报',
    firstSeen: '2026-09-22 10:02:18',
    count: 1,
    disposition: 'pending_check',
    verified: false,
    note: '',
  },
  {
    id: 'a-002',
    deviceSummary: 'b83d…92',
    tenant: '华南包装',
    type: 'receipt_gap',
    detail: '#1204 → #1209（缺 5 个区间）',
    firstSeen: '2026-09-23 08:12:33',
    count: 2,
    disposition: 'pending_check',
    verified: false,
    note: '',
  },
  {
    id: 'a-003',
    deviceSummary: '6f2a…c1',
    tenant: '华中智造集团',
    type: 'receipt_gap',
    detail: '#0881 → #0883（缺 2 个区间）',
    firstSeen: '2026-09-21 17:40:09',
    count: 1,
    disposition: 'verified',
    verified: true,
    note: '已核实：客户检修断电，非破解',
  },
  {
    id: 'a-004',
    deviceSummary: '3d70…f5',
    tenant: '华中智造集团',
    type: 'receipt_delay',
    detail: '平均延迟 42s（允许补报）',
    firstSeen: '2026-09-23 12:00:00',
    count: 0,
    disposition: 'verified',
    verified: true,
    note: '常态，非异常',
  },
];

/** 换机工单列表。 */
const transfers: TransferTicket[] = [
  {
    id: 'RV-2026-0923-0118',
    tenant: '华南包装',
    oldMachineSummary: 'a19c…41',
    oldMachineCode: 'A19C4E7218B3F65D',
    newMachineCode: 'B83D5F90A2C41D77',
    sourceCodeId: 'c-003',
    reason: '主板损坏返修',
    submittedAt: '2026-09-22 09:40:00',
    status: 'processed',
    resolution: '已废弃旧码并重发 IOT-2026-77A2-…-Q4',
  },
  {
    id: 'RV-2026-0923-0119',
    tenant: '华中智造集团',
    oldMachineSummary: '6f2a…c1',
    oldMachineCode: '6F2A9B31C1D4407E',
    newMachineCode: 'C1D2-4E5F-6A7B-8C9D',
    sourceCodeId: 'c-001',
    reason: '系统重装导致机器码变化',
    submittedAt: '2026-09-23 10:12:30',
    status: 'pending',
    resolution: '',
  },
  {
    id: 'RV-2026-0923-0120',
    tenant: '西南制药',
    oldMachineSummary: 'c7e1…0a',
    oldMachineCode: 'C7E10A44B35D2981',
    newMachineCode: '',
    sourceCodeId: 'c-005',
    reason: '设备整体更换',
    submittedAt: '2026-09-23 11:30:12',
    status: 'pending',
    resolution: '',
  },
];

/** 签名密钥列表。 */
const keys: SigningKey[] = [
  {
    kid: 'kid-2026Q3',
    algorithm: 'Ed25519',
    status: 'key_active',
    activatedAt: '2026-07-01',
    retiredAt: '—',
    plannedRetireAt: '2026-10-01',
    usage: '签发 + 验签',
    fingerprint: '9c41f8…b2e7',
  },
  {
    kid: 'kid-2026Q2',
    algorithm: 'Ed25519',
    status: 'key_retiring',
    activatedAt: '2026-04-01',
    retiredAt: '2026-07-01',
    plannedRetireAt: '—',
    usage: '存量租约验签',
    fingerprint: '3f7a12…d904',
  },
  {
    kid: 'kid-2025Q4',
    algorithm: 'Ed25519',
    status: 'key_disabled',
    activatedAt: '2025-10-01',
    retiredAt: '2026-04-01',
    plannedRetireAt: '—',
    usage: '—',
    fingerprint: 'b8e2c5…41a6',
  },
];

/** 管理员账号列表。 */
const users: AdminUser[] = [
  { account: 'zhang.gong', name: '张工', role: 'system', status: 'user_enabled', lastLoginAt: '2026-09-23 09:02:11' },
  { account: 'li.gong', name: '李工', role: 'lic_ops', status: 'user_enabled', lastLoginAt: '2026-09-23 11:44:02' },
  { account: 'wang.gong', name: '王工', role: 'ops', status: 'user_enabled', lastLoginAt: '2026-09-23 10:12:30' },
  { account: 'risk.liu', name: '刘工', role: 'risk', status: 'user_enabled', lastLoginAt: '2026-09-22 18:40:07' },
  { account: 'ex.ops', name: '离职同事', role: 'ops', status: 'user_disabled', lastLoginAt: '2026-06-30 17:20:00' },
];

/** 审计日志（含一条 403 拒绝记录，证明前端门控 + 后端拒绝双层）。 */
const auditLogs: AuditEntry[] = [
  {
    id: 'log-001',
    ts: '2026-09-23 11:44:02',
    actor: '李工',
    actorType: 'human',
    action: '废弃激活码',
    entityType: 'activation_code',
    entityId: 'c-003',
    entityLabel: '激活码',
    detail: '客户更换硬件 / 主板返修后机器码变化',
    ip: '10.20.3.14',
    result: 'success',
  },
  {
    id: 'log-002',
    ts: '2026-09-23 11:44:02',
    actor: '李工',
    actorType: 'human',
    action: '重发激活码',
    entityType: 'activation_code',
    entityId: 'c-004',
    entityLabel: '激活码',
    detail: '同上（与废弃同事务，reissued_from=c-003）',
    ip: '10.20.3.14',
    result: 'success',
  },
  {
    id: 'log-003',
    ts: '2026-09-23 10:12:30',
    actor: '王工',
    actorType: 'human',
    action: '处理换机工单',
    entityType: 'transfer_ticket',
    entityId: 'RV-2026-0923-0119',
    entityLabel: '换机工单',
    detail: '系统重装',
    ip: '10.20.3.22',
    result: 'success',
  },
  {
    id: 'log-004',
    ts: '2026-09-23 09:02:11',
    actor: '李工',
    actorType: 'human',
    action: '查看激活码明文',
    entityType: 'activation_code',
    entityId: 'c-001',
    entityLabel: '激活码',
    detail: '客户电话核对',
    ip: '10.20.3.14',
    result: 'success',
  },
  {
    id: 'log-005',
    ts: '2026-09-22 18:40:07',
    actor: '张工',
    actorType: 'human',
    action: '修改租户档位',
    entityType: 'tenant',
    entityId: 't-003',
    entityLabel: '租户',
    detail: '西南制药 B → C，客户要求最小化数据外发',
    ip: '10.20.3.9',
    result: 'success',
  },
  {
    id: 'log-006',
    ts: '2026-09-22 17:15:44',
    actor: '访客',
    actorType: 'human',
    action: '尝试废弃激活码',
    entityType: 'activation_code',
    entityId: 'c-001',
    entityLabel: '激活码',
    detail: '—',
    ip: '10.20.3.31',
    result: 'denied',
  },
];

// ---------------------------------------------------------------------------
// 仓库 API（返回**拷贝**，避免调用方直接改写内部数组 —— 草稿隔离的第一道防线）
// ---------------------------------------------------------------------------

/** 深拷贝辅助（结构化克隆保证时间线等嵌套对象不被外部改写）。 */
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
    ip: entry.ip ?? '10.20.3.14',
    ...entry,
  });
}

/** 掩码工具（仓库内部使用，避免与页面层循环依赖）。 */
function maskOf(code: string): string {
  const parts = code.split('-').filter((p) => p.length > 0);
  const tail = parts.length > 0 ? parts[parts.length - 1].slice(-2) : '??';
  const prefix = parts[0] === 'IOT' ? 'IOTDAQ' : parts[0];
  return `${prefix}-****-****-****-${tail}`;
}

/** 仓库查询过滤条件。 */
export interface CodeQuery {
  /** 状态筛选（'' = 全部） */
  status: string;
  /** 租户筛选（'' = 全部） */
  tenant: string;
  /** 套餐筛选（'' = 全部） */
  tier: string;
  /** 订单号 / 关键字（匹配码值、机器码摘要、客户名） */
  keyword: string;
  /** 页码（从 1 开始） */
  page: number;
  /** 每页条数 */
  pageSize: number;
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

/** 仓库对外 API。 */
export const repo = {
  // ---------- 激活码 ----------
  /** 按条件查询激活码（分页）。 */
  queryCodes(query: CodeQuery): Paged<CodeRecord> {
    const kw = query.keyword.trim().toLowerCase();
    const filtered = codes.filter((c) => {
      if (query.status && c.status !== query.status) {
        return false;
      }
      if (query.tenant && c.tenant !== query.tenant) {
        return false;
      }
      if (query.tier && c.tier !== query.tier) {
        return false;
      }
      if (kw) {
        const haystack = `${c.code} ${c.boundDeviceSummary ?? ''} ${c.tenant} ${c.orderId}`.toLowerCase();
        if (!haystack.includes(kw)) {
          return false;
        }
      }
      return true;
    });
    const start = (query.page - 1) * query.pageSize;
    return { items: clone(filtered.slice(start, start + query.pageSize)), total: filtered.length, page: query.page };
  },

  /** 按 id 取激活码（不存在返回 null）。 */
  getCode(id: string): CodeRecord | null {
    const found = codes.find((c) => c.id === id);
    return found ? clone(found) : null;
  },

  /** 全部激活码（用于下拉/校验）。 */
  allCodes(): CodeRecord[] {
    return clone(codes);
  },

  /**
   * 签发激活码（对应 POST /admin/codes/issue）。
   * 生成 `count` 个新码并写入时间线，落审计。
   */
  issueCode(input: {
    tenant: string;
    tier: string;
    validFrom: string;
    validUntil: string;
    count: number;
    prebindMachineCode: string;
    note: string;
    actor: string;
  }): CodeRecord[] {
    const created: CodeRecord[] = [];
    const stamp = now();
    for (let i = 0; i < input.count; i += 1) {
      const serial = String(codes.length + 1 + i).padStart(4, '0');
      const random = Math.random().toString(36).slice(2, 6).toUpperCase();
      const code = `IOT-2026-${serial}-${random}-${String.fromCharCode(65 + ((codes.length + i) % 26))}${i}`;
      const record: CodeRecord = {
        id: `c-${Date.now()}-${i}`,
        code,
        status: 'issued',
        tenant: input.tenant,
        tier: input.tier,
        validFrom: input.validFrom,
        validUntil: input.validUntil,
        orderId: `SO-${new Date().getFullYear()}-${serial}`,
        createdAt: stamp,
        boundDeviceSummary: null,
        boundDeviceName: null,
        prebindMachineCode: input.prebindMachineCode || null,
        reissuedFrom: null,
        reissuedTo: null,
        timeline: [
          {
            time: stamp,
            action: '发放',
            tone: 'ok',
            operator: input.actor,
            detail: input.prebindMachineCode
              ? `预绑定机器码 ${input.prebindMachineCode} · ${input.note || '无备注'}`
              : `未预绑定，等待客户首次激活 · ${input.note || '无备注'}`,
          },
        ],
        receiptContinuity: '尚未激活，无回执',
        note: input.note,
      };
      codes.push(record);
      created.push(clone(record));
    }
    pushAudit({
      actor: input.actor,
      actorType: 'human',
      action: '发放激活码',
      entityType: 'activation_code',
      entityId: created.map((c) => c.id).join(','),
      entityLabel: '激活码',
      detail: `租户 ${input.tenant} · tier ${input.tier} · 数量 ${input.count} · 原因/备注：${input.note || '无'}`,
      result: 'success',
    });
    return created;
  },

  /**
   * 废弃激活码（对应 POST /admin/codes/{id}/revoke）。
   * 语义：**立即失效** —— 状态置 revoked、原设备租约作废、回执停止；落审计。
   */
  revokeCode(input: { id: string; reason: string; note: string; actor: string }): boolean {
    const target = codes.find((c) => c.id === input.id);
    if (!target) {
      return false;
    }
    if (target.status === 'revoked') {
      // 幂等：已废弃则直接成功（返回已撤销态）
      return true;
    }
    target.status = 'revoked';
    target.receiptContinuity = '废弃后停止回执';
    target.timeline.push({
      time: now(),
      action: '废弃',
      tone: 'danger',
      operator: input.actor,
      reason: input.reason,
      detail: `补充说明：${input.note}`,
    });
    // 立即作废原设备租约（不只是标记）
    if (target.boundDeviceSummary) {
      const device = devices.find((d) => d.machineSummary === target.boundDeviceSummary);
      if (device) {
        device.licenseStatus = 'revoked_lease';
      }
    }
    pushAudit({
      actor: input.actor,
      actorType: 'human',
      action: '废弃激活码',
      entityType: 'activation_code',
      entityId: target.id,
      entityLabel: '激活码',
      detail: `${input.reason} / ${input.note}`,
      result: 'success',
    });
    return true;
  },

  /**
   * 重发激活码（对应 POST /admin/codes/{id}/reissue）。
   * 语义：生成新码 → 建立 `reissued_from` 溯源链 → 源码标记 `reissuedTo`；
   *       **不自动解绑原设备**（原设备已随废弃失效）。
   */
  reissueCode(input: {
    sourceId: string;
    inheritTier: string;
    inheritValidUntil: string;
    prebindMachineCode: string;
    note: string;
    actor: string;
  }): CodeRecord | null {
    const source = codes.find((c) => c.id === input.sourceId);
    if (!source) {
      return null;
    }
    if (source.status !== 'revoked') {
      // 契约：未废弃不可重发（409 ORIGINAL_NOT_REVOKED）
      return null;
    }
    const stamp = now();
    const serial = String(codes.length + 1).padStart(4, '0');
    const random = Math.random().toString(36).slice(2, 6).toUpperCase();
    const code = `IOT-2026-${serial}-${random}-R${serial.slice(-2)}`;
    const record: CodeRecord = {
      id: `c-${Date.now()}-r`,
      code,
      status: 'issued',
      tenant: source.tenant,
      tier: input.inheritTier,
      validFrom: stamp.slice(0, 10),
      validUntil: input.inheritValidUntil,
      orderId: `SO-${new Date().getFullYear()}-${serial}`,
      createdAt: stamp,
      boundDeviceSummary: null,
      boundDeviceName: null,
      prebindMachineCode: input.prebindMachineCode || null,
      reissuedFrom: source.id,
      reissuedTo: null,
      timeline: [
        {
          time: stamp,
          action: '重发',
          tone: 'warn',
          target: `源码 ${maskOf(source.code)}`,
          operator: input.actor,
          detail: input.prebindMachineCode
            ? `预绑定机器码 ${input.prebindMachineCode} · 继承 tier ${input.inheritTier}`
            : `留待首次激活绑定 · 继承 tier ${input.inheritTier}`,
        },
      ],
      receiptContinuity: '尚未激活，无回执',
      note: input.note,
    };
    source.status = 'revoked';
    source.reissuedTo = record.id;
    source.timeline.push({
      time: stamp,
      action: '重发',
      tone: 'warn',
      target: `新码 ${maskOf(record.code)}`,
      operator: input.actor,
      detail: `继承 tier ${input.inheritTier} · 有效期至 ${input.inheritValidUntil}`,
    });
    codes.push(record);
    pushAudit({
      actor: input.actor,
      actorType: 'human',
      action: '重发激活码',
      entityType: 'activation_code',
      entityId: record.id,
      entityLabel: '激活码',
      detail: `源码 ${source.id}（reissued_from）· ${input.note || '无备注'}`,
      result: 'success',
    });
    return clone(record);
  },

  // ---------- 设备 ----------
  /** 按条件查询设备（分页）。 */
  queryDevices(query: {
    tenant: string;
    deployMode: string;
    licenseStatus: string;
    keyword: string;
    page: number;
    pageSize: number;
  }): Paged<DeviceRecord> {
    const kw = query.keyword.trim().toLowerCase();
    const filtered = devices.filter((d) => {
      if (query.tenant && d.tenant !== query.tenant) {
        return false;
      }
      if (query.deployMode && d.deployMode !== query.deployMode) {
        return false;
      }
      if (query.licenseStatus && d.licenseStatus !== query.licenseStatus) {
        return false;
      }
      if (kw) {
        const haystack = `${d.machineCode} ${d.machineSummary} ${d.name} ${d.tenant}`.toLowerCase();
        if (!haystack.includes(kw)) {
          return false;
        }
      }
      return true;
    });
    const start = (query.page - 1) * query.pageSize;
    return { items: clone(filtered.slice(start, start + query.pageSize)), total: filtered.length, page: query.page };
  },

  /** 按 id 取设备。 */
  getDevice(id: string): DeviceRecord | null {
    const found = devices.find((d) => d.id === id);
    return found ? clone(found) : null;
  },

  /** 全部设备。 */
  allDevices(): DeviceRecord[] {
    return clone(devices);
  },

  /** 标记设备异常（仅备注，**不改变授权**）。 */
  markDeviceAnomaly(input: { id: string; note: string; actor: string }): boolean {
    const device = devices.find((d) => d.id === input.id);
    if (!device) {
      return false;
    }
    device.anomalyNote = input.note;
    pushAudit({
      actor: input.actor,
      actorType: 'human',
      action: '标记设备异常',
      entityType: 'device',
      entityId: device.id,
      entityLabel: '设备',
      detail: `仅记录备注，不改变授权：${input.note}`,
      result: 'success',
    });
    return true;
  },

  // ---------- 租户 ----------
  /** 全部租户。 */
  allTenants(): TenantRecord[] {
    return clone(tenants);
  },

  /** 按 id 取租户。 */
  getTenant(id: string): TenantRecord | null {
    const found = tenants.find((t) => t.id === id);
    return found ? clone(found) : null;
  },

  /** 更新租户策略（高危：改档位 / 宽限 / 回执要求）。 */
  updateTenant(input: {
    id: string;
    defaultGrade: Grade;
    defaultTier: string;
    heartbeatInterval: string;
    offlineGrace: string;
    receiptRequired: boolean;
    reason: string;
    actor: string;
  }): boolean {
    const tenant = tenants.find((t) => t.id === input.id);
    if (!tenant) {
      return false;
    }
    const before = `${tenant.defaultGrade}/${tenant.heartbeatInterval}/${tenant.offlineGrace}/${tenant.receiptRequired ? '强制' : '不要求'}`;
    tenant.defaultGrade = input.defaultGrade;
    tenant.defaultTier = input.defaultTier;
    tenant.heartbeatInterval = input.heartbeatInterval;
    tenant.offlineGrace = input.offlineGrace;
    tenant.receiptRequired = input.receiptRequired;
    const after = `${tenant.defaultGrade}/${tenant.heartbeatInterval}/${tenant.offlineGrace}/${tenant.receiptRequired ? '强制' : '不要求'}`;
    pushAudit({
      actor: input.actor,
      actorType: 'human',
      action: '修改租户策略',
      entityType: 'tenant',
      entityId: tenant.id,
      entityLabel: '租户',
      detail: `${tenant.name}：${before} → ${after} · 原因：${input.reason}`,
      result: 'success',
    });
    return true;
  },

  /** 切换租户启用状态（提交按钮文案随状态取反）。 */
  setTenantEnabled(input: { id: string; enabled: boolean; actor: string }): boolean {
    const tenant = tenants.find((t) => t.id === input.id);
    if (!tenant) {
      return false;
    }
    tenant.enabled = input.enabled;
    pushAudit({
      actor: input.actor,
      actorType: 'human',
      action: input.enabled ? '启用租户' : '停用租户',
      entityType: 'tenant',
      entityId: tenant.id,
      entityLabel: '租户',
      detail: `${tenant.name} → ${input.enabled ? '启用' : '停用'}`,
      result: 'success',
    });
    return true;
  },

  // ---------- 回执异常 ----------
  /** 全部回执异常。 */
  allAnomalies(): ReceiptAnomaly[] {
    return clone(anomalies);
  },

  /** 标记异常处置（仅备注 + 转人工，**无「自动判定破解」动作**）。 */
  resolveAnomaly(input: { id: string; note: string; verified: boolean; actor: string }): boolean {
    const anomaly = anomalies.find((a) => a.id === input.id);
    if (!anomaly) {
      return false;
    }
    anomaly.note = input.note;
    anomaly.verified = input.verified;
    anomaly.disposition = input.verified ? 'verified' : 'pending_check';
    pushAudit({
      actor: input.actor,
      actorType: 'human',
      action: input.verified ? '核实回执异常' : '退回待核查',
      entityType: 'receipt_anomaly',
      entityId: anomaly.id,
      entityLabel: '回执异常',
      detail: `${anomaly.deviceSummary} · ${input.note}`,
      result: 'success',
    });
    return true;
  },

  // ---------- 换机工单 ----------
  /** 全部换机工单。 */
  allTransfers(): TransferTicket[] {
    return clone(transfers);
  },

  /**
   * 一键处理工单：「废弃旧码 + 重发新码」同一事务（目标 ≤3 次点击）。
   * 若 `reissue=false` 则仅废弃旧码。
   */
  processTransfer(input: {
    ticketId: string;
    prebindNew: boolean;
    inheritTier: string;
    validUntil: string;
    note: string;
    actor: string;
    reissue: boolean;
  }): { ok: boolean; ticket: TransferTicket | null; newCode: CodeRecord | null; message: string } {
    const ticket = transfers.find((t) => t.id === input.ticketId);
    if (!ticket) {
      return { ok: false, ticket: null, newCode: null, message: '工单不存在' };
    }
    if (ticket.status === 'processed') {
      // 幂等：重复提交返回已处理
      return { ok: true, ticket: clone(ticket), newCode: null, message: '该工单已处理（幂等返回）' };
    }
    const source = codes.find((c) => c.id === ticket.sourceCodeId);
    if (!source) {
      return { ok: false, ticket: clone(ticket), newCode: null, message: '关联激活码不存在' };
    }

    // 第一步：废弃旧码（立即失效）
    if (source.status !== 'revoked') {
      this.revokeCode({ id: source.id, reason: '客户更换硬件', note: input.note, actor: input.actor });
    }

    let newCode: CodeRecord | null = null;
    if (input.reissue) {
      // 第二步：重发新码（同一事务语义）
      newCode = this.reissueCode({
        sourceId: source.id,
        inheritTier: input.inheritTier,
        inheritValidUntil: input.validUntil,
        prebindMachineCode: input.prebindNew ? ticket.newMachineCode : '',
        note: input.note,
        actor: input.actor,
      });
    }

    ticket.status = 'processed';
    ticket.resolution = newCode
      ? `已废弃旧码并重发 ${maskOf(newCode.code)}`
      : '仅废弃旧码（客户将获得新码后自行激活）';

    pushAudit({
      actor: input.actor,
      actorType: 'human',
      action: '处理换机工单',
      entityType: 'transfer_ticket',
      entityId: ticket.id,
      entityLabel: '换机工单',
      detail: ticket.resolution,
      result: 'success',
    });

    return { ok: true, ticket: clone(ticket), newCode, message: ticket.resolution };
  },

  // ---------- 密钥 ----------
  /** 全部签名密钥。 */
  allKeys(): SigningKey[] {
    return clone(keys);
  },

  /** 轮换密钥：新 kid 置 active（与旧 kid 并存），旧 kid 置 retiring（仅验签）。 */
  rotateKey(input: { newKid: string; actor: string }): boolean {
    const current = keys.find((k) => k.status === 'key_active');
    if (current) {
      // 双密钥并存：旧 key 降级为 retiring，旧客户端仍可验签
      current.status = 'key_retiring';
      current.retiredAt = now().slice(0, 10);
      current.plannedRetireAt = '—';
      current.usage = '存量租约验签';
    }
    keys.unshift({
      kid: input.newKid,
      algorithm: 'Ed25519',
      status: 'key_active',
      activatedAt: now().slice(0, 10),
      retiredAt: '—',
      plannedRetireAt: '—',
      usage: '签发 + 验签',
      fingerprint: `${Math.random().toString(16).slice(2, 8)}…${Math.random().toString(16).slice(2, 6)}`,
    });
    pushAudit({
      actor: input.actor,
      actorType: 'human',
      action: '轮换签名密钥',
      entityType: 'signing_key',
      entityId: input.newKid,
      entityLabel: '签名密钥',
      detail: `新 kid ${input.newKid} 生效；旧 kid 置为已退役（仅验签），客户端内置公钥集仍可验签`,
      result: 'success',
    });
    return true;
  },

  /** 退役指定 kid（不再签发、仅保留验签宽限期）。 */
  retireKey(input: { kid: string; actor: string }): boolean {
    const key = keys.find((k) => k.kid === input.kid);
    if (!key) {
      return false;
    }
    key.status = 'key_retired';
    key.retiredAt = now().slice(0, 10);
    key.usage = '仅验签（宽限期）';
    pushAudit({
      actor: input.actor,
      actorType: 'human',
      action: '退役签名密钥',
      entityType: 'signing_key',
      entityId: key.kid,
      entityLabel: '签名密钥',
      detail: `${key.kid} 退役：不再签发，仅保留验签宽限期`,
      result: 'success',
    });
    return true;
  },

  // ---------- 审计 ----------
  /** 按条件查询审计日志（分页）。 */
  queryAudit(query: {
    actorType: string;
    action: string;
    entityType: string;
    entityId: string;
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
      if (query.entityId && !log.entityId.includes(query.entityId)) {
        return false;
      }
      return true;
    });
    const start = (query.page - 1) * query.pageSize;
    return { items: clone(filtered.slice(start, start + query.pageSize)), total: filtered.length, page: query.page };
  },

  /** 记录前端侧审计（如「查看激活码明文」，由 MaskedCode 的 reveal 事件触发）。 */
  logReveal(input: { entityId: string; actor: string }): void {
    pushAudit({
      actor: input.actor,
      actorType: 'human',
      action: '查看激活码明文',
      entityType: 'activation_code',
      entityId: input.entityId,
      entityLabel: '激活码',
      detail: '详情页按权限揭示明文',
      result: 'success',
    });
  },

  // ---------- 账号 ----------
  /** 全部管理员账号。 */
  allUsers(): AdminUser[] {
    return clone(users);
  },

  /** 切换账号启用 / 停用（按钮文案随状态取反）。 */
  setUserStatus(input: { account: string; enabled: boolean; actor: string }): boolean {
    const user = users.find((u) => u.account === input.account);
    if (!user) {
      return false;
    }
    user.status = input.enabled ? 'user_enabled' : 'user_disabled';
    pushAudit({
      actor: input.actor,
      actorType: 'human',
      action: input.enabled ? '启用账号' : '停用账号',
      entityType: 'admin_user',
      entityId: user.account,
      entityLabel: '管理员账号',
      detail: `${user.name}（${user.role}）→ ${input.enabled ? '启用' : '停用'}`,
      result: 'success',
    });
    return true;
  },

  /** 新增管理员账号（账号唯一；mock 不下发明文口令）。 */
  createUser(input: { account: string; name: string; role: string; actor: string }): boolean {
    const account = input.account.trim();
    if (!account || users.some((u) => u.account === account)) {
      return false;
    }
    users.push({
      account,
      name: input.name.trim() || account,
      role: input.role,
      status: 'user_enabled',
      lastLoginAt: '—',
    });
    pushAudit({
      actor: input.actor,
      actorType: 'human',
      action: '新增账号',
      entityType: 'admin_user',
      entityId: account,
      entityLabel: '管理员账号',
      detail: `${input.name || account}（${input.role}）`,
      result: 'success',
    });
    return true;
  },

  /** 修改管理员账号的资料 / 角色（mock 不处理口令）。 */
  updateUser(input: { account: string; name?: string; role?: string; actor: string }): boolean {
    const user = users.find((u) => u.account === input.account);
    if (!user) {
      return false;
    }
    if (input.name !== undefined && input.name.trim() !== '') {
      user.name = input.name.trim();
    }
    if (input.role !== undefined && input.role !== '') {
      user.role = input.role;
    }
    pushAudit({
      actor: input.actor,
      actorType: 'human',
      action: '修改账号',
      entityType: 'admin_user',
      entityId: user.account,
      entityLabel: '管理员账号',
      detail: `${user.name}（${user.role}）`,
      result: 'success',
    });
    return true;
  },

  /** 删除管理员账号。 */
  deleteUser(input: { account: string; actor: string }): boolean {
    const idx = users.findIndex((u) => u.account === input.account);
    if (idx < 0) {
      return false;
    }
    const [removed] = users.splice(idx, 1);
    pushAudit({
      actor: input.actor,
      actorType: 'human',
      action: '删除账号',
      entityType: 'admin_user',
      entityId: removed.account,
      entityLabel: '管理员账号',
      detail: `${removed.name}（${removed.role}）`,
      result: 'success',
    });
    return true;
  },

  // ---------- 仪表盘聚合 ----------
  /** 总览页所需的聚合数据。 */
  overview(): {
    tenantCount: number;
    licensedDevices: number;
    trialDevices: number;
    onlineDevices: number;
    offlineDevices: number;
    receiptOk: number;
    receiptGap: number;
    receiptMissing: number;
    receiptBadSig: number;
    pendingTransfers: number;
    pendingAnomalies: number;
    currentKid: string;
    kidRetireInDays: number;
    legacyClientCount: number;
    tenantDelta: number;
    newActivationsThisMonth: number;
    expiringIn7Days: number;
  } {
    const online = devices.filter((d) => d.licenseStatus === 'active' || d.licenseStatus === 'trial').length;
    const gap = devices.filter((d) => d.receiptStatus === 'receipt_gap').length;
    const missing = devices.filter((d) => d.receiptStatus === 'receipt_missing').length;
    const badSig = devices.filter((d) => d.receiptStatus === 'receipt_bad_sig').length;
    return {
      tenantCount: tenants.length,
      licensedDevices: devices.filter((d) => d.licenseStatus !== 'inactive').length,
      trialDevices: devices.filter((d) => d.licenseStatus === 'trial').length,
      onlineDevices: online,
      offlineDevices: devices.length - online,
      receiptOk: devices.filter((d) => d.receiptStatus === 'receipt_ok').length,
      receiptGap: gap,
      receiptMissing: missing,
      receiptBadSig: badSig,
      pendingTransfers: transfers.filter((t) => t.status === 'pending').length,
      pendingAnomalies: anomalies.filter((a) => !a.verified).length,
      currentKid: keys.find((k) => k.status === 'key_active')?.kid ?? '—',
      kidRetireInDays: 14,
      legacyClientCount: devices.filter((d) => d.clientVersion.startsWith('v0.9')).length,
      tenantDelta: 3,
      newActivationsThisMonth: 46,
      expiringIn7Days: 9,
    };
  },
};

/** 租户名清单（筛选下拉共用）。 */
export const TENANT_NAMES: readonly string[] = ['华中智造集团', '华南包装', '西南制药', '特种材料（大客户）'];

/** 套餐清单。 */
export const TIER_NAMES: readonly string[] = ['专业版', '免费版'];

/** 废弃原因枚举（对应 licensing-api.md §2.2 reason 枚举 + 文本）。 */
export const REVOKE_REASONS: readonly string[] = ['客户更换硬件', '设备报废', '合同终止', '误发放', '其他'];

/** 当前操作者显示名（演示用，登录后由 session 提供）。 */
export const DEFAULT_ACTOR = '李工（运营）';

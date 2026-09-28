/**
 * @file repo.ts
 * @module admin-console/api/repo
 * @description 统一数据层：`VITE_API_MODE` 分流 mock / real，页面层唯一数据入口。
 *
 * ── 设计（对齐 web-console 已验收的同款机制）────────────────────────────────
 *  · **导出与 `mock-data.ts` 同名同形**：`export *` 透传类型 / 常量，本模块再导出
 *    同名的 `repo`（局部导出优先于 `export *`，TS 合法）。页面只把
 *    `from '../mock/mock-data'` 换成 `from '../api/repo'`，渲染逻辑零改动。
 *  · **mock 模式（默认）**：`repo` 即 mock-data 仓库的 async 适配包装，完全不发
 *    网络请求，逐行为零回归。
 *  · **real 模式**（后端真实契约 = crates/licensing-server/src/http.rs）：
 *      - 登录鉴权：`POST /admin/auth/login` → `{token, role}`（client.ts 注入
 *        `Authorization: Bearer` / `X-Tenant-Id` / `X-Actor-Id`，401 统一跳登录）；
 *      - 已接通 GET 查询端点：`/admin/overview`、`/admin/codes`（分页 + 状态过滤）、
 *        `/admin/codes/:code_id`（详情 + 时间线 + 溯源链）、`/admin/tenants`、
 *        `/admin/devices`、`/admin/receipts/anomalies`、`/admin/keys`、
 *        `/admin/audit/logs`——preload 阶段并行拉取进 **reactive 缓存**，
 *        页面层同步读取（与 mock 同一接口形状），缓存更新即驱动视图刷新；
 *      - 已接通写端点：`POST /admin/codes/issue`（发放）、
 *        `POST /admin/codes/:id/revoke`（废弃）、`POST /admin/codes/:id/reissue`
 *        （重发）——废弃的 `confirm_tail8` 由详情端点的完整码值自动计算；
 *      - **账号 / 角色可配置**（缺口 #9 修复）：`GET/POST/PUT/DELETE /admin/accounts`
 *        + `GET /admin/roles`（GET 仅系统角色可见，非系统 403 静默）；
 *      - 后端确实没有的端点（租户策略写、换机工单、密钥轮换、异常处置）：
 *        读取型方法返回**诚实的空结果**（绝不回退假数据），写入型方法返回 false
 *        并把「后端缺口」写入全局提示横幅（`adminNotices`），做到清晰报错；
 *        后端未提供的数据维度以 `'—'` 展示，绝不编造。
 *  · **大数红线**：uint64 / unix 秒时间戳一律 `string` 原文透传（`pickStr`），
 *    **绝不 parseInt / Number()**——纳秒 / 毫秒时间戳、计数器原文显示；
 *    总览聚合的计数契约本身即 String（`OverviewResponse` 全 String）。
 *  · **不改 mock-data.ts 本身**（它是契约与 fallback）。
 */
import { reactive, ref } from 'vue';
import {
  repo as mockRepo,
  type AdminUser,
  type AuditEntry,
  type CodeQuery,
  type CodeRecord,
  type CodeStatus,
  type DeployMode,
  type DeviceRecord,
  type Grade,
  type LifecycleAction,
  type LifecycleNode,
  type Paged,
  type ReceiptAnomaly,
  type SigningKey,
  type TenantRecord,
  type TransferTicket,
} from '../mock/mock-data';

// 透传 mock-data 的类型 / 常量 / 枚举（`repo` 由下方局部导出遮蔽，属 ES 模块规范行为）
export * from '../mock/mock-data';

import { API_MODE, ApiError, adminRequest } from './client';
import { formatDateTime, formatTimestampText } from '../utils/time';

// 再导出模式常量，页面可统一从本模块取用
export { API_MODE } from './client';

// ===========================================================================
// 全局提示横幅（real 模式降级 / 操作失败的唯一展示通道，App.vue 消费）
// ===========================================================================

/** 提示条目。 */
export interface AdminNotice {
  /** 唯一 id（关闭用） */
  id: string;
  /** 色调：warn = 琥珀（降级提示）/ error = 红（操作失败） */
  tone: 'warn' | 'error';
  /** 文案 */
  message: string;
}

/** 响应式提示列表（最新在前，最多保留 4 条）。 */
export const adminNotices = reactive<AdminNotice[]>([]);

/** 已提示过的降级键（同一缺口只提示一次，避免刷屏）。 */
const noticedKeys = new Set<string>();

/** 追加一条提示。 */
export function pushNotice(tone: AdminNotice['tone'], message: string): void {
  adminNotices.unshift({ id: `notice-${Date.now()}-${adminNotices.length}`, tone, message });
  if (adminNotices.length > 4) {
    adminNotices.length = 4;
  }
}

/** 追加去重提示（同 key 只显示一次）。 */
function pushNoticeOnce(key: string, tone: AdminNotice['tone'], message: string): void {
  if (noticedKeys.has(key)) {
    return;
  }
  noticedKeys.add(key);
  pushNotice(tone, message);
}

/** 关闭一条提示（App.vue 横幅按钮）。 */
export function dismissNotice(id: string): void {
  const idx = adminNotices.findIndex((n) => n.id === id);
  if (idx >= 0) {
    adminNotices.splice(idx, 1);
  }
}

// ===========================================================================
// 宽容取值工具（后端字段缺失 / 类型漂移时回默认值，绝不抛渲染层异常）
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

// （大数红线：后端已接通端点的计数 / 时间戳 / 序号字段均按契约以 **String** 透传，
//  原文显示，绝不 parseInt / Number() 运算。）

/** 把 unknown 收敛为 Record（数组 / 非对象回空对象）。 */
function asRecord(value: unknown): Record<string, unknown> {
  return value !== null && typeof value === 'object' && !Array.isArray(value)
    ? (value as Record<string, unknown>)
    : {};
}

/** 当前时间格式化为 `YYYY-MM-DD HH:mm:ss`（与 mock-data 的约定一致）。 */
function nowText(): string {
  const d = new Date();
  const p = (n: number): string => String(n).padStart(2, '0');
  return `${d.getFullYear()}-${p(d.getMonth() + 1)}-${p(d.getDate())} ${p(d.getHours())}:${p(d.getMinutes())}:${p(d.getSeconds())}`;
}

/** `YYYY-MM-DD` → UTC 秒（**string**，大数红线：后端契约即 string）。 */
function dateToUtcSecs(dateStr: string): string {
  const ms = Date.parse(`${dateStr}T00:00:00Z`);
  if (!Number.isFinite(ms)) {
    throw new ApiError(0, `无效日期：${dateStr}`);
  }
  return String(Math.floor(ms / 1000));
}

/** 激活码 → 后 8 位确认串（去分隔符，废弃二次确认契约）。 */
function tail8Of(code: string): string {
  return code.replace(/[^0-9A-Za-z]/g, '').slice(-8).toUpperCase();
}

// ===========================================================================
// 统一仓库接口（读：同步缓存视图；写：async——real 模式必须等后端确认）
// ===========================================================================

/**
 * 总览聚合形状（mock-data `repo.overview()` 返回值的结构化镜像）。
 *
 * real 模式下后端契约（`OverviewResponse`）**全部计数为 String**（大数红线），
 * 故计数维度统一 `string` 原文透传（后端未提供的维度为 `'—'`）；
 * `tenantDelta` 为纯前端展示增量（非后端计数器），保持 number。
 */
export interface OverviewStats {
  tenantCount: string;
  licensedDevices: string;
  trialDevices: string;
  onlineDevices: string;
  offlineDevices: string;
  receiptOk: string;
  receiptGap: string;
  receiptMissing: string;
  receiptBadSig: string;
  pendingTransfers: string;
  pendingAnomalies: string;
  currentKid: string;
  kidRetireInDays: string;
  legacyClientCount: string;
  tenantDelta: number;
  newActivationsThisMonth: string;
  expiringIn7Days: string;
}

/** 换机工单处理结果（与 mock-data `repo.processTransfer` 同形）。 */
export interface ProcessTransferResult {
  ok: boolean;
  ticket: TransferTicket | null;
  newCode: CodeRecord | null;
  message: string;
}

/** 页面层唯一数据入口的接口形状（写操作 async 化，mock 经 Promise.resolve 适配）。 */
export interface AdminRepo {
  // ---------- 激活码 ----------
  queryCodes(query: CodeQuery): Paged<CodeRecord>;
  getCode(id: string): CodeRecord | null;
  allCodes(): CodeRecord[];
  issueCode(input: {
    tenant: string;
    tier: string;
    validFrom: string;
    validUntil: string;
    count: number;
    prebindMachineCode: string;
    note: string;
    actor: string;
  }): Promise<CodeRecord[]>;
  /**
   * 废弃激活码。
   * `secondApprover` 为**双人复核**字段，未填写时以 `null` 下发（后端 `RevokeCodeRequest.second_approver`
   * 契约：双人复核开启时必填）。接口必须原样透传，不得折进 `note` —— 三字段独立是审计可追溯的前提。
   */
  revokeCode(input: {
    id: string;
    reason: string;
    note: string;
    secondApprover?: string;
    actor: string;
  }): Promise<boolean>;
  reissueCode(input: {
    sourceId: string;
    inheritTier: string;
    inheritValidUntil: string;
    prebindMachineCode: string;
    note: string;
    actor: string;
  }): Promise<CodeRecord | null>;

  // ---------- 设备 ----------
  queryDevices(query: {
    tenant: string;
    deployMode: string;
    licenseStatus: string;
    keyword: string;
    page: number;
    pageSize: number;
  }): Paged<DeviceRecord>;
  getDevice(id: string): DeviceRecord | null;
  allDevices(): DeviceRecord[];
  markDeviceAnomaly(input: { id: string; note: string; actor: string }): Promise<boolean>;

  // ---------- 租户 ----------
  allTenants(): TenantRecord[];
  getTenant(id: string): TenantRecord | null;
  /**
   * 创建租户（real：`POST /admin/tenants`，仅 system 角色；mock：本地追加演示租户）。
   * 成功后 real 模式会刷新租户缓存；失败返回 false 并推全局横幅（真实原因）。
   */
  createTenant(input: {
    tenantId: string;
    name: string;
    contact: string;
    verifyMode: Grade;
    actor: string;
  }): Promise<boolean>;
  updateTenant(input: {
    id: string;
    defaultGrade: Grade;
    defaultTier: string;
    heartbeatInterval: string;
    offlineGrace: string;
    receiptRequired: boolean;
    reason: string;
    actor: string;
  }): Promise<boolean>;
  setTenantEnabled(input: { id: string; enabled: boolean; actor: string }): Promise<boolean>;

  // ---------- 回执异常 ----------
  allAnomalies(): ReceiptAnomaly[];
  resolveAnomaly(input: { id: string; note: string; verified: boolean; actor: string }): Promise<boolean>;

  // ---------- 换机工单 ----------
  allTransfers(): TransferTicket[];
  processTransfer(input: {
    ticketId: string;
    prebindNew: boolean;
    inheritTier: string;
    validUntil: string;
    note: string;
    actor: string;
    reissue: boolean;
  }): Promise<ProcessTransferResult>;

  // ---------- 密钥 ----------
  allKeys(): SigningKey[];
  rotateKey(input: { newKid: string; actor: string }): Promise<boolean>;
  retireKey(input: { kid: string; actor: string }): Promise<boolean>;

  // ---------- 审计 ----------
  queryAudit(query: {
    actorType: string;
    action: string;
    entityType: string;
    entityId: string;
    page: number;
    pageSize: number;
  }): Paged<AuditEntry>;
  logReveal(input: { entityId: string; actor: string }): Promise<void>;

  // ---------- 账号 ----------
  allUsers(): AdminUser[];
  setUserStatus(input: {
    account: string;
    enabled: boolean;
    reason: string;
    note: string;
    confirm: string;
    actor: string;
  }): Promise<boolean>;
  createUser(input: {
    account: string;
    name: string;
    role: string;
    password: string;
    actor: string;
  }): Promise<boolean>;
  updateUser(input: {
    account: string;
    name?: string;
    role?: string;
    password?: string;
    reason?: string;
    note?: string;
    confirm?: string;
    actor: string;
  }): Promise<boolean>;
  deleteUser(input: {
    account: string;
    reason: string;
    note: string;
    confirm: string;
    actor: string;
  }): Promise<boolean>;

  // ---------- 总览 ----------
  overview(): OverviewStats;

  /**
   * 激活趋势按日聚合（real：`GET /admin/stats/activations?days=N`，来自
   * audit_log 真实计数；mock：返回空数组——诚实空态，绝不伪造趋势曲线）。
   * `date` 为 UTC 日锚点 unix 秒（**String** 原文，大数红线）；计数为 String。
   */
  activationTrend(days: number): Promise<ActivationTrendPoint[]>;

  // ---------- 系统更新（OTA 升级包仓库） ----------
  /**
   * 升级包列表（real：`GET /admin/updates`，任意已登录角色可读；
   * mock：内存演示数据）。real 模式加载失败返回空数组并把**真实原因**
   * 写入 `updatesLoadError`（页面据此展示诚实空态，绝不回退 mock）。
   */
  listUpdates(): Promise<OtaPackageRecord[]>;
  /**
   * 上传并签名升级包（real：`POST /admin/updates`，**仅 system**；落库为 `draft`）。
   * `reason` / `noteDetail` / `confirm` 为**彼此独立**的三个字段，绝不拼接；
   * `confirm` 必须等于版本号（大小写不敏感精确匹配）。
   */
  uploadUpdate(input: {
    version: string;
    channel: string;
    payloadB64: string;
    note: string;
    reason: string;
    noteDetail: string;
    confirm: string;
    actor: string;
  }): Promise<OtaPackageRecord | null>;
  /**
   * 发布升级包（real：`POST /admin/updates/:version/publish`，**仅 system**；
   * `draft` / `disabled` → `published`）。
   */
  publishUpdate(
    version: string,
    input: { channel: string; reason: string; note: string; confirm: string; actor: string },
  ): Promise<OtaPackageRecord | null>;
  /**
   * 停用升级包（real：`POST /admin/updates/:version/disable`，**仅 system**；
   * `published` → `disabled`，可逆）。
   */
  disableUpdate(
    version: string,
    input: { channel: string; reason: string; note: string; confirm: string; actor: string },
  ): Promise<OtaPackageRecord | null>;
}

/** 激活趋势单日聚合（`ActivationStatsDay` 前端镜像；字段一律 String 原文透传）。 */
export interface ActivationTrendPoint {
  /** 该日 00:00:00 UTC 的 unix 秒（String）。 */
  date: string;
  /** 当日发放次数（String）。 */
  issue: string;
  /** 当日绑定次数（String）。 */
  bind: string;
  /** 当日废弃次数（String）。 */
  revoke: string;
}

// ===========================================================================
// 系统更新（OTA 升级包仓库，后端 GET/POST /admin/updates）
// ===========================================================================

/**
 * OTA 升级包列表记录（`OtaPackageItem` 前端镜像）。
 *
 * **大数红线**：`version` / `size` 一律 **String 原文透传**，绝不 `Number()` / `parseInt`——
 * 版本号为 u64 单调序十进制串，包体字节数同理，后端契约本身即 String。
 * `publishedAt` 已在**映射边界**格式化为可读文本（裸 epoch 秒串严禁进页面），
 * 未发布时为 `'—'`。
 */
export interface OtaPackageRecord {
  /** 版本号（u64 十进制字符串，原文透传）。 */
  version: string;
  /** 通道（`stable` / `beta`）。 */
  channel: string;
  /** 包体字节数（String 原文透传，绝不数值化）。 */
  size: string;
  /** 包体 SHA-256（小写 hex，原文透传）。 */
  payloadSha256: string;
  /** 签名密钥标识。 */
  kid: string;
  /** 状态（`draft` / `published` / `disabled` / `revoked` 原文）。 */
  status: string;
  /** 发布时间（**已格式化**的可读文本；未发布为 `'—'`）。 */
  publishedAt: string;
  /** 发布人。 */
  publishedBy: string;
  /** 发布说明。 */
  note: string;
}

/**
 * 系统更新列表 real 模式最近一次加载失败的真实原因（空串 = 无错误）。
 *
 * 供页面**诚实空态**展示：拿不到数据时给出后端返回的真实原因，
 * 绝不回落 mock、绝不伪造成功。
 */
export const updatesLoadError = ref('');

// ===========================================================================
// mock 模式适配器：mockRepo + async 包装（行为逐行零回归）
// ===========================================================================

/** mock 总览数值 → 契约形状（计数 String 化，展示结果与原值逐字一致）。 */
function mockOverviewToContract(): OverviewStats {
  const o = mockRepo.overview();
  return {
    tenantCount: String(o.tenantCount),
    licensedDevices: String(o.licensedDevices),
    trialDevices: String(o.trialDevices),
    onlineDevices: String(o.onlineDevices),
    offlineDevices: String(o.offlineDevices),
    receiptOk: String(o.receiptOk),
    receiptGap: String(o.receiptGap),
    receiptMissing: String(o.receiptMissing),
    receiptBadSig: String(o.receiptBadSig),
    pendingTransfers: String(o.pendingTransfers),
    pendingAnomalies: String(o.pendingAnomalies),
    currentKid: o.currentKid,
    kidRetireInDays: String(o.kidRetireInDays),
    legacyClientCount: String(o.legacyClientCount),
    tenantDelta: o.tenantDelta,
    newActivationsThisMonth: String(o.newActivationsThisMonth),
    expiringIn7Days: String(o.expiringIn7Days),
  };
}

/** mock 仓库的 async 适配包装（读方法直通，写方法 Promise.resolve）。 */

/** mock 模式下「新增租户」的本地追加列表（不改 mock-data.ts 契约本体）。 */
const mockCreatedTenants: TenantRecord[] = [];

/**
 * mock 模式下「系统更新」的内存演示数据（不改 mock-data.ts 契约本体）。
 *
 * 字段口径与后端 `OtaPackageItem` 逐字段对齐：`version` / `size` 为十进制**字符串**，
 * `publishedAt` 已是可读文本（与 real 模式映射后的形状一致），未发布为 `'—'`。
 */
const mockOtaPackages: OtaPackageRecord[] = [
  {
    version: '151',
    channel: 'beta',
    size: '45088768',
    payloadSha256: 'c2f5a9d1b7e46a0c8f3d5e2b9a17c40f6d8e3b5a1c9f7d2e4b6a8c0f1d3e5b7a',
    kid: 'kid-2026Q3',
    status: 'draft',
    publishedAt: '—',
    publishedBy: '—',
    note: '新增北向 MQTT 批量上报（beta 验证中）',
  },
  {
    version: '150',
    channel: 'stable',
    size: '41943040',
    payloadSha256: '7d1e4b8a0c3f6d9e2b5a8c1f4d7e0b3a6c9f2d5e8b1a4c7f0d3e6b9a2c5f8d1e',
    kid: 'kid-2026Q3',
    status: 'published',
    publishedAt: '2026-06-21 04:10:00',
    publishedBy: '李工（系统）',
    note: '修复采集断连后重连抖动',
  },
  {
    version: '148',
    channel: 'stable',
    size: '40894464',
    payloadSha256: 'a4c7f0d3e6b9a2c5f8d1e7d1e4b8a0c3f6d9e2b5a8c1f4d7e0b3a6c9f2d5e8b1',
    kid: 'kid-2026Q2',
    status: 'disabled',
    publishedAt: '2026-04-02 09:30:00',
    publishedBy: '李工（系统）',
    note: '回滚备用（已停用）',
  },
];

/** mock：按版本号找升级包（返回引用，用于就地改状态）。 */
function findMockOta(version: string): OtaPackageRecord | null {
  const v = version.trim();
  return mockOtaPackages.find((p) => p.version === v) ?? null;
}

function buildMockRepo(): AdminRepo {
  return {
    ...mockRepo,
    overview: mockOverviewToContract,
    issueCode: (input) => Promise.resolve(mockRepo.issueCode(input)),
    revokeCode: (input) => Promise.resolve(mockRepo.revokeCode(input)),
    reissueCode: (input) => Promise.resolve(mockRepo.reissueCode(input)),
    markDeviceAnomaly: (input) => Promise.resolve(mockRepo.markDeviceAnomaly(input)),
    allTenants(): TenantRecord[] {
      return [...mockRepo.allTenants(), ...mockCreatedTenants.map((t) => ({ ...t }))];
    },
    getTenant(id: string): TenantRecord | null {
      const created = mockCreatedTenants.find((t) => t.id === id);
      if (created) {
        return { ...created };
      }
      return mockRepo.getTenant(id);
    },
    createTenant: (input) => {
      const tenantId = input.tenantId.trim();
      const name = input.name.trim();
      if (!tenantId || !name) {
        pushNotice('error', '新增租户失败：租户 ID 与名称均不能为空。');
        return Promise.resolve(false);
      }
      if (mockCreatedTenants.some((t) => t.id === tenantId)) {
        pushNotice('error', `新增租户失败：租户 ID ${tenantId} 已存在。`);
        return Promise.resolve(false);
      }
      mockCreatedTenants.push({
        id: tenantId,
        name,
        deviceCount: 0,
        licensedCount: 0,
        defaultGrade: input.verifyMode,
        defaultTier: '—',
        heartbeatInterval: '—',
        offlineGrace: '—',
        receiptRequired: false,
        contact: input.contact.trim(),
        enabled: true,
      });
      return Promise.resolve(true);
    },
    // mock 模式无后端聚合：诚实空态（绝不伪造趋势曲线）。
    activationTrend: () => Promise.resolve([]),

    // ---------- 系统更新（mock：内存演示数据） ----------
    listUpdates: () => Promise.resolve(mockOtaPackages.map((p) => ({ ...p }))),
    uploadUpdate: (input) => {
      const version = input.version.trim();
      if (!version) {
        pushNotice('error', '上传升级包失败：版本号不能为空。');
        return Promise.resolve(null);
      }
      if (mockOtaPackages.some((p) => p.version === version)) {
        pushNotice('error', `上传升级包失败：版本号 ${version} 已存在。`);
        return Promise.resolve(null);
      }
      if (!input.payloadB64) {
        pushNotice('error', '上传升级包失败：未读取到包体内容。');
        return Promise.resolve(null);
      }
      if (!input.reason.trim()) {
        pushNotice('error', '上传升级包失败：未选择操作原因（原因将记入审计）。');
        return Promise.resolve(null);
      }
      if (input.noteDetail.trim().length < 10) {
        pushNotice('error', '上传升级包失败：补充说明不足 10 字。');
        return Promise.resolve(null);
      }
      if (input.confirm.trim().toLowerCase() !== version.toLowerCase()) {
        pushNotice('error', '上传升级包失败：二次确认串与版本号不一致。');
        return Promise.resolve(null);
      }
      // 由 base64 原文长度推包体字节数（演示；非 JSON 大数，不做 Number 化解析）。
      const size = String(Math.floor((input.payloadB64.replace(/=+$/, '').length * 3) / 4));
      const record: OtaPackageRecord = {
        version,
        channel: input.channel === 'beta' ? 'beta' : 'stable',
        size,
        payloadSha256: '—',
        kid: 'kid-2026Q3',
        status: 'draft',
        publishedAt: '—',
        publishedBy: '—',
        note: input.note.trim(),
      };
      mockOtaPackages.unshift(record);
      return Promise.resolve({ ...record });
    },
    publishUpdate: (version, input) => {
      const target = findMockOta(version);
      if (!target) {
        pushNotice('error', `发布升级包失败：未找到版本 ${version}。`);
        return Promise.resolve(null);
      }
      if (input.confirm.trim().toLowerCase() !== target.version.toLowerCase()) {
        pushNotice('error', '发布升级包失败：二次确认串与版本号不一致。');
        return Promise.resolve(null);
      }
      target.status = 'published';
      target.publishedAt = nowText();
      target.publishedBy = input.actor || '（未知操作者）';
      if (input.note.trim()) {
        target.note = input.note.trim();
      }
      return Promise.resolve({ ...target });
    },
    disableUpdate: (version, input) => {
      const target = findMockOta(version);
      if (!target) {
        pushNotice('error', `停用升级包失败：未找到版本 ${version}。`);
        return Promise.resolve(null);
      }
      if (input.confirm.trim().toLowerCase() !== target.version.toLowerCase()) {
        pushNotice('error', '停用升级包失败：二次确认串与版本号不一致。');
        return Promise.resolve(null);
      }
      target.status = 'disabled';
      return Promise.resolve({ ...target });
    },
    updateTenant: (input) => Promise.resolve(mockRepo.updateTenant(input)),
    setTenantEnabled: (input) => Promise.resolve(mockRepo.setTenantEnabled(input)),
    resolveAnomaly: (input) => Promise.resolve(mockRepo.resolveAnomaly(input)),
    processTransfer: (input) => Promise.resolve(mockRepo.processTransfer(input)),
    rotateKey: (input) => Promise.resolve(mockRepo.rotateKey(input)),
    retireKey: (input) => Promise.resolve(mockRepo.retireKey(input)),
    logReveal: (input) => Promise.resolve(mockRepo.logReveal(input)),
    setUserStatus: (input) => Promise.resolve(mockRepo.setUserStatus(input)),
    createUser: (input) =>
      Promise.resolve(
        mockRepo.createUser({
          account: input.account,
          name: input.name,
          role: input.role,
          actor: input.actor,
        }),
      ),
    updateUser: (input) =>
      Promise.resolve(
        mockRepo.updateUser({
          account: input.account,
          name: input.name,
          role: input.role,
          actor: input.actor,
        }),
      ),
    deleteUser: (input) =>
      Promise.resolve(mockRepo.deleteUser({ account: input.account, actor: input.actor })),
  };
}

// ===========================================================================
// real 模式仓库：全部 GET 查询端点 + 3 个写端点接通；缺失端点 → 诚实空态 + 缺口提示
// ===========================================================================

/** 后端通用分页信封（`PagedResponse` 前端镜像；total/page/page_size 契约即 String）。 */
interface PagedEnvelope {
  items: unknown[];
  total: string;
  page: string;
  page_size: string;
}

/** real 模式响应行 → IssuedCode 前端镜像（宽容取值）。 */
interface IssuedCodeView {
  codeId: string;
  code: string;
  status: string;
  prebind: string | null;
  reissuedFrom: string | null;
}

/** 解析发放 / 重发响应中的码行。 */
function parseIssuedRow(raw: unknown, idx: number): IssuedCodeView {
  const rec = asRecord(raw);
  const prebind = rec.prebind;
  const reissuedFrom = rec.reissued_from;
  return {
    codeId: pickStr(rec, 'code_id', `code-real-${idx}`),
    code: pickStr(rec, 'code', ''),
    status: pickStr(rec, 'status', 'issued'),
    prebind: typeof prebind === 'string' && prebind !== '' ? prebind : null,
    reissuedFrom: typeof reissuedFrom === 'string' && reissuedFrom !== '' ? reissuedFrom : null,
  };
}

// ---------------------------------------------------------------------------
// real 模式 reactive 缓存（preload 拉取 → 页面同步读 → 更新自动驱动视图）
// ---------------------------------------------------------------------------

/** real 模式码缓存（列表端点回填掩码值；详情端点回填完整码值 + 时间线）。 */
const realCodes = reactive<CodeRecord[]>([]);

/** real 模式设备缓存（GET /admin/devices）。 */
const realDevices = reactive<DeviceRecord[]>([]);

/** real 模式租户缓存（GET /admin/tenants）。 */
const realTenants = reactive<TenantRecord[]>([]);

/** real 模式回执异常缓存（GET /admin/receipts/anomalies）。 */
const realAnomalies = reactive<ReceiptAnomaly[]>([]);

/** real 模式签名密钥缓存（GET /admin/keys）。 */
const realKeys = reactive<SigningKey[]>([]);

/** real 模式审计日志缓存（GET /admin/audit/logs）。 */
const realAuditLogs = reactive<AuditEntry[]>([]);

/** real 模式管理员账号缓存（GET /admin/accounts；缺口 #9 修复）。 */
const realAccounts = reactive<AdminUser[]>([]);

/** real 模式总览聚合（GET /admin/overview；计数契约 String，未提供维度 '—'）。 */
const realStats = reactive<OverviewStats>({
  tenantCount: '—',
  licensedDevices: '—',
  trialDevices: '—',
  onlineDevices: '—',
  offlineDevices: '—',
  receiptOk: '—',
  receiptGap: '—',
  receiptMissing: '—',
  receiptBadSig: '—',
  pendingTransfers: '—',
  pendingAnomalies: '—',
  currentKid: '—',
  kidRetireInDays: '—',
  legacyClientCount: '—',
  tenantDelta: 0,
  newActivationsThisMonth: '—',
  expiringIn7Days: '—',
});

/** 单次拉取的分页上限（管理台数据量级；超出提示截断）。 */
const FETCH_CAP = 500;
/** 每页拉取条数（后端上限 200）。 */
const FETCH_PAGE_SIZE = 200;

/**
 * 分页拉取全部列表（`items.length < page_size` 即止，绝不 Number(total) 比较）。
 *
 * @returns 拉取到的条目（可能因 cap 截断）
 */
async function fetchAllPaged(
  path: string,
  query: Record<string, string>,
  buildItem: (raw: Record<string, unknown>, idx: number) => unknown,
): Promise<unknown[]> {
  const out: unknown[] = [];
  for (let page = 1; page <= Math.ceil(FETCH_CAP / FETCH_PAGE_SIZE); page += 1) {
    const data = await adminRequest<PagedEnvelope>(path, {
      method: 'GET',
      query: { ...query, page: String(page), page_size: String(FETCH_PAGE_SIZE) },
    });
    const items = data && Array.isArray(data.items) ? data.items : [];
    for (const [idx, item] of items.entries()) {
      out.push(buildItem(asRecord(item), out.length + idx));
    }
    if (items.length < FETCH_PAGE_SIZE || out.length >= FETCH_CAP) {
      break;
    }
  }
  return out;
}

// ---------------------------------------------------------------------------
// 后端行 → 页面记录映射（时间戳 / 计数一律 String 原文透传）
// ---------------------------------------------------------------------------

/** 码列表行（CodeSummary）→ 页面 CodeRecord（码值为掩码；完整码值经详情端点回填）。 */
function buildCodeRecordFromSummary(raw: Record<string, unknown>): CodeRecord {
  const boundDevice = raw.bound_device_id;
  return {
    id: pickStr(raw, 'code_id', ''),
    code: pickStr(raw, 'code_masked', ''),
    status: pickStr(raw, 'status', 'issued') as CodeStatus,
    tenant: pickStr(raw, 'tenant_id', ''),
    tier: pickStr(raw, 'tier', ''),
    validFrom: '',
    validUntil: pickStr(raw, 'valid_until', ''),
    orderId: '—',
    createdAt: pickStr(raw, 'created_at', ''),
    boundDeviceSummary: typeof boundDevice === 'string' && boundDevice !== '' ? boundDevice : null,
    boundDeviceName: null,
    prebindMachineCode: null,
    reissuedFrom: null,
    reissuedTo: null,
    timeline: [],
    receiptContinuity: '—',
    note: '',
  };
}

/** 后端时间线动作 → 前端 LifecycleAction + 色调（未知动作诚实降级）。 */
function mapTimelineAction(action: string): { action: LifecycleAction; tone: LifecycleNode['tone'] } {
  switch (action) {
    case 'issue':
      return { action: '发放', tone: 'ok' };
    case 'bind':
      return { action: '绑定设备', tone: 'ok' };
    case 'revoke':
      return { action: '废弃', tone: 'danger' };
    case 'reissue':
      return { action: '重发', tone: 'warn' };
    default:
      return { action: '再次绑定', tone: 'unknown' };
  }
}

/** 码详情行（CodeDetail）→ 页面 CodeRecord。 */
function buildCodeRecordFromDetail(raw: Record<string, unknown>): CodeRecord {
  const timelineRaw = Array.isArray(raw.timeline) ? raw.timeline : [];
  const chainRaw = Array.isArray(raw.reissued_chain) ? raw.reissued_chain : [];
  const boundDevice = raw.bound_device_id;
  const status = pickStr(raw, 'status', 'issued');
  const timeline: LifecycleNode[] = timelineRaw.map((entry) => {
    const t = asRecord(entry);
    const mapped = mapTimelineAction(pickStr(t, 'action', ''));
    return {
      // 后端 `at` 是**裸 epoch 秒串**（proto.rs TimelineEntry.at），必须在此边界格式化：
      // 页面 / ui-kit 组件严禁裸显时间戳（否则时间线会直接显示 10 位数字）。
      time: formatDateTime(pickStr(t, 'at', '')),
      action: mapped.action,
      tone: mapped.tone,
      operator: pickStr(t, 'actor', ''),
      detail: pickStr(t, 'detail', ''),
    };
  });
  return {
    id: pickStr(raw, 'code_id', ''),
    code: pickStr(raw, 'code', ''),
    status: status as CodeStatus,
    tenant: pickStr(raw, 'tenant_id', ''),
    tier: pickStr(raw, 'tier', ''),
    validFrom: pickStr(raw, 'valid_from', ''),
    validUntil: pickStr(raw, 'valid_until', ''),
    orderId: '—',
    createdAt: timeline.length > 0 ? timeline[0].time : '',
    boundDeviceSummary: typeof boundDevice === 'string' && boundDevice !== '' ? boundDevice : null,
    boundDeviceName: null,
    prebindMachineCode: null,
    reissuedFrom: typeof chainRaw[0] === 'string' && chainRaw[0] !== '' ? chainRaw[0] : null,
    reissuedTo: null,
    timeline,
    receiptContinuity: status === 'revoked' ? '废弃后停止回执' : '—',
    note: pickStr(raw, 'revoked_reason', '') || '',
  };
}

/** 把列表 / 详情 / 写响应得到的码记录并入缓存（已有记录的完整码值不会被掩码覆盖）。 */
function upsertCodeRecord(record: CodeRecord): void {
  if (!record.id) {
    return;
  }
  const existing = realCodes.find((c) => c.id === record.id);
  if (!existing) {
    realCodes.push(record);
    return;
  }
  if (record.code) {
    existing.code = record.code;
  }
  existing.status = record.status;
  existing.tenant = record.tenant || existing.tenant;
  existing.tier = record.tier || existing.tier;
  if (record.validFrom) {
    existing.validFrom = record.validFrom;
  }
  if (record.validUntil) {
    existing.validUntil = record.validUntil;
  }
  if (record.createdAt) {
    existing.createdAt = record.createdAt;
  }
  if (record.boundDeviceSummary !== null) {
    existing.boundDeviceSummary = record.boundDeviceSummary;
  }
  if (record.reissuedFrom !== null) {
    existing.reissuedFrom = record.reissuedFrom;
  }
  if (record.timeline.length > 0) {
    existing.timeline = record.timeline;
  }
  if (record.receiptContinuity !== '—') {
    existing.receiptContinuity = record.receiptContinuity;
  }
  if (record.note) {
    existing.note = record.note;
  }
}

/** 设备行（DeviceListItem）→ 页面 DeviceRecord。 */
function buildDeviceRecord(raw: Record<string, unknown>): DeviceRecord {
  // 后端设备行仅下发掩码机器码（machine_code_masked），无设备名 / tier / 档位字段——
  // 未提供维度以 '—' / 未知档位诚实展示，绝不编造。
  const gapSummary = pickStr(raw, 'receipt_gap_summary', '');
  const gapMatch = /gap=(\d+)/.exec(gapSummary);
  const lease = pickStr(raw, 'lease_status', '');
  const imageDigest = raw.image_digest;
  return {
    id: pickStr(raw, 'device_id', ''),
    tenant: pickStr(raw, 'tenant_id', ''),
    name: pickStr(raw, 'device_id', ''),
    machineCode: pickStr(raw, 'machine_code_masked', ''),
    machineSummary: pickStr(raw, 'machine_code_masked', ''),
    tier: '—',
    deployMode: (pickStr(raw, 'deploy_mode', 'native') === 'docker' ? 'docker' : 'native') as DeployMode,
    imageDigest: typeof imageDigest === 'string' && imageDigest !== '' ? imageDigest : null,
    grade: '—' as Grade,
    licenseStatus: lease || 'inactive',
    lastHeartbeatAt: pickStr(raw, 'last_heartbeat_at', ''),
    receiptStatus: lease ? (gapMatch ? 'receipt_gap' : 'receipt_ok') : 'receipt_na',
    // 红线 4：后端下发的 uint64 计数在 JSON 路径是**字符串**，不得经 Number() 处理（2^53-1 精度上限）。
    // 这里保留后端原始十进制串，渲染时直接输出，绝不转换 —— 大数一旦过 Number() 就会被静默舍入。
    gapCount: gapMatch ? gapMatch[1] : '',
    boundCodeId: null,
    boundCodeMasked: null,
    clientVersion: '—',
    firstActivatedAt: '',
    anomalyNote: '',
  };
}

/** 租户行（TenantItem）→ 页面 TenantRecord（后端未提供维度诚实 '—'）。 */
function buildTenantRecord(raw: Record<string, unknown>): TenantRecord {
  const verifyMode = pickStr(raw, 'verify_mode_default', '—');
  return {
    id: pickStr(raw, 'tenant_id', ''),
    name: pickStr(raw, 'name', ''),
    deviceCount: 0,
    licensedCount: 0,
    defaultGrade: (verifyMode === 'A' || verifyMode === 'B' || verifyMode === 'C' ? verifyMode : '—') as Grade,
    defaultTier: '—',
    heartbeatInterval: '—',
    offlineGrace: '—',
    receiptRequired: false,
    contact: pickStr(raw, 'contact', ''),
    enabled: true,
  };
}

/** 回执告警行（ReceiptAnomalyItem）→ 页面 ReceiptAnomaly。 */
function buildAnomalyRecord(raw: Record<string, unknown>): ReceiptAnomaly {
  const kind = pickStr(raw, 'kind', '');
  const typeMap: Record<string, string> = {
    gap: 'receipt_gap',
    overlap: 'receipt_rollback',
    missing: 'receipt_missing',
  };
  const seqFrom = pickStr(raw, 'seq_from', '');
  const seqTo = pickStr(raw, 'seq_to', '');
  return {
    id: pickStr(raw, 'id', ''),
    deviceSummary: pickStr(raw, 'device_mid', ''),
    tenant: '—',
    type: typeMap[kind] ?? kind,
    detail:
      `${pickStr(raw, 'detail', '')}` +
      (seqFrom ? `（区间 ${seqFrom} → ${seqTo}，前沿 ${pickStr(raw, 'last_seq_to', '')}）` : ''),
    firstSeen: pickStr(raw, 'created_at', ''),
    count: 1,
    disposition: 'pending_check',
    verified: false,
    note: '',
  };
}

/** 签名密钥行（SigningKeyItem）→ 页面 SigningKey。 */
function buildSigningKeyRecord(raw: Record<string, unknown>): SigningKey {
  const statusMap: Record<string, string> = {
    active: 'key_active',
    retiring: 'key_retiring',
    retired: 'key_retired',
  };
  const status = pickStr(raw, 'status', '');
  const retiredAt = raw.retired_at;
  const publicKey = pickStr(raw, 'public_key', '');
  return {
    kid: pickStr(raw, 'kid', ''),
    algorithm: '—',
    status: statusMap[status] ?? status,
    activatedAt: pickStr(raw, 'enabled_at', ''),
    retiredAt: typeof retiredAt === 'string' && retiredAt !== '' ? retiredAt : '—',
    plannedRetireAt: '—',
    usage: '—',
    fingerprint: publicKey ? `${publicKey.slice(0, 12)}…` : '—',
  };
}

/**
 * 后端升级包行（`OtaPackageItem`）→ 页面 `OtaPackageRecord`。
 *
 * 大数红线：`version` / `size` 原文透传，绝不数值化；`published_at` 是**裸 UTC 秒串**，
 * 在此映射边界即用 `formatDateTime` 格式化（页面 / 组件严禁裸显 epoch），空串给 `'—'`。
 */
function buildOtaPackageRecord(raw: Record<string, unknown>): OtaPackageRecord {
  const publishedAt = pickStr(raw, 'published_at', '');
  return {
    version: pickStr(raw, 'version', ''),
    channel: pickStr(raw, 'channel', ''),
    size: pickStr(raw, 'size', ''),
    payloadSha256: pickStr(raw, 'payload_sha256', ''),
    kid: pickStr(raw, 'kid', ''),
    status: pickStr(raw, 'status', ''),
    publishedAt: publishedAt === '' ? '—' : formatDateTime(publishedAt),
    publishedBy: pickStr(raw, 'published_by', ''),
    note: pickStr(raw, 'note', ''),
  };
}

/** 审计日志行（AuditLogItem）→ 页面 AuditEntry。 */
function buildAuditRecord(raw: Record<string, unknown>, idx: number): AuditEntry {
  const actor = pickStr(raw, 'actor', '');
  const entity = pickStr(raw, 'entity', '');
  const colonAt = entity.indexOf(':');
  const entityType = colonAt >= 0 ? entity.slice(0, colonAt) : entity;
  const entityId = colonAt >= 0 ? entity.slice(colonAt + 1) : entity;
  const labelMap: Record<string, string> = {
    activation_code: '激活码',
    tenant: '租户',
    device: '设备',
    receipt_anomaly: '回执异常',
    transfer_ticket: '换机工单',
    signing_key: '签名密钥',
    admin_user: '管理员账号',
  };
  return {
    id: `log-real-${idx}`,
    ts: pickStr(raw, 'ts', ''),
    actor,
    actorType: actor.includes(':') ? actor.slice(0, actor.indexOf(':')) : actor,
    action: pickStr(raw, 'action', ''),
    entityType,
    entityId,
    entityLabel: labelMap[entityType] ?? entityType,
    detail: pickStr(raw, 'detail', ''),
    ip: pickStr(raw, 'ip', ''),
    result: '—',
  };
}

/** 管理员账号行（AdminAccountItem）→ 页面 AdminUser（时间戳格式化，绝不裸显 epoch）。 */
function buildAdminUserFromRaw(raw: Record<string, unknown>): AdminUser {
  const statusRaw = pickStr(raw, 'status', 'active');
  const lastLogin = raw.last_login_at;
  return {
    account: pickStr(raw, 'account', ''),
    name: pickStr(raw, 'display_name', ''),
    role: pickStr(raw, 'role', ''),
    status: statusRaw === 'active' ? 'user_enabled' : 'user_disabled',
    lastLoginAt:
      typeof lastLogin === 'string' && lastLogin !== '' ? formatTimestampText(lastLogin) : '—',
  };
}

/** 拉取管理员账号列表（GET /admin/accounts；仅系统角色可见，非系统 403 静默）。 */
async function fetchAccounts(): Promise<void> {
  try {
    const rows = await fetchAllPaged('/admin/accounts', {}, buildAdminUserFromRaw);
    realAccounts.splice(0, realAccounts.length, ...(rows as AdminUser[]));
  } catch (cause) {
    if (cause instanceof ApiError && cause.status === 403) {
      // 非系统角色不可见账号列表（页面本身也仅系统可见）：静默，不刷横幅。
      return;
    }
    pushNoticeOnce('load-accounts', 'warn', `账号列表加载失败：${describeCause(cause)}`);
  }
}

// ---------------------------------------------------------------------------
// real 模式拉取动作（preload 并行执行；各失败独立提示，不互相阻断）
// ---------------------------------------------------------------------------

/** 拉取总览聚合（GET /admin/overview；计数 String 原文透传）。 */
async function fetchOverview(): Promise<void> {
  try {
    const data = asRecord(await adminRequest<unknown>('/admin/overview', { method: 'GET' }));
    const anomalous = pickStr(data, 'receipts_anomalous', '—');
    realStats.tenantCount = pickStr(data, 'tenants', '—');
    realStats.licensedDevices = pickStr(data, 'codes_bound', '—');
    realStats.receiptGap = anomalous;
    realStats.pendingAnomalies = anomalous;
    const activeKid = data.active_kid;
    realStats.currentKid = typeof activeKid === 'string' && activeKid !== '' ? activeKid : '—';
  } catch (cause) {
    pushNoticeOnce('load-overview', 'warn', `总览加载失败：${describeCause(cause)}`);
  }
}

/** 拉取激活码列表（GET /admin/codes；并入缓存，保留会话内发放的完整码值）。 */
async function fetchCodes(): Promise<void> {
  try {
    const rows = await fetchAllPaged('/admin/codes', {}, buildCodeRecordFromSummary);
    for (const raw of rows) {
      const record = raw as CodeRecord;
      const existing = realCodes.find((c) => c.id === record.id);
      if (existing) {
        // 已有记录（会话内发放的完整码值 / 详情回填）优先，仅同步服务端状态
        existing.status = record.status;
        existing.tenant = record.tenant || existing.tenant;
        existing.tier = record.tier || existing.tier;
        if (record.validUntil) {
          existing.validUntil = record.validUntil;
        }
        if (record.createdAt) {
          existing.createdAt = record.createdAt;
        }
        if (record.boundDeviceSummary !== null) {
          existing.boundDeviceSummary = record.boundDeviceSummary;
        }
      } else {
        realCodes.push(record);
      }
    }
    if (rows.length >= FETCH_CAP) {
      pushNoticeOnce('codes-truncated', 'warn', `激活码数量较多，列表仅加载前 ${FETCH_CAP} 条（可用筛选缩小范围）。`);
    }
  } catch (cause) {
    pushNoticeOnce('load-codes', 'warn', `激活码列表加载失败：${describeCause(cause)}`);
  }
}

/** 拉取设备列表（GET /admin/devices）。 */
async function fetchDevices(): Promise<void> {
  try {
    const rows = await fetchAllPaged('/admin/devices', {}, buildDeviceRecord);
    realDevices.splice(0, realDevices.length, ...(rows as DeviceRecord[]));
    if (rows.length >= FETCH_CAP) {
      pushNoticeOnce('devices-truncated', 'warn', `设备数量较多，列表仅加载前 ${FETCH_CAP} 条。`);
    }
  } catch (cause) {
    pushNoticeOnce('load-devices', 'warn', `设备列表加载失败：${describeCause(cause)}`);
  }
}

/** 拉取租户列表（GET /admin/tenants）。 */
async function fetchTenants(): Promise<void> {
  try {
    const rows = await fetchAllPaged('/admin/tenants', {}, buildTenantRecord);
    realTenants.splice(0, realTenants.length, ...(rows as TenantRecord[]));
  } catch (cause) {
    pushNoticeOnce('load-tenants', 'warn', `租户列表加载失败：${describeCause(cause)}`);
  }
}

/** 拉取回执异常（GET /admin/receipts/anomalies）。 */
async function fetchAnomalies(): Promise<void> {
  try {
    const rows = await fetchAllPaged('/admin/receipts/anomalies', {}, buildAnomalyRecord);
    realAnomalies.splice(0, realAnomalies.length, ...(rows as ReceiptAnomaly[]));
  } catch (cause) {
    pushNoticeOnce('load-anomalies', 'warn', `回执异常加载失败：${describeCause(cause)}`);
  }
}

/** 拉取签名密钥（GET /admin/keys）。 */
async function fetchKeys(): Promise<void> {
  try {
    const rows = await fetchAllPaged('/admin/keys', {}, buildSigningKeyRecord);
    realKeys.splice(0, realKeys.length, ...(rows as SigningKey[]));
  } catch (cause) {
    pushNoticeOnce('load-keys', 'warn', `签名密钥加载失败：${describeCause(cause)}`);
  }
}

/** 拉取审计日志（GET /admin/audit/logs）。 */
async function fetchAudit(): Promise<void> {
  try {
    const rows = await fetchAllPaged('/admin/audit/logs', {}, buildAuditRecord);
    realAuditLogs.splice(0, realAuditLogs.length, ...(rows as AuditEntry[]));
    if (rows.length >= FETCH_CAP) {
      pushNoticeOnce('audit-truncated', 'warn', `审计日志较多，仅加载最近 ${FETCH_CAP} 条（可用筛选缩小范围）。`);
    }
  } catch (cause) {
    pushNoticeOnce('load-audit', 'warn', `审计日志加载失败：${describeCause(cause)}`);
  }
}

/**
 * 拉取单个激活码详情（GET /admin/codes/:code_id）：
 * 回填完整码值、有效期、时间线与溯源链（祖先码详情一并回填，上限 5 层）。
 *
 * @returns 是否成功（失败已推横幅）
 */
export async function fetchCodeDetail(id: string): Promise<boolean> {
  if (API_MODE !== 'real' || !id) {
    return false;
  }
  try {
    const data = asRecord(await adminRequest<unknown>(`/admin/codes/${encodeURIComponent(id)}`, { method: 'GET' }));
    upsertCodeRecord(buildCodeRecordFromDetail(data));
    const chainRaw = Array.isArray(data.reissued_chain) ? data.reissued_chain : [];
    for (const ancestor of chainRaw.slice(0, 5)) {
      if (typeof ancestor === 'string' && ancestor !== '' && !realCodes.some((c) => c.id === ancestor)) {
        try {
          const anc = asRecord(
            await adminRequest<unknown>(`/admin/codes/${encodeURIComponent(ancestor)}`, { method: 'GET' }),
          );
          upsertCodeRecord(buildCodeRecordFromDetail(anc));
        } catch {
          /* 祖先详情失败不阻断主详情展示 */
        }
      }
    }
    return true;
  } catch (cause) {
    pushNoticeOnce(`code-detail-${id}`, 'warn', `激活码详情加载失败：${describeCause(cause)}`);
    return false;
  }
}

/** 把 ApiError / 未知异常收敛为一句话（供横幅）。 */
function describeCause(cause: unknown): string {
  if (cause instanceof ApiError) {
    const trace = cause.traceId ? `，trace_id=${cause.traceId}` : '';
    const biz = cause.businessCode !== 'UNKNOWN' ? `［${cause.businessCode}］` : '';
    return `${cause.message}${biz}${trace}`;
  }
  return cause instanceof Error ? cause.message : String(cause);
}

/** real 模式仓库。 */
function buildRealRepo(): AdminRepo {
  /** 生效码列表（拷贝语义与 mock 一致，避免页面层脏写缓存）。 */
  function effectiveCodes(): CodeRecord[] {
    return JSON.parse(JSON.stringify(realCodes)) as CodeRecord[];
  }

  /** 通用分页。 */
  function paginate<T>(items: T[], page: number, pageSize: number): Paged<T> {
    const start = (page - 1) * pageSize;
    return { items: items.slice(start, start + pageSize), total: items.length, page };
  }

  /** 找缓存中的码（返回引用，用于就地更新状态 / 时间线）。 */
  function findCached(id: string): CodeRecord | null {
    return realCodes.find((c) => c.id === id) ?? null;
  }

  /** 把 ApiError 转成横幅提示并返回 false（412 CONFIRM_MISMATCH 给可解释错误）。 */
  function reportFailure(prefix: string, cause: unknown): boolean {
    if (cause instanceof ApiError && cause.businessCode === 'CONFIRM_MISMATCH') {
      pushNotice(
        'error',
        `${prefix}失败：确认串与激活码末 8 位不符（CONFIRM_MISMATCH）。确认串由系统按码值去分隔符后的末 8 位自动计算，请刷新列表后重试；若仍失败请联系系统管理员核对码值。`,
      );
      return false;
    }
    pushNotice('error', `${prefix}失败：${describeCause(cause)}`);
    return false;
  }

  /** OTA 写操作失败：推全局横幅（真实原因）并返回 null。 */
  function failOta(prefix: string, cause: unknown): null {
    pushNotice('error', `${prefix}失败：${describeCause(cause)}`);
    return null;
  }

  /** 确保目标码具备完整码值与租户归属（列表仅掩码，confirm_tail8 需详情端点回填）。 */
  async function ensureFullCode(id: string): Promise<CodeRecord | null> {
    const cached = findCached(id);
    if (cached && cached.tenant && cached.code && !cached.code.includes('*')) {
      return cached;
    }
    const ok = await fetchCodeDetail(id);
    if (!ok) {
      return null;
    }
    const refreshed = findCached(id);
    if (!refreshed || !refreshed.tenant || !refreshed.code || refreshed.code.includes('*')) {
      return null;
    }
    return refreshed;
  }

  return {
    // ---------- 激活码（列表来自 GET /admin/codes 缓存；写直连后端） ----------
    queryCodes(query: CodeQuery): Paged<CodeRecord> {
      const kw = query.keyword.trim().toLowerCase();
      const filtered = effectiveCodes().filter((c) => {
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
      return paginate(filtered, query.page, query.pageSize);
    },

    getCode(id: string): CodeRecord | null {
      return effectiveCodes().find((c) => c.id === id) ?? null;
    },

    allCodes(): CodeRecord[] {
      return effectiveCodes();
    },

    async issueCode(input): Promise<CodeRecord[]> {
      try {
        const data = await adminRequest<{ codes: unknown[] }>('/admin/codes/issue', {
          body: {
            tenant_id: input.tenant,
            tier: input.tier,
            valid_from: dateToUtcSecs(input.validFrom),
            valid_until: dateToUtcSecs(input.validUntil),
            count: input.count,
            prebind_machine_code: input.prebindMachineCode || null,
            // 发放幂等键（同 key 重放返回首次结果；本前端不重试，键仅用于链路追踪）
            idempotency_key: `issue-${input.tenant}-${Date.now()}`,
          },
        });
        const rows = Array.isArray(data?.codes) ? data.codes : [];
        const stamp = nowText();
        const created: CodeRecord[] = rows.map((row, i) => {
          const view = parseIssuedRow(row, i);
          const detailParts: string[] = [];
          if (view.prebind) {
            detailParts.push(`预绑定机器码 ${view.prebind}`);
          }
          if (input.note) {
            detailParts.push(input.note);
          }
          return {
            id: view.codeId,
            code: view.code,
            status: view.status as CodeStatus,
            tenant: input.tenant,
            tier: input.tier,
            validFrom: input.validFrom,
            validUntil: input.validUntil,
            orderId: '—',
            createdAt: stamp,
            boundDeviceSummary: null,
            boundDeviceName: null,
            prebindMachineCode: view.prebind,
            reissuedFrom: view.reissuedFrom,
            reissuedTo: null,
            timeline: [
              {
                time: stamp,
                action: '发放' as LifecycleAction,
                tone: 'ok' as const,
                operator: input.actor,
                ...(view.reissuedFrom ? { target: `源码 ${view.reissuedFrom}` } : {}),
                detail: detailParts.join(' · ') || '—',
              },
            ],
            receiptContinuity: '尚未激活，无回执',
            note: input.note,
          };
        });
        for (const record of created) {
          upsertCodeRecord(JSON.parse(JSON.stringify(record)) as CodeRecord);
        }
        void fetchOverview();
        return JSON.parse(JSON.stringify(created)) as CodeRecord[];
      } catch (cause) {
        // 结构化业务码 → 可操作提示（不靠解析错误文案）。
        if (cause instanceof ApiError && cause.businessCode === 'TENANT_NOT_FOUND') {
          pushNotice(
            'error',
            '发放激活码失败：该租户不存在［TENANT_NOT_FOUND］。请先在「租户与策略」页新增租户，再为该租户发放激活码。',
          );
          return [];
        }
        if (cause instanceof ApiError && cause.businessCode === 'MACHINE_CODE_REQUIRED') {
          pushNotice(
            'error',
            '发放激活码失败：机器码为必填项［MACHINE_CODE_REQUIRED］，请在客户设备上获取机器码后填入。',
          );
          return [];
        }
        reportFailure('发放激活码', cause);
        return [];
      }
    },

    async revokeCode(input): Promise<boolean> {
      // 契约前置拦截：note ≥ 10 字（后端 400），前端先拦并解释
      if (input.note.trim().length < 10) {
        pushNotice('error', '废弃失败：补充说明需 ≥10 字（后端契约），请补充操作原因的具体细节。');
        return false;
      }
      // confirm_tail8 需要完整码值：列表只有掩码，经详情端点回填（租户归属同源）
      const target = await ensureFullCode(input.id);
      if (!target) {
        pushNotice('error', '废弃失败：无法获取该激活码的完整码值与租户归属（详情端点不可用）。');
        return false;
      }
      try {
        await adminRequest<unknown>(`/admin/codes/${encodeURIComponent(target.id)}/revoke`, {
          headers: { 'X-Tenant-Id': target.tenant },
          body: {
            reason: input.reason,
            note: input.note.trim(),
            // 契约：confirm_tail8 = 激活码去分隔符后的末 8 位（自动计算，绝不让用户手算）
            confirm_tail8: tail8Of(target.code),
            // 契约：双人复核开启时后端要求 second_approver 必填，原样透传（不得折进 note）
            second_approver: input.secondApprover ?? null,
          },
        });
        target.status = 'revoked';
        target.receiptContinuity = '废弃后停止回执';
        target.timeline.push({
          time: nowText(),
          action: '废弃',
          tone: 'danger',
          operator: input.actor,
          reason: input.reason,
          detail: `补充说明：${input.note}`,
        });
        void fetchCodes();
        void fetchOverview();
        return true;
      } catch (cause) {
        return reportFailure('废弃激活码', cause);
      }
    },

    async reissueCode(input): Promise<CodeRecord | null> {
      const source = await ensureFullCode(input.sourceId);
      if (!source) {
        pushNotice('error', '重发失败：无法获取源激活码的租户归属（详情端点不可用）。');
        return null;
      }
      const inheritTier = input.inheritTier === source.tier;
      try {
        const data = await adminRequest<{ new_code: unknown }>(
          `/admin/codes/${encodeURIComponent(source.id)}/reissue`,
          {
            headers: { 'X-Tenant-Id': source.tenant },
            body: {
              prebind: input.prebindMachineCode ? { machine_code: input.prebindMachineCode } : null,
              inherit_tier: inheritTier,
              // 页面总是给出明确到期日，故不继承原有效期，而是显式覆盖
              inherit_validity: false,
              overrides: {
                tier: inheritTier ? null : input.inheritTier,
                valid_until: dateToUtcSecs(input.inheritValidUntil),
              },
              // 重发幂等键（与 revoke 同事务；同 key 重放返回同一张新码）
              idempotency_key: `reissue-${source.id}-${Date.now()}`,
            },
          },
        );
        const view = parseIssuedRow(asRecord(data?.new_code), 0);
        const stamp = nowText();
        const created: CodeRecord = {
          id: view.codeId,
          code: view.code,
          status: view.status as CodeStatus,
          tenant: source.tenant,
          tier: input.inheritTier,
          validFrom: stamp.slice(0, 10),
          validUntil: input.inheritValidUntil,
          orderId: '—',
          createdAt: stamp,
          boundDeviceSummary: null,
          boundDeviceName: null,
          prebindMachineCode: view.prebind,
          reissuedFrom: source.id,
          reissuedTo: null,
          timeline: [
            {
              time: stamp,
              action: '重发',
              tone: 'warn',
              target: `源码 ${source.id}`,
              operator: input.actor,
              detail: `继承 tier ${input.inheritTier} · 有效期至 ${input.inheritValidUntil}`,
            },
          ],
          receiptContinuity: '尚未激活，无回执',
          note: input.note,
        };
        // 后端语义：原码 status → reissued（禁止再废弃 / 再重发），并建立溯源链
        source.status = 'reissued';
        source.reissuedTo = created.id;
        source.timeline.push({
          time: stamp,
          action: '重发',
          tone: 'warn',
          target: `新码 ${created.id}`,
          operator: input.actor,
          detail: `继承 tier ${input.inheritTier} · 有效期至 ${input.inheritValidUntil}`,
        });
        upsertCodeRecord(JSON.parse(JSON.stringify(created)) as CodeRecord);
        void fetchCodes();
        void fetchOverview();
        return JSON.parse(JSON.stringify(created)) as CodeRecord;
      } catch (cause) {
        reportFailure('重发激活码', cause);
        return null;
      }
    },

    // ---------- 设备（GET /admin/devices 已接通） ----------
    queryDevices(query: { tenant: string; deployMode: string; licenseStatus: string; keyword: string; page: number; pageSize: number }): Paged<DeviceRecord> {
      const kw = query.keyword.trim().toLowerCase();
      const filtered = (JSON.parse(JSON.stringify(realDevices)) as DeviceRecord[]).filter((d) => {
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
      return paginate(filtered, query.page, query.pageSize);
    },

    getDevice(id: string): DeviceRecord | null {
      return (JSON.parse(JSON.stringify(realDevices)) as DeviceRecord[]).find((d) => d.id === id) ?? null;
    },

    allDevices(): DeviceRecord[] {
      return JSON.parse(JSON.stringify(realDevices)) as DeviceRecord[];
    },

    markDeviceAnomaly(_input: { id: string; note: string; actor: string }): Promise<boolean> {
      pushNoticeOnce('gap-device-note', 'warn', '后端缺口 #4b：未提供设备备注写端点——「标记异常」仅在本页提示，未落库。');
      return Promise.resolve(false);
    },

    // ---------- 租户（GET/POST /admin/tenants 已接通；策略写端点缺失） ----------
    allTenants(): TenantRecord[] {
      return JSON.parse(JSON.stringify(realTenants)) as TenantRecord[];
    },

    getTenant(id: string): TenantRecord | null {
      return (JSON.parse(JSON.stringify(realTenants)) as TenantRecord[]).find((t) => t.id === id) ?? null;
    },

    async createTenant(input): Promise<boolean> {
      const tenantId = input.tenantId.trim();
      const name = input.name.trim();
      if (!tenantId || !name) {
        pushNotice('error', '新增租户失败：租户 ID 与名称均不能为空。');
        return false;
      }
      try {
        await adminRequest<unknown>('/admin/tenants', {
          method: 'POST',
          body: {
            tenant_id: tenantId,
            name,
            contact: input.contact.trim(),
            verify_mode_default: input.verifyMode,
          },
        });
        // 创建成功后刷新租户缓存（发放弹窗的真实租户下拉随之更新）。
        void fetchTenants();
        return true;
      } catch (cause) {
        return reportFailure('新增租户', cause);
      }
    },

    updateTenant(_input: {
      id: string;
      defaultGrade: Grade;
      defaultTier: string;
      heartbeatInterval: string;
      offlineGrace: string;
      receiptRequired: boolean;
      reason: string;
      actor: string;
    }): Promise<boolean> {
      pushNoticeOnce('gap-tenant-policy', 'warn', '后端缺口 #2b：未提供租户策略写端点（PUT /admin/tenants/{id}/policy）——策略修改未生效。');
      return Promise.resolve(false);
    },

    setTenantEnabled(_input: { id: string; enabled: boolean; actor: string }): Promise<boolean> {
      pushNoticeOnce('gap-tenant-enable', 'warn', '后端缺口 #2b：未提供租户启停写端点——启停操作未生效。');
      return Promise.resolve(false);
    },

    // ---------- 回执异常（GET /admin/receipts/anomalies 已接通；处置写端点缺失） ----------
    allAnomalies(): ReceiptAnomaly[] {
      return JSON.parse(JSON.stringify(realAnomalies)) as ReceiptAnomaly[];
    },

    resolveAnomaly(_input: { id: string; note: string; verified: boolean; actor: string }): Promise<boolean> {
      pushNoticeOnce('gap-anomaly-resolve', 'warn', '后端缺口 #5b：未提供异常处置写端点——处置状态未落库。');
      return Promise.resolve(false);
    },

    // ---------- 换机工单（工单实体后端不存在；可用废弃+重发替代） ----------
    allTransfers(): TransferTicket[] {
      pushNoticeOnce(
        'gap-transfers',
        'warn',
        '后端缺口 #6：换机工单为纯前端实体，后端无对应端点——real 模式请直接在「激活码管理」页执行废弃 / 重发。',
      );
      return [];
    },

    async processTransfer(input): Promise<ProcessTransferResult> {
      // real 模式没有工单实体：若源码在缓存中，则退化为「废弃 + 重发」组合
      const source = findCached(input.ticketId);
      if (!source) {
        pushNoticeOnce(
          'gap-transfers-process',
          'warn',
          '后端缺口 #6：换机工单端点缺失且工单实体仅存在于 mock——real 模式请直接对激活码执行废弃 / 重发。',
        );
        return { ok: false, ticket: null, newCode: null, message: '后端未提供换机工单端点（缺口 #6）' };
      }
      if (source.status !== 'revoked') {
        const revoked = await this.revokeCode({ id: source.id, reason: '客户更换硬件', note: input.note, actor: input.actor });
        if (!revoked) {
          return { ok: false, ticket: null, newCode: null, message: '废弃旧码失败（见上方提示）' };
        }
      }
      if (!input.reissue) {
        return { ok: true, ticket: null, newCode: null, message: '已废弃旧码' };
      }
      const newCode = await this.reissueCode({
        sourceId: source.id,
        inheritTier: input.inheritTier,
        inheritValidUntil: input.validUntil,
        // real 模式无工单实体可提供「新机器码」，一律留待首次激活绑定
        prebindMachineCode: '',
        note: input.note,
        actor: input.actor,
      });
      return newCode
        ? { ok: true, ticket: null, newCode, message: `已重发新码 ${newCode.id}` }
        : { ok: false, ticket: null, newCode: null, message: '重发失败（见上方提示）' };
    },

    // ---------- 密钥（GET /admin/keys 已接通；轮换写端点缺失） ----------
    allKeys(): SigningKey[] {
      return JSON.parse(JSON.stringify(realKeys)) as SigningKey[];
    },

    rotateKey(_input: { newKid: string; actor: string }): Promise<boolean> {
      pushNoticeOnce('gap-key-rotate', 'warn', '后端缺口 #7b：未提供密钥轮换写端点——轮换操作未生效。');
      return Promise.resolve(false);
    },

    retireKey(_input: { kid: string; actor: string }): Promise<boolean> {
      pushNoticeOnce('gap-key-retire', 'warn', '后端缺口 #7b：未提供密钥退役写端点——退役操作未生效。');
      return Promise.resolve(false);
    },

    // ---------- 审计（GET /admin/audit/logs 已接通） ----------
    queryAudit(query: { actorType: string; action: string; entityType: string; entityId: string; page: number; pageSize: number }): Paged<AuditEntry> {
      const filtered = (JSON.parse(JSON.stringify(realAuditLogs)) as AuditEntry[]).filter((log) => {
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
      return paginate(filtered, query.page, query.pageSize);
    },

    async logReveal(_input: { entityId: string; actor: string }): Promise<void> {
      // 后端无「查看明文」审计写端点（缺口 #8b）：静默跳过。
      // 说明：揭示动作本身仍受页面权限门控；服务端审计待缺口补齐后接入。
    },

    // ---------- 账号（GET/POST/PUT/DELETE /admin/accounts 已接通；缺口 #9 修复） ----------
    allUsers(): AdminUser[] {
      return JSON.parse(JSON.stringify(realAccounts)) as AdminUser[];
    },

    async setUserStatus(input): Promise<boolean> {
      try {
        await adminRequest<unknown>(`/admin/accounts/${encodeURIComponent(input.account)}`, {
          method: 'PUT',
          body: { status: input.enabled ? 'active' : 'disabled' },
        });
        await fetchAccounts();
        return true;
      } catch (cause) {
        return reportFailure(input.enabled ? '启用账号' : '停用账号', cause);
      }
    },

    async createUser(input): Promise<boolean> {
      if (!input.account.trim() || !input.password) {
        pushNotice('error', '新增账号失败：账号与初始口令均不能为空。');
        return false;
      }
      try {
        await adminRequest<unknown>('/admin/accounts', {
          method: 'POST',
          body: {
            account: input.account.trim(),
            display_name: input.name,
            role: input.role,
            password: input.password,
          },
        });
        await fetchAccounts();
        return true;
      } catch (cause) {
        return reportFailure('新增账号', cause);
      }
    },

    async updateUser(input): Promise<boolean> {
      const body: Record<string, unknown> = {};
      if (input.name !== undefined) {
        body.display_name = input.name;
      }
      if (input.role !== undefined) {
        body.role = input.role;
      }
      if (input.password) {
        body.password = input.password;
      }
      try {
        await adminRequest<unknown>(`/admin/accounts/${encodeURIComponent(input.account)}`, {
          method: 'PUT',
          body,
        });
        await fetchAccounts();
        return true;
      } catch (cause) {
        return reportFailure('修改账号', cause);
      }
    },

    async deleteUser(input): Promise<boolean> {
      try {
        await adminRequest<unknown>(`/admin/accounts/${encodeURIComponent(input.account)}`, {
          method: 'DELETE',
          body: { reason: input.reason, note: input.note, confirm: input.confirm },
        });
        await fetchAccounts();
        return true;
      } catch (cause) {
        return reportFailure('删除账号', cause);
      }
    },

    // ---------- 总览（GET /admin/overview 已接通；计数 String 原文透传） ----------
    overview(): OverviewStats {
      return realStats;
    },

    async activationTrend(days: number): Promise<ActivationTrendPoint[]> {
      try {
        const data = asRecord(
          await adminRequest<unknown>('/admin/stats/activations', {
            method: 'GET',
            query: { days: String(days) },
          }),
        );
        const items = Array.isArray(data.items) ? data.items : [];
        // 日期 / 计数一律 String 原文透传（大数红线），绝不 Number 化存储。
        return items.map((raw) => {
          const rec = asRecord(raw);
          return {
            date: pickStr(rec, 'date', ''),
            issue: pickStr(rec, 'issue', '0'),
            bind: pickStr(rec, 'bind', '0'),
            revoke: pickStr(rec, 'revoke', '0'),
          };
        });
      } catch (cause) {
        pushNoticeOnce('load-activation-trend', 'warn', `激活趋势加载失败：${describeCause(cause)}`);
        return [];
      }
    },

    // ---------- 系统更新（GET /admin/updates 任意角色可读；写仅 system） ----------
    async listUpdates(): Promise<OtaPackageRecord[]> {
      try {
        const data = await adminRequest<unknown>('/admin/updates', { method: 'GET' });
        const rec = asRecord(data);
        const items = Array.isArray(data) ? data : Array.isArray(rec.items) ? (rec.items as unknown[]) : [];
        // 加载成功：清空上一次失败原因（诚实空态依赖它区分「无数据」与「加载失败」）
        updatesLoadError.value = '';
        // 字段一律 String 原文透传（大数红线），published_at 在映射边界格式化。
        return items.map((raw) => buildOtaPackageRecord(asRecord(raw)));
      } catch (cause) {
        // 加载失败：记录**真实原因**供页面诚实空态展示，绝不回落 mock。
        updatesLoadError.value = describeCause(cause);
        pushNoticeOnce('load-updates', 'warn', `系统更新列表加载失败：${describeCause(cause)}`);
        return [];
      }
    },

    async uploadUpdate(input): Promise<OtaPackageRecord | null> {
      try {
        const data = await adminRequest<unknown>('/admin/updates', {
          method: 'POST',
          body: {
            version: input.version.trim(),
            channel: input.channel,
            payload_b64: input.payloadB64,
            note: input.note.trim(),
            // reason / note_detail / confirm 为**彼此独立**的三个字段，绝不拼接
            reason: input.reason,
            note_detail: input.noteDetail.trim(),
            confirm: input.confirm,
          },
        });
        return buildOtaPackageRecord(asRecord(data));
      } catch (cause) {
        return failOta('上传升级包', cause);
      }
    },

    async publishUpdate(version, input): Promise<OtaPackageRecord | null> {
      try {
        const data = await adminRequest<unknown>(
          `/admin/updates/${encodeURIComponent(version)}/publish`,
          {
            method: 'POST',
            body: { channel: input.channel, reason: input.reason, note: input.note, confirm: input.confirm },
          },
        );
        return buildOtaPackageRecord(asRecord(data));
      } catch (cause) {
        return failOta('发布升级包', cause);
      }
    },

    async disableUpdate(version, input): Promise<OtaPackageRecord | null> {
      try {
        const data = await adminRequest<unknown>(
          `/admin/updates/${encodeURIComponent(version)}/disable`,
          {
            method: 'POST',
            body: { channel: input.channel, reason: input.reason, note: input.note, confirm: input.confirm },
          },
        );
        return buildOtaPackageRecord(asRecord(data));
      } catch (cause) {
        return failOta('停用升级包', cause);
      }
    },
  };
}

// ===========================================================================
// preload（登录成功后调用一次；P0 教训：必须有总超时护栏）
// ===========================================================================

/** preload 是否已启动（防重复并发拉取）。 */
let preloadStarted = false;

/**
 * 预取真实数据（real 模式专用；mock 模式直接返回 false）。
 *
 * 并行拉取全部 GET 查询端点进 reactive 缓存（overview / codes / devices /
 * tenants / anomalies / keys / audit）；单端点失败独立提示、不互相阻断。
 * 总超时护栏 10s（超时后缓存保持部分结果，页面空态给出重试路径）。
 *
 * @returns real 模式返回 true（已进入加载态）；mock 模式返回 false
 */
export async function preloadRealData(): Promise<boolean> {
  if (API_MODE !== 'real') {
    return false;
  }
  if (preloadStarted) {
    return true;
  }
  preloadStarted = true;
  const TIMEOUT_MS = 10_000;
  await Promise.race([
    Promise.allSettled([
      fetchOverview(),
      fetchCodes(),
      fetchDevices(),
      fetchTenants(),
      fetchAnomalies(),
      fetchKeys(),
      fetchAudit(),
      fetchAccounts(),
    ]),
    new Promise<'timeout'>((resolve) => setTimeout(() => resolve('timeout'), TIMEOUT_MS)),
  ]);
  return true;
}

// ===========================================================================
// 统一导出：页面层唯一数据入口
// ===========================================================================

/** mock 模式 = mock 仓库 async 适配；real 模式 = 真实端点 + 诚实空态降级。 */
export const repo: AdminRepo = API_MODE === 'real' ? buildRealRepo() : buildMockRepo();

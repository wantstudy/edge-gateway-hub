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
 *      - 已接通：`POST /admin/codes/issue`（发放）、`POST /admin/codes/:id/revoke`
 *        （废弃）、`POST /admin/codes/:id/reissue`（重发）——发放/重发响应回填
 *        会话级码缓存，列表 / 详情 / 溯源随缓存联动；
 *      - 端点缺失（设备 / 租户 / 审计 / 密钥 / 账号 / 工单 / 码列表等）：读取型
 *        方法返回**诚实的空结果**（绝不回退假数据），写入型方法返回 false 并把
 *        「后端缺口」写入全局提示横幅（`adminNotices`），做到清晰报错。
 *  · **大数红线**：uint64 / unix 秒时间戳一律 `string` 直通（`pickStr`），
 *    仅小值业务计数经 `pickNum`；日期 ↔ UTC 秒换算集中在 `dateToUtcSecs`。
 *  · **不改 mock-data.ts 本身**（它是契约与 fallback）。
 */
import { reactive } from 'vue';
import {
  repo as mockRepo,
  type AdminUser,
  type AuditEntry,
  type CodeQuery,
  type CodeRecord,
  type CodeStatus,
  type DeviceRecord,
  type Grade,
  type LifecycleAction,
  type Paged,
  type ReceiptAnomaly,
  type SigningKey,
  type TenantRecord,
  type TransferTicket,
} from '../mock/mock-data';

// 透传 mock-data 的类型 / 常量 / 枚举（`repo` 由下方局部导出遮蔽，属 ES 模块规范行为）
export * from '../mock/mock-data';

import { API_MODE, ApiError, adminRequest } from './client';

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

// （宽容取值当前仅用到 pickStr：后端已接通端点的整数字段均按契约以字符串透传，
//  大数红线——绝不 parseFloat；后续接通列表端点需要数值映射时再按需补充。）

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

/** 总览聚合形状（mock-data `repo.overview()` 返回值的结构化镜像）。 */
export interface OverviewStats {
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
  revokeCode(input: { id: string; reason: string; note: string; actor: string }): Promise<boolean>;
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
  setUserStatus(input: { account: string; enabled: boolean; actor: string }): Promise<boolean>;

  // ---------- 总览 ----------
  overview(): OverviewStats;
}

// ===========================================================================
// mock 模式适配器：mockRepo + async 包装（行为逐行零回归）
// ===========================================================================

/** mock 仓库的 async 适配包装（读方法直通，写方法 Promise.resolve）。 */
function buildMockRepo(): AdminRepo {
  return {
    ...mockRepo,
    issueCode: (input) => Promise.resolve(mockRepo.issueCode(input)),
    revokeCode: (input) => Promise.resolve(mockRepo.revokeCode(input)),
    reissueCode: (input) => Promise.resolve(mockRepo.reissueCode(input)),
    markDeviceAnomaly: (input) => Promise.resolve(mockRepo.markDeviceAnomaly(input)),
    updateTenant: (input) => Promise.resolve(mockRepo.updateTenant(input)),
    setTenantEnabled: (input) => Promise.resolve(mockRepo.setTenantEnabled(input)),
    resolveAnomaly: (input) => Promise.resolve(mockRepo.resolveAnomaly(input)),
    processTransfer: (input) => Promise.resolve(mockRepo.processTransfer(input)),
    rotateKey: (input) => Promise.resolve(mockRepo.rotateKey(input)),
    retireKey: (input) => Promise.resolve(mockRepo.retireKey(input)),
    logReveal: (input) => Promise.resolve(mockRepo.logReveal(input)),
    setUserStatus: (input) => Promise.resolve(mockRepo.setUserStatus(input)),
  };
}

// ===========================================================================
// real 模式仓库：已接通 3 个真实写端点；其余端点缺失 → 诚实空态 + 缺口提示
// ===========================================================================

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

/** 码缓存条目 → 页面 CodeRecord（含时间线）。 */
function buildCodeRecord(view: IssuedCodeView, ctx: {
  tenantId: string;
  tier: string;
  validFrom: string;
  validUntil: string;
  note: string;
  actor: string;
  action: LifecycleAction;
  tone: 'ok' | 'warn';
}): CodeRecord {
  const stamp = nowText();
  const detailParts: string[] = [];
  if (view.prebind) {
    detailParts.push(`预绑定机器码 ${view.prebind}`);
  }
  if (ctx.note) {
    detailParts.push(ctx.note);
  }
  return {
    id: view.codeId,
    code: view.code,
    status: view.status as CodeStatus,
    tenant: ctx.tenantId,
    tier: ctx.tier,
    validFrom: ctx.validFrom,
    validUntil: ctx.validUntil,
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
        action: ctx.action,
        tone: ctx.tone,
        operator: ctx.actor,
        ...(view.reissuedFrom ? { target: `源码 ${view.reissuedFrom}` } : {}),
        detail: detailParts.join(' · ') || '—',
      },
    ],
    receiptContinuity: '尚未激活，无回执',
    note: ctx.note,
  };
}

/** real 模式会话级码缓存（后端未提供码列表端点——缺口 #3，故由写响应回填）。 */
const realCodes: CodeRecord[] = [];

/** real 模式 preload 汇总提示（每次会话只提示一次）。 */
let preloadNoticeShown = false;

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

  /** 把 ApiError 转成横幅提示并返回 false。 */
  function reportFailure(prefix: string, cause: unknown): boolean {
    if (cause instanceof ApiError) {
      const trace = cause.traceId ? `，trace_id=${cause.traceId}` : '';
      const biz = cause.businessCode !== 'UNKNOWN' ? `［${cause.businessCode}］` : '';
      pushNotice('error', `${prefix}失败：${cause.message}${biz}${trace}`);
    } else {
      pushNotice('error', `${prefix}失败：${cause instanceof Error ? cause.message : String(cause)}`);
    }
    return false;
  }

  return {
    // ---------- 激活码（列表 / 详情来自会话级缓存；写直连后端） ----------
    queryCodes(query: CodeQuery): Paged<CodeRecord> {
      pushNoticeOnce(
        'gap-codes-list',
        'warn',
        '后端缺口 #3：未提供 GET /admin/codes 码列表端点——当前仅展示本会话内经「发放 / 重发」产生的码。',
      );
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
        const created = rows.map((row, i) =>
          buildCodeRecord(parseIssuedRow(row, i), {
            tenantId: input.tenant,
            tier: input.tier,
            validFrom: input.validFrom,
            validUntil: input.validUntil,
            note: input.note,
            actor: input.actor,
            action: '发放',
            tone: 'ok',
          }),
        );
        realCodes.push(...created);
        return JSON.parse(JSON.stringify(created)) as CodeRecord[];
      } catch (cause) {
        reportFailure('发放激活码', cause);
        return [];
      }
    },

    async revokeCode(input): Promise<boolean> {
      const target = findCached(input.id);
      if (!target) {
        pushNotice(
          'error',
          '废弃失败：该激活码不在本会话缓存中（后端未提供码列表端点，无法确定其租户归属）。',
        );
        return false;
      }
      try {
        await adminRequest<unknown>(`/admin/codes/${encodeURIComponent(target.id)}/revoke`, {
          headers: { 'X-Tenant-Id': target.tenant },
          body: {
            reason: input.reason,
            // 契约要求 note ≥ 10 字符；不足时以原因补足（后端当前未强制，前端先对齐契约）
            note: input.note.trim().length >= 10 ? input.note : `${input.note}（原因：${input.reason}）`,
            confirm_tail8: tail8Of(target.code),
            second_approver: null,
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
        return true;
      } catch (cause) {
        return reportFailure('废弃激活码', cause);
      }
    },

    async reissueCode(input): Promise<CodeRecord | null> {
      const source = findCached(input.sourceId);
      if (!source) {
        pushNotice(
          'error',
          '重发失败：源激活码不在本会话缓存中（后端未提供码列表端点，无法确定其租户归属）。',
        );
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
        const created = buildCodeRecord(view, {
          tenantId: source.tenant,
          tier: input.inheritTier,
          validFrom: nowText().slice(0, 10),
          validUntil: input.inheritValidUntil,
          note: input.note,
          actor: input.actor,
          action: '重发',
          tone: 'warn',
        });
        // 后端语义：原码 status → reissued（禁止再废弃 / 再重发），并建立溯源链
        source.status = 'reissued';
        source.reissuedTo = created.id;
        source.timeline.push({
          time: nowText(),
          action: '重发',
          tone: 'warn',
          target: `新码 ${created.id}`,
          operator: input.actor,
          detail: `继承 tier ${input.inheritTier} · 有效期至 ${input.inheritValidUntil}`,
        });
        realCodes.push(created);
        return JSON.parse(JSON.stringify(created)) as CodeRecord;
      } catch (cause) {
        reportFailure('重发激活码', cause);
        return null;
      }
    },

    // ---------- 设备（缺口 #4：GET /admin/devices） ----------
    queryDevices(query: { tenant: string; deployMode: string; licenseStatus: string; keyword: string; page: number; pageSize: number }): Paged<DeviceRecord> {
      void query;
      pushNoticeOnce('gap-devices', 'warn', '后端缺口 #4：未提供 GET /admin/devices 设备列表端点——设备页无真实数据可显示。');
      return { items: [], total: 0, page: query.page };
    },

    getDevice(_id: string): DeviceRecord | null {
      return null;
    },

    allDevices(): DeviceRecord[] {
      return [];
    },

    markDeviceAnomaly(_input: { id: string; note: string; actor: string }): Promise<boolean> {
      pushNoticeOnce('gap-device-note', 'warn', '后端缺口 #4b：未提供设备备注写端点——「标记异常」仅在本页提示，未落库。');
      return Promise.resolve(false);
    },

    // ---------- 租户（缺口 #2：GET /admin/tenants + 策略写端点） ----------
    allTenants(): TenantRecord[] {
      pushNoticeOnce('gap-tenants', 'warn', '后端缺口 #2：未提供租户列表 / 策略显著端点——租户页无真实数据可显示。');
      return [];
    },

    getTenant(_id: string): TenantRecord | null {
      return null;
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

    // ---------- 回执异常（缺口 #5：GET /admin/receipts/anomalies） ----------
    allAnomalies(): ReceiptAnomaly[] {
      pushNoticeOnce('gap-anomalies', 'warn', '后端缺口 #5：未提供回执异常查询端点——回执异常页无真实数据可显示。');
      return [];
    },

    resolveAnomaly(_input: { id: string; note: string; verified: boolean; actor: string }): Promise<boolean> {
      pushNoticeOnce('gap-anomaly-resolve', 'warn', '后端缺口 #5b：未提供异常处置写端点——处置状态未落库。');
      return Promise.resolve(false);
    },

    // ---------- 换机工单（缺口 #6：工单实体后端不存在；可用废弃+重发替代） ----------
    allTransfers(): TransferTicket[] {
      pushNoticeOnce(
        'gap-transfers',
        'warn',
        '后端缺口 #6：换机工单为纯前端实体，后端无对应端点——real 模式请直接在「激活码管理」页执行废弃 / 重发。',
      );
      return [];
    },

    async processTransfer(input): Promise<ProcessTransferResult> {
      // real 模式没有工单实体：若源码在本会话缓存中，则退化为「废弃 + 重发」组合
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

    // ---------- 密钥（缺口 #7：GET /admin/keys + 轮换写端点） ----------
    allKeys(): SigningKey[] {
      pushNoticeOnce('gap-keys', 'warn', '后端缺口 #7：未提供签名密钥列表端点——密钥页无真实数据可显示。');
      return [];
    },

    rotateKey(_input: { newKid: string; actor: string }): Promise<boolean> {
      pushNoticeOnce('gap-key-rotate', 'warn', '后端缺口 #7b：未提供密钥轮换写端点——轮换操作未生效。');
      return Promise.resolve(false);
    },

    retireKey(_input: { kid: string; actor: string }): Promise<boolean> {
      pushNoticeOnce('gap-key-retire', 'warn', '后端缺口 #7b：未提供密钥退役写端点——退役操作未生效。');
      return Promise.resolve(false);
    },

    // ---------- 审计（缺口 #8：GET /admin/audit/logs） ----------
    queryAudit(query: { actorType: string; action: string; entityType: string; entityId: string; page: number; pageSize: number }): Paged<AuditEntry> {
      void query;
      pushNoticeOnce('gap-audit', 'warn', '后端缺口 #8：未提供 GET /admin/audit/logs 审计查询端点——审计页无真实数据可显示。');
      return { items: [], total: 0, page: 1 };
    },

    async logReveal(_input: { entityId: string; actor: string }): Promise<void> {
      // 后端无「查看明文」审计写端点（缺口 #8b）：静默跳过。
      // 说明：揭示动作本身仍受页面权限门控；服务端审计待缺口补齐后接入。
    },

    // ---------- 账号（缺口 #9：管理员账号体系整体缺失，含登录/RBAC） ----------
    allUsers(): AdminUser[] {
      pushNoticeOnce('gap-users', 'warn', '后端缺口 #9：未提供管理员账号列表端点——账号页无真实数据可显示。');
      return [];
    },

    setUserStatus(_input: { account: string; enabled: boolean; actor: string }): Promise<boolean> {
      pushNoticeOnce('gap-user-status', 'warn', '后端缺口 #9：未提供账号启停写端点——操作未生效。');
      return Promise.resolve(false);
    },

    // ---------- 总览（由会话级真实缓存聚合；端点缺失维度为诚实 0 值） ----------
    overview(): OverviewStats {
      pushNoticeOnce(
        'gap-overview',
        'warn',
        '后端缺口 #10：未提供总览聚合端点——总览页各维度为会话内真实缓存的聚合（未覆盖维度显示 0）。',
      );
      const tenantSet = new Set(realCodes.map((c) => c.tenant));
      const issued = realCodes.filter((c) => c.status === 'issued').length;
      return {
        tenantCount: tenantSet.size,
        licensedDevices: 0,
        trialDevices: 0,
        onlineDevices: 0,
        offlineDevices: 0,
        receiptOk: 0,
        receiptGap: 0,
        receiptMissing: 0,
        receiptBadSig: 0,
        pendingTransfers: 0,
        pendingAnomalies: 0,
        currentKid: '—',
        kidRetireInDays: 0,
        legacyClientCount: 0,
        tenantDelta: 0,
        newActivationsThisMonth: issued,
        expiringIn7Days: 0,
      };
    },
  };
}

// ===========================================================================
// preload（登录成功后调用一次；P0 教训：必须有总超时护栏）
// ===========================================================================

/**
 * 预取真实数据（real 模式专用；mock 模式直接返回 false）。
 *
 * 后端当前**没有任何 GET 型管理端点**（全部 7 个端点均为 POST 写操作），
 * 因此 preload 阶段无可拉取的列表数据——这里仅推送一次缺口汇总提示并立即返回。
 * 保留该入口 + 超时护栏骨架，待后端补齐列表端点后在此并行预取。
 *
 * @returns real 模式返回 true（已进入降级就绪态）；mock 模式返回 false
 */
export async function preloadRealData(): Promise<boolean> {
  if (API_MODE !== 'real') {
    return false;
  }
  if (!preloadNoticeShown) {
    preloadNoticeShown = true;
    pushNotice(
      'warn',
      'real 模式联调说明：licensing-server 当前仅提供「发放 / 废弃 / 重发激活码」3 个管理端点（均无鉴权），其余页面因端点缺失显示为空，详见后端缺口清单。',
    );
  }
  // 超时护栏骨架：未来预取请求统一经 Promise.race 包裹，绝不无限等待
  const TIMEOUT_MS = 10_000;
  await Promise.race([
    Promise.resolve(),
    new Promise<'timeout'>((resolve) => setTimeout(() => resolve('timeout'), TIMEOUT_MS)),
  ]);
  return true;
}

// ===========================================================================
// 统一导出：页面层唯一数据入口
// ===========================================================================

/** mock 模式 = mock 仓库 async 适配；real 模式 = 真实端点 + 诚实空态降级。 */
export const repo: AdminRepo = API_MODE === 'real' ? buildRealRepo() : buildMockRepo();

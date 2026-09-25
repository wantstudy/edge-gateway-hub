/**
 * @file client.ts
 * @module admin-console/api/client
 * @description 原生 fetch 封装（**零新增依赖**），对接 `crates/licensing-server` 管理 API。
 *
 * ── 模式判定 ────────────────────────────────────────────────────────────────
 * `VITE_API_MODE === 'real'` 时数据层（`repo.ts`）走真实后端（dev 下经 vite proxy
 * `/licensing` → `http://127.0.0.1:7080` 转发，即 licensing-server 默认监听口）；
 * 默认 `mock` 时完全不发请求，页面照常使用 `mock/mock-data.ts`，保证无后端环境
 * 可独立演示与测试（mock 行为零回归）。
 *
 * ── 后端真实契约（crates/licensing-server/src/http.rs，2026 联调调查结论）─────
 *  · 统一响应包裹：`{ code: "OK" | <业务码>, data, message, trace_id }`；
 *    HTTP 状态码由业务码推出（`proto::http_status`）。
 *  · 管理端点（全部 POST，**无任何鉴权**）：
 *      - `POST /admin/codes/issue`                批量发放
 *      - `POST /admin/codes/:code_id/revoke`      废弃（租户取 `X-Tenant-Id` 头）
 *      - `POST /admin/codes/:code_id/reissue`     重发（租户取 `X-Tenant-Id` 头）
 *  · **鉴权缺口**：licensing-api.md §4 要求「管理员登录态 + RBAC 门控」，但后端
 *    既无 `/admin/auth/login` 也无任何 token 校验。前端按契约先行发送
 *    `X-Actor-Id`（操作者）与 `X-Tenant-Id`（默认租户）头，并在登录时探测
 *    `POST /admin/auth/login`——缺失时清晰降级（全局提示横幅），绝不伪造
 *    「已鉴权」假象。缺口明细见任务汇报「后端缺口清单」。
 *
 * ── 大数红线 ────────────────────────────────────────────────────────────────
 * 响应体一律按 JSON 文本解析，**不做任何数值转换**；整数字段（uint64 / 纳秒 /
 * unix 秒时间戳）由后端以字符串返回，repo 层原样透传，绝不 parseFloat。
 */

/** API 模式：`mock`（默认，纯前端演示）/ `real`（经代理访问 licensing-server）。 */
export type ApiMode = 'mock' | 'real';

/** 当前 API 模式（构建期由 `VITE_API_MODE` 决定，默认 `mock`）。 */
export const API_MODE: ApiMode = import.meta.env.VITE_API_MODE === 'real' ? 'real' : 'mock';

/** licensing-server 的同源代理前缀（dev 由 vite proxy 剥掉前缀转发到 7080）。 */
const BASE_PATH = '/licensing';

/** 单请求默认总超时（联调 P0 教训：任何请求都必须有超时护栏，绝不无限等待）。 */
const DEFAULT_TIMEOUT_MS = 15_000;

/** 操作者标识在 localStorage 的键名（登录时写入，作为 `X-Actor-Id` 头）。 */
const ACTOR_KEY = 'iot-daq.ac.actor';

/** 默认租户 ID 在 localStorage 的键名（登录时写入，作为 `X-Tenant-Id` 头兜底）。 */
const TENANT_KEY = 'iot-daq.ac.tenant';

/** 读取本地操作者标识（未登录返回空串）。 */
export function getStoredActor(): string {
  try {
    return localStorage.getItem(ACTOR_KEY) ?? '';
  } catch {
    return '';
  }
}

/** 写入本地操作者标识（登录成功时调用）。 */
export function setStoredActor(actor: string): void {
  try {
    localStorage.setItem(ACTOR_KEY, actor);
  } catch {
    /* localStorage 不可用（隐私模式）时静默降级为内存态会话 */
  }
}

/** 读取本地默认租户 ID（未登录返回空串）。 */
export function getStoredTenantId(): string {
  try {
    return localStorage.getItem(TENANT_KEY) ?? '';
  } catch {
    return '';
  }
}

/** 写入本地默认租户 ID（登录成功时调用）。 */
export function setStoredTenantId(tenantId: string): void {
  try {
    localStorage.setItem(TENANT_KEY, tenantId);
  } catch {
    /* 同上：静默降级 */
  }
}

/** 清空本地认证信息（登出时调用）。 */
export function clearAuth(): void {
  try {
    localStorage.removeItem(ACTOR_KEY);
    localStorage.removeItem(TENANT_KEY);
  } catch {
    /* 静默降级 */
  }
}

/** API 错误：携带 HTTP 状态码与后端业务码；`status === 0` 表示网络层失败。 */
export class ApiError extends Error {
  /** HTTP 状态码（0 = 网络层错误 / DNS / 拒连 / 中断） */
  readonly status: number;
  /** 后端业务码（信封 `code` 字段；网络错误 / 非 JSON 响应为 `UNKNOWN`） */
  readonly businessCode: string;
  /** 后端链路追踪 ID（排障用；网络错误为空串） */
  readonly traceId: string;

  constructor(status: number, message: string, businessCode = 'UNKNOWN', traceId = '') {
    super(message);
    this.name = 'ApiError';
    this.status = status;
    this.businessCode = businessCode;
    this.traceId = traceId;
  }
}

/** 后端统一响应信封（`proto::ApiEnvelope` 的前端镜像）。 */
interface ApiEnvelope<T> {
  code: string;
  data: T | null;
  message: string;
  trace_id: string;
}

/** 请求选项（method / body / 额外头 / 超时）。 */
export interface AdminRequestOptions {
  /** HTTP 方法（默认 POST——licensing-server 管理端点全部为 POST） */
  method?: string;
  /** JSON 请求体（对象；由本函数序列化） */
  body?: unknown;
  /** 额外请求头（如 `X-Tenant-Id` 覆盖默认租户） */
  headers?: Record<string, string>;
  /** 总超时毫秒数（默认 15s） */
  timeoutMs?: number;
}

/**
 * 通用管理端请求：发 JSON → 解统一信封 → 业务码非 `OK` 即抛 `ApiError`。
 *
 * 默认携带 `X-Actor-Id` / `X-Tenant-Id`（本地会话写入值；可用 `headers` 覆盖）。
 * 所有请求都有 AbortController 总超时护栏（P0 教训），绝不无限等待。
 *
 * @throws `ApiError`（业务失败 / HTTP 非 2xx / 网络错误 / 非 JSON 响应）
 */
export async function adminRequest<T>(path: string, options: AdminRequestOptions = {}): Promise<T> {
  const headers = new Headers(options.headers ?? {});
  headers.set('Content-Type', 'application/json');
  const actor = getStoredActor();
  if (actor && !headers.has('X-Actor-Id')) {
    headers.set('X-Actor-Id', actor);
  }
  const tenantId = getStoredTenantId();
  if (tenantId && !headers.has('X-Tenant-Id')) {
    headers.set('X-Tenant-Id', tenantId);
  }

  const controller = new AbortController();
  const timer = setTimeout(() => controller.abort(), options.timeoutMs ?? DEFAULT_TIMEOUT_MS);

  let res: Response;
  try {
    res = await fetch(`${BASE_PATH}${path}`, {
      method: options.method ?? 'POST',
      headers,
      body: options.body === undefined ? undefined : JSON.stringify(options.body),
      signal: controller.signal,
    });
  } catch (cause) {
    // 网络错误 / 超时：包装为 status=0 的 ApiError，调用方降级并给出可读提示
    const reason =
      cause instanceof DOMException && cause.name === 'AbortError'
        ? `请求超时（${options.timeoutMs ?? DEFAULT_TIMEOUT_MS}ms）`
        : cause instanceof Error
          ? cause.message
          : String(cause);
    throw new ApiError(0, `网络请求失败：${reason}`);
  } finally {
    clearTimeout(timer);
  }

  // 先读文本再解析（绝不对流式 / 超大响应做盲目 await JSON——联调 P0 教训）
  const text = await res.text();
  let envelope: ApiEnvelope<T> | null = null;
  if (text) {
    try {
      envelope = JSON.parse(text) as ApiEnvelope<T>;
    } catch {
      envelope = null;
    }
  }

  const traceId = envelope?.trace_id ?? '';
  if (!res.ok || (envelope !== null && envelope.code !== 'OK')) {
    const message =
      envelope?.message ?? (res.ok ? '响应缺少业务信封' : `请求失败（HTTP ${res.status}）`);
    throw new ApiError(res.status, message, envelope?.code ?? 'UNKNOWN', traceId);
  }

  return (envelope?.data ?? undefined) as T;
}

/**
 * 管理员登录探测：`POST /admin/auth/login`（licensing-api.md §4 契约形状）。
 *
 * ⚠️ 后端缺口 #1：licensing-server **当前未实现该端点**——请求会得到 404（空体）。
 * 调用方（LoginPage）据此区分：
 *  · `401 / 403` → 后端已启用鉴权但凭证错误（未来兼容）；
 *  · `404 / 405 / 0`（网络不通除外——由调用方再判）→ 登录端点缺失，前端清晰降级。
 *
 * @throws `ApiError`
 */
export function adminLogin(username: string, password: string): Promise<void> {
  return adminRequest<void>('/admin/auth/login', {
    method: 'POST',
    body: { username, password },
  });
}

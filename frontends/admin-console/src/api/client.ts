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
 * ── 后端真实契约（crates/licensing-server/src/http.rs + admin_auth.rs）─────────
 *  · 统一响应包裹：`{ code: "OK" | <业务码>, data, message, trace_id }`；
 *    HTTP 状态码由业务码推出（`proto::http_status`）。
 *  · 管理端鉴权（后端已落地 admin 鉴权）：
 *      - `POST /admin/auth/login`：`{username, password}` → `{token, role}`
 *        （HS256 JWT，1h TTL）；凭据错误 401；
 *      - **除 login 外所有 /admin/* 端点必须携带 `Authorization: Bearer <token>`**；
 *      - 废弃 / 重发等写端点还要求 `X-Tenant-Id` 头；`X-Actor-Id` 仍随请求发送
 *        （后端审计操作者改用 JWT sub，该头仅作链路辅助）；
 *      - 401（token 过期 / 无效）→ 前端清 token 并跳登录页。
 *  · GET 查询端点：`/admin/overview`、`/admin/codes`、`/admin/codes/:id`、
 *    `/admin/tenants`、`/admin/devices`、`/admin/receipts/anomalies`、
 *    `/admin/keys`、`/admin/audit/logs`。
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

/** 会话 token 在 localStorage 的键名（登录成功时写入，作为 `Authorization: Bearer` 头）。 */
const TOKEN_KEY = 'iot-daq.ac.token';

/** 登录角色在 localStorage 的键名（登录成功时写入，恢复会话用）。 */
const ROLE_KEY = 'iot-daq.ac.role';

/** 读取本地会话 token（未登录返回空串）。 */
export function getStoredToken(): string {
  try {
    return localStorage.getItem(TOKEN_KEY) ?? '';
  } catch {
    return '';
  }
}

/** 写入本地会话 token（登录成功时调用）。 */
export function setStoredToken(token: string): void {
  try {
    localStorage.setItem(TOKEN_KEY, token);
  } catch {
    /* localStorage 不可用（隐私模式）时静默降级为内存态会话 */
  }
}

/** 读取本地登录角色（未登录返回空串）。 */
export function getStoredRole(): string {
  try {
    return localStorage.getItem(ROLE_KEY) ?? '';
  } catch {
    return '';
  }
}

/** 写入本地登录角色（登录成功时调用）。 */
export function setStoredRole(role: string): void {
  try {
    localStorage.setItem(ROLE_KEY, role);
  } catch {
    /* 同上：静默降级 */
  }
}

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

/** 清空本地认证信息（登出 / 401 过期时调用）。 */
export function clearAuth(): void {
  try {
    localStorage.removeItem(ACTOR_KEY);
    localStorage.removeItem(TENANT_KEY);
    localStorage.removeItem(TOKEN_KEY);
    localStorage.removeItem(ROLE_KEY);
  } catch {
    /* 静默降级 */
  }
}

/** 401 过期事件的派发名（session store 监听后同步重置内存会话）。 */
export const AUTH_EXPIRED_EVENT = 'ac-auth-expired';

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

/** 请求选项（method / body / 额外头 / 超时 / 查询参数）。 */
export interface AdminRequestOptions {
  /** HTTP 方法（默认 POST——licensing-server 管理端点全部为 POST） */
  method?: string;
  /** JSON 请求体（对象；由本函数序列化） */
  body?: unknown;
  /** 查询参数（GET 端点用；值为空串的键自动跳过） */
  query?: Record<string, string>;
  /** 额外请求头（如 `X-Tenant-Id` 覆盖默认租户） */
  headers?: Record<string, string>;
  /** 总超时毫秒数（默认 15s） */
  timeoutMs?: number;
}

/** 是否为登录端点（login 不带 Bearer，也不参与 401 跳转）。 */
function isLoginPath(path: string): boolean {
  return path.startsWith('/admin/auth/login');
}

/**
 * 通用管理端请求：发 JSON → 解统一信封 → 业务码非 `OK` 即抛 `ApiError`。
 *
 * 除 login 外自动注入 `Authorization: Bearer <token>`、`X-Actor-Id` / `X-Tenant-Id`
 * （本地会话写入值；可用 `headers` 覆盖）。所有请求都有 AbortController 总超时护栏。
 *
 * 401 统一处理（login 除外）：token 过期 / 无效 → 清空本地认证 → 派发
 * `ac-auth-expired` 事件（session store 同步重置内存会话）→ 跳转登录页。
 *
 * @throws `ApiError`（业务失败 / HTTP 非 2xx / 网络错误 / 非 JSON 响应）
 */
export async function adminRequest<T>(path: string, options: AdminRequestOptions = {}): Promise<T> {
  const headers = new Headers(options.headers ?? {});
  headers.set('Content-Type', 'application/json');
  const token = getStoredToken();
  if (token && !isLoginPath(path) && !headers.has('Authorization')) {
    headers.set('Authorization', `Bearer ${token}`);
  }
  const actor = getStoredActor();
  if (actor && !headers.has('X-Actor-Id')) {
    headers.set('X-Actor-Id', actor);
  }
  const tenantId = getStoredTenantId();
  if (tenantId && !headers.has('X-Tenant-Id')) {
    headers.set('X-Tenant-Id', tenantId);
  }

  // 查询参数：跳过空值键（空筛选条件不进 URL）
  let queryText = '';
  if (options.query) {
    const params = new URLSearchParams();
    for (const [key, value] of Object.entries(options.query)) {
      if (value !== '') {
        params.set(key, value);
      }
    }
    const raw = params.toString();
    if (raw) {
      queryText = `?${raw}`;
    }
  }

  const controller = new AbortController();
  const timer = setTimeout(() => controller.abort(), options.timeoutMs ?? DEFAULT_TIMEOUT_MS);

  let res: Response;
  try {
    res = await fetch(`${BASE_PATH}${path}${queryText}`, {
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
    // 401 = token 缺失 / 无效 / 过期：清会话并回登录页（login 自身的 401 属凭证错误，除外）
    if (res.status === 401 && !isLoginPath(path)) {
      clearAuth();
      try {
        window.dispatchEvent(new CustomEvent(AUTH_EXPIRED_EVENT));
      } catch {
        /* 事件派发失败不阻断跳转 */
      }
      if (window.location.hash !== '#/login') {
        window.location.hash = '#/login';
      }
    }
    const message =
      envelope?.message ?? (res.ok ? '响应缺少业务信封' : `请求失败（HTTP ${res.status}）`);
    throw new ApiError(res.status, message, envelope?.code ?? 'UNKNOWN', traceId);
  }

  return (envelope?.data ?? undefined) as T;
}

/**
 * 管理员登录：`POST /admin/auth/login` → `{token, role}`（HS256 JWT，1h TTL）。
 *
 * 凭据错误 / 未知用户一律 401（后端不区分原因，防账号枚举）。
 *
 * @throws `ApiError`
 */
export function adminLogin(username: string, password: string): Promise<{ token: string; role: string }> {
  return adminRequest<{ token: string; role: string }>('/admin/auth/login', {
    method: 'POST',
    body: { username, password },
  });
}

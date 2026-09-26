/**
 * @file client.ts
 * @module web-console/api/client
 * @description 原生 fetch 封装（**零新增依赖**，不引入 axios）。
 *
 * ── 模式判定 ────────────────────────────────────────────────────────────────
 * **演示模式已废除**：页面数据层（`repo.ts`）一律走真实后端（经 vite proxy
 * 转发到 `http://127.0.0.1:8080`）。`API_MODE` 常量保留供页面判断，取值恒为
 * `'real'`（见下）。
 *
 * ── 认证约定（对齐后端契约）──────────────────────────────────────────────────
 *  · `POST /api/auth/login` body `{username, password}` → 200 `{token, role}` | 401；
 *  · 业务请求统一附 `Authorization: Bearer <token>`；
 *  · 401 → 清空本地会话并跳转 `/login`（hash 路由，直接改 `location.hash`，
 *    避免 client → router 的循环依赖）；
 *  · 403 → 透传给调用方展示「权限不足」；
 *  · 网络错误 → 抛 `ApiError(status=0)` 的 rejected promise，由调用方以
 *    **诚实空态 / 结构化失败**兜底（绝不回退演示数据，页面不崩）。
 *
 * ── 大数红线 ────────────────────────────────────────────────────────────────
 * 响应体一律按 JSON 文本解析，**不做任何数值转换**；整数字段（纳秒时间戳 /
 * 序列号 / 累计计数器）由后端以字符串返回，repo 层原样透传展示，绝不 parseFloat。
 */

/** API 模式：仅 `real`（经代理访问本机 daemon）；演示模式已废除。 */
export type ApiMode = 'real';

/**
 * 当前 API 模式（**恒为 `'real'`**）。
 *
 * 历史 `VITE_API_MODE` 分流与演示数据回退已随演示数据模块一并删除；
 * 本常量保留（类型收窄为 `'real'`）以兼容仍引用它的页面代码。
 */
export const API_MODE: ApiMode = 'real';

/**
 * 管理面 API 的**绝对基址**（默认 `http://127.0.0.1:8080`，与 daemon
 * `DEFAULT_MGMT_BIND`（`crates/daemon/src/bin/iot-daq-daemon.rs:143`）对齐）。
 *
 * ★ 为什么必须显式绝对（P0「响应不是合法 JSON」根因）：
 * Tauri 打包版页面 origin 在 Windows 上是 `http://tauri.localhost`（见
 * tauri-2.11.6 `src/manager/mod.rs` 的 `get_app_url` 断言：
 * `if cfg!(windows) || cfg!(target_os = "android") { "http://tauri.localhost/" }`）。
 * 沿用相对路径 `/api/...` 时，请求会被 Tauri 的资产协议接管，而它对**任何未命中
 * 资产的路径一律回退 index.html**（同文件 `get_asset` 的
 * `log::debug!("Asset `{path}` not found; fallback to index.html")` 分支）→
 * 前端拿到 HTTP 200 + `text/html` 的 index.html → `JSON.parse` 失败 →
 * 本文件 `apiRequest` 抛出「响应不是合法 JSON」。
 * dev 之所以正常：vite dev server 把 `/api` 代理到 `http://127.0.0.1:8080`
 * （`web-console/vite.config.ts` 的 `server.proxy`），相对路径解析到
 * `http://localhost:5274/api/...` 后被正确转发；打包后没有这层代理，缺陷才暴露。
 *
 * 端口可配置：构建期用 `VITE_API_BASE` 覆盖（如 `VITE_API_BASE=http://127.0.0.1:9090`），
 * 未设置则用默认值。
 */
export const API_BASE: string =
  ((import.meta.env.VITE_API_BASE as string | undefined) ?? '').replace(/\/+$/, '') ||
  'http://127.0.0.1:8080';

/** 把 `/api/...` 路径拼成**绝对** URL（供 fetch / EventSource 使用）。 */
export function apiUrl(path: string): string {
  return path.startsWith('/') ? `${API_BASE}${path}` : `${API_BASE}/${path}`;
}

/** token 在 localStorage 的键名（会话状态同源于此，刷新不丢）。 */
const TOKEN_KEY = 'iot-daq.wc.token';

/** 后端角色在 localStorage 的键名（仅用于顶栏展示）。 */
const BACKEND_ROLE_KEY = 'iot-daq.wc.backend-role';

/** 读取本地 token（未登录返回空串）。 */
export function getStoredToken(): string {
  try {
    return localStorage.getItem(TOKEN_KEY) ?? '';
  } catch {
    return '';
  }
}

/** 写入本地 token（登录成功时调用）。 */
export function setStoredToken(token: string): void {
  try {
    localStorage.setItem(TOKEN_KEY, token);
  } catch {
    /* localStorage 不可用（隐私模式）时静默降级为内存态会话 */
  }
}

/** 读取本地后端角色（未登录返回空串）。 */
export function getStoredBackendRole(): string {
  try {
    return localStorage.getItem(BACKEND_ROLE_KEY) ?? '';
  } catch {
    return '';
  }
}

/** 写入本地后端角色（登录成功时调用）。 */
export function setStoredBackendRole(role: string): void {
  try {
    localStorage.setItem(BACKEND_ROLE_KEY, role);
  } catch {
    /* 同上：静默降级 */
  }
}

/** 清空本地认证信息（登出 / 401 时调用）。 */
export function clearAuth(): void {
  try {
    localStorage.removeItem(TOKEN_KEY);
    localStorage.removeItem(BACKEND_ROLE_KEY);
  } catch {
    /* 静默降级 */
  }
}

/** 401 回调签名（由 session store 注册，用于同步清空内存会话）。 */
type UnauthorizedListener = () => void;

/** 已注册的 401 监听器（client 不反向依赖 store，避免循环导入）。 */
const unauthorizedListeners: UnauthorizedListener[] = [];

/** 注册 401 监听器（session store 初始化时调用一次）。 */
export function onUnauthorized(listener: UnauthorizedListener): void {
  unauthorizedListeners.push(listener);
}

/** API 错误：携带 HTTP 状态码；`status === 0` 表示网络层失败（DNS / 拒连 / 中断）。 */
export class ApiError extends Error {
  /** HTTP 状态码（0 = 网络层错误） */
  readonly status: number;

  /**
   * 后端错误体（JSON 解析成功则为对象 / 数组，否则为原文字符串，无则 `null`）。
   *
   * 结构化失败契约依赖此字段：如 `POST /api/points/import` 的 400 携带
   * `{errors:[{line, reason, allowed}, ...]}`、`GET /api/audit` 的 503 携带
   * `{error, message}`——调用方据此给出「原因 + 恢复路径」，而非静默吞错。
   */
  readonly body: unknown;

  constructor(status: number, message: string, body: unknown = null) {
    super(message);
    this.name = 'ApiError';
    this.status = status;
    this.body = body;
  }
}

/** 单请求超时（毫秒）：AbortController 硬中断，防无限流 / 挂死连接拖死页面。 */
const REQUEST_TIMEOUT_MS = 15_000;

/**
 * 发起带超时的 fetch（15s 未响应则 abort → 抛 `ApiError(0)`）。
 *
 * SSE（`/api/stream` / `/api/events`）不走本函数（EventSource 自行管理生命周期），
 * 因此超时只作用于一次性 HTTP 请求。
 */
async function fetchWithTimeout(path: string, init: RequestInit): Promise<Response> {
  const controller = new AbortController();
  const timer = setTimeout(() => controller.abort(), REQUEST_TIMEOUT_MS);
  try {
    return await fetch(path, { ...init, signal: controller.signal });
  } finally {
    clearTimeout(timer);
  }
}

/** 把响应体解析为「JSON 优先、其次原文」的宽容形态（供 `ApiError.body` 携带）。 */
function parseErrorBody(text: string): unknown {
  if (!text) {
    return null;
  }
  try {
    return JSON.parse(text) as unknown;
  } catch {
    return text;
  }
}

/**
 * 401 统一处置：清本地会话 → 通知监听者 → 跳登录页。
 *
 * HTTP 层（`apiRequest`）与 SSE 层（`stream.ts` 的 401 探测）共用同一语义，
 * 保证 token 失效时两条通道的行为一致。
 */
export function handleUnauthorized(): void {
  clearAuth();
  for (const listener of [...unauthorizedListeners]) {
    listener();
  }
  if (window.location.hash !== '#/login') {
    window.location.hash = '#/login';
  }
}

/**
 * 通用请求函数。
 *
 * 请求一律打到**绝对地址** `API_BASE + path`（绝不依赖页面 URL 的相对解析——
 * Tauri 打包页 origin 为 `http://tauri.localhost`，相对路径会命中资产回退，见
 * `API_BASE` 注释）。
 *
 * @param path 以 `/api` 开头的路径
 * @param init 可选 fetch 初始化参数（method / body 等）
 * @throws `ApiError`（401 已在函数内处理跳转；其余状态码与网络错误抛给调用方）
 */
export async function apiRequest<T>(path: string, init: RequestInit = {}): Promise<T> {
  const url = apiUrl(path);
  const headers = new Headers(init.headers ?? {});
  if (init.body && !headers.has('Content-Type')) {
    headers.set('Content-Type', 'application/json');
  }
  const token = getStoredToken();
  if (token) {
    headers.set('Authorization', `Bearer ${token}`);
  }

  let res: Response;
  try {
    res = await fetchWithTimeout(url, { ...init, headers });
  } catch (cause) {
    // 网络错误 / 超时不崩：包装为 status=0 的 rejected promise，调用方降级
    const aborted = cause instanceof Error && cause.name === 'AbortError';
    const reason = aborted
      ? `请求超时（${REQUEST_TIMEOUT_MS} ms）`
      : cause instanceof Error
        ? cause.message
        : String(cause);
    // 诚实降级：把**目标地址**与「服务未就绪」这一真实原因一并给出，便于现场排障
    throw new ApiError(0, `无法连接网关服务 ${url}（${reason}）：本地 daemon 未启动、端口被占用或尚未就绪`, url);
  }

  if (res.status === 401) {
    // 登录失效：清本地会话 + 通知监听者 + 跳登录页（与 SSE 层共用同一处置）
    handleUnauthorized();
    throw new ApiError(401, '登录已失效，请重新登录');
  }

  if (!res.ok) {
    // 结构化失败（400 校验错误 / 501 未落地 / 503 未装配）依赖 body 上报原因
    throw new ApiError(res.status, `请求失败（HTTP ${res.status}）`, parseErrorBody(await res.text()));
  }

  if (res.status === 204) {
    return undefined as T;
  }
  const text = await res.text();
  if (!text) {
    return undefined as T;
  }
  try {
    return JSON.parse(text) as T;
  } catch {
    // 典型成因：请求被打到页面自身（拿到 index.html）或被代理拦截。
    // 带上 URL / Content-Type / 响应体前缀，让「响应不是合法 JSON」可诊断。
    throw new ApiError(
      res.status,
      `响应不是合法 JSON（${url}，HTTP ${res.status}，Content-Type: ${res.headers.get('Content-Type') ?? '未知'}）：${describeBodyHead(text)}`,
      text,
    );
  }
}

/** 取响应体前 120 个字符（单行压缩空白），用于失败诊断文案。 */
function describeBodyHead(text: string): string {
  return text.replace(/\s+/g, ' ').trim().slice(0, 120) || '（空响应体）';
}

/**
 * 文本型请求（CSV 导出等 `text/*` 响应；与 `apiRequest` 同鉴权 / 同超时）。
 *
 * @returns 响应原文（`res.text()`）；204 / 空体返回空串
 * @throws `ApiError`（语义与 `apiRequest` 完全一致）
 */
export async function apiRequestText(path: string, init: RequestInit = {}): Promise<string> {
  const url = apiUrl(path);
  const headers = new Headers(init.headers ?? {});
  const token = getStoredToken();
  if (token) {
    headers.set('Authorization', `Bearer ${token}`);
  }

  let res: Response;
  try {
    res = await fetchWithTimeout(url, { ...init, headers });
  } catch (cause) {
    const aborted = cause instanceof Error && cause.name === 'AbortError';
    const reason = aborted
      ? `请求超时（${REQUEST_TIMEOUT_MS} ms）`
      : cause instanceof Error
        ? cause.message
        : String(cause);
    throw new ApiError(0, `无法连接网关服务 ${url}（${reason}）：本地 daemon 未启动、端口被占用或尚未就绪`, url);
  }

  if (res.status === 401) {
    handleUnauthorized();
    throw new ApiError(401, '登录已失效，请重新登录');
  }
  if (!res.ok) {
    throw new ApiError(res.status, `请求失败（HTTP ${res.status}）`, parseErrorBody(await res.text()));
  }
  return await res.text();
}

/** 登录接口响应体（后端契约：200 `{token, role}`）。 */
export interface LoginResponse {
  /** 会话 token（后续请求以 `Bearer <token>` 携带） */
  token: string;
  /** 后端角色（ops / lic_ops / risk / system） */
  role: string;
}

/**
 * 登录：`POST /api/auth/login`。
 *
 * 401 由 apiRequest 抛出（不触发全局跳转逻辑之外的特殊处理），
 * 调用方（LoginPage）据此展示「账号或密码错误」。
 */
export function apiLogin(username: string, password: string): Promise<LoginResponse> {
  return apiRequest<LoginResponse>('/api/auth/login', {
    method: 'POST',
    body: JSON.stringify({ username, password }),
  });
}

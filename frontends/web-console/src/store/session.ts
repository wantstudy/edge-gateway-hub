/**
 * @file session.ts
 * @module web-console/store/session
 * @description 网关控制台会话与全局运行状态（当前用户、角色、授权状态、连接状态）。
 *
 * ── 边界（红线）─────────────────────────────────────────────────────────────
 *  · 会话以 localStorage 中的 JWT 为准（未登录 → 登录页）；
 *  · **授权判定一律在 Rust 侧**，本模块只持有「用于展示」的授权快照，
 *    不得作为任何授权/放行依据；快照来源于 `GET /api/license/status`（经 repo 映射），
 *    未取到真实快照时展示诚实空值（绝不掺演示数据）。
 *  · 角色枚举仅用于 `RoleGate` 的**可见性**控制（`ui-design-system.md` §3）。
 *
 * ── 为什么角色枚举是 4 个（admin / engineer / operator / viewer）────────────
 * 与 `docs/design/ui-gateway-console.md` §2 的角色模型对齐并细分：
 *   · `admin`    全部权限（含授权、账号、系统参数、备份恢复）
 *   · `engineer` 现场工程师：可改接入/转发配置并处置告警，**不可**改授权与账号
 *   · `operator` 产线操作员：仅监控、告警处置、查看授权（只读）
 *   · `viewer`   只读（总览 / 监控 / 告警 / 授权只读 / 日志 / 诊断）
 * 设计文档 §2 写的是 3 角色（admin/operator/viewer），本骨架按团队约定细分出
 * `engineer`，并在 README 中记录该偏差（对设计文档为「细化」而非「变更」）。
 */
import { reactive, readonly, computed, watch } from 'vue';
import { licenseSnapshot, type MockLicense } from '../api/model';
import { dataVersion, repo } from '../api/repo';
import { clearAuth, getStoredBackendRole, getStoredToken, onUnauthorized } from '../api/client';

/** 网关控制台四角色。 */
export const ROLES = ['admin', 'engineer', 'operator', 'viewer'] as const;

/** 角色标识类型。 */
export type Role = (typeof ROLES)[number];

/** 角色元信息（顶栏角色切换 + 账号页权限矩阵表头共用）。 */
export interface RoleMeta {
  /** 角色 id */
  readonly id: Role;
  /** 中文短名 */
  readonly label: string;
  /** 中文全名（顶栏下拉展示） */
  readonly fullLabel: string;
  /** 职责说明 */
  readonly desc: string;
}

/** 四角色元信息表（顺序即下拉顺序，admin 在上）。 */
export const ROLE_META: Readonly<Record<Role, RoleMeta>> = Object.freeze({
  admin: {
    id: 'admin',
    label: '管理员',
    fullLabel: '管理员（全部权限）',
    desc: '可配置接入与转发、触发授权与换机申请、管理账号与系统参数。',
  },
  engineer: {
    id: 'engineer',
    label: '现场工程师',
    fullLabel: '现场工程师（接入 / 转发 / 告警处置）',
    desc: '可增改设备与点位、配置北向出口与转发规则、处置告警；不可改授权与账号。',
  },
  operator: {
    id: 'operator',
    label: '操作员',
    fullLabel: '操作员（监控 + 告警处置）',
    desc: '可查看实时监控与告警并执行处置；接入与转发配置只读。',
  },
  viewer: {
    id: 'viewer',
    label: '只读',
    fullLabel: '只读（总览 / 监控 / 告警 / 授权只读 / 日志 / 诊断）',
    desc: '全站只读，写操作按钮一律不可见。',
  },
});

// ---------------------------------------------------------------------------
// UI 角色派生（网关侧权限集 → 可见性角色）
// ---------------------------------------------------------------------------

/**
 * 网关侧权限 id → UI 角色派生规则（**唯一真源**，纯函数、可测）。
 *
 * ── 为什么要从权限集派生，而不是按服务端角色名硬映射 ──────────────────────────
 * `GET /api/permissions` 默认只返回 10 项**网关侧**权限（`device.view` /
 * `device.write` / `point.write` / `audit.view` / `audit.export` / `account.view` /
 * `account.update` / `ops.restart` / `ops.collectors` / `ops.logs_read`）。
 * 登录签发的 JWT 把账号实际持有哪些权限写进 `perms` claim（服务端签名防篡改）。
 * 因此 UI 角色应当**由权限集派生**——两端角色名（服务端 `system`/`ops`/`lic_ops`/
 * `risk` vs 客户端 `admin`/`engineer`/`operator`/`viewer`）本就是两套模型，
 * 直接按名字映射会把厂商侧运营角色错误地提权或降权。
 *
 * ── 规则（按优先级自上而下，命中即返回）────────────────────────────────────
 * | # | 条件                                                   | 结果       |
 * |---|--------------------------------------------------------|------------|
 * | 1 | `backendRole === 'system'`                             | `admin`    |
 * | 2 | 含 `account.update`，或同时含 `ops.restart`+`device.write` | `admin`    |
 * | 3 | 含 `device.write` 或 `point.write`                     | `engineer` |
 * | 4 | 含 `ops.collectors` 或 `ops.restart`                   | `operator` |
 * | 5 | 其余（仅 `*.view` / 空集）                             | `viewer`   |
 *
 * ⚠️ 判定只用**明确的权限 id 字面量**（精确成员判断），不做子串/前后缀模糊匹配，
 * 避免 `ops.logs_read` 之类的只读权限被误判为写权限。系统管理员档（`system`）
 * 保留直通 `admin`，防止引导账号被降权而锁死配置入口。
 *
 * ⚠️ 本函数只影响 `RoleGate` 的按钮 / 菜单**可见性**，**不是授权判定**——
 * 真正的授权一律在网关 Rust 侧按 token 的 `perms` 执行。
 *
 * @param perms 账号权限集（来自 JWT `perms` claim 或 `/api/permissions`）
 * @param backendRole 服务端角色原文（`/api/auth/login` 返回的 `role`）；仅 `system` 有特殊语义
 */
export function deriveRoleFromPerms(perms: readonly string[], backendRole?: string): Role {
  if (backendRole === 'system') {
    return 'admin';
  }
  const held = new Set(perms);
  if (held.has('account.update') || (held.has('ops.restart') && held.has('device.write'))) {
    return 'admin';
  }
  if (held.has('device.write') || held.has('point.write')) {
    return 'engineer';
  }
  if (held.has('ops.collectors') || held.has('ops.restart')) {
    return 'operator';
  }
  return 'viewer';
}

/**
 * base64url 解码为 UTF-8 字符串（JWT 段 → 原文）。
 *
 * JWT 段用的是 base64url（`-` / `_` 替换 `+` / `/`，且省略尾随 `=`），需先还原
 * 再 `atob`。解码失败（非法字符 / 截断）返回 `null`，由调用方走兜底。
 */
function decodeBase64Url(segment: string): string | null {
  try {
    const base64 = segment.replace(/-/g, '+').replace(/_/g, '/');
    const padded = base64 + '='.repeat((4 - (base64.length % 4)) % 4);
    const binary = atob(padded);
    const bytes = Uint8Array.from(binary, (ch) => ch.charCodeAt(0));
    return new TextDecoder().decode(bytes);
  } catch {
    return null;
  }
}

/**
 * 解析 JWT **payload**（中段）里的 `perms` claim → 权限集。
 *
 * ⚠️ **只读解析，不校验签名**：前端不做任何安全判定，真正的授权判定在网关 Rust
 * 侧。此处解析 token 仅为了让 UI 在「带 token 刷新页面」后仍能还原出正确的
 * 按钮可见性角色——即便 token 被篡改，最多影响按钮显隐，任何越权请求都会被
 * 网关拒绝。切勿把本函数的结果当作授权依据。
 *
 * @returns 权限 id 数组；token 非法 / 无 `perms` claim → `null`（调用方走兜底映射）
 */
export function parseJwtPerms(token: string): readonly string[] | null {
  if (!token) {
    return null;
  }
  const segments = token.split('.');
  if (segments.length < 2 || !segments[1]) {
    return null;
  }
  const payload = decodeBase64Url(segments[1]);
  if (payload === null) {
    return null;
  }
  try {
    const body = JSON.parse(payload) as { perms?: unknown };
    if (!Array.isArray(body.perms)) {
      return null;
    }
    return body.perms.filter((item): item is string => typeof item === 'string');
  } catch {
    return null;
  }
}

/** 顶栏连接状态（实时通道 / 北向出口联通性）。 */
export type ConnectionState = 'connected' | 'degraded' | 'disconnected';

/** 会话内部可变状态。 */
interface SessionState {
  /** 是否已登录 */
  loggedIn: boolean;
  /** 当前用户账号 */
  account: string;
  /** 当前用户显示名 */
  displayName: string;
  /** 当前角色（由 JWT `perms` 派生，拿不到时回落服务端角色映射；仅控制 `RoleGate` 可见性） */
  role: Role;
  /** 后端角色原文（登录后由 `/api/auth/login` 返回；未登录为空串） */
  backendRole: string;
  /** 展示用授权快照（**非**授权判定，判定在 Rust 侧；真实值来自 `/api/license/status`） */
  license: MockLicense;
  /** 顶栏连接状态指示 */
  connection: ConnectionState;
  /** 侧栏是否折叠 */
  sidebarCollapsed: boolean;
}

/** 可变状态对象（模块内可直接写，对外只读）。
 *
 * 登录态以 localStorage token 为准（未登录 → 登录页）；授权快照初始为诚实空值，
 * 待 `preloadRealData()` 填充后由 `dataVersion` 驱动刷新为真实值。
 */
const state: SessionState = reactive<SessionState>({
  loggedIn: Boolean(getStoredToken()),
  account: '',
  displayName: '',
  // 启动即从 token 的 `perms` 派生 UI 角色：带 token 刷新（F5）后不再停在 'viewer'，
  // 否则写操作按钮会被 RoleGate 替换为「无权操作」。解析失败 / 无 perms → 兜底映射。
  role: resolveInitialRole(),
  backendRole: getStoredBackendRole(),
  license: licenseSnapshot,
  connection: 'connected',
  sidebarCollapsed: false,
});

/**
 * 服务端角色原文 → UI 角色（**仅兜底**，首选 {@link deriveRoleFromPerms}）。
 *
 * ── 两端角色模型**互不映射**（task：权限两端隔离）────────────────────────────
 * 这里产出的是**网关侧自有**的 4 个 UI 角色（`admin` / `engineer` / `operator` /
 * `viewer`），它与厂商侧（licensing-server / admin-console）的角色模型
 * （`ops` / `lic_ops` / `risk` / `system`）**不是同一套东西**，也不做语义对齐：
 *  · 网关控制台**不消费**任何厂商侧权限（`code.*` / `tenant.*` / `key.*` /
 *    `receipt.*` / `transfer.*` / `device.mark_anomaly`，见 `rbac::PermissionScope`）；
 *  · 厂商侧运营角色（`ops` / `lic_ops` / `risk`）**不下放**任何网关侧写权限——
 *    一律收敛为只读档 `viewer`（服务端角色 → 网关侧只读）；
 *  · 仅网关自身的系统管理员档 `system`（即引导账号 `root` 所在档）映射为
 *    `admin`，否则配置端将被锁死（网关账号/角色管理入口不可达）。
 *
 * ⚠️ 历史实现把 `system/ops → admin`、`lic_ops → operator`、`risk → viewer`
 * 「强行压平」，使**厂商侧运营角色**获得了网关侧管理/配置权限——这是两端权限
 * 串味的根因之一。现已按上述隔离语义收口。
 *
 * ⚠️ **本函数已降级为兜底**：正常路径一律由 `deriveRoleFromPerms`（基于 JWT
 * `perms` 的网关侧权限集）派生 UI 角色；仅当拿不到 `perms`（老 token / 解析失败）
 * 时才回落到本函数。授权判定一律在 Rust 侧按 token 的权限集执行，本函数仅影响
 * 按钮可见性。
 */
export function mapBackendRole(backendRole: string): Role {
  if (backendRole === 'system') {
    return 'admin';
  }
  return 'viewer';
}

/**
 * 从已有 token / 后端角色原文解析 UI 角色（**首选 `perms` 派生，兜底再映射**）。
 *
 * 解析顺序：
 *  1. 取 token 的 `perms`（{@link parseJwtPerms}）→ {@link deriveRoleFromPerms}；
 *  2. 拿不到 `perms`（老 token / 解析失败）→ {@link mapBackendRole}（仅 `system` → `admin`）；
 *  3. 仍为空 → `viewer`。
 *
 * 该函数同时用于**启动时**（带 token 刷新页面）与**登录成功后**，保证 refresh
 * 后角色不会停在 `'viewer'` 导致写操作按钮被替换为「无权操作」。
 */
export function resolveRoleFromToken(token: string, backendRole: string): Role {
  const perms = parseJwtPerms(token);
  if (perms) {
    return deriveRoleFromPerms(perms, backendRole);
  }
  return mapBackendRole(backendRole);
}

/** 启动引导时的角色：读本地 token → 派生（无 token / 无 perms → 兜底）。 */
function resolveInitialRole(): Role {
  return resolveRoleFromToken(getStoredToken(), getStoredBackendRole());
}

/** `login()` 可选覆盖项。 */
export interface LoginOptions {
  /** 后端角色原文（`/api/auth/login` 返回的 `role`；未登录为空串） */
  backendRole?: string;
  /** 会话 token；优先从其中 `perms` claim 派生 UI 角色 */
  token?: string;
  /** 已解析的权限集（优先级高于 token；供测试或已解析调用方直接传入） */
  perms?: readonly string[];
  /** 最后兜底的显式角色（拿不到 perms 且无 backendRole 时使用） */
  role?: Role;
}

/**
 * 登录：记录账号，并按**首选 `perms` 派生**的规则写入 UI 角色。
 *
 * 角色解析优先级：`perms`（显式传入，或从 `token` 解析）→ 显式 `role` →
 * `mapBackendRole(backendRole)` → `viewer`。
 */
function login(account: string, opts?: LoginOptions): void {
  if (opts?.backendRole !== undefined) {
    state.backendRole = opts.backendRole;
  }
  const perms = opts?.perms ?? parseJwtPerms(opts?.token ?? '');
  state.role = perms
    ? deriveRoleFromPerms(perms, opts?.backendRole ?? state.backendRole)
    : (opts?.role ?? mapBackendRole(opts?.backendRole ?? state.backendRole));
  state.loggedIn = true;
  state.account = account;
  state.displayName = account;
}

/** 登出：清空会话并清除本地 token。 */
function logout(): void {
  state.loggedIn = false;
  state.account = '';
  state.displayName = '';
  state.role = 'viewer';
  state.backendRole = '';
  state.license = licenseSnapshot;
  clearAuth();
}

/** 设置顶栏连接状态指示。 */
function setConnection(next: ConnectionState): void {
  state.connection = next;
}

/** 切换侧栏折叠。 */
function toggleSidebar(): void {
  state.sidebarCollapsed = !state.sidebarCollapsed;
}

/** 更新展示用授权快照（仅演示降级横幅，非判定入口）。 */
function setLicense(next: MockLicense): void {
  state.license = next;
}

/** 401 统一处理：client 层清 token 后同步清空内存会话（下一跳由守卫接管）。 */
onUnauthorized(() => {
  state.loggedIn = false;
});

/** 授权快照刷新：读取 repo 映射的真实授权状态（缓存未就绪时为诚实空值）。 */
function refreshLicense(): void {
  state.license = repo.getLicense();
}

/** 缓存填充 / 写操作刷新后，由 `dataVersion` 驱动同步顶栏授权快照。 */
watch(dataVersion, refreshLicense, { immediate: true });

/** 会话 API（对组件暴露只读状态 + 动作）。 */
export const session = {
  state: readonly(state),
  login,
  logout,
  setConnection,
  toggleSidebar,
  setLicense,
};

/** 当前用户显示名首字符（顶栏头像）。 */
export const accountInitial = computed(() => (state.displayName ? state.displayName.slice(0, 1) : '—'));

/** 授权是否处于可用档位（用于顶栏徽标着色：合法 = 中性/绿，降级 = 琥珀）。 */
export const licenseHealthy = computed(() => state.license.status === 'active' || state.license.status === 'trial');

/**
 * 顶栏授权徽标文案（项目硬性约定：授权状态常驻顶栏）。
 *
 * 例：`已授权 · 标准版 · 剩余 362 天` / 试用 `试用中 · 专业版试用 · 剩余 2 天 14 小时`。
 */
export const licenseBadgeText = computed<string>(() => {
  const lic = state.license;
  if (lic.status === 'active') {
    return `已授权 · ${lic.tierName} · 剩余 ${lic.remainingDays} 天`;
  }
  if (lic.status === 'trial') {
    return `试用中 · ${lic.tierName} · 剩余 ${lic.remainingText}`;
  }
  if (lic.status === 'grace') {
    return `已降级 · ${lic.tierName} · 宽限期 ${lic.remainingText}`;
  }
  return `已停用 · ${lic.tierName}`;
});

/** 降级原因文案（降级时顶栏「查看原因」链接的悬浮/跳转目标说明）。 */
export const licenseReasonText = computed<string>(() => state.license.degradeReason);

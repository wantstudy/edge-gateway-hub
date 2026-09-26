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
  /** 当前角色（登录后由后端角色映射而来，仅控制 `RoleGate` 可见性） */
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
  role: 'viewer',
  backendRole: getStoredBackendRole(),
  license: licenseSnapshot,
  connection: 'connected',
  sidebarCollapsed: false,
});

/**
 * 后端账号角色 → 网关控制台**可见性**角色（仅控制 `RoleGate` 可见性，非授权判定）。
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
 * 授权判定一律在 Rust 侧按 token 的权限集（gateway scope）执行，本映射仅影响
 * 按钮可见性。
 */
export function mapBackendRole(backendRole: string): Role {
  if (backendRole === 'system') {
    return 'admin';
  }
  return 'viewer';
}

/** 登录：记录账号；可选覆盖角色与后端角色原文（由 LoginPage 传入）。 */
function login(account: string, opts?: { role?: Role; backendRole?: string }): void {
  state.loggedIn = true;
  state.account = account;
  state.displayName = account;
  state.role = opts?.role ?? 'viewer';
  if (opts?.backendRole !== undefined) {
    state.backendRole = opts.backendRole;
  }
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

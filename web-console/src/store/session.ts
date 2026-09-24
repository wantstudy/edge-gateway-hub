/**
 * @file session.ts
 * @module web-console/store/session
 * @description 网关控制台会话与全局运行状态（当前用户、角色、授权状态、连接状态）。
 *
 * ── 边界（红线）─────────────────────────────────────────────────────────────
 *  · 本原型不接后端，会话仅存在于内存（刷新即回登录态）；
 *  · **授权判定一律在 Rust 侧**，本模块只持有「用于展示」的授权快照，
 *    不得作为任何授权/放行依据；
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
import { reactive, readonly, computed } from 'vue';
import { licenseSnapshot, type MockLicense } from '../mock/mock-data';

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
  /** 当前角色（顶栏可切换，用于演示 RoleGate 与权限矩阵真实生效） */
  role: Role;
  /** 展示用授权快照（**非**授权判定，判定在 Rust 侧） */
  license: MockLicense;
  /** 顶栏连接状态指示 */
  connection: ConnectionState;
  /** 侧栏是否折叠 */
  sidebarCollapsed: boolean;
}

/** 可变状态对象（模块内可直接写，对外只读）。 */
const state: SessionState = reactive<SessionState>({
  loggedIn: true,
  account: 'field.zhang',
  displayName: '张工',
  role: 'admin',
  license: licenseSnapshot,
  connection: 'connected',
  sidebarCollapsed: false,
});

/** 登录：记录账号并重置为 admin（便于演示全量页面）。 */
function login(account: string): void {
  state.loggedIn = true;
  state.account = account || 'field.zhang';
  state.displayName = account || '张工';
  state.role = 'admin';
}

/** 登出：清空会话。 */
function logout(): void {
  state.loggedIn = false;
  state.account = '';
  state.displayName = '';
  state.role = 'viewer';
}

/**
 * 切换角色（顶栏下拉）——用于验证 `RoleGate` 的可见性控制真实生效。
 *
 * ⚠️ 切换角色**不会**改变任何授权状态：授权快照来自 Rust 侧的租约校验，
 *    与前端角色无关（红线 3）。
 */
function setRole(role: Role): void {
  state.role = role;
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

/** 会话 API（对组件暴露只读状态 + 动作）。 */
export const session = {
  state: readonly(state),
  login,
  logout,
  setRole,
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

/**
 * @file session.ts
 * @module admin-console/store/session
 * @description 管理员会话与全局运营状态（登录态、角色、双人复核开关）。
 *
 * 注：顶栏「待处理 / 回执」计数**不再由本模块持有**——历史实现曾硬编码演示常量
 * （`receiptOk: 398` / `receiptTotal: 412` / `pendingCount: 9`），已移除；真实计数
 * 一律来自 `repo.overview()`（`GET /admin/overview`），缺失维度展示诚实空态 `—`。
 *
 * 会话策略（real 模式已接入后端鉴权，契约 = crates/licensing-server/src/admin_auth.rs）：
 *  · `POST /admin/auth/login` 返回 `{token, role}`（HS256 JWT，1h TTL）；
 *  · token + role + 操作者 + 默认租户 ID 均持久化到 localStorage——刷新后经
 *    `restore()` 恢复会话（mock 模式不持久化，行为零回归）；
 *  · token 过期（后端 401 `SESSION_EXPIRED`）由 client.ts 统一捕获：清 localStorage、
 *    派发 `ac-auth-expired` 事件 → 本模块监听并重置内存会话 → 路由守卫跳登录页；
 *  · 操作者账号同步写入 localStorage（client.ts 读取后作为 `X-Actor-Id` 头）。
 */
import { reactive, readonly, computed } from 'vue';
import type { Role } from '@ui-kit';
import {
  AUTH_EXPIRED_EVENT,
  clearAuth,
  getStoredActor,
  getStoredRole,
  getStoredTenantId,
  getStoredToken,
  setStoredActor,
  setStoredRole,
  setStoredTenantId,
  setStoredToken,
} from '../api/client';

/** 会话内部可变状态。 */
interface SessionState {
  /** 是否已登录 */
  loggedIn: boolean;
  /** 当前管理员账号（同时作为 X-Actor-Id 操作者标识） */
  account: string;
  /** 当前角色（real 模式取自后端签发的 JWT role claim） */
  role: Role;
  /** 默认租户 ID（real 模式发放 / 废弃 / 重发请求的租户兜底） */
  tenantId: string;
  /** 是否开启双人复核（开启后废弃 / 重发必须填写第二审批人） */
  dualApproval: boolean;
}

/** 可变状态对象（模块内可直接写，对外只读）。 */
const state: SessionState = reactive<SessionState>({
  loggedIn: false,
  account: '',
  role: 'system',
  tenantId: '',
  dualApproval: false,
});

/** 是否为后端签发的规范角色（后端只发四个规范 id，别名一律拒绝）。 */
function isKnownRole(role: string): role is Role {
  return role === 'ops' || role === 'lic_ops' || role === 'risk' || role === 'system';
}

/** 把后端角色字面量收敛为前端 Role（未知值降级为最小权限的 ops）。 */
function toRole(role: string): Role {
  return isKnownRole(role) ? role : 'ops';
}

/**
 * 登录：记录账号 / 默认租户 ID，并在 real 模式提供 token + 角色时持久化。
 *
 * @param account  管理员账号（= X-Actor-Id 操作者标识）
 * @param tenantId 默认租户 ID（写端点 X-Tenant-Id 头兜底）
 * @param token    会话 JWT（real 模式必传；mock 模式不传）
 * @param role     后端签发的规范角色（real 模式必传）
 */
function login(account: string, tenantId = '', token = '', role = ''): void {
  state.loggedIn = true;
  state.account = account || 'admin';
  state.tenantId = tenantId.trim();
  if (token && role) {
    state.role = toRole(role);
    setStoredToken(token);
    setStoredRole(state.role);
  }
  // 同步到 localStorage：client.ts 的请求头（Bearer / X-Actor-Id / X-Tenant-Id）同源于此
  setStoredActor(state.account);
  setStoredTenantId(state.tenantId);
}

/** 登出：清空会话与本地认证信息。 */
function logout(): void {
  resetState();
  clearAuth();
}

/** 重置内存会话（不重复清 localStorage 的内部工具）。 */
function resetState(): void {
  state.loggedIn = false;
  state.account = '';
  state.tenantId = '';
  state.role = 'system';
}

/**
 * 恢复会话（应用启动时调用一次；仅 real 模式生效，mock 行为零回归）。
 *
 * localStorage 中存在 token 即视为已登录（token 是否仍有效由首个请求的 401
 * 统一裁决——后端校验失败会经 client.ts 清会话并跳登录页，前端绝不自证有效期）。
 *
 * @param isReal 是否 real 模式（构建期由 VITE_API_MODE 决定）
 */
function restore(isReal: boolean): void {
  if (!isReal) {
    return;
  }
  const token = getStoredToken();
  if (!token) {
    return;
  }
  const account = getStoredActor();
  const tenantId = getStoredTenantId();
  const role = getStoredRole();
  state.loggedIn = true;
  state.account = account || 'admin';
  state.tenantId = tenantId.trim();
  state.role = role ? toRole(role) : 'system';
}

/** 切换角色（顶栏下拉）——用于验证权限矩阵真实生效。 */
function setRole(role: Role): void {
  state.role = role;
}

/** 切换双人复核开关。 */
function setDualApproval(enabled: boolean): void {
  state.dualApproval = enabled;
}

// 401 过期事件：client.ts 在后端判令 token 失效时派发，这里同步重置内存会话。
// （localStorage 已由 client.ts 清空；路由守卫随 loggedIn=false 把用户引到登录页。）
if (typeof window !== 'undefined') {
  window.addEventListener(AUTH_EXPIRED_EVENT, () => {
    resetState();
  });
}

/** 会话 API（对组件暴露只读状态 + 动作）。 */
export const session = {
  state: readonly(state),
  login,
  logout,
  restore,
  setRole,
  setDualApproval,
};

/** 当前登录管理员的显示名（取账号首字母大写，用于顶栏头像）。 */
export const accountInitial = computed(() => (state.account ? state.account.slice(0, 1).toUpperCase() : '—'));

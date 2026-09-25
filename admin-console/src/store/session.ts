/**
 * @file session.ts
 * @module admin-console/store/session
 * @description 管理员会话与全局运营状态（登录态、角色、双人复核开关、回执告警计数）。
 *
 * 会话策略（real 模式联调约定）：
 *  · 后端当前未实现管理员登录 / RBAC（后端缺口 #1），会话仍为前端本地态；
 *  · real 模式登录时额外记录「默认租户 ID」——它既是 `POST /admin/codes/issue`
 *    的 `tenant_id` 来源，也是废弃 / 重发请求 `X-Tenant-Id` 头的兜底值；
 *  · 操作者账号会同步写入 localStorage（client.ts 读取后作为 `X-Actor-Id` 头）。
 */
import { reactive, readonly, computed } from 'vue';
import type { Role } from '@ui-kit';
import { setStoredActor, setStoredTenantId, clearAuth } from '../api/client';

/** 会话内部可变状态。 */
interface SessionState {
  /** 是否已登录 */
  loggedIn: boolean;
  /** 当前管理员账号（同时作为 X-Actor-Id 操作者标识） */
  account: string;
  /** 当前角色（可在顶栏切换，用于演示权限矩阵真实生效） */
  role: Role;
  /** 默认租户 ID（real 模式发放 / 废弃 / 重发请求的租户兜底） */
  tenantId: string;
  /** 是否开启双人复核（开启后废弃 / 重发必须填写第二审批人） */
  dualApproval: boolean;
  /** 待处理项计数（顶栏徽标） */
  pendingCount: number;
  /** 回执健康台数 / 总台数（顶栏胶囊） */
  receiptOk: number;
  receiptTotal: number;
}

/** 可变状态对象（模块内可直接写，对外只读）。 */
const state: SessionState = reactive<SessionState>({
  loggedIn: false,
  account: '',
  role: 'system',
  tenantId: '',
  dualApproval: false,
  pendingCount: 9,
  receiptOk: 398,
  receiptTotal: 412,
});

/** 登录：记录账号（与可选的默认租户 ID）并重置为默认角色（system，便于演示密钥/租户页）。 */
function login(account: string, tenantId = ''): void {
  state.loggedIn = true;
  state.account = account || 'admin';
  state.tenantId = tenantId.trim();
  state.role = 'system';
  // 同步到 localStorage：client.ts 的请求头（X-Actor-Id / X-Tenant-Id）同源于此
  setStoredActor(state.account);
  setStoredTenantId(state.tenantId);
}

/** 登出：清空会话与本地认证信息。 */
function logout(): void {
  state.loggedIn = false;
  state.account = '';
  state.tenantId = '';
  state.role = 'system';
  clearAuth();
}

/** 切换角色（顶栏下拉）——用于验证权限矩阵真实生效。 */
function setRole(role: Role): void {
  state.role = role;
}

/** 切换双人复核开关。 */
function setDualApproval(enabled: boolean): void {
  state.dualApproval = enabled;
}

/** 会话 API（对组件暴露只读状态 + 动作）。 */
export const session = {
  state: readonly(state),
  login,
  logout,
  setRole,
  setDualApproval,
};

/** 当前登录管理员的显示名（取账号首字母大写，用于顶栏头像）。 */
export const accountInitial = computed(() => (state.account ? state.account.slice(0, 1).toUpperCase() : '—'));

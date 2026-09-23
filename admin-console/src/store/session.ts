/**
 * @file session.ts
 * @module admin-console/store/session
 * @description 管理员会话与全局运营状态（登录态、角色、双人复核开关、回执告警计数）。
 *
 * 说明：本原型不接后端，会话仅存在于内存（刷新即回登录页），
 * 符合「前端不持有私钥、不直连授权数据库」的边界要求。
 */
import { reactive, readonly, computed } from 'vue';
import type { Role } from '@ui-kit';

/** 会话内部可变状态。 */
interface SessionState {
  /** 是否已登录 */
  loggedIn: boolean;
  /** 当前管理员账号 */
  account: string;
  /** 当前角色（可在顶栏切换，用于演示权限矩阵真实生效） */
  role: Role;
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
  dualApproval: false,
  pendingCount: 9,
  receiptOk: 398,
  receiptTotal: 412,
});

/** 登录：记录账号并重置为默认角色（system，便于演示密钥/租户页）。 */
function login(account: string): void {
  state.loggedIn = true;
  state.account = account || 'admin';
  state.role = 'system';
}

/** 登出：清空会话。 */
function logout(): void {
  state.loggedIn = false;
  state.account = '';
  state.role = 'system';
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

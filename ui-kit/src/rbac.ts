/**
 * @file rbac.ts
 * @module ui-kit/rbac
 * @description 管理后台 RBAC 的**页面级 + 操作级**权限矩阵。
 *
 * 权威来源：`docs/design/licensing-api.md` §4「RBAC 四角色」与 `ui-admin-console.md` §2 页面清单。
 *
 * ⚠️ 重要边界（设计系统 §3 `RoleGate` 约束）：
 * 本模块**仅用于前端可见性/可用性控制**，不承担授权判定。真正的判定在 Rust 侧与管理 API。
 * 因此 `can()` 返回 false 时前端要**不渲染或禁用**（而非「点了才报 403」），
 * 但后端必须独立再校验一次（前端可控）。
 */

import type { Component } from 'vue';
import {
  IconDashboard,
  IconFile,
  IconComputer,
  IconSafe,
  IconExclamationCircle,
  IconSwap,
  IconLock,
  IconHistory,
  IconUserGroup,
} from '@arco-design/web-vue/es/icon';

/** 四角色（对应 licensing-api.md §4 / task 57）。 */
export const ROLES = ['ops', 'lic_ops', 'risk', 'system'] as const;

/** 角色标识类型。 */
export type Role = (typeof ROLES)[number];

/** 页面标识类型（11 个页面）。 */
export type PageId =
  | 'login'
  | 'overview'
  | 'codes'
  | 'code-detail'
  | 'devices'
  | 'device-detail'
  | 'tenants'
  | 'receipts'
  | 'transfers'
  | 'keys'
  | 'audit'
  | 'accounts';

/** 可授权操作清单（操作级权限的最小闭集）。 */
export type Action =
  | 'code.view'
  | 'code.issue'
  | 'code.revoke'
  | 'code.reissue'
  | 'code.reveal'
  | 'device.view'
  | 'device.mark_anomaly'
  | 'tenant.view'
  | 'tenant.policy_update'
  | 'receipt.view'
  | 'receipt.mark'
  | 'transfer.view'
  | 'transfer.process'
  | 'key.view'
  | 'key.rotate'
  | 'audit.view'
  | 'audit.export'
  | 'account.view'
  | 'account.update';

/** 角色元信息（用于顶栏角色切换与账号页权限矩阵表头）。 */
export interface RoleMeta {
  readonly id: Role;
  /** 中文短名 */
  readonly label: string;
  /** 中文全名 */
  readonly fullLabel: string;
  /** 职责说明 */
  readonly desc: string;
}

/** 四角色元信息表。 */
export const ROLE_META: Readonly<Record<Role, RoleMeta>> = Object.freeze({
  ops: {
    id: 'ops',
    label: '运营',
    fullLabel: '运营（只读 + 发放）',
    desc: '日常发码与查询；不可废弃 / 重发，不可改租户策略与密钥。',
  },
  lic_ops: {
    id: 'lic_ops',
    label: '授权运营',
    fullLabel: '授权运营（发放 / 废弃 / 重发）',
    desc: '执行高危授权操作（废弃、重发、换机工单），须填原因并接受审计。',
  },
  risk: {
    id: 'risk',
    label: '风控',
    fullLabel: '风控（回执异常 / 审计只读 + 标记异常）',
    desc: '核查回执异常并标记处置，无权发放 / 废弃激活码。',
  },
  system: {
    id: 'system',
    label: '系统',
    fullLabel: '系统（密钥轮换 / 租户档位配置）',
    desc: '签名密钥轮换与租户策略配置，不参与日常发码。',
  },
});

/** 页面元信息。 */
export interface PageMeta {
  readonly id: PageId;
  readonly title: string;
  /** 导航分组（按运维动线，不按技术模块） */
  readonly group: string;
  /** 面包屑 */
  readonly crumb: string;
  /** 可见该页面的角色（页面级门控） */
  readonly visibleTo: readonly Role[];
  /** 侧边导航图标 */
  readonly icon?: Component;
  /** 是否为详情页（不直接出现在导航，由列表跳转） */
  readonly detailOnly?: boolean;
}

/** 页面清单（顺序即导航顺序）。 */
export const PAGES: readonly PageMeta[] = Object.freeze([
  {
    id: 'overview',
    title: '总览',
    group: '运营',
    crumb: '运营 / 总览',
    visibleTo: ['ops', 'lic_ops', 'risk', 'system'],
    icon: IconDashboard,
  },
  {
    id: 'codes',
    title: '激活码管理',
    group: '授权运营',
    crumb: '授权运营 / 激活码管理',
    visibleTo: ['ops', 'lic_ops'],
    icon: IconFile,
  },
  {
    id: 'code-detail',
    title: '激活码详情',
    group: '授权运营',
    crumb: '授权运营 / 激活码管理',
    visibleTo: ['ops', 'lic_ops'],
    icon: IconFile,
    detailOnly: true,
  },
  {
    id: 'devices',
    title: '设备管理',
    group: '授权运营',
    crumb: '授权运营 / 设备管理',
    visibleTo: ['ops', 'lic_ops'],
    icon: IconComputer,
  },
  {
    id: 'device-detail',
    title: '设备详情',
    group: '授权运营',
    crumb: '授权运营 / 设备管理',
    visibleTo: ['ops', 'lic_ops'],
    icon: IconComputer,
    detailOnly: true,
  },
  {
    id: 'tenants',
    title: '租户与策略',
    group: '授权运营',
    crumb: '授权运营 / 租户与策略',
    visibleTo: ['system'],
    icon: IconSafe,
  },
  {
    id: 'receipts',
    title: '回执与异常',
    group: '风控',
    crumb: '风控 / 回执与异常',
    visibleTo: ['ops', 'lic_ops', 'risk', 'system'],
    icon: IconExclamationCircle,
  },
  {
    id: 'transfers',
    title: '换机工单',
    group: '风控',
    crumb: '风控 / 换机工单',
    visibleTo: ['lic_ops'],
    icon: IconSwap,
  },
  {
    id: 'keys',
    title: '签名密钥管理',
    group: '风控',
    crumb: '风控 / 签名密钥管理',
    visibleTo: ['system'],
    icon: IconLock,
  },
  {
    id: 'audit',
    title: '审计日志',
    group: '系统',
    crumb: '系统 / 审计日志',
    visibleTo: ['system', 'risk'],
    icon: IconHistory,
  },
  {
    id: 'accounts',
    title: '账号与角色',
    group: '系统',
    crumb: '系统 / 账号与角色',
    visibleTo: ['system'],
    icon: IconUserGroup,
  },
]);

/**
 * 操作级权限矩阵：`角色 → 允许的操作集合`。
 *
 * 与 `ui-admin-console.md` §2 的可见角色对齐：
 * - 租户与策略 / 密钥 / 账号 仅 system；
 * - 激活码 与 设备 由 ops / lic_ops 承担，其中废弃/重发仅 lic_ops；
 * - 回执异常与审计只读由 risk / system（+ ops/lic_ops 看回执）承担。
 */
export const ACTION_MATRIX: Readonly<Record<Role, readonly Action[]>> = Object.freeze({
  ops: [
    'code.view',
    'code.issue',
    'device.view',
    'receipt.view',
    'receipt.mark',
  ],
  lic_ops: [
    'code.view',
    'code.issue',
    'code.revoke',
    'code.reissue',
    'code.reveal',
    'device.view',
    'device.mark_anomaly',
    'receipt.view',
    'receipt.mark',
    'transfer.view',
    'transfer.process',
  ],
  risk: [
    'receipt.view',
    'receipt.mark',
    'audit.view',
    'device.view',
    'code.view',
  ],
  system: [
    'tenant.view',
    'tenant.policy_update',
    'key.view',
    'key.rotate',
    'audit.view',
    'audit.export',
    'account.view',
    'account.update',
    'code.reveal',
    'device.view',
    'receipt.view',
  ],
});

/**
 * 页面级门控：当前角色是否可见某页面。
 *
 * @param role 当前角色
 * @param page 页面 id
 */
export function canSeePage(role: Role, page: PageId): boolean {
  const meta = PAGES.find((p) => p.id === page);
  if (!meta) {
    return false;
  }
  return meta.visibleTo.includes(role);
}

/**
 * 操作级门控：当前角色是否被允许某操作。
 *
 * ⚠️ 仅控制可见性/禁用态，**不替代服务端判定**。
 *
 * @param role 当前角色
 * @param action 操作
 */
export function can(role: Role, action: Action): boolean {
  return ACTION_MATRIX[role].includes(action);
}

/**
 * 当前角色首个可见页面（用于切换角色后校正当前页）。
 *
 * @param role 当前角色
 */
export function firstAllowedPage(role: Role): PageId {
  const found = PAGES.find((p) => !p.detailOnly && p.visibleTo.includes(role));
  return found ? found.id : 'overview';
}

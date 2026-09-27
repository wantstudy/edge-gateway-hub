/**
 * @file rbac.ts
 * @module ui-kit/rbac
 * @description 管理后台 RBAC 的**页面级 + 操作级**权限矩阵（**唯一真源 / single source of truth**）。
 *
 * 权威来源：`docs/design/licensing-api.md` §4「RBAC 四角色」与 `ui-admin-console.md` §2 页面清单。
 *
 * ⚠️ 重要边界（设计系统 §3 `RoleGate` 约束）：
 * 本模块**仅用于前端可见性/可用性控制**，不承担授权判定。真正的判定在 Rust 侧与管理 API。
 * 因此 `can()` 返回 false 时前端要**不渲染或禁用**（而非「点了才报 403」），
 * 但后端必须独立再校验一次（前端可控）。
 *
 * ── 契约：四角色命名与后端鉴权角色的映射（single point of change）──────────────
 * 后端（licensing-api §4）用「鉴权角色名」，前端沿用同一套 id，避免两处维护：
 *   - 后端 `admin`   →  前端 `system`   （系统管理员：密钥轮换 / 租户策略 / 账号）
 *   - 后端 `viewer`  →  前端 `risk`     （风控只读者：回执异常 + 审计只读）
 *   - 前端 `ops`     （运营：日常发码与查询，只读 + 发放）
 *   - 前端 `lic_ops` （授权运营：执行废弃 / 重发 / 换机等**高危**操作）
 * 若后端改名，**只改这张表 + `ROLE_META`**，消费方（路由守卫 / 侧边菜单 / RoleGate）零改动。
 *
 * ── 契约：`audit.export` 为何仅授予 `system` ────────────────────────────────
 * 审计日志的**导出**会把全量操作记录（含 actor / ip / 原因明细）落盘为可外传文件，
 * 属高敏感数据外带面。设计上：
 *   - `risk` 与 `system` 都能 `audit.view`（在线只读审计，用于风控核查）；
 *   - 但只有 `system` 额外持有 `audit.export` —— 导出是「数据出境」动作，
 *     收敛到系统管理员单一角色，便于追责与合规审计。
 *   - `ops` / `lic_ops` 既不可 `audit.view` 也不可 `audit.export`（最小权限原则）。
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

/**
 * 本模块所属的「端」——**厂商侧（licensing / 服务端）**。
 *
 * 这里定义的 `Role` / `Action` / `ACTION_MATRIX` / `PAGES` 只描述厂商侧
 * （admin-console / licensing-server）的角色与操作级权限；**网关侧**
 * （daemon / web-console）的权限模型与之**互不映射**（网关侧见 daemon
 * `rbac::PermissionScope`）。两端存在同名 id（如 `device.view` / `account.view`），
 * 但语义不同：厂商侧指「授权绑定的在线设备台账 / 厂商运营账号」，网关侧指
 * 「现场采集设备与点位 / 网关账号」。**禁止**跨端复用同一套权限判定。
 */
export const RBAC_SIDE = 'licensing' as const;

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
    fullLabel: '系统（全部权限）',
    desc: '系统管理员：具备全部权限（发码/废弃/重发/换机/密钥/租户/账号/审计）。',
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
    visibleTo: ['ops', 'lic_ops', 'system'],
    icon: IconFile,
  },
  {
    id: 'code-detail',
    title: '激活码详情',
    group: '授权运营',
    crumb: '授权运营 / 激活码管理',
    visibleTo: ['ops', 'lic_ops', 'system'],
    icon: IconFile,
    detailOnly: true,
  },
  {
    id: 'devices',
    title: '设备管理',
    group: '授权运营',
    crumb: '授权运营 / 设备管理',
    visibleTo: ['ops', 'lic_ops', 'system'],
    icon: IconComputer,
  },
  {
    id: 'device-detail',
    title: '设备详情',
    group: '授权运营',
    crumb: '授权运营 / 设备管理',
    visibleTo: ['ops', 'lic_ops', 'system'],
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
    visibleTo: ['lic_ops', 'system'],
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
    // 系统管理员：厂商侧全部 19 项操作权限（发码 / 废弃 / 重发 / 换机 / 密钥 / 租户 / 账号 / 审计）
    'code.view',
    'code.issue',
    'code.revoke',
    'code.reissue',
    'code.reveal',
    'device.view',
    'device.mark_anomaly',
    'tenant.view',
    'tenant.policy_update',
    'receipt.view',
    'receipt.mark',
    'transfer.view',
    'transfer.process',
    'key.view',
    'key.rotate',
    'audit.view',
    'audit.export',
    'account.view',
    'account.update',
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

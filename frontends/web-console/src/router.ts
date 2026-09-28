/**
 * @file router.ts
 * @module web-console/router
 * @description 路由表（16 页，懒加载）。
 *
 * 与 `admin-console/router.ts` 的差异：客户端控制台**不引入路由级 RBAC 守卫**。
 * 原因（红线 3）：`RoleGate` 只控制**可见性**，授权与权限判定一律在 Rust 侧；
 * 客户端页面本身均可访问（网关本机浏览器，物理可达即视为已认证），
 * 写操作按钮由页面内 `RoleGate` 按角色隐藏。这样避免前端误导性地声称「已鉴权」。
 *
 * 每条路由的 `meta` 携带 `title`（文档标题 + 顶栏面包屑）与 `group`（侧栏分组）。
 *
 * **分组事实源**：`meta.group` 与 `App.vue` 的 `NAV_GROUPS` 必须保持一致（均以
 * 原型 `docs/design/prototype/gateway-v2a-glacier.html` 的 `PAGES[*].group` 为准）：
 * 总览 → 数据接入 → 数据分发 → 授权与安全 → 运维。
 * 侧栏渲染读 `NAV_GROUPS`，本表的 `group` 用于文档/面包屑归属与后续按组派生能力，
 * 二者不同步即产生第二套分组事实源，改动时务必同步两边。
 */
import { createRouter, createWebHashHistory, type RouteRecordRaw } from 'vue-router';
import { getStoredToken } from './api/client';

/** 路由表（懒加载各页面组件）。 */
const routes: readonly RouteRecordRaw[] = [
  { path: '/', redirect: '/overview' },

  // ---------- 登录（未登录时守卫会把所有页面导到这里） ----------
  {
    path: '/login',
    name: 'login',
    component: () => import('@/pages/LoginPage.vue'),
    meta: { title: '登录', group: '' },
  },

  // ---------- 分组「总览」 ----------
  {
    path: '/overview',
    name: 'overview',
    component: () => import('@/pages/OverviewPage.vue'),
    meta: { title: '总览', group: '总览' },
  },
  {
    path: '/monitor',
    name: 'monitor',
    component: () => import('@/pages/MonitorPage.vue'),
    meta: { title: '实时监控', group: '总览' },
  },
  {
    path: '/alarms',
    name: 'alarms',
    component: () => import('@/pages/AlarmsPage.vue'),
    meta: { title: '告警中心', group: '总览' },
  },

  // ---------- 分组「数据接入」 ----------
  {
    path: '/devices',
    name: 'devices',
    component: () => import('@/pages/DevicesPage.vue'),
    meta: { title: '设备列表', group: '数据接入' },
  },
  {
    path: '/device-new',
    name: 'device-new',
    component: () => import('@/pages/DeviceNewPage.vue'),
    meta: { title: '新增设备', group: '数据接入' },
  },
  {
    path: '/points',
    name: 'points',
    component: () => import('@/pages/PointsPage.vue'),
    meta: { title: '点位与映射', group: '数据接入' },
  },
  {
    path: '/live',
    name: 'live',
    component: () => import('@/pages/LivePage.vue'),
    meta: { title: '实时数据', group: '数据接入' },
  },

  // ---------- 分组「数据分发」 ----------
  {
    path: '/northbound',
    name: 'northbound',
    component: () => import('@/pages/NorthboundPage.vue'),
    meta: { title: '北向转发', group: '数据分发' },
  },
  {
    path: '/rules',
    name: 'rules',
    component: () => import('@/pages/RulesPage.vue'),
    meta: { title: '转发规则', group: '数据分发' },
  },

  // ---------- 分组「授权与安全」 ----------
  {
    path: '/license',
    name: 'license',
    component: () => import('@/pages/LicensePage.vue'),
    meta: { title: '授权与激活', group: '授权与安全' },
  },
  {
    path: '/audit',
    name: 'audit',
    component: () => import('@/pages/AuditPage.vue'),
    meta: { title: '日志与审计', group: '授权与安全' },
  },
  {
    path: '/accounts',
    name: 'accounts',
    component: () => import('@/pages/AccountsPage.vue'),
    meta: { title: '账号与角色', group: '授权与安全' },
  },

  // ---------- 分组「运维」 ----------
  {
    path: '/update',
    name: 'update',
    component: () => import('@/pages/UpdatePage.vue'),
    meta: { title: '系统更新', group: '运维' },
  },
  {
    path: '/startup',
    name: 'startup',
    component: () => import('@/pages/StartupPage.vue'),
    meta: { title: '启动与自启', group: '运维' },
  },
  {
    path: '/diagnose',
    name: 'diagnose',
    component: () => import('@/pages/DiagnosePage.vue'),
    meta: { title: '诊断与自检', group: '运维' },
  },
  {
    path: '/backup',
    name: 'backup',
    component: () => import('@/pages/BackupPage.vue'),
    meta: { title: '备份与恢复', group: '运维' },
  },
  {
    path: '/settings',
    name: 'settings',
    component: () => import('@/pages/SettingsPage.vue'),
    meta: { title: '系统设置', group: '运维' },
  },

  { path: '/:pathMatch(.*)*', redirect: '/overview' },
];

/** 路由实例（hash 模式：网关本机 / tauri WebView 部署均无需服务端 rewrite）。 */
export const router = createRouter({
  history: createWebHashHistory(),
  routes: [...routes],
});

/**
 * 全局守卫（登录拦截）。
 *
 * · 无 token：访问任何页面 → `/login`（携带 redirect 便于登录后回跳）；
 * · 已有 token：访问 `/login` → 直接回总览。
 */
router.beforeEach((to) => {
  const authed = Boolean(getStoredToken());
  if (!authed && to.name !== 'login') {
    return { name: 'login', query: { redirect: to.fullPath } };
  }
  if (authed && to.name === 'login') {
    return { name: 'overview' };
  }
  return true;
});

/** 同步文档标题，便于多标签场景辨识。 */
router.afterEach((to) => {
  const title = (to.meta.title as string | undefined) ?? '';
  document.title = title ? `${title} · 数据网关控制台` : '数据网关控制台';
});

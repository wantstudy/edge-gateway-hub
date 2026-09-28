/**
 * @file router.ts
 * @module admin-console/router
 * @description 路由表 + **路由级 RBAC 守卫**。
 *
 * 守卫策略（对应验收「权限矩阵真实生效」）：
 *  - 未登录 → 强制跳 `/login`；
 *  - 已登录但角色无该页可见性 → 不报 403，而是**重定向到该角色的首个可见页面**，
 *    这样用户不会「点进去才看到拒绝」，也不会卡在空白页；
 *  - 侧边导航在同一套 `canSeePage()` 上过滤，菜单与路由守卫共用同一矩阵，保证不漂移。
 */
import { createRouter, createWebHashHistory, type RouteRecordRaw } from 'vue-router';
import { canSeePage, firstAllowedPage, type PageId } from '@ui-kit';
import { session } from './store/session';

/** 路由表（懒加载各页面组件）。 */
const routes: readonly RouteRecordRaw[] = [
  { path: '/', redirect: '/overview' },
  {
    path: '/login',
    name: 'login',
    component: () => import('./pages/LoginPage.vue'),
    meta: { title: '登录' },
  },
  {
    path: '/overview',
    name: 'overview',
    component: () => import('./pages/OverviewPage.vue'),
    meta: { title: '总览', rbPage: 'overview' },
  },
  {
    path: '/codes',
    name: 'codes',
    component: () => import('./pages/CodesPage.vue'),
    meta: { title: '激活码管理', rbPage: 'codes' },
  },
  {
    path: '/codes/:id',
    name: 'code-detail',
    component: () => import('./pages/CodeDetailPage.vue'),
    meta: { title: '激活码详情', rbPage: 'code-detail' },
  },
  {
    path: '/devices',
    name: 'devices',
    component: () => import('./pages/DevicesPage.vue'),
    meta: { title: '设备管理', rbPage: 'devices' },
  },
  {
    path: '/devices/:id',
    name: 'device-detail',
    component: () => import('./pages/DeviceDetailPage.vue'),
    meta: { title: '设备详情', rbPage: 'device-detail' },
  },
  {
    path: '/tenants',
    name: 'tenants',
    component: () => import('./pages/TenantsPage.vue'),
    meta: { title: '租户与策略', rbPage: 'tenants' },
  },
  {
    path: '/receipts',
    name: 'receipts',
    component: () => import('./pages/ReceiptsPage.vue'),
    meta: { title: '回执与异常', rbPage: 'receipts' },
  },
  {
    path: '/transfers',
    name: 'transfers',
    component: () => import('./pages/TransfersPage.vue'),
    meta: { title: '换机工单', rbPage: 'transfers' },
  },
  {
    path: '/keys',
    name: 'keys',
    component: () => import('./pages/KeysPage.vue'),
    meta: { title: '签名密钥管理', rbPage: 'keys' },
  },
  {
    path: '/audit',
    name: 'audit',
    component: () => import('./pages/AuditPage.vue'),
    meta: { title: '审计日志', rbPage: 'audit' },
  },
  {
    path: '/accounts',
    name: 'accounts',
    component: () => import('./pages/AccountsPage.vue'),
    meta: { title: '账号与角色', rbPage: 'accounts' },
  },
  {
    path: '/updates',
    name: 'updates',
    component: () => import('./pages/UpdatesPage.vue'),
    meta: { title: '系统更新', rbPage: 'updates' },
  },
  { path: '/:pathMatch(.*)*', redirect: '/overview' },
];

/** 路由实例（hash 模式：后台内网部署无需服务端 rewrite 配置）。 */
export const router = createRouter({
  history: createWebHashHistory(),
  routes: [...routes],
});

/**
 * 全局前置守卫：登录态 + 页面级 RBAC。
 */
router.beforeEach((to) => {
  const isLogin = to.name === 'login';

  // 未登录：只允许停在登录页
  if (!session.state.loggedIn) {
    return isLogin ? true : { name: 'login' };
  }

  // 已登录访问登录页：直接回到首个可见页面
  if (isLogin) {
    return { name: firstAllowedPage(session.state.role) };
  }

  // 页面级 RBAC：无权限则重定向到首个可见页面（而非 403 或白屏）
  const page = to.meta.rbPage as PageId | undefined;
  if (page && !canSeePage(session.state.role, page)) {
    return { name: firstAllowedPage(session.state.role) };
  }

  return true;
});

/** 同步文档标题，便于多标签场景辨识。 */
router.afterEach((to) => {
  const title = (to.meta.title as string | undefined) ?? '';
  document.title = title ? `${title} · IoT-DAQ 授权管理后台` : 'IoT-DAQ 授权管理后台';
});

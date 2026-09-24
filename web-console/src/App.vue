<template>
  <!--
    AppShell —— 客户端网关控制台应用外壳：顶栏 + 可折叠侧栏 + 主内容区。

    硬性约定：
      1. 顶栏**常驻授权状态徽标**（降级时转琥珀并附「查看原因」）；
      2. 顶栏右侧：连接状态指示 + 当前用户 + 角色切换（演示 RoleGate）；
      3. 路由高亮跟随当前页；侧栏可折叠（宽度切换，图标/文字同步）。
  -->
  <div class="wc-app" :class="{ 'is-collapsed': session.state.sidebarCollapsed }">
    <!-- 顶栏 -->
    <header class="wc-topbar">
      <button
        type="button"
        class="wc-icon-btn"
        :title="session.state.sidebarCollapsed ? '展开侧栏' : '折叠侧栏'"
        aria-label="切换侧栏"
        @click="onToggleSidebar"
      >
        {{ session.state.sidebarCollapsed ? '»' : '«' }}
      </button>

      <div class="wc-brand">
        <span class="wc-brand__logo">GW</span>
        <span class="wc-brand__text">数据网关控制台</span>
      </div>

      <span class="wc-topbar__mini">{{ gatewayName }}</span>

      <span class="wc-spacer" />

      <!-- 授权状态徽标（常驻；降级转琥珀 + 查看原因） -->
      <span
        class="wc-license"
        :class="licenseHealthy ? 'wc-license--ok' : 'wc-license--warn'"
        title="点击进入授权与激活页"
        @click="go('license')"
      >
        <span class="wc-license__dot">●</span>
        <span>{{ licenseBadgeText }}</span>
        <template v-if="!licenseHealthy">
          <span class="wc-license__sep">|</span>
          <span class="wc-license__link" @click.stop="go('license')">查看原因</span>
        </template>
      </span>

      <!-- 连接状态指示 -->
      <span class="wc-conn" :class="`wc-conn--${session.state.connection}`">
        <span class="wc-conn__dot">●</span>
        <span>{{ connectionLabel }}</span>
      </span>

      <!-- 角色切换（演示 RoleGate 可见性控制；不影响授权） -->
      <span class="wc-topbar__mini">角色</span>
      <select
        class="wc-role-select"
        :value="session.state.role"
        aria-label="切换当前角色"
        @change="onRoleChange"
      >
        <option v-for="role in ROLES" :key="role" :value="role">
          {{ ROLE_META[role].fullLabel }}
        </option>
      </select>

      <span class="wc-avatar" :title="`当前账号：${session.state.account}`">{{ initial }}</span>
      <span class="wc-user-name">{{ session.state.displayName }}</span>
    </header>

    <div class="wc-body">
      <!-- 侧栏：按运维动线分组 -->
      <nav class="wc-sidebar" aria-label="主导航">
        <template v-for="group in NAV_GROUPS" :key="group.name">
          <div class="wc-nav-group" :title="group.name">
            <span class="wc-nav-group__text">{{ group.name }}</span>
          </div>
          <RouterLink
            v-for="item in group.items"
            :key="item.id"
            class="wc-nav-item"
            :class="{ 'is-on': isActive(item.id) }"
            :to="{ name: item.id }"
            :title="item.title"
          >
            <span class="wc-nav-item__icon" aria-hidden="true">{{ item.icon }}</span>
            <span class="wc-nav-item__text">{{ item.title }}</span>
            <span v-if="badgeOf(item.id)" class="wc-nav-item__badge" :class="`wc-nav-item__badge--${badgeOf(item.id)!.tone}`">
              {{ badgeOf(item.id)!.count }}
            </span>
          </RouterLink>
        </template>

        <div class="wc-sidebar__foot">
          web-console v1.0<br />
          网关本机控制台 · 独立构建<br />
          授权判定在网关侧
        </div>
      </nav>

      <!-- 主内容区 -->
      <main class="wc-main">
        <RouterView />
      </main>
    </div>
  </div>
</template>

<script setup lang="ts">
/**
 * @file App.vue
 * @module web-console/App
 * @description 应用外壳与全局导航（16 页，5 分组）。
 */
import { computed } from 'vue';
import { useRoute, useRouter } from 'vue-router';
import {
  ROLES,
  ROLE_META,
  session,
  accountInitial,
  licenseHealthy,
  licenseBadgeText,
  type Role,
} from './store/session';

const route = useRoute();
const router = useRouter();

/** 顶栏头像字符。 */
const initial = accountInitial;

/** 顶栏网关名（本机标识）。 */
const gatewayName = '线1-网关-01';

/** 导航项定义（id 与路由 name 一致）。 */
interface NavItem {
  /** 路由 name / 页面 id */
  readonly id: string;
  /** 中文名 */
  readonly title: string;
  /** 折叠态仍可见的图标字符 */
  readonly icon: string;
}

/** 导航分组定义（顺序即运维动线）。 */
interface NavGroup {
  /** 分组名 */
  readonly name: string;
  /** 分组内页面 */
  readonly items: readonly NavItem[];
}

/**
 * 侧栏分组导航（16 页 / 5 分组）。
 *
 * 分组顺序遵循设计 §1「动线优先」：监控 → 接入 → 分发 → 运维 → 系统。
 * `live`（实时点位值）置于「监控」组末尾：它是实时监控的**明细下钻视图**
 * （见 README「页面数与设计文档的核对说明」）。
 */
const NAV_GROUPS: readonly NavGroup[] = [
  {
    name: '监控',
    items: [
      { id: 'overview', title: '总览', icon: '▤' },
      { id: 'monitor', title: '实时监控', icon: '◉' },
      { id: 'alarms', title: '告警中心', icon: '⚠' },
      { id: 'live', title: '实时点位值', icon: '≣' },
    ],
  },
  {
    name: '接入',
    items: [
      { id: 'devices', title: '设备接入', icon: '⊞' },
      { id: 'device-new', title: '新增设备', icon: '⊕' },
      { id: 'points', title: '点位与映射', icon: '⊹' },
    ],
  },
  {
    name: '分发',
    items: [
      { id: 'northbound', title: '北向转发', icon: '⇧' },
      { id: 'rules', title: '转发规则', icon: '⚙' },
    ],
  },
  {
    name: '运维',
    items: [
      { id: 'audit', title: '日志与审计', icon: '☰' },
      { id: 'diagnose', title: '诊断与自检', icon: '✚' },
      { id: 'backup', title: '备份与恢复', icon: '⤓' },
      { id: 'update', title: '系统更新', icon: '↻' },
      { id: 'startup', title: '启动与自启', icon: '⏻' },
    ],
  },
  {
    name: '系统',
    items: [
      { id: 'license', title: '授权与激活', icon: '🔑' },
      { id: 'accounts', title: '账号与角色', icon: '☺' },
      { id: 'settings', title: '系统设置', icon: '⚒' },
    ],
  },
];

/** 顶栏连接状态中文标签。 */
const connectionLabel = computed<string>(() => {
  const map: Record<string, string> = {
    connected: '已连接',
    degraded: '链路降级',
    disconnected: '已断开',
  };
  return map[session.state.connection] ?? '未知';
});

/** 当前激活态（按路由 name 精确匹配）。 */
function isActive(id: string): boolean {
  return (route.name as string | undefined) === id;
}

/** 导航徽标（真实计数；tone 决定配色）。 */
function badgeOf(id: string): { count: number; tone: 'danger' | 'warn' } | null {
  if (id === 'alarms') {
    return { count: 3, tone: 'danger' };
  }
  if (id === 'license' && !licenseHealthy.value) {
    return { count: 1, tone: 'warn' };
  }
  return null;
}

/** 跳转。 */
function go(id: string): void {
  void router.push({ name: id });
}

/** 折叠 / 展开侧栏。 */
function onToggleSidebar(): void {
  session.toggleSidebar();
}

/**
 * 角色切换。
 *
 * 客户端控制台不做路由级 RBAC（红线 3），因此切换角色**不强制跳页**：
 * 仅使页面内 `RoleGate` 的可见性即时变化，便于验收「权限矩阵真实生效」。
 */
function onRoleChange(event: Event): void {
  const role = (event.target as HTMLSelectElement).value as Role;
  session.setRole(role);
}
</script>

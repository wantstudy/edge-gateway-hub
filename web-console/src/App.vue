<template>
  <!--
    AppShell —— 客户端网关控制台应用外壳：顶栏 + 可折叠侧栏 + 主内容区。

    硬性约定：
      1. 顶栏**常驻授权状态徽标**（降级时转琥珀并附「查看原因」）+ 租约剩余环；
      2. 顶栏右侧：连接状态指示 + 当前用户 + 角色（real 模式展示后端角色原文）；
      3. 路由高亮跟随当前页；侧栏可折叠（宽度切换，图标/文字同步）；
      4. /login 路由（仅 real 模式可达）走独立全屏布局，不渲染本外壳。

    视觉权威：docs/design/prototype/gateway-v2a-glacier.html（方案 A · 冰川）。
    氛围层 .uik-atmo 由 ui-kit 提供，侧栏深海军蓝与顶栏毛玻璃取自原型 :77-105。
  -->
  <RouterView v-if="isLoginRoute" />
  <div v-else class="wc-app" :class="{ 'is-collapsed': session.state.sidebarCollapsed }">
    <!-- 氛围层：原型 .atmo（:59-71），由 ui-kit 提供 .uik-atmo（固定定位 + 不拦截交互） -->
    <div class="uik-atmo" aria-hidden="true" />

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
        <!-- 品牌标识：透明底 PNG（web-console/public/logo.png，由设计原图抠白底生成） -->
        <img class="wc-brand__logo" src="/logo.png" alt="IoT-DAQ" />
        <span class="wc-brand__text">数据网关控制台</span>
      </div>

      <span class="wc-topbar__mini">{{ gatewayName }}</span>

      <span class="wc-spacer" />

      <!-- 授权状态徽标（常驻；降级转琥珀 + 查看原因） -->
      <span
        class="wc-license"
        :class="licenseHealthy ? 'wc-license--ok' : 'wc-license--warn'"
        @click="go('license')"
      >
        <span class="wc-license__dot">●</span>
        <span>{{ licenseBadgeText }}</span>
        <template v-if="!licenseHealthy">
          <span class="wc-license__sep">|</span>
          <span class="wc-license__link" @click.stop="go('license')">查看原因</span>
        </template>
      </span>

      <!-- 记忆点：租约剩余环（原型 :3394） -->
      <span
        class="wc-lic-ring"
        role="img"
        :aria-label="`租约有效期剩余 ${licenseRemainingText}`"
        @click="go('license')"
      >
        <svg width="34" height="34" viewBox="0 0 34 34" aria-hidden="true">
          <circle class="wc-lic-ring__track" cx="17" cy="17" r="14" />
          <circle
            class="wc-lic-ring__arc"
            :class="{ 'wc-lic-ring__arc--warn': !licenseHealthy }"
            cx="17"
            cy="17"
            r="14"
            stroke-dasharray="88"
            :stroke-dashoffset="licRingOffset"
          />
        </svg>
        <span class="wc-lic-ring__lbl" :class="{ 'wc-lic-ring__lbl--warn': !licenseHealthy }">
          {{ licRingLabel }}
        </span>
      </span>

      <!-- 主题切换（明暗双主题；状态持久化在 localStorage，缺省回退系统偏好） -->
      <button
        type="button"
        class="wc-icon-btn wc-theme-btn"
        :title="themeMode === 'dark' ? '切换为浅色主题' : '切换为深色主题'"
        :aria-label="themeMode === 'dark' ? '切换为浅色主题' : '切换为深色主题'"
        :data-testid="themeMode === 'dark' ? 'theme-toggle-to-light' : 'theme-toggle-to-dark'"
        @click="toggleThemeMode"
      >
        {{ themeMode === 'dark' ? '☀' : '☾' }}
      </button>

      <!-- 连接状态指示 -->
      <span class="wc-conn" :class="`wc-conn--${session.state.connection}`">
        <span class="wc-conn__dot">●</span>
        <span>{{ connectionLabel }}</span>
      </span>

      <!-- 角色来自登录接口（后端角色原文），并附退出登录；前端不提供角色切换器 -->
      <span class="wc-topbar__mini">角色</span>
      <span class="wc-role-badge">
        {{ session.state.backendRole || '—' }}
      </span>
      <button type="button" class="wc-icon-btn" title="退出登录" aria-label="退出登录" @click="onLogout">
        ⏻
      </button>

      <span class="wc-avatar">{{ initial }}</span>
      <span class="wc-user-name">{{ session.state.displayName || session.state.backendRole || '未登录' }}</span>
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
          v{{ appVersion }} · 网关本机控制台
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
import { computed, onBeforeUnmount, onMounted, ref, watch } from 'vue';
import { useRoute, useRouter } from 'vue-router';
import {
  session,
  accountInitial,
  licenseHealthy,
  licenseBadgeText,
} from './store/session';
import { repo, dataVersion } from './api/repo';
import { themeMode, toggleThemeMode } from '@ui-kit/theme';

const route = useRoute();
const router = useRouter();

/** 顶栏头像字符。 */
const initial = accountInitial;

/** 侧栏页脚版本号（随 dataVersion 响应式刷新；挂载早于 preload 时显示诚实空值）。 */
const appVersion = ref<string>(repo.getGateway().version);
watch(dataVersion, () => {
  appVersion.value = repo.getGateway().version;
});

/** 当前是否为登录页（登录页不渲染应用外壳）。 */
const isLoginRoute = computed<boolean>(() => route.name === 'login');

/** 顶栏网关名（real 取 GET /api/overview 的 name，随 dataVersion 刷新；不再写死演示名）。 */
const gatewayName = ref<string>(repo.getGateway().name);
watch(dataVersion, () => {
  gatewayName.value = repo.getGateway().name;
});

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
 * 分组与命名一律对齐原型 `PAGES[*].group`（:1385 起），即：
 *   总览 → 数据接入 → 数据分发 → 授权与安全 → 运维。
 * 与既有实现的差异（本次对齐点）：
 *   · 「监控 / 接入 / 分发 / 运维 / 系统」五组重排为原型五组；
 *   · `live` 由「监控」组挪入「数据接入」（原型 :1666，命名「实时数据」）；
 *   · `devices` 由「设备接入」改名「设备列表」（原型 :1511）；
 *   · `audit` / `accounts` 由「运维 / 系统」并入「授权与安全」（原型 :1833 / :1863）；
 *   · `settings` 由「系统」并入「运维」（原型 :2137）。
 */
const NAV_GROUPS: readonly NavGroup[] = [
  {
    name: '总览',
    items: [
      { id: 'overview', title: '总览', icon: '▤' },
      { id: 'monitor', title: '实时监控', icon: '◉' },
      { id: 'alarms', title: '告警中心', icon: '⚠' },
    ],
  },
  {
    name: '数据接入',
    items: [
      { id: 'devices', title: '设备列表', icon: '⊞' },
      { id: 'device-new', title: '新增设备', icon: '⊕' },
      { id: 'points', title: '点位与映射', icon: '⊹' },
      { id: 'live', title: '实时数据', icon: '≣' },
    ],
  },
  {
    name: '数据分发',
    items: [
      { id: 'northbound', title: '北向转发', icon: '⇧' },
      { id: 'rules', title: '转发规则', icon: '⚙' },
    ],
  },
  {
    name: '授权与安全',
    items: [
      { id: 'license', title: '授权与激活', icon: '🔑' },
      { id: 'audit', title: '日志与审计', icon: '☰' },
      { id: 'accounts', title: '账号与角色', icon: '☺' },
    ],
  },
  {
    name: '运维',
    items: [
      { id: 'update', title: '系统更新', icon: '↻' },
      { id: 'startup', title: '启动与自启', icon: '⏻' },
      { id: 'diagnose', title: '诊断与自检', icon: '✚' },
      { id: 'backup', title: '备份与恢复', icon: '⤓' },
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

// ---------------------------------------------------------------------------
// 顶栏指示：租约剩余环 / 告警徽标
// ---------------------------------------------------------------------------

/** 租约总天数基准（环的满圈口径；原型 :3394 以 365d 为满圈）。 */
const LEASE_TOTAL_DAYS = 365;

/** 租约环周长（r=14 → 2πr ≈ 88）。 */
const RING_CIRCUMFERENCE = 88;

/** 租约剩余天数（取自展示用授权快照；判定仍在 Rust 侧）。 */
const licenseRemainingDays = computed<number>(() => Math.max(0, session.state.license.remainingDays));

/** 租约剩余文案（环的悬浮提示）。 */
const licenseRemainingText = computed<string>(() =>
  session.state.license.remainingText || `${licenseRemainingDays.value} 天`,
);

/** 环内短标签（原型 :3394：`365d`）。 */
const licRingLabel = computed<string>(() => `${licenseRemainingDays.value}d`);

/** 环的 dashoffset（剩余占比 → 缺口）。 */
const licRingOffset = computed<number>(() => {
  const ratio = Math.min(1, licenseRemainingDays.value / LEASE_TOTAL_DAYS);
  return Number((RING_CIRCUMFERENCE * (1 - ratio)).toFixed(2));
});

/**
 * 未处置告警数（真实数据源计数；无数据 → 徽标不显示，绝不硬编码）。
 */
const openAlarmCount = ref<number>(0);

/** 告警计数轮询句柄（repo 缓存非响应式，故按低频轮询对齐处置结果）。 */
let alarmTimer: ReturnType<typeof setInterval> | null = null;

/** 重新统计未处置告警数（`resolved` 之外均视为未确认）。 */
function refreshAlarmCount(): void {
  openAlarmCount.value = repo.allAlarms().filter((a) => a.state !== 'resolved').length;
}

onMounted(() => {
  refreshAlarmCount();
  alarmTimer = setInterval(refreshAlarmCount, 10_000);
});

onBeforeUnmount(() => {
  if (alarmTimer !== null) {
    clearInterval(alarmTimer);
    alarmTimer = null;
  }
});

/** 导航徽标（真实计数；tone 决定配色）。 */
function badgeOf(id: string): { count: number; tone: 'danger' | 'warn' } | null {
  if (id === 'alarms' && openAlarmCount.value > 0) {
    return { count: openAlarmCount.value, tone: 'danger' };
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
 * 退出登录（real 模式顶栏）：清空会话与本地 token，回到登录页。
 */
function onLogout(): void {
  session.logout();
  window.location.hash = '#/login';
}
</script>

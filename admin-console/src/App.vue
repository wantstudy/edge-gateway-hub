<template>
  <!--
    AppShell —— 应用外壳：顶栏 + 侧边导航 + 内容区。
    导航项由 `canSeePage(role, page)` 过滤，与路由守卫共用同一权限矩阵（保证菜单与路由不漂移）。
  -->
  <div class="ac-app">
    <!-- 顶栏：应用标识 | 角色切换 | 全局状态胶囊 | 账号 -->
    <header class="ac-topbar">
      <div class="ac-brand">
        <span class="ac-brand__logo">LIC</span>
        <span>IoT-DAQ 授权管理后台</span>
      </div>
      <span class="ac-topbar__mini">厂商侧 · 运营与售后</span>
      <span class="ac-spacer" />

      <!-- 角色切换：切换后菜单与按钮可用性立即变化（验收重点） -->
      <span class="ac-topbar__mini">角色</span>
      <select
        class="ac-role-select"
        :value="session.state.role"
        aria-label="切换当前角色"
        @change="onRoleChange"
      >
        <option v-for="role in ROLES" :key="role" :value="role">
          {{ ROLE_META[role].fullLabel }}
        </option>
      </select>

      <span class="ac-pill ac-pill--warn" title="点击进入待处理总览" @click="go('overview')">
        ⚠ 待处理 {{ session.state.pendingCount }} 项
      </span>
      <span class="ac-pill ac-pill--ok">
        ● 回执健康 {{ session.state.receiptOk }}/{{ session.state.receiptTotal }}
      </span>

      <!--
        双人复核开关（全局策略）
        ─────────────────────────────────────────────────────────────────
        【此开关位置与形态为**设计文档未明确定位**时的合理补充，非照抄】：
        设计文档只要求「高危操作在双人复核开启时需第二审批人」，但未规定
        该策略的开关放在哪、由谁控制。这里把它上提到**顶栏全局开关**，理由：
          · 它是**全局策略**而非单页状态 → 放在 AppShell 顶栏，跨页一致；
          · 运维需要「临时收紧 / 放宽」的直观入口 → 一次点击即可全局生效。
        【实现性质：前端模拟】当前值只存在前端 session（`session.setDualApproval`），
        并未落库。真实系统中它**必须**成为服务端租户策略
        （licensing-api 的 `/admin/policy`，本原型未接线），因为：
          · 复核人数是安全策略，不能由前端自证；
          · 刷新 / 换端后必须保持一致 → 需服务端持久化。
        开启后，各高危操作弹窗（DangerConfirmModal）会据 `requireSecondApprover`
        追加「第二审批人」必填项。
      -->
      <label class="ac-dual" title="开启后，废弃 / 重发等高危操作需第二位管理员复核">
        <input
          type="checkbox"
          :checked="session.state.dualApproval"
          @change="onDualApprovalChange"
        />
        <span>双人复核{{ session.state.dualApproval ? '：开' : '：关' }}</span>
      </label>

      <span class="ac-avatar" :title="`当前账号：${session.state.account}`">{{ initial }}</span>
      <button type="button" class="ac-btn ac-btn--sm" @click="onLogout">退出</button>
    </header>

    <div class="ac-body">
      <!-- 侧边导航：按运维动线分组 -->
      <nav class="ac-sidebar" aria-label="主导航">
        <template v-for="group in visibleGroups" :key="group">
          <div class="ac-nav-group">{{ group }}</div>
          <RouterLink
            v-for="page in pagesOfGroup(group)"
            :key="page.id"
            class="ac-nav-item"
            :class="{ 'is-on': isActive(page.id) }"
            :to="pathOf(page.id)"
          >
            <span>{{ page.title }}</span>
            <!-- 徽标只出现在有真实待处理数的页面 -->
            <span v-if="badgeOf(page.id)" class="ac-nav-item__badge ac-nav-item__badge--warn">
              {{ badgeOf(page.id) }}
            </span>
          </RouterLink>
        </template>
        <div class="ac-sidebar__foot">
          admin-console v1.0<br />
          独立构建 · 与网关侧隔离<br />
          前端无私钥 · 不直连授权库
        </div>
      </nav>

      <!-- 主内容区 -->
      <main class="ac-main">
        <RouterView />
      </main>
    </div>
  </div>
</template>

<script setup lang="ts">
/**
 * @file App.vue
 * @module admin-console/App
 * @description 应用外壳与全局导航。
 */
import { computed } from 'vue';
import { useRoute, useRouter } from 'vue-router';
import { PAGES, ROLE_META, ROLES, canSeePage, firstAllowedPage, type PageId, type Role } from '@ui-kit';
import { session } from './store/session';
import { accountInitial } from './store/session';

const route = useRoute();
const router = useRouter();

/** 顶栏头像字符。 */
const initial = accountInitial;

/** 当前角色可见且非详情页的页面清单（导航只列主页面）。 */
const visiblePages = computed(() => PAGES.filter((p) => !p.detailOnly && canSeePage(session.state.role, p.id)));

/** 可见分组（按 PAGES 中首次出现顺序）。 */
const visibleGroups = computed<string[]>(() => {
  const groups: string[] = [];
  for (const page of visiblePages.value) {
    if (!groups.includes(page.group)) {
      groups.push(page.group);
    }
  }
  return groups;
});

/** 取某分组下的可见页面。 */
function pagesOfGroup(group: string): typeof PAGES {
  return visiblePages.value.filter((p) => p.group === group);
}

/** 页面 id → 路由路径。 */
function pathOf(id: PageId): string {
  return id === 'code-detail' ? '/codes' : id === 'device-detail' ? '/devices' : `/${id}`;
}

/** 当前激活态：详情页高亮其父列表页。 */
function isActive(id: PageId): boolean {
  const name = route.name as string | undefined;
  if (id === 'codes') {
    return name === 'codes' || name === 'code-detail';
  }
  if (id === 'devices') {
    return name === 'devices' || name === 'device-detail';
  }
  return name === id;
}

/** 导航徽标（真实待处理计数）。 */
function badgeOf(id: PageId): number | null {
  if (id === 'transfers') {
    return 2;
  }
  if (id === 'receipts') {
    return 2;
  }
  return null;
}

/** 跳转。 */
function go(id: PageId): void {
  void router.push({ name: id });
}

/**
 * 角色切换：切换后若当前页对新角色不可见，则跳到该角色首个可见页面，
 * 避免停留在空白页（验收「权限矩阵真实生效」的核心体验）。
 */
function onRoleChange(event: Event): void {
  const role = (event.target as HTMLSelectElement).value as Role;
  session.setRole(role);
  const current = route.meta.rbPage as PageId | undefined;
  if (!current || !canSeePage(role, current)) {
    void router.push({ name: firstAllowedPage(role) });
  }
}

/** 退出登录。 */
function onLogout(): void {
  session.logout();
  void router.push({ name: 'login' });
}

/**
 * 切换双人复核开关。
 * 开启后，废弃 / 重发等高危弹窗会额外要求「第二审批人」，且该字段必填（见 DangerConfirmModal）。
 */
function onDualApprovalChange(event: Event): void {
  session.setDualApproval((event.target as HTMLInputElement).checked);
}
</script>

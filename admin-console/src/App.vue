<template>
  <!--
    AppShell —— 应用外壳：顶栏 + 侧边导航 + 内容区。
    导航项由 `canSeePage(role, page)` 过滤，与路由守卫共用同一权限矩阵（保证菜单与路由不漂移）。
  -->
  <!--
    AppShell 在 `/login` 路由下**让位**：不渲染顶栏 / 侧边导航 / 提示横幅——
    登录必须是独立的整页体验，未登录时既无内嵌表单 / 浮层，也不露出「已登录」
    的控制台外壳。注意：`RouterView` 必须保持挂载（登录页由它渲染），
    因此只隐藏外壳本身（顶栏 / 侧栏 / 提示横幅），不隐藏 `.ac-body` / `.ac-main`。
  -->
  <div class="ac-app" :class="{ 'ac-app--login': isLoginRoute }">
    <template v-if="!isLoginRoute">
      <!-- 顶栏：应用标识 | 角色切换 | 全局状态胶囊 | 账号 -->
      <header class="ac-topbar">
        <div class="ac-brand">
          <img class="ac-brand__logo" src="/logo-white.png" alt="IoT-DAQ" />
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
          ⚠ 待处理 {{ overview.pendingAnomalies }} 项
        </span>
        <span class="ac-pill ac-pill--ok" title="回执正常台数（后端未提供该维度时显示 —）">
          ● 回执健康 {{ overview.receiptOk }}
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

      <!--
        全局提示横幅（real 模式联调专用）：
        数据层（api/repo.ts）在后端端点缺失（诚实空态降级）或写操作失败时推入
        `adminNotices`，此处统一展示、逐条可关闭。mock 模式下恒为空、不渲染。
      -->
      <div v-if="adminNotices.length > 0" class="ac-notices" role="status">
        <div v-for="n in adminNotices" :key="n.id" class="ac-notice" :class="`ac-notice--${n.tone}`">
          <span class="ac-notice__msg">{{ n.message }}</span>
          <button type="button" class="ac-notice__close" aria-label="关闭提示" @click="dismissNotice(n.id)">×</button>
        </div>
      </div>
    </template>

    <div class="ac-body" :class="{ 'ac-body--login': isLoginRoute }">
      <!-- 侧边导航：按运维动线分组（登录路由下不渲染） -->
      <nav v-if="!isLoginRoute" class="ac-sidebar" aria-label="主导航">
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
import { computed, onMounted } from 'vue';
import { useRoute, useRouter } from 'vue-router';
import { PAGES, ROLE_META, ROLES, canSeePage, firstAllowedPage, type PageId, type Role } from '@ui-kit';
import { session } from './store/session';
import { accountInitial } from './store/session';
import { API_MODE } from './api/client';
import { adminNotices, dismissNotice, preloadRealData, repo } from './api/repo';

const route = useRoute();
const router = useRouter();

/** 顶栏头像字符。 */
const initial = accountInitial;

/** 是否停在登录页（登录路由下不渲染控制台外壳，见模板顶部说明）。 */
const isLoginRoute = computed(() => route.name === 'login');

/**
 * real 模式启动即拉真实数据（mock 行为零回归）。
 *
 * 作用不止「首屏有数据」：会话恢复（`session.restore`）只凭 localStorage 里的
 * token 判定已登录，token 是否真的有效必须由后端裁决。启动即发请求 → 失效 /
 * 过期 token 会立刻收到 401，由 client.ts 清会话并跳登录页，**不在已失效的
 * 会话上静默展示一个空控制台**。
 */
onMounted(() => {
  // 仅在「已恢复出会话」时预取：未登录时发请求只会换来一串 401 与「加载失败」横幅，
  // 既噪声又白费流量；登录页那条路径由 LoginPage 登录成功后自己触发 preload。
  if (API_MODE === 'real' && session.state.loggedIn) {
    void preloadRealData();
  }
});

/**
 * 总览真实聚合（顶栏计数唯一来源）。
 *
 * real 模式为 `GET /admin/overview` 的 reactive 快照，未提供的维度为诚实空态 `—`；
 * mock 模式为 mock 聚合。**绝不**在组件内硬编码演示计数。
 */
const overview = computed(() => repo.overview());

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

/** 导航徽标（真实待处理计数；后端未提供 / 为 0 / 诚实空态时**不显示**，绝不编造）。 */
function badgeOf(id: PageId): string | null {
  const ov = overview.value;
  const raw = id === 'transfers' ? ov.pendingTransfers : id === 'receipts' ? ov.pendingAnomalies : null;
  if (raw === null || raw === undefined || raw === '' || raw === '—' || raw === '0') {
    return null;
  }
  return raw;
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

<style scoped>
/* 全局提示横幅（real 模式降级 / 失败提示；本页自用样式，避免污染 ui-kit） */
.ac-notices {
  display: flex;
  flex-direction: column;
  gap: 6px;
  padding: 8px 20px 0;
}
.ac-notice {
  display: flex;
  align-items: flex-start;
  gap: 10px;
  padding: 8px 12px;
  border-radius: 6px;
  font-size: 13px;
  line-height: 1.5;
  border: 1px solid;
}
.ac-notice--warn {
  background: #fff7e8;
  border-color: #ffe4ba;
  color: #874d00;
}
.ac-notice--error {
  background: #ffece8;
  border-color: #fdcaca;
  color: #a02c2c;
}
.ac-notice__msg {
  flex: 1;
}
.ac-notice__close {
  border: none;
  background: transparent;
  color: inherit;
  font-size: 15px;
  line-height: 1;
  cursor: pointer;
  padding: 0 2px;
}
</style>

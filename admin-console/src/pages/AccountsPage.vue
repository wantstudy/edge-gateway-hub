<template>
  <!--
    AccountsPage —— 账号与角色（页面清单第 12 项，仅「系统」角色可见）。
    核心交付：**四角色权限矩阵的真实展示**（页面级 + 操作级）。
    该矩阵不是装饰 —— 它由 `@ui-kit` 导出的 RBAC 常量为唯一数据源，
    与路由守卫 / 菜单过滤 / 按钮门控**同源**，因此展示与实际行为必然一致。
  -->
  <PageHeader
    crumb="系统 / 账号与角色"
    title="账号与角色"
    desc="四角色（运营 / 授权运营 / 风控 / 系统）的页面级 + 操作级权限矩阵。矩阵为唯一数据源，与菜单、路由、按钮门控同源。"
  />

  <div class="ac-content">
    <!-- 角色说明 -->
    <div class="ac-grid ac-grid--2">
      <section v-for="role in ROLES" :key="role" class="ac-card">
        <div class="ac-card__head">
          <h3>{{ ROLE_META[role].label }}</h3>
          <span class="ac-card__sub">{{ ROLE_META[role].fullLabel }}</span>
        </div>
        <div class="ac-card__body">
          <p class="ac-role-desc">{{ ROLE_META[role].desc }}</p>
          <div class="ac-role-ops">
            <span
              v-for="action in ACTION_MATRIX[role]"
              :key="action"
              class="ac-role-chip"
            >
              {{ ACTION_LABEL[action] }}
            </span>
          </div>
          <button type="button" class="ac-btn ac-btn--sm" @click="switchTo(role)">
            切换为该角色查看效果
          </button>
        </div>
      </section>
    </div>

    <!-- 页面级权限矩阵 -->
    <section class="ac-card">
      <div class="ac-card__head">
        <h3>页面级权限矩阵</h3>
        <span class="ac-card__sub">√ 可见并进入 · — 菜单不显示且路由重定向</span>
      </div>
      <div class="ac-table-wrap">
        <table class="ac-matrix-table">
          <thead>
            <tr>
              <th>页面</th>
              <th v-for="role in ROLES" :key="role">{{ ROLE_META[role].label }}</th>
            </tr>
          </thead>
          <tbody>
            <tr v-for="page in mainPages" :key="page.id">
              <td>
                <span class="ac-matrix-page">{{ page.title }}</span>
                <span class="ac-matrix-group">{{ page.group }}</span>
              </td>
              <td v-for="role in ROLES" :key="role">
                <span v-if="canSeePage(role, page.id)" class="ac-matrix__yes">✓</span>
                <span v-else class="ac-matrix__no">—</span>
              </td>
            </tr>
          </tbody>
        </table>
      </div>
    </section>

    <!-- 操作级权限矩阵 -->
    <section class="ac-card">
      <div class="ac-card__head">
        <h3>操作级权限矩阵</h3>
        <span class="ac-card__sub">√ 按钮可用 · — 按钮隐藏或禁用（点击前即生效，不是点了才 403）</span>
      </div>
      <div class="ac-table-wrap">
        <table class="ac-matrix-table">
          <thead>
            <tr>
              <th>操作</th>
              <th v-for="role in ROLES" :key="role">{{ ROLE_META[role].label }}</th>
            </tr>
          </thead>
          <tbody>
            <tr v-for="action in ALL_ACTIONS" :key="action">
              <td>{{ ACTION_LABEL[action] }}</td>
              <td v-for="role in ROLES" :key="role">
                <span v-if="can(role, action)" class="ac-matrix__yes">✓</span>
                <span v-else class="ac-matrix__no">—</span>
              </td>
            </tr>
          </tbody>
        </table>
      </div>
    </section>

    <!-- 管理员账号 -->
    <section class="ac-card">
      <div class="ac-card__head">
        <h3>管理员账号</h3>
        <span class="ac-card__sub">共 {{ users.length }} 个账号</span>
      </div>
      <UiTable :columns="columns" :rows="users" row-key-field="account">
        <template #cell-account="{ row }">
          <span class="ac-mono">{{ row.account }}</span>
        </template>
        <template #cell-role="{ row }">
          {{ roleLabel(row.role) }}
        </template>
        <template #cell-status="{ row }">
          <StatusTag :status="row.status" />
        </template>
        <template #cell-lastLoginAt="{ row }">
          <span class="ac-mono">{{ row.lastLoginAt }}</span>
        </template>
        <template #actions="{ row }">
          <!-- 按钮文案按当前状态取反：启用中 → 「停用」 -->
          <button
            type="button"
            class="ac-btn ac-btn--sm"
            :class="{ 'ac-btn--danger': row.status === 'user_enabled' }"
            @click="toggleUser(row)"
          >
            {{ row.status === 'user_enabled' ? '停用' : '启用' }}
          </button>
        </template>
      </UiTable>
    </section>

    <p class="ac-note">
      <span class="ac-note__icon">ⓘ</span>
      <span>
        前端 RBAC 仅作可见性 / 可用性控制，<b>不承担授权判定</b>；
        服务端仍会独立校验（非管理员调用 <span class="ac-mono">/admin/*</span> 返回 403 ADMIN_ONLY）。
        审计页可见「访客尝试废弃激活码 → 拒绝 403」记录，即双层防护的体现。
      </span>
    </p>
  </div>
</template>

<script setup lang="ts">
/**
 * @file AccountsPage.vue
 * @module admin-console/pages/AccountsPage
 * @description 账号与角色页（含权限矩阵展示）。
 */
import { computed, ref } from 'vue';
import { useRouter } from 'vue-router';
import {
  PageHeader,
  UiTable,
  StatusTag,
  ROLES,
  ROLE_META,
  PAGES,
  ACTION_MATRIX,
  canSeePage,
  can,
  firstAllowedPage,
  type Role,
  type Action,
  type TableColumn,
} from '@ui-kit';
import { repo, DEFAULT_ACTOR } from '../api/repo';
import { session } from '../store/session';

const router = useRouter();

/** 刷新触发器。 */
const reloadKey = ref(0);

/** 账号列表。 */
const users = computed(() => {
  void reloadKey.value;
  return repo.allUsers();
});

/** 主页面（排除详情页，矩阵更清晰）。 */
const mainPages = computed(() => PAGES.filter((p) => !p.detailOnly));

/** 全部操作清单（去重，按 ACTION_MATRIX 中出现顺序）。 */
const ALL_ACTIONS = computed<Action[]>(() => {
  const out: Action[] = [];
  for (const role of ROLES) {
    for (const action of ACTION_MATRIX[role]) {
      if (!out.includes(action)) {
        out.push(action);
      }
    }
  }
  return out;
});

/**
 * 操作中文名映射（矩阵表展示）。
 *
 * ⚠️ 两端隔离：本表（及 `@ui-kit` 的 `Action` / `ACTION_MATRIX`）描述的是
 * **厂商侧（licensing / 服务端）**的操作级权限语义（激活码 / 租户 / 密钥 /
 * 回执 / 换机 / 授权设备台账）。它与**网关侧（daemon / web-console）**的
 * 权限模型**互不映射**（网关侧权限见 daemon `rbac::PermissionScope::Gateway`）。
 * 同名 id（如 `device.view` / `account.view`）两端含义不同，切勿交叉复用。
 */
const ACTION_LABEL: Readonly<Record<Action, string>> = Object.freeze({
  'code.view': '查看激活码',
  'code.issue': '发放激活码',
  'code.revoke': '废弃激活码（高危）',
  'code.reissue': '重发激活码（高危）',
  'code.reveal': '查看激活码明文',
  'device.view': '查看设备',
  'device.mark_anomaly': '标记设备异常',
  'tenant.view': '查看租户',
  'tenant.policy_update': '修改租户策略（高危）',
  'receipt.view': '查看回执异常',
  'receipt.mark': '标记回执异常处置',
  'transfer.view': '查看换机工单',
  'transfer.process': '处理换机工单（高危）',
  'key.view': '查看签名密钥',
  'key.rotate': '轮换签名密钥（高危）',
  'audit.view': '查看审计日志',
  'audit.export': '导出审计日志',
  'account.view': '查看账号',
  'account.update': '修改账号状态',
});

/** 列定义。 */
const columns: readonly TableColumn[] = [
  { key: 'account', label: '账号', mono: true },
  { key: 'name', label: '姓名' },
  { key: 'role', label: '角色' },
  { key: 'status', label: '状态' },
  { key: 'lastLoginAt', label: '最近登录', mono: true },
];

/** 角色 → 中文名。 */
function roleLabel(role: string): string {
  const meta = ROLE_META[role as Role];
  return meta ? meta.fullLabel : role;
}

/** 切换角色并跳到该角色首个可见页面（演示权限矩阵真实生效）。 */
function switchTo(role: Role): void {
  session.setRole(role);
  void router.push({ name: firstAllowedPage(role) });
}

/** 启用 / 停用账号（按钮文案随状态取反）。 */
function toggleUser(row: { account: string; status: string }): void {
  void repo.setUserStatus({ account: row.account, enabled: row.status !== 'user_enabled', actor: DEFAULT_ACTOR });
  reloadKey.value += 1;
}
</script>

<style scoped>
.ac-role-desc {
  margin: 0;
  font-size: var(--fs-caption);
  color: var(--text-2);
  line-height: 1.7;
}
.ac-role-ops {
  display: flex;
  flex-wrap: wrap;
  gap: 6px;
}
.ac-role-chip {
  display: inline-block;
  font-size: 11px;
  padding: 2px 8px;
  border-radius: var(--radius-sm);
  background: var(--brand-subtle);
  color: var(--info-fg);
  border: 1px solid var(--info-border);
}
.ac-table-wrap {
  overflow-x: auto;
}
.ac-matrix-table {
  width: 100%;
  border-collapse: collapse;
  font-size: var(--fs-table);
}
.ac-matrix-table th {
  text-align: center;
  font-weight: 500;
  color: var(--text-2);
  background: #fafbfc;
  padding: 9px 12px;
  border-bottom: 1px solid var(--border);
  font-size: var(--fs-caption);
  white-space: nowrap;
}
.ac-matrix-table th:first-child {
  text-align: left;
}
.ac-matrix-table td {
  text-align: center;
  padding: 8px 12px;
  border-bottom: 1px solid var(--divider);
}
.ac-matrix-table td:first-child {
  text-align: left;
}
.ac-matrix-table tbody tr:hover {
  background: var(--bg-hover);
}
.ac-matrix-page {
  display: block;
}
.ac-matrix-group {
  font-size: 11px;
  color: var(--text-3);
}
</style>

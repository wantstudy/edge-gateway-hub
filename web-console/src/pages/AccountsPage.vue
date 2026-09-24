<!--
  =============================================================================
  AccountsPage —— 账号与角色
  =============================================================================
  交付要点（`docs/design/ui-gateway-console.md` §3.7）：
    · 账号列表**分页**（`UiPager`）—— 条数只有分页条一个口径，不再额外写「共 N 条」；
    · 角色权限矩阵表（哪个角色能看什么 / 能做什么）；
    · 新增账号 / 重置口令（危险操作走二次确认 + 原因必填 + 对象名二次校验）；
    · 授权相关操作以 `RoleGate` 限制角色。
  边界：前端 `RoleGate` 只控制**可见性**，权限判定在网关（Rust）侧。
-->
<template>
  <PageHeader
    crumb="系统 / 账号与角色"
    title="账号与角色"
    desc="账号、角色与权限矩阵。首次登录强制修改初始口令；角色变更与禁用均写入审计日志。"
  >
    <template #actions>
      <RoleGate :allowed="canManageAccounts" mode="disable" deny-text="当前角色无权新增账号" fallback-label="无权新增">
        <button type="button" class="wc-btn wc-btn--primary wc-btn--sm" data-testid="btn-add-account" @click="openCreate">
          新增账号
        </button>
      </RoleGate>
    </template>
  </PageHeader>

  <div class="wc-content">
    <!-- ===== 账号列表（分页；条数只有分页条一个口径）===== -->
    <section class="wc-card">
      <div class="wc-card__head">
        <h3>账号列表</h3>
        <span class="wc-card__sub">角色变更与禁用均写入审计日志</span>
      </div>
      <div class="wc-card__body wc-card__body--flush">
        <UiTable :columns="columns" :rows="pagedUsers" row-key-field="account" data-testid="account-table">
          <template #cell-account="{ row }">
            <span class="wc-mono">{{ row.account }}</span>
          </template>
          <template #cell-role="{ row }">
            <span class="wc-tag" :class="`wc-tag--${roleTone(row.role)}`">{{ roleLabel(row.role) }}</span>
          </template>
          <template #cell-status="{ row }">
            <StatusTag :status="row.status" />
          </template>
          <template #cell-lastLoginAt="{ row }">
            <span class="wc-mono">{{ row.lastLoginAt }}</span>
          </template>
          <template #actions="{ row }">
            <RoleGate :allowed="canManageAccounts" mode="disable" deny-text="当前角色无权编辑账号" fallback-label="编辑">
              <button type="button" class="wc-btn wc-btn--sm" data-testid="btn-row-edit" @click="openEditPassword(row.account)">
                重置口令
              </button>
            </RoleGate>
            <RoleGate :allowed="canManageAccounts" mode="disable" deny-text="当前角色无权变更账号状态" fallback-label="禁用">
              <button
                type="button"
                class="wc-btn wc-btn--sm"
                :class="{ 'wc-btn--danger': row.status === 'user_enabled' }"
                data-testid="btn-row-toggle"
                @click="openToggle(row.account)"
              >
                {{ row.status === 'user_enabled' ? '禁用' : '启用' }}
              </button>
            </RoleGate>
          </template>
        </UiTable>
        <UiPager
          :page="page"
          :total="users.length"
          :page-size="PAGE_SIZE"
          data-testid="account-pager"
          @update:page="page = $event"
        />
      </div>
    </section>

    <!-- ===== 权限矩阵 ===== -->
    <div class="wc-grid wc-grid--2">
      <section class="wc-card">
        <div class="wc-card__head">
          <h3>权限矩阵</h3>
          <span class="wc-card__sub">√ 可见 · — 不可见（按钮不渲染，点击前即生效）</span>
        </div>
        <div class="wc-card__body wc-card__body--flush">
          <table class="wc-matrix">
            <thead>
              <tr>
                <th>能力</th>
                <th v-for="r in ROLES" :key="r" class="wc-matrix__c">{{ ROLE_META[r].label }}</th>
              </tr>
            </thead>
            <tbody>
              <tr v-for="cap in CAPABILITIES" :key="cap.name">
                <td>{{ cap.name }}</td>
                <td v-for="r in ROLES" :key="r" class="wc-matrix__c">
                  <span v-if="cap.allowed.includes(r)" class="wc-matrix__yes">✓</span>
                  <span v-else class="wc-matrix__no">—</span>
                </td>
              </tr>
            </tbody>
          </table>
        </div>
      </section>

      <section class="wc-card">
        <div class="wc-card__head">
          <h3>登录安全</h3>
          <span class="wc-card__sub">策略由网关侧强制执行</span>
        </div>
        <div class="wc-card__body">
          <div class="wc-list">
            <div v-for="item in SECURITY_POLICIES" :key="item.label" class="wc-list__item">
              <div>
                <div class="wc-list__title">{{ item.label }}</div>
                <div class="wc-list__desc">{{ item.desc }}</div>
              </div>
              <div class="wc-list__ops">
                <StatusTag :status="item.enabled ? 'user_enabled' : 'user_disabled'" :text="item.enabled ? '启用' : '关闭'" />
              </div>
            </div>
          </div>
          <p class="wc-note">
            <span class="wc-note__icon" aria-hidden="true">ⓘ</span>
            <span>账号口令使用加盐哈希存储，不可逆；管理员也无法查看原文，只能重置。</span>
          </p>
        </div>
      </section>
    </div>

    <!-- ===== 角色说明 ===== -->
    <section class="wc-card">
      <div class="wc-card__head">
        <h3>角色说明</h3>
        <span class="wc-card__sub">四角色（管理员 / 现场工程师 / 操作员 / 只读）</span>
      </div>
      <div class="wc-card__body">
        <div class="wc-grid wc-grid--4">
          <div v-for="r in ROLES" :key="r" class="wc-role-card" :data-testid="`role-${r}`">
            <div class="wc-role-card__head">
              <span class="wc-tag" :class="`wc-tag--${roleTone(r)}`">{{ ROLE_META[r].label }}</span>
            </div>
            <p class="wc-role-card__desc">{{ ROLE_META[r].desc }}</p>
          </div>
        </div>
        <p class="wc-note">
          <span class="wc-note__icon" aria-hidden="true">ⓘ</span>
          <span>
            前端权限矩阵仅作可见性控制，<b>不承担授权判定</b>；
            网关侧仍会独立校验，非授权调用返回 403。
          </span>
        </p>
      </div>
    </section>
  </div>

  <!-- ===== 新增账号弹窗 ===== -->
  <Teleport to="body">
    <div v-if="createOpen" class="wc-modal__mask" @click.self="createOpen = false">
      <div class="wc-modal" role="dialog" aria-modal="true" aria-label="新增账号">
        <div class="wc-modal__head"><h3 class="wc-modal__title">新增账号</h3></div>
        <div class="wc-modal__body">
          <div class="wc-modal__grid">
            <UiField label="账号" required :error="createError">
              <UiInput v-model="createForm.account" placeholder="如 ops04" data-testid="create-account" />
            </UiField>
            <UiField label="姓名" required>
              <UiInput v-model="createForm.name" placeholder="如 赵运行" data-testid="create-name" />
            </UiField>
            <UiField label="角色" required>
              <UiSelect v-model="createForm.role" :options="roleOptions" data-testid="create-role" />
            </UiField>
            <UiField label="初始口令" required hint="≥10 位，含大小写、数字与符号；首次登录强制修改">
              <UiInput v-model="createForm.password" type="password" placeholder="初始口令" data-testid="create-password" />
            </UiField>
          </div>
        </div>
        <div class="wc-modal__foot">
          <button type="button" class="wc-btn" @click="createOpen = false">取消</button>
          <button type="button" class="wc-btn wc-btn--primary" data-testid="btn-create-submit" @click="submitCreate">创建账号</button>
        </div>
      </div>
    </div>
  </Teleport>

  <!-- ===== 重置口令弹窗 ===== -->
  <Teleport to="body">
    <div v-if="pwdOpen" class="wc-modal__mask" @click.self="pwdOpen = false">
      <div class="wc-modal" role="dialog" aria-modal="true" aria-label="重置口令">
        <div class="wc-modal__head"><h3 class="wc-modal__title">重置口令 · {{ pwdAccount }}</h3></div>
        <div class="wc-modal__body">
          <UiField label="新口令" required hint="≥10 位，含大小写、数字与符号">
            <UiInput v-model="pwdForm.password" type="password" placeholder="新口令" data-testid="pwd-input" />
          </UiField>
          <p class="wc-modal__hint">重置后该账号需用新口令登录；操作写入审计日志。</p>
        </div>
        <div class="wc-modal__foot">
          <button type="button" class="wc-btn" @click="pwdOpen = false">取消</button>
          <button type="button" class="wc-btn wc-btn--primary" :disabled="pwdForm.password.length < 10" data-testid="btn-pwd-submit" @click="submitPassword">
            确认重置
          </button>
        </div>
      </div>
    </div>
  </Teleport>

  <!-- ===== 启用 / 禁用：危险操作二次确认（原因必填 + 对象名二次校验）===== -->
  <DangerConfirmModal
    :open="toggleConfirm.open"
    :title="toggleConfirm.title"
    :impacts="toggleConfirm.impacts"
    :facts="toggleConfirm.facts"
    :reasons="ACCOUNT_REASONS"
    :min-note-length="10"
    :confirm-value="toggleConfirm.account"
    confirm-label="账号二次确认（输入账号名）"
    confirm-placeholder="输入目标账号名"
    :confirm-text="toggleConfirm.confirmText"
    @close="closeToggle"
    @submit="submitToggle"
  />
</template>

<script setup lang="ts">
/**
 * @file AccountsPage.vue
 * @module web-console/pages/AccountsPage
 * @description 账号与角色页（分页列表 + 权限矩阵 + 危险操作二次确认）。
 *
 * 说明：客户端控制台 mock 未提供账号仓库（`mock-data.ts` 无 users 方法），
 * 本页以组件内本地状态演示账号管理交互；如需与其它页面共享账号数据，
 * 需由骨架维护者扩展 `mock-data.ts`（本页不擅自改动共享文件）。
 */
import { computed, reactive, ref } from 'vue';
import {
  PageHeader,
  UiTable,
  UiPager,
  UiField,
  UiInput,
  UiSelect,
  StatusTag,
  RoleGate,
  DangerConfirmModal,
  type TableColumn,
  type SelectOption,
  type DangerFact,
} from '@ui-kit';
import { session, ROLES, ROLE_META, type Role } from '../store/session';

/** 每页条数。 */
const PAGE_SIZE = 10;

/** 账号记录。 */
interface AccountRow {
  /** 账号 */
  account: string;
  /** 姓名 */
  name: string;
  /** 角色 */
  role: Role;
  /** 状态（user_enabled / user_disabled） */
  status: 'user_enabled' | 'user_disabled';
  /** 最后登录 */
  lastLoginAt: string;
}

/** 账号数据集（演示用本地状态）。 */
const users = ref<AccountRow[]>([
  { account: 'admin', name: '系统管理员', role: 'admin', status: 'user_enabled', lastLoginAt: '2026-09-23 13:41' },
  { account: 'eng01', name: '张工', role: 'engineer', status: 'user_enabled', lastLoginAt: '2026-09-23 11:02' },
  { account: 'eng02', name: '王工', role: 'engineer', status: 'user_enabled', lastLoginAt: '2026-09-23 09:20' },
  { account: 'eng03', name: '陈工', role: 'engineer', status: 'user_enabled', lastLoginAt: '2026-09-21 14:05' },
  { account: 'ops01', name: '产线操作员甲', role: 'operator', status: 'user_enabled', lastLoginAt: '2026-09-23 08:00' },
  { account: 'ops02', name: '产线操作员乙', role: 'operator', status: 'user_enabled', lastLoginAt: '2026-09-22 20:10' },
  { account: 'viewer01', name: '李查看', role: 'viewer', status: 'user_enabled', lastLoginAt: '2026-09-22 16:40' },
  { account: 'viewer02', name: '车间看板', role: 'viewer', status: 'user_enabled', lastLoginAt: '2026-09-23 08:00' },
  { account: 'viewer03', name: '质量部', role: 'viewer', status: 'user_enabled', lastLoginAt: '2026-09-22 15:30' },
  { account: 'audit01', name: '内审查看', role: 'viewer', status: 'user_enabled', lastLoginAt: '2026-09-18 16:44' },
  { account: 'temp01', name: '临时账号（调试）', role: 'viewer', status: 'user_disabled', lastLoginAt: '2026-09-10 10:12' },
  { account: 'temp02', name: '临时账号（产线）', role: 'operator', status: 'user_disabled', lastLoginAt: '2026-09-09 09:00' },
]);

/** 当前页（受控）。 */
const page = ref(1);

/** 分页后的行。 */
const pagedUsers = computed<AccountRow[]>(() => {
  const start = (page.value - 1) * PAGE_SIZE;
  return users.value.slice(start, start + PAGE_SIZE);
});

/** 列定义。 */
const columns: readonly TableColumn[] = [
  { key: 'account', label: '账号', mono: true },
  { key: 'name', label: '姓名' },
  { key: 'role', label: '角色' },
  { key: 'status', label: '状态' },
  { key: 'lastLoginAt', label: '最后登录', mono: true },
];

/** 角色下拉选项。 */
const roleOptions: readonly SelectOption[] = ROLES.map((r) => ({ value: r, label: ROLE_META[r].fullLabel }));

/** 角色 → 中文名。 */
function roleLabel(role: string): string {
  const meta = ROLE_META[role as Role];
  return meta ? meta.label : role;
}

/** 角色 → 标签色调。 */
function roleTone(role: string): 'ok' | 'info' | 'warn' | 'unknown' {
  const map: Record<string, 'ok' | 'info' | 'warn' | 'unknown'> = {
    admin: 'info',
    engineer: 'warn',
    operator: 'ok',
    viewer: 'unknown',
  };
  return map[role] ?? 'unknown';
}

/** 当前角色是否可管理账号（仅 admin）。 */
const canManageAccounts = computed<boolean>(() => session.state.role === 'admin');

// ---------------------------------------------------------------------------
// 能力权限矩阵
// ---------------------------------------------------------------------------

/** 能力条目。 */
interface Capability {
  /** 能力名 */
  readonly name: string;
  /** 允许的角色 */
  readonly allowed: readonly Role[];
}

/** 能力矩阵（管理员全量；工程师接入/转发；操作员监控/告警；只读只读）。 */
const CAPABILITIES: readonly Capability[] = [
  { name: '查看总览 / 监控 / 告警', allowed: ['admin', 'engineer', 'operator', 'viewer'] },
  { name: '查看日志与审计', allowed: ['admin', 'engineer', 'viewer'] },
  { name: '修改设备与点位', allowed: ['admin', 'engineer'] },
  { name: '配置北向转发与规则', allowed: ['admin', 'engineer'] },
  { name: '处置告警', allowed: ['admin', 'engineer', 'operator'] },
  { name: '诊断 / 备份恢复', allowed: ['admin', 'engineer'] },
  { name: '系统更新 / 回滚', allowed: ['admin'] },
  { name: '开机自启 / 服务重启', allowed: ['admin'] },
  { name: '授权与激活 / 换机', allowed: ['admin'] },
  { name: '账号与角色管理', allowed: ['admin'] },
];

/** 登录安全策略（只读展示）。 */
const SECURITY_POLICIES: readonly { label: string; desc: string; enabled: boolean }[] = [
  { label: '首次登录强制修改口令', desc: '初始口令仅用于首次登录，未修改前不可进行其它操作。', enabled: true },
  { label: '连续失败锁定', desc: '同一账号连续失败 5 次锁定 15 分钟，并写入审计。', enabled: true },
  { label: '口令强度校验', desc: '≥10 位，含大小写、数字与符号。', enabled: true },
  { label: '会话超时自动登出', desc: '无操作 30 分钟自动登出。', enabled: true },
];

// ---------------------------------------------------------------------------
// 新增账号
// ---------------------------------------------------------------------------

/** 新增弹窗开关。 */
const createOpen = ref(false);

/** 新增表单草稿。 */
const createForm = reactive({ account: '', name: '', role: 'viewer' as Role, password: '' });

/** 新增行内错误。 */
const createError = ref('');

/** 打开新增弹窗（重置草稿）。 */
function openCreate(): void {
  createOpen.value = true;
  createForm.account = '';
  createForm.name = '';
  createForm.role = 'viewer';
  createForm.password = '';
  createError.value = '';
}

/** 提交新增账号。 */
function submitCreate(): void {
  createError.value = '';
  const account = createForm.account.trim();
  if (!account) {
    createError.value = '请输入账号';
    return;
  }
  if (users.value.some((u) => u.account === account)) {
    createError.value = '账号已存在';
    return;
  }
  users.value = [
    ...users.value,
    {
      account,
      name: createForm.name.trim() || account,
      role: createForm.role,
      status: 'user_enabled',
      lastLoginAt: '—',
    },
  ];
  createOpen.value = false;
  page.value = Math.max(1, Math.ceil(users.value.length / PAGE_SIZE));
}

// ---------------------------------------------------------------------------
// 重置口令
// ---------------------------------------------------------------------------

/** 重置口令弹窗开关。 */
const pwdOpen = ref(false);

/** 目标账号。 */
const pwdAccount = ref('');

/** 口令表单草稿。 */
const pwdForm = reactive({ password: '' });

/** 打开重置口令弹窗。 */
function openEditPassword(account: string): void {
  pwdOpen.value = true;
  pwdAccount.value = account;
  pwdForm.password = '';
}

/** 提交重置口令。 */
function submitPassword(): void {
  pwdOpen.value = false;
}

// ---------------------------------------------------------------------------
// 启用 / 禁用：危险操作二次确认
// ---------------------------------------------------------------------------

/** 危险确认弹窗状态。 */
const toggleConfirm = reactive<{
  open: boolean;
  account: string;
  title: string;
  confirmText: string;
  impacts: readonly string[];
  facts: readonly DangerFact[];
}>({
  open: false,
  account: '',
  title: '',
  confirmText: '确认执行',
  impacts: [],
  facts: [],
});

/** 账号状态变更原因枚举。 */
const ACCOUNT_REASONS: readonly string[] = [
  '人员离职',
  '账号安全风险',
  '权限调整',
  '其它（请在补充说明中描述）',
];

/** 打开二次确认（区分启用 / 禁用）。 */
function openToggle(account: string): void {
  const target = users.value.find((u) => u.account === account);
  if (!target) {
    return;
  }
  const disabling = target.status === 'user_enabled';
  toggleConfirm.open = true;
  toggleConfirm.account = account;
  toggleConfirm.title = `${disabling ? '禁用' : '启用'}账号 · ${account}`;
  toggleConfirm.confirmText = disabling ? '确认禁用' : '确认启用';
  toggleConfirm.impacts = disabling
    ? ['该账号将无法登录控制台，进行中的会话被终止。', '已有审计记录保留，可在日志与审计页检索。', '后续如需恢复，可在本页重新启用。']
    : ['该账号将恢复登录能力。', '账号权限仍受其角色约束，不会超出角色范围。'];
  toggleConfirm.facts = [
    { label: '账号', value: target.account },
    { label: '姓名', value: target.name },
    { label: '角色', value: roleLabel(target.role) },
  ];
}

/** 关闭二次确认。 */
function closeToggle(): void {
  toggleConfirm.open = false;
}

/** 提交启用 / 禁用（写入本地状态；真实系统由网关侧执行 + 审计）。 */
function submitToggle(_payload: { reason: string; note: string; tail: string }): void {
  const target = users.value.find((u) => u.account === toggleConfirm.account);
  if (target) {
    users.value = users.value.map((u) =>
      u.account === target.account
        ? { ...u, status: u.status === 'user_enabled' ? 'user_disabled' : 'user_enabled' }
        : u,
    );
  }
  toggleConfirm.open = false;
}
</script>

<style scoped>
/* 权限矩阵表 */
.wc-matrix {
  width: 100%;
  border-collapse: collapse;
  font-size: var(--fs-table);
}
.wc-matrix th,
.wc-matrix td {
  padding: 9px 12px;
  border-bottom: 1px solid var(--divider);
  text-align: left;
}
.wc-matrix th {
  font-size: var(--fs-caption);
  color: var(--text-3);
  font-weight: 600;
  background: var(--bg-app);
  white-space: nowrap;
}
.wc-matrix__c {
  text-align: center;
}
.wc-matrix__yes {
  color: var(--ok-fg);
  font-weight: 600;
}
.wc-matrix__no {
  color: var(--text-3);
}
/* 角色说明卡 */
.wc-role-card {
  border: 1px solid var(--border);
  border-radius: var(--radius-sm);
  padding: 12px;
  display: flex;
  flex-direction: column;
  gap: 8px;
}
.wc-role-card__head {
  display: flex;
  align-items: center;
}
.wc-role-card__desc {
  margin: 0;
  font-size: var(--fs-caption);
  color: var(--text-3);
  line-height: 1.7;
}
/* 弹窗 */
.wc-modal__mask {
  position: fixed;
  inset: 0;
  background: rgba(29, 33, 41, 0.45);
  display: flex;
  align-items: center;
  justify-content: center;
  z-index: 2000;
  padding: 24px;
}
.wc-modal {
  background: #fff;
  border-radius: var(--radius);
  box-shadow: var(--shadow);
  width: 560px;
  max-width: 100%;
  display: flex;
  flex-direction: column;
}
.wc-modal__head {
  padding: 16px 20px;
  border-bottom: 1px solid var(--divider);
}
.wc-modal__title {
  margin: 0;
  font-size: 16px;
  font-weight: 600;
}
.wc-modal__body {
  padding: 20px;
  display: flex;
  flex-direction: column;
  gap: 12px;
}
.wc-modal__grid {
  display: grid;
  grid-template-columns: repeat(2, minmax(0, 1fr));
  gap: 14px 16px;
}
.wc-modal__hint {
  margin: 0;
  font-size: var(--fs-caption);
  color: var(--text-3);
  line-height: 1.6;
}
.wc-modal__foot {
  padding: 14px 20px;
  border-top: 1px solid var(--divider);
  display: flex;
  justify-content: flex-end;
  gap: 8px;
}
</style>

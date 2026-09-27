<template>
  <!--
    AccountsPage —— 账号与角色（页面清单第 12 项，仅「系统」角色可见）。
    核心交付：
      1) **四角色权限矩阵的真实展示**（页面级 + 操作级）——由 `@ui-kit` 的 RBAC
         常量为唯一数据源，与路由守卫 / 菜单过滤 / 按钮门控同源；
      2) **管理员账号可配置**：真实拉取 `GET /admin/accounts`（缺口 #9 修复），
         并支持新增 / 编辑 / 启停 / 删除（危险动作走 DangerConfirmModal 四要素）。
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
        <span class="ac-head-actions">
          <button type="button" class="ac-btn ac-btn--sm ac-btn--primary" @click="openCreate">
            新增账号
          </button>
        </span>
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
          <span class="ac-row-actions">
            <button type="button" class="ac-btn ac-btn--sm" @click="openEdit(row)">编辑</button>
            <!-- 按钮文案按当前状态取反：启用中 → 「停用」 -->
            <button
              type="button"
              class="ac-btn ac-btn--sm"
              :class="{ 'ac-btn--danger': row.status === 'user_enabled' }"
              @click="askToggle(row)"
            >
              {{ row.status === 'user_enabled' ? '停用' : '启用' }}
            </button>
            <button
              type="button"
              class="ac-btn ac-btn--sm ac-btn--danger"
              @click="askDelete(row)"
            >
              删除
            </button>
          </span>
        </template>
      </UiTable>
    </section>

    <p class="ac-note">
      <span class="ac-note__icon">ⓘ</span>
      <span>
        前端 RBAC 仅作可见性 / 可用性控制，<b>不承担授权判定</b>；
        服务端仍会独立校验（非系统角色调用 <span class="ac-mono">/admin/accounts</span> 返回 403 ADMIN_ONLY）。
        账号口令只存 SHA-256 摘要，明文不落盘、不下发。
      </span>
    </p>
  </div>

  <!-- 新增 / 编辑账号弹窗（自包含，草稿仅存在于本页） -->
  <Teleport to="body">
    <div v-if="formOpen" class="ac-modal__mask" @click.self="formOpen = false">
      <div class="ac-modal" role="dialog" aria-modal="true" :aria-label="editing ? '编辑账号' : '新增账号'">
        <h3 class="ac-modal__title">{{ editing ? '编辑账号' : '新增账号' }}</h3>
        <div class="ac-modal__body">
          <UiField label="账号" required hint="登录名，全局唯一">
            <UiInput v-model="form.account" :disabled="!!editing" placeholder="如 wang.gong" />
          </UiField>
          <UiField label="姓名">
            <UiInput v-model="form.name" placeholder="如 王工" />
          </UiField>
          <UiField label="角色" required>
            <UiSelect v-model="form.role" :options="roleOptions" />
          </UiField>
          <UiField
            :label="editing ? '重置口令（留空不改）' : '初始口令'"
            :required="!editing"
            hint="口令即刻摘要，明文不落盘"
          >
            <UiInput v-model="form.password" type="password" placeholder="输入口令" />
          </UiField>
          <p v-if="formError" class="ac-modal__error">{{ formError }}</p>
        </div>
        <div class="ac-modal__foot">
          <button type="button" class="ac-btn ac-btn--sm" @click="formOpen = false">取消</button>
          <button
            type="button"
            class="ac-btn ac-btn--sm ac-btn--primary"
            :disabled="saving"
            @click="saveForm"
          >
            {{ saving ? '保存中…' : '保存' }}
          </button>
        </div>
      </div>
    </div>
  </Teleport>

  <!-- 危险二次确认（四要素：影响清单 / 原因 / 补充说明≥10字 / 对象名全名二次校验） -->
  <DangerConfirmModal
    :open="dangerOpen"
    :title="dangerTitle"
    :impacts="dangerImpacts"
    :facts="dangerFacts"
    :reasons="dangerReasons"
    confirm-mode="full"
    :confirm-value="dangerRow?.account ?? ''"
    :confirm-text="dangerConfirmText"
    :min-note-length="10"
    @close="dangerOpen = false"
    @submit="onDangerSubmit"
  />
</template>

<script setup lang="ts">
/**
 * @file AccountsPage.vue
 * @module admin-console/pages/AccountsPage
 * @description 账号与角色页（权限矩阵展示 + 管理员账号可配置）。
 */
import { computed, reactive, ref } from 'vue';
import { useRouter } from 'vue-router';
import {
  PageHeader,
  UiTable,
  StatusTag,
  DangerConfirmModal,
  UiField,
  UiInput,
  UiSelect,
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
  type SelectOption,
  type DangerFact,
} from '@ui-kit';
import { repo, DEFAULT_ACTOR, type AdminUser } from '../api/repo';
import { session } from '../store/session';

const router = useRouter();

/** 刷新触发器。 */
const reloadKey = ref(0);

/** 账号列表（来自 repo 缓存：real = GET /admin/accounts；mock = 内置样例）。 */
const users = computed(() => {
  void reloadKey.value;
  return repo.allUsers();
});

/** 角色下拉选项（与后端 `GET /admin/roles` 同一套 id / 中文名）。 */
const roleOptions: readonly SelectOption[] = ROLES.map((r) => ({
  value: r,
  label: ROLE_META[r].label,
}));

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

// ---------------------------------------------------------------------------
// 新增 / 编辑账号
// ---------------------------------------------------------------------------

/** 弹窗开关。 */
const formOpen = ref(false);
/** 正在编辑的账号（null = 新增）。 */
const editing = ref<AdminUser | null>(null);
/** 保存中标记（防重复提交）。 */
const saving = ref(false);
/** 表单级错误提示。 */
const formError = ref('');
/** 表单草稿（仅存在于本页，关闭即弃）。 */
const form = reactive({ account: '', name: '', role: 'ops' as string, password: '' });

/** 打开「新增账号」。 */
function openCreate(): void {
  editing.value = null;
  form.account = '';
  form.name = '';
  form.role = 'ops';
  form.password = '';
  formError.value = '';
  formOpen.value = true;
}

/** 打开「编辑账号」。 */
function openEdit(row: AdminUser): void {
  editing.value = row;
  form.account = row.account;
  form.name = row.name;
  form.role = row.role;
  form.password = '';
  formError.value = '';
  formOpen.value = true;
}

/** 提交新增 / 编辑表单。 */
async function saveForm(): Promise<void> {
  formError.value = '';
  const account = form.account.trim();
  if (!account) {
    formError.value = '账号不能为空。';
    return;
  }
  if (!editing.value && !form.password) {
    formError.value = '新增账号必须设置初始口令。';
    return;
  }
  saving.value = true;
  const ok = editing.value
    ? await repo.updateUser({
        account: editing.value.account,
        name: form.name,
        role: form.role,
        password: form.password || undefined,
        actor: DEFAULT_ACTOR,
      })
    : await repo.createUser({
        account,
        name: form.name,
        role: form.role,
        password: form.password,
        actor: DEFAULT_ACTOR,
      });
  saving.value = false;
  if (ok) {
    formOpen.value = false;
    reloadKey.value += 1;
  }
}

// ---------------------------------------------------------------------------
// 启停 / 删除（危险动作 → DangerConfirmModal 四要素）
// ---------------------------------------------------------------------------

/** 危险弹窗开关。 */
const dangerOpen = ref(false);
/** 危险动作类别。 */
const dangerMode = ref<'enable' | 'disable' | 'delete'>('disable');
/** 危险动作目标行。 */
const dangerRow = ref<AdminUser | null>(null);

/** 目标账号名。 */
const dangerAccount = computed(() => dangerRow.value?.account ?? '');

/** 弹窗标题（含对象名，便于操作者确认）。 */
const dangerTitle = computed(() => {
  const verb = dangerMode.value === 'delete' ? '删除' : dangerMode.value === 'enable' ? '启用' : '停用';
  return `${verb}账号 ${dangerAccount.value}`;
});

/** 影响清单（写清后果与恢复路径）。 */
const dangerImpacts = computed<readonly string[]>(() =>
  dangerMode.value === 'delete'
    ? [
        '该账号记录将被永久删除，不可恢复。',
        '若它是最后一个「系统」管理员，删除后将无人可管理本后台，请先确保还有其他系统管理员。',
        '该账号的登录会话最长 1 小时后失效，期间仍可能有效。',
      ]
    : dangerMode.value === 'enable'
      ? [
          '该账号将恢复登录管理后台的能力（本操作可逆）。',
          '请确认该账号当前确由在职人员持有。',
        ]
      : [
          '该账号将立即无法登录管理后台；已签发的会话 token 最长 1 小时后失效。',
          '本操作可逆：随时可重新启用该账号。',
        ],
);

/** 操作对象摘要。 */
const dangerFacts = computed<readonly DangerFact[]>(() => [
  { label: '账号', value: dangerAccount.value },
  { label: '角色', value: dangerRow.value ? roleLabel(dangerRow.value.role) : '—' },
]);

/** 必选原因枚举。 */
const dangerReasons = computed<readonly string[]>(() =>
  dangerMode.value === 'delete'
    ? ['人员离职且账号不再需要', '重复 / 误建账号清理', '安全事件处置']
    : dangerMode.value === 'enable'
      ? ['人员到岗 / 恢复访问', '安全排查结束恢复', '误停用纠正']
      : ['账号泄露 / 疑似异常登录', '人员离职 / 岗位调整', '临时停用排查'],
);

/** 确认按钮文案（动词短语）。 */
const dangerConfirmText = computed(() =>
  dangerMode.value === 'delete' ? '删除账号' : dangerMode.value === 'enable' ? '启用账号' : '停用账号',
);

/** 打开「启停」危险确认。 */
function askToggle(row: AdminUser): void {
  dangerRow.value = row;
  dangerMode.value = row.status === 'user_enabled' ? 'disable' : 'enable';
  dangerOpen.value = true;
}

/** 打开「删除」危险确认。 */
function askDelete(row: AdminUser): void {
  dangerRow.value = row;
  dangerMode.value = 'delete';
  dangerOpen.value = true;
}

/** 危险确认提交（四要素已在前端校验通过）。 */
async function onDangerSubmit(payload: {
  reason: string;
  note: string;
  tail: string;
  secondApprover: string;
  confirm: string;
}): Promise<void> {
  const row = dangerRow.value;
  if (!row) {
    dangerOpen.value = false;
    return;
  }
  const common = {
    account: row.account,
    reason: payload.reason,
    note: payload.note,
    confirm: payload.confirm,
    actor: DEFAULT_ACTOR,
  };
  const ok =
    dangerMode.value === 'delete'
      ? await repo.deleteUser(common)
      : await repo.setUserStatus({ ...common, enabled: dangerMode.value === 'enable' });
  dangerOpen.value = false;
  if (ok) {
    reloadKey.value += 1;
  }
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
.ac-head-actions {
  margin-left: auto;
  display: inline-flex;
  gap: 8px;
}
.ac-row-actions {
  display: inline-flex;
  gap: 6px;
  white-space: nowrap;
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

/* 账号表单弹窗（自包含） */
.ac-modal__mask {
  position: fixed;
  inset: 0;
  z-index: 1200;
  display: flex;
  align-items: center;
  justify-content: center;
  background: rgb(15 27 61 / 45%);
}
.ac-modal {
  width: min(440px, 92vw);
  background: var(--surface, #fff);
  border-radius: var(--radius-md, 12px);
  box-shadow: 0 18px 48px rgb(15 27 61 / 28%);
  overflow: hidden;
}
.ac-modal__title {
  margin: 0;
  padding: 16px 20px;
  font-family: var(--font-display);
  font-size: 16px;
  font-weight: 600;
  color: var(--text-1);
  border-bottom: 1px solid var(--divider);
}
.ac-modal__body {
  padding: 16px 20px;
  display: flex;
  flex-direction: column;
  gap: 14px;
}
.ac-modal__error {
  margin: 0;
  font-size: var(--fs-caption);
  color: var(--danger-fg, #c0392b);
}
.ac-modal__foot {
  padding: 12px 20px;
  display: flex;
  justify-content: flex-end;
  gap: 8px;
  border-top: 1px solid var(--divider);
  background: #fafbfc;
}
.ac-btn--primary {
  background: var(--brand, #17c3b2);
  color: #fff;
  border-color: var(--brand, #17c3b2);
}
.ac-btn--primary:disabled {
  opacity: 0.6;
  cursor: not-allowed;
}
</style>

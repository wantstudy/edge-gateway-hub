<!--
  =============================================================================
  AccountsPage —— 账号与角色
  =============================================================================
  交付要点：
    · 页头已去掉（原型 :3425）：不渲染标题 / 描述 / 面包屑，只保留内容区块；
      「新增账号」按钮收进账号列表卡工具条右侧；
    · 无提示性文案块（无说明条 / 无解释性卡片副标题）；仅保留真实错误原因、
      危险操作二次确认文案、极简空态 + 下一步动作；
    · 账号管理：分页列表 + 新增（账号名 + 初始口令 + 角色，角色下拉含自定义角色）、
      改角色、重置口令、禁用 / 启用（危险操作走二次确认）；全部真实接口，
      按 WriteResult.ok 判定，失败展示后端真实 message，绝不本地假装成功；
    · 角色自定义：角色列表（角色名 / 标识 / 权限数 / 关联账号 / 编辑·删除）+
      新增 / 编辑角色弹窗（角色名 + 按分组勾选权限）；内置角色不可删除（按钮禁用 + 原因），
      被账号在用的角色不可删除（展示真实原因）；
    · 权限矩阵按 repo.permissions() + repo.roles.list() 真实渲染，不再写死。
  边界：前端 RoleGate 只控制**可见性**，权限判定在网关（Rust）侧。
-->
<template>
  <div class="wc-content">
    <!-- 写操作失败：真实原因（可关闭） -->
    <p v-if="actionError" class="ac-alert" role="alert" data-testid="action-error">
      <span class="ac-alert__text">{{ actionError }}</span>
      <button type="button" class="wc-btn wc-btn--sm" @click="actionError = ''">关闭</button>
    </p>

    <!-- 页签：账号管理 / 角色与权限 -->
    <div class="ac-tabs" role="tablist">
      <button
        type="button"
        class="ac-tab"
        :class="{ 'is-on': tab === 'accounts' }"
        role="tab"
        :aria-selected="tab === 'accounts' ? 'true' : 'false'"
        data-testid="tab-accounts"
        @click="tab = 'accounts'"
      >
        账号管理
      </button>
      <button
        type="button"
        class="ac-tab"
        :class="{ 'is-on': tab === 'roles' }"
        role="tab"
        :aria-selected="tab === 'roles' ? 'true' : 'false'"
        data-testid="tab-roles"
        @click="tab = 'roles'"
      >
        角色与权限
      </button>
    </div>

    <!-- ================= 账号管理 ================= -->
    <section v-if="tab === 'accounts'" class="wc-card">
      <div class="wc-card__head">
        <h3>账号列表</h3>
        <div class="wc-card__ops">
          <RoleGate :allowed="canManage" mode="disable" deny-text="当前角色无权新增账号" fallback-label="无权新增">
            <button
              type="button"
              class="wc-btn wc-btn--primary wc-btn--sm"
              data-testid="btn-add-account"
              @click="openCreateAccount"
            >
              新增账号
            </button>
          </RoleGate>
        </div>
      </div>
      <div class="wc-card__body wc-card__body--flush">
        <!-- 真实失败原因（后端 404 / 403 / 网络失败等） -->
        <p v-if="accountsNotice" class="ac-alert" role="alert" data-testid="accounts-notice">
          <span class="ac-alert__text">{{ accountsNotice }}</span>
          <button type="button" class="wc-btn wc-btn--sm" @click="loadAccounts">重试</button>
        </p>

        <EmptyState
          v-else-if="accounts.length === 0"
          title="暂无账号"
          desc="账号清单为空；新增账号后将在此列出。"
        >
          <template #actions>
            <button type="button" class="wc-btn wc-btn--primary" data-testid="accounts-empty-add" @click="openCreateAccount">
              新增账号
            </button>
          </template>
        </EmptyState>

        <template v-else>
          <UiTable :columns="accountColumns" :rows="pagedAccounts" row-key-field="account" data-testid="account-table">
            <template #cell-account="{ row }">
              <span class="wc-mono">{{ row.account }}</span>
            </template>
            <template #cell-role="{ row }">
              {{ roleName(row.role) }}
            </template>
            <template #cell-status="{ row }">
              <StatusTag :status="row.status" />
            </template>
            <template #cell-lastLoginAt="{ row }">
              <span class="wc-mono">{{ row.lastLoginAt }}</span>
            </template>
            <template #actions="{ row }">
              <RoleGate :allowed="canManage" mode="disable" deny-text="当前角色无权修改账号" fallback-label="改角色">
                <button type="button" class="wc-btn wc-btn--sm" data-testid="btn-row-role" @click="openChangeRole(row)">
                  改角色
                </button>
              </RoleGate>
              <RoleGate :allowed="canManage" mode="disable" deny-text="当前角色无权重置口令" fallback-label="重置口令">
                <button type="button" class="wc-btn wc-btn--sm" data-testid="btn-row-edit" @click="openResetPassword(row.account)">
                  重置口令
                </button>
              </RoleGate>
              <RoleGate :allowed="canManage" mode="disable" deny-text="当前角色无权变更账号状态" fallback-label="禁用">
                <button
                  type="button"
                  class="wc-btn wc-btn--sm"
                  :class="{ 'wc-btn--danger': isEnabled(row.status) }"
                  data-testid="btn-row-toggle"
                  @click="openToggleAccount(row)"
                >
                  {{ isEnabled(row.status) ? '禁用' : '启用' }}
                </button>
              </RoleGate>
            </template>
          </UiTable>
          <UiPager
            :page="page"
            :total="accounts.length"
            :page-size="ACCOUNT_PAGE_SIZE"
            data-testid="account-pager"
            @update:page="page = $event"
          />
        </template>
      </div>
    </section>

    <!-- ================= 角色与权限 ================= -->
    <template v-if="tab === 'roles'">
      <section class="wc-card">
        <div class="wc-card__head">
          <h3>角色列表</h3>
          <div class="wc-card__ops">
            <RoleGate :allowed="canManage" mode="disable" deny-text="当前角色无权管理角色" fallback-label="无权新增">
              <button type="button" class="wc-btn wc-btn--primary wc-btn--sm" data-testid="btn-add-role" @click="openCreateRole">
                新增角色
              </button>
            </RoleGate>
          </div>
        </div>
        <div class="wc-card__body wc-card__body--flush">
          <p v-if="rolesNotice" class="ac-alert" role="alert" data-testid="roles-notice">
            <span class="ac-alert__text">{{ rolesNotice }}</span>
            <button type="button" class="wc-btn wc-btn--sm" @click="loadRoles">重试</button>
          </p>

          <EmptyState v-else-if="roles.length === 0" title="暂无角色" desc="角色清单为空；新增角色后账号即可选用。">
            <template #actions>
              <button type="button" class="wc-btn wc-btn--primary" data-testid="roles-empty-add" @click="openCreateRole">
                新增角色
              </button>
            </template>
          </EmptyState>

          <UiTable v-else :columns="roleColumns" :rows="roles" row-key-field="id" data-testid="role-table">
            <template #cell-name="{ row }">
              <span :data-testid="`role-${row.id}`">{{ row.name }}</span>
              <span v-if="row.builtin" class="ac-builtin">内置</span>
            </template>
            <template #cell-id="{ row }">
              <span class="wc-mono">{{ row.id }}</span>
            </template>
            <template #cell-permissions="{ row }">
              {{ row.permissions.length }}
            </template>
            <template #cell-accountCount="{ row }">
              {{ row.accountCount }}
            </template>
            <template #actions="{ row }">
              <button type="button" class="wc-btn wc-btn--sm" data-testid="role-row-edit" @click="openEditRole(row)">
                编辑
              </button>
              <button
                type="button"
                class="wc-btn wc-btn--sm"
                :disabled="!canDeleteRole(row)"
                :title="canDeleteRole(row) ? '' : roleDeleteReason(row)"
                data-testid="role-row-delete"
                @click="openDeleteRole(row)"
              >
                删除
              </button>
            </template>
          </UiTable>
        </div>
      </section>

      <!-- 权限矩阵：按真实权限清单 + 各角色真实权限集渲染 -->
      <section class="wc-card">
        <div class="wc-card__head">
          <h3>权限矩阵</h3>
        </div>
        <div class="wc-card__body wc-card__body--flush">
          <p v-if="permissionsNotice" class="ac-alert" role="alert" data-testid="permissions-notice">
            <span class="ac-alert__text">{{ permissionsNotice }}</span>
            <button type="button" class="wc-btn wc-btn--sm" @click="loadPermissions">重试</button>
          </p>

          <EmptyState
            v-else-if="permissions.length === 0"
            title="暂无权限清单"
            desc="权限清单为空；后端权限接口返回数据后自动渲染。"
          />

          <div v-else class="ac-matrix-wrap">
            <table class="wc-matrix">
              <thead>
                <tr>
                  <th>权限</th>
                  <th v-for="r in roles" :key="r.id" class="wc-matrix__c">{{ r.name }}</th>
                </tr>
              </thead>
              <tbody>
                <template v-for="grp in permissionGroups" :key="grp.group">
                  <tr class="ac-matrix__group">
                    <td :colspan="roles.length + 1">{{ grp.group }}</td>
                  </tr>
                  <tr v-for="p in grp.items" :key="p.id">
                    <td>{{ p.label }}</td>
                    <td v-for="r in roles" :key="r.id" class="wc-matrix__c">
                      <span v-if="r.permissions.includes(p.id)" class="wc-matrix__yes">✓</span>
                      <span v-else class="wc-matrix__no">—</span>
                    </td>
                  </tr>
                </template>
              </tbody>
            </table>
          </div>
        </div>
      </section>
    </template>
  </div>

  <!-- ================= 新增账号弹窗 ================= -->
  <Teleport to="body">
    <div v-if="createOpen" class="wc-modal__mask" @click.self="closeCreateAccount">
      <div class="wc-modal" role="dialog" aria-modal="true" aria-label="新增账号">
        <div class="wc-modal__head"><h3 class="wc-modal__title">新增账号</h3></div>
        <div class="wc-modal__body">
          <UiField label="账号" required :error="createError">
            <UiInput v-model="createForm.account" placeholder="如 ops04" data-testid="create-account" />
          </UiField>
          <UiField label="角色" required>
            <UiSelect v-model="createForm.role" :options="roleOptions" data-testid="create-role" />
          </UiField>
          <UiField label="初始口令" required :hint="`≥10 位，含大小写、数字与符号；首次登录强制修改`" :error="createPwdError">
            <UiInput
              v-model="createForm.password"
              type="password"
              autocomplete="new-password"
              placeholder="初始口令"
              data-testid="create-password"
            />
          </UiField>
        </div>
        <div class="wc-modal__foot">
          <button type="button" class="wc-btn" @click="closeCreateAccount">取消</button>
          <button
            type="button"
            class="wc-btn wc-btn--primary"
            :disabled="creating"
            data-testid="btn-create-submit"
            @click="submitCreateAccount"
          >
            创建账号
          </button>
        </div>
      </div>
    </div>
  </Teleport>

  <!-- ================= 改角色弹窗 ================= -->
  <Teleport to="body">
    <div v-if="roleOpen" class="wc-modal__mask" @click.self="roleOpen = false">
      <div class="wc-modal" role="dialog" aria-modal="true" aria-label="修改账号角色">
        <div class="wc-modal__head"><h3 class="wc-modal__title">改角色 · {{ roleTarget }}</h3></div>
        <div class="wc-modal__body">
          <UiField label="角色" required :error="roleError">
            <UiSelect v-model="roleForm.role" :options="roleOptions" data-testid="change-role-select" />
          </UiField>
        </div>
        <div class="wc-modal__foot">
          <button type="button" class="wc-btn" @click="roleOpen = false">取消</button>
          <button type="button" class="wc-btn wc-btn--primary" data-testid="btn-change-role-submit" @click="submitChangeRole">
            保存
          </button>
        </div>
      </div>
    </div>
  </Teleport>

  <!-- ================= 重置口令弹窗 ================= -->
  <Teleport to="body">
    <div v-if="pwdOpen" class="wc-modal__mask" @click.self="closeResetPassword">
      <div class="wc-modal" role="dialog" aria-modal="true" aria-label="重置口令">
        <div class="wc-modal__head"><h3 class="wc-modal__title">重置口令 · {{ pwdAccount }}</h3></div>
        <div class="wc-modal__body">
          <UiField label="新口令" required hint="≥10 位，含大小写、数字与符号" :error="pwdError">
            <UiInput
              v-model="pwdForm.password"
              type="password"
              autocomplete="new-password"
              placeholder="新口令"
              data-testid="pwd-input"
            />
          </UiField>
        </div>
        <div class="wc-modal__foot">
          <button type="button" class="wc-btn" @click="closeResetPassword">取消</button>
          <button
            type="button"
            class="wc-btn wc-btn--primary"
            :disabled="pwdForm.password.length < 10"
            data-testid="btn-pwd-submit"
            @click="submitResetPassword"
          >
            确认重置
          </button>
        </div>
      </div>
    </div>
  </Teleport>

  <!-- ================= 新增 / 编辑角色弹窗 ================= -->
  <Teleport to="body">
    <div v-if="roleEditorOpen" class="wc-modal__mask" @click.self="roleEditorOpen = false">
      <div class="wc-modal" role="dialog" aria-modal="true" :aria-label="roleEditorId ? '编辑角色' : '新增角色'">
        <div class="wc-modal__head">
          <h3 class="wc-modal__title">{{ roleEditorId ? `编辑角色 · ${roleEditorId}` : '新增角色' }}</h3>
        </div>
        <div class="wc-modal__body">
          <UiField label="角色名" required :error="roleEditorError">
            <UiInput
              v-model="roleEditorName"
              :disabled="!nameEditable"
              placeholder="如 现场巡检"
              data-testid="role-name"
            />
          </UiField>
          <UiField label="权限（勾选即授予）" required>
            <div class="ac-perms" data-testid="role-perms">
              <p v-if="permissions.length === 0" class="ac-perms__empty">权限清单为空，暂无可勾选权限。</p>
              <div v-for="grp in permissionGroups" :key="grp.group" class="ac-perms__group">
                <p class="ac-perms__gtitle">{{ grp.group }}</p>
                <label v-for="p in grp.items" :key="p.id" class="ac-perms__item" :data-testid="`role-perm-${p.id}`">
                  <input
                    type="checkbox"
                    :checked="roleEditorPerms.includes(p.id)"
                    @change="togglePerm(p.id)"
                  />
                  <span class="ac-perms__label">{{ p.label }}</span>
                  <span class="ac-perms__id wc-mono">{{ p.id }}</span>
                </label>
              </div>
            </div>
          </UiField>
        </div>
        <div class="wc-modal__foot">
          <button type="button" class="wc-btn" @click="roleEditorOpen = false">取消</button>
          <button type="button" class="wc-btn wc-btn--primary" data-testid="btn-role-submit" @click="submitRoleEditor">
            保存
          </button>
        </div>
      </div>
    </div>
  </Teleport>

  <!-- ================= 账号启用 / 禁用：危险操作二次确认 ================= -->
  <DangerConfirmModal
    :open="toggleConfirm.open"
    :title="toggleConfirm.title"
    :impacts="toggleConfirm.impacts"
    :facts="toggleConfirm.facts"
    :reasons="ACCOUNT_REASONS"
    :min-note-length="10"
    confirm-mode="full"
    :confirm-value="toggleConfirm.account"
    confirm-label="请输入账号全名"
    confirm-placeholder="输入账号全名"
    :confirm-text="toggleConfirm.confirmText"
    @close="toggleConfirm.open = false"
    @submit="submitToggleAccount"
  />

  <!-- ================= 删除角色：危险操作二次确认 ================= -->
  <DangerConfirmModal
    :open="deleteConfirm.open"
    :title="deleteConfirm.title"
    :impacts="deleteConfirm.impacts"
    :facts="deleteConfirm.facts"
    :reasons="ROLE_REASONS"
    :min-note-length="10"
    confirm-mode="full"
    :confirm-value="deleteConfirm.name"
    confirm-label="请输入角色全名"
    confirm-placeholder="输入角色全名"
    confirm-text="确认删除角色"
    @close="deleteConfirm.open = false"
    @submit="submitDeleteRole"
  />
</template>

<script setup lang="ts">
/**
 * @file AccountsPage.vue
 * @module web-console/pages/AccountsPage
 * @description 账号与角色页：账号管理（分页 + 自定义角色）+ 角色自定义（权限勾选）
 *              + 权限矩阵（真实渲染）。数据一律来自 `@/api/repo`，失败展示真实原因。
 */
import { computed, onMounted, reactive, ref } from 'vue';
import {
  DangerConfirmModal,
  EmptyState,
  RoleGate,
  StatusTag,
  UiField,
  UiInput,
  UiPager,
  UiSelect,
  UiTable,
  type DangerFact,
  type SelectOption,
  type TableColumn,
} from '@ui-kit';
import {
  repo,
  type AccountRecord,
  type PermissionRecord,
  type RoleRecord,
  type WriteResult,
} from '../api/repo';
import { session } from '../store/session';

/** 每页条数。 */
const ACCOUNT_PAGE_SIZE = 10;

/** 当前页签。 */
const tab = ref<'accounts' | 'roles'>('accounts');

/** 写操作失败的真实原因（可关闭）。 */
const actionError = ref('');

/** 当前角色是否可管理账号 / 角色（仅 admin 可见可点；判定在网关侧）。 */
const canManage = computed<boolean>(() => session.state.role === 'admin');

// ---------------------------------------------------------------------------
// 数据加载（真实接口；空 = 诚实空态，失败 = 真实原因）
// ---------------------------------------------------------------------------

/** 账号清单。 */
const accounts = ref<AccountRecord[]>([]);
/** 账号清单不可得原因。 */
const accountsNotice = ref('');
/** 账号分页。 */
const page = ref(1);

/** 角色清单。 */
const roles = ref<RoleRecord[]>([]);
/** 角色清单不可得原因。 */
const rolesNotice = ref('');

/** 权限清单（**仅网关侧**；已剔除厂商侧 `scope=licensing` 行）。 */
const permissions = ref<PermissionRecord[]>([]);
/** 权限清单不可得原因。 */
const permissionsNotice = ref('');
/** 厂商侧（licensing）权限 id 集合（从原始目录提取，供编辑草稿剔除）。 */
const licensingPermIds = ref<ReadonlySet<string>>(new Set<string>());

/** 拉取账号清单。 */
async function loadAccounts(): Promise<void> {
  accounts.value = await repo.accounts.list();
  accountsNotice.value = repo.actions.notices().accounts;
  const maxPage = Math.max(1, Math.ceil(accounts.value.length / ACCOUNT_PAGE_SIZE));
  if (page.value > maxPage) {
    page.value = maxPage;
  }
}

/** 拉取角色清单。 */
async function loadRoles(): Promise<void> {
  roles.value = await repo.roles.list();
  rolesNotice.value = repo.actions.notices().roles;
}

/** 拉取权限清单（两端隔离兜底：即便后端误透出厂商侧 `scope=licensing` 权限，也不渲染）。 */
async function loadPermissions(): Promise<void> {
  const rows = await repo.permissions();
  licensingPermIds.value = new Set(
    rows.filter((p) => p.scope === 'licensing').map((p) => p.id),
  );
  permissions.value = rows.filter((p) => p.scope !== 'licensing');
  permissionsNotice.value = repo.actions.notices().permissions;
}

onMounted(() => {
  void loadAccounts();
  void loadRoles();
  void loadPermissions();
});

/** 分页后的账号行。 */
const pagedAccounts = computed<AccountRecord[]>(() => {
  const start = (page.value - 1) * ACCOUNT_PAGE_SIZE;
  return accounts.value.slice(start, start + ACCOUNT_PAGE_SIZE);
});

/** 账号列定义。 */
const accountColumns: readonly TableColumn[] = [
  { key: 'account', label: '账号', mono: true },
  { key: 'role', label: '角色' },
  { key: 'status', label: '状态' },
  { key: 'lastLoginAt', label: '最后登录', mono: true },
];

/** 角色列定义。 */
const roleColumns: readonly TableColumn[] = [
  { key: 'name', label: '角色名' },
  { key: 'id', label: '标识', mono: true },
  { key: 'permissions', label: '拥有权限' },
  { key: 'accountCount', label: '关联账号' },
];

/** 角色下拉选项（含自定义角色）。 */
const roleOptions = computed<readonly SelectOption[]>(() =>
  roles.value.map((r) => ({ value: r.id, label: r.name })),
);

/** 角色 id → 角色名（未知 id 原样透传）。 */
function roleName(roleId: string): string {
  const found = roles.value.find((r) => r.id === roleId);
  return found ? found.name : roleId || '—';
}

/** 账号是否处于启用态（未知状态按启用处理，禁用态集合显式列出）。 */
const DISABLED_STATUSES: readonly string[] = ['user_disabled', 'disabled', 'inactive', 'locked', 'expired'];
function isEnabled(status: string): boolean {
  return !DISABLED_STATUSES.includes(status);
}

/** 权限分组（按后端 group 字段归并，保持首次出现顺序）。 */
interface PermGroup {
  /** 分组名 */
  readonly group: string;
  /** 组内权限 */
  readonly items: PermissionRecord[];
}
const permissionGroups = computed<PermGroup[]>(() => {
  const map = new Map<string, PermissionRecord[]>();
  for (const p of permissions.value) {
    const key = p.group || '其他';
    const arr = map.get(key);
    if (arr) {
      arr.push(p);
    } else {
      map.set(key, [p]);
    }
  }
  return [...map.entries()].map(([group, items]) => ({ group, items }));
});

// ---------------------------------------------------------------------------
// 新增账号
// ---------------------------------------------------------------------------

/** 新增弹窗开关。 */
const createOpen = ref(false);
/** 提交中（防重复提交）。 */
const creating = ref(false);
/** 新增表单草稿（口令提交后立即清空，绝不写入日志 / 页面文本）。 */
const createForm = reactive({ account: '', role: '', password: '' });
/** 新增行内错误（真实原因）。 */
const createError = ref('');
/** 口令强度错误。 */
const createPwdError = ref('');

/** 打开新增弹窗（重置草稿）。 */
function openCreateAccount(): void {
  createOpen.value = true;
  createForm.account = '';
  createForm.role = roleOptions.value[0]?.value ?? '';
  createForm.password = '';
  createError.value = '';
  createPwdError.value = '';
}

/** 关闭新增弹窗（清空口令草稿）。 */
function closeCreateAccount(): void {
  createOpen.value = false;
  createForm.password = '';
}

/** 提交新增账号（真实接口；按 ok 判定）。 */
async function submitCreateAccount(): Promise<void> {
  createError.value = '';
  createPwdError.value = '';
  const account = createForm.account.trim();
  if (!account) {
    createError.value = '请输入账号';
    return;
  }
  if (!createForm.role) {
    createError.value = roles.value.length === 0 ? '角色清单为空，请先新增角色' : '请选择角色';
    return;
  }
  if (createForm.password.length < 10) {
    createPwdError.value = '初始口令至少 10 位';
    return;
  }
  creating.value = true;
  const result: WriteResult<AccountRecord> = await repo.accounts.create({
    account,
    role: createForm.role,
    password: createForm.password,
  });
  // 无论成败，提交后立即清空口令草稿
  createForm.password = '';
  creating.value = false;
  if (!result.ok) {
    createError.value = result.message;
    return;
  }
  createOpen.value = false;
  await loadAccounts();
  page.value = Math.max(1, Math.ceil(accounts.value.length / ACCOUNT_PAGE_SIZE));
}

// ---------------------------------------------------------------------------
// 改角色
// ---------------------------------------------------------------------------

/** 改角色弹窗开关。 */
const roleOpen = ref(false);
/** 目标账号。 */
const roleTarget = ref('');
/** 角色草稿。 */
const roleForm = reactive({ role: '' });
/** 行内错误。 */
const roleError = ref('');

/** 打开改角色弹窗。 */
function openChangeRole(row: AccountRecord): void {
  roleOpen.value = true;
  roleTarget.value = row.account;
  roleForm.role = row.role;
  roleError.value = '';
}

/** 提交改角色（真实接口）。 */
async function submitChangeRole(): Promise<void> {
  roleError.value = '';
  if (!roleForm.role) {
    roleError.value = '请选择角色';
    return;
  }
  const result: WriteResult<AccountRecord> = await repo.accounts.update({
    account: roleTarget.value,
    role: roleForm.role,
  });
  if (!result.ok) {
    roleError.value = result.message;
    return;
  }
  roleOpen.value = false;
  await loadAccounts();
}

// ---------------------------------------------------------------------------
// 重置口令
// ---------------------------------------------------------------------------

/** 重置口令弹窗开关。 */
const pwdOpen = ref(false);
/** 目标账号。 */
const pwdAccount = ref('');
/** 口令草稿。 */
const pwdForm = reactive({ password: '' });
/** 行内错误。 */
const pwdError = ref('');

/** 打开重置口令弹窗。 */
function openResetPassword(account: string): void {
  pwdOpen.value = true;
  pwdAccount.value = account;
  pwdForm.password = '';
  pwdError.value = '';
}

/** 关闭重置口令弹窗（清空口令草稿）。 */
function closeResetPassword(): void {
  pwdOpen.value = false;
  pwdForm.password = '';
}

/** 提交重置口令（真实接口）。 */
async function submitResetPassword(): Promise<void> {
  pwdError.value = '';
  if (pwdForm.password.length < 10) {
    pwdError.value = '新口令至少 10 位';
    return;
  }
  const password = pwdForm.password;
  const result = await repo.accounts.resetPassword({ account: pwdAccount.value, newPassword: password });
  pwdForm.password = '';
  if (!result.ok) {
    pwdError.value = result.message;
    return;
  }
  pwdOpen.value = false;
}

// ---------------------------------------------------------------------------
// 账号 启用 / 禁用：危险操作二次确认
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
const ACCOUNT_REASONS: readonly string[] = ['人员离职', '账号安全风险', '权限调整', '其它（请在补充说明中描述）'];

/** 打开启用 / 禁用二次确认。 */
function openToggleAccount(row: AccountRecord): void {
  const disabling = isEnabled(row.status);
  toggleConfirm.open = true;
  toggleConfirm.account = row.account;
  toggleConfirm.title = `${disabling ? '禁用' : '启用'}账号 · ${row.account}`;
  toggleConfirm.confirmText = disabling ? '确认禁用' : '确认启用';
  toggleConfirm.impacts = disabling
    ? ['该账号将无法登录控制台，进行中的会话被终止。', '已有审计记录保留，可在日志与审计页检索。', '后续如需恢复，可在本页重新启用。']
    : ['该账号将恢复登录能力。', '账号权限仍受其角色约束，不会超出角色范围。'];
  toggleConfirm.facts = [
    { label: '账号', value: row.account },
    { label: '角色', value: roleName(row.role) },
  ];
}

/** 提交启用 / 禁用（真实接口；失败展示真实原因）。 */
async function submitToggleAccount(_payload: { reason: string; note: string; tail: string }): Promise<void> {
  const account = toggleConfirm.account;
  const enabling = !isEnabled(currentStatus(account));
  toggleConfirm.open = false;
  const result = await repo.accounts.update({
    account,
    status: enabling ? 'user_enabled' : 'user_disabled',
  });
  if (!result.ok) {
    actionError.value = result.message;
    return;
  }
  actionError.value = '';
  await loadAccounts();
}

/** 取账号当前状态（用于判定本次是启用还是禁用）。 */
function currentStatus(account: string): string {
  return accounts.value.find((a) => a.account === account)?.status ?? '';
}

// ---------------------------------------------------------------------------
// 角色自定义：新增 / 编辑
// ---------------------------------------------------------------------------

/** 角色编辑弹窗开关。 */
const roleEditorOpen = ref(false);
/** 编辑目标 id（空串 = 新增）。 */
const roleEditorId = ref('');
/** 角色名草稿。 */
const roleEditorName = ref('');
/** 已勾选权限 id。 */
const roleEditorPerms = ref<string[]>([]);
/** 行内错误。 */
const roleEditorError = ref('');
/** 是否内置角色（内置角色名不可改）。 */
const roleEditorBuiltin = ref(false);

/** 角色名是否可编辑（内置角色名固定）。 */
const nameEditable = computed<boolean>(() => !roleEditorBuiltin.value);

/** 打开新增角色弹窗。 */
function openCreateRole(): void {
  roleEditorOpen.value = true;
  roleEditorId.value = '';
  roleEditorName.value = '';
  roleEditorPerms.value = [];
  roleEditorError.value = '';
  roleEditorBuiltin.value = false;
}

/** 打开编辑角色弹窗。 */
function openEditRole(row: RoleRecord): void {
  roleEditorOpen.value = true;
  roleEditorId.value = row.id;
  roleEditorName.value = row.name;
  // 两端隔离：厂商侧权限不在网关侧呈现，编辑草稿一并剔除
  // （否则提交时会被后端 `validate_permissions` fail-closed 拒绝为 400）。
  roleEditorPerms.value = row.permissions.filter((id) => !licensingPermIds.value.has(id));
  roleEditorError.value = '';
  roleEditorBuiltin.value = row.builtin;
}

/** 勾选 / 取消勾选权限。 */
function togglePerm(id: string): void {
  const idx = roleEditorPerms.value.indexOf(id);
  if (idx >= 0) {
    roleEditorPerms.value.splice(idx, 1);
  } else {
    roleEditorPerms.value.push(id);
  }
}

/** 提交角色新增 / 编辑（真实接口；角色 id 由服务端生成，页面不编造）。 */
async function submitRoleEditor(): Promise<void> {
  roleEditorError.value = '';
  const name = roleEditorName.value.trim();
  if (nameEditable.value && !name) {
    roleEditorError.value = '请输入角色名';
    return;
  }
  const result: WriteResult<RoleRecord> = roleEditorId.value
    ? roleEditorBuiltin.value
      ? await repo.roles.update({ id: roleEditorId.value, permissions: roleEditorPerms.value })
      : await repo.roles.update({ id: roleEditorId.value, name, permissions: roleEditorPerms.value })
    : await repo.roles.create({ name, permissions: roleEditorPerms.value });
  if (!result.ok) {
    roleEditorError.value = result.message;
    return;
  }
  roleEditorOpen.value = false;
  await loadRoles();
}

// ---------------------------------------------------------------------------
// 删除角色：内置不可删 / 被账号在用不可删 / 危险二次确认
// ---------------------------------------------------------------------------

/** 危险确认弹窗状态。 */
const deleteConfirm = reactive<{
  open: boolean;
  id: string;
  name: string;
  title: string;
  impacts: readonly string[];
  facts: readonly DangerFact[];
}>({
  open: false,
  id: '',
  name: '',
  title: '',
  impacts: [],
  facts: [],
});

/** 角色删除原因枚举。 */
const ROLE_REASONS: readonly string[] = ['角色调整', '职责变更', '角色重复', '其它（请在补充说明中描述）'];

/** 角色是否可删除（内置不可删；被账号在用不可删）。 */
function canDeleteRole(row: RoleRecord): boolean {
  return !row.builtin && row.accountCount === 0;
}

/** 角色不可删除的真实原因（可删除时为空串）。 */
function roleDeleteReason(row: RoleRecord): string {
  if (row.builtin) {
    return '内置角色不可删除';
  }
  if (row.accountCount > 0) {
    return `仍有 ${row.accountCount} 个账号使用该角色，不可删除`;
  }
  return '';
}

/** 打开删除角色二次确认。 */
function openDeleteRole(row: RoleRecord): void {
  if (!canDeleteRole(row)) {
    return;
  }
  deleteConfirm.open = true;
  deleteConfirm.id = row.id;
  deleteConfirm.name = row.name;
  deleteConfirm.title = `删除角色 · ${row.name}`;
  deleteConfirm.impacts = [
    `角色「${row.name}」将从角色清单中移除，不再可分配给账号。`,
    '当前无账号使用该角色，删除不影响任何账号。',
    '如需恢复，需重新创建角色并重新勾选权限。',
  ];
  deleteConfirm.facts = [
    { label: '角色名', value: row.name },
    { label: '标识', value: row.id },
    { label: '拥有权限', value: `${row.permissions.length} 项` },
  ];
}

/** 提交删除角色（真实接口；失败展示真实原因）。 */
async function submitDeleteRole(_payload: { reason: string; note: string; tail: string }): Promise<void> {
  const id = deleteConfirm.id;
  deleteConfirm.open = false;
  const result = await repo.roles.remove({ id });
  if (!result.ok) {
    actionError.value = result.message;
    return;
  }
  actionError.value = '';
  await loadRoles();
}
</script>

<style scoped>
/* 页签 */
.ac-tabs {
  display: flex;
  gap: 6px;
}
.ac-tab {
  font-family: inherit;
  font-size: var(--fs-table);
  border: 1px solid var(--border);
  border-radius: var(--radius-sm);
  padding: 7px 16px;
  background: var(--bg-hover);
  color: var(--text-2);
  cursor: pointer;
  white-space: nowrap;
}
.ac-tab:hover {
  border-color: var(--brand);
  color: var(--brand-ink);
}
/* 选中态：品牌色填充 + 可读品牌文字（--brand 直用文字在 --brand-subtle 上仅 2.0:1） */
.ac-tab.is-on {
  border-color: var(--brand);
  background: var(--brand-subtle);
  color: var(--brand-ink);
  font-weight: 600;
}

/* 真实错误提示条 */
.ac-alert {
  display: flex;
  align-items: center;
  gap: 12px;
  margin: 0;
  padding: 10px 14px;
  border: 1px solid var(--danger-border);
  border-radius: var(--radius-sm);
  background: var(--danger-bg);
  color: var(--danger-fg);
  font-size: var(--fs-table);
  line-height: 1.7;
}
.ac-alert__text {
  flex: 1;
  word-break: break-all;
}

/* 内置角色徽标 */
.ac-builtin {
  margin-left: 6px;
  padding: 0 6px;
  border-radius: var(--radius-pill);
  border: 1px solid var(--border);
  background: var(--bg-app);
  color: var(--text-3);
  font-size: 11px;
}

/* 权限矩阵 */
.ac-matrix-wrap {
  overflow-x: auto;
}
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
  white-space: nowrap;
}
.wc-matrix th {
  font-size: var(--fs-caption);
  color: var(--text-3);
  font-weight: 600;
  background: var(--bg-app);
}
.wc-matrix__c {
  text-align: center;
}
.ac-matrix__group td {
  background: var(--bg-hover);
  color: var(--text-2);
  font-weight: 600;
  font-size: var(--fs-caption);
}
.wc-matrix__yes {
  color: var(--ok-fg);
  font-weight: 600;
}
.wc-matrix__no {
  color: var(--text-3);
}

/* 权限勾选面板（可滚动） */
.ac-perms {
  max-height: 320px;
  overflow: auto;
  border: 1px solid var(--border);
  border-radius: var(--radius-sm);
  padding: 8px 12px;
  display: flex;
  flex-direction: column;
  gap: 10px;
}
.ac-perms__empty {
  margin: 0;
  font-size: var(--fs-caption);
  color: var(--text-3);
}
.ac-perms__group {
  display: flex;
  flex-direction: column;
  gap: 4px;
}
.ac-perms__gtitle {
  margin: 0;
  font-size: var(--fs-caption);
  font-weight: 600;
  color: var(--text-2);
}
.ac-perms__item {
  display: flex;
  align-items: center;
  gap: 8px;
  font-size: var(--fs-table);
  cursor: pointer;
}
.ac-perms__label {
  color: var(--text-1);
}
.ac-perms__id {
  color: var(--text-3);
  font-size: var(--fs-caption);
}

/* 弹窗 */
.wc-modal__mask {
  position: fixed;
  inset: 0;
  background: var(--mask);
  display: flex;
  align-items: center;
  justify-content: center;
  z-index: 2000;
  padding: 24px;
}
.wc-modal {
  background: var(--bg-card);
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
.wc-modal__foot {
  padding: 14px 20px;
  border-top: 1px solid var(--divider);
  display: flex;
  justify-content: flex-end;
  gap: 8px;
}
</style>

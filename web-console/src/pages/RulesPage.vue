<!--
  =============================================================================
  RulesPage —— 转发规则（设计 §3.4 关联 / 原型 gateway-v2a-glacier「rules」）
  =============================================================================
  声明式规则：来源 / 条件 / 动作 / 优先级；命中即路由。支持规则启用停用、编辑
  （结构化 `when` / `actions`，对齐后端 RuleUpsertBody 契约）、优先级排序。
  列表分页（UiPager，条数只此一个口径）。

  布局（需求 9 弹窗化）：主视图只有 **列表 + 搜索框 + 按钮**；
  「新增规则 / 编辑规则」表单收进标准弹窗（wc-modal），列表不再常驻渲染表单；
  「校验与版本」的诚实说明折进弹窗内折叠帮助，主视图不出现任何开发期提示文案。

  诚实边界（不再有任何伪造数据）：
  · 保存走真实 `POST /api/rules` / `PUT /api/rules/:id`，成功失败都不编造；
  · 网关**没有**转发规则 dry-run 端点 → 不提供「试运行」；
  · 网关**没有**规则版本历史 / 规则回滚端点 → 版本列表与回滚 UI 不渲染；
  · `hit_count` / `last_hit_at` 后端恒 0 / 恒空（引擎不记账）→ 列表显示「—」。
-->
<template>
  <div class="wc-content">
    <!-- ══ 规则列表（分页 + 搜索 + 弹窗式增改）═══════════════════════════ -->
    <section class="wc-card">
      <div class="wc-card__head">
        <h3>规则列表</h3>
        <span class="wc-card__sub">按优先级从小到大匹配，命中即路由 · 共 {{ ruleTotal }} 条</span>
        <div class="wc-card__ops">
          <RoleGate :allowed="canEdit" mode="disable" deny-text="当前角色为只读，不能新增规则">
            <button type="button" class="wc-btn wc-btn--primary" data-testid="add-rule" @click="startNewRule">
              新增规则
            </button>
          </RoleGate>
        </div>
      </div>

      <!-- 工具条：搜索框 -->
      <div class="rb-toolbar">
        <input
          v-model="query"
          class="wc-input"
          type="search"
          placeholder="搜索规则名称 / 生效出口 / 条件 / 动作"
          aria-label="搜索规则"
          data-testid="rule-search"
        />
      </div>

      <!-- 真实错误：数据源不可得的原因（不静默吞错） -->
      <div v-if="rulesNotice" class="wc-banner wc-banner--warn" data-testid="rules-notice">
        <span class="wc-banner__icon">!</span>
        <span>{{ rulesNotice }}</span>
      </div>

      <!-- 真实操作结果：启停 / 删除的回执 -->
      <div v-if="operationMessage" class="wc-banner wc-banner--warn" data-testid="operation-notice">
        <span class="wc-banner__icon">i</span>
        <span>{{ operationMessage }}</span>
      </div>

      <EmptyState v-if="ruleTotal === 0" title="还没有转发规则" desc="未配置规则时，出口会转发全部点位。">
        <template #actions>
          <button type="button" class="wc-btn wc-btn--primary" data-testid="add-rule-empty" @click="startNewRule">
            新增规则
          </button>
        </template>
      </EmptyState>

      <EmptyState v-else-if="filteredRules.length === 0" title="没有匹配的规则" desc="换个关键词试试。">
        <template #actions>
          <button type="button" class="wc-btn" data-testid="clear-search" @click="query = ''">清除搜索</button>
        </template>
      </EmptyState>

      <template v-else>
        <UiTable :columns="ruleColumns" :rows="pagedRules" row-key-field="id">
          <template #cell-name="{ row }">
            <span>{{ row.name }}</span>
          </template>
          <template #cell-forwarderName="{ row }">
            {{ row.forwarderName }}
          </template>
          <template #cell-condition="{ row }">
            <span class="wc-mono">{{ row.condition }}</span>
          </template>
          <template #cell-action="{ row }">
            <span class="wc-mono">{{ row.action }}</span>
          </template>
          <template #cell-priority="{ row }">
            <span class="wc-mono">{{ row.priority }}</span>
          </template>
          <template #cell-hitCount="{ row }">
            <span class="wc-mono">{{ row.hitCount > 0 ? row.hitCount.toLocaleString('en-US') : '—' }}</span>
          </template>
          <template #cell-enabled="{ row }">
            <StatusTag :status="row.enabled ? 'success' : 'inactive'" :text="row.enabled ? '启用' : '停用'" />
          </template>
          <template #cell-lastHitAt="{ row }">
            <span class="wc-mono">{{ row.lastHitAt && row.lastHitAt !== '—' ? row.lastHitAt : '—' }}</span>
          </template>
          <template #actions="{ row }">
            <button
              type="button"
              class="wc-btn wc-btn--sm"
              :data-testid="`rule-edit-${row.id}`"
              @click="editRule(row)"
            >
              编辑
            </button>
            <button
              type="button"
              class="wc-btn wc-btn--sm"
              :disabled="!canEdit"
              :data-testid="`rule-toggle-${row.id}`"
              @click="toggleRule(row)"
            >
              {{ row.enabled ? '停用' : '启用' }}
            </button>
            <button
              type="button"
              class="wc-btn wc-btn--sm wc-btn--danger"
              :disabled="!canEdit"
              :data-testid="`rule-delete-${row.id}`"
              @click="askDelete(row)"
            >
              删除
            </button>
          </template>
        </UiTable>

        <UiPager :page="rulePage" :total="filteredRules.length" :page-size="RULE_PAGE_SIZE" @update:page="onRulePage" />
      </template>
    </section>
  </div>

  <!-- ══ 新增 / 编辑规则弹窗（原内联编辑区整体收进此处）════════════════ -->
  <Teleport to="body">
    <div v-show="modalOpen" class="wc-modal__mask" data-testid="rule-modal" @click.self="closeModal">
      <div
        class="wc-modal"
        role="dialog"
        aria-modal="true"
        :aria-label="editingId ? '编辑规则' : '新增规则'"
        tabindex="-1"
        @keydown.esc="closeModal"
      >
        <div class="wc-modal__head">
          <h3 class="wc-modal__title">{{ editingId ? `编辑规则 · ${editingName}` : '新增规则' }}</h3>
        </div>
        <div class="wc-modal__body">
          <UiField label="规则名称" required>
            <UiInput v-model="form.name" :disabled="!canEdit" placeholder="如：质量过滤：仅转发 Good" data-testid="rule-name" />
          </UiField>

          <div class="rb-row">
            <UiField label="规则状态" hint="停用仅保存不生效，需再次启用">
              <UiSelect v-model="form.enabledText" :disabled="!canEdit" :options="ENABLE_OPTIONS" />
            </UiField>
            <UiField label="优先级" hint="数字越小越先匹配">
              <UiInput v-model="form.priority" type="number" :disabled="!canEdit" placeholder="1" />
            </UiField>
          </div>

          <UiField label="生效出口 id" hint="网关配置文件 [[outlets]] 的出口名；留空 = 未绑定出口">
            <UiInput v-model="form.forwarderId" :disabled="!canEdit" placeholder="如：factory-01" />
          </UiField>

          <!-- 条件：逐条比较，多条之间按「且」组合；留空 = 恒真 -->
          <UiField
            label="条件过滤"
            hint="多条条件之间按「且」组合；全部留空则规则对所有数据生效（下发 when = null）"
          >
            <div class="rb-conds">
              <div v-for="(row, i) in form.conditions" :key="i" class="rb-cond">
                <UiInput v-model="row.field" :disabled="!canEdit" placeholder="字段，如 quality" />
                <UiSelect v-model="row.op" :disabled="!canEdit" :options="CMP_OP_OPTIONS" />
                <UiInput v-model="row.value" :disabled="!canEdit" placeholder="比较值，如 Bad" />
                <button
                  type="button"
                  class="wc-btn wc-btn--sm"
                  :disabled="!canEdit"
                  data-testid="rule-condition-remove"
                  @click="removeCondition(i)"
                >
                  删除
                </button>
              </div>
              <button
                type="button"
                class="wc-btn wc-btn--sm"
                :disabled="!canEdit"
                data-testid="rule-condition-add"
                @click="addCondition"
              >
                + 增加条件
              </button>
            </div>
          </UiField>

          <!-- 动作：对齐后端 Action 契约（publish / remap） -->
          <UiField label="动作" required hint="至少一项；后端业务校验会拒绝「无动作」的规则">
            <div class="rb-acts">
              <div v-for="(act, i) in form.actions" :key="i" class="rb-act">
                <div class="rb-act__head">
                  <UiSelect v-model="act.kind" :disabled="!canEdit" :options="ACTION_KIND_OPTIONS" />
                  <button
                    type="button"
                    class="wc-btn wc-btn--sm"
                    :disabled="!canEdit"
                    data-testid="rule-action-remove"
                    @click="form.actions.splice(i, 1)"
                  >
                    删除动作
                  </button>
                </div>
                <UiInput
                  v-if="act.kind === 'publish'"
                  v-model="act.topic"
                  :disabled="!canEdit"
                  placeholder="下发 topic，如 factory/injection/line1"
                />
                <div v-else class="rb-map">
                  <div v-for="(f, j) in act.fields" :key="j" class="rb-map__row">
                    <UiInput v-model="f.key" :disabled="!canEdit" placeholder="输出字段名" />
                    <span class="rb-map__arrow" aria-hidden="true">←</span>
                    <UiInput v-model="f.path" :disabled="!canEdit" placeholder="源字段，如 T_Barrel1" />
                    <button
                      type="button"
                      class="wc-btn wc-btn--sm"
                      :disabled="!canEdit"
                      @click="act.fields.splice(j, 1)"
                    >
                      删除
                    </button>
                  </div>
                  <button
                    type="button"
                    class="wc-btn wc-btn--sm"
                    :disabled="!canEdit"
                    @click="act.fields.push({ key: '', path: '' })"
                  >
                    + 增加映射
                  </button>
                </div>
              </div>
              <button
                type="button"
                class="wc-btn wc-btn--sm"
                :disabled="!canEdit"
                data-testid="rule-action-add"
                @click="form.actions.push({ kind: 'publish', topic: '', fields: [] })"
              >
                + 增加动作
              </button>
            </div>
          </UiField>

          <UiField label="输出字段白名单（select）" hint="逗号分隔；留空 = 输出全部字段">
            <UiInput v-model="form.select" :disabled="!canEdit" placeholder="如：barrelTemp,injPressure" />
          </UiField>

          <!-- 真实错误 / 校验失败原因（唯一允许留在弹窗里的提示） -->
          <p v-if="unsupportedEdit" class="wc-modal__error" role="alert" data-testid="rule-unsupported">
            {{ unsupportedEdit }}
          </p>
          <p v-if="saveMessage" class="wc-modal__error" role="alert" data-testid="rule-save-message">
            {{ saveMessage }}
          </p>

          <!-- 折叠帮助：校验与版本边界（主视图不出现） -->
          <details class="rb-help">
            <summary>校验与版本说明</summary>
            <p class="rb-help__line">
              网关未提供独立规则校验接口；保存时会由后端 <code>validate_rule_set</code> 做配置级校验，
              结果以保存请求的返回为准。
            </p>
            <p class="rb-help__line">网关未提供规则版本历史与回滚接口，此页不提供规则回滚；如需回退配置，请到「备份与恢复」。</p>
          </details>
        </div>
        <div class="wc-modal__foot">
          <button type="button" class="wc-btn" data-testid="rule-cancel" @click="closeModal">取消</button>
          <button
            type="button"
            class="wc-btn wc-btn--primary"
            :disabled="!canEdit || Boolean(unsupportedEdit)"
            data-testid="rule-save"
            @click="saveRule"
          >
            保存
          </button>
        </div>
      </div>
    </div>
  </Teleport>

  <!-- ══ 删除规则：危险操作二次确认（影响 + 原因 + 对象全名二次校验）═══ -->
  <DangerConfirmModal
    :open="deleteOpen"
    :title="`删除规则 ${deleteTarget?.name ?? ''}`"
    :impacts="DELETE_IMPACTS"
    :facts="deleteFacts"
    :reasons="DELETE_REASONS"
    :min-note-length="10"
    :confirm-value="deleteTarget?.name ?? ''"
    confirm-mode="full"
    confirm-label="规则名二次确认（输入规则名称）"
    confirm-placeholder="输入待删除的规则名称"
    confirm-text="删除规则"
    @close="deleteOpen = false"
    @submit="onDeleteSubmit"
  />
</template>

<script setup lang="ts">
/**
 * @file RulesPage.vue
 * @module web-console/pages/RulesPage
 * @description 转发规则页（规则列表 + 搜索 + 弹窗式结构化编辑 + 危险删除确认）。
 *
 * 契约边界（全部对齐 `crates/daemon/src/mgmt/rules_api.rs`）：
 * · 真实端点 `GET /api/rules`、`POST /api/rules`、`PUT /api/rules/:id`、`DELETE /api/rules/:id`；
 * · 条件 / 动作均为**结构化对象**，不下发任何条件字符串或规则体 JSON；
 * · 网关无 dry-run 端点、无版本历史端点、无规则回滚端点 —— 相关 UI 不渲染 / 不提供。
 */
import { computed, onMounted, reactive, ref, watch } from 'vue';
import {
  UiTable,
  UiPager,
  UiInput,
  UiSelect,
  UiField,
  StatusTag,
  EmptyState,
  RoleGate,
  DangerConfirmModal,
  type TableColumn,
  type DangerFact,
  type SelectOption,
} from '@ui-kit';
import {
  repo,
  coerceRuleConditionValue,
  type RuleRecord,
  type RuleDetail,
  type RuleCondition,
  type RuleAction,
  type RuleCmpOp,
} from '@/api/repo';
import { session } from '../store/session';

/** 每页条数。 */
const RULE_PAGE_SIZE = 5;

/** 当前角色是否可编辑。 */
const canEdit = computed<boolean>(() => session.state.role === 'admin' || session.state.role === 'engineer');

// ---------- 列表 + 搜索 ----------
const rules = ref<RuleRecord[]>([]);
const query = ref('');
const rulePage = ref(1);

/** 数据源不可得的真实原因（`repo.rules.list()` 的 notice 原样展示）。 */
const rulesNotice = ref('');

/** 启停 / 删除操作的真实回执。 */
const operationMessage = ref('');

/** 规则总数（条数只有分页条一个口径）。 */
const ruleTotal = computed<number>(() => rules.value.length);

/** 按关键词过滤（名称 / 生效出口 / 条件 / 动作）。 */
const filteredRules = computed<RuleRecord[]>(() => {
  const kw = query.value.trim().toLowerCase();
  if (!kw) {
    return rules.value;
  }
  return rules.value.filter(
    (r) =>
      r.name.toLowerCase().includes(kw) ||
      r.forwarderName.toLowerCase().includes(kw) ||
      r.condition.toLowerCase().includes(kw) ||
      r.action.toLowerCase().includes(kw),
  );
});

/** 当前页规则。 */
const pagedRules = computed<RuleRecord[]>(() => {
  const start = (rulePage.value - 1) * RULE_PAGE_SIZE;
  return filteredRules.value.slice(start, start + RULE_PAGE_SIZE);
});

/** 搜索变化时回到第 1 页。 */
watch(query, () => {
  rulePage.value = 1;
});

/** 列定义。 */
const ruleColumns: readonly TableColumn[] = [
  { key: 'name', label: '规则名称' },
  { key: 'forwarderName', label: '生效出口' },
  { key: 'condition', label: '条件', mono: true },
  { key: 'action', label: '动作', mono: true },
  { key: 'priority', label: '优先级', align: 'right', mono: true },
  { key: 'hitCount', label: '命中次数', align: 'right', mono: true },
  { key: 'enabled', label: '状态' },
  { key: 'lastHitAt', label: '最近命中', mono: true },
];

/** 换页。 */
function onRulePage(next: number): void {
  rulePage.value = next;
}

/** 拉取规则清单（`GET /api/rules`，兼容数组与 `{items, reason}` 两种形状）。 */
async function loadRules(): Promise<void> {
  const res = await repo.rules.list();
  rules.value = res.items;
  rulesNotice.value = res.notice;
  if (rulePage.value > totalPages.value) {
    rulePage.value = Math.max(1, totalPages.value);
  }
}

onMounted(() => {
  void loadRules();
});

/** 总页数。 */
const totalPages = computed<number>(() => Math.max(1, Math.ceil(filteredRules.value.length / RULE_PAGE_SIZE)));

// ---------- 编辑表单（结构化，对齐 RuleUpsertBody；收进弹窗） ----------
/** 条件行。 */
interface ConditionRow {
  /** 比较字段 */
  field: string;
  /** 比较符（空 = 本行未填完整） */
  op: RuleCmpOp | '';
  /** 比较值（原文，下发时按大数红线口径转换） */
  value: string;
}

/** 动作草稿。 */
interface ActionDraft {
  kind: 'publish' | 'remap';
  /** `publish` 的 topic */
  topic: string;
  /** `remap` 的字段映射行 */
  fields: { key: string; path: string }[];
}

/** 表单草稿。 */
const form = reactive({
  name: '',
  forwarderId: '',
  priority: '1',
  enabledText: 'true',
  conditions: [] as ConditionRow[],
  actions: [] as ActionDraft[],
  select: '',
});

/** 规则状态下拉（SelectOption 取值为字符串，提交时再转 boolean）。 */
const ENABLE_OPTIONS: readonly SelectOption[] = [
  { value: 'true', label: '启用' },
  { value: 'false', label: '停用' },
];

/** 比较符下拉（后端 `CmpOp` 的 wire 值）。 */
const CMP_OP_OPTIONS: readonly SelectOption[] = [
  { value: 'gt', label: '> 大于' },
  { value: 'ge', label: '>= 大于等于' },
  { value: 'lt', label: '< 小于' },
  { value: 'le', label: '<= 小于等于' },
  { value: 'eq', label: '= 等于' },
  { value: 'ne', label: '≠ 不等于' },
];

/** 动作类型下拉。 */
const ACTION_KIND_OPTIONS: readonly SelectOption[] = [
  { value: 'publish', label: '发布（publish）：下发到 topic' },
  { value: 'remap', label: '重映射（remap）：改写输出字段名' },
];

/** 保存提示（真实错误 / 回执，留在弹窗内）。 */
const saveMessage = ref('');

/** 当前条件组合无法在界面上编辑时的诚实原因（非空即禁用保存）。 */
const unsupportedEdit = ref('');

// ---------- 弹窗状态 ----------
/** 弹窗是否打开（v-show：关闭后保留节点于 DOM，display:none）。 */
const modalOpen = ref(false);

/** 当前编辑的规则 id（空串 = 新增）。 */
const editingId = ref('');
const editingName = ref('');

/** 打开「新增规则」弹窗（空表单）。 */
function startNewRule(): void {
  editingId.value = '';
  editingName.value = '';
  saveMessage.value = '请填写新规则并保存。';
  unsupportedEdit.value = '';
  applyDetail(null);
  modalOpen.value = true;
}

/** 关闭弹窗并清空提示。 */
function closeModal(): void {
  modalOpen.value = false;
  saveMessage.value = '';
}

/** 启用 / 停用规则（写失败时如实呈现原因，绝不假装已切换）。 */
async function toggleRule(row: RuleRecord): Promise<void> {
  if (!canEdit.value) {
    return;
  }
  const result = await repo.setRuleEnabled({
    id: row.id,
    enabled: !row.enabled,
    actor: session.state.displayName,
  });
  if (result.ok) {
    operationMessage.value = result.message;
  } else {
    operationMessage.value = `规则「${row.name}」未变更：${result.message}`;
  }
  await loadRules();
}

/**
 * 结构化条件 → 条件行（扁平化 and / or）。
 *
 * 遇到 `not` 或其它组合方式时**如实上报不支持**，不做降级改写，避免下发语义与读到的不一致。
 */
function conditionRows(when: RuleCondition | null): ConditionRow[] {
  if (!when) {
    return [];
  }
  if (when.kind === 'cmp') {
    return [{ field: when.field, op: when.op, value: String(when.value) }];
  }
  if (when.kind === 'and' || when.kind === 'or') {
    const rows: ConditionRow[] = [];
    for (const inner of when.conditions) {
      rows.push(...conditionRows(inner));
    }
    return rows;
  }
  unsupportedEdit.value =
    `该规则的条件含 ${when.kind} 组合，页面只支持逐条比较（cmp）与 and / or 组合，` +
    '无法在界面上编辑；请到网关配置文件修改后再回看。';
  return [];
}

/** 空条件行。 */
function emptyCondition(): ConditionRow {
  return { field: '', op: 'ne', value: '' };
}

function addCondition(): void {
  form.conditions.push(emptyCondition());
}

function removeCondition(index: number): void {
  form.conditions.splice(index, 1);
}

/** 动作草稿 → 结构化动作（丢弃空行，绝不下发空动作）。 */
function buildActions(): RuleAction[] {
  const out: RuleAction[] = [];
  for (const act of form.actions) {
    if (act.kind === 'publish') {
      const topic = act.topic.trim();
      if (topic) {
        out.push({ kind: 'publish', topic });
      }
      continue;
    }
    const fields: Record<string, string> = {};
    for (const f of act.fields) {
      const key = f.key.trim();
      const path = f.path.trim();
      if (key && path) {
        fields[key] = path;
      }
    }
    if (Object.keys(fields).length > 0) {
      out.push({ kind: 'remap', fields });
    }
  }
  return out;
}

/** 把一条真实规则灌入表单。 */
function applyDetail(detail: RuleDetail | null): void {
  unsupportedEdit.value = '';
  form.name = detail?.name ?? '';
  form.forwarderId = detail?.forwarderId ?? '';
  form.priority = String(detail?.priority ?? 1);
  form.enabledText = detail?.enabled === false ? 'false' : 'true';
  form.select = (detail?.select ?? []).join(', ');
  if (detail) {
    form.conditions = conditionRows(detail.when);
    form.actions =
      detail.actions.length > 0
        ? detail.actions.map((a) =>
            a.kind === 'publish'
              ? { kind: 'publish' as const, topic: a.topic, fields: [] }
              : { kind: 'remap' as const, topic: '', fields: Object.keys(a.fields).map((k) => ({ key: k, path: a.fields[k] ?? '' })) },
          )
        : [{ kind: 'publish', topic: '', fields: [] }];
  } else {
    form.conditions = [emptyCondition()];
    form.actions = [{ kind: 'publish', topic: '', fields: [] }];
  }
}

/** 行内「编辑」：拉取结构化字段后打开预填弹窗。 */
async function editRule(row: RuleRecord): Promise<void> {
  if (!canEdit.value) {
    return;
  }
  saveMessage.value = '';
  unsupportedEdit.value = '';
  const res = await repo.rules.list();
  const detail = res.items.find((r) => r.id === row.id) ?? null;
  editingId.value = detail?.id ?? row.id;
  editingName.value = row.name;
  applyDetail(detail);
  if (!detail) {
    saveMessage.value = `已载入「${row.name}」，但取不到其结构化字段（when / actions）：${
      res.notice || '网关未返回该规则的结构化条件与动作。'
    }`;
  }
  modalOpen.value = true;
}

/** 表单层面的必填校验（只做前端防御，最终结果一律以后端返回为准）。 */
function formProblem(): string {
  if (!form.name.trim()) {
    return '规则名称不能为空。';
  }
  const complete = form.conditions.filter((r) => r.field.trim() && r.op && r.value.trim());
  if (complete.length === 0 && form.conditions.length > 0) {
    return '条件还没有填完整：请补全每条条件的字段 / 比较符 / 比较值，或直接删掉留空的行（留空 = 不下发条件）。';
  }
  if (buildActions().length === 0) {
    return '规则至少需要一个动作：请填写 publish 的 topic，或至少一条 remap 字段映射。';
  }
  return '';
}

/**
 * 完整条件行 → 结构化条件（对齐后端 `Condition`）。
 *
 * 单行直接下发 `cmp`；多行按 `and` 组合；一行都没有 = `null`（恒真，不下发条件对象）。
 */
function buildWhen(complete: readonly ConditionRow[]): RuleCondition | null {
  const first = complete[0];
  if (!first) {
    return null;
  }
  const toCmp = (r: ConditionRow): RuleCondition => ({
    kind: 'cmp',
    field: r.field.trim(),
    op: r.op as RuleCmpOp,
    value: coerceRuleConditionValue(r.value),
  });
  return complete.length === 1 ? toCmp(first) : { kind: 'and', conditions: complete.map(toCmp) };
}

/** 保存：新增走 `POST /api/rules`，编辑走 `PUT /api/rules/:id`；成败都不编造。 */
async function saveRule(): Promise<void> {
  if (!canEdit.value || unsupportedEdit.value) {
    return;
  }
  const problem = formProblem();
  if (problem) {
    saveMessage.value = problem;
    return;
  }
  const actions = buildActions();
  const complete = form.conditions.filter((r) => r.field.trim() && r.op && r.value.trim());
  const input = {
    name: form.name.trim(),
    forwarderId: form.forwarderId.trim(),
    enabled: form.enabledText !== 'false',
    priority: Number(form.priority.trim()) || 0,
    when: buildWhen(complete),
    actions,
    select: form.select
      .split(',')
      .map((s) => s.trim())
      .filter((s) => s !== ''),
  };
  const result =
    editingId.value === ''
      ? await repo.rules.create(input)
      : await repo.rules.update({ ...input, id: editingId.value });
  if (result.ok) {
    // 成功：关弹窗 + 刷新列表，回执进主视图操作提示。
    closeModal();
    operationMessage.value = result.message;
    await loadRules();
  } else {
    // 失败：错误留在弹窗内显示，绝不回退成内联展开，也绝不假装成功。
    saveMessage.value = `规则「${input.name}」未保存：${result.message}`;
  }
}

// ---------------------------------------------------------------------------
// 删除规则（危险操作二次确认；`DELETE /api/rules/:id`，body 带 reason / note / confirm）
// ---------------------------------------------------------------------------
const deleteOpen = ref(false);
const deleteTarget = ref<RuleRecord | null>(null);

/** 删除影响清单。 */
const DELETE_IMPACTS: readonly string[] = [
  '该规则将从网关配置移除，命中该规则的数据不再按其路由。',
  '请确认没有其它规则依赖该规则（depends_on）。',
  '删除记录写入审计日志，但规则内容不可恢复。',
];

/** 删除对象摘要。 */
const deleteFacts = computed<readonly DangerFact[]>(() => [
  { label: '规则名称', value: deleteTarget.value?.name ?? '—' },
  { label: '生效出口', value: deleteTarget.value?.forwarderName ?? '—' },
]);

/** 删除原因枚举（必选）。 */
const DELETE_REASONS: readonly string[] = ['规则不再需要', '条件配置错误', '路由目标变更', '调试清理'];

/** 打开删除确认。 */
function askDelete(row: RuleRecord): void {
  if (!canEdit.value) {
    return;
  }
  deleteTarget.value = row;
  operationMessage.value = '';
  deleteOpen.value = true;
}

/** 删除提交：真实 `DELETE /api/rules/:id`，失败原样报错。 */
async function onDeleteSubmit(payload: { reason: string; note: string; tail: string; secondApprover: string }): Promise<void> {
  const row = deleteTarget.value;
  deleteOpen.value = false;
  if (!row) {
    return;
  }
  const result = await repo.rules.remove({
    id: row.id,
    reason: payload.reason,
    note: payload.note,
    confirm: row.name,
  });
  operationMessage.value = result.ok
    ? result.message
    : `规则「${row.name}」未删除：${result.message}`;
  await loadRules();
}
</script>

<style scoped>
.rb-toolbar {
  display: flex;
  gap: 10px;
  align-items: center;
  padding: 12px 16px 0;
}
.rb-toolbar .wc-input {
  flex: 1 1 auto;
  max-width: 360px;
}
.wc-card > .wc-banner {
  margin: 12px 16px 0;
}
.wc-card > .wc-banner:last-child {
  margin-bottom: 12px;
}
.rb-row {
  display: grid;
  grid-template-columns: 1fr 1fr;
  gap: 12px;
}
.rb-conds,
.rb-acts {
  display: flex;
  flex-direction: column;
  gap: 8px;
}
.rb-cond,
.rb-act__head {
  display: flex;
  align-items: flex-start;
  gap: 8px;
}
.rb-cond > * {
  min-width: 0;
}
.rb-act {
  display: flex;
  flex-direction: column;
  gap: 8px;
  border: 1px solid var(--border);
  border-radius: var(--radius-sm);
  padding: 8px;
}
.rb-map {
  display: flex;
  flex-direction: column;
  gap: 6px;
}
.rb-map__row {
  display: flex;
  align-items: center;
  gap: 6px;
}
.rb-map__row > * {
  min-width: 0;
}
.rb-map__arrow {
  color: var(--text-3);
  flex: 0 0 auto;
}
/* 折叠帮助（编辑弹窗内，主视图不出现） */
.rb-help {
  border: 1px solid var(--divider);
  border-radius: var(--radius-sm);
  padding: 8px 12px;
  display: flex;
  flex-direction: column;
  gap: 6px;
}
.rb-help summary {
  cursor: pointer;
  font-size: var(--fs-table);
  color: var(--text-2);
  user-select: none;
}
.rb-help__line {
  margin: 0;
  font-size: var(--fs-caption);
  color: var(--text-3);
  line-height: 1.7;
}
/* ── 弹窗（与 AccountsPage 同一模式；随组件作用域，不改全局样式）──── */
.wc-modal__mask {
  position: fixed;
  inset: 0;
  background: #1d212973;
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
  width: 720px;
  max-width: 100%;
  max-height: 88vh;
  overflow: auto;
  display: flex;
  flex-direction: column;
  outline: none;
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
  gap: 14px;
}
.wc-modal__error {
  margin: 0;
  font-size: var(--fs-table);
  color: var(--danger-fg);
  background: var(--danger-bg);
  border: 1px solid var(--danger-border);
  border-radius: var(--radius-sm);
  padding: 8px 10px;
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

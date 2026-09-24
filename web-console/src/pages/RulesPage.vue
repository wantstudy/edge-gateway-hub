<!--
  =============================================================================
  RulesPage —— 转发规则（设计 §3.4 关联 / 原型 gateway-v2a-glacier「rules」）
  =============================================================================
  声明式规则：来源 / 条件 / 动作 / 优先级；命中即路由。支持规则启用停用、编辑、
  优先级排序、校验步骤（语法 / 字段引用 / 环检测 / dry-run）与版本回滚。
  列表分页（UiPager，条数只此一个口径）。
-->
<template>
  <PageHeader
    crumb="分发 / 转发规则"
    title="转发规则"
    desc="声明式规则：点位映射、单位换算、死区过滤、Topic 路由。按优先级从小到大匹配，命中即路由。支持规则版本与回滚。"
  >
    <template #actions>
      <RoleGate :allowed="canEdit" mode="disable" deny-text="当前角色为只读，不能新增规则">
        <button type="button" class="wc-btn wc-btn--primary" @click="startNewRule">新增规则</button>
      </RoleGate>
    </template>
  </PageHeader>

  <div class="wc-content">
    <!-- ══ 规则列表（分页）═══════════════════════════════════════════════ -->
    <section class="wc-card">
      <div class="wc-card__head">
        <h3>规则列表</h3>
        <span class="wc-card__sub">按优先级从小到大匹配，命中即路由 · 共 {{ ruleTotal }} 条</span>
      </div>

      <EmptyState
        v-if="ruleTotal === 0"
        title="还没有转发规则"
        desc="未配置规则时，出口会转发全部点位。若需按质量 / 死区 / 路由过滤数据，请新建第一条规则。"
      >
        <template #actions>
          <button type="button" class="wc-btn wc-btn--primary" @click="startNewRule">新增规则</button>
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
            <span class="wc-mono">{{ row.hitCount.toLocaleString('en-US') }}</span>
          </template>
          <template #cell-enabled="{ row }">
            <StatusTag :status="row.enabled ? 'success' : 'inactive'" :text="row.enabled ? '启用' : '停用'" />
          </template>
          <template #cell-lastHitAt="{ row }">
            <span class="wc-mono">{{ row.lastHitAt }}</span>
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
          </template>
        </UiTable>

        <UiPager :page="rulePage" :total="ruleTotal" :page-size="RULE_PAGE_SIZE" @update:page="onRulePage" />
      </template>
    </section>

    <!-- ══ 规则编辑 + 校验与版本 ════════════════════════════════════════ -->
    <div class="wc-grid wc-grid--2">
      <section class="wc-card">
        <div class="wc-card__head">
          <h3>规则编辑</h3>
          <span v-if="editingName" class="wc-card__sub">当前：{{ editingName }}</span>
        </div>
        <div class="wc-card__body">
          <UiField label="规则名称" required>
            <UiInput v-model="form.name" :disabled="!canEdit" placeholder="如：质量过滤：仅转发 Good" />
          </UiField>
          <UiField
            label="条件表达式"
            hint="支持 quality / value / deadband / ts 等内置字段"
          >
            <UiInput v-model="form.condition" :disabled="!canEdit" placeholder="quality != Bad && deadband > 0.5" />
          </UiField>
          <UiField label="路由目标">
            <UiInput v-model="form.routeTarget" :disabled="!canEdit" placeholder="factory/injection/line1" />
          </UiField>
          <UiField label="规则体（JSON）" hint="字段引用不存在 / 存在环时校验会给出具体路径">
            <textarea
              v-model="form.body"
              class="rb-textarea"
              rows="7"
              :disabled="!canEdit"
              spellcheck="false"
            />
          </UiField>
          <div style="display: flex; gap: 8px">
            <button
              type="button"
              class="wc-btn wc-btn--primary"
              :disabled="!canEdit"
              data-testid="rule-save"
              @click="saveRule"
            >
              保存并校验
            </button>
            <button type="button" class="wc-btn" :disabled="!canEdit" @click="runDryRun">试运行</button>
          </div>
          <p v-if="saveMessage" class="wc-hint" data-testid="rule-save-message">{{ saveMessage }}</p>
        </div>
      </section>

      <section class="wc-card">
        <div class="wc-card__head">
          <h3>校验与版本</h3>
        </div>
        <div class="wc-card__body">
          <ol class="rb-steps">
            <li v-for="step in validationSteps" :key="step.n" class="rb-step" :class="`rb-step--${step.state}`">
              <span class="rb-step__n">{{ step.n }}</span>
              <div class="rb-step__body">
                <span class="rb-step__label">{{ step.label }}</span>
                <span v-if="step.desc" class="rb-step__desc">{{ step.desc }}</span>
              </div>
              <span class="rb-step__time">{{ step.time }}</span>
            </li>
          </ol>

          <div class="wc-list">
            <div v-for="(ver, i) in versions" :key="ver.version" class="wc-list__item">
              <div>
                <div class="wc-list__title">{{ ver.version }}<span v-if="i === 0" class="rb-cur">当前</span></div>
                <div class="wc-list__desc">{{ ver.desc }}</div>
              </div>
              <div class="wc-list__ops">
                <span class="wc-mono wc-card__sub">{{ ver.time }}</span>
                <button
                  v-if="i > 0"
                  type="button"
                  class="wc-btn wc-btn--sm"
                  :disabled="!canEdit"
                  @click="rollback(ver.version)"
                >
                  回滚到此版本
                </button>
              </div>
            </div>
          </div>

          <p class="wc-note">
            <span class="wc-note__icon">i</span>
            <span>规则变更写入审计（含改前改后全文），纳入配置版本可一键回滚。</span>
          </p>
        </div>
      </section>
    </div>
  </div>

  <!-- 回滚二次确认（危险操作） -->
  <DangerConfirmModal
    :open="rollbackOpen"
    :title="`回滚规则到 ${rollbackTarget}`"
    :impacts="rollbackImpacts"
    :facts="rollbackFacts"
    :reasons="ROLLBACK_REASONS"
    :min-note-length="10"
    :confirm-value="editingId"
    confirm-label="对象二次校验（输入规则 id 后 8 位）"
    :confirm-text="`回滚到 ${rollbackTarget}`"
    @close="rollbackOpen = false"
    @submit="onRollbackSubmit"
  />
</template>

<script setup lang="ts">
/**
 * @file RulesPage.vue
 * @module web-console/pages/RulesPage
 * @description 转发规则页（规则列表分页 + 编辑 + 校验步骤 + 版本回滚危险确认）。
 */
import { computed, reactive, ref } from 'vue';
import {
  PageHeader,
  UiTable,
  UiPager,
  UiInput,
  UiField,
  StatusTag,
  EmptyState,
  RoleGate,
  DangerConfirmModal,
  type TableColumn,
  type DangerFact,
} from '@ui-kit';
import { repo, DEFAULT_ACTOR, type RuleRecord } from '../mock/mock-data';
import { session } from '../store/session';

/** 每页条数。 */
const RULE_PAGE_SIZE = 5;

/** 当前角色是否可编辑。 */
const canEdit = computed<boolean>(() => session.state.role === 'admin' || session.state.role === 'engineer');

// ---------- 列表 ----------
const rules = ref<RuleRecord[]>(repo.allRules());

/** 规则总数（条数只有分页条一个口径）。 */
const ruleTotal = computed<number>(() => rules.value.length);

const rulePage = ref(1);

/** 当前页规则。 */
const pagedRules = computed<RuleRecord[]>(() => {
  const start = (rulePage.value - 1) * RULE_PAGE_SIZE;
  return rules.value.slice(start, start + RULE_PAGE_SIZE);
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

/** 当前编辑的规则 id（用于危险弹窗的二次校验）。 */
const editingId = ref('rule-001');
const editingName = ref('质量过滤：仅转发 Good');

/** 启用 / 停用规则。 */
function toggleRule(row: RuleRecord): void {
  if (!canEdit.value) {
    return;
  }
  const ok = repo.setRuleEnabled({ id: row.id, enabled: !row.enabled, actor: DEFAULT_ACTOR });
  if (ok) {
    rules.value = repo.allRules();
  }
}

// ---------- 编辑表单 ----------
/** 编辑表单草稿。 */
const form = reactive({
  name: '质量过滤：仅转发 Good',
  condition: 'quality != Bad && deadband > 0.5',
  routeTarget: 'factory/injection/line1',
  body:
    '{\n  "map": { "T_Barrel1": "barrelTemp", "P_Inj": "injPressure" },\n' +
    '  "unit": { "barrelTemp": "℃", "injPressure": "MPa" },\n  "deadband": 0.5\n}',
});

/** 保存提示。 */
const saveMessage = ref('');

/** 选中某条规则进入编辑区。 */
function editRule(row: RuleRecord): void {
  editingId.value = row.id;
  editingName.value = row.name;
  form.name = row.name;
  form.condition = row.condition;
  form.routeTarget = row.action;
  saveMessage.value = `已载入规则「${row.name}」。`;
}

/** 新增规则（清空表单）。 */
function startNewRule(): void {
  editingId.value = '';
  editingName.value = '';
  form.name = '';
  form.condition = '';
  form.routeTarget = '';
  form.body = '{\n  "map": {}\n}';
  saveMessage.value = '请填写新规则并保存校验。';
}

/** 保存并校验。 */
function saveRule(): void {
  if (!canEdit.value) {
    return;
  }
  if (!form.name.trim()) {
    saveMessage.value = '规则名称不能为空。';
    return;
  }
  saveMessage.value = `规则「${form.name}」校验通过：语法 / 字段引用 / 组合检测均无错误，可保存。`;
}

/** 试运行。 */
function runDryRun(): void {
  saveMessage.value = `试运行完成：取样 100 条历史数据，命中 87 条，路由到 ${form.routeTarget}。`;
}

// ---------- 校验步骤 ----------
/** 校验步骤（照搬原型）。 */
const validationSteps = [
  { n: '1', state: 'done', label: '语法校验通过', desc: '', time: '3 ms' },
  { n: '2', state: 'done', label: '字段引用存在性校验', desc: '6 个字段全部命中点位表', time: '2 ms' },
  { n: '3', state: 'done', label: '组合与循环检测', desc: '无环、无重复路由', time: '1 ms' },
  { n: '4', state: 'run', label: '试运行（dry-run）', desc: '取样 100 条历史数据', time: '进行中' },
] as const;

// ---------- 版本 ----------
/** 规则版本历史。 */
const versions = [
  { version: 'v4', desc: 'deadband 0.2 → 0.5', time: '09-21 14:02' },
  { version: 'v3', desc: '新增 injPressure 映射', time: '09-18 09:31' },
  { version: 'v2', desc: '路由改为 line1', time: '09-15 16:20' },
];

// ---------- 回滚危险确认 ----------
const rollbackOpen = ref(false);
const rollbackTarget = ref('');

/** 回滚影响清单。 */
const rollbackImpacts: readonly string[] = [
  '当前规则内容将被所选历史版本完全覆盖，未保存的编辑会丢失。',
  '规则立即生效，命中路由可能变化，正在补发的数据按新规则重新路由。',
  '回滚本身写入审计，可从版本历史再次回滚。',
];

/** 回滚对象摘要。 */
const rollbackFacts = computed<readonly DangerFact[]>(() => [
  { label: '规则名称', value: form.name || editingName.value || '—' },
  { label: '目标版本', value: rollbackTarget.value || '—' },
  { label: '当前版本', value: 'v4' },
]);

/** 回滚原因枚举（必选）。 */
const ROLLBACK_REASONS: readonly string[] = ['规则导致误路由', '配置写错需恢复', '业务需求变更', '调试回退'];

/** 打开回滚确认。 */
function rollback(version: string): void {
  if (!canEdit.value) {
    return;
  }
  rollbackTarget.value = version;
  rollbackOpen.value = true;
}

/** 回滚提交。 */
function onRollbackSubmit(payload: { reason: string; note: string; tail: string; secondApprover: string }): void {
  rollbackOpen.value = false;
  saveMessage.value = `已回滚到 ${rollbackTarget.value}（原因：${payload.reason}），变更已写入审计。`;
}
</script>

<style scoped>
.rb-textarea {
  font-family: var(--font-mono);
  font-size: var(--fs-caption);
  line-height: 1.6;
  width: 100%;
  box-sizing: border-box;
  padding: 8px 10px;
  border: 1px solid var(--border);
  border-radius: var(--radius-sm);
  background: #fff;
  color: var(--text-1);
  resize: vertical;
  outline: none;
}
.rb-textarea:focus {
  border-color: var(--brand);
}
.rb-textarea:disabled {
  background: var(--divider);
  color: var(--text-3);
}
.rb-steps {
  margin: 0;
  padding: 0;
  list-style: none;
  display: flex;
  flex-direction: column;
  gap: 10px;
}
.rb-step {
  display: flex;
  align-items: flex-start;
  gap: 10px;
}
.rb-step__n {
  width: 20px;
  height: 20px;
  flex: 0 0 20px;
  border-radius: 50%;
  display: flex;
  align-items: center;
  justify-content: center;
  font-size: 11px;
  font-weight: 600;
  background: var(--divider);
  color: var(--text-3);
}
.rb-step--done .rb-step__n {
  background: var(--ok-bg);
  color: var(--ok-fg);
}
.rb-step--run .rb-step__n {
  background: var(--info-bg);
  color: var(--info-fg);
}
.rb-step__body {
  display: flex;
  flex-direction: column;
  flex: 1;
  min-width: 0;
}
.rb-step__label {
  font-size: var(--fs-table);
  color: var(--text-1);
}
.rb-step__desc {
  font-size: var(--fs-caption);
  color: var(--text-3);
}
.rb-step__time {
  font-size: var(--fs-caption);
  color: var(--text-3);
  white-space: nowrap;
}
.rb-cur {
  margin-left: 8px;
  font-size: var(--fs-caption);
  color: var(--brand);
  font-weight: 500;
}
</style>

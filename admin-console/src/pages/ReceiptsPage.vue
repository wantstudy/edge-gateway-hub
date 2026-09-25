<template>
  <!--
    ReceiptsPage —— 回执与异常（页面清单第 7 项，四角色可见）。
    强制要求（ui-admin-console.md §3.5 / §6）：
      - 必须内置「异常 ≠ 破解」说明文案，避免客服误判并对客户越界指控
      - 只有「标记异常（备注）→ 转人工核实」，**无「自动判定破解 / 自动封禁」动作**
  -->
  <PageHeader
    crumb="风控 / 回执与异常"
    title="回执与异常"
    desc="B 档下厂商不接收业务数据，回执是唯一的在线证据。异常需人工核实后再判定处置。"
  >
    <template #actions>
      <RoleGate :allowed="canExport" mode="hide">
        <button type="button" class="ac-btn" @click="exportCsv">导出</button>
      </RoleGate>
    </template>
  </PageHeader>

  <div class="ac-content">
    <!-- 「异常 ≠ 破解」说明横幅：必须显著可见 -->
    <div class="ac-banner ac-banner--info">
      <div>
        <b>B 档下厂商不接收业务数据，回执是唯一的在线证据</b><br />
        回执异常 <b>不等于</b> 破解。连续跳空 / 长期缺失应在续约或售后环节核查（运维手册第 4 章）。
        本页仅支持「标记异常（备注）→ 转人工核实」，<b>无「自动判定破解 / 自动封禁」动作</b>。
      </div>
    </div>

    <!-- 筛选 -->
    <div class="ac-card">
      <div class="ac-card__body">
        <div class="ac-filters">
          <div class="ac-filters__item">
            <label for="r-type">异常类型</label>
            <UiSelect v-model="filters.type" :options="typeOptions" />
          </div>
          <div class="ac-filters__item">
            <label for="r-tenant">租户</label>
            <UiSelect v-model="filters.tenant" :options="tenantOptions" />
          </div>
          <div class="ac-filters__item">
            <label for="r-disp">处置状态</label>
            <UiSelect v-model="filters.disposition" :options="dispositionOptions" />
          </div>
        </div>
      </div>
    </div>

    <section class="ac-card">
      <div class="ac-card__head">
        <h3>异常列表</h3>
        <span class="ac-card__sub">{{ filtered.length }} 条</span>
      </div>

      <EmptyState
        v-if="filtered.length === 0"
        title="没有符合条件的回执异常"
        desc="当前筛选条件下没有异常记录，说明回执链路健康。可清空筛选复核全部记录。"
      >
        <template #actions>
          <button type="button" class="ac-btn" @click="resetFilters">清空筛选</button>
        </template>
      </EmptyState>

      <template v-else>
        <UiTable :columns="columns" :rows="filtered" row-key-field="id">
          <template #cell-deviceSummary="{ row }">
            <span class="ac-mono">{{ row.deviceSummary }}</span>
          </template>
          <template #cell-type="{ row }">
            <StatusTag :status="row.type" />
          </template>
          <template #cell-detail="{ row }">
            <span class="ac-mono">{{ row.detail }}</span>
          </template>
          <template #cell-firstSeen="{ row }">
            <span class="ac-mono">{{ formatDateTime(row.firstSeen) }}</span>
          </template>
          <template #cell-disposition="{ row }">
            <StatusTag :status="row.disposition" />
          </template>
          <template #actions="{ row }">
            <!-- 只有「标记 / 核实」动作，绝不提供「判定破解」「封禁」 -->
            <RoleGate :allowed="canMark" mode="disable" deny-text="当前角色无「标记异常」权限">
              <button type="button" class="ac-btn ac-btn--sm" @click="openDispose(row)">
                {{ row.verified ? '取消核实' : '核实 / 标记' }}
              </button>
            </RoleGate>
          </template>
        </UiTable>
      </template>
    </section>
  </div>

  <!-- 处置弹窗（草稿隔离） -->
  <Teleport to="body">
    <div v-if="disposeOpen" class="ac-modal-mask" @click.self="closeDispose">
      <div class="ac-modal" role="dialog" aria-modal="true" aria-label="标记回执异常处置">
        <div class="ac-modal__head">
          <h3>标记处置 · {{ disposeTarget?.deviceSummary ?? '' }}</h3>
        </div>
        <div class="ac-modal__body">
          <div class="ac-banner ac-banner--info">
            <div>
              本操作<b>仅记录人工核实结论</b>，不改变设备授权状态。
              请先联系客户确认是否检修 / 断电 / 更换硬件，<b>不得仅凭跳空判定客户破解</b>。
            </div>
          </div>
          <UiField label="备注" required :error="noteError" :hint="`必填，≥ ${MIN_NOTE} 字，将记入审计`">
            <UiTextarea
              v-model="disposeNote"
              :rows="3"
              :min-length="MIN_NOTE"
              show-counter
              :invalid="noteError.length > 0"
              placeholder="例如：已电话联系张工，确认为车间计划检修断电，非破解"
            />
          </UiField>
          <UiField label="核实结论" required>
            <UiRadio v-model="disposeVerified" :options="verifiedOptions" />
          </UiField>
        </div>
        <div class="ac-modal__foot">
          <button type="button" class="ac-btn" @click="closeDispose">取消</button>
          <button type="button" class="ac-btn ac-btn--primary" :disabled="!canSubmitDispose" @click="submitDispose">
            保存处置
          </button>
        </div>
      </div>
    </div>
  </Teleport>
</template>

<script setup lang="ts">
/**
 * @file ReceiptsPage.vue
 * @module admin-console/pages/ReceiptsPage
 * @description 回执与异常页。
 */
import { computed, reactive, ref } from 'vue';
import {
  PageHeader,
  UiField,
  UiSelect,
  UiTextarea,
  UiRadio,
  UiTable,
  StatusTag,
  EmptyState,
  RoleGate,
  formatDateTime,
  can,
  type SelectOption,
  type RadioOption,
  type TableColumn,
} from '@ui-kit';
import { repo, TENANT_NAMES, DEFAULT_ACTOR, type ReceiptAnomaly } from '../api/repo';
import { session } from '../store/session';

/** 备注最小字数。 */
const MIN_NOTE = 10;

/** 权限。 */
const canMark = computed(() => can(session.state.role, 'receipt.mark'));
const canExport = computed(() => can(session.state.role, 'audit.export'));

/** 刷新触发器。 */
const reloadKey = ref(0);

/** 筛选草稿。 */
const filters = reactive({
  type: '',
  tenant: '',
  disposition: '',
});

/** 过滤后的异常列表。 */
const filtered = computed(() => {
  void reloadKey.value;
  return repo.allAnomalies().filter((a) => {
    if (filters.type && a.type !== filters.type) {
      return false;
    }
    if (filters.tenant && a.tenant !== filters.tenant) {
      return false;
    }
    if (filters.disposition && a.disposition !== filters.disposition) {
      return false;
    }
    return true;
  });
});

/** 筛选选项。 */
const typeOptions: readonly SelectOption[] = [
  { value: '', label: '全部类型' },
  { value: 'receipt_gap', label: '序号跳空' },
  { value: 'receipt_missing', label: '回执缺失' },
  { value: 'receipt_rollback', label: '序号回退' },
  { value: 'receipt_bad_sig', label: '签名无效' },
  { value: 'receipt_delay', label: '回执延迟' },
];
const tenantOptions: readonly SelectOption[] = [
  { value: '', label: '全部租户' },
  ...TENANT_NAMES.map((t) => ({ value: t, label: t })),
];
const dispositionOptions: readonly SelectOption[] = [
  { value: '', label: '全部状态' },
  { value: 'pending_check', label: '待核查' },
  { value: 'verified', label: '已核实' },
];

/** 列定义。 */
const columns: readonly TableColumn[] = [
  { key: 'deviceSummary', label: '设备', mono: true },
  { key: 'tenant', label: '租户' },
  { key: 'type', label: '异常类型' },
  { key: 'detail', label: '明细', mono: true },
  { key: 'firstSeen', label: '首次出现', mono: true },
  { key: 'count', label: '次数', align: 'right' },
  { key: 'disposition', label: '处置状态' },
];

/** 清空筛选。 */
function resetFilters(): void {
  filters.type = '';
  filters.tenant = '';
  filters.disposition = '';
}

// ---------------- 处置弹窗 ----------------
/** 弹窗开关。 */
const disposeOpen = ref(false);
/** 目标 id。 */
const disposeTargetId = ref('');
/** 备注草稿。 */
const disposeNote = ref('');
/** 核实结论草稿（'verified' / 'pending'）。 */
const disposeVerified = ref('verified');

/** 处置目标。 */
const disposeTarget = computed<ReceiptAnomaly | null>(() =>
  disposeTargetId.value ? repo.allAnomalies().find((a) => a.id === disposeTargetId.value) ?? null : null,
);

/** 备注错误。 */
const noteError = computed(() =>
  disposeNote.value.trim().length > 0 && disposeNote.value.trim().length < MIN_NOTE
    ? `还差 ${MIN_NOTE - disposeNote.value.trim().length} 字`
    : '',
);

/** 可提交。 */
const canSubmitDispose = computed(() => disposeNote.value.trim().length >= MIN_NOTE);

/** 核实结论选项（说明后果）。 */
const verifiedOptions: readonly RadioOption[] = [
  { value: 'verified', label: '已核实（可解释的常规原因）', desc: '如检修断电 / 网络中断，非破解；异常关闭' },
  { value: 'pending', label: '转人工进一步核实', desc: '保留待核查，转技术支持继续跟进' },
];

/** 打开处置弹窗：以现有备注初始化草稿。 */
function openDispose(row: ReceiptAnomaly): void {
  disposeTargetId.value = row.id;
  disposeNote.value = row.note;
  disposeVerified.value = row.verified ? 'verified' : 'pending';
  disposeOpen.value = true;
}

/** 关闭：清草稿。 */
function closeDispose(): void {
  disposeOpen.value = false;
  disposeTargetId.value = '';
  disposeNote.value = '';
}

/** 保存处置。 */
function submitDispose(): void {
  if (!canSubmitDispose.value || !disposeTargetId.value) {
    return;
  }
  void repo.resolveAnomaly({
    id: disposeTargetId.value,
    note: disposeNote.value.trim(),
    verified: disposeVerified.value === 'verified',
    actor: DEFAULT_ACTOR,
  });
  closeDispose();
  reloadKey.value += 1;
}

/** 导出异常清单。 */
function exportCsv(): void {
  const rows = filtered.value;
  const header = '设备摘要,租户,异常类型,明细,首次出现,次数,处置状态';
  const body = rows.map((r) => [r.deviceSummary, r.tenant, r.type, r.detail, r.firstSeen, r.count, r.disposition].join(',')).join('\n');
  const blob = new Blob([`\uFEFF${header}\n${body}`], { type: 'text/csv;charset=utf-8' });
  const url = URL.createObjectURL(blob);
  const a = document.createElement('a');
  a.href = url;
  a.download = `receipt-anomalies-${Date.now()}.csv`;
  a.click();
  URL.revokeObjectURL(url);
}
</script>

<style scoped>
.ac-modal-mask {
  position: fixed;
  inset: 0;
  background: rgba(29, 33, 41, 0.45);
  display: flex;
  align-items: center;
  justify-content: center;
  z-index: 2000;
  padding: 24px;
}
.ac-modal {
  background: #fff;
  border-radius: var(--radius);
  box-shadow: var(--shadow);
  width: 560px;
  max-width: 100%;
  max-height: 88vh;
  display: flex;
  flex-direction: column;
}
.ac-modal__head {
  padding: 16px 20px;
  border-bottom: 1px solid var(--divider);
}
.ac-modal__head h3 {
  margin: 0;
  font-size: 16px;
  font-weight: 600;
}
.ac-modal__body {
  padding: 20px;
  overflow: auto;
  display: flex;
  flex-direction: column;
  gap: 14px;
}
.ac-modal__foot {
  padding: 14px 20px;
  border-top: 1px solid var(--divider);
  display: flex;
  justify-content: flex-end;
  gap: 8px;
}
</style>

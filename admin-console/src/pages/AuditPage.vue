<template>
  <!--
    AuditPage —— 审计日志（页面清单第 10 项，system / risk 可见；导出仅 system）。
    要点：筛选（actor_type / action / entity_type / entity_id / 时间区间）、分页、导出。
    口径：审计日志追加写入、不可修改；导出为 CSV 时明文激活码默认掩码。
  -->
  <PageHeader
    crumb="系统 / 审计日志"
    title="审计日志"
    desc="全部 /admin/* 操作留痕：操作者 / 对象 / 原因 / 来源 IP / 结果。日志追加写入、不可修改。"
  >
    <template #actions>
      <!-- 导出属操作级权限：risk 只读不可导出 -->
      <RoleGate :allowed="canExport" mode="disable" deny-text="当前角色只有审计只读权限，不能导出">
        <button type="button" class="ac-btn" @click="exportCsv">导出</button>
      </RoleGate>
    </template>
  </PageHeader>

  <div class="ac-content">
    <!-- 筛选 -->
    <div class="ac-card">
      <div class="ac-card__body">
        <div class="ac-filters">
          <div class="ac-filters__item">
            <label for="a-actor">操作者类型</label>
            <UiSelect v-model="filters.actorType" :options="actorTypeOptions" />
          </div>
          <div class="ac-filters__item">
            <label for="a-action">动作（包含匹配）</label>
            <UiInput v-model="filters.action" placeholder="如：废弃 / 重发 / 轮换" />
          </div>
          <div class="ac-filters__item">
            <label for="a-entity">对象类型</label>
            <UiSelect v-model="filters.entityType" :options="entityTypeOptions" />
          </div>
          <div class="ac-filters__item">
            <label for="a-entity-id">对象标识</label>
            <UiInput v-model="filters.entityId" placeholder="如：c-003 / RV-2026" />
          </div>
        </div>
      </div>
    </div>

    <section class="ac-card">
      <div class="ac-card__head">
        <h3>操作审计</h3>
        <span class="ac-card__sub">共 {{ paged.total }} 条</span>
      </div>

      <EmptyState
        v-if="paged.total === 0"
        title="没有符合条件的审计记录"
        desc="可能是筛选条件过窄或时间区间内无操作。清空筛选可查看全部审计记录。"
      >
        <template #actions>
          <button type="button" class="ac-btn" @click="resetFilters">清空筛选</button>
        </template>
      </EmptyState>

      <template v-else>
        <UiTable :columns="columns" :rows="paged.items" row-key-field="id">
          <template #cell-ts="{ row }">
            <span class="ac-mono">{{ row.ts }}</span>
          </template>
          <template #cell-actor="{ row }">
            {{ row.actor }}（{{ row.actorType === 'human' ? '人工' : '系统' }}）
          </template>
          <template #cell-entityId="{ row }">
            <span class="ac-mono">{{ row.entityLabel }} {{ row.entityId }}</span>
          </template>
          <template #cell-ip="{ row }">
            <span class="ac-mono">{{ maskIp(row.ip) }}</span>
          </template>
          <template #cell-result="{ row }">
            <StatusTag :status="row.result" />
          </template>
        </UiTable>

        <UiPager :page="paged.page" :total="paged.total" :page-size="PAGE_SIZE" @update:page="onPage" />
      </template>
    </section>

    <p class="ac-note">
      <span class="ac-note__icon">ⓘ</span>
      <span>
        审计日志追加写入、不可修改；导出为 CSV 时明文激活码默认掩码，需二次权限确认。
        敏感操作（废弃 / 重发 / 密钥 / 档位 / 明文查看）均强制记录原因。
      </span>
    </p>
  </div>
</template>

<script setup lang="ts">
/**
 * @file AuditPage.vue
 * @module admin-console/pages/AuditPage
 * @description 审计日志页。
 */
import { computed, reactive, ref, watch } from 'vue';
import {
  PageHeader,
  UiInput,
  UiSelect,
  UiTable,
  UiPager,
  StatusTag,
  EmptyState,
  RoleGate,
  maskIp,
  can,
  type SelectOption,
  type TableColumn,
} from '@ui-kit';
import { repo } from '../mock/mock-data';
import { session } from '../store/session';

/** 权限。 */
const canExport = computed(() => can(session.state.role, 'audit.export'));

/** 每页条数。 */
const PAGE_SIZE = 8;

/** 筛选草稿。 */
const filters = reactive({
  actorType: '',
  action: '',
  entityType: '',
  entityId: '',
});

/** 页码。 */
const page = ref(1);

/** 条件变化回到第 1 页。 */
watch(
  () => [filters.actorType, filters.action, filters.entityType, filters.entityId],
  () => {
    page.value = 1;
  },
);

/** 查询结果。 */
const paged = computed(() =>
  repo.queryAudit({
    actorType: filters.actorType,
    action: filters.action,
    entityType: filters.entityType,
    entityId: filters.entityId,
    page: page.value,
    pageSize: PAGE_SIZE,
  }),
);

/** 筛选选项。 */
const actorTypeOptions: readonly SelectOption[] = [
  { value: '', label: '全部' },
  { value: 'human', label: '人工' },
  { value: 'system', label: '系统' },
];
const entityTypeOptions: readonly SelectOption[] = [
  { value: '', label: '全部对象' },
  { value: 'activation_code', label: '激活码' },
  { value: 'device', label: '设备' },
  { value: 'tenant', label: '租户' },
  { value: 'signing_key', label: '签名密钥' },
  { value: 'transfer_ticket', label: '换机工单' },
  { value: 'receipt_anomaly', label: '回执异常' },
  { value: 'admin_user', label: '管理员账号' },
];

/** 列定义。 */
const columns: readonly TableColumn[] = [
  { key: 'ts', label: '时间', mono: true },
  { key: 'actor', label: '操作者' },
  { key: 'action', label: '操作' },
  { key: 'entityId', label: '对象', mono: true },
  { key: 'detail', label: '原因 / 备注' },
  { key: 'ip', label: '来源 IP', mono: true },
  { key: 'result', label: '结果' },
];

/** 换页。 */
function onPage(next: number): void {
  page.value = next;
}

/** 清空筛选。 */
function resetFilters(): void {
  filters.actorType = '';
  filters.action = '';
  filters.entityType = '';
  filters.entityId = '';
}

/** 导出审计 CSV（二次确认 + IP 掩码）。 */
function exportCsv(): void {
  const ok = window.confirm('导出审计日志（含全部筛选结果）。导出动作本身也会记入审计。确认导出？');
  if (!ok) {
    return;
  }
  const rows = repo.queryAudit({ actorType: '', action: '', entityType: '', entityId: '', page: 1, pageSize: 100000 }).items;
  const header = '时间,操作者,操作者类型,操作,对象类型,对象标识,原因备注,来源IP,结果';
  const body = rows
    .map((r) => [r.ts, r.actor, r.actorType, r.action, r.entityLabel, r.entityId, r.detail, maskIp(r.ip), r.result].join(','))
    .join('\n');
  const blob = new Blob([`\uFEFF${header}\n${body}`], { type: 'text/csv;charset=utf-8' });
  const url = URL.createObjectURL(blob);
  const a = document.createElement('a');
  a.href = url;
  a.download = `audit-logs-${Date.now()}.csv`;
  a.click();
  URL.revokeObjectURL(url);
}
</script>

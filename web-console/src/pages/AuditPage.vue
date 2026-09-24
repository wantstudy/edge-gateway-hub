<!--
  =============================================================================
  AuditPage —— 日志与审计（设计 §3.7 / 原型 gateway-v2a-glacier「audit」）
  =============================================================================
  · 按 actor / action / 时间范围筛选，分页（UiPager，条数只此一个口径）。
  · 「导出审计记录」契约里 `audit.export` **只给 system 角色**，用 RoleGate 控制可见性。
    注意：客户端角色模型为 admin/engineer/operator/viewer（session.ts），
    与 ui-kit rbac 的 system 语义对齐 —— 只有 admin 映射为 system，可导出。
  · 日志追加写入不可篡改；导出为 CSV。
-->
<template>
  <PageHeader
    crumb="运维 / 日志与审计"
    title="日志与审计"
    desc="登录、配置变更、授权事件、模拟开关变更均留痕，追加写入不可篡改。保留 180 天。"
  >
    <template #actions>
      <!-- 导出 = audit.export，仅 system（= admin）角色可见 -->
      <RoleGate
        :allowed="canExport"
        mode="disable"
        deny-text="当前角色只有审计只读权限，导出记录（数据出境）仅限系统管理员"
        fallback-label="无权导出"
      >
        <button type="button" class="wc-btn" data-testid="audit-export" @click="exportCsv">导出审计记录</button>
      </RoleGate>
    </template>
  </PageHeader>

  <div class="wc-content">
    <!-- ══ 筛选 ════════════════════════════════════════════════════════ -->
    <section class="wc-card">
      <div class="wc-card__body">
        <div class="wc-filters">
          <div class="wc-filters__item">
            <label for="audit-actor">操作人</label>
            <UiInput v-model="filters.actor" placeholder="如：张工 / system" />
          </div>
          <div class="wc-filters__item">
            <label for="audit-action">动作（包含匹配）</label>
            <UiInput v-model="filters.action" placeholder="如：修改 / 导入 / 登录" />
          </div>
          <div class="wc-filters__item">
            <label for="audit-from">开始时间</label>
            <UiInput v-model="filters.from" placeholder="2026-09-23 00:00:00" />
          </div>
          <div class="wc-filters__item">
            <label for="audit-to">结束时间</label>
            <UiInput v-model="filters.to" placeholder="2026-09-23 23:59:59" />
          </div>
          <div class="wc-filters__item">
            <label for="audit-result">结果</label>
            <UiSelect v-model="filters.result" :options="resultOptions" />
          </div>
          <div class="wc-filters__item">
            <label>&nbsp;</label>
            <button type="button" class="wc-btn" data-testid="audit-reset" @click="resetFilters">清空筛选</button>
          </div>
        </div>
      </div>
    </section>

    <!-- ══ 审计日志列表（分页）════════════════════════════════════════ -->
    <section class="wc-card">
      <div class="wc-card__head">
        <h3>审计日志</h3>
        <span class="wc-card__sub" data-testid="audit-summary">
          {{ rangeSummary }} · 共 {{ auditTotal }} 条
        </span>
      </div>

      <EmptyState
        v-if="auditTotal === 0"
        title="没有符合条件的审计记录"
        desc="可能是筛选条件过窄或时间区间内无操作。清空筛选可查看全部审计记录。"
      >
        <template #actions>
          <button type="button" class="wc-btn" @click="resetFilters">清空筛选</button>
        </template>
      </EmptyState>

      <template v-else>
        <UiTable
          :columns="columns"
          :rows="pagedLogs"
          row-key-field="id"
          footer="保留 180 天 · 每日 03:00 归档，归档后仍可检索"
        >
          <template #cell-ts="{ row }">
            <span class="wc-mono">{{ row.ts }}</span>
          </template>
          <template #cell-actor="{ row }">
            {{ row.actor }}（{{ row.actorType === 'human' ? '人工' : '系统' }}）
          </template>
          <template #cell-action="{ row }">
            <span class="wc-tag wc-tag--info">{{ row.action }}</span>
          </template>
          <template #cell-object="{ row }">
            <span class="wc-mono">{{ row.entityLabel }} {{ row.entityId }}</span>
          </template>
          <template #cell-ip="{ row }">
            <span class="wc-mono">{{ maskIp(row.ip) }}</span>
          </template>
          <template #cell-result="{ row }">
            <StatusTag :status="row.result" />
          </template>
        </UiTable>

        <UiPager :page="page" :total="auditTotal" :page-size="PAGE_SIZE" @update:page="onPage" />
      </template>
    </section>

    <p class="wc-note">
      <span class="wc-note__icon">i</span>
      <span>
        审计日志追加写入、不可修改；导出会把全量操作记录（含 actor / IP / 原因明细）落盘为可外传文件，
        属高敏感数据外带面，因此收敛到系统管理员单一角色（`audit.export`）。
      </span>
    </p>
  </div>
</template>

<script setup lang="ts">
/**
 * @file AuditPage.vue
 * @module web-console/pages/AuditPage
 * @description 日志与审计页（actor / action / 时间范围筛选 + 分页 + 导出角色门控）。
 */
import { computed, reactive, ref, watch } from 'vue';
import {
  PageHeader,
  UiTable,
  UiPager,
  UiInput,
  UiSelect,
  StatusTag,
  EmptyState,
  RoleGate,
  maskIp,
  type TableColumn,
  type SelectOption,
} from '@ui-kit';
import { repo, type AuditEntry } from '../mock/mock-data';
import { session } from '../store/session';

/** 每页条数。 */
const PAGE_SIZE = 5;

/**
 * 导出权限：契约中 `audit.export` 只授 `system`。
 *
 * 客户端角色模型（session.ts）为 admin/engineer/operator/viewer，
 * 只有 `admin` 对应 ui-kit rbac 的语义 system（系统管理员），其余角色只有只读。
 * ⚠️ RoleGate 仅控制可见性，非授权判定（判定在网关侧）。
 */
const canExport = computed<boolean>(() => session.state.role === 'admin');

/** 筛选草稿。 */
const filters = reactive({
  actor: '',
  action: '',
  from: '',
  to: '',
  result: '',
});

/** 页码。 */
const page = ref(1);

/** 条件变化回到第 1 页。 */
watch(
  () => [filters.actor, filters.action, filters.from, filters.to, filters.result],
  () => {
    page.value = 1;
  },
);

/** 结果筛选选项。 */
const resultOptions: readonly SelectOption[] = [
  { value: '', label: '全部结果' },
  { value: 'success', label: '成功' },
  { value: 'denied', label: '拒绝 403' },
  { value: 'failed', label: '失败' },
];

/**
 * 拉取全量审计（应用本地筛选），再本地分页。
 *
 * mock 的 `queryAudit` 只支持 actorType / action / entityType / result，
 * 不支持 actor（操作人）与时间范围，因此此处取全量后用页面级筛选补足，
 * 保持「条数只有分页条一个口径」。
 */
const allLogs = ref<AuditEntry[]>(fetchAll());

/** mock 全量审计（pageSize 取大值拿全量）。 */
function fetchAll(): AuditEntry[] {
  return repo.queryAudit({ actorType: '', action: '', entityType: '', result: '', page: 1, pageSize: 100000 }).items;
}

/** 筛选后的全量日志。 */
const filteredLogs = computed<AuditEntry[]>(() => {
  const actor = filters.actor.trim();
  const action = filters.action.trim();
  const from = filters.from.trim();
  const to = filters.to.trim();
  return allLogs.value.filter((log) => {
    if (actor && !log.actor.includes(actor)) {
      return false;
    }
    if (action && !log.action.includes(action)) {
      return false;
    }
    if (filters.result && log.result !== filters.result) {
      return false;
    }
    if (from && log.ts < from) {
      return false;
    }
    if (to && log.ts > to) {
      return false;
    }
    return true;
  });
});

/** 筛选后总数（分页条唯一口径）。 */
const auditTotal = computed<number>(() => filteredLogs.value.length);

/** 当前页日志。 */
const pagedLogs = computed<AuditEntry[]>(() => {
  const start = (page.value - 1) * PAGE_SIZE;
  return filteredLogs.value.slice(start, start + PAGE_SIZE);
});

/** 筛选口径摘要。 */
const rangeSummary = computed<string>(() => {
  const actor = filters.actor.trim() || '全部操作人';
  const action = filters.action.trim() || '全部动作';
  const range =
    filters.from.trim() || filters.to.trim()
      ? `${filters.from.trim() || '起始'} ~ ${filters.to.trim() || '至今'}`
      : '全部时间';
  return `${range} · ${actor} · ${action}`;
});

/** 列定义。 */
const columns: readonly TableColumn[] = [
  { key: 'ts', label: '时间', mono: true },
  { key: 'actor', label: '操作人' },
  { key: 'action', label: '动作' },
  { key: 'object', label: '对象' },
  { key: 'detail', label: '变更详情' },
  { key: 'ip', label: '来源 IP', mono: true },
  { key: 'result', label: '结果' },
];

/** 换页。 */
function onPage(next: number): void {
  page.value = next;
}

/** 清空筛选。 */
function resetFilters(): void {
  filters.actor = '';
  filters.action = '';
  filters.from = '';
  filters.to = '';
  filters.result = '';
}

/** 导出审计 CSV（按当前筛选结果；IP 掩码）。 */
function exportCsv(): void {
  const rows = filteredLogs.value;
  const header = '时间,操作人,操作者类型,动作,对象类型,对象标识,变更详情,来源IP,结果';
  const body = rows
    .map((r) =>
      [r.ts, r.actor, r.actorType, r.action, r.entityLabel, r.entityId, r.detail, maskIp(r.ip), r.result].join(','),
    )
    .join('\n');
  const blob = new Blob([`\uFEFF${header}\n${body}`], { type: 'text/csv;charset=utf-8' });
  const url = URL.createObjectURL(blob);
  const a = document.createElement('a');
  a.href = url;
  a.download = `audit-logs-${Date.now()}.csv`;
  a.click();
  URL.revokeObjectURL(url);
}

/** 初始加载（保留引用，便于刷新）。 */
void fetchAll;
</script>

<style scoped>
</style>

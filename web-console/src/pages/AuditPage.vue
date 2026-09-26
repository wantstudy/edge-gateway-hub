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
  <div class="wc-content">
    <!-- 工具条：刷新 + 导出（原页头右侧按钮迁入） -->
    <div class="pg-toolbar">
      <span class="wc-tag wc-tag--ok">实时数据</span>
      <span class="wc-spacer" />
      <button type="button" class="wc-btn wc-btn--sm" data-testid="audit-refresh" @click="refreshLogs">刷新</button>
      <!-- 导出 = audit.export，仅 system（= admin）角色可见 -->
      <RoleGate
        :allowed="canExport"
        mode="disable"
        deny-text="当前角色只有审计只读权限，导出记录（数据出境）仅限系统管理员"
        fallback-label="无权导出"
      >
        <button type="button" class="wc-btn wc-btn--sm" data-testid="audit-export" @click="exportCsv">导出审计记录</button>
      </RoleGate>
    </div>

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
        title="暂无审计记录"
        :desc="auditNotice || '当前筛选条件下没有审计记录。'"
      >
        <template #actions>
          <button type="button" class="wc-btn" data-testid="audit-reset-empty" @click="resetFilters">清空筛选</button>
        </template>
      </EmptyState>

      <template v-else>
        <UiTable
          :columns="columns"
          :rows="pagedLogs"
          row-key-field="id"
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
  </div>
</template>

<script setup lang="ts">
/**
 * @file AuditPage.vue
 * @module web-console/pages/AuditPage
 * @description 日志与审计页（actor / action / 时间范围筛选 + 分页 + 导出角色门控）。
 *
 * 数据源：`repo.queryAudit`（真实审计缓存，由预取 / `GET /api/ops/logs` 填充）；
 * 取全量后做页面级筛选（后端查询参数不支持 actor 与时间范围），保证「条数只有分页条一个口径」。
 */
import { computed, onMounted, reactive, ref, watch } from 'vue';
import {
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
import { dataVersion, repo, type AuditEntry, type NoticeKey } from '@/api/repo';
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

/** 数据源不可得原因（诚实空态文案；空串 = 正常）。 */
const noticesState = ref<Record<NoticeKey, string>>(repo.actions.notices());

/** 审计不可得原因（仅取 error / 失败类，避免把「本就为空」说成故障）。 */
const auditNotice = computed<string>(() => noticesState.value.audit ?? '');

/**
 * 筛选后总数（分页条唯一口径，来自 `queryAudit` 的 `total`）。
 *
 * ⚠️ 必须先于 `pagedLogs` 声明：`pagedLogs` 的初值由 `queryPage()` 计算，
 * 而 `queryPage()` 会写 `auditTotal` —— 顺序颠倒会触发 TDZ
 * （`ReferenceError: Cannot access 'auditTotal' before initialization`），
 * setup 抛错 → 整页空白（曾真实发生过）。
 */
const auditTotal = ref<number>(0);

/** 按当前筛选 + 页码拉取本页。 */
function queryPage(): AuditEntry[] {
  const res = repo.queryAudit({
    actorType: '',
    actor: filters.actor,
    action: filters.action,
    entityType: '',
    result: filters.result,
    from: filters.from,
    to: filters.to,
    page: page.value,
    pageSize: PAGE_SIZE,
  });
  auditTotal.value = res.total;
  return res.items;
}

/**
 * 当前页审计（真实分页：直接传 `page` / `PAGE_SIZE` 给 `repo.queryAudit`，
 * 由它在缓存上做筛选 + 分页后只回本页数据，不再以 `pageSize: 100000` 兜底全量）。
 * 筛选条件（actor / action / 时间范围 / result）也一并交给 `queryAudit`，保证总数与分页一致。
 */
const pagedLogs = ref<AuditEntry[]>(queryPage());

/** 重新读取（筛选/页码/缓存变化后刷新当前页）。 */
function reload(): void {
  pagedLogs.value = queryPage();
  noticesState.value = repo.actions.notices();
}

/** 导出用：取当前筛选条件下的全量（不走分页条，仅导出上下文）。 */
function exportAllFiltered(): AuditEntry[] {
  const res = repo.queryAudit({
    actorType: '',
    actor: filters.actor,
    action: filters.action,
    entityType: '',
    result: filters.result,
    from: filters.from,
    to: filters.to,
    page: 1,
    pageSize: Number.MAX_SAFE_INTEGER,
  });
  return res.items;
}

/** 刷新：先向网关拉一次运行日志，再重读缓存。 */
async function refreshLogs(): Promise<void> {
  await repo.ops.logs();
  reload();
}

/** 缓存填充（预取完成）后刷新列表。 */
watch(dataVersion, reload);

onMounted(reload);

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

/** 换页（由 UiPager 驱动，触发真实分页查询）。 */
function onPage(next: number): void {
  page.value = next;
  reload();
}

/** 清空筛选。 */
function resetFilters(): void {
  filters.actor = '';
  filters.action = '';
  filters.from = '';
  filters.to = '';
  filters.result = '';
}

/** 导出审计 CSV（按当前筛选条件下的全量；IP 掩码）。 */
function exportCsv(): void {
  const rows = exportAllFiltered();
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
</script>

<style scoped>
.pg-toolbar {
  display: flex;
  align-items: center;
  flex-wrap: wrap;
  gap: 8px;
  min-height: 28px;
}
</style>

<template>
  <!--
    DevicesPage —— 设备接入（接入分组第 1 页，路由 `/devices`）。

    硬性约定遵守情况：
      · 列表全部分页（UiPager），条数只有分页条一个口径；
      · KPI（总数 / 在线 / 离线故障 / 点位）来自全量设备，与筛选无关；
      · 写操作（新增 / 删除）受 RoleGate 控制（工程师及以上可见）；
      · 删除走 DangerConfirmModal（影响清单 + 原因必填 + 对象名二次校验）；
      · 未配设备时给空态引导（清空筛选 / 新增设备）；
      · 不暴露任何内部 id 以外的高危入口；授权判定不在前端。
  -->
  <PageHeader
    crumb="接入 / 设备接入"
    title="设备接入"
    desc="网关当前纳管的现场设备。列表全部分页，条数只有分页条一个口径。新增 / 删除需工程师及以上角色，且删除必须经二次确认。"
  >
    <template #actions>
      <RoleGate :allowed="canWrite">
        <button type="button" class="wc-btn wc-btn--primary" @click="go('device-new')">新增设备</button>
      </RoleGate>
    </template>
  </PageHeader>

  <div class="wc-content">
    <!-- KPI 卡片行 -->
    <div class="wc-grid wc-grid--4">
      <StatCard label="设备总数" :value="String(kpi.total)" unit="台" />
      <StatCard label="在线" :value="String(kpi.online)" unit="台" tone="ok" />
      <StatCard
        label="离线 / 故障"
        :value="String(kpi.abnormal)"
        unit="台"
        :tone="kpi.abnormal > 0 ? 'warn' : 'default'"
      />
      <StatCard label="点位总数" :value="formatInt(kpi.points)" unit="点" />
    </div>

    <!-- 设备列表 -->
    <section class="wc-card">
      <div class="wc-card__head">
        <h3>设备列表</h3>
        <span class="wc-card__sub">共 {{ total }} 台 · 第 {{ page }} / {{ totalPages }} 页</span>
      </div>

      <div class="wc-card__body">
        <div class="wc-filters">
          <div class="wc-filters__item">
            <label>状态</label>
            <UiSelect v-model="statusFilter" :options="statusOptions" />
          </div>
          <div class="wc-filters__item">
            <label>协议</label>
            <UiSelect v-model="protocolFilter" :options="protocolOptions" />
          </div>
          <div class="wc-filters__item wc-filters__grow">
            <label>关键字</label>
            <UiInput v-model="keyword" placeholder="设备名 / 连接摘要" />
          </div>
          <div class="wc-filters__item">
            <label>&nbsp;</label>
            <button type="button" class="wc-btn" @click="resetFilters">重置</button>
          </div>
        </div>
      </div>

      <EmptyState
        v-if="items.length === 0"
        title="没有符合条件的设备"
        desc="可能是筛选条件过窄，或网关尚未接入任何设备。你可以清空筛选，或以工程师角色新增一台设备。"
      >
        <template #actions>
          <button type="button" class="wc-btn" @click="resetFilters">清空筛选</button>
          <RoleGate :allowed="canWrite">
            <button type="button" class="wc-btn wc-btn--primary" @click="go('device-new')">新增设备</button>
          </RoleGate>
        </template>
      </EmptyState>

      <template v-else>
        <UiTable :columns="columns" :rows="items" row-key-field="id">
          <template #cell-protocolLabel="{ row }">
            <span class="wc-tag wc-tag--info">{{ row.protocolLabel }}</span>
          </template>
          <template #cell-status="{ row }">
            <StatusTag :status="row.status" />
          </template>
          <template #cell-pointCount="{ row }">
            <button type="button" class="dv-link" data-test="device-point-count" @click="goPoints(row.id)">
              {{ row.pointCount }}
            </button>
          </template>
          <template #cell-successRate="{ row }">
            <span class="dv-rate" :class="rateClass(row.successRate)">{{ row.successRate }}%</span>
          </template>
          <template #actions="{ row }">
            <button type="button" class="wc-btn wc-btn--sm" @click="goPoints(row.id)">点位</button>
            <RoleGate :allowed="canWrite">
              <button type="button" class="wc-btn wc-btn--sm wc-btn--danger" @click="askDelete(row)">删除</button>
            </RoleGate>
          </template>
        </UiTable>
        <UiPager :page="page" :total="total" :page-size="PAGE_SIZE" @update:page="onPage" />
      </template>
    </section>
  </div>

  <DangerConfirmModal
    :open="!!pendingDelete"
    :title="pendingDelete ? `删除设备：${pendingDelete.name}` : '删除设备'"
    :confirm-value="pendingDelete ? pendingDelete.id : ''"
    confirm-label="风险二次确认（输入设备标识去分隔符后 6 位）"
    confirm-placeholder="输入设备标识（如 dev001）后 6 位"
    :impacts="deleteImpacts"
    :facts="deleteFacts"
    :reasons="deleteReasons"
    confirm-text="删除设备"
    @close="cancelDelete"
    @submit="confirmDelete"
  />
</template>

<script setup lang="ts">
/**
 * @file DevicesPage.vue
 * @module web-console/pages/DevicesPage
 * @description 设备接入列表页：筛选 / 分页 / KPI / 删除确认。
 *
 * 数据来自 `repo.queryDevices`（筛选 + 分页的**唯一**入口），写操作经 `repo.deleteDevice`
 * 落到内存态并写审计。前端不持有任何授权判定。
 */
import { computed, onMounted, ref, watch } from 'vue';
import { useRouter } from 'vue-router';
import {
  PageHeader,
  StatCard,
  UiTable,
  UiPager,
  UiSelect,
  UiInput,
  StatusTag,
  EmptyState,
  DangerConfirmModal,
  RoleGate,
  type SelectOption,
  type TableColumn,
  type DangerFact,
} from '@ui-kit';
import { repo, PROTOCOL_OPTIONS, type DeviceRecord } from '@/api/repo';
import { session } from '../store/session';

const router = useRouter();

/** 每页条数（UiPager 单一口径）。 */
const PAGE_SIZE = 8;

/** 状态筛选选项。 */
const statusOptions: readonly SelectOption[] = [
  { value: '', label: '全部状态' },
  { value: 'online', label: '在线' },
  { value: 'offline', label: '离线' },
  { value: 'error', label: '故障' },
];

/** 协议筛选选项（动态合并协议枚举）。 */
const protocolOptions = computed<readonly SelectOption[]>(() => [
  { value: '', label: '全部协议' },
  ...PROTOCOL_OPTIONS.map((p) => ({ value: p.value, label: p.label })),
]);

/** 删除原因枚举。 */
const deleteReasons = ['误添加', '设备已退役', '重复录入', '其他'];

/** 表格列（行高 40px，紧凑）。 */
const columns: readonly TableColumn[] = [
  { key: 'name', label: '设备' },
  { key: 'protocolLabel', label: '协议' },
  { key: 'connectionSummary', label: '连接' },
  { key: 'status', label: '状态' },
  { key: 'pointCount', label: '点位', align: 'right', mono: true },
  { key: 'successRate', label: '成功率', align: 'right' },
  { key: 'failStreak', label: '连续失败', align: 'right', mono: true },
  { key: 'lastSampleAt', label: '最后成功采集', mono: true },
];

/** 是否可写（工程师及以上）。 */
const canWrite = computed(() => session.state.role === 'admin' || session.state.role === 'engineer');

/** 筛选与分页状态。 */
const statusFilter = ref('');
const protocolFilter = ref('');
const keyword = ref('');
const page = ref(1);

/** 当前页数据 + 总条数。 */
const items = ref<DeviceRecord[]>([]);
const total = ref(0);
const totalPages = computed(() => Math.max(1, Math.ceil(total.value / PAGE_SIZE)));

/** 全量设备（KPI 口径，独立于筛选）。 */
const allDevices = ref<DeviceRecord[]>([]);

/** KPI 视图模型。 */
const kpi = computed(() => {
  const list = allDevices.value;
  const online = list.filter((d) => d.status === 'online').length;
  return {
    total: list.length,
    online,
    abnormal: list.length - online,
    points: list.reduce<number>((s, d) => s + d.pointCount, 0),
  };
});

/** 重新查询当前页。 */
function reload(): void {
  const res = repo.queryDevices({
    status: statusFilter.value,
    protocol: protocolFilter.value,
    keyword: keyword.value,
    page: page.value,
    pageSize: PAGE_SIZE,
  });
  items.value = res.items;
  total.value = res.total;
}

/** 刷新全量设备（KPI）。 */
function refreshAll(): void {
  allDevices.value = repo.allDevices();
}

/** 成功率色调（仅语义辅助，不代表好坏）。 */
function rateClass(rate: number): string {
  if (rate >= 95) {
    return 'is-ok';
  }
  if (rate >= 80) {
    return 'is-warn';
  }
  return 'is-danger';
}

/** 换页。 */
function onPage(next: number): void {
  page.value = next;
}

/** 重置筛选（回到第 1 页）。 */
function resetFilters(): void {
  statusFilter.value = '';
  protocolFilter.value = '';
  keyword.value = '';
  page.value = 1;
}

/** 千分位。 */
function formatInt(value: number): string {
  return value.toLocaleString('en-US');
}

/** 跳转。 */
function go(name: string): void {
  void router.push({ name });
}

/** 跳到点位页并预选设备（列表「点位」按钮 / 点位数链接）。 */
function goPoints(deviceId: string): void {
  void router.push({ name: 'points', query: { device: deviceId } });
}

onMounted(() => {
  refreshAll();
  reload();
});

/** 筛选变化 → 回到第 1 页并重查。 */
watch([statusFilter, protocolFilter, keyword], () => {
  page.value = 1;
  reload();
});
watch(page, reload);

// ---------------------------------------------------------------------------
// 删除（二次确认）
// ---------------------------------------------------------------------------

/** 待删除设备。 */
const pendingDelete = ref<DeviceRecord | null>(null);

/** 删除影响清单。 */
const deleteImpacts = computed<string[]>(() =>
  pendingDelete.value
    ? [
        `将删除设备「${pendingDelete.value.name}」及其下全部 ${pendingDelete.value.pointCount} 个点位，操作不可恢复。`,
        '该设备已建立的北向转发映射需另行清理；本操作仅移除网关侧采集配置。',
      ]
    : [],
);

/** 删除对象摘要。 */
const deleteFacts = computed<readonly DangerFact[]>(() =>
  pendingDelete.value
    ? [
        { label: '设备标识', value: pendingDelete.value.id },
        { label: '协议', value: pendingDelete.value.protocolLabel },
        { label: '点位数量', value: String(pendingDelete.value.pointCount) },
      ]
    : [],
);

/** 打开删除确认。 */
function askDelete(row: DeviceRecord): void {
  pendingDelete.value = row;
}

/** 取消删除（弹窗会自行清空草稿）。 */
function cancelDelete(): void {
  pendingDelete.value = null;
}

/** 确认删除。 */
function confirmDelete(): void {
  if (!pendingDelete.value) {
    return;
  }
  repo.deleteDevice({ id: pendingDelete.value.id, actor: session.state.displayName });
  pendingDelete.value = null;
  refreshAll();
  if (page.value > totalPages.value) {
    page.value = totalPages.value;
  }
  reload();
}
</script>

<style scoped>
.dv-link {
  font-family: var(--font-mono);
  font-variant-numeric: tabular-nums;
  color: var(--brand);
  background: none;
  border: 0;
  padding: 0;
  cursor: pointer;
  font-size: var(--fs-table);
}
.dv-link:hover {
  text-decoration: underline;
}
.dv-rate {
  font-family: var(--font-mono);
  font-variant-numeric: tabular-nums;
}
.dv-rate.is-ok {
  color: var(--ok);
}
.dv-rate.is-warn {
  color: var(--warn);
}
.dv-rate.is-danger {
  color: var(--danger);
}
</style>

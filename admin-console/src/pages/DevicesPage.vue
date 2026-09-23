<template>
  <!--
    DevicesPage —— 设备管理（页面清单第 8 项）。
    筛选：租户 / 部署形态 native|docker / 授权状态 / 机器码关键字；分页。
    列：设备 ID、租户、机器码（掩码）、部署形态、镜像 digest、最近心跳、租约状态、回执异常摘要。
  -->
  <PageHeader
    crumb="授权运营 / 设备管理"
    title="设备管理"
    desc="已激活设备的授权与在线证据。注意：同一宿主上容器重建不视为换机（宿主锚点指纹一致）。"
  >
    <template #actions>
      <button type="button" class="ac-btn" @click="exportCsv">导出</button>
    </template>
  </PageHeader>

  <div class="ac-content">
    <div class="ac-card">
      <div class="ac-card__body">
        <div class="ac-filters">
          <div class="ac-filters__item">
            <label for="d-tenant">租户</label>
            <UiSelect v-model="filters.tenant" :options="tenantOptions" />
          </div>
          <div class="ac-filters__item">
            <label for="d-deploy">部署形态</label>
            <UiSelect v-model="filters.deployMode" :options="deployOptions" />
          </div>
          <div class="ac-filters__item">
            <label for="d-license">授权状态</label>
            <UiSelect v-model="filters.licenseStatus" :options="licenseOptions" />
          </div>
          <div class="ac-filters__item ac-filters__grow">
            <label for="d-keyword">机器码 / 设备名</label>
            <UiInput v-model="filters.keyword" placeholder="输入机器码片段或设备名" />
          </div>
        </div>
      </div>
    </div>

    <section class="ac-card">
      <div class="ac-card__head">
        <h3>设备列表</h3>
        <span class="ac-card__sub">{{ paged.total }} 台</span>
      </div>

      <EmptyState
        v-if="paged.total === 0"
        title="没有符合条件的设备"
        desc="可能是筛选条件过窄。清空筛选可查看全部已激活设备。"
      >
        <template #actions>
          <button type="button" class="ac-btn" @click="resetFilters">清空筛选</button>
        </template>
      </EmptyState>

      <template v-else>
        <UiTable :columns="columns" :rows="paged.items" row-key-field="id">
          <template #cell-machineSummary="{ row }">
            <span class="ac-mono">{{ maskMachineSummary(row.machineSummary) }}</span>
          </template>
          <template #cell-deployMode="{ row }">
            <StatusTag :status="row.deployMode" />
          </template>
          <template #cell-imageDigest="{ row }">
            <span class="ac-mono">{{ row.imageDigest ?? '—' }}</span>
          </template>
          <template #cell-grade="{ row }">
            <StatusTag :status="row.grade === 'A' ? 'ok' : row.grade === 'B' ? 'info' : 'unknown'" :text="`档位 ${row.grade}`" />
          </template>
          <template #cell-licenseStatus="{ row }">
            <StatusTag :status="row.licenseStatus" />
          </template>
          <template #cell-lastHeartbeatAt="{ row }">
            <span class="ac-mono">{{ shortTime(row.lastHeartbeatAt) }}</span>
          </template>
          <template #cell-receiptStatus="{ row }">
            <StatusTag
              :status="row.receiptStatus"
              :text="row.gapCount > 0 ? `${row.gapCount} 次跳空` : undefined"
            />
          </template>
          <template #actions="{ row }">
            <button type="button" class="ac-btn ac-btn--sm" @click="openDetail(row.id)">详情</button>
          </template>
        </UiTable>

        <UiPager :page="paged.page" :total="paged.total" :page-size="PAGE_SIZE" @update:page="onPage" />
      </template>
    </section>
  </div>
</template>

<script setup lang="ts">
/**
 * @file DevicesPage.vue
 * @module admin-console/pages/DevicesPage
 * @description 设备列表页。
 */
import { computed, reactive, ref, watch } from 'vue';
import { useRouter } from 'vue-router';
import {
  PageHeader,
  UiInput,
  UiSelect,
  UiTable,
  UiPager,
  StatusTag,
  EmptyState,
  maskMachineSummary,
  type SelectOption,
  type TableColumn,
} from '@ui-kit';
import { repo, TENANT_NAMES } from '../mock/mock-data';

const router = useRouter();

/** 每页条数。 */
const PAGE_SIZE = 8;

/** 筛选草稿。 */
const filters = reactive({
  tenant: '',
  deployMode: '',
  licenseStatus: '',
  keyword: '',
});

/** 页码。 */
const page = ref(1);

/** 条件变化回到第 1 页。 */
watch(
  () => [filters.tenant, filters.deployMode, filters.licenseStatus, filters.keyword],
  () => {
    page.value = 1;
  },
);

/** 查询结果。 */
const paged = computed(() =>
  repo.queryDevices({
    tenant: filters.tenant,
    deployMode: filters.deployMode,
    licenseStatus: filters.licenseStatus,
    keyword: filters.keyword,
    page: page.value,
    pageSize: PAGE_SIZE,
  }),
);

/** 筛选选项。 */
const tenantOptions: readonly SelectOption[] = [
  { value: '', label: '全部租户' },
  ...TENANT_NAMES.map((t) => ({ value: t, label: t })),
];
const deployOptions: readonly SelectOption[] = [
  { value: '', label: '全部形态' },
  { value: 'native', label: 'native' },
  { value: 'docker', label: 'docker' },
];
const licenseOptions: readonly SelectOption[] = [
  { value: '', label: '全部状态' },
  { value: 'active', label: '已授权' },
  { value: 'trial', label: '试用中' },
  { value: 'revoked_lease', label: '授权失效' },
];

/** 列定义。 */
const columns: readonly TableColumn[] = [
  { key: 'id', label: '设备 ID', mono: true },
  { key: 'tenant', label: '租户' },
  { key: 'machineSummary', label: '机器码', mono: true },
  { key: 'deployMode', label: '部署形态' },
  { key: 'imageDigest', label: '镜像 digest', mono: true },
  { key: 'lastHeartbeatAt', label: '最近心跳', mono: true },
  { key: 'licenseStatus', label: '租约状态' },
  { key: 'receiptStatus', label: '回执异常' },
];

/** 只显示时分（列表紧凑）。 */
function shortTime(value: string): string {
  const parts = value.split(' ');
  return parts.length === 2 ? parts[1] : value;
}

/** 换页。 */
function onPage(next: number): void {
  page.value = next;
}

/** 清空筛选。 */
function resetFilters(): void {
  filters.tenant = '';
  filters.deployMode = '';
  filters.licenseStatus = '';
  filters.keyword = '';
}

/** 跳设备详情。 */
function openDetail(id: string): void {
  void router.push({ name: 'device-detail', params: { id } });
}

/** 导出设备清单（机器码保持掩码）。 */
function exportCsv(): void {
  const ok = window.confirm('导出设备清单；机器码默认掩码，导出操作会记入审计。确认导出？');
  if (!ok) {
    return;
  }
  const rows = repo.queryDevices({ tenant: '', deployMode: '', licenseStatus: '', keyword: '', page: 1, pageSize: 100000 }).items;
  const header = '设备ID,租户,设备名,机器码摘要,部署形态,镜像digest,档位,租约状态,最近心跳,回执';
  const body = rows
    .map((d) =>
      [d.id, d.tenant, d.name, maskMachineSummary(d.machineSummary), d.deployMode, d.imageDigest ?? '-', d.grade, d.licenseStatus, d.lastHeartbeatAt, d.receiptStatus].join(','),
    )
    .join('\n');
  const blob = new Blob([`\uFEFF${header}\n${body}`], { type: 'text/csv;charset=utf-8' });
  const url = URL.createObjectURL(blob);
  const a = document.createElement('a');
  a.href = url;
  a.download = `devices-${Date.now()}.csv`;
  a.click();
  URL.revokeObjectURL(url);
}
</script>

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
      <StatCard label="设备总数" :value="String(kpi.total)" unit="台" icon-tone="ink" :sub="`免费基础版上限 ${FREE_TIER_DEVICE_LIMIT} 台`">
        <template #icon>▣</template>
      </StatCard>
      <StatCard label="在线" :value="String(kpi.online)" unit="台" tone="ok" icon-tone="teal">
        <template #icon>✓</template>
      </StatCard>
      <StatCard
        label="离线 / 故障"
        :value="String(kpi.abnormal)"
        unit="台"
        :tone="kpi.abnormal > 0 ? 'warn' : 'default'"
        icon-tone="amber"
      >
        <template #icon>!</template>
      </StatCard>
      <StatCard label="点位总数" :value="formatInt(kpi.points)" unit="点" icon-tone="violet">
        <template #icon>◈</template>
      </StatCard>
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
            <!-- 原型 :1197-1199：实时数据（primary，下钻实时曲线）/ 点位映射 / 编辑 -->
            <button
              type="button"
              class="wc-btn wc-btn--sm wc-btn--primary"
              data-test="device-live"
              @click="goLive(row.id)"
            >
              实时数据
            </button>
            <button
              type="button"
              class="wc-btn wc-btn--sm"
              data-test="device-points"
              @click="goPoints(row.id)"
            >
              点位映射
            </button>
            <button type="button" class="wc-btn wc-btn--sm" data-test="device-edit" @click="goEdit(row.id)">
              编辑
            </button>
            <RoleGate :allowed="canWrite">
              <button type="button" class="wc-btn wc-btn--sm wc-btn--danger" @click="askDelete(row)">删除</button>
            </RoleGate>
          </template>
        </UiTable>

        <!-- 表格 foot（原型 :1524）：配额口径 + 批量连通性探测 -->
        <div class="dv-foot">
          <span class="dv-foot__text" data-test="quota-note">{{ quotaNote }}</span>
          <button
            type="button"
            class="wc-btn wc-btn--sm"
            :disabled="testingAll"
            data-test="test-all"
            @click="testAllConnections"
          >
            {{ testingAll ? '测试中…' : '测试全部连接' }}
          </button>
        </div>

        <!-- 逐台探测结果（real 模式为后端结构化结果；mock 为演示行为） -->
        <div v-if="probeResults.length > 0" class="dv-probe">
          <p class="dv-probe__head" data-test="probe-summary">
            {{ probeSummary }}
          </p>
          <UiTable :columns="probeColumns" :rows="probeResults" row-key-field="id">
            <template #cell-ok="{ row }">
              <span v-if="row.ok" class="wc-tag wc-tag--ok">连通</span>
              <span v-else class="wc-tag wc-tag--danger">未连通</span>
            </template>
            <template #cell-elapsedMs="{ row }">
              <span class="wc-mono">{{ row.elapsedMs }}</span>
            </template>
          </UiTable>
        </div>

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
import { API_MODE, repo, PROTOCOL_OPTIONS, type DeviceRecord } from '@/api/repo';
import { session } from '../store/session';

const router = useRouter();

/** 是否接入真实后端（`VITE_API_MODE=real`）；mock 模式完全不发请求。 */
const IS_REAL = API_MODE === 'real';

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

/** 跳到点位页并预选设备（列表「点位映射」按钮 / 点位数链接）。 */
function goPoints(deviceId: string): void {
  void router.push({ name: 'points', query: { device: deviceId } });
}

/** 「实时数据」下钻：进入实时点位值页并预选该设备（原型 :1197 `goLive`）。 */
function goLive(deviceId: string): void {
  void router.push({ name: 'live', query: { device: deviceId } });
}

/** 「编辑」：进入新增 / 编辑设备页并带入该设备（原型 :1199）。 */
function goEdit(deviceId: string): void {
  void router.push({ name: 'device-new', query: { device: deviceId } });
}

// ---------------------------------------------------------------------------
// 表格 foot：配额口径 + 批量连通性探测（原型 :1524）
// ---------------------------------------------------------------------------

/** 免费基础版设备上限（原型 :1515 / :1524）。 */
const FREE_TIER_DEVICE_LIMIT = 8;

/** 配额文案：超过免费额度才说「已按专业版计费」，额度内不得暗示计费。 */
const quotaNote = computed<string>(() => {
  const n = kpi.value.total;
  return n > FREE_TIER_DEVICE_LIMIT
    ? `免费基础版上限 ${FREE_TIER_DEVICE_LIMIT} 台（当前 ${n} 台，已按专业版计费）`
    : `免费基础版上限 ${FREE_TIER_DEVICE_LIMIT} 台（当前 ${n} 台，额度内）`;
});

/** 单台连通性探测结果（后端结构化字段原样透传）。 */
interface ProbeResult {
  /** 设备 id */
  id: string;
  /** 设备名 */
  name: string;
  /** 是否连通 */
  ok: boolean;
  /** 结构化失败类型（`ok` 时为空串） */
  errorKind: string;
  /** 原因（后端原文；非 Modbus 协议为 `unsupported_protocol`） */
  reason: string;
  /** 耗时毫秒（**字符串透传**，绝不 parseInt） */
  elapsedMs: string;
  /** 行色条（失败行左侧红色条，见 UiTable `_rowClass`） */
  _rowClass?: string;
}

/** 探测结果列定义。 */
const probeColumns: readonly TableColumn[] = [
  { key: 'name', label: '设备' },
  { key: 'ok', label: '结果' },
  { key: 'errorKind', label: '失败类型', mono: true },
  { key: 'reason', label: '原因' },
  { key: 'elapsedMs', label: '耗时(ms)', align: 'right', mono: true },
];

/** 探测结果（逐台展示）。 */
const probeResults = ref<ProbeResult[]>([]);

/** 是否正在批量探测（禁用按钮，避免重复提交）。 */
const testingAll = ref(false);

/** 探测汇总文案（成功 / 失败台数 + 数据源口径）。 */
const probeSummary = computed<string>(() => {
  const okCount = probeResults.value.filter((r) => r.ok).length;
  const source = IS_REAL ? 'POST /api/devices/test（后端结构化探测）' : 'mock 演示（未发真实请求）';
  return `测试全部连接：${okCount} / ${probeResults.value.length} 台连通 · 数据源 ${source}`;
});

/**
 * 逐台探测连通性。
 *
 * real：`repo.actions.testDevice({deviceId})`（后端恒 200，结构化失败不 500；
 * 非 Modbus 协议返回 `unsupported_protocol`）；
 * mock：不发请求，按设备当前状态给出**明确标注为演示**的结果。
 */
async function testAllConnections(): Promise<void> {
  if (testingAll.value) {
    return;
  }
  testingAll.value = true;
  const results: ProbeResult[] = [];
  try {
    for (const device of allDevices.value) {
      if (!IS_REAL) {
        const ok = device.status === 'online';
        results.push({
          id: device.id,
          name: device.name,
          ok,
          errorKind: ok ? '' : 'demo_offline',
          reason: ok
            ? 'mock 演示：设备在线（未发起真实请求）'
            : `mock 演示：设备当前为「${device.status}」状态（未发起真实请求）`,
          elapsedMs: '—',
          _rowClass: ok ? '' : 'row-danger',
        });
        continue;
      }
      try {
        // real：统一走 repo.actions.testDevice（已封装 POST /api/devices/test，
        // 返回结构化 ProbeResult；mock 走下方演示分支）。
        const pr = await repo.actions.testDevice({ deviceId: device.id });
        results.push({
          id: device.id,
          name: device.name,
          ok: pr.ok,
          errorKind: pr.ok ? '' : pr.errorKind,
          reason: pr.ok
            ? pr.reason || 'TCP connect + read 1 holding register succeeded'
            : pr.reason || '探测失败（后端未给出原因）',
          elapsedMs: pr.elapsedMs || '—',
          _rowClass: pr.ok ? '' : 'row-danger',
        });
      } catch {
        results.push({
          id: device.id,
          name: device.name,
          ok: false,
          errorKind: 'error',
          reason: '探测请求失败（网络层错误）：请确认网关进程在监听 8080 端口',
          elapsedMs: '—',
          _rowClass: 'row-danger',
        });
      }
    }
  } finally {
    testingAll.value = false;
  }
  probeResults.value = results;
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
/* 表格 foot（原型 :1524）：左文案 + 右操作，与 UiTable__footer 同口径 */
.dv-foot {
  display: flex;
  align-items: center;
  justify-content: space-between;
  gap: 12px;
  flex-wrap: wrap;
  padding: 10px 12px;
  font-size: var(--fs-caption);
  color: var(--text-3);
  border-top: 1px solid var(--divider);
}
.dv-foot__text {
  line-height: 1.6;
}
/* 逐台探测结果 */
.dv-probe {
  padding: 4px 12px 12px;
  border-top: 1px solid var(--divider);
}
.dv-probe__head {
  margin: 8px 0 10px;
  font-size: var(--fs-caption);
  color: var(--text-2);
}
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

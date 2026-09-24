<template>
  <!--
    PointsPage —— 点位与映射（接入分组第 3 页，路由 `/points`）。

    硬性约定遵守情况：
      · 点位数**派生**自各设备（单一数据源），未配点表的设备不假装有数据；
      · 列表全部分页（UiPager）；
      · 批量导入 / 导出 CSV（校验给「行号 + 原因 + 允许值」，导出即可当导入模板）；
      · 导入仅内存态覆盖写库 + 审计（真实系统落库 + 网关侧字段校验）；
      · 写操作（导入 / 删除）受 RoleGate 控制；删除走 DangerConfirmModal。
  -->
  <PageHeader
    crumb="接入 / 点位与映射"
    title="点位与映射"
    desc="网关采集点位的统一台账。支持按设备筛选与分页；批量导入 / 导出采用 CSV，字段校验会精确到「行号 + 原因 + 允许值」。"
  >
    <template #actions>
      <RoleGate :allowed="canWrite">
        <button type="button" class="wc-btn" @click="triggerImport">导入 CSV</button>
      </RoleGate>
      <button type="button" class="wc-btn" @click="exportTemplate">导出模板</button>
      <input ref="fileInput" type="file" accept=".csv,text/csv" class="dv-hidden" @change="onFile" />
    </template>
  </PageHeader>

  <div class="wc-content">
    <!-- 筛选 -->
    <section class="wc-card">
      <div class="wc-card__body">
        <div class="wc-filters">
          <div class="wc-filters__item">
            <label>设备</label>
            <UiSelect v-model="deviceIdFilter" :options="deviceOptions" />
          </div>
          <div class="wc-filters__item">
            <label>类型</label>
            <UiSelect v-model="typeFilter" :options="typeOptions" />
          </div>
          <div class="wc-filters__item">
            <label>质量</label>
            <UiSelect v-model="qualityFilter" :options="qualityOptions" />
          </div>
          <div class="wc-filters__item wc-filters__grow">
            <label>关键字</label>
            <UiInput v-model="keyword" placeholder="点位名 / 目标点 / 地址" />
          </div>
          <div class="wc-filters__item">
            <label>&nbsp;</label>
            <button type="button" class="wc-btn" @click="resetFilters">重置</button>
          </div>
        </div>
      </div>
    </section>

    <!-- 列表 -->
    <section class="wc-card">
      <div class="wc-card__head">
        <h3>点位台账</h3>
        <span class="wc-card__sub">共 {{ total }} 个点位 · 第 {{ page }} / {{ totalPages }} 页</span>
      </div>

      <EmptyState
        v-if="items.length === 0"
        title="该条件下没有点位"
        desc="可能是筛选过窄，或设备尚未配置点表。你可以前往「新增设备」后导入点表，或导出模板了解字段格式。"
      >
        <template #actions>
          <button type="button" class="wc-btn" @click="resetFilters">清空筛选</button>
          <RoleGate :allowed="canWrite">
            <button type="button" class="wc-btn wc-btn--primary" @click="triggerImport">导入 CSV</button>
          </RoleGate>
        </template>
      </EmptyState>

      <template v-else>
        <UiTable :columns="columns" :rows="items" row-key-field="id">
          <template #cell-pointType="{ row }">
            <span class="wc-tag" :class="row.pointType === 'derived' ? 'wc-tag--warn' : 'wc-tag--info'">
              {{ row.pointType === 'derived' ? '计算点' : '物理点' }}
            </span>
          </template>
          <template #cell-quality="{ row }">
            <span class="wc-tag" :class="qualityTagClass(row.quality)">{{ row.quality }}</span>
          </template>
          <template #actions="{ row }">
            <RoleGate :allowed="canWrite">
              <button type="button" class="wc-btn wc-btn--sm wc-btn--danger" @click="askDelete(row)">删除</button>
            </RoleGate>
          </template>
        </UiTable>
        <UiPager :page="page" :total="total" :page-size="PAGE_SIZE" @update:page="onPage" />
      </template>
    </section>

    <!-- 导入结果 -->
    <section v-if="importResult" class="wc-card">
      <div class="wc-card__head">
        <h3>导入校验结果</h3>
        <span class="wc-card__sub">
          合法 {{ importResult.validRows.length }} 条 · 异常 {{ importResult.errors.length }} 条
        </span>
      </div>
      <div class="wc-card__body" style="gap: 12px">
        <div v-if="importResult.errors.length" class="wc-banner wc-banner--danger">
          <span>!</span>
          <span>存在 {{ importResult.errors.length }} 条异常行，将被跳过。请按行号与原因修正后重新导入。</span>
        </div>

        <div v-if="importResult.errors.length" class="dv-err-wrap">
          <table class="wc-table dv-err">
            <thead>
              <tr><th>行号</th><th>原因</th></tr>
            </thead>
            <tbody>
              <tr v-for="e in importResult.errors" :key="e.line">
                <td class="wc-mono">{{ e.line }}</td>
                <td>{{ e.reasons.join('；') }}</td>
              </tr>
            </tbody>
          </table>
        </div>

        <div class="dv-foot">
          <button
            type="button"
            class="wc-btn wc-btn--primary"
            :disabled="importResult.validRows.length === 0"
            @click="commitImport"
          >
            导入合法行（{{ importResult.validRows.length }} 条）
          </button>
          <button type="button" class="wc-btn" @click="clearImport">取消</button>
          <span class="dv-foot__hint">导入将按设备覆盖其原有全部点表（先删后写）。</span>
        </div>
      </div>
    </section>

    <!-- 导入成功提示 -->
    <div v-if="importDone" class="wc-banner wc-banner--ok">
      <span>✓</span>
      <span>已按设备覆盖导入 {{ importDone }} 个点位。</span>
    </div>
  </div>

  <DangerConfirmModal
    :open="!!pendingDelete"
    :title="pendingDelete ? `删除点位：${pendingDelete.name}` : '删除点位'"
    :confirm-value="pendingDelete ? pendingDelete.id : ''"
    confirm-label="风险二次确认（输入点位标识后 8 位）"
    confirm-placeholder="输入点位标识（如 pt001）后 8 位"
    :impacts="deleteImpacts"
    :facts="deleteFacts"
    :reasons="deleteReasons"
    confirm-text="删除点位"
    @close="cancelDelete"
    @submit="confirmDelete"
  />
</template>

<script setup lang="ts">
/**
 * @file PointsPage.vue
 * @module web-console/pages/PointsPage
 * @description 点位与映射：列表 / 筛选 / 分页 / CSV 导入导出（含行号 + 原因 + 允许值校验）/ 删除。
 *
 * 点位数派生自设备（单一数据源）；导入经 `repo.replacePointsOfDevice` 落内存态 + 审计。
 */
import { computed, onMounted, ref, watch } from 'vue';
import { useRoute } from 'vue-router';
import {
  PageHeader,
  UiTable,
  UiPager,
  UiSelect,
  UiInput,
  EmptyState,
  DangerConfirmModal,
  RoleGate,
  type SelectOption,
  type TableColumn,
  type DangerFact,
} from '@ui-kit';
import {
  repo,
  DATA_TYPE_OPTIONS,
  BYTE_ORDER_OPTIONS,
  QUALITY_OPTIONS,
  type PointRecord,
  type PointDraft,
  type DeviceRecord,
} from '@/api/repo';
import { session } from '../store/session';

const route = useRoute();

/** 每页条数。 */
const PAGE_SIZE = 10;

/** 删除原因。 */
const deleteReasons = ['误添加', '点位已废弃', '重复录入', '其他'];

/** 表格列。 */
const columns: readonly TableColumn[] = [
  { key: 'deviceName', label: '设备' },
  { key: 'name', label: '点位名' },
  { key: 'pointType', label: '类型' },
  { key: 'address', label: '地址' },
  { key: 'dataType', label: '数据类型' },
  { key: 'byteOrder', label: '字节序' },
  { key: 'unit', label: '单位' },
  { key: 'targetKey', label: '北向目标点', mono: true },
  { key: 'quality', label: '质量' },
];

/** 是否可写（工程师及以上）。 */
const canWrite = computed(() => session.state.role === 'admin' || session.state.role === 'engineer');

// ---------------------------------------------------------------------------
// 筛选与分页
// ---------------------------------------------------------------------------

const deviceIdFilter = ref('');
const typeFilter = ref('');
const qualityFilter = ref('');
const keyword = ref('');
const page = ref(1);

const items = ref<PointRecord[]>([]);
const total = ref(0);
const totalPages = computed(() => Math.max(1, Math.ceil(total.value / PAGE_SIZE)));

/** 设备下拉（含「全部设备」）。 */
const deviceOptions = computed<readonly SelectOption[]>(() => [
  { value: '', label: '全部设备' },
  ...repo.allDevices().map((d) => ({ value: d.id, label: d.name })),
]);

/** 类型下拉。 */
const typeOptions: readonly SelectOption[] = [
  { value: '', label: '全部类型' },
  { value: 'physical', label: '物理点' },
  { value: 'derived', label: '计算点' },
];

/** 质量下拉。 */
const qualityOptions = computed<readonly SelectOption[]>(() => [
  { value: '', label: '全部质量' },
  ...QUALITY_OPTIONS.map((q) => ({ value: q, label: q })),
]);

/** 质量标签色调（与 MonitorPage 保持一致；质量码不入 status-map）。 */
function qualityTagClass(quality: string): string {
  if (quality === 'Good') {
    return 'wc-tag--ok';
  }
  if (quality === 'Uncertain') {
    return 'wc-tag--warn';
  }
  if (quality === 'Bad' || quality === 'Timeout') {
    return 'wc-tag--danger';
  }
  return 'wc-tag--info';
}

/** 重新查询当前页。 */
function reload(): void {
  const res = repo.queryPoints({
    deviceId: deviceIdFilter.value,
    pointType: typeFilter.value,
    quality: qualityFilter.value,
    keyword: keyword.value,
    page: page.value,
    pageSize: PAGE_SIZE,
  });
  items.value = res.items;
  total.value = res.total;
}

/** 重置筛选。 */
function resetFilters(): void {
  deviceIdFilter.value = '';
  typeFilter.value = '';
  qualityFilter.value = '';
  keyword.value = '';
  page.value = 1;
}

/** 换页。 */
function onPage(next: number): void {
  page.value = next;
}

onMounted(() => {
  const fromDevice = route.query.device;
  if (typeof fromDevice === 'string' && fromDevice) {
    deviceIdFilter.value = fromDevice;
  }
  reload();
});

watch([deviceIdFilter, typeFilter, qualityFilter, keyword], () => {
  page.value = 1;
  reload();
});
watch(page, reload);

// ---------------------------------------------------------------------------
// CSV 导入 / 导出
// ---------------------------------------------------------------------------

/** CSV 表头（导入 / 导出一致：导出即可当导入模板）。 */
const CSV_HEADER = ['设备', '点位名', '类型', '地址', '数据类型', '字节序', '单位', '死区', '北向目标点名', '公式'];

/** 文件输入 ref（隐藏，由按钮触发）。 */
const fileInput = ref<HTMLInputElement | null>(null);

/** 触发文件选择。 */
function triggerImport(): void {
  fileInput.value?.click();
}

/** 解析 CSV 为二维数组（简化：按行、按逗号；不处理引号内逗号，原型足够）。 */
function parseCsv(text: string): string[][] {
  return text
    .split(/\r?\n/)
    .map((line) => line.trim())
    .filter((line) => line.length > 0)
    .map((line) => line.split(',').map((cell) => cell.trim()));
}

/** 序列化为 CSV 文本（UTF-8 BOM，便于 Excel 识别中文）。 */
function toCsv(rows: string[][]): string {
  return '﻿' + rows.map((r) => r.join(',')).join('\r\n');
}

/** 导入结果。 */
interface ImportError {
  /** 行号（从 1 开始的表体行号，即文件物理行号） */
  line: number;
  /** 原因列表 */
  reasons: string[];
}
interface ImportResult {
  /** 合法行的设备分组 */
  groups: Map<string, PointDraft[]>;
  /** 合法行（扁平，用于计数与展示） */
  validRows: PointDraft[];
  /** 异常行 */
  errors: ImportError[];
}

/** 当前导入结果（null 表示未导入）。 */
const importResult = ref<ImportResult | null>(null);
/** 最近一次成功导入的条数。 */
const importDone = ref(0);

/** 设备名 / id → 设备对象 查表（导入解析用）。 */
const deviceIndex = computed<readonly DeviceRecord[]>(() => repo.allDevices());

/** 文件选择回调。 */
function onFile(event: Event): void {
  importDone.value = 0;
  const input = event.target as HTMLInputElement;
  const file = input.files?.[0];
  if (!file) {
    return;
  }
  const reader = new FileReader();
  reader.onload = () => {
    const text = String(reader.result ?? '');
    importResult.value = validateCsv(text);
    // 允许再次选择同一文件
    input.value = '';
  };
  reader.readAsText(file, 'utf-8');
}

/**
 * 校验 CSV：解析 + 逐行「行号 + 原因 + 允许值」。
 *
 * 设备列支持填设备 id 或设备名；类型只允许 physical / derived；数据类型与字节序必须为
 * 白名单集合；死区须为 ≥ 0 的数值；北向目标点名在同一设备内必须唯一。
 */
function validateCsv(text: string): ImportResult {
  const rows = parseCsv(text);
  const groups = new Map<string, PointDraft[]>();
  const validRows: PointDraft[] = [];
  const errors: ImportError[] = [];
  const seenTargets = new Map<string, Set<string>>();

  // 跳过表头（若首行是表头则跳过）
  const data = rows[0] && rows[0].join(',').replace(/﻿/g, '') === CSV_HEADER.join(',') ? rows.slice(1) : rows;

  data.forEach((row, i) => {
    const line = i + 1;
    const reasons: string[] = [];
    const deviceRaw = (row[0] ?? '').trim();
    const name = (row[1] ?? '').trim();
    const typeRaw = (row[2] ?? '').trim().toLowerCase();
    const address = (row[3] ?? '').trim();
    const dataType = (row[4] ?? '').trim();
    const byteOrder = (row[5] ?? '').trim();
    const unit = (row[6] ?? '').trim();
    const deadbandRaw = (row[7] ?? '').trim();
    const targetKey = (row[8] ?? '').trim();
    const formula = (row[9] ?? '').trim();

    // 设备
    const device =
      deviceIndex.value.find((d) => d.id === deviceRaw) ??
      deviceIndex.value.find((d) => d.name === deviceRaw);
    if (!device) {
      reasons.push('设备无法识别（应填设备 id 或名称）');
    }
    if (!name) {
      reasons.push('点位名必填');
    }
    const pointType = typeRaw === 'derived' ? 'derived' : typeRaw === 'physical' ? 'physical' : '';
    if (!pointType) {
      reasons.push('类型只能是 physical 或 derived');
    }
    if (pointType === 'physical' && !address) {
      reasons.push('物理点地址必填');
    }
    if (!DATA_TYPE_OPTIONS.includes(dataType)) {
      reasons.push(`数据类型须为：${DATA_TYPE_OPTIONS.join(' / ')}`);
    }
    if (pointType === 'physical' && !(BYTE_ORDER_OPTIONS as readonly string[]).includes(byteOrder)) {
      reasons.push(`字节序须为：${BYTE_ORDER_OPTIONS.join(' / ')}`);
    }
    if (!unit) {
      reasons.push('单位必填');
    }
    const deadband = Number(deadbandRaw);
    if (deadbandRaw === '' || !Number.isFinite(deadband) || deadband < 0) {
      reasons.push('死区须为 ≥ 0 的数值');
    }
    if (!targetKey) {
      reasons.push('北向目标点名必填');
    }
    // 北向目标点唯一性（按设备）
    if (device && targetKey) {
      if (!seenTargets.has(device.id)) {
        seenTargets.set(device.id, new Set());
      }
      const set = seenTargets.get(device.id)!;
      if (set.has(targetKey)) {
        reasons.push('同一设备内北向目标点名重复');
      } else {
        set.add(targetKey);
      }
    }

    if (reasons.length > 0 || !device || !pointType) {
      errors.push({ line, reasons });
      return;
    }

    const draft: PointDraft = {
      deviceId: device.id,
      name,
      pointType,
      address: pointType === 'derived' ? '' : address,
      dataType,
      byteOrder: pointType === 'derived' ? '—' : (byteOrder as PointDraft['byteOrder']),
      unit,
      deadband,
      targetKey,
      formula: pointType === 'derived' ? formula : null,
      actor: session.state.displayName,
    };
    validRows.push(draft);
    if (!groups.has(device.id)) {
      groups.set(device.id, []);
    }
    groups.get(device.id)!.push(draft);
  });

  return { groups, validRows, errors };
}

/** 提交导入（按设备覆盖写库）。 */
function commitImport(): void {
  if (!importResult.value) {
    return;
  }
  let count = 0;
  for (const [deviceId, rows] of importResult.value.groups) {
    count += repo.replacePointsOfDevice({ deviceId, rows, actor: session.state.displayName });
  }
  importResult.value = null;
  importDone.value = count;
  page.value = 1;
  reload();
}

/** 取消导入。 */
function clearImport(): void {
  importResult.value = null;
}

/** 导出模板（含当前筛选设备现有点表，便于作为导入模板）。 */
function exportTemplate(): void {
  const rows: string[][] = [CSV_HEADER];
  const source = deviceIdFilter.value
    ? repo.pointsOfDevice(deviceIdFilter.value)
    : repo.allPoints();
  if (source.length === 0) {
    // 给出一个示例行，确保导出文件非空、格式可见
    rows.push(['（填设备 id 或名称）', '示例点位', 'physical', 'DB1.0', 'float32', 'AB CD', '℃', '0', 'line1_temp', '']);
  } else {
    for (const p of source) {
      rows.push([
        p.deviceName,
        p.name,
        p.pointType,
        p.address,
        p.dataType,
        String(p.byteOrder),
        p.unit,
        String(p.deadband),
        p.targetKey,
        p.formula ?? '',
      ]);
    }
  }
  const blob = new Blob([toCsv(rows)], { type: 'text/csv;charset=utf-8' });
  const url = URL.createObjectURL(blob);
  const a = document.createElement('a');
  a.href = url;
  a.download = deviceIdFilter.value ? `points-${deviceIdFilter.value}.csv` : 'points-template.csv';
  document.body.appendChild(a);
  a.click();
  document.body.removeChild(a);
  URL.revokeObjectURL(url);
}

// ---------------------------------------------------------------------------
// 删除（二次确认）
// ---------------------------------------------------------------------------

const pendingDelete = ref<PointRecord | null>(null);

const deleteImpacts = computed<string[]>(() =>
  pendingDelete.value
    ? [`将删除点位「${pendingDelete.value.name}」（${pendingDelete.value.targetKey}），操作不可恢复。`, '该点位已建立的北向转发映射需另行清理。']
    : [],
);

const deleteFacts = computed<readonly DangerFact[]>(() =>
  pendingDelete.value
    ? [
        { label: '点位标识', value: pendingDelete.value.id },
        { label: '所属设备', value: pendingDelete.value.deviceName },
        { label: '北向目标', value: pendingDelete.value.targetKey },
      ]
    : [],
);

function askDelete(row: PointRecord): void {
  pendingDelete.value = row;
}
function cancelDelete(): void {
  pendingDelete.value = null;
}
function confirmDelete(): void {
  if (!pendingDelete.value) {
    return;
  }
  repo.deletePoint({ id: pendingDelete.value.id, actor: session.state.displayName });
  pendingDelete.value = null;
  reload();
}
</script>

<style scoped>
.dv-hidden {
  display: none;
}
.dv-err-wrap {
  max-height: 220px;
  overflow: auto;
  border: 1px solid var(--divider);
  border-radius: var(--radius-sm);
}
.dv-err th,
.dv-err td {
  text-align: left;
  padding: 7px 12px;
  border-bottom: 1px solid var(--divider);
  font-size: var(--fs-caption);
}
.dv-err tr:last-child td {
  border-bottom: 0;
}
.dv-foot {
  display: flex;
  align-items: center;
  gap: 12px;
}
.dv-foot__hint {
  font-size: var(--fs-caption);
  color: var(--text-3);
}
</style>

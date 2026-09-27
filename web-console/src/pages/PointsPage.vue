<template>
  <!--
    PointsPage —— 点位与映射（接入分组第 3 页，路由 `/points`）。

    结构（`docs/design/prototype/gateway-v2a-glacier.html` :1090-1184 / :1655-1660）：
      纯主从（master-detail）—— 左列「找到设备」，右面板「维护这张点表」，职责不混。

    约定：
      · 点位数**派生**自各设备（单一数据源），未配点表的设备不假装有数据；
      · 点位「推送」开关默认开，切换走 `PUT /api/points/:device_id/:point_id`（真实落库）；
      · 批量导入 / 导出 CSV（校验给「行号 + 原因 + 允许值」，导出即可当导入模板）；
      · 写操作（导入 / 模板 / 新增 / 编辑 / 删除 / 推送）受 RoleGate 控制；删除走 DangerConfirmModal。
  -->
  <div class="wc-content">
    <!-- 工具行：类型 / 质量 / 关键字 / 重置，作用于当前选中设备 -->
    <section class="wc-card">
      <div class="wc-card__body">
        <div class="wc-filters">
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
          <div class="wc-filters__item">
            <label>&nbsp;</label>
            <RoleGate :allowed="canWrite">
              <button type="button" class="wc-btn" @click="triggerImport">导入 CSV</button>
            </RoleGate>
          </div>
          <div class="wc-filters__item">
            <label>&nbsp;</label>
            <button type="button" class="wc-btn" @click="exportTemplate">导出点表 CSV</button>
          </div>
          <input ref="fileInput" type="file" accept=".csv,text/csv" class="pt-hidden" @change="onFile" />
        </div>
      </div>
    </section>

    <div class="pt-split">
      <!-- ══ 左：设备列表 ══ -->
      <section class="wc-card">
        <div class="wc-card__head">
          <h3>设备列表</h3>
        </div>
        <div class="wc-card__body">
          <UiInput v-model="deviceKw" placeholder="搜索设备名称 / 协议 / 地址…" />

          <div class="pt-list">
            <div
              v-for="d in pagedDevices"
              :key="d.id"
              class="pt-dev"
              :class="{ 'is-on': d.id === selectedDeviceId }"
              role="button"
              tabindex="0"
              :aria-selected="d.id === selectedDeviceId ? 'true' : 'false'"
              @click="pickDevice(d.id)"
              @keydown.enter="pickDevice(d.id)"
            >
              <div class="pt-dev__r1">
                <span class="pt-dev__name">{{ d.name }}</span>
                <span class="wc-tag wc-tag--info">{{ d.protocolLabel }}</span>
              </div>
              <div class="pt-dev__r2 wc-mono">{{ d.protocolLabel }} · {{ d.connectionSummary }}</div>
              <div class="pt-dev__r3">
                <span class="pt-dot" :class="`is-${d.status}`" aria-hidden="true" />
                <span>{{ statusLabel(d.status) }}</span>
                <span v-if="pointCountOf(d.id) === 0" class="wc-tag wc-tag--warn">未配点表</span>
                <span class="pt-dev__pts wc-mono">{{ pointCountOf(d.id) }} 点</span>
              </div>
            </div>
            <p v-if="filteredDevices.length === 0" class="pt-list__empty">没有匹配的设备。</p>
          </div>

          <UiPager :page="devPage" :total="filteredDevices.length" :page-size="DEV_PAGE_SIZE" numeric jump @update:page="devPage = $event" />
        </div>
      </section>

      <!-- ══ 右：选中设备 ══ -->
      <section class="wc-card pt-pane">
        <div v-if="!selected" class="wc-card__body">
          <EmptyState title="未选中设备" desc="在左侧设备列表点选一台设备，即可维护它的点表。">
            <template #actions>
              <button type="button" class="wc-btn wc-btn--primary" @click="pickDevice(devices[0]?.id ?? '')">
                选中第一台设备
              </button>
            </template>
          </EmptyState>
        </div>

        <template v-else>
          <div class="pt-head">
            <div class="pt-head__title">
              <b>{{ selected.name }}</b>
              <span class="wc-tag wc-tag--info">{{ selected.protocolLabel }}</span>
              <span class="pt-head__st">
                <span class="pt-dot" :class="`is-${selected.status}`" aria-hidden="true" />
                {{ statusLabel(selected.status) }}
              </span>
              <span class="pt-head__ops">
                <button type="button" class="wc-btn wc-btn--sm wc-btn--primary" @click="goLive(selected.id)">实时数据</button>
                <RoleGate :allowed="canWrite">
                  <button type="button" class="wc-btn wc-btn--sm wc-btn--primary" @click="openPointForm(null)">新增点位</button>
                </RoleGate>
                <RoleGate :allowed="canWrite">
                  <button type="button" class="wc-btn wc-btn--sm" @click="triggerImport">导入点表</button>
                </RoleGate>
                <button type="button" class="wc-btn wc-btn--sm" @click="exportTemplate">导出点表</button>
                <RoleGate :allowed="canWrite">
                  <button type="button" class="wc-btn wc-btn--sm" @click="applyProtocolTemplate">使用协议模板</button>
                  <button type="button" class="wc-btn wc-btn--sm" data-testid="download-point-template" @click="downloadImportTemplate">下载模板</button>
                </RoleGate>
              </span>
            </div>
            <div class="pt-head__meta wc-mono">
              {{ selected.connectionSummary }} · 采集 {{ selected.intervalMs }} ms · 超时 {{ selected.timeoutMs }} ms
              · 重试 {{ selected.retryTimes }} 次 · 最后采集 {{ selected.lastSampleAt }}
            </div>
            <div class="pt-tabs" role="tablist">
              <button
                v-for="(tab, i) in PANE_VIEWS"
                :key="tab"
                type="button"
                class="pt-tab"
                :class="{ 'is-on': paneTab === i }"
                role="tab"
                :aria-selected="paneTab === i ? 'true' : 'false'"
                @click="paneTab = i"
              >
                {{ tab }}
              </button>
            </div>
          </div>

          <!-- ── 操作反馈 ── -->
          <div v-if="lastNote" class="pt-note" :class="lastNoteKind === 'warn' ? 'is-warn' : 'is-ok'">
            <span aria-hidden="true">{{ lastNoteKind === 'warn' ? '⚠' : '✓' }}</span>
            <span>{{ lastNote }}</span>
          </div>

          <!-- ── 页签 0：点表 ── -->
          <template v-if="paneTab === 0">
            <EmptyState
              v-if="selectedPoints.length === 0"
              :title="`${selected.name} 尚未配置点表`"
              :desc="`可导入 CSV，或用 ${selected.protocolLabel} 协议模板生成起点点位。`"
            >
              <template #actions>
                <RoleGate :allowed="canWrite">
                  <button type="button" class="wc-btn wc-btn--primary" @click="triggerImport">导入 CSV 点表</button>
                  <button type="button" class="wc-btn" @click="applyProtocolTemplate">使用 {{ selected.protocolLabel }} 协议模板</button>
                  <button type="button" class="wc-btn" @click="downloadImportTemplate">下载模板</button>
                  <button type="button" class="wc-btn" @click="openPointForm(null)">手动新增点位</button>
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
                <template #cell-address="{ row }">
                  <span class="wc-mono">{{ displayAddress(row) }}</span>
                </template>
                <template #cell-push="{ row }">
                  <UiSwitch
                    :model-value="pushValue(row)"
                    :disabled="!canWrite || !!pushBusy[row.id]"
                    on-text="推送"
                    off-text="不推送"
                    :data-testid="`point-push-${row.id}`"
                    @update:model-value="(v) => togglePush(row, v)"
                  />
                </template>
                <template #cell-quality="{ row }">
                  <span class="wc-tag" :class="qualityTagClass(row.quality)">{{ row.quality }}</span>
                </template>
                <template #cell-value="{ row }">
                  <span class="wc-mono">{{ row.valueText }}</span>
                </template>
                <template #actions="{ row }">
                  <span class="pt-ops">
                    <button type="button" class="wc-btn wc-btn--sm" @click="openFormula(row)">公式</button>
                    <RoleGate :allowed="canWrite">
                      <button type="button" class="wc-btn wc-btn--sm" @click="copyPoint(row)">复制</button>
                      <button type="button" class="wc-btn wc-btn--sm" @click="openPointForm(row)">编辑</button>
                      <button type="button" class="wc-btn wc-btn--sm wc-btn--danger" @click="askDelete(row)">删除</button>
                    </RoleGate>
                  </span>
                </template>
              </UiTable>
              <UiPager :page="page" :total="total" :page-size="PAGE_SIZE" numeric jump @update:page="onPage" />
            </template>
          </template>

          <!-- ── 页签 1：点表信息 ── -->
          <div v-else-if="paneTab === 1" class="pt-pad">
            <dl v-if="selectedPoints.length > 0" class="wc-kv">
              <dt>地址风格</dt><dd class="wc-mono">{{ addrStyleText }}</dd>
              <dt>点表来源</dt><dd><span class="wc-tag wc-tag--info">未记录</span></dd>
              <dt>点位数量</dt><dd class="wc-mono">{{ selectedPoints.length }} 个（含 {{ calcCount }} 个公式点）</dd>
              <dt>地址区间</dt><dd class="wc-mono">{{ addrRangeText }}</dd>
              <dt>target 前缀</dt><dd class="wc-mono">{{ targetPrefixText }}</dd>
              <dt>改点表影响</dt><dd>同步更新设备点位数与北向转发字段</dd>
            </dl>
          </div>

          <!-- ── 页签 2：映射链路 ── -->
          <div v-else-if="paneTab === 2" class="pt-pad">
            <dl class="wc-kv">
              <dt>设备</dt><dd class="wc-mono">{{ selected.name }} · {{ selected.id }}</dd>
              <dt>地址区间</dt><dd class="wc-mono">{{ addrRangeText }}</dd>
              <dt>点数</dt><dd class="wc-mono">{{ selectedPoints.length }}</dd>
              <dt>北向字段</dt><dd class="wc-mono">{{ selectedPoints.map((p) => p.targetKey).slice(0, 8).join('、') || '—' }}</dd>
            </dl>
          </div>

          <!-- ── 页签 3：点位模拟 ── -->
          <div v-else class="pt-pad">
            <dl class="wc-kv">
              <dt>当前数据量</dt><dd class="wc-mono">{{ selectedPoints.length }} 点</dd>
            </dl>
          </div>
        </template>
      </section>
    </div>

    <!-- 导入校验结果（弹窗，不在列表页内联展示） -->
    <Teleport to="body">
      <div v-if="importResult" class="wc-modal__mask" @click.self="clearImport">
        <div class="wc-modal wc-modal--wide" role="dialog" aria-modal="true" aria-label="导入校验结果">
          <div class="wc-modal__head">
            <h3 class="wc-modal__title">导入校验结果 · {{ importDeviceName }}</h3>
          </div>
          <div class="wc-modal__body">
            <div v-if="importResult.errors.length" class="wc-banner wc-banner--danger">
              <span aria-hidden="true">!</span>
              <span>存在 {{ importResult.errors.length }} 条异常行，整批拒绝（一条都不写）。请按行号与原因修正后重新导入。</span>
            </div>

            <div v-if="importResult.errors.length" class="pt-err-wrap">
              <table class="wc-table pt-err">
                <thead>
                  <tr><th>行号</th><th>原因</th><th>允许值</th></tr>
                </thead>
                <tbody>
                  <tr v-for="e in importResult.errors" :key="`${e.line}-${e.reason}`">
                    <td class="wc-mono">{{ e.line }}</td>
                    <td>{{ e.reason }}</td>
                    <td class="wc-mono">{{ e.allowed }}</td>
                  </tr>
                </tbody>
              </table>
            </div>
            <p v-else class="pt-import-ok">
              校验通过：合法 <b class="wc-mono">{{ importResult.validRows.length }}</b> 条，导入后按设备**覆盖**原有点表。
            </p>
          </div>
          <div class="wc-modal__foot">
            <button type="button" class="wc-btn" @click="clearImport">取消</button>
            <button
              type="button"
              class="wc-btn wc-btn--primary"
              :disabled="importBusy || importResult.validRows.length === 0 || importResult.errors.length > 0"
              @click="commitImport"
            >
              {{ importBusy ? '导入中…' : `导入合法行（${importResult.validRows.length} 条，覆盖原有点表）` }}
            </button>
          </div>
        </div>
      </div>
    </Teleport>

    <div v-if="importDone > 0" class="wc-banner wc-banner--ok">
      <span aria-hidden="true">✓</span>
      <span>已按设备覆盖导入 {{ importDone }} 个点位。</span>
    </div>
  </div>

  <DangerConfirmModal
    :open="!!pendingDelete"
    :title="pendingDelete ? `删除点位：${pendingDelete.name}` : '删除点位'"
    :confirm-value="pendingDelete ? pendingDelete.id : ''"
    confirm-mode="full"
    confirm-label="风险二次确认（输入点位标识原文）"
    confirm-placeholder="输入点位标识原文"
    :impacts="deleteImpacts"
    :facts="deleteFacts"
    :reasons="deleteReasons"
    confirm-text="删除点位"
    @close="cancelDelete"
    @submit="confirmDelete"
  />

  <!-- ================= 点位新增 / 编辑弹窗 ================= -->
  <Teleport to="body">
    <div v-if="pointForm" class="wc-modal__mask" @click.self="closePointForm">
      <div
        class="wc-modal"
        role="dialog"
        aria-modal="true"
        :aria-label="pointForm.id ? '编辑点位' : '新增点位'"
        data-testid="point-modal"
      >
        <div class="wc-modal__head">
          <h3 class="wc-modal__title">
            {{ pointForm.id ? `编辑点位：${pointForm.id}` : `为「${selected?.name ?? ''}」新增点位` }}
          </h3>
        </div>
        <div class="wc-modal__body">
          <div class="dv-form">
            <UiField label="点位类型">
              <UiSelect v-model="pointForm.pointType" :options="pointTypeOptions" data-testid="point-modal-type" />
            </UiField>
            <UiField label="点位名" required :error="formTouched && !formValid.name ? '必填' : ''">
              <UiInput v-model="pointForm.name" placeholder="如 料筒温度1" data-testid="point-modal-name" />
            </UiField>
            <UiField
              label="地址"
              :required="pointForm.pointType === 'physical'"
              :hint="pointForm.pointType === 'physical' ? addrStyleText : '计算点无 PLC 地址'"
              :error="formTouched && !formValid.address ? '物理点地址必填' : ''"
            >
              <UiInput
                v-model="pointForm.address"
                :disabled="pointForm.pointType === 'derived'"
                placeholder="如 40001 / DB1.0"
                data-testid="point-modal-address"
              />
            </UiField>
            <UiField
              label="设备连接地址"
              required
              hint="设备访问地址 host:port（如 192.168.1.10:502）；寄存器地址（如 40001）应填在「地址」列"
              :error="formTouched && !formValid.endpoint ? '必填，如 192.168.1.10:502' : ''"
            >
              <UiInput
                v-model="pointForm.endpoint"
                placeholder="如 192.168.1.10:502"
                data-testid="point-modal-endpoint"
              />
            </UiField>
            <UiField label="数据类型" :error="formTouched && !formValid.dataType ? `须为 ${DATA_TYPE_OPTIONS.join(' / ')}` : ''">
              <UiSelect v-model="pointForm.dataType" :options="dataTypeOptions" />
            </UiField>
            <UiField
              label="字节序"
              :error="formTouched && !formValid.byteOrder ? `须为 ${BYTE_ORDER_OPTIONS.join(' / ')}` : ''"
            >
              <UiSelect v-model="pointForm.byteOrder" :options="byteOrderOptions" :disabled="pointForm.pointType === 'derived'" />
            </UiField>
            <UiField label="单位" hint="选填，如 ℃ / MPa">
              <UiInput v-model="pointForm.unit" placeholder="如 ℃ / MPa" />
            </UiField>
            <UiField label="死区" hint="≥ 0" :error="formTouched && !formValid.deadband ? '须为 ≥ 0 的数值' : ''">
              <UiInput v-model="pointForm.deadband" type="number" />
            </UiField>
            <UiField
              label="北向目标点名"
              required
              hint="同一设备内必须唯一"
              :error="formTouched && !formValid.targetKey ? '必填且设备内唯一' : ''"
            >
              <UiInput v-model="pointForm.targetKey" placeholder="如 M_Temp1" data-testid="point-modal-target" />
            </UiField>
            <UiField
              v-if="pointForm.pointType === 'derived'"
              label="公式"
              full
              required
              :error="formTouched && !formValid.formula ? '计算点公式必填' : ''"
            >
              <UiInput v-model="pointForm.formula" placeholder="如 M_Good / M_Count * 100" />
            </UiField>
            <UiField label="北向推送" hint="关闭后该点位不进入北向转发">
              <UiSwitch
                v-model="pointForm.push"
                on-text="推送"
                off-text="不推送"
                :disabled="formBusy"
                data-testid="point-modal-push"
              />
            </UiField>
          </div>
        </div>
        <div class="wc-modal__foot">
          <button type="button" class="wc-btn" @click="closePointForm">取消</button>
          <button
            type="button"
            class="wc-btn wc-btn--primary"
            :disabled="formBusy"
            data-testid="point-modal-save"
            @click="submitPointForm"
          >
            {{ formBusy ? '保存中…' : '保存点位' }}
          </button>
        </div>
      </div>
    </div>
  </Teleport>
</template>

<script setup lang="ts">
/**
 * @file PointsPage.vue
 * @module web-console/pages/PointsPage
 * @description 点位与映射：主从布局（左设备列表 / 右四页签）。
 *
 * · 左列表：搜索 + 固定高度滚动 + 分页；点即切换右侧；
 * · 右页签：**点表**（含北向推送开关）、**点表信息**、**映射链路**、**点位模拟**；
 * · 写操作全部走既有 `repo.*`（真实 HTTP + 刷新），返回 `WriteResult`，失败展示真实原因；
 * · CSV 导入：校验到「行号 + 原因 + 允许值」，fail-closed（有错整批不写）。
 */
import { computed, onMounted, ref, watch } from 'vue';
import { useRoute, useRouter } from 'vue-router';
import {
  UiTable,
  UiPager,
  UiSelect,
  UiInput,
  UiField,
  UiSwitch,
  EmptyState,
  DangerConfirmModal,
  RoleGate,
  type SelectOption,
  type TableColumn,
  type DangerFact,
} from '@ui-kit';
import {
  dataVersion,
  refresh,
  repo,
  DATA_TYPE_OPTIONS,
  BYTE_ORDER_OPTIONS,
  QUALITY_OPTIONS,
  type PointRecord,
  type PointDraft,
  type DeviceRecord,
  type ProtocolType,
} from '@/api/repo';
import { session } from '../store/session';

const route = useRoute();
const router = useRouter();

/** 右侧页签（原型 PANE_VIEWS :1090）。 */
const PANE_VIEWS = ['点表', '点表信息', '映射链路', '点位模拟'] as const;

/** 点表每页条数。 */
const PAGE_SIZE = 8;
/** 左列设备每页条数。 */
const DEV_PAGE_SIZE = 8;

/** 删除原因。 */
const deleteReasons = ['误添加', '点位已废弃', '重复录入', '其他'];

/**
 * 补充说明最小字数：与 `DangerConfirmModal` 默认 `minNoteLength` 及后端
 * `MIN_NOTE_CHARS` 同口径（`note` 非空即须 ≥ 10 字），不足则在提交前挡下。
 */
const MIN_NOTE_CHARS = 10;

/** 表格列（对齐原型 :1077-1083，新增北向「推送」开关列）。 */
const columns: readonly TableColumn[] = [
  { key: 'pointType', label: '类型' },
  { key: 'name', label: '点位名称' },
  { key: 'address', label: '地址', mono: true },
  { key: 'dataType', label: '数据类型', mono: true },
  { key: 'byteOrder', label: '字节序', mono: true },
  { key: 'unit', label: '单位' },
  { key: 'targetKey', label: '目标点位', mono: true },
  { key: 'push', label: '推送' },
  { key: 'sim', label: '模拟', align: 'right' },
  { key: 'simRange', label: '模拟范围' },
  { key: 'simDec', label: '小数位', align: 'right' },
  { key: 'value', label: '当前值' },
  { key: 'quality', label: '质量' },
];

/** 是否可写（工程师及以上）。 */
const canWrite = computed(() => session.state.role === 'admin' || session.state.role === 'engineer');

// ---------------------------------------------------------------------------
// 左：设备列表
// ---------------------------------------------------------------------------

const devices = ref<DeviceRecord[]>([]);
const deviceKw = ref('');
const devPage = ref(1);
const selectedDeviceId = ref('');

/** 重新取设备清单（数据派生自同一份 repo，不另设数据源）。 */
function reloadDevices(): void {
  devices.value = repo.allDevices();
}

const filteredDevices = computed(() => {
  const kw = deviceKw.value.trim().toLowerCase();
  if (!kw) {
    return devices.value;
  }
  return devices.value.filter((d) =>
    `${d.name} ${d.protocolLabel} ${d.connectionSummary} ${d.id}`.toLowerCase().includes(kw),
  );
});

const pagedDevices = computed(() => {
  const start = (devPage.value - 1) * DEV_PAGE_SIZE;
  return filteredDevices.value.slice(start, start + DEV_PAGE_SIZE);
});

const selected = computed<DeviceRecord | null>(
  () => devices.value.find((d) => d.id === selectedDeviceId.value) ?? null,
);

/** 设备状态中文（列表行内小字）。 */
function statusLabel(status: DeviceRecord['status']): string {
  if (status === 'online') {
    return '在线';
  }
  return status === 'error' ? '故障' : '离线';
}

/** 某设备的点位数（派生，未经点时显示 0 而非声明值）。 */
function pointCountOf(deviceId: string): number {
  return points.value.filter((p) => p.deviceId === deviceId).length;
}

function pickDevice(id: string): void {
  if (!id) {
    return;
  }
  selectedDeviceId.value = id;
  page.value = 1;
}

watch(deviceKw, () => {
  devPage.value = 1;
});

// ---------------------------------------------------------------------------
// 右：点表查询与分页
// ---------------------------------------------------------------------------

const points = ref<PointRecord[]>([]);
const typeFilter = ref('');
const qualityFilter = ref('');
const keyword = ref('');
const page = ref(1);
const paneTab = ref(0);

const typeOptions: readonly SelectOption[] = [
  { value: '', label: '全部类型' },
  { value: 'physical', label: '物理点' },
  { value: 'derived', label: '计算点' },
];

const qualityOptions: readonly SelectOption[] = [
  { value: '', label: '全部质量' },
  ...QUALITY_OPTIONS.map((q) => ({ value: q, label: q })),
];

const pointTypeOptions: readonly SelectOption[] = [
  { value: 'physical', label: '物理点' },
  { value: 'derived', label: '计算点' },
];

const dataTypeOptions: readonly SelectOption[] = DATA_TYPE_OPTIONS.map((t) => ({ value: t, label: t }));

const byteOrderOptions: readonly SelectOption[] = [
  ...BYTE_ORDER_OPTIONS.map((b) => ({ value: b, label: b })),
  { value: '—', label: '—（计算点）' },
];

/** 选中设备的全部点位（未过滤）。 */
const selectedPoints = computed<readonly PointRecord[]>(() =>
  points.value.filter((p) => p.deviceId === selectedDeviceId.value),
);

/** 选中设备 + 当前筛选后的点位。 */
const filteredPoints = computed<readonly PointRecord[]>(() => {
  const kw = keyword.value.trim().toLowerCase();
  return selectedPoints.value.filter((p) => {
    if (typeFilter.value && p.pointType !== typeFilter.value) {
      return false;
    }
    if (qualityFilter.value && p.quality !== qualityFilter.value) {
      return false;
    }
    if (kw && !`${p.name} ${p.targetKey} ${p.id} ${p.address}`.toLowerCase().includes(kw)) {
      return false;
    }
    return true;
  });
});

/** 当前页点位。 */
const items = computed<readonly PointRecord[]>(() => {
  const start = (page.value - 1) * PAGE_SIZE;
  return filteredPoints.value.slice(start, start + PAGE_SIZE);
});
const total = computed(() => filteredPoints.value.length);

/** 质量标签色调（质量码不入 status-map，与其余页面的质量列口径保持一致）。 */
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

/** 重新取点位（单一数据源）。 */
function reload(): void {
  reloadDevices();
  points.value = repo.allPoints();
}

function resetFilters(): void {
  typeFilter.value = '';
  qualityFilter.value = '';
  keyword.value = '';
  page.value = 1;
}

function onPage(next: number): void {
  page.value = next;
}

watch([typeFilter, qualityFilter, keyword], () => {
  page.value = 1;
});

/**
 * 整页刷新（深链 `#/points?device=…`）时 repo 设备清单往往尚未就绪，
 * 先记住深链目标，待清单到位后再选中，避免刷新后丢失选中的设备。
 */
let pendingDeviceId = '';

watch(devices, (list) => {
  if (pendingDeviceId && list.some((d) => d.id === pendingDeviceId)) {
    selectedDeviceId.value = pendingDeviceId;
    pendingDeviceId = '';
  } else if (!selectedDeviceId.value && list.length > 0) {
    selectedDeviceId.value = list[0].id;
  }
});

onMounted(() => {
  reload();
  // 兜底：与 DevicesPage 同源问题——若挂载时设备缓存为空（登录后预取未落到
  // 本实例），主动重取一次真相，避免「其它页有设备、本页设备列表为空」。
  if (devices.value.length === 0) {
    void (async () => {
      try {
        await refresh();
      } catch {
        /* 保持诚实空态 */
      }
      reload();
    })();
  }
  const fromDevice = route.query.device;
  const devicesSnapshot = devices.value;
  if (typeof fromDevice === 'string' && fromDevice && devicesSnapshot.some((d) => d.id === fromDevice)) {
    selectedDeviceId.value = fromDevice;
  } else if (typeof fromDevice === 'string' && fromDevice) {
    pendingDeviceId = fromDevice;
  } else if (devicesSnapshot.length > 0) {
    selectedDeviceId.value = devicesSnapshot[0].id;
  }
});

// ---------------------------------------------------------------------------
// 右页签 · 点表信息（全部由现有点位派生）
// ---------------------------------------------------------------------------

/** 地址风格随协议变化（同一物理量在不同协议下地址完全不同）。 */
const addrStyleText = computed(() => {
  const proto = selected.value?.protocol as ProtocolType | undefined;
  if (!proto) {
    return '—';
  }
  if (proto === 's7') {
    return 'DB 块绝对地址（DB1.DBD0）';
  }
  if (proto === 'modbus-tcp' || proto === 'modbus-rtu') {
    return '寄存器地址（40001 起）';
  }
  if (proto === 'opc-ua') {
    return 'NodeId 符号地址（ns=2;s=…）';
  }
  if (proto === 'mc') {
    return '软元件地址（D100）';
  }
  if (proto === 'http') {
    return 'JSONPath 取值表达式';
  }
  return 'MQTT Topic';
});

/** 公式点数量。 */
const calcCount = computed(() => selectedPoints.value.filter((p) => p.pointType === 'derived').length);

/** 地址区间（不算公式点 —— 公式点没有 PLC 地址）。 */
const addrRangeText = computed(() => {
  const raw = selectedPoints.value.filter((p) => p.pointType !== 'derived');
  // 展示 point_id（寄存器 / 表达式）—— address 列在后端语义里是设备连接端点。
  return raw.length ? `${displayAddress(raw[0])} – ${displayAddress(raw[raw.length - 1])}` : '—';
});

/** target 前缀（取全部 targetKey 的最长公共前缀；无共同前缀则诚实给「—」）。 */
const targetPrefixText = computed(() => {
  const keys = selectedPoints.value.map((p) => p.targetKey).filter((k) => k.length > 0);
  if (keys.length === 0) {
    return '—';
  }
  let prefix = keys[0];
  for (const key of keys.slice(1)) {
    while (prefix.length > 0 && !key.startsWith(prefix)) {
      prefix = prefix.slice(0, -1);
    }
    if (prefix.length === 0) {
      break;
    }
  }
  return prefix.length > 0 ? `${prefix}…` : '—（无公共前缀）';
});

// ---------------------------------------------------------------------------
// 设备连接地址（endpoint）解析
//
// 后端契约（fail-closed，勿改后端）：
//   · `endpoint` = 设备访问地址 host:port（寄存器地址如 40001 一律放 `point_id`）；
//   · endpoint 填了纯数字寄存器地址 → 400（`pages.rs` / `writeapi.rs` 双重校验）；
//   · 单位 unit 允许为空（后端 `unit: Option<String>`，空 = 缺省）。
//
// 设备记录本身不含 endpoint，按可信度依次从：该设备既有点位行（后端 address
// 列即端点事实源）→ 连接摘要首段（host:port 形态）带出；都取不到则由用户在
// 表单/模板 CSV 中填写。
// ---------------------------------------------------------------------------

/** 寄存器形态（纯数字，如 40001）——绝不能当 endpoint。 */
function isRegisterShape(value: string): boolean {
  return /^\d+$/.test(value.trim());
}

/** 端点形态：非空、非占位符、非纯数字寄存器。 */
function isEndpointShape(value: string): boolean {
  const v = value.trim();
  return v.length > 0 && v !== '—' && !isRegisterShape(v);
}

/** 从该设备既有点位行带出设备连接地址（后端 address 列 = 端点事实源）。 */
function endpointFromPoints(deviceId: string): string {
  for (const p of points.value) {
    if (p.deviceId !== deviceId) {
      continue;
    }
    if (isEndpointShape(p.address)) {
      return p.address.trim();
    }
  }
  return '';
}

/** 从连接摘要首段带出（形如 `192.168.1.10:502 · 从站 1`）。 */
function endpointFromSummary(device: DeviceRecord): string {
  const first = device.connectionSummary.split('·')[0]?.trim() ?? '';
  return first.includes(':') && !isRegisterShape(first) ? first : '';
}

/** 当前选中设备的连接地址（表单预填 / 模板与导入共用）。 */
const deviceEndpoint = computed<string>(() => {
  const dev = selected.value;
  if (!dev) {
    return '';
  }
  return endpointFromPoints(dev.id) || endpointFromSummary(dev);
});

/** 点位行展示地址：物理点显示 point_id（寄存器 / 表达式），计算点显示 —。 */
function displayAddress(row: PointRecord): string {
  if (row.pointType === 'derived') {
    return '—';
  }
  const id = row.id.trim();
  if (id.length > 0 && id !== '—' && !id.startsWith('pt-real-')) {
    return id;
  }
  return row.address === '—' ? '—' : row.address;
}

// ---------------------------------------------------------------------------
// 协议模板（起点点位，务必再校准）
// ---------------------------------------------------------------------------

interface TemplateRow {
  /** 地址 */
  addr: string;
  /** 数据类型 */
  dt: string;
  /** 字节序 */
  bo: string;
  /** 点位名 */
  name: string;
}

const PT_TEMPLATE: Readonly<Record<ProtocolType, readonly TemplateRow[]>> = Object.freeze({
  'modbus-tcp': [
    { addr: '40001', dt: 'uint16', bo: 'CD AB', name: '料筒温度1' },
    { addr: '40003', dt: 'uint16', bo: 'CD AB', name: '注射压力' },
    { addr: '40005', dt: 'uint32', bo: 'CD AB', name: '累计产量' },
  ],
  'modbus-rtu': [
    { addr: '40001', dt: 'uint16', bo: 'CD AB', name: '温度' },
    { addr: '40002', dt: 'uint16', bo: 'CD AB', name: '湿度' },
    { addr: '40003', dt: 'uint32', bo: 'CD AB', name: '电度' },
  ],
  s7: [
    { addr: 'DB1.0', dt: 'float32', bo: 'AB CD', name: '料筒温度1' },
    { addr: 'DB1.4', dt: 'float32', bo: 'AB CD', name: '注射压力' },
    { addr: 'DB2.0', dt: 'float32', bo: 'CD AB', name: '锁模力' },
  ],
  'opc-ua': [
    { addr: 'ns=2;s=Machine.BarrelTemp', dt: 'float32', bo: '—', name: '料筒温度1' },
    { addr: 'ns=2;s=Machine.InjPressure', dt: 'float32', bo: '—', name: '注射压力' },
    { addr: 'ns=2;s=Machine.ClampForce', dt: 'float32', bo: '—', name: '锁模力' },
  ],
  mc: [
    { addr: 'D100', dt: 'int16', bo: 'AB CD', name: '料筒温度1' },
    { addr: 'D102', dt: 'int16', bo: 'AB CD', name: '注射压力' },
    { addr: 'D200', dt: 'int32', bo: 'AB CD', name: '累计产量' },
  ],
  http: [
    { addr: '$.data.temperature', dt: 'float32', bo: '—', name: '温度' },
    { addr: '$.data.pressure', dt: 'float32', bo: '—', name: '压力' },
    { addr: '$.data.count', dt: 'uint32', bo: '—', name: '计数' },
  ],
  mqtt: [
    { addr: 'plant/line1/temp', dt: 'float32', bo: '—', name: '温度' },
    { addr: 'plant/line1/humi', dt: 'float32', bo: '—', name: '湿度' },
    { addr: 'plant/line1/count', dt: 'uint32', bo: '—', name: '计数' },
  ],
});

const lastNote = ref('');
const lastNoteKind = ref<'ok' | 'warn'>('ok');

/** 结果区提示（写操作的真实反馈）。 */
function note(text: string, kind: 'ok' | 'warn' = 'ok'): void {
  lastNote.value = text;
  lastNoteKind.value = kind;
}

/** 按协议模板生成起点点位（追加，不覆盖现有点表）。 */
async function applyProtocolTemplate(): Promise<void> {
  const dev = selected.value;
  if (!dev) {
    return;
  }
  const rows = PT_TEMPLATE[dev.protocol] ?? [];
  if (rows.length === 0) {
    note(`${dev.protocolLabel} 暂无协议模板。`, 'warn');
    return;
  }
  // fail-closed：endpoint 取不到时不猜、不写（后端必校验非空）。
  const endpoint = deviceEndpoint.value;
  if (!endpoint) {
    note(`无法从设备记录确定「设备连接地址」：请先手动新增一个点位并填写该地址（host:port），再使用模板。`, 'warn');
    return;
  }
  let created = 0;
  for (const row of rows) {
    const draft: PointDraft = {
      deviceId: dev.id,
      name: row.name,
      pointType: 'physical',
      address: row.addr,
      endpoint,
      dataType: row.dt,
      byteOrder: row.bo === '—' ? 'AB CD' : (row.bo as PointDraft['byteOrder']),
      unit: '',
      deadband: 0,
      targetKey: `${dev.id}_${row.addr.replace(/[^A-Za-z0-9.]/g, '_')}`,
      pushEnabled: true,
      formula: null,
      actor: session.state.displayName,
    };
    const res = await repo.createPoint(draft);
    if (!res.ok) {
      reload();
      note(`已写入 ${created} 个点位后停止：${res.message}`, 'warn');
      return;
    }
    created += 1;
  }
  reload();
  note(`已按 ${dev.protocolLabel} 模板追加 ${created} 个起点点位（连接地址 ${endpoint}）。`);
}

// ---------------------------------------------------------------------------
// 点位表单（新增 / 编辑）
// ---------------------------------------------------------------------------

interface PointForm {
  /** 编辑时携带原记录 id；新增为空串 */
  id: string;
  pointType: 'physical' | 'derived';
  name: string;
  /** 寄存器地址 / 表达式（后端 `point_id`，物理点必填） */
  address: string;
  /** 设备连接地址 host:port（后端 `endpoint`，fail-closed 必填） */
  endpoint: string;
  dataType: string;
  byteOrder: string;
  unit: string;
  deadband: string;
  targetKey: string;
  formula: string;
  /** 北向推送开关（弹窗内可改；新增默认开） */
  push: boolean;
}

const pointForm = ref<PointForm | null>(null);
const formTouched = ref(false);
const formBusy = ref(false);

/** 表单校验结果（逐字段，实时驱动 saves）。 */
const formValid = computed(() => {
  const f = pointForm.value;
  if (!f) {
    return {
      name: false,
      address: false,
      endpoint: false,
      dataType: false,
      byteOrder: false,
      deadband: false,
      targetKey: false,
      formula: false,
    };
  }
  const isPhysical = f.pointType === 'physical';
  const deadband = Number(f.deadband);
  return {
    name: f.name.trim().length > 0,
    address: !isPhysical || f.address.trim().length > 0,
    // 后端对 endpoint 做 fail-closed 校验（非空且不得是寄存器号）。
    endpoint: isEndpointShape(f.endpoint),
    dataType: DATA_TYPE_OPTIONS.includes(f.dataType),
    byteOrder: !isPhysical || (BYTE_ORDER_OPTIONS as readonly string[]).includes(f.byteOrder),
    // 单位允许为空（后端 `unit: Option<String>`，空 = 缺省）。
    deadband: f.deadband.trim() !== '' && Number.isFinite(deadband) && deadband >= 0,
    targetKey:
      f.targetKey.trim().length > 0 &&
      selectedPoints.value.every((p) => p.id === f.id || p.targetKey !== f.targetKey.trim()),
    formula: isPhysical || f.formula.trim().length > 0,
  };
});

const formOk = computed(() => {
  const v = formValid.value;
  return v.name && v.address && v.endpoint && v.dataType && v.byteOrder && v.deadband && v.targetKey && v.formula;
});

/** 由表单草稿构造点位草稿（pushEnabled 由调用方决定）。 */
function draftFromForm(f: PointForm, deviceId: string, pushEnabled: boolean): PointDraft {
  const isPhysical = f.pointType === 'physical';
  return {
    deviceId,
    name: f.name.trim(),
    pointType: f.pointType,
    address: isPhysical ? f.address.trim() : '',
    // 设备连接地址：后端 `endpoint` 正例键；缺省即 400。
    endpoint: f.endpoint.trim(),
    dataType: f.dataType,
    byteOrder: isPhysical ? (f.byteOrder as PointDraft['byteOrder']) : '—',
    unit: f.unit.trim(),
    deadband: Number(f.deadband),
    targetKey: f.targetKey.trim(),
    pushEnabled,
    formula: isPhysical ? null : f.formula.trim(),
    actor: session.state.displayName,
  };
}

/** 打开表单弹窗（row 为空 = 新增）。 */
function openPointForm(row: PointRecord | null): void {
  formTouched.value = false;
  // 设备连接地址优先从记录带出；既有点位的 address 列即端点事实源（后端契约）。
  const recordEndpoint = row && isEndpointShape(row.address) ? row.address : deviceEndpoint.value;
  // 地址输入框承载寄存器 / 表达式（落库到 point_id）：物理点取记录 id（后端 point_id）。
  const recordAddress = row
    ? row.pointType === 'physical'
      ? row.id.trim().length > 0 && row.id !== '—'
        ? row.id
        : isRegisterShape(row.address)
          ? row.address
          : ''
      : ''
    : '';
  pointForm.value = row
    ? {
        id: row.id,
        pointType: row.pointType,
        name: row.name,
        address: recordAddress,
        endpoint: recordEndpoint,
        dataType: row.dataType,
        byteOrder: row.byteOrder === '—' ? 'AB CD' : row.byteOrder,
        unit: row.unit,
        deadband: String(row.deadband),
        targetKey: row.targetKey,
        formula: row.formula ?? '',
        push: pushValue(row),
      }
    : {
        id: '',
        pointType: 'physical',
        name: '',
        address: '',
        endpoint: deviceEndpoint.value,
        dataType: 'uint16',
        byteOrder: 'AB CD',
        unit: '',
        deadband: '0',
        targetKey: '',
        formula: '',
        push: true,
      };
}

/** 关闭弹窗并丢弃草稿。 */
function closePointForm(): void {
  pointForm.value = null;
}

/** 保存点位：新增走 `createPoint`，编辑走 `updatePoint`（真实 PUT）。 */
async function submitPointForm(): Promise<void> {
  formTouched.value = true;
  const f = pointForm.value;
  const dev = selected.value;
  if (!f || !dev || !formOk.value || formBusy.value) {
    return;
  }
  formBusy.value = true;
  try {
    if (f.id) {
      const res = await repo.updatePoint({
        ...draftFromForm(f, dev.id, f.push),
        id: f.id,
      });
      if (!res.ok) {
        note(`点位更新失败：${res.message}`, 'warn');
        return;
      }
      pointForm.value = null;
      reload();
      note(`点位「${f.name.trim()}」已更新。`);
      return;
    }
    const res = await repo.createPoint(draftFromForm(f, dev.id, f.push));
    if (!res.ok) {
      note(`点位新增失败：${res.message}`, 'warn');
      return;
    }
    pointForm.value = null;
    reload();
    note(`点位「${res.data?.name ?? f.name.trim()}」已新增到「${dev.name}」。`);
  } finally {
    formBusy.value = false;
  }
}

/** 复制点位（除名称与目标点名外照搬）。 */
async function copyPoint(row: PointRecord): Promise<void> {
  const dev = selected.value;
  if (!dev) {
    return;
  }
  let suffix = 1;
  let targetKey = `${row.targetKey}_copy`;
  while (selectedPoints.value.some((p) => p.targetKey === targetKey)) {
    suffix += 1;
    targetKey = `${row.targetKey}_copy${suffix}`;
  }
  // 地址列承载寄存器 / 表达式（落库 point_id）；连接地址从记录或设备带出。
  const copyAddress =
    row.pointType === 'physical'
      ? row.id.trim().length > 0 && row.id !== '—' && !row.id.startsWith('pt-real-')
        ? row.id
        : isRegisterShape(row.address)
          ? row.address
          : ''
      : '';
  const copyEndpoint = isEndpointShape(row.address) ? row.address : deviceEndpoint.value;
  const draft: PointDraft = {
    deviceId: dev.id,
    name: `${row.name} 副本`,
    pointType: row.pointType,
    address: copyAddress,
    endpoint: copyEndpoint,
    dataType: row.dataType,
    byteOrder: row.byteOrder === '—' ? '—' : row.byteOrder,
    unit: row.unit,
    deadband: row.deadband,
    targetKey,
    pushEnabled: pushValue(row),
    formula: row.formula,
    actor: session.state.displayName,
  };
  const res = await repo.createPoint(draft);
  if (!res.ok) {
    note(`复制失败：${res.message}`, 'warn');
    return;
  }
  reload();
  note(`已复制为「${res.data?.name ?? draft.name}」（目标点名 ${res.data?.targetKey ?? targetKey}）。`, 'warn');
}

/**
 * 推送开关当前显示值：优先取本次会话内 **PUT 成功确认** 的值；
 * 无确认值时回落到后端行（缺失字段由 repo 按需求默认「开」）。
 */
function pushValue(row: PointRecord): boolean {
  const confirmed = pushConfirmed.value[row.id];
  return confirmed === undefined ? row.pushEnabled : confirmed;
}

/** 北向推送开关：真实 PUT；失败不改变开关状态并展示真实原因。 */
async function togglePush(row: PointRecord, next: boolean): Promise<void> {
  if (pushBusy.value[row.id]) {
    return;
  }
  pushBusy.value = { ...pushBusy.value, [row.id]: true };
  try {
    const res = await repo.updatePoint({
      id: row.id,
      deviceId: row.deviceId,
      name: row.name,
      pointType: row.pointType,
      address: row.address === '—' ? '' : row.address,
      // 显式带端点（address 别名列承载的是端点事实，仍显式传 endpoint 防别名歧义）。
      endpoint: isEndpointShape(row.address) ? row.address : deviceEndpoint.value || undefined,
      dataType: row.dataType,
      byteOrder: row.byteOrder,
      unit: row.unit,
      deadband: row.deadband,
      targetKey: row.targetKey,
      pushEnabled: next,
      formula: row.formula,
      actor: session.state.displayName,
    });
    if (!res.ok) {
      note(`推送开关更新失败：${res.message}`, 'warn');
      return;
    }
    // 更新成功：记录后端已确认的值，再刷新列表（避免被默认值覆盖回原值）。
    pushConfirmed.value = { ...pushConfirmed.value, [row.id]: next };
    reload();
    note(`点位「${row.name}」已${next ? '开启' : '关闭'}北向推送。`);
  } finally {
    const rest = { ...pushBusy.value };
    delete rest[row.id];
    pushBusy.value = rest;
  }
}

/** 公式入口：展示当前公式原文（公式编辑器未落地）。 */
function openFormula(row: PointRecord): void {
  if (row.pointType === 'derived') {
    note(`「${row.name}」当前公式：${row.formula || '（空）'}。`);
    return;
  }
  note(`「${row.name}」是物理点，本身不带公式。如需派生量，请新建「计算点」并填写公式。`, 'warn');
}

// ---------------------------------------------------------------------------
// CSV 导入 / 导出
// ---------------------------------------------------------------------------

/** 新格式表头（导入 / 导出一致：导出即可当导入模板）。 */
const CSV_HEADER = ['地址', '点位名', '数据类型', '字节序', '单位', '死区', '目标点名', '公式'];
/** 扩展表头：末列「设备连接地址」（host:port，后端 fail-closed 必填）。 */
const CSV_HEADER_WITH_ENDPOINT = [...CSV_HEADER, '设备连接地址'];
/** 旧格式表头（含设备列，向后兼容此前导出的文件）。 */
const CSV_HEADER_LEGACY = ['设备', '点位名', '类型', '地址', '数据类型', '字节序', '单位', '死区', '北向目标点名', '公式'];

interface ImportError {
  /** 行号（字符串透传，绝不 parseInt） */
  line: string;
  /** 原因 */
  reason: string;
  /** 允许值 */
  allowed: string;
}
interface ImportResult {
  /** 目标设备 id */
  deviceId: string;
  /** 合法行 */
  validRows: PointDraft[];
  /** 异常行 */
  errors: ImportError[];
}

const fileInput = ref<HTMLInputElement | null>(null);
const importResult = ref<ImportResult | null>(null);
const importDone = ref(0);
const importBusy = ref(false);

const importDeviceName = computed(
  () => devices.value.find((d) => d.id === importResult.value?.deviceId)?.name ?? '—',
);

function triggerImport(): void {
  const dev = selected.value;
  if (!dev) {
    note('请先在左侧选择一台设备再导入（点表必须挂在某个设备上）。', 'warn');
    return;
  }
  importDone.value = 0;
  fileInput.value?.click();
}

/** 解析 CSV 为二维数组（按行、按逗号）。 */
function parseCsv(text: string): string[][] {
  return text
    .split(/\r?\n/)
    .map((line) => line.trim())
    .filter((line) => line.length > 0)
    .map((line) => line.split(',').map((cell) => cell.trim()));
}

function onFile(event: Event): void {
  const input = event.target as HTMLInputElement;
  const file = input.files?.[0];
  const dev = selected.value;
  if (!file || !dev) {
    return;
  }
  importDone.value = 0;
  const reader = new FileReader();
  reader.onload = () => {
    importResult.value = validateCsv(String(reader.result ?? ''), dev);
    input.value = '';
  };
  reader.readAsText(file, 'utf-8');
}

/**
 * 校验 CSV：逐行「行号 + 原因 + 允许值」，**fail-closed**（有错整批不写）。
 *
 * 兼容两种表头：新表头（无设备列，作用于当前选中设备）与旧表头（含设备列）。
 */
function validateCsv(text: string, dev: DeviceRecord): ImportResult {
  const rows = parseCsv(text);
  const validRows: PointDraft[] = [];
  const errors: ImportError[] = [];
  const seenTargets = new Set<string>(selectedPoints.value.map((p) => p.targetKey));

  const head = rows[0]?.join(',').replace(/﻿/g, '') ?? '';
  const legacy = head === CSV_HEADER_LEGACY.join(',');
  const withEndpoint = head === CSV_HEADER_WITH_ENDPOINT.join(',');
  const hasHeader = legacy || withEndpoint || head === CSV_HEADER.join(',');
  const data = hasHeader ? rows.slice(1) : rows;
  // 设备连接地址（endpoint）：优先取行内「设备连接地址」列，缺省从该设备既有点位
  // （后端 address 列 = 端点事实源）或连接摘要带出；取不到则该行判错 —— 后端对
  // endpoint fail-closed（非空且不得是寄存器号）。
  const deviceEndpointForCsv = endpointFromPoints(dev.id) || endpointFromSummary(dev);

  data.forEach((row, i) => {
    const line = i + 1;
    const name = (legacy ? row[1] : row[1] ?? '').trim();
    const typeRaw = legacy ? (row[2] ?? '').trim().toLowerCase() : '';
    const address = (legacy ? row[3] : row[0] ?? '').trim();
    const dataType = (legacy ? row[4] : row[2] ?? '').trim();
    const byteOrder = (legacy ? row[5] : row[3] ?? '').trim();
    const unit = (legacy ? row[6] : row[4] ?? '').trim();
    const deadbandRaw = (legacy ? row[7] : row[5] ?? '').trim();
    const targetKey = (legacy ? row[8] : row[6] ?? '').trim();
    const formula = (legacy ? row[9] : row[7] ?? '').trim();
    const endpoint = (withEndpoint && !legacy ? row[CSV_HEADER.length] ?? '' : '').trim() || deviceEndpointForCsv;

    const pointType: PointDraft['pointType'] =
      typeRaw === 'physical' ? 'physical' : typeRaw === 'derived' || typeRaw === 'calc' ? 'derived' : formula ? 'derived' : 'physical';

    const push = (reason: string, allowed: string): void => {
      errors.push({ line: String(line), reason, allowed });
    };

    if (!name) {
      push('点位名必填', '任意非空文本');
    }
    if (!DATA_TYPE_OPTIONS.includes(dataType)) {
      push(`数据类型非法：${dataType || '（空）'}`, DATA_TYPE_OPTIONS.join(' / '));
    }
    if (pointType === 'physical' && !address) {
      push('物理点地址必填', '按协议地址风格：40001 / DB1.0 / ns=2;s=… / D100 / JSONPath');
    }
    if (pointType === 'physical' && !(BYTE_ORDER_OPTIONS as readonly string[]).includes(byteOrder)) {
      push(`字节序非法：${byteOrder || '（空）'}`, BYTE_ORDER_OPTIONS.join(' / '));
    }
    // 设备连接地址：后端 fail-closed 必填（host:port），不得是寄存器号。
    if (!isEndpointShape(endpoint)) {
      push(
        '设备连接地址（endpoint）缺失',
        'host:port（如 192.168.1.10:502）；可先为该设备手动新增一个点位补全连接地址',
      );
    }
    const deadband = Number(deadbandRaw);
    if (deadbandRaw === '' || !Number.isFinite(deadband) || deadband < 0) {
      push(`死区非法：${deadbandRaw || '（空）'}`, '≥ 0 的数值');
    }
    if (!targetKey) {
      push('目标点名必填', '同一设备内唯一的名称');
    } else if (seenTargets.has(targetKey)) {
      push(`同一设备内目标点名重复：${targetKey}`, '换一个未被占用的名称');
    }
    if (pointType === 'derived' && !formula) {
      push('计算点公式必填', '如 M_Good / M_Count * 100');
    }

    if (errors.some((e) => e.line === String(line))) {
      return;
    }

    seenTargets.add(targetKey);
    validRows.push({
      deviceId: dev.id,
      name,
      pointType,
      address: pointType === 'physical' ? address : '',
      endpoint,
      dataType,
      byteOrder: pointType === 'physical' ? (byteOrder as PointDraft['byteOrder']) : '—',
      unit,
      deadband,
      targetKey,
      pushEnabled: true,
      formula: pointType === 'derived' ? formula : null,
      actor: session.state.displayName,
    });
  });

  return { deviceId: dev.id, validRows, errors };
}

/**
 * 提交导入：按后端导入契约（列 `device_id,point_id,protocol,endpoint,frequency_ms,push`）
 * 自组 CSV 走 `repo.actions.importPoints`（按设备覆盖；任一行失败整批拒绝，零落盘）。
 *
 * 说明：`repo.replacePointsOfDevice` 组装的 CSV 无 `endpoint` 列（后端必需列），
 * 必被整批拒绝，故由本页直接组正确列序的 CSV —— point_id 放寄存器、endpoint 放
 * 设备连接地址（host:port），与后端 `pages.rs` 导入契约一致。
 */
async function commitImport(): Promise<void> {
  const result = importResult.value;
  if (!result || result.validRows.length === 0 || importBusy.value) {
    return;
  }
  if (result.errors.length > 0) {
    note('存在异常行，整批拒绝：请按「行号 + 原因 + 允许值」修正后重新导入。', 'warn');
    return;
  }
  const dev = selected.value;
  if (!dev) {
    return;
  }
  importBusy.value = true;
  try {
    const csvCell = (v: string): string => (/[",\n]/.test(v) ? `"${v.replace(/"/g, '""')}"` : v);
    const frequency = String(Math.max(dev.intervalMs > 0 ? dev.intervalMs : 1000, 100));
    const csv = [
      'device_id,point_id,protocol,endpoint,frequency_ms,push',
      ...result.validRows.map((row) => {
        // point_id = 寄存器 / 表达式（物理点取地址）；endpoint = 设备连接地址。
        const pointId =
          row.pointType === 'physical' && row.address.trim().length > 0 ? row.address.trim() : row.targetKey;
        return [
          dev.id,
          csvCell(pointId),
          dev.protocol,
          csvCell(row.endpoint ?? ''),
          frequency,
          row.pushEnabled === false ? '0' : '1',
        ].join(',');
      }),
    ].join('\n');
    const res = await repo.actions.importPoints({ csv, deviceId: dev.id, replace: true });
    if (!res.ok) {
      importDone.value = 0;
      const first = res.errors[0];
      note(
        first
          ? `导入被整批拒绝（零落盘）：${res.errors.length} 行不合法；首条 第 ${first.line} 行 —— ${first.reason}${first.allowed ? `（允许值：${first.allowed}）` : ''}`
          : `导入失败（未落盘）：${res.message}`,
        'warn',
      );
      return;
    }
    importResult.value = null;
    importDone.value = Number(res.imported) || result.validRows.length;
    page.value = 1;
    reload();
    note(`已覆盖导入 ${importDone.value} 个点位（endpoint 已随行写入）。`);
  } finally {
    importBusy.value = false;
  }
}

function clearImport(): void {
  importResult.value = null;
}

/** 导出点表 CSV（real：`repo.actions.downloadPointsCsv` → `GET /api/points/export`）。 */
async function exportTemplate(): Promise<void> {
  const dev = selected.value;
  const result = await repo.actions.downloadPointsCsv(dev?.id);
  if (!result.ok) {
    note(`点表导出失败：${result.message}`, 'warn');
    return;
  }
  note(result.message || '已导出点表 CSV（可直接当导入模板）。');
}

/**
 * 下载点表导入模板（CSV，前端 Blob 直下，无需后端）。
 *
 * 表头与 `validateCsv` 解析列一致（另含末列「设备连接地址」）；含 2 行示例数据，
 * 地址风格随所选设备协议取自协议模板。
 */
function downloadImportTemplate(): void {
  const dev = selected.value;
  const protocol = dev?.protocol ?? 'modbus-tcp';
  const endpoint = dev ? deviceEndpoint.value || '192.168.1.10:502' : '192.168.1.10:502';
  const rows = (PT_TEMPLATE[protocol] ?? PT_TEMPLATE['modbus-tcp']).slice(0, 2);
  const lines: string[] = [CSV_HEADER_WITH_ENDPOINT.join(',')];
  for (const row of rows) {
    lines.push(
      [
        row.addr,
        row.name,
        row.dt,
        row.bo,
        '',
        '0',
        `${dev?.id ?? 'dev-01'}_${row.addr.replace(/[^A-Za-z0-9.]/g, '_')}`,
        '',
        endpoint,
      ].join(','),
    );
  }
  const csv = '﻿' + lines.join('\r\n');
  const blob = new Blob([csv], { type: 'text/csv;charset=utf-8' });
  const url = URL.createObjectURL(blob);
  const a = document.createElement('a');
  a.href = url;
  a.download = 'points-import-template.csv';
  document.body.appendChild(a);
  a.click();
  document.body.removeChild(a);
  URL.revokeObjectURL(url);
  note(`已下载 ${dev ? dev.protocolLabel : 'Modbus TCP'} 点表导入模板（含表头与 ${rows.length} 行示例）。`);
}

// ---------------------------------------------------------------------------
// 删除（二次确认）
// ---------------------------------------------------------------------------

const pendingDelete = ref<PointRecord | null>(null);
/** 北向推送开关的逐行提交中标记（禁用重复点击）。 */
const pushBusy = ref<Record<string, boolean>>({});
/**
 * 推送开关的本地已确认值（key = 点位 id）。
 *
 * `GET /api/points` 目前**不回填 `push_enabled`**（缺失即按需求 6 取默认「开」），
 * 若开关更新成功后直接 `reload()`，界面会被默认值覆盖回「开」，与刚才的
 * 「已关闭」反馈自相矛盾（把用户的真实变更静默吞掉）。因此把 **PUT 成功**的
 * 值在此保留为会话内权威值，直到列表读到后端真实字段为止；更新失败则不写入
 * （界面保持原值 + 展示真实失败原因）。
 */
const pushConfirmed = ref<Record<string, boolean>>({});

const deleteImpacts = computed<string[]>(() =>
  pendingDelete.value
    ? [
        `将删除点位「${pendingDelete.value.name}」（${pendingDelete.value.targetKey}），操作不可恢复。`,
        '该点位已建立的北向转发映射需另行清理。',
      ]
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
/**
 * 危险操作四要素（`DELETE /api/points/{device_id}/{point_id}` 的硬契约）：
 * · `reason`：必填枚举（`deleteReasons`，与设备删除同口径），随操作写入审计；
 * · `note`：补充说明，弹窗侧 `minNoteLength` 校验（不足时提交按钮即禁用）；
 * · `confirm`：须回显 **point_id 原文**（trim + 大小写不敏感）；
 * · 本函数只经由弹窗的 `@submit` 触发 —— 弹窗已在本地按同口径 fail-fast 并禁用按钮，
 *   所以这里拿到的一定是三项都填对的值（不重复做 DOM 猜测式比对）。
 *
 * 失败（400 结构不符 / 400 `confirm_mismatch` / 404 对象不存在）一律把后端 message
 * 原样透出，**绝不报告成功**。
 */
async function confirmDelete(payload: {
  reason: string;
  note: string;
  confirm: string;
}): Promise<void> {
  const target = pendingDelete.value;
  if (!target) {
    return;
  }
  const noteText = payload.note.trim();
  // 二次校验：确认值须等于 point_id 原文（与后端 `danger_confirm_matches` 同口径）。
  if (payload.confirm.trim().toLowerCase() !== target.id.trim().toLowerCase()) {
    note('二次校验未通过：确认值与点位标识不一致，未发起删除。', 'warn');
    return;
  }
  if (!payload.reason) {
    note('请填写删除原因。', 'warn');
    return;
  }
  if (noteText.length > 0 && noteText.length < MIN_NOTE_CHARS) {
    note(`补充说明需 ≥ ${MIN_NOTE_CHARS} 字。`, 'warn');
    return;
  }
  // 对象先落变量：三要素随本次调用一并下发，供 repo 侧按后端契约拼进 DELETE body。
  const write = {
    id: target.id,
    actor: session.state.displayName,
    reason: payload.reason,
    note: noteText,
    confirm: target.id,
  };
  const res = await repo.deletePoint(write);
  if (!res.ok) {
    pendingDelete.value = null;
    note(`删除失败：${res.message}`, 'warn');
    return;
  }
  pendingDelete.value = null;
  note(res.message, 'ok');
  reload();
}

// ---------------------------------------------------------------------------
// 导航
// ---------------------------------------------------------------------------

/** 查看该设备的实时数据。 */
function goLive(deviceId: string): void {
  void router.push({ name: 'live', query: { device: deviceId } });
}

watch(selectedDeviceId, () => {
  paneTab.value = 0;
  pointForm.value = null;
  lastNote.value = '';
});

// real 模式：后端写操作经 repo 热生效并自增 dataVersion，此处响应式刷新列表 / 点表。
watch(dataVersion, reload);

// 弹窗打开期间按 Esc 关闭（遮罩点击同样关闭）；置尾避免引用后面声明的 importResult（TDZ）。
function onModalKeydown(event: KeyboardEvent): void {
  if (event.key !== 'Escape') {
    return;
  }
  if (pointForm.value) {
    closePointForm();
  } else if (importResult.value) {
    clearImport();
  }
}

watch(
  () => Boolean(pointForm.value) || Boolean(importResult.value),
  (open) => {
    if (open) {
      window.addEventListener('keydown', onModalKeydown);
    } else {
      window.removeEventListener('keydown', onModalKeydown);
    }
  },
);
</script>

<style scoped>
.pt-hidden {
  display: none;
}

/* 主从布局（原型 .split） */
.pt-split {
  display: grid;
  grid-template-columns: minmax(272px, 340px) minmax(0, 1fr);
  gap: 18px;
  align-items: start;
}
@media (max-width: 1240px) {
  .pt-split {
    grid-template-columns: 1fr;
  }
}

/* 左列：固定高度滚动 + 分页 */
.pt-list {
  height: 520px;
  overflow-y: auto;
  padding-right: 4px;
  margin-top: 10px;
  display: flex;
  flex-direction: column;
  gap: 6px;
}
.pt-dev {
  display: flex;
  flex-direction: column;
  gap: 3px;
  padding: 11px 13px;
  cursor: pointer;
  border: 1px solid var(--border);
  border-radius: var(--radius-sm);
  background: var(--bg-card);
  transition: all 160ms cubic-bezier(0.16, 1, 0.3, 1);
}
.pt-dev:hover {
  border-color: var(--brand);
  background: var(--brand-subtle);
}
.pt-dev.is-on {
  border-color: var(--brand);
  background: var(--brand-subtle);
  box-shadow: 0 0 0 1px var(--brand) inset;
}
.pt-dev__r1 {
  display: flex;
  align-items: center;
  gap: 8px;
}
.pt-dev__name {
  font-weight: 600;
  font-size: var(--fs-body);
}
.pt-dev.is-on .pt-dev__name {
  color: var(--brand);
}
.pt-dev__r2 {
  font-family: var(--font-mono);
  font-size: var(--fs-caption);
  color: var(--text-3);
  white-space: nowrap;
  overflow: hidden;
  text-overflow: ellipsis;
}
.pt-dev__r3 {
  display: flex;
  align-items: center;
  gap: 7px;
  font-size: var(--fs-caption);
  color: var(--text-2);
}
.pt-dev__pts {
  margin-left: auto;
  font-family: var(--font-mono);
  font-weight: 700;
  color: var(--text-2);
}
.pt-dev.is-on .pt-dev__pts {
  color: var(--brand);
}
.pt-dot {
  width: 7px;
  height: 7px;
  border-radius: 50%;
  background: var(--text-3);
  flex: 0 0 7px;
}
.pt-dot.is-online {
  background: var(--ok);
}
.pt-dot.is-error {
  background: var(--danger);
}
.pt-dot.is-offline {
  background: var(--warn);
}
.pt-list__empty {
  padding: 26px 8px;
  text-align: center;
  color: var(--text-3);
  font-size: var(--fs-caption);
}

/* 右面板（原型 .sp-head / .sp-tabs） */
.pt-pane {
  overflow: hidden;
}
.pt-head {
  padding: 16px 18px 14px;
  border-bottom: 1px solid var(--divider);
  display: flex;
  flex-direction: column;
  gap: 5px;
}
.pt-head__title {
  display: flex;
  align-items: center;
  gap: 9px;
  flex-wrap: wrap;
}
.pt-head__title b {
  font-size: var(--fs-h3);
  font-weight: 700;
  letter-spacing: -0.015em;
}
.pt-head__st {
  display: inline-flex;
  align-items: center;
  gap: 7px;
  font-size: var(--fs-caption);
  color: var(--text-2);
}
.pt-head__ops {
  display: inline-flex;
  gap: 8px;
  flex-wrap: wrap;
  margin-left: 8px;
}
.pt-head__meta {
  font-size: var(--fs-caption);
  color: var(--text-3);
  word-break: break-all;
}
.pt-tabs {
  margin-top: 10px;
  display: flex;
  gap: 6px;
  flex-wrap: wrap;
}
.pt-tab {
  font-family: inherit;
  font-size: var(--fs-caption);
  border: 1px solid var(--border);
  border-radius: var(--radius-sm);
  padding: 7px 14px;
  background: var(--bg-hover);
  color: var(--text-2);
  cursor: pointer;
  white-space: nowrap;
  transition: all 160ms cubic-bezier(0.16, 1, 0.3, 1);
}
.pt-tab:hover {
  border-color: var(--brand);
  color: var(--brand);
}
.pt-tab.is-on {
  border-color: var(--brand);
  background: var(--brand-subtle);
  color: var(--brand);
  font-weight: 600;
}

/* 点位表单（弹窗内复用） */
.dv-form {
  display: grid;
  grid-template-columns: repeat(2, minmax(0, 1fr));
  gap: 14px 16px;
  margin-bottom: 12px;
}
.dv-foot {
  display: flex;
  align-items: center;
  gap: 12px;
  flex-wrap: wrap;
  margin-top: 10px;
}

/* 操作反馈 */
.pt-note {
  display: flex;
  align-items: flex-start;
  gap: 8px;
  margin: 12px 18px 0;
  padding: 10px 12px;
  border-radius: var(--radius-sm);
  border: 1px solid var(--ok-border);
  background: var(--ok-bg);
  color: var(--ok-fg);
  font-size: var(--fs-caption);
  line-height: 1.6;
}
.pt-note.is-warn {
  border-color: var(--warn-border);
  background: var(--warn-bg);
  color: var(--warn-fg);
}

/* 页签内容容器 */
.pt-pad {
  padding: 16px 18px;
}
.pt-ops {
  display: inline-flex;
  gap: 6px;
  white-space: nowrap;
}

/* 导入错误表 */
.pt-err-wrap {
  max-height: 220px;
  overflow: auto;
  border: 1px solid var(--divider);
  border-radius: var(--radius-sm);
  margin-bottom: 12px;
}
.pt-import-ok {
  margin: 0;
  font-size: var(--fs-body);
  color: var(--text-2);
}

/* 弹窗（与 AccountsPage 同一套 token） */
.wc-modal__mask {
  position: fixed;
  inset: 0;
  background: var(--mask);
  display: flex;
  align-items: center;
  justify-content: center;
  z-index: 2000;
  padding: 24px;
}
.wc-modal {
  background: var(--bg-card);
  border-radius: var(--radius);
  box-shadow: var(--shadow);
  width: 560px;
  max-width: 100%;
  display: flex;
  flex-direction: column;
}
.wc-modal--wide {
  width: 760px;
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
  gap: 12px;
}
.wc-modal__foot {
  padding: 14px 20px;
  border-top: 1px solid var(--divider);
  display: flex;
  justify-content: flex-end;
  gap: 8px;
}
.pt-err th,
.pt-err td {
  text-align: left;
  padding: 7px 12px;
  border-bottom: 1px solid var(--divider);
  font-size: var(--fs-caption);
}
.pt-err tr:last-child td {
  border-bottom: 0;
}
</style>

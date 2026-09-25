<template>
  <!--
    PointsPage —— 点位与映射（接入分组第 3 页，路由 `/points`）。

    结构（`docs/design/prototype/gateway-v2a-glacier.html` :1090-1184 / :1655-1660）：
      纯主从（master-detail）—— 左列「找到设备」，右面板「维护这张点表」，职责不混。

    硬性约定遵守情况：
      · 点位数**派生**自各设备（单一数据源），未配点表的设备不假装有数据；
      · 右侧四个页签：**点表 / 点表信息** 为已实现内容，**映射链路 / 点位模拟** 只给骨架与
        诚实空态（P0-4 未立项，不伪造链路图与模拟值）；
      · 模拟列在模拟策略立项前一律显示占位「—」，不做假开关；
      · 批量导入 / 导出 CSV（校验给「行号 + 原因 + 允许值」，导出即可当导入模板）；
      · 导入经 `repo.replacePointsOfDevice` 落库 + 审计（按设备覆盖，写明提示）；
      · 写操作（导入 / 模板 / 新增 / 编辑 / 删除）受 RoleGate 控制；删除走 DangerConfirmModal。
  -->
  <PageHeader
    crumb="接入 / 点位与映射"
    title="点位与映射"
    desc="左侧设备列表（搜索 + 分页），右侧是该设备的点表 / 点表信息 / 映射链路 / 点位模拟。没有点表的设备采不到任何数据。"
  >
    <template #actions>
      <RoleGate :allowed="canWrite">
        <button type="button" class="wc-btn" @click="triggerImport">导入 CSV</button>
      </RoleGate>
      <button type="button" class="wc-btn" @click="exportTemplate">导出点表 CSV</button>
      <input ref="fileInput" type="file" accept=".csv,text/csv" class="pt-hidden" @change="onFile" />
    </template>
  </PageHeader>

  <div class="wc-content">
    <!-- 工具行：原「筛选 + 台账」能力收编于此（类型 / 质量 / 关键字 / 重置），作用于当前选中设备 -->
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
        </div>
      </div>
    </section>

    <div class="pt-split">
      <!-- ══ 左：设备列表 ══ -->
      <section class="wc-card">
        <div class="wc-card__head">
          <h3>设备列表</h3>
          <span class="wc-card__sub">搜索后点选，右侧维护该设备</span>
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
            <p v-if="filteredDevices.length === 0" class="pt-list__empty">
              没有匹配的设备（关键词「{{ deviceKw }}」）。
            </p>
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

          <!-- ── 点位表单（新增 / 编辑共用；写操作一律 RoleGate 已在按钮侧控制）── -->
          <div v-if="pointForm" class="pt-form">
            <p class="pt-form__title">
              {{ pointForm.id ? `编辑点位：${pointForm.id}` : `为「${selected.name}」新增点位` }}
            </p>
            <div class="dv-form">
              <UiField label="点位类型">
                <UiSelect v-model="pointForm.pointType" :options="pointTypeOptions" />
              </UiField>
              <UiField label="点位名" required :error="formTouched && !formValid.name ? '必填' : ''">
                <UiInput v-model="pointForm.name" placeholder="如 料筒温度1" />
              </UiField>
              <UiField
                label="地址"
                :required="pointForm.pointType === 'physical'"
                :hint="pointForm.pointType === 'physical' ? addrStyleText : '计算点无 PLC 地址'"
                :error="formTouched && !formValid.address ? '物理点地址必填' : ''"
              >
                <UiInput v-model="pointForm.address" :disabled="pointForm.pointType === 'derived'" placeholder="如 40001 / DB1.0" />
              </UiField>
              <UiField label="数据类型" :error="formTouched && !formValid.dataType ? `须为 ${DATA_TYPE_OPTIONS.join(' / ')}` : ''">
                <UiSelect v-model="pointForm.dataType" :options="dataTypeOptions" />
              </UiField>
              <UiField
                label="字节序"
                :hint="pointForm.pointType === 'derived' ? '计算点无字节序' : ''"
                :error="formTouched && !formValid.byteOrder ? `须为 ${BYTE_ORDER_OPTIONS.join(' / ')}` : ''"
              >
                <UiSelect v-model="pointForm.byteOrder" :options="byteOrderOptions" :disabled="pointForm.pointType === 'derived'" />
              </UiField>
              <UiField label="单位" :error="formTouched && !formValid.unit ? '必填' : ''">
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
                <UiInput v-model="pointForm.targetKey" placeholder="如 M_Temp1" />
              </UiField>
              <UiField
                v-if="pointForm.pointType === 'derived'"
                label="公式"
                full
                required
                hint="本页只做原文保存；公式的权威校验与求值在网关侧，不通过会以质量码 CalcFailed 体现"
                :error="formTouched && !formValid.formula ? '计算点公式必填' : ''"
              >
                <UiInput v-model="pointForm.formula" placeholder="如 M_Good / M_Count * 100" />
              </UiField>
            </div>
            <p class="wc-note">
              <span class="wc-note__icon" aria-hidden="true">ⓘ</span>
              <span>
                {{
                  pointForm.id
                    ? '编辑以「删除 + 重建」落到数据层（现有数据层未开放按字段更新的写接口），点位标识会变化并写入审计。'
                    : '保存后立即对该设备生效；北向字段同步使用该「目标点名」。'
                }}
              </span>
            </p>
            <div class="dv-foot">
              <button type="button" class="wc-btn wc-btn--primary wc-btn--sm" @click="submitPointForm">保存点位</button>
              <button type="button" class="wc-btn wc-btn--sm" @click="pointForm = null">取消</button>
            </div>
          </div>

          <!-- ── 操作反馈 ── -->
          <div v-if="lastNote" class="pt-note">
            <span aria-hidden="true">{{ lastNoteKind === 'warn' ? '⚠' : '✓' }}</span>
            <span>{{ lastNote }}</span>
          </div>

          <!-- ── 页签 0：点表 ── -->
          <template v-if="paneTab === 0">
            <EmptyState
              v-if="selectedPoints.length === 0"
              :title="`${selected.name} 尚未配置点表`"
              :desc="`设备已保存，但还没有点位映射 —— 没有点表，这台设备采不到任何数据。可导入 CSV，或用 ${selected.protocolLabel} 协议模板先生成、再逐点校准地址。`"
            >
              <template #actions>
                <RoleGate :allowed="canWrite">
                  <button type="button" class="wc-btn wc-btn--primary" @click="triggerImport">导入 CSV 点表</button>
                  <button type="button" class="wc-btn" @click="applyProtocolTemplate">使用 {{ selected.protocolLabel }} 协议模板</button>
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
                <template #cell-quality="{ row }">
                  <span class="wc-tag" :class="qualityTagClass(row.quality)">{{ row.quality }}</span>
                </template>
                <template #cell-sim="{ row }">
                  <span v-if="row.pointType === 'derived'" class="pt-dim">—</span>
                  <span v-else class="pt-dim">未启用</span>
                </template>
                <template #cell-simRange>
                  <span class="pt-dim">—</span>
                </template>
                <template #cell-simDec>
                  <span class="pt-dim">—</span>
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
              <p class="pt-foot-note">
                模拟 / 模拟范围 / 小数位三列在<b>模拟策略（P0-4）立项前为占位</b>，统一显示「—」；
                当前值为网关真实采集值（换算与质量判定在网关侧）。
              </p>
            </template>
          </template>

          <!-- ── 页签 1：点表信息 ── -->
          <div v-else-if="paneTab === 1" class="pt-pad">
            <p v-if="selectedPoints.length === 0" class="wc-note">
              <span class="wc-note__icon" aria-hidden="true">ⓘ</span>
              <span>该设备尚未配置点表，暂无点表信息。可先导入点表或使用协议模板生成。</span>
            </p>
            <template v-else>
              <dl class="wc-kv">
                <dt>地址风格</dt><dd class="wc-mono">{{ addrStyleText }}</dd>
                <dt>点表来源</dt>
                <dd>
                  <span class="wc-tag wc-tag--info">未记录</span>
                  <span class="pt-dd-hint">当前数据层不保存点表来源（CSV 导入 / 协议模板 / 手动新增），因此此处不猜测。</span>
                </dd>
                <dt>点位数量</dt><dd class="wc-mono">{{ selectedPoints.length }} 个（含 {{ calcCount }} 个公式点）</dd>
                <dt>地址区间</dt><dd class="wc-mono">{{ addrRangeText }}</dd>
                <dt>target 前缀</dt><dd class="wc-mono">{{ targetPrefixText }}</dd>
                <dt>改点表影响</dt><dd>同步更新设备点位数与北向转发字段</dd>
              </dl>
              <p class="wc-note">
                <span class="wc-note__icon" aria-hidden="true">ⓘ</span>
                <span>本页所有信息均由<b>现有点位记录</b>派生，不另设第二套元数据，避免两处口径打架。</span>
              </p>
            </template>
          </div>

          <!-- ── 页签 2：映射链路（骨架 + 诚实空态）── -->
          <div v-else-if="paneTab === 2" class="pt-pad">
            <p class="pt-sub-h">设备 → 地址区间 → 点位 → 北向 target key</p>
            <EmptyState
              title="映射链路可视化：规划中"
              desc="「设备 / 点位 / 北向出口」的三段链路尚未立项（当前也没有可供绘制的结构化映射关系），因此这里不画示意链路图。现已确认的两段事实如下，可作核对依据。"
            >
              <template #actions>
                <button type="button" class="wc-btn" @click="paneTab = 0">回到点表</button>
                <button type="button" class="wc-btn" @click="go('rules')">查看转发规则</button>
              </template>
            </EmptyState>
            <dl class="wc-kv">
              <dt>设备</dt><dd class="wc-mono">{{ selected.name }} · {{ selected.id }}</dd>
              <dt>地址区间</dt><dd class="wc-mono">{{ addrRangeText }}</dd>
              <dt>点数</dt><dd class="wc-mono">{{ selectedPoints.length }}</dd>
              <dt>北向字段</dt><dd class="wc-mono">{{ selectedPoints.map((p) => p.targetKey).slice(0, 8).join('、') || '—' }}</dd>
            </dl>
          </div>

          <!-- ── 页签 3：点位模拟（诚实占位）── -->
          <div v-else class="pt-pad">
            <EmptyState
              title="点位模拟：P0-4 尚未立项"
              desc="逐点模拟（模式 / 范围 / 小数位 / 开关）属于待立项能力，未提供任何入口，也不可能伪造模拟值。当前可看真实采集值：实时数据页按 1s 节流渲染各点位曲线。"
            >
              <template #actions>
                <button type="button" class="wc-btn wc-btn--primary" @click="goLive(selected.id)">看真实采集值</button>
                <button type="button" class="wc-btn" @click="go('northbound')">看北向转发口径</button>
              </template>
            </EmptyState>
            <dl class="wc-kv">
              <dt>当前数据量</dt><dd class="wc-mono">{{ selectedPoints.length }} 点</dd>
              <dt>模拟配置</dt><dd><span class="wc-tag wc-tag--unknown">无（未启用模拟）</span></dd>
            </dl>
          </div>
        </template>
      </section>
    </div>

    <!-- 导入校验结果 -->
    <section v-if="importResult" class="wc-card">
      <div class="wc-card__head">
        <h3>导入校验结果 · {{ importDeviceName }}</h3>
        <span class="wc-card__sub">合法 {{ importResult.validRows.length }} 条 · 异常 {{ importResult.errors.length }} 条</span>
      </div>
      <div class="wc-card__body">
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

    <div v-if="importDone" class="wc-banner wc-banner--ok">
      <span aria-hidden="true">✓</span>
      <span>已按设备覆盖导入 {{ importDone }} 个点位。</span>
    </div>
  </div>

  <DangerConfirmModal
    :open="!!pendingDelete"
    :title="pendingDelete ? `删除点位：${pendingDelete.name}` : '删除点位'"
    :confirm-value="pendingDelete ? pendingDelete.id : ''"
    confirm-label="风险二次确认（输入点位标识后 8 位）"
    confirm-placeholder="输入点位标识后 8 位"
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
 * @description 点位与映射：主从布局（左设备列表 / 右四页签）。
 *
 * · 左列表：仅搜索框筛选 + 固定高度滚动 + 分页；点即切换右侧；
 * · 右页签：**点表**（12 列 + 行内操作）、**点表信息**（由现有点位派生）、
 *   **映射链路 / 点位模拟**（骨架 + 诚实空态 —— 未立项，不伪造）；
 * · 未配点表的设备给设备级空态 + 三个入口（导入 / 协议模板 / 手动新增）；
 * · CSV 导入：校验到「行号 + 原因 + 允许值」，fail-closed（有错整批不写）；
 * · 写操作全部走既有 `repo.*`（内存态 / 本地覆盖层 + 审计），签名不变。
 */
import { computed, onMounted, ref, watch } from 'vue';
import { useRoute, useRouter } from 'vue-router';
import {
  PageHeader,
  UiTable,
  UiPager,
  UiSelect,
  UiInput,
  UiField,
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

/** 表格列（12 列：对齐原型 :1077-1083；原型的首列勾选框因无批量能力实现，换成「类型」）。 */
const columns: readonly TableColumn[] = [
  { key: 'pointType', label: '类型' },
  { key: 'name', label: '点位名称' },
  { key: 'address', label: '地址', mono: true },
  { key: 'dataType', label: '数据类型', mono: true },
  { key: 'byteOrder', label: '字节序', mono: true },
  { key: 'unit', label: '单位' },
  { key: 'targetKey', label: '目标点位', mono: true },
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

/** 设备状态中文（列表行内小字；与 status-map 的 online / offline / error 文案保持一致）。 */
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
    if (kw && !`${p.name} ${p.targetKey} ${p.address}`.toLowerCase().includes(kw)) {
      return false;
    }
    return true;
  });
});

/** 当前页点位（补出模拟三列占位，避免 UI 里做假数据）。 */
const items = computed(() => {
  const start = (page.value - 1) * PAGE_SIZE;
  return filteredPoints.value.slice(start, start + PAGE_SIZE).map((p) => ({
    ...p,
    sim: '',
    simRange: '',
    simDec: '',
  }));
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

onMounted(() => {
  reload();
  const fromDevice = route.query.device;
  const devicesSnapshot = devices.value;
  if (typeof fromDevice === 'string' && fromDevice && devicesSnapshot.some((d) => d.id === fromDevice)) {
    selectedDeviceId.value = fromDevice;
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
  return raw.length ? `${raw[0].address} – ${raw[raw.length - 1].address}` : '—';
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
// 协议模板（第 3 动作的起点，务必再校准）
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

/** 结果区提示（保留一小段时间后自动收起）。 */
function note(text: string, kind: 'ok' | 'warn' = 'ok'): void {
  lastNote.value = text;
  lastNoteKind.value = kind;
}

/** 按协议模板生成起点点位（追加，不覆盖现有点表）。 */
function applyProtocolTemplate(): void {
  const dev = selected.value;
  if (!dev) {
    return;
  }
  const rows = PT_TEMPLATE[dev.protocol] ?? [];
  if (rows.length === 0) {
    note(`${dev.protocolLabel} 暂无协议模板（P0 未覆盖该协议的起始点表）。`, 'warn');
    return;
  }
  let created = 0;
  for (const row of rows) {
    const draft: PointDraft = {
      deviceId: dev.id,
      name: row.name,
      pointType: 'physical',
      address: row.addr,
      dataType: row.dt,
      byteOrder: row.bo === '—' ? 'AB CD' : (row.bo as PointDraft['byteOrder']),
      unit: '',
      deadband: 0,
      targetKey: `${dev.id}_${row.addr.replace(/[^A-Za-z0-9.]/g, '_')}`,
      formula: null,
      actor: session.state.displayName,
    };
    repo.createPoint(draft);
    created += 1;
  }
  reload();
  note(
    `已按 ${dev.protocolLabel} 模板追加 ${created} 个起点点位（单位为空、目标点名由设备 ID + 地址生成）。` +
      '模板只解决「地址风格」，数量与含义必须对照手册逐条校准后才能上线。',
    'warn',
  );
}

// ---------------------------------------------------------------------------
// 点位表单（新增 / 编辑）
// ---------------------------------------------------------------------------

interface PointForm {
  /** 编辑时携带原记录 id；新增为空串 */
  id: string;
  pointType: 'physical' | 'derived';
  name: string;
  address: string;
  dataType: string;
  byteOrder: string;
  unit: string;
  deadband: string;
  targetKey: string;
  formula: string;
}

const pointForm = ref<PointForm | null>(null);
const formTouched = ref(false);

/** 表单校验结果（逐字段，实时驱动 saves）。 */
const formValid = computed(() => {
  const f = pointForm.value;
  if (!f) {
    return {
      name: false,
      address: false,
      dataType: false,
      byteOrder: false,
      unit: false,
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
    dataType: DATA_TYPE_OPTIONS.includes(f.dataType),
    byteOrder: !isPhysical || (BYTE_ORDER_OPTIONS as readonly string[]).includes(f.byteOrder),
    unit: f.unit.trim().length > 0,
    deadband: f.deadband.trim() !== '' && Number.isFinite(deadband) && deadband >= 0,
    targetKey:
      f.targetKey.trim().length > 0 &&
      selectedPoints.value.every((p) => p.id === f.id || p.targetKey !== f.targetKey.trim()),
    formula: isPhysical || f.formula.trim().length > 0,
  };
});

const formOk = computed(() => {
  const v = formValid.value;
  return v.name && v.address && v.dataType && v.byteOrder && v.unit && v.deadband && v.targetKey && v.formula;
});

/** 打开表单（row 为空 = 新增）。 */
function openPointForm(row: PointRecord | null): void {
  formTouched.value = false;
  pointForm.value = row
    ? {
        id: row.id,
        pointType: row.pointType,
        name: row.name,
        address: row.address === '—' ? '' : row.address,
        dataType: row.dataType,
        byteOrder: row.byteOrder === '—' ? 'AB CD' : row.byteOrder,
        unit: row.unit,
        deadband: String(row.deadband),
        targetKey: row.targetKey,
        formula: row.formula ?? '',
      }
    : {
        id: '',
        pointType: 'physical',
        name: '',
        address: '',
        dataType: 'uint16',
        byteOrder: 'AB CD',
        unit: '',
        deadband: '0',
        targetKey: '',
        formula: '',
      };
}

/** 保存点位：新增走 `createPoint`；编辑 = 删除 + 重建（数据层无按字段更新接口）。 */
function submitPointForm(): void {
  formTouched.value = true;
  const f = pointForm.value;
  const dev = selected.value;
  if (!f || !dev || !formOk.value) {
    return;
  }
  const isPhysical = f.pointType === 'physical';
  const draft: PointDraft = {
    deviceId: dev.id,
    name: f.name.trim(),
    pointType: f.pointType,
    address: isPhysical ? f.address.trim() : '',
    dataType: f.dataType,
    byteOrder: isPhysical ? (f.byteOrder as PointDraft['byteOrder']) : '—',
    unit: f.unit.trim(),
    deadband: Number(f.deadband),
    targetKey: f.targetKey.trim(),
    formula: isPhysical ? null : f.formula.trim(),
    actor: session.state.displayName,
  };
  if (f.id) {
    repo.deletePoint({ id: f.id, actor: session.state.displayName });
  }
  const created = repo.createPoint(draft);
  pointForm.value = null;
  reload();
  note(
    f.id
      ? `点位已更新为新记录 ${created.id}（删除 + 重建，审计两条记录）；原记录的北向映射请同步核对。`
      : `点位「${created.name}」已新增到「${dev.name}」。`,
    f.id ? 'warn' : 'ok',
  );
}

/** 复制点位（除名称与目标点名外照搬，需二次确认落南向前的语义）。 */
function copyPoint(row: PointRecord): void {
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
  const draft: PointDraft = {
    deviceId: dev.id,
    name: `${row.name} 副本`,
    pointType: row.pointType,
    address: row.address === '—' ? '' : row.address,
    dataType: row.dataType,
    byteOrder: row.byteOrder === '—' ? '—' : row.byteOrder,
    unit: row.unit,
    deadband: row.deadband,
    targetKey,
    formula: row.formula,
    actor: session.state.displayName,
  };
  const created = repo.createPoint(draft);
  reload();
  note(`已复制为「${created.name}」（目标点名 ${created.targetKey}）。复制点是起点，请改掉地址与目标点名后再用。`, 'warn');
}

/** 公式入口：公式编辑器尚未立项，给出诚实说明而不是假装已实现校验。 */
function openFormula(row: PointRecord): void {
  if (row.pointType === 'derived') {
    note(
      `「${row.name}」当前公式：${row.formula || '（空）'}。公式编辑器（语法校验 / 变量联想 / 试算）规划中，` +
        '可用「编辑」直接改原文，权威校验与求值在网关侧（不通过会呈现 CalcFailed）。',
      'warn',
    );
    return;
  }
  note(
    `「${row.name}」是物理点，本身不带公式。若需派生量，请把新点位建成「计算点」再写公式；` +
      '公式编辑器规划中，当前仅支持原文录入。',
    'warn',
  );
}

// ---------------------------------------------------------------------------
// CSV 导入 / 导出
// ---------------------------------------------------------------------------

/** 新格式表头（导入 / 导出一致：导出即可当导入模板）。 */
const CSV_HEADER = ['地址', '点位名', '数据类型', '字节序', '单位', '死区', '目标点名', '公式'];
/** 旧格式表头（含设备列，向后兼容此前导出的文件）。 */
const CSV_HEADER_LEGACY = ['设备', '点位名', '类型', '地址', '数据类型', '字节序', '单位', '死区', '北向目标点名', '公式'];

interface ImportError {
  /** 行号（表体行号，从 1 开始） */
  line: number;
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

/** 解析 CSV 为二维数组（按行、按逗号；不处理引号内逗号，原型足够）。 */
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
  const hasHeader = legacy || head === CSV_HEADER.join(',');
  const data = hasHeader ? rows.slice(1) : rows;

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

    const pointType: PointDraft['pointType'] =
      typeRaw === 'physical' ? 'physical' : typeRaw === 'derived' || typeRaw === 'calc' ? 'derived' : formula ? 'derived' : 'physical';

    const push = (reason: string, allowed: string): void => {
      errors.push({ line, reason, allowed });
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
    if (!unit) {
      push('单位必填', '任意非空文本（无量纲填 无）');
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

    if (errors.some((e) => e.line === line)) {
      return;
    }

    seenTargets.add(targetKey);
    validRows.push({
      deviceId: dev.id,
      name,
      pointType,
      address: pointType === 'physical' ? address : '',
      dataType,
      byteOrder: pointType === 'physical' ? (byteOrder as PointDraft['byteOrder']) : '—',
      unit,
      deadband,
      targetKey,
      formula: pointType === 'derived' ? formula : null,
      actor: session.state.displayName,
    });
  });

  return { deviceId: dev.id, validRows, errors };
}

/** 提交导入（按设备覆盖写库）。 */
function commitImport(): void {
  const result = importResult.value;
  if (!result || result.validRows.length === 0) {
    return;
  }
  const count = repo.replacePointsOfDevice({
    deviceId: result.deviceId,
    rows: result.validRows,
    actor: session.state.displayName,
  });
  importResult.value = null;
  importDone.value = count;
  page.value = 1;
  reload();
}

function clearImport(): void {
  importResult.value = null;
}

/** 导出点表 CSV（选中设备 → 该设备；否则导出全部，便于整体核对）。 */
function exportTemplate(): void {
  const rows: string[][] = [CSV_HEADER];
  const dev = selected.value;
  const source = dev ? repo.pointsOfDevice(dev.id) : repo.allPoints();
  if (source.length === 0) {
    const sampleAddr = dev ? (PT_TEMPLATE[dev.protocol]?.[0]?.addr ?? '40001') : '40001';
    rows.push([sampleAddr, '示例点位', 'uint16', 'CD AB', '℃', '0', 'example_point', '']);
  } else {
    for (const p of source) {
      rows.push([
        p.address === '—' ? '' : p.address,
        p.name,
        p.dataType,
        p.byteOrder === '—' ? '' : p.byteOrder,
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
  a.download = dev ? `points-${dev.id}.csv` : 'points-all.csv';
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
function confirmDelete(): void {
  if (!pendingDelete.value) {
    return;
  }
  repo.deletePoint({ id: pendingDelete.value.id, actor: session.state.displayName });
  pendingDelete.value = null;
  reload();
}

// ---------------------------------------------------------------------------
// 导航
// ---------------------------------------------------------------------------

function go(name: string): void {
  void router.push({ name });
}

/** 查看该设备的实时数据。 */
function goLive(deviceId: string): void {
  void router.push({ name: 'live', query: { device: deviceId } });
}

watch(selectedDeviceId, () => {
  paneTab.value = 0;
  pointForm.value = null;
  lastNote.value = '';
});
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

/* 左列：固定高度滚动 + 分页（原型 :620-621） */
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

/* 点位表单 */
.pt-form {
  padding: 16px 18px;
  border-bottom: 1px solid var(--divider);
  background: var(--bg-hover);
}
.pt-form__title {
  margin: 0 0 12px;
  font-size: var(--fs-body);
  font-weight: 600;
  color: var(--text-1);
}
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
.dv-foot__hint {
  font-size: var(--fs-caption);
  color: var(--text-3);
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

/* 页签内容容器 */
.pt-pad {
  padding: 16px 18px;
}
.pt-sub-h {
  margin: 0 0 10px;
  font-size: var(--fs-caption);
  font-weight: 600;
  color: var(--text-3);
}
.pt-dim {
  color: var(--text-3);
  opacity: 0.6;
  font-size: var(--fs-caption);
}
.pt-dd-hint {
  margin-left: 8px;
  font-size: var(--fs-caption);
  color: var(--text-3);
}
.pt-ops {
  display: inline-flex;
  gap: 6px;
  white-space: nowrap;
}
.pt-foot-note {
  margin: 8px 16px 14px;
  font-size: var(--fs-caption);
  color: var(--text-3);
  line-height: 1.6;
}

/* 导入错误表 */
.pt-err-wrap {
  max-height: 220px;
  overflow: auto;
  border: 1px solid var(--divider);
  border-radius: var(--radius-sm);
  margin-bottom: 12px;
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

<template>
  <!--
    DevicesPage —— 设备接入（接入分组第 1 页，路由 `/devices`）。

    约定：
      · 列表全部分页（UiPager），条数只有分页条一个口径；
      · KPI（总数 / 在线 / 离线故障 / 点位）来自全量设备，与筛选无关；
      · 分组列来自 `GET /api/groups`（缺省回落「默认分组」）；运行状态展示后端真实
        `status` + `last_sample_at` + `offlineText`/`unknownText` 中的真实原因；
      · 分组管理：默认分组不可删除 / 改名；删除按二次确认（非空分组设备回落默认分组，由后端处理）；
      · 写操作（新增 / 删除 / 分组）受 RoleGate 控制（工程师及以上）；授权判定不在前端。
  -->
  <div class="wc-content">
    <!-- KPI 卡片行 -->
    <div class="wc-grid wc-grid--4">
      <StatCard label="设备总数" :value="String(kpi.total)" unit="台" icon-tone="ink">
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
        <span class="wc-card__ops">
          <button type="button" class="wc-btn wc-btn--sm" data-test="group-manage" @click="openGroupModal">
            分组管理
          </button>
          <RoleGate :allowed="canWrite">
            <button type="button" class="wc-btn wc-btn--sm wc-btn--primary" @click="go('device-new')">新增设备</button>
          </RoleGate>
        </span>
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
          <div class="wc-filters__item">
            <label>分组</label>
            <UiSelect v-model="groupFilter" :options="groupFilterOptions" />
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

      <EmptyState v-if="items.length === 0" title="暂无设备">
        <template #actions>
          <button v-if="hasActiveFilter" type="button" class="wc-btn" @click="resetFilters">清空筛选</button>
          <RoleGate :allowed="canWrite">
            <button type="button" class="wc-btn wc-btn--primary" @click="go('device-new')">新增设备</button>
          </RoleGate>
        </template>
      </EmptyState>

      <template v-else>
        <UiTable :columns="columns" :rows="items" row-key-field="id">
          <template #cell-groupName="{ row }">
            <span class="wc-tag wc-tag--info">{{ row.groupName }}</span>
          </template>
          <template #cell-protocolLabel="{ row }">
            <span class="wc-tag wc-tag--info">{{ row.protocolLabel }}</span>
          </template>
          <template #cell-runStatus="{ row }">
            <div class="dv-run">
              <StatusTag :status="runStatusOf(row)" />
              <span class="dv-run__meta wc-mono">{{ row.lastSampleAt }}</span>
              <span v-if="runReason(row)" class="dv-run__reason">{{ runReason(row) }}</span>
            </div>
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

        <!-- 表格 foot：批量连通性探测 -->
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

        <!-- 逐台探测结果（后端结构化结果） -->
        <div v-if="probeResults.length > 0" class="dv-probe">
          <p class="dv-probe__head" data-test="probe-summary">{{ probeSummary }}</p>
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

  <!-- ══ 分组管理弹窗 ══ -->
  <Teleport to="body">
    <div v-if="groupModalOpen" class="gpm__mask" @click.self="closeGroupModal">
      <div class="gpm" role="dialog" aria-modal="true" aria-label="分组管理" data-test="group-modal">
        <div class="gpm__head">
          <h3>分组管理</h3>
          <button type="button" class="wc-icon-btn" aria-label="关闭" @click="closeGroupModal">✕</button>
        </div>
        <div class="gpm__body">
          <div v-if="groupError" class="wc-banner wc-banner--danger">
            <span aria-hidden="true">!</span>
            <span>{{ groupError }}</span>
          </div>

          <div class="gpm__add">
            <UiInput v-model="newGroupName" placeholder="新分组名称" />
            <button
              type="button"
              class="wc-btn wc-btn--primary wc-btn--sm"
              :disabled="!newGroupName.trim() || groupBusy"
              data-test="group-create"
              @click="createGroup"
            >
              新增分组
            </button>
          </div>

          <ul class="gpm__list">
            <li v-for="g in mergedGroups" :key="g.id" class="gpm__row" :data-testid="`group-row-${g.id}`">
              <template v-if="editingId === g.id">
                <UiInput v-model="editingName" />
                <span class="gpm__ops">
                  <button type="button" class="wc-btn wc-btn--sm wc-btn--primary" :disabled="groupBusy" @click="saveRename(g)">
                    保存
                  </button>
                  <button type="button" class="wc-btn wc-btn--sm" @click="editingId = ''">取消</button>
                </span>
              </template>
              <template v-else>
                <span class="gpm__name">
                  {{ g.name }}
                  <span v-if="g.isDefault" class="wc-tag wc-tag--info">默认分组</span>
                </span>
                <span class="gpm__count wc-mono">{{ g.deviceCount }} 台</span>
                <span class="gpm__ops">
                  <template v-if="g.isDefault">
                    <span class="gpm__reason">默认分组不可改名 / 删除（设备回落目标）</span>
                    <button type="button" class="wc-btn wc-btn--sm" disabled :data-testid="`group-rename-${g.id}`">
                      重命名
                    </button>
                    <button
                      type="button"
                      class="wc-btn wc-btn--sm wc-btn--danger"
                      disabled
                      :data-testid="`group-delete-${g.id}`"
                    >
                      删除
                    </button>
                  </template>
                  <template v-else>
                    <button
                      type="button"
                      class="wc-btn wc-btn--sm"
                      :disabled="groupBusy"
                      :data-testid="`group-rename-${g.id}`"
                      @click="startRename(g)"
                    >
                      重命名
                    </button>
                    <template v-if="deletingId === g.id">
                      <span class="gpm__confirm">确认删除「{{ g.name }}」？</span>
                      <button
                        type="button"
                        class="wc-btn wc-btn--sm wc-btn--danger"
                        :disabled="groupBusy"
                        :data-testid="`group-delete-confirm-${g.id}`"
                        @click="removeGroup(g)"
                      >
                        确认删除
                      </button>
                      <button type="button" class="wc-btn wc-btn--sm" @click="deletingId = ''">取消</button>
                    </template>
                    <button
                      v-else
                      type="button"
                      class="wc-btn wc-btn--sm wc-btn--danger"
                      :disabled="groupBusy"
                      :data-testid="`group-delete-${g.id}`"
                      @click="deletingId = g.id"
                    >
                      删除
                    </button>
                  </template>
                </span>
              </template>
            </li>
          </ul>

          <div v-if="groupNote" class="gpm__note" :class="groupNoteKind === 'warn' ? 'is-warn' : 'is-ok'">
            {{ groupNote }}
          </div>
        </div>
        <div class="gpm__foot">
          <button type="button" class="wc-btn" @click="closeGroupModal">关闭</button>
        </div>
      </div>
    </div>
  </Teleport>

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
 * @description 设备接入列表页：筛选 / 分页 / KPI / 分组管理 / 删除确认。
 *
 * 数据来自 `repo.allDevices()`（真实缓存），筛选与分页在页面内派生（含分组筛选，
 * 使「条数」仍只有分页条一个口径）。写操作经 `repo.deleteDevice` / `repo.groups.*`，
 * 失败即如实展示真实原因。前端不持有任何授权判定。
 */
import { computed, onMounted, ref, watch } from 'vue';
import { useRouter } from 'vue-router';
import {
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
import { dataVersion, repo, PROTOCOL_OPTIONS, type DeviceGroup, type DeviceRecord } from '@/api/repo';
import { session } from '../store/session';

const router = useRouter();

/** 每页条数（UiPager 单一口径）。 */
const PAGE_SIZE = 8;

/** 分组筛选中的「默认分组」哨兵值。 */
const DEFAULT_GROUP = '__default__';

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

/** 表格行：设备记录 + 派生分组名。 */
interface DeviceRow extends DeviceRecord {
  /** 分组显示名（未知 / 未分组回落「默认分组」）。 */
  groupName: string;
}

/** 表格列。 */
const columns: readonly TableColumn[] = [
  { key: 'name', label: '设备' },
  { key: 'groupName', label: '分组' },
  { key: 'protocolLabel', label: '协议' },
  { key: 'connectionSummary', label: '连接' },
  { key: 'runStatus', label: '运行状态' },
  { key: 'pointCount', label: '点位', align: 'right', mono: true },
  { key: 'successRate', label: '成功率', align: 'right' },
  { key: 'failStreak', label: '连续失败', align: 'right', mono: true },
];

/** 是否可写（工程师及以上）。 */
const canWrite = computed(() => session.state.role === 'admin' || session.state.role === 'engineer');

// ---------------------------------------------------------------------------
// 数据源
// ---------------------------------------------------------------------------

/** 全量设备（真实缓存）。 */
const allDevices = ref<DeviceRecord[]>([]);
/** 真实分组清单（含默认分组）。 */
const groups = ref<DeviceGroup[]>([]);
/** 分组清单不可得原因（空串 = 正常）。 */
const groupError = ref('');

/** 筛选与分页状态。 */
const statusFilter = ref('');
const protocolFilter = ref('');
const groupFilter = ref('');
const keyword = ref('');
const page = ref(1);

/** 刷新全量设备与分组。 */
async function reload(): Promise<void> {
  allDevices.value = repo.allDevices();
  groups.value = await repo.groups.list();
  groupError.value = repo.actions.notices().groups || '';
}

/** 分组 id → 名称（未知返回空串，交由展示层回落「默认分组」）。 */
const groupNames = computed<Record<string, string>>(() => {
  const map: Record<string, string> = {};
  for (const g of groups.value) {
    map[g.id] = g.name;
  }
  return map;
});

/** 设备所属分组显示名（未分组 / 未知 → 默认分组）。 */
function groupNameOf(groupId: string): string {
  if (groupId && groupNames.value[groupId]) {
    return groupNames.value[groupId];
  }
  return '默认分组';
}

/** 未分组 / 未知分组的设备数（默认分组兜底行的计数）。 */
const ungroupedCount = computed(
  () => allDevices.value.filter((d) => !d.groupId || !groupNames.value[d.groupId]).length,
);

/**
 * 分组清单（契约保证必含默认分组）。
 *
 * 后端未返回默认分组时，前端补一条**只读**默认分组兜底行（不可改名 / 删除），
 * 以保证「设备回落目标」始终可见；设备数按未分组设备实时统计。
 */
const mergedGroups = computed<readonly DeviceGroup[]>(() => {
  const hasDefault = groups.value.some((g) => g.isDefault || g.id === 'default');
  if (hasDefault) {
    return groups.value;
  }
  return [
    { id: 'default', name: '默认分组', deviceCount: ungroupedCount.value, isDefault: true },
    ...groups.value,
  ];
});

/** 分组筛选选项。 */
const groupFilterOptions = computed<readonly SelectOption[]>(() => [
  { value: '', label: '全部分组' },
  { value: DEFAULT_GROUP, label: '默认分组' },
  ...groups.value.filter((g) => !g.isDefault && g.id !== 'default').map((g) => ({ value: g.id, label: g.name })),
]);

/** 当前筛选后的全量设备。 */
const filtered = computed<readonly DeviceRecord[]>(() => {
  const kw = keyword.value.trim().toLowerCase();
  return allDevices.value.filter((d) => {
    if (statusFilter.value && d.status !== statusFilter.value) {
      return false;
    }
    if (protocolFilter.value && d.protocol !== protocolFilter.value) {
      return false;
    }
    if (groupFilter.value === DEFAULT_GROUP) {
      if (d.groupId && groupNames.value[d.groupId]) {
        return false;
      }
    } else if (groupFilter.value && d.groupId !== groupFilter.value) {
      return false;
    }
    if (kw) {
      const haystack = `${d.name} ${d.connectionSummary} ${d.protocolLabel}`.toLowerCase();
      if (!haystack.includes(kw)) {
        return false;
      }
    }
    return true;
  });
});

/** 总条数（分页条唯一口径）。 */
const total = computed(() => filtered.value.length);
const totalPages = computed(() => Math.max(1, Math.ceil(total.value / PAGE_SIZE)));

/** 当前页行（补出分组显示名）。 */
const items = computed<readonly DeviceRow[]>(() =>
  filtered.value
    .slice((page.value - 1) * PAGE_SIZE, (page.value - 1) * PAGE_SIZE + PAGE_SIZE)
    .map((d) => ({ ...d, groupName: groupNameOf(d.groupId) })),
);

/** 是否有生效中的筛选条件。 */
const hasActiveFilter = computed(
  () => statusFilter.value !== '' || protocolFilter.value !== '' || groupFilter.value !== '' || keyword.value.trim() !== '',
);

/** KPI 视图模型（仅统计**后端真实上报**状态的设备：未上报既不算在线也不算离线）。 */
const kpi = computed(() => {
  const list = allDevices.value;
  const online = list.filter((d) => statusReported(d) && d.status === 'online').length;
  const abnormal = list.filter((d) => statusReported(d) && d.status !== 'online').length;
  return {
    total: list.length,
    online,
    abnormal,
    points: list.reduce<number>((s, d) => s + d.pointCount, 0),
  };
});

/** 后端是否真的上报了 `status`（未上报时 `unknownText` 含 `status`）。 */
function statusReported(row: DeviceRecord): boolean {
  return !row.unknownText.includes('status');
}

/** 状态标签入参：后端未上报时传 `null`（渲染灰色「—」），**绝不默认离线**。 */
function runStatusOf(row: DeviceRecord): string | null {
  return statusReported(row) ? row.status : null;
}

/** 运行状态补充原因（后端未上报 / 离线原因；在线且无异常时为空串）。 */
function runReason(row: DeviceRecord): string {
  const parts: string[] = [];
  if (statusReported(row) && row.status !== 'online' && row.offlineText) {
    parts.push(row.offlineText);
  }
  if (row.unknownText) {
    parts.push(row.unknownText);
  }
  return parts.join(' · ');
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
function onPage(nextPage: number): void {
  page.value = nextPage;
}

/** 重置筛选（回到第 1 页）。 */
function resetFilters(): void {
  statusFilter.value = '';
  protocolFilter.value = '';
  groupFilter.value = '';
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

/** 「实时数据」下钻：进入实时点位值页并预选该设备。 */
function goLive(deviceId: string): void {
  void router.push({ name: 'live', query: { device: deviceId } });
}

/** 「编辑」：进入新增 / 编辑设备页并带入该设备。 */
function goEdit(deviceId: string): void {
  void router.push({ name: 'device-new', query: { device: deviceId } });
}

// ---------------------------------------------------------------------------
// 表格 foot：配额口径 + 批量连通性探测
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
  /** 原因（后端原文） */
  reason: string;
  /** 耗时毫秒（**字符串透传**，绝不 parseInt） */
  elapsedMs: string;
  /** 行色条（失败行左侧红色条） */
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

/** 探测汇总文案。 */
const probeSummary = computed<string>(() => {
  const okCount = probeResults.value.filter((r) => r.ok).length;
  return `测试全部连接：${okCount} / ${probeResults.value.length} 台连通 · 数据源 POST /api/devices/test`;
});

/** 逐台探测连通性（后端结构化失败不 500）。 */
async function testAllConnections(): Promise<void> {
  if (testingAll.value) {
    return;
  }
  testingAll.value = true;
  const results: ProbeResult[] = [];
  try {
    for (const device of allDevices.value) {
      try {
        const pr = await repo.actions.testDevice({ deviceId: device.id });
        results.push({
          id: device.id,
          name: device.name,
          ok: pr.ok,
          errorKind: pr.ok ? '' : pr.errorKind,
          reason: pr.ok ? pr.reason || '探测成功' : pr.reason || '探测失败（后端未给出原因）',
          elapsedMs: pr.elapsedMs || '—',
          _rowClass: pr.ok ? '' : 'row-danger',
        });
      } catch (cause) {
        results.push({
          id: device.id,
          name: device.name,
          ok: false,
          errorKind: 'error',
          reason: `探测请求失败（网络层错误）：${cause instanceof Error ? cause.message : String(cause)}`,
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
  void reload();
});

/** 筛选变化 → 回到第 1 页。 */
watch([statusFilter, protocolFilter, groupFilter, keyword], () => {
  page.value = 1;
});
/** 页数超出总页数时回退。 */
watch(totalPages, (tp) => {
  if (page.value > tp) {
    page.value = tp;
  }
});
/** 真实数据变更 → 刷新。 */
watch(dataVersion, () => {
  void reload();
});

// ---------------------------------------------------------------------------
// 分组管理弹窗
// ---------------------------------------------------------------------------

const groupModalOpen = ref(false);
const newGroupName = ref('');
const editingId = ref('');
const editingName = ref('');
const deletingId = ref('');
const groupBusy = ref(false);
const groupNote = ref('');
const groupNoteKind = ref<'ok' | 'warn'>('ok');

/** 打开弹窗：拉取最新分组并清空草稿。 */
async function openGroupModal(): Promise<void> {
  groupModalOpen.value = true;
  newGroupName.value = '';
  editingId.value = '';
  deletingId.value = '';
  groupNote.value = '';
  await reload();
}

/** 关闭弹窗。 */
function closeGroupModal(): void {
  groupModalOpen.value = false;
  editingId.value = '';
  deletingId.value = '';
  newGroupName.value = '';
}

/** 新增分组。 */
async function createGroup(): Promise<void> {
  const name = newGroupName.value.trim();
  if (!name || groupBusy.value) {
    return;
  }
  groupBusy.value = true;
  groupNote.value = '';
  try {
    const res = await repo.groups.create({ name, actor: session.state.displayName });
    if (!res.ok) {
      groupNote.value = res.message;
      groupNoteKind.value = 'warn';
      return;
    }
    newGroupName.value = '';
    groups.value = await repo.groups.list();
    groupNote.value = res.message;
    groupNoteKind.value = 'ok';
  } finally {
    groupBusy.value = false;
  }
}

/** 进入重命名。 */
function startRename(g: DeviceGroup): void {
  if (g.isDefault) {
    return;
  }
  editingId.value = g.id;
  editingName.value = g.name;
}

/** 保存重命名。 */
async function saveRename(g: DeviceGroup): Promise<void> {
  const name = editingName.value.trim();
  if (!name || groupBusy.value) {
    return;
  }
  groupBusy.value = true;
  groupNote.value = '';
  try {
    const res = await repo.groups.update({ id: g.id, name, actor: session.state.displayName });
    if (!res.ok) {
      groupNote.value = res.message;
      groupNoteKind.value = 'warn';
      return;
    }
    editingId.value = '';
    groups.value = await repo.groups.list();
    groupNote.value = res.message;
    groupNoteKind.value = 'ok';
  } finally {
    groupBusy.value = false;
  }
}

/** 删除分组（已进入二次确认）。 */
async function removeGroup(g: DeviceGroup): Promise<void> {
  if (g.isDefault || groupBusy.value) {
    return;
  }
  groupBusy.value = true;
  groupNote.value = '';
  try {
    const res = await repo.groups.remove({ id: g.id, actor: session.state.displayName });
    if (!res.ok) {
      groupNote.value = res.message;
      groupNoteKind.value = 'warn';
      return;
    }
    deletingId.value = '';
    groups.value = await repo.groups.list();
    groupNote.value = res.message;
    groupNoteKind.value = 'ok';
  } finally {
    groupBusy.value = false;
  }
}

// ---------------------------------------------------------------------------
// 删除设备（二次确认）
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

/** 确认删除（真实落库，失败展示真实原因）。 */
async function confirmDelete(): Promise<void> {
  const target = pendingDelete.value;
  if (!target) {
    return;
  }
  const res = await repo.deleteDevice({ id: target.id, actor: session.state.displayName });
  if (!res.ok) {
    const failed = target.name;
    pendingDelete.value = null;
    window.alert(`删除设备「${failed}」失败：${res.message}`);
    return;
  }
  pendingDelete.value = null;
  await reload();
}
</script>

<style scoped>
/* 表格 foot：左文案 + 右操作 */
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
/* 运行状态单元格：状态标签 + 最后采集 + 真实原因 */
.dv-run {
  display: flex;
  flex-direction: column;
  gap: 2px;
  min-width: 0;
}
.dv-run__meta {
  font-size: var(--fs-caption);
  color: var(--text-3);
}
.dv-run__reason {
  font-size: var(--fs-caption);
  color: var(--warn-fg);
  word-break: break-all;
}

/* ══ 分组管理弹窗 ══ */
.gpm__mask {
  position: fixed;
  inset: 0;
  background: var(--mask);
  display: flex;
  align-items: center;
  justify-content: center;
  z-index: 2000;
  padding: 24px;
}
.gpm {
  background: var(--bg-card);
  border-radius: var(--radius);
  box-shadow: var(--shadow);
  width: 620px;
  max-width: 100%;
  max-height: 86vh;
  display: flex;
  flex-direction: column;
}
.gpm__head {
  display: flex;
  align-items: center;
  justify-content: space-between;
  padding: 14px 18px;
  border-bottom: 1px solid var(--divider);
}
.gpm__head h3 {
  margin: 0;
  font-size: 15px;
  font-weight: 600;
}
.gpm__body {
  padding: 16px 18px;
  overflow: auto;
  display: flex;
  flex-direction: column;
  gap: 12px;
}
.gpm__add {
  display: grid;
  grid-template-columns: 1fr auto;
  gap: 8px;
  align-items: center;
}
.gpm__list {
  list-style: none;
  margin: 0;
  padding: 0;
  display: flex;
  flex-direction: column;
  gap: 8px;
}
.gpm__row {
  display: grid;
  grid-template-columns: minmax(0, 1fr) auto auto;
  gap: 10px;
  align-items: center;
  padding: 9px 12px;
  border: 1px solid var(--border);
  border-radius: var(--radius-sm);
}
.gpm__name {
  display: inline-flex;
  align-items: center;
  gap: 8px;
  font-weight: 600;
  min-width: 0;
}
.gpm__count {
  font-size: var(--fs-caption);
  color: var(--text-3);
}
.gpm__ops {
  display: inline-flex;
  align-items: center;
  gap: 6px;
  flex-wrap: wrap;
  justify-content: flex-end;
}
.gpm__reason {
  font-size: var(--fs-caption);
  color: var(--text-3);
}
.gpm__confirm {
  font-size: var(--fs-caption);
  color: var(--danger-fg);
}
.gpm__note {
  font-size: var(--fs-caption);
  border-radius: var(--radius-sm);
  padding: 8px 12px;
  line-height: 1.6;
}
.gpm__note.is-ok {
  background: var(--ok-bg);
  color: var(--ok-fg);
}
.gpm__note.is-warn {
  background: var(--warn-bg);
  color: var(--warn-fg);
}
.gpm__foot {
  padding: 12px 18px;
  border-top: 1px solid var(--divider);
  display: flex;
  justify-content: flex-end;
}
</style>

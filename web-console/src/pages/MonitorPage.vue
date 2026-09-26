<template>
  <!--
    MonitorPage —— 实时监控（监控分组第 2 页，路由 `/monitor`）。

    结构对齐原型 `monitor`（:1435 起）与设计 §3.5：
      ① 顶部状态条（设备/质量筛选 + 仅显示异常 + 暂停 / 立即刷新 / 最后更新）
      ② KPI 卡片行（本周期采样 / 数据质量 / 最慢驱动 / 离线队列）
      ③ 实时点位值表格（1s 合并刷新，含 sparkline 波动可视化）
      ④ 2 列栅格：设备连接状态 + 质量码汇总（SVG 柱状）

    硬性约定遵守情况：
      · **1s 节流渲染**：`setInterval(tick, 1000)`，上游再怎么高频每秒只写一次响应式状态；
      · 暂停 / 继续刷新；显示「最后更新 HH:mm:ss」；
      · 陈旧数据（`stale`）整行转灰；质量非 Good 的行左侧带 `--warn` 竖条；
      · 列表分页，条数只有 UiPager 一个口径；
      · 空态给下一步动作（EmptyState 一句话说明 + 动作按钮）；
      · 数值只来自 `GET /api/stream`（SSE）与 `GET /api/points`，
        **绝不预填随机历史 / 随机曲线**：无流数据时曲线为平线、数值为 `——`；
      · sparkline 为纯内联 SVG，未引入图表库；无 emoji。
  -->
  <div class="wc-content">
    <!-- 顶部状态条：暂停 / 最后更新 / 连接态 -->
    <div class="wc-card">
      <div class="wc-card__body">
        <div class="mn-bar">
          <div class="wc-filters mn-bar__filters">
            <div class="wc-filters__item">
              <label for="mn-device">设备筛选</label>
              <UiSelect v-model="deviceFilter" :options="deviceOptions" />
            </div>
            <div class="wc-filters__item">
              <label for="mn-quality">质量筛选</label>
              <UiSelect v-model="qualityFilter" :options="qualityOptions" />
            </div>
            <div class="wc-filters__item mn-bar__switch">
              <label>&nbsp;</label>
              <UiSwitch v-model="abnormalOnly" on-text="仅显示异常" off-text="显示全部" />
            </div>
          </div>

          <div class="mn-bar__live">
            <button type="button" class="wc-btn" data-testid="monitor-refresh" @click="handleManualRefresh">立即刷新</button>
            <button type="button" class="wc-btn" :class="{ 'wc-btn--primary': paused }" data-testid="monitor-pause" @click="togglePause">
              {{ paused ? '继续刷新' : '暂停刷新' }}
            </button>
            <span class="wc-conn" :class="paused ? 'wc-conn--degraded' : `wc-conn--${session.state.connection}`">
              <span class="wc-conn__dot">●</span>
              <span>{{ paused ? '已暂停' : connectionLabel }}</span>
            </span>
            <span class="mn-bar__ts">
              最后更新 <span class="wc-mono" data-test="last-update">{{ lastTickText }}</span>
            </span>
          </div>
        </div>
      </div>
    </div>

    <!-- KPI 卡片行：数值来自 1s 节流快照（= SSE 实际到达的点位；后端未提供的指标显示 —） -->
    <div class="wc-grid wc-grid--4">
      <StatCard label="本周期采样" :value="formatInt(live.sampledPoints)" unit="点" sub="合并刷新" />
      <StatCard label="数据质量" :value="live.goodPct.toFixed(1)" unit="% 良好" tone="ok" />
      <!-- SSE 帧不含「最慢驱动耗时 / 离线队列深度」→ 显示 —，不把「未知」伪装成 0 -->
      <StatCard label="最慢驱动" value="—" sub="后端未提供" />
      <StatCard label="离线队列" value="—" sub="后端未提供" />
    </div>

    <!-- 实时点位值 -->
    <section class="wc-card">
      <div class="wc-card__head">
        <h3>实时点位值</h3>
        <span class="wc-card__sub">1s 合并刷新 · 共 {{ filteredPoints.length }} 个点位</span>
        <div class="wc-card__ops">
          <span v-if="paused" class="wc-tag wc-tag--warn">已暂停，数值冻结</span>
        </div>
      </div>

      <EmptyState
        v-if="filteredPoints.length === 0"
        title="没有符合条件的点位"
        desc="放宽设备 / 质量筛选，或前往点位与映射新增点位。"
      >
        <template #actions>
          <button type="button" class="wc-btn" @click="resetFilters">清空筛选</button>
          <button type="button" class="wc-btn wc-btn--primary" @click="go('points')">前往点位与映射</button>
        </template>
      </EmptyState>

      <template v-else>
        <div class="mn-table-wrap">
          <table class="wc-table mn-table">
            <thead>
              <tr>
                <th>目标点位</th>
                <th class="is-right">数值</th>
                <th>单位</th>
                <th>变化</th>
                <th>波动（近 20 拍）</th>
                <th>质量</th>
                <th>耗时</th>
                <th>时间戳</th>
              </tr>
            </thead>
            <tbody>
              <tr v-for="row in pagedRows" :key="row.id" :class="row.rowClass">
                <td>
                  <div class="mn-point">
                    <span class="wc-mono mn-point__key">{{ row.targetKey }}</span>
                    <span class="mn-point__name">
                      {{ row.name }}
                      <span v-if="row.pointType === 'derived'" class="wc-tag wc-tag--info mn-point__type">计算点</span>
                    </span>
                    <span class="mn-point__dev">{{ row.deviceName }}</span>
                  </div>
                </td>
                <td class="is-right">
                  <span class="wc-mono mn-value" data-test="point-value">{{ row.valueText }}</span>
                </td>
                <td>{{ row.unit }}</td>
                <td>
                  <span class="mn-delta" :class="row.deltaClass">{{ row.deltaText }}</span>
                </td>
                <td>
                  <!-- sparkline：纯内联 SVG，100×22 逻辑坐标由外层拉伸 -->
                  <svg
                    class="mn-spark"
                    viewBox="0 0 100 22"
                    preserveAspectRatio="none"
                    role="img"
                    :aria-label="`${row.name} 近 20 次采样的波动趋势`"
                  >
                    <path :d="row.spark" :style="{ stroke: row.sparkColor }" stroke-width="1.4" fill="none" />
                  </svg>
                </td>
                <td>
                  <span class="wc-tag" :class="row.qualityClass" :title="row.qualityTitle">{{ row.quality }}</span>
                </td>
                <td>
                  <span class="wc-mono">{{ row.latencyMs }} ms</span>
                </td>
                <td>
                  <span class="wc-mono">{{ row.tsText }}</span>
                </td>
              </tr>
            </tbody>
          </table>
        </div>
        <UiPager
          :page="pointPage"
          :total="filteredPoints.length"
          :page-size="pointPageSize"
          :sizes="POINT_PAGE_SIZES"
          numeric
          jump
          @update:page="onPointPage"
          @update:page-size="onPointPageSize"
        />
      </template>
    </section>

    <!-- 设备连接状态 + 质量码汇总 -->
    <div class="wc-grid wc-grid--2">
      <section class="wc-card">
        <div class="wc-card__head">
          <h3>设备连接状态</h3>
          <span class="wc-card__sub">点位数与重连次数</span>
        </div>
        <div class="wc-card__body wc-card__body--flush">
          <UiTable :columns="deviceColumns" :rows="devices" row-key-field="id">
            <template #cell-name="{ row }">{{ row.name }}</template>
            <template #cell-status="{ row }">
              <StatusTag :status="row.status" />
            </template>
            <template #cell-failStreak="{ row }">
              <span class="wc-mono">{{ row.failStreak }}</span>
            </template>
            <template #cell-lastSampleAt="{ row }">
              <span class="wc-mono">{{ row.lastSampleAt }}</span>
            </template>
          </UiTable>
        </div>
      </section>

      <section class="wc-card">
        <div class="wc-card__head">
          <h3>质量码汇总</h3>
          <span class="wc-card__sub">本周期 {{ filteredPoints.length }} 个点位</span>
        </div>
        <div class="wc-card__body">
          <svg
            class="mn-bars"
            viewBox="0 0 100 60"
            preserveAspectRatio="none"
            role="img"
            aria-label="质量码分布柱状图"
          >
            <g v-for="(bar, i) in qualityBars" :key="bar.key">
              <rect
                :x="barSlot * i + BAR_GAP / 2"
                :y="BAR_BASE_Y - bar.height"
                :width="barW"
                :height="bar.height"
                :style="{ fill: bar.color }"
                rx="1.5"
              />
              <text
                class="mn-bars__label"
                :x="barSlot * i + barSlot / 2"
                :y="58"
                text-anchor="middle"
              >
                {{ qualityLabelOf(bar.key) }}
              </text>
              <text
                class="mn-bars__value"
                :x="barSlot * i + barSlot / 2"
                :y="BAR_BASE_Y - BAR_VALUE_LIFT - bar.height"
                text-anchor="middle"
                font-weight="600"
              >
                {{ bar.value }}
              </text>
            </g>
          </svg>
        </div>
      </section>
    </div>
  </div>
</template>

<script setup lang="ts">
/**
 * @file MonitorPage.vue
 * @module web-console/pages/MonitorPage
 * @description 实时监控页。1s 节流 + 消费 `/api/stream` SSE + sparkline 波动可视化。
 *
 * ── 节流契约（本页最关键的硬约束）────────────────────────────────────────────
 * `setInterval(tick, 1000)` 就是节流点：上游推送频率无关紧要（SSE 帧
 * 只落 `pointSnapshots` 缓存、不触发渲染），`live` 快照与 `series` 历史每秒
 * **至多**被写一次，Vue 因此每秒至多重渲染一次。
 * 「暂停」只清除定时器，不改变任何已渲染数据（冻结语义）。
 * 数据全部来自真实接口（`/api/stream` SSE + `/api/points`），无任何演示数据。
 */
import { computed, onBeforeUnmount, onMounted, reactive, ref, watch } from 'vue';
import { useRouter } from 'vue-router';
import {
  EmptyState,
  StatCard,
  StatusTag,
  UiPager,
  UiSelect,
  UiSwitch,
  UiTable,
  type SelectOption,
  type TableColumn,
} from '@ui-kit';
import {
  dataVersion,
  repo,
  type DeviceRecord,
  type PointRecord,
} from '@/api/repo';
import { pointSnapshots, snapshotKey, streamStatus, wireQualityToDataQuality, type StreamStatus } from '@/api/stream';
import { formatNanoTimestampText, formatTimestampText } from '@/utils/time';
import { session } from '../store/session';

const router = useRouter();

// ---------------------------------------------------------------------------
// 常量
// ---------------------------------------------------------------------------

/** 实时点位表每页条数（用 UiPager 单一口径；可经 sizes chip 切换，sizes 需可变数组）。 */
const POINT_PAGE_SIZES: number[] = [8, 16, 32];
const pointPageSize = ref<number>(POINT_PAGE_SIZES[0]);

/** sparkline 保留的历史拍数。 */
const HISTORY_LEN = 20;

/** 质量码展示顺序（固定，避免柱状图顺序漂移）。 */
const QUALITY_ORDER: readonly PointRecord['quality'][] = ['Good', 'Uncertain', 'Bad', 'Timeout', 'CalcFailed'];

/**
 * 质量码 → 中文短名（设计系统 §「质量码对外展示一律中文」）。
 *
 * 仅用于图表轴标签展示；wire 枚举本身不改（数据层仍是英文 5 值枚举）。
 */
const QUALITY_LABEL: Readonly<Record<string, string>> = Object.freeze({
  Good: '良好',
  Uncertain: '不确定',
  Bad: '坏',
  Timeout: '超时',
  CalcFailed: '计算失败',
});

/**
 * 质量码 → 柱状图颜色（全部取自 ui-kit token，非自造色值）。
 *
 * SVG 表现属性不支持 `var()`，故这里只存放「变量名字符串」，由模板经
 * `:style` 下发（见 `qualityBars` 与模板 `:style="{ fill: bar.color }"`）。
 *
 * 暗黑模式（`root[data-theme='dark']`，底色 `--bg-card` #161a22）实测对比度：
 *   --ok #2fbf87 → 7.28:1 / --warn #f5a524 → 8.40:1 / --danger #f9746a → 6.23:1
 *   --unknown #7d879c → 4.75:1（≥4.5，达标）
 * 原 `--series-alt`（global.css 单点桥接变量 #7a5af8）未在暗色块提亮，
 * 对 --bg-card 仅 3.76:1，低于图形 4.5:1 阈值，故 CalcFailed 改用同为暗色块
 * 提亮过的 `--info` #4c9bff（6.06:1），且与其余四类语义不冲突。
 */
const QUALITY_COLOR: Readonly<Record<string, string>> = Object.freeze({
  Good: 'var(--ok)',
  Uncertain: 'var(--warn)',
  Bad: 'var(--danger)',
  Timeout: 'var(--unknown)',
  CalcFailed: 'var(--info)',
});

/** 设备连接状态列定义（分页口径：设备数固定 8 台，不设分页）。 */
const deviceColumns: readonly TableColumn[] = [
  { key: 'name', label: '设备' },
  { key: 'status', label: '连接' },
  { key: 'failStreak', label: '连续失败', align: 'right', mono: true },
  { key: 'lastSampleAt', label: '最后成功', mono: true },
];

// ---------------------------------------------------------------------------
// 数据源
// ---------------------------------------------------------------------------

/** 全部点位（实时监控消费全量）。 */
const allPoints: PointRecord[] = repo.allPoints();

/** 全部设备（连接状态表）。 */
const devices: DeviceRecord[] = repo.allDevices();

/**
 * 缓存填充完成（dataVersion 自增）→ 原地刷新静态清单。
 *
 * 避免预取晚于挂载时实时表停留空态：清单来自 `preloadRealData`
 * 填好的 `realCache`（点位来自 `GET /api/points`、设备来自 `GET /api/devices`）；
 * dataVersion 自增即表示缓存已就绪，此处原地刷新（保持数组引用 / 响应式不变）。
 * 实时数值本身由 1s tick 从 SSE `pointSnapshots` 消费，不在此重读。
 */
watch(dataVersion, () => {
  applyInto(allPoints, repo.allPoints());
  applyInto(devices, repo.allDevices());
  // 新到达的点位尚无运行时行 → 重播 seed（幂等；仅缓存就绪时触发一次）。
  seed();
});

/** 把 source 内容覆盖写入 target（原地变更，保持 target 引用 / 响应式不变）。 */
function applyInto<T>(target: T[], source: readonly T[]): void {
  target.length = 0;
  target.push(...source);
}

/**
 * 点位运行时状态：当前值 / 上一拍差值 / 历史序列 / 耗时 / 时间戳。
 *
 * 以 `point.id` 为键；基线取后端点位快照值，`history` 恒从空开始（首帧平线），
 * 绝不伪造曲线。后续历史只由 SSE 实际帧推进。
 *
 * `tsRaw`（SSE 帧的纳秒字符串原文，透传展示）与
 * `qualityCode`（后端质量码整数，悬浮提示用）。
 */
interface PointRuntime {
  /** 当前值（不可用为 null） */
  value: number | null;
  /** 与上一拍的差值 */
  delta: number;
  /** 近 HISTORY_LEN 拍历史 */
  history: number[];
  /** 本拍采集耗时（ms） */
  latencyMs: number;
  /** 最后更新时间（毫秒时间戳） */
  tsMs: number;
  /** 最近一次流帧的纳秒时间戳原文（SSE 帧透传；无帧时为空串） */
  tsRaw: string;
  /** 最近一次流帧的质量码整数（SSE 帧透传；无帧时为 null） */
  qualityCode: number | null;
}

/** 各点位运行时状态（响应式，但每秒只被写一次）。 */
const series = reactive<Record<string, PointRuntime>>({});

/** 质量码（可被推流改写：如 Timeout 行偶发恢复为 Uncertain）。 */
const runtimeQuality = reactive<Record<string, PointRecord['quality']>>({});

/**
 * 初始化运行时状态（首帧基线）。
 *
 * **绝不预填随机历史**：`history` 保持为空（sparkline 落平线），数值用后端快照
 * 基线（无值即 `——`），耗时置 0，时间戳从当前时刻起算（超过 1.5s 无流帧即整行转灰）。
 */
function seed(): void {
  const nowMs = Date.now();
  for (const point of allPoints) {
    runtimeQuality[point.id] = point.quality;
    series[point.id] = {
      value: point.value,
      delta: 0,
      history: [],
      latencyMs: 0,
      tsMs: nowMs,
      tsRaw: '',
      qualityCode: null,
    };
  }
}

// ---------------------------------------------------------------------------
// 1s 节流推流
// ---------------------------------------------------------------------------

/** 节流后的实时快照（初值 0；首拍 tick 立即由 SSE 快照覆盖）。 */
const live = reactive({
  /** 本周期采样点数（= 已有流数据的点位数） */
  sampledPoints: 0,
  /** 质量良好占比（%） */
  goodPct: 0,
});

/** 最后更新毫秒时间戳。 */
const lastTickMs = ref<number>(Date.now());

/** 已完成节拍数（用于界面自证「确实在刷新」）。 */
const tickCount = ref<number>(0);

/** 是否暂停。 */
const paused = ref<boolean>(false);

/** 定时器句柄。 */
let timer: ReturnType<typeof setInterval> | null = null;

/**
 * 单个节拍的调度入口（由 1s 定时器驱动 —— 即「1s 节流渲染」的实现）。
 *
 * 从 SSE 逐点快照表（`pointSnapshots`）读取最新值：SSE 帧的到达频率与本函数
 * 无关 —— 快照表只被写不渲染，渲染仍每秒至多一次。
 */
function tick(): void {
  tickReal();
}

/**
 * 节拍实现：消费 SSE 逐点快照（key = `${device_id}/${point_id}`）。
 *
 * · 快照存在 → 以流帧为准：值 / 质量（wire 枚举映射到前端 5 值枚举）/
 *   纳秒 ts 原文透传；历史序列仅在取到数值时推进；
 * · 快照不存在（该点尚无流数据）→ 保持基线值与配置质量，不伪造；
 * · 采集耗时 / 离线队列：后端帧不含这些指标，置 0，展示为 `—`，不伪造。
 */
function tickReal(): void {
  const nowMs = Date.now();

  let good = 0;
  for (const point of allPoints) {
    const state = series[point.id];
    if (!state) {
      continue;
    }
    const snap = pointSnapshots.get(snapshotKey(point.deviceId, point.id));
    if (!snap) {
      runtimeQuality[point.id] = point.quality;
      continue;
    }

    const quality = wireQualityToDataQuality(snap.quality);
    runtimeQuality[point.id] = quality;
    const previous = state.value;
    state.value = snap.value;
    state.delta =
      previous !== null && snap.value !== null ? Number((snap.value - previous).toFixed(2)) : 0;
    state.latencyMs = 0;
    state.tsMs = snap.receivedAtMs;
    state.tsRaw = snap.ts;
    state.qualityCode = snap.qualityCode;
    if (snap.value !== null) {
      state.history.push(snap.value);
      if (state.history.length > HISTORY_LEN) {
        state.history.shift();
      }
    }
    if (quality === 'Good') {
      good += 1;
    }
  }

  // KPI 快照（同样每秒只写一次；采样数 = 已有流数据的点位数）
  live.sampledPoints = pointSnapshots.size;
  live.goodPct = Number(((good / Math.max(allPoints.length, 1)) * 100).toFixed(1));

  lastTickMs.value = nowMs;
  tickCount.value += 1;
}

/** 启动定时器（幂等）。 */
function startTimer(): void {
  if (timer !== null) {
    return;
  }
  timer = setInterval(tick, 1000);
}

/** 停止定时器。 */
function stopTimer(): void {
  if (timer !== null) {
    clearInterval(timer);
    timer = null;
  }
}

/** 暂停 / 继续。 */
function togglePause(): void {
  paused.value = !paused.value;
  if (paused.value) {
    stopTimer();
  } else {
    tick();
    startTimer();
  }
}

/** 手动刷新（页头按钮）：无论是否暂停都立即推进一拍。 */
function handleManualRefresh(): void {
  tick();
}

onMounted(() => {
  seed();
  tick();
  startTimer();
});

onBeforeUnmount(() => {
  stopTimer();
});

/** 最后更新时刻 `HH:mm:ss`。 */
const lastTickText = computed<string>(() => {
  const d = new Date(lastTickMs.value);
  const p = (n: number): string => String(n).padStart(2, '0');
  return `${p(d.getHours())}:${p(d.getMinutes())}:${p(d.getSeconds())}`;
});

/** 顶栏连接态文案（直读 SSE 通道状态）。 */
const connectionLabel = computed<string>(() => {
  const labels: Readonly<Record<StreamStatus, string>> = Object.freeze({
    open: '实时通道已连接（SSE）',
    connecting: '实时通道连接中…',
    unauthorized: '实时通道未授权',
    idle: '实时通道未连接',
  });
  return labels[streamStatus.value];
});

// ---------------------------------------------------------------------------
// 筛选
// ---------------------------------------------------------------------------

/** 设备筛选值（'' = 全部）。 */
const deviceFilter = ref<string>('');

/** 质量筛选值（'' = 全部）。 */
const qualityFilter = ref<string>('');

/** 仅显示异常。 */
const abnormalOnly = ref<boolean>(false);

/** 设备下拉选项。 */
const deviceOptions = computed<readonly SelectOption[]>(() => [
  { value: '', label: '全部设备' },
  ...devices.map((d) => ({ value: d.id, label: d.name })),
]);

/** 质量下拉选项。 */
const qualityOptions: readonly SelectOption[] = [
  { value: '', label: '全部质量' },
  { value: 'Good', label: 'Good（良好）' },
  { value: 'Uncertain', label: 'Uncertain（不确定）' },
  { value: 'Bad', label: 'Bad（坏）' },
  { value: 'Timeout', label: 'Timeout（超时）' },
  { value: 'CalcFailed', label: 'CalcFailed（计算失败）' },
];

/** 满足筛选条件的点位（保持后端清单顺序稳定）。 */
const filteredPoints = computed<readonly PointRecord[]>(() =>
  allPoints.filter((point) => {
    if (deviceFilter.value && point.deviceId !== deviceFilter.value) {
      return false;
    }
    const quality = runtimeQuality[point.id] ?? point.quality;
    if (qualityFilter.value && quality !== qualityFilter.value) {
      return false;
    }
    if (abnormalOnly.value && quality === 'Good') {
      return false;
    }
    return true;
  }),
);

/** 当前页（页面内分页）。 */
const pointPage = ref<number>(1);

/** 清空筛选。 */
function resetFilters(): void {
  deviceFilter.value = '';
  qualityFilter.value = '';
  abnormalOnly.value = false;
  pointPage.value = 1;
}

/**
 * 仅依赖**筛选条件**的静态切片（不含每秒变化的运行时数据）。
 *
 * 把「筛选 + 分页」与「实时数值」拆成两个 computed：
 * 后者每秒重算只做一次 `.map()`，前者（字符串比较 + slice）不参与每秒重算。
 */
const staticRows = computed<readonly PointRecord[]>(() => {
  const start = (pointPage.value - 1) * pointPageSize.value;
  return filteredPoints.value.slice(start, start + pointPageSize.value);
});

/** 换页。 */
function onPointPage(next: number): void {
  pointPage.value = next;
}

/** 换每页条数（回到第 1 页，避免停留在越界空页）。 */
function onPointPageSize(next: number): void {
  pointPageSize.value = next;
  pointPage.value = 1;
}

/** 筛选条件变化时回到第 1 页（避免停留在空页）。 */
watch(
  () => [deviceFilter.value, qualityFilter.value, abnormalOnly.value] as const,
  () => {
    pointPage.value = 1;
  },
);

// ---------------------------------------------------------------------------
// 渲染辅助
// ---------------------------------------------------------------------------

/**
 * 实时点位表的一行（**完全预计算**视图模型）。
 *
 * 之所以把格式化、色调、sparkline 路径、时间戳文案全部在 computed 内算成纯字符串：
 * 模板只做插值，不在渲染期调用函数 —— 既避免模板中散布逻辑，又让 `vue-tsc`
 * 对行模型有完整类型（含模板插槽的严格检查）。
 */
interface PointRow {
  /** 点位主键 */
  readonly id: string;
  /** 北向目标点名 */
  readonly targetKey: string;
  /** 点位中文名 */
  readonly name: string;
  /** 所属设备名 */
  readonly deviceName: string;
  /** 点位类型（physical / derived） */
  readonly pointType: PointRecord['pointType'];
  /** 数值展示文本（不可用为 `——`） */
  readonly valueText: string;
  /** 工程单位 */
  readonly unit: string;
  /** 变化量展示文本 */
  readonly deltaText: string;
  /** 变化量色调类（仅表示方向，不代表好坏 —— 设计系统 §4.2） */
  readonly deltaClass: string;
  /** sparkline 路径（100×22 逻辑坐标） */
  readonly spark: string;
  /** sparkline 描边色 */
  readonly sparkColor: string;
  /** 质量码原文 */
  readonly quality: string;
  /** 质量标签色调类 */
  readonly qualityClass: string;
  /** 质量码悬浮提示（含后端 quality_code；无帧时为空） */
  readonly qualityTitle: string;
  /** 采集耗时展示 */
  readonly latencyMs: string;
  /** 时间戳展示 */
  readonly tsText: string;
  /** 行样式类（陈旧转灰 / 异常竖条） */
  readonly rowClass: string;
}

/** 数值格式化（null → `——`）。 */
function formatValue(value: number | null): string {
  if (value === null) {
    return '——';
  }
  return value.toLocaleString('en-US', { minimumFractionDigits: 0, maximumFractionDigits: 2 });
}

/** 变化量格式化（带符号；0 显示 `0.0`）。 */
function formatDelta(delta: number): string {
  if (delta === 0) {
    return '0.0';
  }
  const sign = delta > 0 ? '+' : '−';
  return `${sign}${Math.abs(delta).toFixed(2)}`;
}

/** 变化量色调。 */
function deltaClassOf(delta: number): string {
  if (delta === 0) {
    return 'is-flat';
  }
  return delta > 0 ? 'is-up' : 'is-down';
}

/** 质量标签色调。 */
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

/** sparkline 描边色：质量异常统一转对应语义色以强化信号（token 变量名，模板经 style 下发）。 */
function sparkColorOf(quality: string): string {
  if (quality === 'Good') {
    return 'var(--ok)';
  }
  if (quality === 'Uncertain') {
    return 'var(--warn)';
  }
  if (quality === 'Bad' || quality === 'Timeout') {
    return 'var(--danger)';
  }
  return 'var(--series-alt)';
}

/**
 * sparkline 路径：把历史序列映射到 100×22 逻辑坐标系。
 *
 * 纵轴按序列自身 min/max 归一化（避免同一设备不同量纲互相压制），
 * 并在 `min === max` 时落于中线，防止除零。
 */
function sparkPathOf(history: readonly number[]): string {
  if (history.length === 0) {
    return 'M0,11 L100,11';
  }
  const min = Math.min(...history);
  const max = Math.max(...history);
  const span = max - min;
  const n = history.length;
  return history
    .map((value, i) => {
      const x = n <= 1 ? 0 : (i / (n - 1)) * 100;
      const y = span === 0 ? 11 : 20 - ((value - min) / span) * 18;
      return `${i === 0 ? 'M' : 'L'}${x.toFixed(1)},${y.toFixed(1)}`;
    })
    .join(' ');
}

/**
 * 当前页的行视图模型。
 *
 * 该 computed 依赖 `series` / `runtimeQuality`（每秒被写一次），因此每秒重算一次 ——
 * 与节拍严格对齐，**不会**超过 1 Hz。
 */
const pagedRows = computed<readonly PointRow[]>(() => {
  const nowMs = Date.now();
  return staticRows.value.map((point) => {
    const state = series[point.id];
    const quality = runtimeQuality[point.id] ?? point.quality;
    const value = state ? state.value : point.value;
    const delta = state ? state.delta : 0;
    const history = state ? state.history : [];
    const latency = state ? state.latencyMs : 0;
    const tsMs = state ? state.tsMs : nowMs;
    const ageSec = (nowMs - tsMs) / 1000;
    const stale = ageSec > 1.5;

    const classes: string[] = [];
    if (stale) {
      classes.push('is-stale');
    }
    if (quality !== 'Good') {
      classes.push('is-abnormal');
    }

    // 时间戳：SSE 帧的纳秒原文**绝不原样上屏**，一律格式化后再展示（大数红线：
    // 纳秒串只截秒段、不整串数值化）；无帧则按陈旧度 / 本地时刻展示可读文本。
    const tsRaw = state?.tsRaw ?? '';
    const tsText = tsRaw
      ? formatNanoTimestampText(tsRaw)
      : stale
        ? `${ageSec.toFixed(0)}s 前（陈旧）`
        : formatTimestampText(tsMs);
    const qualityCode = state?.qualityCode ?? null;
    const qualityTitle = qualityCode !== null ? `quality_code: ${qualityCode}` : '';

    return {
      id: point.id,
      targetKey: point.targetKey,
      name: point.name,
      deviceName: point.deviceName,
      pointType: point.pointType,
      valueText: formatValue(value),
      unit: point.unit,
      deltaText: formatDelta(delta),
      deltaClass: deltaClassOf(delta),
      spark: sparkPathOf(history),
      sparkColor: sparkColorOf(quality),
      quality,
      qualityClass: qualityTagClass(quality),
      qualityTitle,
      latencyMs: String(latency),
      tsText,
      rowClass: classes.join(' '),
    };
  });
});

/** 千分位。 */
function formatInt(value: number): string {
  return value.toLocaleString('en-US');
}

// ---------------------------------------------------------------------------
// 质量码汇总柱状
// ---------------------------------------------------------------------------

/** 柱状图视图模型。 */
interface QualityBar {
  /** 质量码（X 轴标签） */
  key: string;
  /** 点位数 */
  value: number;
  /** 柱高（viewBox 逻辑坐标，已夹紧在 BAR_MIN_H–BAR_MAX_H 之间） */
  height: number;
  /** 填充色 */
  color: string;
}

// ---------------------------------------------------------------------------
// 质量码柱状图几何（viewBox 坐标系：宽 100 × 高 60，与模板 viewBox 一致）
// ---------------------------------------------------------------------------

/** viewBox 可用宽度，柱子等分于此宽度。 */
const BAR_VIEW_W = 100;
/** 基线 y：柱底贴 52，其下留 8 单位给轴标签。 */
const BAR_BASE_Y = 52;
/** 柱间空隙（viewBox 单位），等分时留出。 */
const BAR_GAP = 4;
/**
 * 柱最大高度 38：基线 52 − 38 = 顶边 y=14（不越过 52）；
 * 其上数值标签基线 = 52 − BAR_VALUE_LIFT − 38 = 8，减去实测字形上伸 ≈5.43 后
 * bbox 顶边 = 2.57，仍在 viewBox 内——故数据变大时柱体与数值标签都不会顶出容器。
 * （先取 41，实测标签 bbox 顶边 −0.43 被裁切，据此收紧到 38。）
 */
const BAR_MAX_H = 38;
/** 柱最小高度：计数为 0 的类别也保留一段可见柱体。 */
const BAR_MIN_H = 2;
/** 数值标签相对柱顶的抬升量（viewBox 单位）。 */
const BAR_VALUE_LIFT = 6;

/** 单条柱占的槽位宽度（含空隙）= 可用宽 / 柱条数，条数变化时自动重算。 */
const barSlot = computed(() => BAR_VIEW_W / Math.max(qualityBars.value.length, 1));

/** 柱宽 = 槽位宽 − 空隙，由条数等分得出（模板 `:width`，不再写死）。 */
const barW = computed(() => Math.max(1, barSlot.value - BAR_GAP));

/** 质量分布柱（基于**全量**点位统计，与筛选无关）。 */
const qualityBars = computed<readonly QualityBar[]>(() => {
  const counts = QUALITY_ORDER.map((key) => ({
    key,
    value: allPoints.filter((p) => (runtimeQuality[p.id] ?? p.quality) === key).length,
    color: QUALITY_COLOR[key] ?? 'var(--unknown)',
  }));
  const max = Math.max(...counts.map((c) => c.value), 1);
  return counts.map((c) => ({
    ...c,
    // 归一到 BAR_MAX_H 并双向夹紧：max=0 时取下限，数据再大也不会超过上限。
    height: Math.min(BAR_MAX_H, Math.max(BAR_MIN_H, (c.value / max) * BAR_MAX_H)),
  }));
});

/** 质量码 → 图表轴中文短名（未知码原样回显）。 */
function qualityLabelOf(key: string): string {
  return QUALITY_LABEL[key] ?? key;
}

// ---------------------------------------------------------------------------
// 导航
// ---------------------------------------------------------------------------

/** 跳转。 */
function go(name: string): void {
  void router.push({ name });
}
</script>

<style scoped>
.mn-bar {
  display: flex;
  gap: 16px;
  align-items: flex-end;
  flex-wrap: wrap;
  justify-content: space-between;
}
.mn-bar__filters {
  flex: 1;
  min-width: 420px;
}
.mn-bar__switch {
  min-width: 150px;
}
.mn-bar__live {
  display: flex;
  align-items: center;
  gap: 12px;
  flex-wrap: wrap;
}
.mn-bar__ts {
  font-size: var(--fs-caption);
  color: var(--text-2);
}
.mn-bar__tick {
  color: var(--text-3);
}
.mn-table-wrap {
  overflow-x: auto;
}
.mn-table th,
.mn-table td {
  white-space: nowrap;
}
.mn-table .is-right {
  text-align: right;
}
.mn-point {
  display: flex;
  flex-direction: column;
  gap: 1px;
}
.mn-point__key {
  font-weight: 600;
  color: var(--text-1);
}
.mn-point__name {
  font-size: var(--fs-caption);
  color: var(--text-2);
}
.mn-point__type {
  margin-left: 4px;
}
.mn-point__dev {
  font-size: 11px;
  color: var(--text-3);
}
.mn-value {
  font-size: 14px;
  font-weight: 600;
}
.mn-delta {
  font-family: var(--font-mono);
  font-size: var(--fs-caption);
}
.mn-delta.is-up {
  color: var(--ok);
}
.mn-delta.is-down {
  color: var(--brand);
}
.mn-delta.is-flat {
  color: var(--text-3);
}
.mn-spark {
  width: 96px;
  height: 22px;
  display: block;
}
.mn-bars {
  width: 100%;
  height: 160px;
  display: block;
}
.mn-bars__label {
  font-size: 4.2px;
  fill: var(--text-3);
}
.mn-bars__value {
  font-size: 4.6px;
  fill: var(--text-1);
}
.mn-table tr.is-abnormal > td:first-child {
  border-left: 3px solid var(--warn);
}
.mn-table tr.is-stale {
  color: var(--text-3);
}
</style>

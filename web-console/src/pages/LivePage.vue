<template>
  <!--
    LivePage —— 实时点位值（监控分组第 4 页，路由 `/live`）。

    定位：实时监控的**明细下钻视图**（见 App.vue 导航注释）。与 MonitorPage 的密集表格不同，
    本页以「卡片」呈现选中设备的实时点位值，便于现场读数。

    硬性约定遵守情况：
      · 1s 节流渲染（setInterval(tick, 1000) 为唯一写入点）；
      · 暂停 / 继续刷新；「最后更新 HH:mm:ss」；
      · 陈旧数据（>1s）整卡转灰，质量非 Good 的角标转警示色；
      · 不暴露内部 id；授权判定不在前端。
  -->
  <PageHeader
    crumb="运行监控 / 实时点位值"
    title="实时点位值"
    desc="按 1s 节流刷新选中设备的实时点位。数值卡支持读数；暂停后将冻结当前一拍以便记录。"
  >
    <template #actions>
      <span class="wc-tag wc-tag--info">演示数据</span>
      <button type="button" class="wc-btn" @click="togglePause">{{ paused ? '继续刷新' : '暂停刷新' }}</button>
    </template>
  </PageHeader>

  <div class="wc-content">
    <!-- 设备筛选 + 状态条 -->
    <div class="wc-card">
      <div class="wc-card__body">
        <div class="wc-filters">
          <div class="wc-filters__item wc-filters__grow">
            <label>设备范围</label>
            <UiSelect v-model="liveDevice" :options="deviceOptions" />
          </div>
          <div class="wc-filters__item">
            <label>&nbsp;</label>
            <span class="wc-conn" :class="paused ? 'wc-conn--degraded' : 'wc-conn--connected'">
              <span class="wc-conn__dot">●</span>
              <span>{{ paused ? '已暂停' : '采集中' }} · 最后更新 {{ lastTickText }}</span>
            </span>
          </div>
        </div>
      </div>
    </div>

    <!-- 实时点位卡 -->
    <EmptyState
      v-if="pagedRows.length === 0"
      title="没有可展示的实时点位"
      desc="当前设备范围下没有点位。你可以切换设备，或前往点位与映射为该设备配置点表。"
    >
      <template #actions>
        <button type="button" class="wc-btn" @click="go('points')">前往点位与映射</button>
      </template>
    </EmptyState>

    <template v-else>
      <div class="dv-cards">
        <div
          v-for="card in pagedRows"
          :key="card.id"
          class="dv-card"
          :class="{ 'is-stale': card.stale }"
        >
          <div class="dv-card__head">
            <div class="dv-card__name">
              <span class="wc-mono dv-card__key">{{ card.targetKey }}</span>
              <span class="dv-card__title">{{ card.name }}</span>
            </div>
            <span class="wc-tag" :class="card.qualityClass">{{ card.quality }}</span>
          </div>
          <div class="dv-card__value">
            <span class="dv-card__num" data-test="live-value">{{ card.valueText }}</span>
            <span class="dv-card__unit">{{ card.unit }}</span>
            <span class="dv-card__delta" :class="card.deltaClass">{{ card.deltaText }}</span>
          </div>
          <div class="dv-card__foot">
            <svg class="dv-spark" viewBox="0 0 100 22" preserveAspectRatio="none" role="img" :aria-label="`${card.name} 近 20 拍波动`">
              <path :d="card.spark" :stroke="card.sparkColor" stroke-width="1.4" fill="none" />
            </svg>
            <span class="dv-card__ts">{{ card.tsText }}</span>
          </div>
        </div>
      </div>
      <UiPager :page="page" :total="scopedPoints.length" :page-size="PAGE_SIZE" @update:page="onPage" />
    </template>
  </div>
</template>

<script setup lang="ts">
/**
 * @file LivePage.vue
 * @module web-console/pages/LivePage
 * @description 实时点位值：按设备范围以卡片呈现，1s 节流推流 + sparkline 波动。
 */
import { computed, onBeforeUnmount, onMounted, reactive, ref, watch } from 'vue';
import { useRouter } from 'vue-router';
import {
  PageHeader,
  UiSelect,
  UiPager,
  EmptyState,
  type SelectOption,
} from '@ui-kit';
import { repo, type PointRecord } from '@/api/repo';

const router = useRouter();

/** 每页卡片数。 */
const PAGE_SIZE = 12;

/** 历史拍数。 */
const HISTORY_LEN = 20;

/** 设备范围（'' = 全部）。 */
const liveDevice = ref('');

/** 设备下拉。 */
const deviceOptions = computed<readonly SelectOption[]>(() => [
  { value: '', label: '全部设备' },
  ...repo.allDevices().map((d) => ({ value: d.id, label: d.name })),
]);

/** 当前范围内的点位（静态，不随每秒变化）。 */
const scopedPoints = ref<PointRecord[]>([]);

/** 当前页号。 */
const page = ref(1);

/** 运行时状态（每秒至多写一次）。 */
interface PointRuntime {
  value: number | null;
  delta: number;
  history: number[];
  tsMs: number;
}
const series = reactive<Record<string, PointRuntime>>({});

/** 质量（可被推流改写，这里沿用初始）。 */
const runtimeQuality = reactive<Record<string, PointRecord['quality']>>({});

/** 振幅（按量纲）。 */
function amplitudeOf(point: PointRecord): number {
  if (point.value === null) {
    return 0;
  }
  const u = point.unit;
  if (u === '℃') return 0.8;
  if (u === 'MPa') return 0.06;
  if (u === 'V' || u === 'A') return 0.9;
  if (u === 'kW') return 0.12;
  if (u === '%' || u === '%RH') return 0.4;
  if (u === 's') return 0.06;
  if (u === 'kWh') return 4;
  return Math.max(Math.abs(point.value) * 0.0005, 0.5);
}
function decimalsOf(point: PointRecord): number {
  if (point.value === null) return 2;
  const t = String(point.value);
  const dot = t.indexOf('.');
  return dot < 0 ? 0 : Math.min(t.length - dot - 1, 2);
}

/** 以 mock 基线初始化运行时。 */
function seed(): void {
  const nowMs = Date.now();
  for (const point of scopedPoints.value) {
    runtimeQuality[point.id] = point.quality;
    const base = point.value;
    const amp = amplitudeOf(point);
    const history: number[] = [];
    if (base !== null) {
      for (let i = HISTORY_LEN - 1; i >= 0; i -= 1) {
        history.push(Number((base + Math.sin(i / 3) * amp * 0.8 + (Math.random() * 2 - 1) * amp * 0.6).toFixed(decimalsOf(point))));
      }
    }
    series[point.id] = { value: base, delta: 0, history, tsMs: point.stale ? nowMs - 187_000 : nowMs };
  }
}

/** 1s 节拍（唯一写入点）。 */
function tick(): void {
  const nowMs = Date.now();
  for (const point of scopedPoints.value) {
    const state = series[point.id];
    if (!state) continue;
    const quality = runtimeQuality[point.id] ?? point.quality;
    if (quality === 'Bad' || quality === 'Timeout' || quality === 'CalcFailed') {
      state.value = null;
      state.delta = 0;
      state.tsMs = nowMs;
      continue;
    }
    const amp = amplitudeOf(point);
    const base = point.value ?? 0;
    const prev = state.value ?? base;
    const next = Number((prev + (Math.random() * 2 - 1) * amp).toFixed(decimalsOf(point)));
    state.delta = Number((next - prev).toFixed(decimalsOf(point)));
    state.value = next;
    state.tsMs = nowMs;
    state.history.push(next);
    if (state.history.length > HISTORY_LEN) state.history.shift();
  }
  lastTickMs.value = nowMs;
  tickCount.value += 1;
}

/** 刷新设备范围。 */
function refreshScope(): void {
  scopedPoints.value = liveDevice.value ? repo.pointsOfDevice(liveDevice.value) : repo.allPoints();
  seed();
}

/** 最后更新。 */
const lastTickMs = ref(Date.now());
const tickCount = ref(0);
const paused = ref(false);
let timer: ReturnType<typeof setInterval> | null = null;

function startTimer(): void {
  if (timer === null) timer = setInterval(tick, 1000);
}
function stopTimer(): void {
  if (timer !== null) {
    clearInterval(timer);
    timer = null;
  }
}
function togglePause(): void {
  paused.value = !paused.value;
  if (paused.value) stopTimer();
  else {
    tick();
    startTimer();
  }
}

onMounted(() => {
  refreshScope();
  tick();
  startTimer();
});
onBeforeUnmount(() => stopTimer());

/** 设备范围变化 → 回到第 1 页并重刷。 */
watch(liveDevice, () => {
  page.value = 1;
  refreshScope();
});

// ---------------------------------------------------------------------------
// 渲染
// ---------------------------------------------------------------------------

const lastTickText = computed(() => {
  const d = new Date(lastTickMs.value);
  const p = (n: number): string => String(n).padStart(2, '0');
  return `${p(d.getHours())}:${p(d.getMinutes())}:${p(d.getSeconds())}`;
});

const staticRows = computed<readonly PointRecord[]>(() => {
  const start = (page.value - 1) * PAGE_SIZE;
  return scopedPoints.value.slice(start, start + PAGE_SIZE);
});

interface CardRow {
  id: string;
  targetKey: string;
  name: string;
  valueText: string;
  unit: string;
  deltaText: string;
  deltaClass: string;
  spark: string;
  sparkColor: string;
  quality: string;
  qualityClass: string;
  tsText: string;
  stale: boolean;
}

function formatValue(v: number | null): string {
  if (v === null) return '——';
  return v.toLocaleString('en-US', { minimumFractionDigits: 0, maximumFractionDigits: 2 });
}
function formatDelta(d: number): string {
  if (d === 0) return '0.0';
  return `${d > 0 ? '+' : '−'}${Math.abs(d).toFixed(2)}`;
}
function deltaClassOf(d: number): string {
  if (d === 0) return 'is-flat';
  return d > 0 ? 'is-up' : 'is-down';
}
function qualityTagClass(q: string): string {
  if (q === 'Good') return 'wc-tag--ok';
  if (q === 'Uncertain') return 'wc-tag--warn';
  if (q === 'Bad' || q === 'Timeout') return 'wc-tag--danger';
  return 'wc-tag--info';
}
function sparkColorOf(q: string): string {
  if (q === 'Good') return '#00A870';
  if (q === 'Uncertain') return '#FF7D00';
  if (q === 'Bad' || q === 'Timeout') return '#F53F3F';
  return '#7A5AF8';
}
function sparkPathOf(history: readonly number[]): string {
  if (history.length === 0) return 'M0,11 L100,11';
  const min = Math.min(...history);
  const max = Math.max(...history);
  const span = max - min;
  const n = history.length;
  return history
    .map((v, i) => {
      const x = n <= 1 ? 0 : (i / (n - 1)) * 100;
      const y = span === 0 ? 11 : 20 - ((v - min) / span) * 18;
      return `${i === 0 ? 'M' : 'L'}${x.toFixed(1)},${y.toFixed(1)}`;
    })
    .join(' ');
}

const pagedRows = computed<readonly CardRow[]>(() => {
  const nowMs = Date.now();
  return staticRows.value.map((point) => {
    const state = series[point.id];
    const quality = runtimeQuality[point.id] ?? point.quality;
    const value = state ? state.value : point.value;
    const delta = state ? state.delta : 0;
    const history = state ? state.history : [];
    const tsMs = state ? state.tsMs : nowMs;
    const ageSec = (nowMs - tsMs) / 1000;
    const stale = ageSec > 1.5;
    return {
      id: point.id,
      targetKey: point.targetKey,
      name: point.name,
      valueText: formatValue(value),
      unit: point.unit,
      deltaText: formatDelta(delta),
      deltaClass: deltaClassOf(delta),
      spark: sparkPathOf(history),
      sparkColor: sparkColorOf(quality),
      quality,
      qualityClass: qualityTagClass(quality),
      tsText: stale ? `${ageSec.toFixed(0)}s 前（陈旧）` : `${(tsMs / 1000).toFixed(3)}`,
      stale,
    };
  });
});

function onPage(next: number): void {
  page.value = next;
}
function go(name: string): void {
  void router.push({ name });
}
</script>

<style scoped>
.dv-cards {
  display: grid;
  grid-template-columns: repeat(auto-fill, minmax(220px, 1fr));
  gap: 12px;
}
.dv-card {
  background: var(--bg-card);
  border: 1px solid var(--border);
  border-radius: var(--radius);
  padding: 12px 14px;
  display: flex;
  flex-direction: column;
  gap: 8px;
}
.dv-card.is-stale {
  color: var(--text-3);
}
.dv-card__head {
  display: flex;
  align-items: flex-start;
  justify-content: space-between;
  gap: 8px;
}
.dv-card__name {
  display: flex;
  flex-direction: column;
  gap: 1px;
  min-width: 0;
}
.dv-card__key {
  font-weight: 600;
  font-size: 12px;
  color: var(--text-1);
}
.dv-card__title {
  font-size: var(--fs-caption);
  color: var(--text-2);
}
.dv-card__value {
  display: flex;
  align-items: baseline;
  gap: 6px;
}
.dv-card__num {
  font-family: var(--font-mono);
  font-variant-numeric: tabular-nums;
  font-size: 22px;
  font-weight: 600;
}
.dv-card__unit {
  font-size: var(--fs-caption);
  color: var(--text-3);
}
.dv-card__delta {
  font-family: var(--font-mono);
  font-size: var(--fs-caption);
}
.dv-card__delta.is-up {
  color: var(--ok);
}
.dv-card__delta.is-down {
  color: var(--brand);
}
.dv-card__delta.is-flat {
  color: var(--text-3);
}
.dv-card__foot {
  display: flex;
  align-items: center;
  gap: 8px;
}
.dv-spark {
  width: 100%;
  height: 22px;
  flex: 1;
}
.dv-card__ts {
  font-size: 11px;
  color: var(--text-3);
  white-space: nowrap;
}
</style>

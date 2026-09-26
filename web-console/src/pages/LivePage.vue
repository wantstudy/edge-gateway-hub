<template>
  <!--
    LivePage —— 实时数据（数据接入分组第 4 页，路由 `/live`）。

    结构对齐原型 `live`（:1666-1678）：
      ① 设备查询框（input + datalist，:1261-1272）+ 暂停 / 刷新 / 连接态；
      ② 4 张 KPI（点位数 / 采集频率 / 连接状态 / 曲线窗口 55s·12 采样点，:1670-1675）；
      ③ 点位**面积曲线卡**（12 点 55s 窗口，:1278-1297），替代原数值卡 + sparkline。

    硬性约定遵守情况：
      · 1s 节流渲染（setInterval(tick, 1000) 为唯一写入点）；
      · 暂停 / 继续刷新；「最后更新 HH:mm:ss」；
      · 曲线窗口语义：每 5 拍（≈5s）采样一次、保留 12 点 → 窗口跨度 55s；
      · 陈旧数据（>1s）整卡转灰，质量非 Good 的角标转警示色；
      · 曲线只由 `GET /api/stream`（SSE）实际帧推进，**绝不预填随机窗口**；
      · 不暴露内部 id；授权判定不在前端。
  -->
  <div class="wc-content">
    <!-- ① 设备查询框（原型 :1266-1271）+ 采集状态 + 刷新控制 -->
    <div class="wc-card">
      <div class="wc-card__body">
        <div class="live-q">
          <input
            v-model="devKeyword"
            type="search"
            class="live-q__input"
            list="live-dev-dl"
            placeholder="输入设备名称查询…"
            aria-label="设备查询"
            @change="onDevQuery"
            @keyup.enter="onDevQuery"
          />
          <datalist id="live-dev-dl">
            <option v-for="opt in deviceOptions" :key="opt.value" :value="opt.label">{{ opt.hint }}</option>
          </datalist>
          <span class="lq-cur">
            <span class="lq-cur__dot" :class="`lq-cur__dot--${curDeviceStatus}`" aria-hidden="true" />
            <b>{{ curDeviceName }}</b>
            <i>{{ curDeviceMeta }}</i>
          </span>
          <span class="wc-spacer" />
          <button type="button" class="wc-btn" data-testid="live-pause" @click="togglePause">
            {{ paused ? '继续刷新' : '暂停刷新' }}
          </button>
          <span class="wc-conn" :class="streamConnClass">
            <span class="wc-conn__dot">●</span>
            <span>{{ streamConnLabel }} · 最后更新 {{ lastTickText }}</span>
          </span>
        </div>
      </div>
    </div>

    <!-- ② KPI（原型 :1670-1675） -->
    <div class="wc-grid wc-grid--4">
      <StatCard label="点位数" :value="String(scopedPoints.length)" :sub="curDeviceName" icon-tone="ink">
        <template #icon>◈</template>
      </StatCard>
      <StatCard label="采集频率" :value="freqValue" :unit="freqUnit" :sub="curDeviceProtocol" icon-tone="teal">
        <template #icon>◷</template>
      </StatCard>
      <StatCard label="连接状态" :value="connValue" :sub="connSub" :tone="connTone" icon-tone="violet">
        <template #icon>✓</template>
      </StatCard>
      <StatCard label="曲线窗口" value="55" unit="s" sub="12 个采样点" icon-tone="amber">
        <template #icon>▲</template>
      </StatCard>
    </div>

    <!-- ③ 点位面积曲线卡 -->
    <EmptyState
      v-if="scopedPoints.length === 0"
      title="没有可展示的实时曲线"
      desc="当前设备尚未配置点表。"
    >
      <template #actions>
        <button type="button" class="wc-btn wc-btn--primary" @click="go('points')">前往点位与映射</button>
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
          <!-- 面积曲线：12 点 / 55s 窗口（原型 :1293） -->
          <svg
            class="dv-area"
            viewBox="0 0 100 34"
            preserveAspectRatio="none"
            role="img"
            :aria-label="`${card.name} 近 55 秒（12 个采样点）趋势`"
          >
            <path class="dv-area__fill" :d="card.area" />
            <path class="dv-area__line" :d="card.line" />
          </svg>
          <div class="dv-card__foot">
            <span class="dv-card__win">-55s</span>
            <span class="wc-spacer" />
            <span class="dv-card__ts">{{ card.tsText }}</span>
            <span class="dv-card__win">now</span>
          </div>
        </div>
      </div>
      <UiPager
        :page="page"
        :total="scopedPoints.length"
        :page-size="pageSize"
        :sizes="PAGE_SIZES"
        numeric
        @update:page="onPage"
        @update:page-size="onPageSize"
      />
    </template>
  </div>
</template>

<script setup lang="ts">
/**
 * @file LivePage.vue
 * @module web-console/pages/LivePage
 * @description 实时数据：设备查询框切换设备 + 4 张 KPI + 点位面积曲线卡（12 点 / 55s 窗口）。
 *
 * ── 曲线窗口语义 ──────────────────────────────────────────────────────────────
 * 节拍仍为 1s（节流契约不变）；每 5 拍向窗口序列追加一个采样点、窗口保留 12 点，
 * 因此单卡曲线跨度 = 11 × 5s = 55s，与原型 :1278-1297 的 12 点 / 55s 窗口一致。
 */
import { computed, onBeforeUnmount, onMounted, reactive, ref, watch } from 'vue';
import { useRouter } from 'vue-router';
import { EmptyState, StatCard, UiPager } from '@ui-kit';
import {
  dataVersion,
  repo,
  type DeviceRecord,
  type PointRecord,
} from '@/api/repo';
import { pointSnapshots, snapshotKey, streamStatus, wireQualityToDataQuality } from '@/api/stream';

const router = useRouter();

// ---------------------------------------------------------------------------
// 常量
// ---------------------------------------------------------------------------

/** 每页卡片数（可经 UiPager 的 sizes chip 切换；UiPager 的 sizes 需要可变数组）。 */
const PAGE_SIZES: number[] = [6, 12, 24];

/** 曲线窗口采样点数（原型 :1274 / :1293）。 */
const WINDOW_POINTS = 12;

/**
 * 窗口采样间隔（拍）：1s 一拍 → 每 5s 采样一次。
 * 12 点 × 5s 间隔 = 55s 跨度（首点 -55s，末点 now）。
 */
const WINDOW_STEP_TICKS = 5;

/** 「全部设备」选项文案（保留既有能力：不选设备时展示全部点位）。 */
const ALL_DEVICES_LABEL = '全部设备';

/** 曲线卡逻辑坐标系高度（viewBox 0 0 100 34）。 */
const AREA_H = 34;

// ---------------------------------------------------------------------------
// 数据源
// ---------------------------------------------------------------------------

/** 全量设备（查询框 datalist 的数据源；dataVersion 驱动原地刷新）。 */
const devices = ref<DeviceRecord[]>(repo.allDevices());

/** 当前设备 id（'' = 全部设备）。 */
const liveDevice = ref<string>(devices.value.length > 0 ? devices.value[0].id : '');

/** 查询框输入值（与 liveDevice 同步；未命中时回显当前设备）。 */
const devKeyword = ref<string>(devices.value.length > 0 ? devices.value[0].name : ALL_DEVICES_LABEL);

/** 缓存填充后重读设备清单（避免预取晚于挂载时 datalist / onDevQuery 停留在空态）。 */
function reloadDevices(): void {
  devices.value = repo.allDevices();
}

/** datalist 选项：value = 设备名，hint = 协议 · 连接摘要。 */
const deviceOptions = computed<readonly { value: string; label: string; hint: string }[]>(() => [
  { value: '', label: ALL_DEVICES_LABEL, hint: '全部设备的点位' },
  ...devices.value.map((d) => ({
    value: d.id,
    label: d.name,
    hint: `${d.protocolLabel} · ${d.connectionSummary}`,
  })),
]);

/** 当前设备（'' 时为 null）。 */
const curDevice = computed<DeviceRecord | null>(() =>
  liveDevice.value ? devices.value.find((d) => d.id === liveDevice.value) ?? null : null,
);

/** 当前范围内的点位（静态，不随每秒变化）。 */
const scopedPoints = ref<PointRecord[]>([]);

/** 当前页号。 */
const page = ref(1);

/** 当前每页条数。 */
const pageSize = ref<number>(12);

// ---------------------------------------------------------------------------
// 运行时状态（每秒至多写一次）
// ---------------------------------------------------------------------------

interface PointRuntime {
  value: number | null;
  delta: number;
  /** 曲线窗口序列（12 点 / 55s） */
  window: number[];
  tsMs: number;
  /** 采样时间戳（**纳秒字符串**，由 SSE 帧透传，绝不 parseInt） */
  tsRaw: string;
  /** 质量码整数（由 SSE 帧透传；缺失为 null） */
  qualityCode: number | null;
}

const series = reactive<Record<string, PointRuntime>>({});

/** 质量（可被推流改写，这里沿用初始）。 */
const runtimeQuality = reactive<Record<string, PointRecord['quality']>>({});

/**
 * 以基线初始化运行时。曲线窗口恒从空开始（**绝不预填演示历史**），
 * 后续只由 `/api/stream` 的实际帧推进。
 */
function seed(): void {
  const nowMs = Date.now();
  for (const point of scopedPoints.value) {
    runtimeQuality[point.id] = point.quality;
    series[point.id] = {
      value: point.value,
      delta: 0,
      window: [],
      tsMs: nowMs,
      tsRaw: '',
      qualityCode: null,
    };
  }
}

/** 1s 节拍（唯一写入点）。 */
function tick(): void {
  tickReal();
}

/**
 * 节拍实现：消费 SSE 逐点快照（key = `${device_id}/${point_id}`）。
 *
 * · 快照存在 → 以流帧为准：值 / 质量（wire 枚举映射到前端 5 值枚举）/
 *   纳秒 ts 原文透传；每 5 拍（窗口采样点）且取到数值时才推进曲线窗口；
 * · 快照不存在（该点尚无流数据）→ 保持基线值与配置质量，不伪造；
 * · 时间戳使用 `receivedAtMs`（客户端接收时刻）做陈旧判定，业务纳秒字符串
 *   原样透传给 `tsRaw` 展示，绝不 `parseInt`（2^53 精度陷阱）。
 */
function tickReal(): void {
  const nowMs = Date.now();
  const sampleWindow = tickCount.value % WINDOW_STEP_TICKS === 0;
  for (const point of scopedPoints.value) {
    const state = series[point.id];
    if (!state) continue;
    const snap = pointSnapshots.get(snapshotKey(point.deviceId, point.id));
    if (!snap) {
      // 尚无流数据：保持基线值与配置质量，不伪造
      runtimeQuality[point.id] = point.quality;
      continue;
    }
    const quality = wireQualityToDataQuality(snap.quality);
    runtimeQuality[point.id] = quality;
    const previous = state.value;
    state.value = snap.value;
    state.delta = previous !== null && snap.value !== null ? Number((snap.value - previous).toFixed(2)) : 0;
    state.tsMs = snap.receivedAtMs;
    state.tsRaw = snap.ts; // 纳秒字符串透传，绝不 parseInt
    state.qualityCode = snap.qualityCode;
    // 仅每 5 拍入窗一次且取到数值 → 窗口跨度 55s
    if (snap.value !== null && sampleWindow) {
      state.window.push(snap.value);
      if (state.window.length > WINDOW_POINTS) state.window.shift();
    }
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

/** 顶栏连接指示：反映 SSE 通道状态。 */
const streamConnClass = computed<string>(() => {
  if (paused.value) return 'wc-conn--degraded';
  const s = streamStatus.value;
  if (s === 'open') return 'wc-conn--connected';
  return 'wc-conn--degraded'; // connecting / idle / unauthorized → 链路降级
});
const streamConnLabel = computed<string>(() => {
  if (paused.value) return '已暂停';
  const s = streamStatus.value;
  if (s === 'open') return '实时流已连接';
  if (s === 'connecting') return '实时流连接中…';
  if (s === 'unauthorized') return '鉴权失效';
  return '实时流未连接';
});

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

/** 缓存填充完成（dataVersion 自增）→ 重读设备清单与点位范围，避免预取晚于挂载时停留空态。 */
watch(dataVersion, () => {
  reloadDevices();
  refreshScope();
});

// ---------------------------------------------------------------------------
// 设备查询框（原型 :1261-1276）
// ---------------------------------------------------------------------------

/** 查询框确认：命中设备名则切换；未命中回显当前设备（不静默跳设备）。 */
function onDevQuery(): void {
  const kw = devKeyword.value.trim();
  if (!kw || kw === ALL_DEVICES_LABEL) {
    liveDevice.value = '';
    devKeyword.value = ALL_DEVICES_LABEL;
    return;
  }
  const hit = devices.value.find((d) => d.name === kw);
  if (hit) {
    liveDevice.value = hit.id;
    devKeyword.value = hit.name;
    return;
  }
  devKeyword.value = curDevice.value ? curDevice.value.name : ALL_DEVICES_LABEL;
}

// ---------------------------------------------------------------------------
// KPI 派生（原型 :1671-1674）
// ---------------------------------------------------------------------------

/** 采集频率（数值 + 单位分离，便于 StatCard 小号单位排版）。 */
const freqValue = computed<string>(() => {
  if (!curDevice.value) return '—';
  const ms = curDevice.value.intervalMs;
  return ms >= 1000 ? String(ms / 1000) : String(ms);
});
const freqUnit = computed<string>(() => {
  if (!curDevice.value) return '';
  return curDevice.value.intervalMs >= 1000 ? 's' : 'ms';
});

/** 连接状态文案与色调。 */
const connValue = computed<string>(() => {
  const status = curDevice.value?.status;
  if (status === 'online') return '在线';
  if (status === 'offline') return '离线';
  if (status === 'error') return '采集失败';
  return '多设备';
});
const connTone = computed<'default' | 'ok' | 'warn' | 'danger'>(() => {
  const status = curDevice.value?.status;
  if (!curDevice.value) return 'default';
  if (status === 'online') return 'ok';
  if (status === 'error') return 'warn';
  return 'danger';
});

/** 当前设备名 / 元信息（查询框右侧与 KPI 副标题共用）。 */
const curDeviceName = computed<string>(() => curDevice.value?.name ?? ALL_DEVICES_LABEL);
const curDeviceMeta = computed<string>(() =>
  curDevice.value ? `${curDevice.value.protocolLabel} · ${curDevice.value.connectionSummary}` : '全部设备的点位',
);
const curDeviceProtocol = computed<string>(() => curDevice.value?.protocolLabel ?? '全部协议');
const curDeviceStatus = computed<string>(() => curDevice.value?.status ?? 'multi');
/** 连接状态副标题：最后采集时间。 */
const connSub = computed<string>(() =>
  curDevice.value ? `最后采集 ${curDevice.value.lastSampleAt}` : `${devices.value.length} 台设备的合计点位`,
);

// ---------------------------------------------------------------------------
// 渲染
// ---------------------------------------------------------------------------

const lastTickText = computed(() => {
  const d = new Date(lastTickMs.value);
  const p = (n: number): string => String(n).padStart(2, '0');
  return `${p(d.getHours())}:${p(d.getMinutes())}:${p(d.getSeconds())}`;
});

const staticRows = computed<readonly PointRecord[]>(() => {
  const start = (page.value - 1) * pageSize.value;
  return scopedPoints.value.slice(start, start + pageSize.value);
});

interface CardRow {
  id: string;
  targetKey: string;
  name: string;
  valueText: string;
  unit: string;
  deltaText: string;
  deltaClass: string;
  /** 面积路径（闭合到基线） */
  area: string;
  /** 折线路径 */
  line: string;
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

/** 曲线折线路径：窗口序列 → 100×34 逻辑坐标（上下各留 4/2 的余量）。 */
function linePathOf(win: readonly number[]): string {
  if (win.length === 0) return 'M0,17 L100,17';
  const min = Math.min(...win);
  const max = Math.max(...win);
  const span = max - min;
  const n = win.length;
  return win
    .map((v, i) => {
      const x = n <= 1 ? 0 : (i / (n - 1)) * 100;
      const y = span === 0 ? 17 : 30 - ((v - min) / span) * 26;
      return `${i === 0 ? 'M' : 'L'}${x.toFixed(1)},${y.toFixed(1)}`;
    })
    .join(' ');
}

/** 面积路径：折线 + 回落到基线闭合。 */
function areaPathOf(win: readonly number[]): string {
  if (win.length === 0) return 'M0,32 L100,32 Z';
  return `${linePathOf(win)} L100,${AREA_H} L0,${AREA_H} Z`;
}

const pagedRows = computed<readonly CardRow[]>(() => {
  const nowMs = Date.now();
  return staticRows.value.map((point) => {
    const state = series[point.id];
    const quality = runtimeQuality[point.id] ?? point.quality;
    const value = state ? state.value : point.value;
    const delta = state ? state.delta : 0;
    const win = state ? state.window : [];
    const tsMs = state ? state.tsMs : nowMs;
    const tsRaw = state ? state.tsRaw : '';
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
      area: areaPathOf(win),
      line: linePathOf(win),
      quality,
      qualityClass: qualityTagClass(quality),
      tsText: tsRaw
        ? tsRaw
        : stale
          ? `${ageSec.toFixed(0)}s 前（陈旧）`
          : `${(tsMs / 1000).toFixed(3)}`,
      stale,
    };
  });
});

function onPage(next: number): void {
  page.value = next;
}
function onPageSize(next: number): void {
  pageSize.value = next;
  page.value = 1;
}
function go(name: string): void {
  void router.push({ name });
}
</script>

<style scoped>
/* 设备查询框（原型 :628-635） */
.live-q {
  display: flex;
  align-items: center;
  gap: 14px;
  flex-wrap: wrap;
}
.live-q__input {
  font-family: var(--font-mono);
  font-size: var(--fs-caption);
  padding: 9px 13px;
  min-height: 38px;
  width: 320px;
  border: 1px solid var(--border);
  border-radius: var(--radius-btn);
  background: var(--bg-hover);
  color: var(--text-1);
}
.live-q__input:focus {
  outline: none;
  border-color: var(--brand);
  background: var(--bg-card);
  box-shadow: 0 0 0 3px rgba(23, 195, 178, 0.16);
}
.lq-cur {
  display: inline-flex;
  align-items: center;
  gap: 8px;
  font-size: var(--fs-caption);
  color: var(--text-2);
}
.lq-cur b {
  font-family: var(--font-display);
  color: var(--text-1);
  font-size: var(--fs-table);
}
.lq-cur i {
  font-style: normal;
  font-family: var(--font-mono);
  font-size: 11px;
  color: var(--text-3);
}
.lq-cur__dot {
  width: 7px;
  height: 7px;
  border-radius: 50%;
  background: var(--unknown);
  display: block;
}
.lq-cur__dot--online {
  background: var(--ok);
}
.lq-cur__dot--error {
  background: var(--warn);
}
.lq-cur__dot--offline {
  background: var(--danger);
}

.dv-cards {
  display: grid;
  grid-template-columns: repeat(auto-fill, minmax(260px, 1fr));
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
/* 面积曲线（原型 :1293：accent 描边 + 12% 面积） */
.dv-area {
  width: 100%;
  height: 44px;
  display: block;
}
.dv-area__fill {
  fill: var(--brand);
  opacity: 0.12;
  stroke: none;
}
.dv-area__line {
  fill: none;
  stroke: var(--brand);
  stroke-width: 1.6;
  stroke-linejoin: round;
}
.dv-card__foot {
  display: flex;
  align-items: center;
  gap: 8px;
  font-size: 11px;
  color: var(--text-3);
}
.dv-card__win {
  font-family: var(--font-mono);
}
.dv-card__ts {
  white-space: nowrap;
}
</style>

<template>
  <!--
    BarChart —— 极简柱状图（内联 SVG，零图表依赖）。
    支持多序列分组与「缺失点标红」，用于激活趋势与心跳时序。
    `preserveAspectRatio="none"` 让图表随容器拉伸，适配 1366 起的窄屏。
  -->
  <div class="ac-chart">
    <svg :viewBox="`0 0 ${W} ${H}`" :style="{ width: '100%', height: `${height}px` }" preserveAspectRatio="none">
      <!-- 网格与 Y 轴刻度 -->
      <g v-for="g in 4" :key="`grid-${g}`">
        <line
          :x1="PAD.left"
          :y1="yOfGrid(g - 1)"
          :x2="W - PAD.right"
          :y2="yOfGrid(g - 1)"
          stroke="#F2F3F5"
        />
        <text
          :x="PAD.left - 6"
          :y="yOfGrid(g - 1) + 4"
          text-anchor="end"
          font-size="10"
          fill="#86909C"
        >
          {{ gridLabel(g - 1) }}
        </text>
      </g>

      <!-- 柱体 -->
      <g v-for="(label, i) in labels" :key="`col-${i}`">
        <rect
          v-for="(s, si) in series"
          :key="`bar-${i}-${si}`"
          :x="barX(i, si)"
          :y="barY(s.data[i] ?? 0)"
          :width="barWidth"
          :height="barHeight(s.data[i] ?? 0)"
          rx="2"
          :fill="s.missFlag && s.missFlag[i] ? '#F53F3F' : s.color"
        />
        <!-- X 轴标签：稀疏显示避免重叠 -->
        <text
          v-if="i % labelStep === 0"
          :x="PAD.left + i * groupWidth + groupWidth / 2"
          :y="H - 6"
          text-anchor="middle"
          font-size="10"
          fill="#86909C"
        >
          {{ label }}
        </text>
      </g>
    </svg>
    <div class="ac-chart__legend">
      <span v-for="s in series" :key="s.name">
        <i :style="{ background: s.color }" />{{ s.name }}
      </span>
    </div>
  </div>
</template>

<script setup lang="ts">
/**
 * @file BarChart.vue
 * @module admin-console/components/BarChart
 * @description 内联 SVG 分组柱状图。
 */
import { computed } from 'vue';

/** 数据序列。 */
export interface BarSeries {
  /** 序列名 */
  name: string;
  /** 填充色（取自 ui-kit token） */
  color: string;
  /** 每列数值 */
  data: readonly number[];
  /** 需要标红的缺失点（与 data 同序） */
  missFlag?: readonly boolean[];
}

interface Props {
  /** X 轴标签 */
  labels: readonly string[];
  /** 序列 */
  series: readonly BarSeries[];
  /** 图表高度（px） */
  height?: number;
}

const props = withDefaults(defineProps<Props>(), { height: 160 });

/** 逻辑坐标系（viewBox 固定，外层拉伸）。 */
const W = 620;
const H = 170;
const PAD = { left: 40, right: 12, top: 14, bottom: 24 };

/** 绘图区宽高。 */
const innerW = computed(() => W - PAD.left - PAD.right);
const innerH = computed(() => H - PAD.top - PAD.bottom);

/** 每列宽度。 */
const groupWidth = computed(() => (props.labels.length > 0 ? innerW.value / props.labels.length : innerW.value));

/** 单根柱宽度（留 30% 间距）。 */
const barWidth = computed(() => Math.max(2, (groupWidth.value * 0.7) / Math.max(1, props.series.length)));

/** 全量最大值（乘 1.15 留顶部余量，避免顶格）。 */
const maxValue = computed(() => {
  const all: number[] = [];
  for (const s of props.series) {
    all.push(...s.data);
  }
  return Math.max(...all, 1) * 1.15;
});

/** X 轴标签稀疏步长。 */
const labelStep = computed(() => Math.max(1, Math.ceil(props.labels.length / 8)));

/** 第 i 列、第 si 根柱的 x 坐标。 */
function barX(index: number, seriesIndex: number): number {
  return PAD.left + index * groupWidth.value + groupWidth.value * 0.15 + seriesIndex * barWidth.value;
}

/** 柱体高度（px）。 */
function barHeight(value: number): number {
  return (value / maxValue.value) * innerH.value;
}

/** 柱体 y 坐标（自底向上）。 */
function barY(value: number): number {
  return PAD.top + innerH.value - barHeight(value);
}

/** Y 轴网格线的 y 坐标（t: 0 = 顶部）。 */
function yOfGrid(t: number): number {
  return PAD.top + (innerH.value * t) / 3;
}

/** Y 轴刻度文案（k 缩写）。 */
function gridLabel(t: number): string {
  const value = maxValue.value * (1 - t / 3);
  if (value >= 1000) {
    return `${(value / 1000).toFixed(1)}k`;
  }
  return String(Math.round(value));
}
</script>

<style scoped>
.ac-chart {
  width: 100%;
}
</style>

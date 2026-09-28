<template>
  <!--
    StatCard —— KPI 卡（渐变图标方块 + 标签 + 大数值 + 单位 + 环比 + 迷你趋势）。
    设计基准：A·冰川原型 kpi-ic（:198-221）——42px 渐变图标方块为记忆点；
    设计系统 §3：数值等宽字体；环比升降用箭头 + 文案，不单靠颜色。
    图标为可选槽：不传 icon 槽时保持原布局，不渲染方块（布局不塌）。
  -->
  <div class="uik-stat" :class="{ 'is-clickable': clickable, 'has-icon': hasIcon }" @click="onClick">
    <div v-if="hasIcon" class="uik-stat__top">
      <div class="uik-stat__icon" :class="`uik-stat__icon--${iconTone}`">
        <slot name="icon" />
      </div>
      <div class="uik-stat__label">{{ label }}</div>
    </div>
    <div v-else class="uik-stat__label">{{ label }}</div>
    <div class="uik-stat__value" :class="toneClass">
      <span class="uik-stat__num">{{ value }}</span>
      <span v-if="unit" class="uik-stat__unit">{{ unit }}</span>
    </div>
    <div class="uik-stat__foot">
      <!-- 环比：箭头（形状）+ 文案（语义）+ 颜色（辅助），三重表达方向 -->
      <span v-if="delta !== null" class="uik-stat__delta" :class="deltaClass">
        <span aria-hidden="true">{{ deltaArrow }}</span>{{ deltaText }}
      </span>
      <span v-if="sub" class="uik-stat__sub">{{ sub }}</span>
    </div>
  </div>
</template>

<script setup lang="ts">
/**
 * @file StatCard.vue
 * @module ui-kit/components/StatCard
 * @description KPI 统计卡。
 */
import { computed, useSlots } from 'vue';

interface Props {
  /** 指标名 */
  label: string;
  /** 指标值（已格式化的字符串，便于直接内联小号单位文案） */
  value: string;
  /** 单位 */
  unit?: string;
  /** 环比数值（正数上升 / 负数下降 / null 不显示） */
  delta?: number | null;
  /** 底部补充说明（如「本月新激活 46」） */
  sub?: string;
  /** 数值色调（可选，默认主色文本） */
  tone?: 'default' | 'ok' | 'warn' | 'danger';
  /** 是否可点击（跳转到明细页） */
  clickable?: boolean;
  /** 图标方块渐变色相（A·冰川 kpi-ic，可选，默认 teal 青绿） */
  iconTone?: 'teal' | 'ink' | 'amber' | 'violet' | 'rose';
}

const props = withDefaults(defineProps<Props>(), {
  unit: '',
  delta: null,
  sub: '',
  tone: 'default',
  clickable: false,
  iconTone: 'teal',
});

const emit = defineEmits<{
  /** 点击卡片 */
  (e: 'click'): void;
}>();

/** 是否传入 icon 槽（传入才渲染 42px 渐变方块，缺省不塌布局）。 */
const hasIcon = computed(() => !!useSlots().icon);

/** 数值色调类。 */
const toneClass = computed(() => (props.tone === 'default' ? '' : `uik-stat__value--${props.tone}`));

/** 环比箭头：上升 ↑ / 下降 ↓ / 持平 →。 */
const deltaArrow = computed(() => {
  if (props.delta === null || props.delta === 0) {
    return '→';
  }
  return props.delta > 0 ? '↑' : '↓';
});

/** 环比文案：带符号数值，避免只看颜色。 */
const deltaText = computed(() => {
  if (props.delta === null) {
    return '';
  }
  const abs = Math.abs(props.delta);
  return props.delta === 0 ? ' 持平' : ` ${abs}`;
});

/** 环比颜色：仅表示方向，不代表好坏（设计系统 §4.2）。 */
const deltaClass = computed(() => {
  if (props.delta === null || props.delta === 0) {
    return 'is-flat';
  }
  return props.delta > 0 ? 'is-up' : 'is-down';
});

/** 点击：仅在可点击时冒泡。 */
function onClick(): void {
  if (props.clickable) {
    emit('click');
  }
}
</script>

<style scoped>
.uik-stat {
  background: var(--bg-card);
  border: 1px solid var(--border);
  border-radius: var(--radius);
  padding: 14px 16px;
  display: flex;
  flex-direction: column;
  gap: 2px;
  box-shadow: var(--shadow-2);
}
/* 带 KPI 渐变图标方块时，加大纵向呼吸（原型 .kpi gap:12px） */
.uik-stat.has-icon {
  gap: 12px;
  padding: 18px 20px;
}
.uik-stat__top {
  display: flex;
  align-items: center;
  gap: 12px;
}
/* 42px 渐变图标方块（原型 .kpi-ic :206-212） */
.uik-stat__icon {
  width: 42px;
  height: 42px;
  flex: 0 0 42px;
  border-radius: 13px;
  display: flex;
  align-items: center;
  justify-content: center;
  font-size: 18px;
  box-shadow: 0 8px 18px -8px rgba(16, 24, 60, 0.6);
}
.uik-stat__icon--teal {
  background: var(--grad-teal);
  color: #04241f;
}
.uik-stat__icon--ink {
  background: var(--grad-ink);
  color: #fff;
}
.uik-stat__icon--amber {
  background: linear-gradient(135deg, #ffc249 0%, #f79009 100%);
  color: #4a2a00;
}
.uik-stat__icon--violet {
  background: linear-gradient(135deg, #a78bfa 0%, #7a5af8 100%);
  color: #fff;
}
.uik-stat__icon--rose {
  background: linear-gradient(135deg, #fb7185 0%, #f04438 100%);
  color: #fff;
}
.uik-stat.is-clickable {
  cursor: pointer;
}
.uik-stat.is-clickable:hover {
  border-color: var(--brand);
}
.uik-stat__label {
  font-size: var(--fs-caption);
  color: var(--text-3);
}
.uik-stat__value {
  display: flex;
  align-items: baseline;
  gap: 4px;
  font-family: var(--font-mono);
  font-variant-numeric: tabular-nums;
  font-size: 26px;
  font-weight: 600;
  line-height: 1.3;
  color: var(--text-1);
}
.uik-stat__value--ok {
  color: var(--ok);
}
.uik-stat__value--warn {
  color: var(--warn);
}
.uik-stat__value--danger {
  color: var(--danger);
}
.uik-stat__unit {
  font-size: var(--fs-caption);
  font-weight: 400;
  color: var(--text-3);
}
.uik-stat__foot {
  display: flex;
  align-items: center;
  gap: 8px;
  font-size: var(--fs-caption);
  color: var(--text-3);
}
.uik-stat__delta.is-up {
  color: var(--ok);
}
.uik-stat__delta.is-down {
  color: var(--brand);
}
.uik-stat__delta.is-flat {
  color: var(--text-3);
}
</style>

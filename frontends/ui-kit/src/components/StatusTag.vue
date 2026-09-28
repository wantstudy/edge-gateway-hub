<template>
  <!--
    StatusTag —— 全系统**唯一**的状态标签实现。
    设计系统 §3 硬约束：「每个状态固定色 + 固定文案，禁止各处自造同义词」。
    颜色 + 图标 + 文案三重信号，满足「颜色不是唯一信号」与色盲可辨要求。

    `variant="icon"`：紧凑场景（如表格单元格内与时间同行）只渲染符号，
    状态含义通过 `title` / `aria-label` 传达；默认 `variant="tag"` 行为不变。
  -->
  <span
    v-if="isIcon"
    class="uik-tag-icon"
    :class="`uik-tag-icon--${view.tone}`"
    role="img"
    :aria-label="iconText"
    :title="iconText"
  >
    <span class="uik-tag__icon" v-html="iconSvg" />
  </span>
  <span v-else class="uik-tag" :class="`uik-tag--${view.tone}`" :title="titleText">
    <span class="uik-tag__icon" v-html="iconSvg" />
    <span class="uik-tag__text">{{ view.label }}</span>
  </span>
</template>

<script setup lang="ts">
/**
 * @file StatusTag.vue
 * @module ui-kit/components/StatusTag
 * @description 状态标签。所有状态文案/颜色的唯一出口。
 */
import { computed } from 'vue';
import { statusView } from '../status-map';
import { ICON_CHECK, ICON_WARN, ICON_DANGER, ICON_INFO, ICON_DOT } from '../icons';
import type { Tone } from '../tokens';

interface Props {
  /** 后端原始状态值（如 `bound` / `online` / `receipt_gap`） */
  status: string | null | undefined;
  /** 覆盖展示文案（仅用于需要补充上下文的场景，如「试用剩 2 天」） */
  text?: string;
  /**
   * 渲染形态：`tag`（默认，符号 + 文案胶囊）/ `icon`（仅符号）。
   *
   * `icon` 用于行内紧凑布局；状态含义通过 `title` + `aria-label` 保留，
   * 不放宽「颜色不是唯一信号」的约束。
   */
  variant?: 'tag' | 'icon';
}

const props = withDefaults(defineProps<Props>(), {
  text: '',
  variant: 'tag',
});

/** 是否 icon-only 形态。 */
const isIcon = computed(() => props.variant === 'icon');

/** 解析出的状态视图（文案 + 色调）。 */
const view = computed(() => statusView(props.status));

/** 悬停提示：回显原始枚举值，便于排障定位。 */
const titleText = computed(() => (props.status ? `状态码：${props.status}` : ''));

/**
 * icon 形态的可读文本（`title` + `aria-label`）：
 * 固定中文文案 + 原始状态码，保证符号脱离颜色仍可辨。
 */
const iconText = computed(() => {
  const label = props.text || view.value.label;
  return props.status ? `状态：${label}（${props.status}）` : `状态：${label}`;
});

/** 按色调选择与之配对的图标形状。 */
const TONE_ICON: Record<Tone, string> = {
  ok: ICON_CHECK,
  warn: ICON_WARN,
  danger: ICON_DANGER,
  info: ICON_INFO,
  unknown: ICON_DOT,
};

/** 当前色调对应的图标 SVG。 */
const iconSvg = computed(() => TONE_ICON[view.value.tone]);
</script>

<style scoped>
.uik-tag {
  display: inline-flex;
  align-items: center;
  gap: 4px;
  padding: 1px 8px;
  border-radius: var(--radius-pill);
  font-size: var(--fs-caption);
  line-height: 18px;
  border: 1px solid;
  white-space: nowrap;
  /* 浅底 + 深字，不用深底白字（设计系统 §6） */
  background: var(--unknown-bg);
  color: var(--unknown-fg);
  border-color: var(--unknown-border);
}
.uik-tag--ok {
  background: var(--ok-bg);
  color: var(--ok-fg);
  border-color: var(--ok-border);
}
.uik-tag--warn {
  background: var(--warn-bg);
  color: var(--warn-fg);
  border-color: var(--warn-border);
}
.uik-tag--danger {
  background: var(--danger-bg);
  color: var(--danger-fg);
  border-color: var(--danger-border);
}
.uik-tag--info {
  background: var(--info-bg);
  color: var(--info-fg);
  border-color: var(--info-border);
}
.uik-tag--unknown {
  background: var(--unknown-bg);
  color: var(--unknown-fg);
  border-color: var(--unknown-border);
}
/* icon-only 形态：圆形符号，与 tag 同源色调 */
.uik-tag-icon {
  display: inline-flex;
  align-items: center;
  justify-content: center;
  width: 18px;
  height: 18px;
  flex: 0 0 auto;
  border-radius: var(--radius-pill);
  border: 1px solid;
  background: var(--unknown-bg);
  color: var(--unknown-fg);
  border-color: var(--unknown-border);
}
.uik-tag-icon--ok {
  background: var(--ok-bg);
  color: var(--ok-fg);
  border-color: var(--ok-border);
}
.uik-tag-icon--warn {
  background: var(--warn-bg);
  color: var(--warn-fg);
  border-color: var(--warn-border);
}
.uik-tag-icon--danger {
  background: var(--danger-bg);
  color: var(--danger-fg);
  border-color: var(--danger-border);
}
.uik-tag-icon--info {
  background: var(--info-bg);
  color: var(--info-fg);
  border-color: var(--info-border);
}
.uik-tag-icon--unknown {
  background: var(--unknown-bg);
  color: var(--unknown-fg);
  border-color: var(--unknown-border);
}
.uik-tag-icon .uik-tag__icon :deep(svg) {
  width: 12px;
  height: 12px;
}
.uik-tag__icon {
  display: inline-flex;
  align-items: center;
}
.uik-tag__icon :deep(svg) {
  width: 12px;
  height: 12px;
}
</style>

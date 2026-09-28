<template>
  <!--
    EmptyState —— 空态必须给下一步动作（设计系统 §4.1）。
    绝不出现「白板」：无数据时同时提供「主操作 + 备选入口」。
  -->
  <div class="uik-empty">
    <div class="uik-empty__art" v-html="EMPTY_ART" />
    <p class="uik-empty__title">{{ title }}</p>
    <p v-if="desc" class="uik-empty__desc">{{ desc }}</p>
    <div v-if="$slots.actions" class="uik-empty__ops">
      <slot name="actions" />
    </div>
  </div>
</template>

<script setup lang="ts">
/**
 * @file EmptyState.vue
 * @module ui-kit/components/EmptyState
 * @description 列表/区块空态。动作为必填（通过插槽强制调用方提供出口）。
 */
import { ICON_EMPTY } from '../icons';

interface Props {
  /** 空态标题（说明「没有什么」） */
  title: string;
  /** 补充说明（说明「为什么为空 / 如何产生第一条数据」） */
  desc?: string;
}

withDefaults(defineProps<Props>(), {
  desc: '',
});

/** 空态插画（内联 SVG，零外部资源）。 */
const EMPTY_ART = ICON_EMPTY;
</script>

<style scoped>
.uik-empty {
  display: flex;
  flex-direction: column;
  align-items: center;
  gap: 6px;
  padding: 40px 16px;
  text-align: center;
}
.uik-empty__art {
  color: var(--border);
  margin-bottom: 4px;
}
.uik-empty__title {
  margin: 0;
  font-size: var(--fs-body);
  color: var(--text-2);
}
.uik-empty__desc {
  margin: 0;
  font-size: var(--fs-caption);
  color: var(--text-3);
  max-width: 460px;
  line-height: 1.6;
}
.uik-empty__ops {
  display: flex;
  gap: 8px;
  margin-top: 10px;
}
</style>

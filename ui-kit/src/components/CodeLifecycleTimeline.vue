<template>
  <!--
    CodeLifecycleTimeline —— 激活码生命周期时间线（发放 → 绑定 → 废弃 → 重发 → 再绑定）。
    设计系统 §3：每步带时间、操作者、原因、关联码；用于「一键回答它是谁、谁操作的、为什么废弃」。
  -->
  <ol class="uik-tl">
    <li v-for="(node, i) in nodes" :key="i" class="uik-tl__item" :class="`uik-tl__item--${node.tone}`">
      <div class="uik-tl__time">{{ node.time }}</div>
      <div class="uik-tl__title">
        <span class="uik-tl__badge">{{ node.action }}</span>
        <span v-if="node.target" class="uik-tl__target">{{ node.target }}</span>
      </div>
      <div v-if="node.operator || node.reason" class="uik-tl__meta">
        <span v-if="node.operator">操作者：{{ node.operator }}</span>
        <span v-if="node.reason">原因：{{ node.reason }}</span>
      </div>
      <div v-if="node.detail" class="uik-tl__detail">{{ node.detail }}</div>
    </li>
  </ol>
</template>

<script setup lang="ts">
/**
 * @file CodeLifecycleTimeline.vue
 * @module ui-kit/components/CodeLifecycleTimeline
 * @description 激活码生命周期时间线（纯展示）。
 */
import type { Tone } from '../tokens';

/** 时间线节点。 */
export interface TimelineNode {
  /** 时间（`YYYY-MM-DD HH:mm:ss`） */
  time: string;
  /** 动作名（发放 / 绑定 / 废弃 / 重发 / 再绑定） */
  action: string;
  /** 色调（已发生用实心色，未发生用灰点） */
  tone: Tone;
  /** 关联对象（设备摘要 / 新码标识） */
  target?: string;
  /** 操作者 */
  operator?: string;
  /** 原因（高危操作必有） */
  reason?: string;
  /** 其他明细（备注 / 有效期 / 指纹校验结果） */
  detail?: string;
}

interface Props {
  /** 节点列表（按时间升序） */
  nodes: readonly TimelineNode[];
}

defineProps<Props>();
</script>

<style scoped>
.uik-tl {
  position: relative;
  margin: 0;
  padding-left: 20px;
  list-style: none;
}
.uik-tl::before {
  content: '';
  position: absolute;
  left: 5px;
  top: 6px;
  bottom: 6px;
  width: 1px;
  background: var(--border);
}
.uik-tl__item {
  position: relative;
  padding-bottom: 16px;
}
.uik-tl__item:last-child {
  padding-bottom: 0;
}
/* 节点圆点：用色调驱动边框/填充，未发生节点为空心灰点 */
.uik-tl__item::before {
  content: '';
  position: absolute;
  left: -19px;
  top: 5px;
  width: 9px;
  height: 9px;
  border-radius: 50%;
  background: #fff;
  border: 2px solid var(--border);
  box-sizing: border-box;
}
.uik-tl__item--ok::before {
  border-color: var(--ok);
  background: var(--ok);
}
.uik-tl__item--warn::before {
  border-color: var(--warn);
}
.uik-tl__item--danger::before {
  border-color: var(--danger);
}
.uik-tl__item--info::before {
  border-color: var(--info);
}
.uik-tl__time {
  font-family: var(--font-mono);
  font-variant-numeric: tabular-nums;
  font-size: var(--fs-caption);
  color: var(--text-3);
}
.uik-tl__title {
  display: flex;
  align-items: center;
  gap: 8px;
  font-size: var(--fs-table);
  font-weight: 600;
  color: var(--text-1);
}
.uik-tl__badge {
  display: inline-block;
  font-weight: 600;
}
.uik-tl__target {
  font-family: var(--font-mono);
  font-weight: 400;
  color: var(--text-2);
}
.uik-tl__meta {
  display: flex;
  gap: 14px;
  flex-wrap: wrap;
  font-size: var(--fs-caption);
  color: var(--text-2);
  margin-top: 2px;
}
.uik-tl__detail {
  font-size: var(--fs-caption);
  color: var(--text-3);
  margin-top: 2px;
  line-height: 1.6;
}
</style>

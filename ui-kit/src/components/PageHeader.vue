<template>
  <!-- 页面头部：面包屑 + 标题 + 说明 + 右侧唯一主操作区 -->
  <header class="uik-page-head">
    <div class="uik-page-head__main">
      <nav class="uik-page-head__crumb" aria-label="面包屑">{{ crumb }}</nav>
      <h1 class="uik-page-head__title">{{ title }}</h1>
      <p v-if="desc" class="uik-page-head__desc">{{ desc }}</p>
    </div>
    <!-- 同页只允许一个主按钮（设计系统 §2.3） -->
    <div v-if="$slots.actions" class="uik-page-head__ops">
      <slot name="actions" />
    </div>
  </header>
</template>

<script setup lang="ts">
/**
 * @file PageHeader.vue
 * @module ui-kit/components/PageHeader
 * @description 页面头部（面包屑 / 标题 / 说明 / 主操作槽）。
 */

interface Props {
  /** 面包屑文案 */
  crumb: string;
  /** 页面标题 */
  title: string;
  /** 页面说明（可选，用于写清口径与边界） */
  desc?: string;
}

withDefaults(defineProps<Props>(), {
  desc: '',
});
</script>

<style scoped>
.uik-page-head {
  background: var(--bg-card);
  border-bottom: 1px solid var(--border);
  padding: 16px 24px;
  display: flex;
  align-items: flex-start;
  gap: 16px;
}
.uik-page-head__main {
  min-width: 0;
}
.uik-page-head__crumb {
  font-size: var(--fs-caption);
  color: var(--text-3);
  margin-bottom: 6px;
}
.uik-page-head__title {
  margin: 0;
  font-size: var(--fs-page);
  font-weight: 600;
  color: var(--text-1);
}
.uik-page-head__desc {
  margin: 4px 0 0;
  font-size: var(--fs-caption);
  color: var(--text-3);
  max-width: 900px;
  line-height: 1.6;
}
.uik-page-head__ops {
  margin-left: auto;
  display: flex;
  gap: 8px;
  flex: 0 0 auto;
  align-items: flex-start;
}
</style>

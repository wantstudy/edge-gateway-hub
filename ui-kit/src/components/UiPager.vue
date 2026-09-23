<template>
  <!--
    UiPager —— 通用分页条（受控组件，页码状态由调用页面持有）。
    两端共用：列表页分页语义一致（总数 + 首/上/下/末页 + 边界裁剪）。
  -->
  <div class="uik-pager">
    <span class="uik-pager__info">共 {{ total }} 条 · 第 {{ page }} / {{ totalPages }} 页</span>
    <div class="uik-pager__ops">
      <button type="button" class="uik-pager__btn" :disabled="page <= 1" @click="emitPage(1)">首页</button>
      <button type="button" class="uik-pager__btn" :disabled="page <= 1" @click="emitPage(page - 1)">上一页</button>
      <button type="button" class="uik-pager__btn" :disabled="page >= totalPages" @click="emitPage(page + 1)">
        下一页
      </button>
      <button type="button" class="uik-pager__btn" :disabled="page >= totalPages" @click="emitPage(totalPages)">
        末页
      </button>
    </div>
  </div>
</template>

<script setup lang="ts">
/**
 * @file UiPager.vue
 * @module ui-kit/components/UiPager
 * @description 分页条。样式自带，不依赖应用侧 class。
 */
import { computed } from 'vue';

interface Props {
  /** 当前页（从 1 开始） */
  page: number;
  /** 总条数 */
  total: number;
  /** 每页条数 */
  pageSize: number;
}

const props = defineProps<Props>();

const emit = defineEmits<{
  /** 换页（已完成边界裁剪） */
  (e: 'update:page', page: number): void;
}>();

/** 总页数（至少 1 页，避免出现 0/0）。 */
const totalPages = computed(() => Math.max(1, Math.ceil(props.total / props.pageSize)));

/** 换页（带边界裁剪，避免越界请求）。 */
function emitPage(next: number): void {
  emit('update:page', Math.min(Math.max(1, next), totalPages.value));
}
</script>

<style scoped>
.uik-pager {
  display: flex;
  align-items: center;
  gap: 12px;
  padding: 10px 12px;
  border-top: 1px solid var(--divider);
  font-size: var(--fs-caption);
  color: var(--text-3);
}
.uik-pager__info {
  white-space: nowrap;
}
.uik-pager__ops {
  margin-left: auto;
  display: flex;
  gap: 6px;
}
.uik-pager__btn {
  font-family: inherit;
  font-size: var(--fs-caption);
  padding: 3px 10px;
  min-height: 26px;
  border-radius: var(--radius-sm);
  border: 1px solid var(--border);
  background: #fff;
  color: var(--text-1);
  cursor: pointer;
  white-space: nowrap;
}
.uik-pager__btn:hover:not(:disabled) {
  border-color: #c9cdd4;
  background: var(--bg-hover);
}
.uik-pager__btn:disabled {
  opacity: 0.5;
  cursor: not-allowed;
}
</style>

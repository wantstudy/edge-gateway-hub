<template>
  <!--
    UiPager —— 通用分页条（受控组件，页码状态由调用页面持有）。
    两端共用：列表页分页语义一致（总数 + 边界裁剪）。
    视觉基准：A·冰川原型 pager（:475-489 样式 / :2845-2871 结构）。
    向后兼容：既有消费方仅用 page/total/page-size + update:page，默认渲染
    首/上/下/末页文本按钮（原样保留）；三项新能力全部走可选 props 显式开启：
      · sizes     → 每页条数 chip 组（5/10/20/50），选中回传 update:pageSize
      · numeric   → 数字页码序列（首/尾 + 当前±1 + 省略号）替代文本按钮
      · jump      → 「跳至 N 页」输入框
  -->
  <div class="uik-pager">
    <span class="uik-pager__info">共 {{ total }} 条 · 第 {{ page }} / {{ totalPages }} 页</span>
    <!-- 每页条数 chip 组（可选）：原型 .pg-sizes -->
    <div v-if="sizes.length" class="uik-pager__sizes" role="group" aria-label="每页条数">
      <span class="uik-pager__sizes-label">每页</span>
      <button
        v-for="n in sizes"
        :key="n"
        type="button"
        class="uik-pager__chip"
        :class="{ 'is-on': n === pageSize }"
        @click="emitSize(n)"
      >
        {{ n }}
      </button>
    </div>
    <div class="uik-pager__ops" :class="{ 'is-push': !sizes.length }">
      <template v-if="numeric">
        <!-- 数字页码序列（可选）：原型 .pg-nav —— ‹ + 序列 + › -->
        <button
          type="button"
          class="uik-pager__btn uik-pager__btn--num"
          :disabled="page <= 1"
          aria-label="上一页"
          @click="emitPage(page - 1)"
        >
          ‹
        </button>
        <template v-for="(n, i) in pageSeq" :key="`${n}-${i}`">
          <span v-if="n === '…'" class="uik-pager__gap">…</span>
          <button
            v-else
            type="button"
            class="uik-pager__btn uik-pager__btn--num"
            :class="{ 'is-on': n === page }"
            :aria-current="n === page ? 'page' : 'false'"
            @click="emitPage(n as number)"
          >
            {{ n }}
          </button>
        </template>
        <button
          type="button"
          class="uik-pager__btn uik-pager__btn--num"
          :disabled="page >= totalPages"
          aria-label="下一页"
          @click="emitPage(page + 1)"
        >
          ›
        </button>
      </template>
      <template v-else>
        <!-- 默认：文本按钮（与既有消费方渲染保持一致） -->
        <button type="button" class="uik-pager__btn" :disabled="page <= 1" @click="emitPage(1)">首页</button>
        <button type="button" class="uik-pager__btn" :disabled="page <= 1" @click="emitPage(page - 1)">上一页</button>
        <button type="button" class="uik-pager__btn" :disabled="page >= totalPages" @click="emitPage(page + 1)">
          下一页
        </button>
        <button type="button" class="uik-pager__btn" :disabled="page >= totalPages" @click="emitPage(totalPages)">
          末页
        </button>
      </template>
    </div>
    <!-- 跳页输入（可选）：原型 .pg-jump -->
    <div v-if="jump" class="uik-pager__jump">
      <span>跳至</span>
      <input type="number" min="1" :max="totalPages" :value="page" aria-label="跳至指定页" @change="onJump" />
      <span>页</span>
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
  /** 可选：每页条数选项组（如 [5,10,20,50]）；传入才渲染 chip 组，缺省不渲染 */
  sizes?: number[];
  /** 可选：数字页码序列（首/尾 + 当前±1 + 省略号）；缺省保持首/上/下/末页文本按钮 */
  numeric?: boolean;
  /** 可选：「跳至 N 页」输入框；缺省不渲染 */
  jump?: boolean;
}

const props = withDefaults(defineProps<Props>(), {
  sizes: () => [],
  numeric: false,
  jump: false,
});

const emit = defineEmits<{
  /** 换页（已完成边界裁剪） */
  (e: 'update:page', page: number): void;
  /** 换每页条数（可选：仅 sizes 传入后由 chip 触发） */
  (e: 'update:pageSize', size: number): void;
}>();

/** 总页数（至少 1 页，避免出现 0/0）。 */
const totalPages = computed(() => Math.max(1, Math.ceil(props.total / props.pageSize)));

/** 换页（带边界裁剪，避免越界请求）。 */
function emitPage(next: number): void {
  emit('update:page', Math.min(Math.max(1, next), totalPages.value));
}

/** 切换每页条数（回传消费方；换页到第 1 页由消费方自行处理）。 */
function emitSize(size: number): void {
  if (size !== props.pageSize) {
    emit('update:pageSize', size);
  }
}

/** 数字页码序列：{1, 尾页, 当前±1} 去重排序，间隔 >1 处插入省略号（原型 pageSeq 逻辑）。 */
const pageSeq = computed<(number | '…')[]>(() => {
  const pages = totalPages.value;
  const cur = Math.min(Math.max(1, props.page), pages);
  const set = new Set<number>([1, pages, cur - 1, cur, cur + 1]);
  const list = [...set].filter((n) => n >= 1 && n <= pages).sort((a, b) => a - b);
  const out: (number | '…')[] = [];
  let prev = 0;
  for (const n of list) {
    if (n - prev > 1) {
      out.push('…');
    }
    out.push(n);
    prev = n;
  }
  return out;
});

/** 跳页：解析输入并走统一边界裁剪；非法输入由 number 输入兜底。 */
function onJump(e: Event): void {
  const el = e.target as HTMLInputElement;
  const n = Number.parseInt(el.value, 10);
  if (Number.isFinite(n)) {
    emitPage(n);
  }
}
</script>

<style scoped>
.uik-pager {
  display: flex;
  align-items: center;
  gap: 14px;
  flex-wrap: wrap;
  padding: 12px 16px;
  border-top: 1px solid var(--divider);
  font-size: var(--fs-caption);
  color: var(--text-3);
}
.uik-pager__info {
  white-space: nowrap;
}
.uik-pager__info b {
  color: var(--text-1);
  font-family: var(--font-mono);
  font-weight: 700;
}
/* 每页条数 chip 组（原型 .pg-sizes：靠右 + 5px 间距） */
.uik-pager__sizes {
  margin-left: auto;
  display: flex;
  align-items: center;
  gap: 5px;
  white-space: nowrap;
}
.uik-pager__sizes-label {
  color: var(--text-3);
}
.uik-pager__chip {
  font-family: var(--font-mono);
  font-size: var(--fs-caption);
  min-width: 26px;
  padding: 3px 9px;
  border: 1px solid var(--border);
  border-radius: var(--radius-sm);
  background: var(--bg-card);
  color: var(--text-2);
  cursor: pointer;
  transition: all 160ms cubic-bezier(0.16, 1, 0.3, 1);
}
.uik-pager__chip:hover {
  border-color: var(--brand);
  color: var(--brand-hover);
  background: var(--brand-subtle);
}
.uik-pager__chip.is-on {
  background: var(--brand);
  border-color: var(--brand);
  color: #fff;
  font-weight: 700;
}
.uik-pager__ops {
  display: flex;
  align-items: center;
  gap: 3px;
}
/* 无 chip 组时页码区保持靠右（兼容既有视觉位置） */
.uik-pager__ops.is-push {
  margin-left: auto;
}
.uik-pager__btn {
  font-family: inherit;
  font-size: var(--fs-caption);
  padding: 3px 10px;
  min-height: 26px;
  border-radius: var(--radius-sm);
  border: 1px solid var(--border);
  background: var(--bg-card);
  color: var(--text-1);
  cursor: pointer;
  white-space: nowrap;
  transition: all 160ms cubic-bezier(0.16, 1, 0.3, 1);
}
.uik-pager__btn:hover:not(:disabled) {
  border-color: var(--brand);
  color: var(--brand-hover);
  background: var(--brand-subtle);
}
.uik-pager__btn:disabled {
  opacity: 0.38;
  cursor: not-allowed;
}
/* 数字页码按钮（原型 .pg-btn：等宽字体 + 最小宽 28px） */
.uik-pager__btn--num {
  font-family: var(--font-mono);
  min-width: 28px;
  padding: 0 8px;
  color: var(--text-2);
}
.uik-pager__btn--num.is-on {
  background: var(--brand);
  border-color: var(--brand);
  color: #fff;
  font-weight: 700;
}
/* 省略号（原型 .pg-gap） */
.uik-pager__gap {
  color: var(--text-3);
  padding: 0 3px;
}
/* 跳页输入（原型 .pg-jump input：48px 等宽居中） */
.uik-pager__jump {
  display: flex;
  align-items: center;
  gap: 5px;
  white-space: nowrap;
}
.uik-pager__jump input {
  width: 48px;
  height: 26px;
  padding: 0 6px;
  text-align: center;
  font-family: var(--font-mono);
  font-size: var(--fs-caption);
  border: 1px solid var(--border);
  border-radius: var(--radius-sm);
  background: var(--bg-card);
  color: var(--text-1);
}
.uik-pager__jump input:focus-visible {
  outline: 2px solid var(--brand);
  outline-offset: 1px;
}
</style>

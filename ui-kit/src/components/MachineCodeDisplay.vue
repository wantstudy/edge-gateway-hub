<template>
  <!--
    MachineCodeDisplay —— 机器码展示。
    设计系统 §3：只读、禁止可编辑；分段显示 + 一键复制 + 来源分解（锚点列表）。
  -->
  <div class="uik-mcode">
    <div class="uik-mcode__row">
      <code class="uik-mcode__value">{{ displayValue }}</code>
      <button type="button" class="uik-mcode__copy" @click="copy">
        <span class="uik-mcode__copy-icon" v-html="COPY_ICON" />
        <span>{{ copied ? '已复制' : '复制' }}</span>
      </button>
    </div>
    <ul v-if="anchors.length" class="uik-mcode__anchors">
      <li v-for="anchor in anchors" :key="anchor">{{ anchor }}</li>
    </ul>
  </div>
</template>

<script setup lang="ts">
/**
 * @file MachineCodeDisplay.vue
 * @module ui-kit/components/MachineCodeDisplay
 * @description 机器码只读展示 + 复制反馈 + 锚点来源分解。
 */
import { computed, ref, onUnmounted } from 'vue';
import { formatMachineCode, maskMachineCode } from '../mask';
import { ICON_COPY } from '../icons';

interface Props {
  /** 原始机器码 */
  code: string | null | undefined;
  /** 是否掩码展示（列表页 true，详情页 false） */
  masked?: boolean;
  /** 锚点来源说明列表（如「宿主板 UUID」「宿主网卡 MAC」） */
  anchors?: readonly string[];
}

const props = withDefaults(defineProps<Props>(), {
  masked: false,
  anchors: () => [],
});

/** 展示值：掩码态 / 完整态。 */
const displayValue = computed(() =>
  props.masked ? maskMachineCode(props.code) : formatMachineCode(props.code),
);

/** 复制成功反馈（1.6s 后自动复原，避免「已复制」长期残留造成误读）。 */
const copied = ref(false);
let timer: ReturnType<typeof setTimeout> | null = null;

/**
 * 复制机器码到剪贴板。
 * 优先用 Clipboard API；不可用时降级为 textarea + execCommand（HTTP 环境兜底）。
 */
async function copy(): Promise<void> {
  const text = formatMachineCode(props.code);
  try {
    if (navigator.clipboard && window.isSecureContext) {
      await navigator.clipboard.writeText(text);
    } else {
      const el = document.createElement('textarea');
      el.value = text;
      el.style.position = 'fixed';
      el.style.opacity = '0';
      document.body.appendChild(el);
      el.select();
      document.execCommand('copy');
      document.body.removeChild(el);
    }
    copied.value = true;
    if (timer) {
      clearTimeout(timer);
    }
    timer = setTimeout(() => {
      copied.value = false;
    }, 1600);
  } catch {
    copied.value = false;
  }
}

/** 卸载时清理定时器，避免内存泄漏与卸载后 setState。 */
onUnmounted(() => {
  if (timer) {
    clearTimeout(timer);
  }
});

/** 复制图标（内联 SVG）。 */
const COPY_ICON = ICON_COPY;
</script>

<style scoped>
.uik-mcode {
  display: flex;
  flex-direction: column;
  gap: 6px;
}
.uik-mcode__row {
  display: flex;
  align-items: center;
  gap: 8px;
  flex-wrap: wrap;
}
.uik-mcode__value {
  font-family: var(--font-mono);
  font-variant-numeric: tabular-nums;
  font-size: var(--fs-table);
  color: var(--text-1);
  background: var(--bg-app);
  border: 1px solid var(--border);
  border-radius: var(--radius-sm);
  padding: 3px 8px;
  user-select: all;
}
.uik-mcode__copy {
  display: inline-flex;
  align-items: center;
  gap: 4px;
  font-family: inherit;
  font-size: var(--fs-caption);
  color: var(--text-2);
  background: #fff;
  border: 1px solid var(--border);
  border-radius: var(--radius-sm);
  padding: 3px 8px;
  min-height: 26px;
  cursor: pointer;
}
.uik-mcode__copy:hover {
  border-color: var(--brand);
  color: var(--brand);
}
.uik-mcode__copy-icon {
  display: inline-flex;
}
.uik-mcode__anchors {
  margin: 0;
  padding-left: 18px;
  font-size: var(--fs-caption);
  color: var(--text-3);
  line-height: 1.7;
}
</style>

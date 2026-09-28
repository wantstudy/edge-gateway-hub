<template>
  <!--
    MaskedCode —— 激活码掩码显示 + 显式揭示（reveal）交互。
    设计系统 §6 验收要点：「激活码明文默认掩码，查看需权限且记审计」。
    因此组件本身不判断权限——`canReveal` 由调用方传入，揭示动作通过 `reveal` 事件上报给页面去落审计。
  -->
  <span class="uik-maskcode">
    <code class="uik-maskcode__value">{{ revealed ? displayRaw : maskedValue }}</code>
    <button
      type="button"
      class="uik-maskcode__btn"
      :title="canReveal ? '查看明文（将记录审计）' : '无查看明文权限'"
      :disabled="!canReveal"
      @click="toggle"
    >
      {{ revealed ? '隐藏' : '显示明文' }}
    </button>
    <span v-if="!canReveal" class="uik-maskcode__tip">无权限</span>
  </span>
</template>

<script setup lang="ts">
/**
 * @file MaskedCode.vue
 * @module ui-kit/components/MaskedCode
 * @description 激活码掩码 / 揭示切换。真实交互（非两套静态文案）。
 */
import { computed, ref } from 'vue';
import { maskCode } from '../mask';

interface Props {
  /** 激活码原文 */
  code: string | null | undefined;
  /** 当前角色是否有权查看明文（页面从 RBAC 推导） */
  canReveal?: boolean;
  /** 是否默认已揭示（详情页可由「已授权揭示」的状态驱动） */
  defaultRevealed?: boolean;
}

const props = withDefaults(defineProps<Props>(), {
  canReveal: false,
  defaultRevealed: false,
});

const emit = defineEmits<{
  /** 揭示动作（仅从隐藏 → 显示时触发一次），调用方据此落审计 */
  (e: 'reveal'): void;
}>();

/** 揭示状态。 */
const revealed = ref(props.defaultRevealed);

/** 明文（原文，缺失显示 —）。 */
const displayRaw = computed(() => props.code || '—');

/** 掩码值。 */
const maskedValue = computed(() => maskCode(props.code));

/** 切换揭示：无权时不响应；首次揭示上报 reveal 事件。 */
function toggle(): void {
  if (!props.canReveal) {
    return;
  }
  revealed.value = !revealed.value;
  if (revealed.value) {
    emit('reveal');
  }
}
</script>

<style scoped>
.uik-maskcode {
  display: inline-flex;
  align-items: center;
  gap: 6px;
  flex-wrap: wrap;
}
.uik-maskcode__value {
  font-family: var(--font-mono);
  font-variant-numeric: tabular-nums;
  font-size: var(--fs-table);
  color: var(--text-1);
}
.uik-maskcode__btn {
  font-family: inherit;
  font-size: var(--fs-caption);
  color: var(--brand);
  background: none;
  border: 0;
  padding: 0 2px;
  cursor: pointer;
}
.uik-maskcode__btn:disabled {
  color: var(--text-3);
  cursor: not-allowed;
}
.uik-maskcode__tip {
  font-size: 11px;
  color: var(--text-3);
}
</style>

<template>
  <!--
    UiTextarea —— 统一多行文本框。
    用于「补充说明（必填 ≥10 字）」「备注」等审计字段；实时回显字数便于用户判断是否达标。
  -->
  <div class="uik-textarea-wrap">
    <textarea
      class="uik-textarea"
      :class="{ 'uik-textarea--invalid': invalid }"
      :value="modelValue"
      :placeholder="placeholder"
      :rows="rows"
      :disabled="disabled"
      :aria-invalid="invalid"
      @input="onInput"
    />
    <span v-if="showCounter" class="uik-textarea__counter" :class="{ 'is-short': modelValue.length < minLength }">
      {{ modelValue.length }} / {{ minLength }} 字
    </span>
  </div>
</template>

<script setup lang="ts">
/**
 * @file UiTextarea.vue
 * @module ui-kit/components/UiTextarea
 * @description 多行文本框 + 字数计数器。
 */

interface Props {
  /** v-model 值 */
  modelValue: string;
  /** 占位符 */
  placeholder?: string;
  /** 行数 */
  rows?: number;
  /** 是否禁用 */
  disabled?: boolean;
  /** 是否错误态 */
  invalid?: boolean;
  /** 是否显示字数计数器 */
  showCounter?: boolean;
  /** 期望最小字数（仅用于计数器的达标提示，真正校验在业务层） */
  minLength?: number;
}

withDefaults(defineProps<Props>(), {
  placeholder: '',
  rows: 3,
  disabled: false,
  invalid: false,
  showCounter: false,
  minLength: 0,
});

const emit = defineEmits<{
  /** 值变更 */
  (e: 'update:modelValue', value: string): void;
}>();

/** 输入事件：冒泡原始文本。 */
function onInput(event: Event): void {
  const target = event.target as HTMLTextAreaElement;
  emit('update:modelValue', target.value);
}
</script>

<style scoped>
.uik-textarea-wrap {
  display: flex;
  flex-direction: column;
  gap: 4px;
}
.uik-textarea {
  width: 100%;
  box-sizing: border-box;
  font-family: inherit;
  font-size: var(--fs-table);
  line-height: 1.6;
  color: var(--text-1);
  background: #fff;
  border: 1px solid var(--border);
  border-radius: var(--radius-sm);
  padding: 6px 8px;
  outline: none;
  resize: vertical;
}
.uik-textarea::placeholder {
  color: var(--text-3);
}
.uik-textarea:focus {
  border-color: var(--brand);
  box-shadow: 0 0 0 2px var(--brand-subtle);
}
.uik-textarea--invalid {
  border-color: var(--danger);
}
.uik-textarea:disabled {
  background: var(--divider);
  color: var(--text-3);
  cursor: not-allowed;
}
.uik-textarea__counter {
  align-self: flex-end;
  font-size: 11px;
  color: var(--text-3);
  font-variant-numeric: tabular-nums;
}
.uik-textarea__counter.is-short {
  color: var(--warn);
}
</style>

<template>
  <!--
    UiSelect —— 统一下拉选择控件。
    选项以 value/label 对传入；若选项 value 为空字符串，则视为「全部」占位项。
  -->
  <select
    class="uik-select"
    :value="modelValue"
    :disabled="disabled"
    @change="onChange"
  >
    <option v-for="opt in options" :key="opt.value" :value="opt.value">
      {{ opt.label }}
    </option>
  </select>
</template>

<script setup lang="ts">
/**
 * @file UiSelect.vue
 * @module ui-kit/components/UiSelect
 * @description 下拉选择框。用于筛选栏与表单；筛选栏的「全部」用 value = '' 表示。
 */

/** 选项结构。 */
export interface SelectOption {
  /** 提交值（'' 表示「全部」，业务层负责翻译为不传该参数） */
  value: string;
  /** 展示文案 */
  label: string;
}

interface Props {
  /** v-model 值 */
  modelValue: string;
  /** 选项列表 */
  options: readonly SelectOption[];
  /** 是否禁用 */
  disabled?: boolean;
}

withDefaults(defineProps<Props>(), {
  disabled: false,
});

const emit = defineEmits<{
  /** 值变更 */
  (e: 'update:modelValue', value: string): void;
}>();

/** 变更事件：把选中的字符串值冒泡。 */
function onChange(event: Event): void {
  const target = event.target as HTMLSelectElement;
  emit('update:modelValue', target.value);
}
</script>

<style scoped>
.uik-select {
  width: 100%;
  box-sizing: border-box;
  font-family: inherit;
  font-size: var(--fs-table);
  color: var(--text-1);
  background: #fff;
  border: 1px solid var(--border);
  border-radius: var(--radius-sm);
  padding: 5px 8px;
  min-height: 32px;
  outline: none;
  cursor: pointer;
}
.uik-select:focus {
  border-color: var(--brand);
  box-shadow: 0 0 0 2px var(--brand-subtle);
}
.uik-select:disabled {
  background: var(--divider);
  color: var(--text-3);
  cursor: not-allowed;
}
</style>

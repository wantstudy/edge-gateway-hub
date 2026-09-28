<template>
  <!--
    UiInput —— 统一文本输入控件（原生 input 包装，非受控不受理）。
    必须保持 `v-model` 双向绑定，表单草稿一律由调用方持有，组件自身不缓存状态。
  -->
  <input
    class="uik-input"
    :class="{ 'uik-input--invalid': invalid }"
    :type="type"
    :value="modelValue"
    :placeholder="placeholder"
    :disabled="disabled"
    :maxlength="maxlength"
    :aria-invalid="invalid"
    @input="onInput"
  />
</template>

<script setup lang="ts">
/**
 * @file UiInput.vue
 * @module ui-kit/components/UiInput
 * @description 文本输入框（支持 v-model、错误态高亮、禁用态）。
 */

interface Props {
  /** v-model 值 */
  modelValue: string;
  /** 输入类型 */
  type?: 'text' | 'password' | 'number';
  /** 占位符 */
  placeholder?: string;
  /** 是否禁用 */
  disabled?: boolean;
  /** 是否为错误态（红框） */
  invalid?: boolean;
  /** 最大长度 */
  maxlength?: number;
}

const props = withDefaults(defineProps<Props>(), {
  type: 'text',
  placeholder: '',
  disabled: false,
  invalid: false,
  maxlength: undefined,
});

const emit = defineEmits<{
  /** 值变更（每次输入实时冒泡，便于弹窗实时校验禁用态） */
  (e: 'update:modelValue', value: string): void;
}>();

/** 输入事件：把原生值原样冒泡给父组件（父组件持有草稿）。 */
function onInput(event: Event): void {
  const target = event.target as HTMLInputElement;
  emit('update:modelValue', target.value);
}

/** 显式引用以通过 `noUnusedLocals`（props 在模板中已被使用）。 */
void props;
</script>

<style scoped>
.uik-input {
  width: 100%;
  box-sizing: border-box;
  font-family: inherit;
  font-size: var(--fs-table);
  color: var(--text-1);
  background: var(--control-bg);
  border: 1px solid var(--border);
  border-radius: var(--radius-sm);
  padding: 5px 8px;
  min-height: 32px;
  outline: none;
}
.uik-input::placeholder {
  color: var(--text-3);
}
.uik-input:focus {
  border-color: var(--brand);
  box-shadow: 0 0 0 2px var(--brand-subtle);
}
.uik-input--invalid {
  border-color: var(--danger);
}
.uik-input:disabled {
  background: var(--divider);
  color: var(--text-3);
  cursor: not-allowed;
}
</style>

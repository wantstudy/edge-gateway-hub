<template>
  <!--
    UiSwitch —— 统一开关控件。
    用于「双人复核」「强制回执」等布尔策略；提交按钮文案按状态取反由调用方决定。
  -->
  <button
    type="button"
    class="uik-switch"
    :class="{ 'is-on': modelValue, 'is-disabled': disabled }"
    role="switch"
    :aria-checked="modelValue"
    :disabled="disabled"
    @click="toggle"
  >
    <span class="uik-switch__thumb" aria-hidden="true" />
    <span class="uik-switch__text">{{ modelValue ? onText : offText }}</span>
  </button>
</template>

<script setup lang="ts">
/**
 * @file UiSwitch.vue
 * @module ui-kit/components/UiSwitch
 * @description 开关（布尔 v-model）。文案随状态切换，符合设计系统 §4.2「可解释」要求。
 */

interface Props {
  /** v-model 布尔值 */
  modelValue: boolean;
  /** 开启时文案 */
  onText?: string;
  /** 关闭时文案 */
  offText?: string;
  /** 是否禁用 */
  disabled?: boolean;
}

const props = withDefaults(defineProps<Props>(), {
  onText: '已启用',
  offText: '已关闭',
  disabled: false,
});

const emit = defineEmits<{
  /** 值变更 */
  (e: 'update:modelValue', value: boolean): void;
}>();

/** 切换：禁用态不响应。 */
function toggle(): void {
  if (props.disabled) {
    return;
  }
  emit('update:modelValue', !props.modelValue);
}
</script>

<style scoped>
.uik-switch {
  display: inline-flex;
  align-items: center;
  gap: 8px;
  background: none;
  border: 0;
  padding: 0;
  min-height: 32px;
  font-family: inherit;
  cursor: pointer;
}
.uik-switch.is-disabled {
  cursor: not-allowed;
  opacity: 0.55;
}
.uik-switch__thumb {
  position: relative;
  width: 34px;
  height: 18px;
  flex: 0 0 34px;
  border-radius: var(--radius-pill);
  background: var(--control-border-hover);
  transition: background 0.16s ease;
}
.uik-switch__thumb::after {
  content: '';
  position: absolute;
  top: 2px;
  left: 2px;
  width: 14px;
  height: 14px;
  border-radius: 50%;
  background: #fff;
  transition: left 0.16s ease;
}
.uik-switch.is-on .uik-switch__thumb {
  background: var(--brand);
}
.uik-switch.is-on .uik-switch__thumb::after {
  left: 18px;
}
.uik-switch__text {
  font-size: var(--fs-caption);
  color: var(--text-3);
}
</style>

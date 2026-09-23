<template>
  <!--
    UiRadio —— 统一单选组（卡片式）。
    用于「预绑定新机器码 / 留待首次激活绑定」「允许离线运行」等关键二选一，
    卡片式让选项的**后果说明**直接可见（设计系统 §4.1 可解释性）。
  -->
  <div class="uik-radio-row" role="radiogroup">
    <label
      v-for="opt in options"
      :key="opt.value"
      class="uik-radio"
      :class="{ 'is-on': opt.value === modelValue, 'is-disabled': disabled }"
    >
      <input
        class="uik-sr-only"
        type="radio"
        :name="groupName"
        :value="opt.value"
        :checked="opt.value === modelValue"
        :disabled="disabled"
        @change="choose(opt.value)"
      />
      <span class="uik-radio__dot" aria-hidden="true" />
      <span class="uik-radio__body">
        <span class="uik-radio__title">{{ opt.label }}</span>
        <span v-if="opt.desc" class="uik-radio__desc">{{ opt.desc }}</span>
      </span>
    </label>
  </div>
</template>

<script setup lang="ts">
/**
 * @file UiRadio.vue
 * @module ui-kit/components/UiRadio
 * @description 卡片式单选组。
 */
import { useId } from 'vue';

/** 单选项。 */
export interface RadioOption {
  /** 取值 */
  value: string;
  /** 标题 */
  label: string;
  /** 后果 / 说明（强烈建议填写，用于「可解释」） */
  desc?: string;
}

interface Props {
  /** v-model 值 */
  modelValue: string;
  /** 选项 */
  options: readonly RadioOption[];
  /** 是否禁用（整体） */
  disabled?: boolean;
}

const props = withDefaults(defineProps<Props>(), {
  disabled: false,
});

const emit = defineEmits<{
  /** 值变更 */
  (e: 'update:modelValue', value: string): void;
}>();

/** 同一实例内所有 radio 共享 name，保证原生键盘可达与互斥。 */
const groupName = `uik-radio-${useId()}`;

/** 选中某项。 */
function choose(value: string): void {
  if (props.disabled) {
    return;
  }
  emit('update:modelValue', value);
}
</script>

<style scoped>
.uik-radio-row {
  display: flex;
  flex-direction: column;
  gap: 8px;
}
.uik-radio {
  display: flex;
  gap: 10px;
  align-items: flex-start;
  border: 1px solid var(--border);
  border-radius: var(--radius-sm);
  padding: 10px 12px;
  cursor: pointer;
}
.uik-radio:hover {
  border-color: #c9cdd4;
  background: var(--bg-hover);
}
.uik-radio.is-on {
  border-color: var(--brand);
  background: var(--brand-subtle);
}
.uik-radio.is-disabled {
  cursor: not-allowed;
  opacity: 0.6;
}
.uik-radio__dot {
  width: 14px;
  height: 14px;
  flex: 0 0 14px;
  margin-top: 3px;
  border-radius: 50%;
  border: 1px solid #c9cdd4;
  background: #fff;
  box-sizing: border-box;
}
.uik-radio.is-on .uik-radio__dot {
  border: 4px solid var(--brand);
}
.uik-radio__body {
  display: flex;
  flex-direction: column;
  gap: 2px;
}
.uik-radio__title {
  font-size: var(--fs-table);
  font-weight: 500;
  color: var(--text-1);
}
.uik-radio__desc {
  font-size: var(--fs-caption);
  color: var(--text-3);
  line-height: 1.5;
}
.uik-sr-only {
  position: absolute;
  width: 1px;
  height: 1px;
  overflow: hidden;
  clip: rect(0 0 0 0);
}
</style>

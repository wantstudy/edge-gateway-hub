<template>
  <!--
    UiField —— 表单字段壳（标签 + 必填星号 + 控件槽 + 提示/错误信息）。
    统一两端表单的标签排版、必填标记与「字段旁行内错误」语义（设计系统 §4.1）。
  -->
  <div class="uik-field" :class="{ 'uik-field--full': full }">
    <label class="uik-field__label" :for="inputId">
      {{ label }}<span v-if="required" class="uik-field__req" aria-hidden="true">*</span>
      <span v-if="required" class="uik-sr-only">（必填）</span>
    </label>
    <div class="uik-field__control">
      <slot />
    </div>
    <!-- 错误优先于提示：错误态必须说明「发生了什么 + 怎么解决」 -->
    <p v-if="error" class="uik-field__error" role="alert">{{ error }}</p>
    <p v-else-if="hint" class="uik-field__hint">{{ hint }}</p>
  </div>
</template>

<script setup lang="ts">
/**
 * @file UiField.vue
 * @module ui-kit/components/UiField
 * @description 表单字段外壳组件（纯展示，无业务逻辑）。
 */
import { useId } from 'vue';

interface Props {
  /** 字段标签 */
  label: string;
  /** 是否必填（显示红色星号 + 无障碍文本） */
  required?: boolean;
  /** 底部提示文案 */
  hint?: string;
  /** 行内错误文案（非空时覆盖 hint） */
  error?: string;
  /** 是否占满两列栅格 */
  full?: boolean;
}

withDefaults(defineProps<Props>(), {
  required: false,
  hint: '',
  error: '',
  full: false,
});

/** 生成稳定 id，供 label[for] 关联到控件。 */
const inputId = `uik-field-${useId()}`;
</script>

<style scoped>
.uik-field {
  display: flex;
  flex-direction: column;
  gap: 5px;
  min-width: 0;
}
.uik-field--full {
  grid-column: 1 / -1;
}
.uik-field__label {
  font-size: var(--fs-caption);
  color: var(--text-2);
}
.uik-field__req {
  color: var(--danger);
  margin-left: 2px;
}
.uik-field__hint {
  margin: 0;
  font-size: 11px;
  line-height: 1.5;
  color: var(--text-3);
}
.uik-field__error {
  margin: 0;
  font-size: 11px;
  line-height: 1.5;
  color: var(--danger);
}
.uik-sr-only {
  position: absolute;
  width: 1px;
  height: 1px;
  padding: 0;
  margin: -1px;
  overflow: hidden;
  clip: rect(0 0 0 0);
  white-space: nowrap;
  border: 0;
}
</style>

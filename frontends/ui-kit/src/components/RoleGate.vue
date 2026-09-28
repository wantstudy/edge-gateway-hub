<template>
  <!--
    RoleGate —— 角色可见性控制（菜单 / 按钮）。
    设计系统 §3 硬约束：**仅控制可见性与禁用态，不承担授权判定**（判定在 Rust 侧与后端）。
    `mode="hide"` 时无权限直接不渲染；`mode="disable"` 时渲染但禁用并给出原因提示，
    便于用户理解「此功能存在但我的角色无权使用」。
  -->
  <slot v-if="allowed" />
  <template v-else-if="mode === 'disable'">
    <span class="uik-gate" :title="denyText">
      <slot name="denied">
        <button type="button" class="uik-gate__btn" disabled>{{ fallbackLabel }}</button>
      </slot>
    </span>
  </template>
</template>

<script setup lang="ts">
/**
 * @file RoleGate.vue
 * @module ui-kit/components/RoleGate
 * @description 角色门控包装组件。可见性控制，非授权判定。
 */
interface Props {
  /** 是否有权限 */
  allowed: boolean;
  /** 无权限时的表现：hide 不渲染 / disable 渲染为禁用态 */
  mode?: 'hide' | 'disable';
  /** 无权限时的悬停原因文案 */
  denyText?: string;
  /** disable 模式下的按钮兜底文案 */
  fallbackLabel?: string;
}

withDefaults(defineProps<Props>(), {
  mode: 'hide',
  denyText: '当前角色无权执行此操作',
  fallbackLabel: '无权操作',
});
</script>

<style scoped>
.uik-gate {
  display: inline-flex;
  cursor: not-allowed;
}
.uik-gate__btn {
  font-family: inherit;
  font-size: var(--fs-table);
  min-height: 32px;
  padding: 6px 14px;
  border-radius: var(--radius-sm);
  border: 1px solid var(--border);
  background: var(--divider);
  color: var(--text-3);
  cursor: not-allowed;
}
</style>

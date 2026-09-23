<template>
  <!--
    UiTable —— 统一数据表格（紧凑为主，行高 40px）。
    泛型组件：列定义 + 具名插槽 `cell-<key>` 自定义单元格渲染。
    数值列自动加 tabular-nums，保证实时监控表不抖动（设计系统 §2.3）。
  -->
  <div class="uik-table-wrap">
    <table class="uik-table">
      <thead>
        <tr>
          <th v-for="col in columns" :key="col.key" :class="{ 'is-right': col.align === 'right' }">
            {{ col.label }}
          </th>
          <th v-if="$slots.actions" class="is-right">操作</th>
        </tr>
      </thead>
      <tbody>
        <tr v-for="(row, index) in rows" :key="rowKey(row, index)" :class="rowClass(row)">
          <td
            v-for="col in columns"
            :key="col.key"
            :class="{ 'is-right': col.align === 'right', 'uik-mono': col.mono }"
          >
            <!-- 优先使用具名插槽自定义渲染，否则直接输出字段值 -->
            <slot v-if="$slots[`cell-${col.key}`]" :name="`cell-${col.key}`" :row="row" :value="row[col.key]" />
            <template v-else>{{ display(row[col.key]) }}</template>
          </td>
          <td v-if="$slots.actions" class="is-right">
            <div class="uik-table__actions">
              <slot name="actions" :row="row" :index="index" />
            </div>
          </td>
        </tr>
      </tbody>
    </table>
    <div v-if="footer" class="uik-table__footer">{{ footer }}</div>
  </div>
</template>

<script setup lang="ts" generic="T extends Record<string, unknown>">
/**
 * @file UiTable.vue
 * @module ui-kit/components/UiTable
 * @description 泛型表格。行高紧凑、表头浅底、支持右侧操作列插槽与底部说明。
 */

/** 列定义。 */
export interface TableColumn {
  /** 取值字段名 */
  key: string;
  /** 表头文案 */
  label: string;
  /** 对齐方式 */
  align?: 'left' | 'right';
  /** 是否使用等宽数字字体 */
  mono?: boolean;
}

interface Props {
  /** 列定义 */
  columns: readonly TableColumn[];
  /** 行数据 */
  rows: readonly T[];
  /** 行 key 字段名（默认 `id`，缺省时回退到行下标） */
  rowKeyField?: string;
  /** 底部说明文案（分页 / 口径提示） */
  footer?: string;
}

const props = withDefaults(defineProps<Props>(), {
  rowKeyField: 'id',
  footer: '',
});

/**
 * 计算行 key：优先取 rowKeyField，缺失时回退下标（保证渲染稳定）。
 */
function rowKey(row: T, index: number): string {
  const value = row[props.rowKeyField];
  return value === undefined || value === null ? `row-${index}` : String(value);
}

/**
 * 行附加样式类：业务数据若带 `_rowClass`（如 row-warn / row-danger）则透传，
 * 用于「异常行左侧色条」这类高优先级视觉信号。
 */
function rowClass(row: T): string {
  const cls = row._rowClass;
  return typeof cls === 'string' ? cls : '';
}

/** 单元格兜底显示：null/undefined 统一显示为 `—`，避免出现空白或 "undefined"。 */
function display(value: unknown): string {
  if (value === null || value === undefined || value === '') {
    return '—';
  }
  return String(value);
}
</script>

<style scoped>
.uik-table-wrap {
  overflow-x: auto;
}
.uik-table {
  width: 100%;
  border-collapse: collapse;
  font-size: var(--fs-table);
}
.uik-table th {
  text-align: left;
  font-weight: 500;
  color: var(--text-2);
  background: #fafbfc;
  padding: 9px 12px;
  border-bottom: 1px solid var(--border);
  white-space: nowrap;
  font-size: var(--fs-caption);
}
.uik-table td {
  padding: 9px 12px;
  height: 40px;
  border-bottom: 1px solid var(--divider);
  color: var(--text-1);
  vertical-align: middle;
}
.uik-table tbody tr:hover {
  background: var(--bg-hover);
}
.uik-table tbody tr:last-child td {
  border-bottom: 0;
}
.uik-table .is-right,
.uik-table th.is-right {
  text-align: right;
}
.uik-table .uik-mono {
  font-family: var(--font-mono);
  font-variant-numeric: tabular-nums;
}
.uik-table__actions {
  display: inline-flex;
  gap: 6px;
  justify-content: flex-end;
  flex-wrap: wrap;
}
.uik-table__footer {
  padding: 10px 12px;
  font-size: var(--fs-caption);
  color: var(--text-3);
  border-top: 1px solid var(--divider);
}
/* 异常行左侧色条 —— 与 StatusTag 配合形成「色 + 形 + 文」三重信号 */
.uik-table :deep(tr.row-warn) td:first-child {
  box-shadow: inset 3px 0 0 var(--warn);
}
.uik-table :deep(tr.row-danger) td:first-child {
  box-shadow: inset 3px 0 0 var(--danger);
}
</style>

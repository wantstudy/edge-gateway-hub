/**
 * @file index.ts
 * @module ui-kit
 * @description ui-kit 公共出口。两端应用**只能**从这里导入 token 与业务组件，
 *              以确保视觉与交互不漂移（`docs/design/ui-design-system.md` §1）。
 *
 * 导入顺序约定：调用方先 `import '@ui-kit/tokens.css'`，再使用组件。
 */

// ---------- 设计 token（CSS 变量）----------
import './tokens.css';

// ---------- token 的 TS 常量（供图表 / 内联样式引用）----------
export { BRAND, NEUTRAL, SEMANTIC, SEMANTIC_SURFACE, SCALE } from './tokens';
export type { Tone } from './tokens';

// ---------- 明暗双主题运行时（唯一状态源，两端共用）----------
export {
  themeMode,
  setThemeMode,
  toggleThemeMode,
  applyTheme,
  THEME_STORAGE_KEY,
  DARK_QUERY,
} from './theme';
export type { ThemeMode } from './theme';

// ---------- 纯函数工具（可单测，无 Vue 依赖）----------
export { STATUS_MAP, statusView } from './status-map';
export type { StatusView } from './status-map';
export {
  maskCode,
  codeTail8,
  maskMachineSummary,
  maskMachineCode,
  formatMachineCode,
  normalizeMachineCodeInput,
  maskIp,
  isValidMachineCode,
  formatDateTime,
  relativeTime,
  CODE_PATTERN,
  MACHINE_CODE_PATTERN,
} from './mask';

// ---------- 时间格式化（唯一出口；页面严禁裸显 epoch）----------
export { parseTime, formatDate, formatMonthDay, formatClock, formatRelative, TIME_PLACEHOLDER } from './time';

// ---------- RBAC（页面级 + 操作级矩阵）----------
export {
  ROLES,
  ROLE_META,
  PAGES,
  ACTION_MATRIX,
  canSeePage,
  can,
  firstAllowedPage,
} from './rbac';
export type { Role, PageId, Action, RoleMeta, PageMeta } from './rbac';

// ---------- 图标 ----------
export * from './icons';

// ---------- 业务组件 ----------
export { default as StatusTag } from './components/StatusTag.vue';
export { default as StatCard } from './components/StatCard.vue';
export { default as PageHeader } from './components/PageHeader.vue';
export { default as EmptyState } from './components/EmptyState.vue';
export { default as MachineCodeDisplay } from './components/MachineCodeDisplay.vue';
export { default as MaskedCode } from './components/MaskedCode.vue';
export { default as CodeLifecycleTimeline } from './components/CodeLifecycleTimeline.vue';
export { default as DangerConfirmModal } from './components/DangerConfirmModal.vue';
export type { DangerFact } from './components/DangerConfirmModal.vue';
export type { TimelineNode } from './components/CodeLifecycleTimeline.vue';
export { default as RoleGate } from './components/RoleGate.vue';

// ---------- 表单原语 ----------
export { default as UiField } from './components/UiField.vue';
export { default as UiInput } from './components/UiInput.vue';
export { default as UiSelect } from './components/UiSelect.vue';
export type { SelectOption } from './components/UiSelect.vue';
export { default as UiTextarea } from './components/UiTextarea.vue';
export { default as UiSwitch } from './components/UiSwitch.vue';
export { default as UiRadio } from './components/UiRadio.vue';
export type { RadioOption } from './components/UiRadio.vue';
export { default as UiTable } from './components/UiTable.vue';
export type { TableColumn } from './components/UiTable.vue';

// ---------- 应用级复合组件（分页条等）----------
export { default as UiPager } from './components/UiPager.vue';

/**
 * @file icons.ts
 * @module ui-kit/icons
 * @description ui-kit 自带的极简内联图标集（内联 SVG，零外部资源）。
 *
 * 设计系统 §2.1 要求「颜色不是唯一信号：每个状态色必须同时配图标或文案」。
 * 这些图标就是与状态色配对的可辨识形状，色盲 / 车间强光下仍可区分。
 *
 * 统一约定：`viewBox="0 0 16 16"`、`fill="none"`、`stroke="currentColor"`、`stroke-width="1.5"`，
 * 因此颜色由外层 CSS `color` 控制，可随状态色变化。
 */

/** 图标基础属性（供各图标组件复用）。 */
export const ICON_BASE_PROPS = {
  viewBox: '0 0 16 16',
  width: 14,
  height: 14,
  fill: 'none',
  stroke: 'currentColor',
  'stroke-width': 1.5,
  'stroke-linecap': 'round',
  'stroke-linejoin': 'round',
  'aria-hidden': 'true',
} as const;

/** 对勾（正常 / 成功 / 在线）。 */
export const ICON_CHECK = `
  <svg viewBox="0 0 16 16" width="14" height="14" fill="none" stroke="currentColor" stroke-width="1.5"
       stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">
    <path d="M3 8.5 L6.5 12 L13 4.5" />
  </svg>`;

/** 感叹号三角（告警 / 宽限期 / 试用）。 */
export const ICON_WARN = `
  <svg viewBox="0 0 16 16" width="14" height="14" fill="none" stroke="currentColor" stroke-width="1.5"
       stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">
    <path d="M8 2 L15 14 H1 Z" />
    <path d="M8 6.5 V9.5" />
    <circle cx="8" cy="11.8" r="0.6" fill="currentColor" stroke="none" />
  </svg>`;

/** 叉号圆（故障 / 危险 / 离线）。 */
export const ICON_DANGER = `
  <svg viewBox="0 0 16 16" width="14" height="14" fill="none" stroke="currentColor" stroke-width="1.5"
       stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">
    <circle cx="8" cy="8" r="6.2" />
    <path d="M5.6 5.6 L10.4 10.4 M10.4 5.6 L5.6 10.4" />
  </svg>`;

/** 信息圆（info）。 */
export const ICON_INFO = `
  <svg viewBox="0 0 16 16" width="14" height="14" fill="none" stroke="currentColor" stroke-width="1.5"
       stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">
    <circle cx="8" cy="8" r="6.2" />
    <path d="M8 7.2 V11" />
    <circle cx="8" cy="5" r="0.6" fill="currentColor" stroke="none" />
  </svg>`;

/** 圆形（未知 / 未激活 / 已停用）。 */
export const ICON_DOT = `
  <svg viewBox="0 0 16 16" width="14" height="14" fill="none" stroke="currentColor" stroke-width="1.5"
       stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">
    <circle cx="8" cy="8" r="4" stroke-dasharray="2 2" />
  </svg>`;

/** 复制（一键复制机器码 / 激活码）。 */
export const ICON_COPY = `
  <svg viewBox="0 0 16 16" width="14" height="14" fill="none" stroke="currentColor" stroke-width="1.5"
       stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">
    <rect x="5.5" y="5.5" width="8" height="8" rx="1.5" />
    <path d="M10.5 3.5 H3.5 A1.5 1.5 0 0 0 2 5 V12" />
  </svg>`;

/** 空态插画（列表为空时使用，避免「白板」观感）。 */
export const ICON_EMPTY = `
  <svg viewBox="0 0 64 48" width="64" height="48" fill="none" stroke="currentColor" stroke-width="1.5"
       stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">
    <rect x="8" y="10" width="48" height="30" rx="4" stroke-dasharray="4 3" />
    <path d="M8 20 H56" />
    <path d="M20 28 H30 M36 28 H44" />
  </svg>`;

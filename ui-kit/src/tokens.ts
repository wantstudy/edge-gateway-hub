/**
 * @file tokens.ts
 * @module ui-kit/tokens
 * @description 设计 token 的 TypeScript 单一事实源（Single Source of Truth）。
 *
 * 权威来源：`docs/design/ui-design-system.md` §2。
 * `tokens.css` 里的 CSS 变量值必须与本文件保持一致（测试用例 `tests/tokens.spec.ts` 会校验）。
 *
 * 之所以同时提供 TS 常量：图表、内联样式、SVG 填充等场景无法直接读取 CSS 变量，
 * 需要一个可被 JS 引用的稳定取值来源，避免各处硬编码颜色字符串。
 */

/** 品牌色（主按钮 / 选中态 / 链接）。 */
export const BRAND = {
  /** 主色 */
  brand: '#1F6FEB',
  /** 悬停 */
  brandHover: '#3B82F6',
  /** 按下 */
  brandActive: '#1A5FD0',
  /** 选中行底色 / 标签底 */
  brandSubtle: '#E8F1FE',
} as const;

/** 中性色（文本 / 边框 / 背景）。 */
export const NEUTRAL = {
  text1: '#1D2129',
  text2: '#4E5969',
  text3: '#86909C',
  border: '#E5E6EB',
  divider: '#F2F3F5',
  bgApp: '#F7F8FA',
  bgCard: '#FFFFFF',
  bgHover: '#F7F8FA',
} as const;

/** 语义状态色（工业设备语境）。 */
export const SEMANTIC = {
  ok: '#00A870',
  warn: '#FF7D00',
  danger: '#F53F3F',
  info: '#1F6FEB',
  unknown: '#86909C',
} as const;

/** 状态标签的浅色底 + 深色文字组合（对比度 ≥ 4.5:1，见设计系统 §6）。 */
export const SEMANTIC_SURFACE = {
  ok: { bg: '#E8FFEA', fg: '#0A6B47', border: '#B5F0CE' },
  warn: { bg: '#FFF7E8', fg: '#8A4B00', border: '#FFE0B2' },
  danger: { bg: '#FFECE8', fg: '#A8171B', border: '#FFCDC5' },
  info: { bg: '#E8F1FE', fg: '#12489C', border: '#C6DDFB' },
  unknown: { bg: '#F2F3F5', fg: '#5A6069', border: '#E5E6EB' },
} as const;

/** 语义色调枚举。 */
export type Tone = keyof typeof SEMANTIC_SURFACE;

/** 尺度 token。 */
export const SCALE = {
  /** 间距梯度 */
  space: [4, 8, 12, 16, 24, 32],
  /** 圆角：控件 4 / 卡片 8 / 标签 4 / 胶囊 999 */
  radius: { control: 4, card: 8, tag: 4, pill: 999 },
  /** 字号：页标题 20 / 区块标题 16 / 正文 14 / 表格 13 / 辅助 12 */
  fontSize: { page: 20, section: 16, body: 14, table: 13, caption: 12 },
  /** 行高 */
  lineHeight: { body: 22, table: 20 },
  /** 表格行高 */
  rowHeight: { compact: 40, comfortable: 48 },
  /** 浮层阴影 */
  shadow: '0 4px 16px rgba(29,33,41,.08)',
  /** 侧边导航宽度 */
  sidebar: { expanded: 240, collapsed: 64 },
  /** 顶栏高度 */
  topbar: 56,
  /** 内容区最大宽度 */
  contentMaxWidth: 1600,
} as const;

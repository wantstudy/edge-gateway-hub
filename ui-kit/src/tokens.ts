/**
 * @file tokens.ts
 * @module ui-kit/tokens
 * @description 设计 token 的 TypeScript 单一事实源（Single Source of Truth）。
 *
 * 视觉权威：`docs/design/prototype/gateway-v2a-glacier.html`（方案 A · 冰川，用户确认稿）。
 * `tokens.css` 里的 CSS 变量值必须与本文件保持一致。
 *
 * 之所以同时提供 TS 常量：图表、内联样式、SVG 填充等场景无法直接读取 CSS 变量，
 * 需要一个可被 JS 引用的稳定取值来源，避免各处硬编码颜色字符串。
 */

/** 品牌色（主按钮 / 选中态 / 链接）。A·冰川：青绿 #17C3B2 及其派生。 */
export const BRAND = {
  /** 主色（原型 --accent） */
  brand: '#17C3B2',
  /** 悬停（原型 --accent-2） */
  brandHover: '#0FA396',
  /** 按下（accent-2 再深一档） */
  brandActive: '#0D8F84',
  /** 选中行底色 / 标签底（原型 --accent-soft） */
  brandSubtle: '#E2FBF7',
  /** 青绿描边（原型 --accent-line） */
  brandLine: '#A8EDE4',
} as const;

/** 中性色（文本 / 边框 / 背景）。 */
export const NEUTRAL = {
  text1: '#101733',
  text2: '#5A6486',
  text3: '#8E97B4',
  border: '#E1E5EF',
  divider: '#F3F5FA',
  bgApp: '#F4F6FB',
  bgCard: '#FFFFFF',
  bgHover: '#FAFBFE',
} as const;

/** 语义状态色（工业设备语境）。 */
export const SEMANTIC = {
  ok: '#12B76A',
  warn: '#F79009',
  danger: '#F04438',
  info: '#2E90FA',
  unknown: '#8E97B4',
} as const;

/** 状态标签的浅色底 + 深色文字组合（对比度 ≥ 4.5:1，见设计系统 §6）。 */
export const SEMANTIC_SURFACE = {
  ok: { bg: '#E6F9F0', fg: '#0A6B47', border: '#A9EBC8' },
  warn: { bg: '#FFF6E6', fg: '#8A4B00', border: '#FFDFA6' },
  danger: { bg: '#FEECEA', fg: '#8C1D18', border: '#FBC9C4' },
  info: { bg: '#E9F3FE', fg: '#12489C', border: '#BFDDFB' },
  unknown: { bg: '#F0F3F9', fg: '#5A6486', border: '#DDE2EC' },
} as const;

/** 品牌渐变（按钮 / KPI 图标方块，原型 :27-29）。 */
export const GRADIENT = {
  /** 青绿→蓝（品牌主渐变） */
  brand: 'linear-gradient(135deg, #17C3B2 0%, #1B6BE0 100%)',
  /** 青绿渐变（KPI 图标 teal 方块） */
  teal: 'linear-gradient(135deg, #2BD9C8 0%, #0FA396 100%)',
  /** 蓝渐变（主按钮） */
  ink: 'linear-gradient(135deg, #4C8DF6 0%, #1B4FD8 100%)',
} as const;

/** 深色侧栏专用 token（原型 :20, :77-100）。 */
export const SIDEBAR = {
  bg: '#0F1B3D',
  bg2: '#16244C',
  line: 'rgba(255,255,255,.08)',
  text: '#B9C2DC',
  textActive: '#FFFFFF',
  muted: '#8A94B8',
  activeBg: 'rgba(23,195,178,.16)',
} as const;

/** 语义色调枚举。 */
export type Tone = keyof typeof SEMANTIC_SURFACE;

/** 尺度 token。 */
export const SCALE = {
  /** 间距梯度 */
  space: [4, 8, 12, 16, 24, 32],
  /** 圆角：控件 8 / 卡片 16 / 内部分区 12 / 按钮 10 / 胶囊 999（A·冰川大圆角） */
  radius: { control: 8, card: 16, inner: 12, button: 10, tag: 8, pill: 999 },
  /** 字号：页标题 20 / 区块标题 16 / 正文 14 / 表格 13 / 辅助 12 */
  fontSize: { page: 20, section: 16, body: 14, table: 13, caption: 12 },
  /** 行高 */
  lineHeight: { body: 22, table: 20 },
  /** 表格行高 */
  rowHeight: { compact: 40, comfortable: 48 },
  /** 浮层阴影（= shadow-2 柔和扩散） */
  shadow: '0 10px 26px -10px rgba(16,24,60,.16)',
  /** 阴影梯度（原型 :39-42） */
  shadowScale: {
    s1: '0 1px 2px rgba(16,24,60,.05)',
    s2: '0 10px 26px -10px rgba(16,24,60,.16)',
    s3: '0 30px 70px -20px rgba(16,24,60,.30)',
    pop: '0 24px 60px -16px rgba(16,24,60,.28)',
  },
  /** 侧边导航宽度 */
  sidebar: { expanded: 240, collapsed: 64 },
  /** 顶栏高度 */
  topbar: 56,
  /** 内容区最大宽度 */
  contentMaxWidth: 1600,
} as const;

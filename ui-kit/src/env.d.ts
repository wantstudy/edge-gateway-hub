/// <reference types="vite/client" />

/**
 * 允许在 TypeScript 中直接 import 纯 CSS 文件（tokens.css 等）。
 * Vite 构建时会将其作为副作用导入并注入到产物中。
 */
declare module '*.css';

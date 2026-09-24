/**
 * @file web-console 环境类型声明。
 */

/// <reference types="vite/client" />

/** Vite 环境变量类型补充（与 vite/client 的 ImportMetaEnv 合并）。 */
interface ImportMetaEnv {
  /** API 模式：`real` 走真实后端（经 vite proxy）；未设 / 其他值 = `mock`（默认）。 */
  readonly VITE_API_MODE?: string;
}

/** 允许 TS 直接导入 .vue 单文件组件。 */
declare module '*.vue' {
  import type { DefineComponent } from 'vue';
  const component: DefineComponent<Record<string, unknown>, Record<string, unknown>, unknown>;
  export default component;
}

/** 允许导入纯 CSS。 */
declare module '*.css';

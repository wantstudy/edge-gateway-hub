/**
 * @file admin-console 环境类型声明。
 */

/// <reference types="vite/client" />

/** 允许 TS 直接导入 .vue 单文件组件。 */
declare module '*.vue' {
  import type { DefineComponent } from 'vue';
  const component: DefineComponent<Record<string, unknown>, Record<string, unknown>, unknown>;
  export default component;
}

/** 允许导入纯 CSS。 */
declare module '*.css';

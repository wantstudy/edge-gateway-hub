/**
 * @file arco-icon.ts
 * @description Arco 图标模块的轻量 stub，供 ui-kit 单元测试使用。
 *
 * 为什么需要：`src/rbac.ts` 为页面元信息保留了 `icon` 字段（真实应用里是 Arco 图标组件），
 * 但 ui-kit 包本身**不依赖 Arco**（两端应用各自安装）。测试时通过 vitest alias 把
 * `@arco-design/web-vue/es/icon` 指向本文件，避免引入整个组件库。
 *
 * 这里导出的是「空组件」工厂：`icon` 字段在测试中不会被渲染，只做类型占位。
 */

/** 生成一个占位图标组件（有稳定 name，便于调试）。 */
function createStubIcon(name: string): { name: string } {
  return { name: `StubIcon_${name}` };
}

export const IconDashboard = createStubIcon('Dashboard');
export const IconFile = createStubIcon('File');
export const IconComputer = createStubIcon('Computer');
export const IconSafe = createStubIcon('Safe');
export const IconExclamationCircle = createStubIcon('ExclamationCircle');
export const IconSwap = createStubIcon('Swap');
export const IconLock = createStubIcon('Lock');
export const IconHistory = createStubIcon('History');
export const IconUserGroup = createStubIcon('UserGroup');

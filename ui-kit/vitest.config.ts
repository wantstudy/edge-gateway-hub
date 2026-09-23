import { defineConfig } from 'vitest/config';

/**
 * ui-kit 单元测试配置。
 * 使用 jsdom 环境以便挂载 Vue 组件做行为断言（如 DangerConfirmModal 的三重校验）。
 */
export default defineConfig({
  test: {
    environment: 'jsdom',
    globals: true,
    include: ['tests/**/*.spec.ts'],
  },
});

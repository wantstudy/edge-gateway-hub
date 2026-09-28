import { defineConfig } from 'vitest/config';
import vue from '@vitejs/plugin-vue';

/**
 * ui-kit 单元测试配置。
 * 使用 jsdom 环境以便挂载 Vue 组件做行为断言（如 DangerConfirmModal 的三重校验）。
 *
 * 说明：`rbac.ts` 引用了 Arco 图标组件（导航图标），而 ui-kit 自身不依赖 Arco。
 * 这里用 `alias` 把 `@arco-design/web-vue/es/icon` 映射到一个轻量 stub 模块，
 * 使 ui-kit 的单测无需安装 Arco 即可运行，同时保证运行时（在 admin-console 内）仍用真图标。
 */
export default defineConfig({
  plugins: [vue()],
  resolve: {
    alias: {
      '@arco-design/web-vue/es/icon': new URL('./tests/stubs/arco-icon.ts', import.meta.url).pathname,
    },
  },
  test: {
    environment: 'jsdom',
    globals: true,
    include: ['tests/**/*.spec.ts'],
  },
});

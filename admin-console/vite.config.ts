import { fileURLToPath, URL } from 'node:url';
import { defineConfig } from 'vite';
import vue from '@vitejs/plugin-vue';

/**
 * admin-console Vite 配置。
 *
 * 关键约束（`ui-design-system.md` §1）：本应用与 `web-console` 是**两个独立构建产物**，
 * 必须产出独立 dist；只有在源码层面通过 `@ui-kit` 别名共享设计 token 与业务组件，
 * 以保证视觉一致且不引入两套组件库 / 两套 token。
 */
export default defineConfig({
  plugins: [vue()],
  resolve: {
    alias: {
      /** 共用设计 token 与业务组件包（源码方式引用，无需预构建） */
      '@ui-kit': fileURLToPath(new URL('../ui-kit/src', import.meta.url)),
      /** 应用内源码根 */
      '@': fileURLToPath(new URL('./src', import.meta.url)),
    },
  },
  server: {
    port: 5273,
    open: false,
  },
  build: {
    outDir: 'dist',
    emptyOutDir: true,
    sourcemap: false,
    chunkSizeWarningLimit: 1200,
  },
});

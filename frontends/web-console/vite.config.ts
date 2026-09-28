import { fileURLToPath, URL } from 'node:url';
import { defineConfig } from 'vite';
import vue from '@vitejs/plugin-vue';

/**
 * web-console Vite 配置。
 *
 * 关键约束（`ui-design-system.md` §1）：本应用与 `admin-console` 是**两个独立构建产物**，
 * 必须产出独立 dist；只有在源码层面通过 `@ui-kit` 别名共享设计 token 与业务组件，
 * 以保证视觉一致且不引入两套组件库 / 两套 token。
 *
 * 端口：本机 5274（admin-console 使用 5273，避免同机开发时端口冲突）。
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
    port: 5274,
    open: false,
    /**
     * 本地联调代理（real 模式）：把 `/api` 转发到本机网关 daemon。
     *
     * · 仅 dev server 生效，构建产物不受影响；
     * · `VITE_API_MODE` 未设或为 `mock` 时前端根本不发请求，此代理空闲无副作用；
     * · 目标地址与 daemon 默认管理端口对齐（`http://127.0.0.1:8080`）。
     */
    proxy: {
      '/api': {
        target: 'http://127.0.0.1:8080',
        changeOrigin: true,
      },
    },
  },
  build: {
    outDir: 'dist',
    emptyOutDir: true,
    sourcemap: false,
    chunkSizeWarningLimit: 1200,
  },
});

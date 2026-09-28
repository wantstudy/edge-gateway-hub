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
    /**
     * real 模式联调代理：`/licensing/*` → licensing-server（默认 0.0.0.0:7080，
     * 经 `IOT_DAQ_LISTEN_ADDR` 配置）。仅 `VITE_API_MODE=real` 时数据层会发请求，
     * mock 模式完全不经过此代理（零影响）。前缀在转发前剥掉，保持后端原始路径
     * （如 `/admin/codes/issue`）。
     */
    proxy: {
      '/licensing': {
        target: 'http://127.0.0.1:7080',
        changeOrigin: true,
        rewrite: (path: string) => path.replace(/^\/licensing/, ''),
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

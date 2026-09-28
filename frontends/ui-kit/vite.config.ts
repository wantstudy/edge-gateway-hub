import { defineConfig } from 'vite';
import vue from '@vitejs/plugin-vue';

/**
 * ui-kit 库构建配置（`npx vite build` 真构建自验用）。
 * ui-kit 以源码形式被两端引用（package.json main → src/index.ts），
 * 此构建不发布产物，仅用于回归验证「ui-kit 可独立编译通过」。
 * vue / Arco 图标作为外部依赖不打入包内（运行时由消费端提供）。
 */
export default defineConfig({
  plugins: [vue()],
  build: {
    lib: {
      entry: 'src/index.ts',
      formats: ['es'],
      fileName: 'index',
    },
    rollupOptions: {
      external: ['vue', '@arco-design/web-vue', '@arco-design/web-vue/es/icon'],
    },
    outDir: 'dist',
    emptyOutDir: true,
  },
});

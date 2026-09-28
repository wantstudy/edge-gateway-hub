/**
 * @file main.ts
 * @module web-console
 * @description 应用入口：装配 ui-kit 设计 token、Arco Design Vue、路由与根组件。
 *
 * 注意：设计 token（`@ui-kit/tokens.css`）必须**先于** Arco 样式导入，
 * 以便应用侧变量可覆盖组件库默认值。
 */
import { createApp } from 'vue';
import ArcoVue from '@arco-design/web-vue';
import ArcoVueIcon from '@arco-design/web-vue/es/icon';

// ui-kit 设计 token（两端共用，禁止在本应用内再定义第二套）
import '@ui-kit/tokens.css';
// Arco 组件库样式
import '@arco-design/web-vue/dist/arco.css';
// 应用级全局样式（布局骨架 + 少量覆盖）
import './styles/global.css';

import { watch } from 'vue';
import App from './App.vue';
import { router } from './router';
import i18n from './i18n';
import { getStoredToken } from './api/client';
import { preloadRealData } from './api/repo';
import { closeStream, connectStream, streamStatus } from './api/stream';
import { session } from './store/session';

const app = createApp(App);

app.use(ArcoVue);
app.use(ArcoVueIcon);
app.use(router);
app.use(i18n);

/**
 * 启动引导。
 *
 * ── D-01 修复要点 ────────────────────────────────────────────────────────────
 * 旧实现在挂载前 `await preloadRealData()`，而 preload 里混入了 `/api/events`
 * （**无限 SSE 流**，`res.text()` 永不 resolve）→ 带 token 刷新时**永久白屏**。
 * 现改为：preload **后台触发**（内部另有 8s 超时护栏），`app.mount` 不再等待它；
 * 缓存填充完成后由 `dataVersion`（响应式）驱动页面刷新（总览页 1s tick 亦会
 * 自动取到最新缓存）。未登录场景不发任何网络请求。
 */
async function bootstrap(): Promise<void> {
  if (getStoredToken()) {
    // 后台预取：失败 / 超时均由 repo 层按诚实空态处理，不影响首屏挂载
    void preloadRealData().catch((cause: unknown) => {
      console.warn('[web-console] bootstrap 预取真实数据失败，页面按诚实空态展示', cause);
    });
  }
  // SSE 生命周期由登录态驱动（刷新 / 登录 / 登出三态全覆盖）；SSE 通道状态同步到
  // 顶栏连接指示（open→connected，connecting→degraded，unauthorized / idle→disconnected）。
  watch(
    () => session.state.loggedIn,
    (loggedIn) => {
      if (loggedIn && getStoredToken()) {
        connectStream();
      } else {
        closeStream();
      }
    },
    { immediate: true },
  );
  watch(
    () => streamStatus.value,
    (status) => {
      if (status === 'open') {
        session.setConnection('connected');
      } else if (status === 'connecting') {
        session.setConnection('degraded');
      } else {
        session.setConnection('disconnected');
      }
    },
  );
  app.mount('#app');
}

void bootstrap();

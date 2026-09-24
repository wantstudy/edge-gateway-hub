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

import App from './App.vue';
import { router } from './router';
import { API_MODE, getStoredToken } from './api/client';
import { preloadRealData } from './api/repo';

const app = createApp(App);

app.use(ArcoVue);
app.use(ArcoVueIcon);
app.use(router);

/**
 * 启动引导：real 模式且已有 token（页面刷新场景）时，先预取真实数据再挂载，
 * 保证首屏读到的是后端数据而非 mock 回退；任一接口失败由 repo 层回退 mock 兜底。
 * mock 模式（默认）与未登录场景直接挂载，无任何网络请求。
 */
async function bootstrap(): Promise<void> {
  if (API_MODE === 'real' && getStoredToken()) {
    await preloadRealData();
  }
  app.mount('#app');
}

void bootstrap();

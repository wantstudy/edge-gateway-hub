/**
 * @file main.ts
 * @module admin-console
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
import { API_MODE } from './api/client';
import { session } from './store/session';

// real 模式启动时恢复持久化会话（token 有效性由首个后端请求的 401 统一裁决）
session.restore(API_MODE === 'real');

const app = createApp(App);

app.use(ArcoVue);
app.use(ArcoVueIcon);
app.use(router);

app.mount('#app');

import { createApp } from 'vue'
import ArcoDesign from '@arco-design/web-vue'
import '@arco-design/web-vue/dist/arco.css'
import zhCN from '@arco-design/web-vue/es/locale/lang/zh-cn'
import App from './App.vue'
import router from './router'

const app = createApp(App)
app.use(ArcoDesign, { locale: zhCN })
app.use(router)
app.mount('#app')

import { createRouter, createWebHistory } from 'vue-router'
import Home from '@/pages/Home.vue'
import Docs from '@/pages/Docs.vue'
import Contact from '@/pages/Contact.vue'

const router = createRouter({
  history: createWebHistory(),
  routes: [
    { path: '/', name: 'home', component: Home },
    { path: '/docs', name: 'docs', component: Docs },
    { path: '/contact', name: 'contact', component: Contact },
  ],
})

export default router

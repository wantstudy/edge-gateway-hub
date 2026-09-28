<template>
  <header class="site-header">
    <div class="container header-inner">
      <div class="logo">
        <div class="logo-icon">⬡</div>
        <span class="logo-text">IoT-DAQ Gateway</span>
      </div>
      <nav class="nav">
        <router-link to="/" class="nav-link" :class="{ active: isActive('/') }">首页</router-link>
        <router-link to="/docs" class="nav-link" :class="{ active: isActive('/docs') }">文档</router-link>
        <router-link to="/contact" class="nav-link" :class="{ active: isActive('/contact') }">联系我们</router-link>
      </nav>
      <a-button type="primary" class="nav-cta" href="#download" @click.prevent="scrollToDownload">立即获取</a-button>
    </div>
  </header>
</template>

<script setup lang="ts">
import { useRoute } from 'vue-router'

const route = useRoute()

function isActive(path: string): boolean {
  if (path === '/') return route.path === '/'
  return route.path.startsWith(path)
}

function scrollToDownload() {
  const el = document.getElementById('download')
  if (el) el.scrollIntoView({ behavior: 'smooth' })
}
</script>

<style scoped>
.site-header {
  position: sticky;
  top: 0;
  z-index: 100;
  background: rgba(15, 27, 61, 0.95);
  backdrop-filter: blur(10px);
  border-bottom: 1px solid rgba(23, 195, 178, 0.2);
}
.container {
  max-width: 1200px;
  margin: 0 auto;
  padding: 0 24px;
}
.header-inner {
  display: flex;
  align-items: center;
  height: 64px;
  gap: 32px;
}
.logo {
  display: flex;
  align-items: center;
  gap: 10px;
  font-weight: 700;
  font-size: 18px;
  color: var(--brand);
  text-decoration: none;
}
.logo-icon {
  font-size: 24px;
}
.nav {
  display: flex;
  gap: 24px;
  flex: 1;
}
.nav-link {
  color: var(--text-muted);
  text-decoration: none;
  font-size: 15px;
  font-weight: 500;
  transition: color 0.2s;
  padding: 4px 0;
  border-bottom: 2px solid transparent;
}
.nav-link:hover,
.nav-link.active {
  color: var(--brand);
  border-bottom-color: var(--brand);
}
.nav-cta {
  background: var(--brand) !important;
  border-color: var(--brand) !important;
}
@media (max-width: 768px) {
  .nav { display: none; }
  .nav-cta { display: none; }
}
</style>

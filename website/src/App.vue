<template>
  <div class="site">
    <SiteHeader />
    <router-view />
    <SiteFooter />
  </div>
</template>

<script setup lang="ts">
import SiteHeader from '@/components/SiteHeader.vue'
import SiteFooter from '@/components/SiteFooter.vue'
</script>

<style>
:root {
  --brand: #17C3B2;
  --brand-dim: #0e8f82;
  --navy: #0F1B3D;
  --navy-light: #162547;
  --surface: #0a0e1a;
  --text: #e2e8f0;
  --text-muted: #94a3b8;
}
</style>

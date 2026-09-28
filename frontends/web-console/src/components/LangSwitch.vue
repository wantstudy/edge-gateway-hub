<template>
  <select
    :value="locale"
    @change="onLangChange"
    class="lang-switch"
    aria-label="切换语言"
  >
    <option value="zh-CN">中文</option>
    <option value="en-US">English</option>
  </select>
</template>

<script setup lang="ts">
import { useI18n } from 'vue-i18n'

const { locale } = useI18n()

function onLangChange(e: Event) {
  const lang = (e.target as HTMLSelectElement).value
  locale.value = lang
  localStorage.setItem('locale', lang)
}
</script>

<style scoped>
.lang-switch {
  background: transparent;
  color: #17C3B2;
  border: 1px solid rgba(23,195,178,0.3);
  border-radius: 6px;
  padding: 4px 8px;
  font-size: 13px;
  cursor: pointer;
}
.lang-switch:hover {
  border-color: #17C3B2;
}
</style>

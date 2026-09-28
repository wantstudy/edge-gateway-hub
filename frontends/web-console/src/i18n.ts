import { createI18n } from 'vue-i18n'
import zhCN from './locales/zh-CN'
import enUS from './locales/en-US'

// Detect language from browser or storage
function detectLocale(): string {
  const stored = localStorage.getItem('locale')
  if (stored && (stored === 'zh-CN' || stored === 'en-US')) return stored
  const nav = navigator.language?.toLowerCase()
  if (nav?.startsWith('zh')) return 'zh-CN'
  return 'en-US'
}

const i18n = createI18n({
  legacy: false,
  locale: detectLocale(),
  fallbackLocale: 'en-US',
  messages: {
    'zh-CN': zhCN,
    'en-US': enUS
  }
})

export default i18n
export { detectLocale }

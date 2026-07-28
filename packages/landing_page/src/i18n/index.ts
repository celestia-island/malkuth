import { setLocale, mergeMessages } from '@celestia-island/hikari'
import en from './locales/en/main.json'
import zhs from './locales/zhs/main.json'

mergeMessages(en, 'en')
mergeMessages(zhs, 'zhs')

const full = (navigator.language || 'en').toLowerCase()
const userLang = full.split('-')[0]
function resolveLocale(): string {
  if (full.startsWith('zh-cn') || full.startsWith('zh-sg') || full.startsWith('zh-my')) return 'zhs'
  if (full.startsWith('zh-tw') || full.startsWith('zh-hk') || full.startsWith('zh-mo')) return 'zht'
  if (userLang === 'zh') return 'zhs'
  if (userLang === 'en') return 'en'
  return 'en'
}
setLocale(resolveLocale())

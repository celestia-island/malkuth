import { setLocale } from '@celestia-island/hikari'

const userLang = (navigator.language || 'en').split('-')[0]
const supported = ['en', 'zhs', 'zht', 'ja', 'ko', 'fr', 'de', 'es', 'pt', 'ru', 'ar']
const lang = supported.includes(userLang) ? userLang : 'en'

setLocale(lang)

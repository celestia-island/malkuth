import { setLocale, mergeMessages } from '@celestia-island/hikari'
import en from './locales/en/main.json'
import zhs from './locales/zhs/main.json'

mergeMessages(en, 'en')
mergeMessages(zhs, 'zhs')

const userLang = (navigator.language || 'en').split('-')[0]
const supported = ['en', 'zhs', 'zht', 'ja', 'ko', 'fr', 'de', 'es', 'pt', 'ru', 'ar']
const lang = supported.includes(userLang) ? userLang : 'en'

setLocale(lang)

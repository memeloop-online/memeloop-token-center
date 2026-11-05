import DefaultTheme from 'vitepress/theme'
import { withBase } from 'vitepress'
import type { Theme } from 'vitepress'
import ProductGallery from './components/ProductGallery.vue'
import './custom.css'

export default {
  extends: DefaultTheme,
  enhanceApp({ app, router }) {
    app.component('ProductGallery', ProductGallery)

    if (typeof window !== 'undefined') {
      const syncLocale = (path: string) => {
        const base = withBase('/')
        const relativePath = path.startsWith(base) ? path.slice(base.length) : path.replace(/^\//, '')
        const locale = relativePath.match(/^(zh|en)(?:\/|$)/)?.[1]
        if (!locale) return
        try {
          window.localStorage.setItem('mtc-docs-locale', locale)
        } catch {
          // Navigation continues when browser storage is unavailable.
        }
      }

      syncLocale(router.route.path)
      const previousAfterRouteChange = router.onAfterRouteChange
      router.onAfterRouteChange = async (to) => {
        await previousAfterRouteChange?.(to)
        syncLocale(to)
      }
    }
  }
} satisfies Theme

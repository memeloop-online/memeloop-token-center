import DefaultTheme from 'vitepress/theme'
import type { Theme } from 'vitepress'
import ProductGallery from './components/ProductGallery.vue'
import './custom.css'

export default {
  extends: DefaultTheme,
  enhanceApp({ app, router }) {
    app.component('ProductGallery', ProductGallery)

    if (typeof window !== 'undefined') {
      const syncLocale = (path: string) => {
        const locale = path.match(/^\/(zh|en)(?:\/|$)/)?.[1]
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

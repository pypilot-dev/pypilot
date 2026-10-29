import { h, nextTick } from 'vue'
import type { Theme } from 'vitepress'
import DefaultTheme from 'vitepress/theme'

import '@fontsource-variable/inter'
import '@fontsource-variable/jetbrains-mono'
import './style.css'

import HeroOrbit from './components/HeroOrbit.vue'
import TerminalDemo from './components/TerminalDemo.vue'
import ArchDiagram from './components/ArchDiagram.vue'
import { observeReveals } from './reveal'

const reducedMotion = () =>
  typeof window !== 'undefined' &&
  window.matchMedia('(prefers-reduced-motion: reduce)').matches

export default {
  extends: DefaultTheme,

  Layout() {
    return h(DefaultTheme.Layout, null, {
      'home-hero-image': () => h(HeroOrbit)
    })
  },

  enhanceApp({ app, router }) {
    app.component('TerminalDemo', TerminalDemo)
    app.component('ArchDiagram', ArchDiagram)

    if (typeof window === 'undefined') return

    // Page transitions with the View Transitions API. VitePress is a SPA, so
    // the transition wraps its route change: the "old" snapshot is taken
    // before the route changes, the "new" one once the new page has rendered.
    // Browsers without the API, and readers who prefer reduced motion, get a
    // plain instant navigation.
    let finish: (() => void) | null = null
    const done = () => {
      finish?.()
      finish = null
    }

    router.onBeforeRouteChange = (to) => {
      const doc = document as Document & {
        startViewTransition?: (cb: () => Promise<void>) => unknown
      }
      if (!doc.startViewTransition || reducedMotion()) return
      // Same page, different anchor: let the browser scroll.
      const target = new URL(to, location.href)
      if (target.pathname === location.pathname) return
      done()
      return new Promise<void>((started) => {
        doc.startViewTransition!(
          () =>
            new Promise<void>((rendered) => {
              finish = rendered
              started()
              // Never leave a transition hanging if the route change fails.
              setTimeout(done, 1500)
            })
        )
      })
    }

    router.onAfterRouteChange = async () => {
      await nextTick()
      done()
      observeReveals()
    }

    // First page load.
    if (document.readyState === 'loading') {
      document.addEventListener('DOMContentLoaded', () => observeReveals(), { once: true })
    } else {
      setTimeout(observeReveals, 0)
    }
  }
} satisfies Theme

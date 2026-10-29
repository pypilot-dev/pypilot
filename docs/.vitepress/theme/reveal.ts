// Reveal-on-scroll for feature cards and anything marked `.reveal`.
// One shared IntersectionObserver; each element is revealed once and then
// forgotten. Without IntersectionObserver, or with reduced motion, everything
// is simply visible (the CSS only hides elements that carry `.reveal-armed`).

let observer: IntersectionObserver | null = null

const SELECTOR = '.VPFeatures .item, .reveal'

export function observeReveals(): void {
  if (typeof window === 'undefined') return
  if (!('IntersectionObserver' in window)) return
  if (window.matchMedia('(prefers-reduced-motion: reduce)').matches) return

  observer ??= new IntersectionObserver(
    (entries) => {
      for (const entry of entries) {
        if (!entry.isIntersecting) continue
        entry.target.classList.add('reveal-in')
        observer!.unobserve(entry.target)
      }
    },
    { rootMargin: '0px 0px -8% 0px', threshold: 0.08 }
  )

  const elements = document.querySelectorAll<HTMLElement>(SELECTOR)
  elements.forEach((el, i) => {
    if (el.classList.contains('reveal-armed')) return
    // Stagger siblings a little so a grid ripples in rather than popping.
    el.style.setProperty('--reveal-delay', `${(i % 6) * 60}ms`)
    el.classList.add('reveal-armed')
    observer!.observe(el)
  })
}

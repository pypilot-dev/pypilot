<script setup lang="ts">
// A terminal window that types a command and then prints its output, line by
// line, once it scrolls into view. The output is a real transcript from
// docs/snippets/<src>.txt, the same file the reference pages include, so the
// animation and the docs cannot drift apart.
//
// Server-rendered HTML holds the full transcript (readable without JS, and
// indexed by search); the animation only starts after hydration, and never
// with prefers-reduced-motion.

import { computed, onBeforeUnmount, onMounted, ref } from 'vue'

const props = withDefaults(
  defineProps<{ src: string; cmd: string; title?: string; prompt?: string }>(),
  { title: 'Terminal', prompt: '$' }
)

const files = import.meta.glob('../../../snippets/*.txt', {
  query: '?raw',
  import: 'default',
  eager: true
}) as Record<string, string>

const text = computed(() => {
  const hit = Object.entries(files).find(([k]) => k.endsWith(`/${props.src}.txt`))
  return hit ? hit[1].replace(/\s+$/, '') : `(missing transcript: ${props.src})`
})
const lines = computed(() => text.value.split('\n'))

type Seg = { t: string; c?: string }
const TOKENS: [RegExp, string][] = [
  [/\[ERROR\]/, 'err'],
  [/\[WARN \]/, 'warn'],
  [/\[INFO \]/, 'info'],
  [/✓/, 'ok'],
  [/✗/, 'err'],
  [/▸/, 'info']
]
function paint(line: string): Seg[] {
  if (/^\s*──/.test(line)) return [{ t: line, c: 'rule' }]
  if (/^(PyPilot|Dry run|Would write|Wrote|Next:)/.test(line)) return [{ t: line, c: 'head' }]
  if (/^\+/.test(line)) return [{ t: line, c: 'add' }]
  if (/^(---|@@)/.test(line)) return [{ t: line, c: 'dim' }]
  const out: Seg[] = []
  let rest = line
  while (rest.length) {
    let best: { i: number; len: number; c: string } | null = null
    for (const [re, c] of TOKENS) {
      const m = re.exec(rest)
      if (m && (best === null || m.index < best.i)) best = { i: m.index, len: m[0].length, c }
    }
    if (!best) {
      out.push({ t: rest })
      break
    }
    if (best.i > 0) out.push({ t: rest.slice(0, best.i) })
    out.push({ t: rest.slice(best.i, best.i + best.len), c: best.c })
    rest = rest.slice(best.i + best.len)
  }
  return out
}
const painted = computed(() => lines.value.map(paint))

// Animation state. SSR / no-JS / reduced motion: everything visible.
const typed = ref(props.cmd.length)
const shown = ref(Number.MAX_SAFE_INTEGER)
const running = ref(false)
const root = ref<HTMLElement | null>(null)
let timers: number[] = []
let io: IntersectionObserver | null = null

const clear = () => {
  timers.forEach((t) => clearTimeout(t))
  timers = []
}
const later = (fn: () => void, ms: number) => timers.push(window.setTimeout(fn, ms))

function play() {
  clear()
  running.value = true
  typed.value = 0
  shown.value = 0
  let t = 250
  for (let i = 1; i <= props.cmd.length; i++) {
    later(() => (typed.value = i), t)
    t += 34 + Math.random() * 40
  }
  t += 380 // "thinking"
  const step = Math.max(14, Math.min(60, 1400 / lines.value.length))
  for (let i = 1; i <= lines.value.length; i++) {
    later(() => (shown.value = i), t)
    t += step
  }
  later(() => (running.value = false), t)
}

onMounted(() => {
  if (window.matchMedia('(prefers-reduced-motion: reduce)').matches) return
  if (!('IntersectionObserver' in window)) return
  typed.value = 0
  shown.value = 0
  io = new IntersectionObserver(
    (entries) => {
      if (entries.some((e) => e.isIntersecting)) {
        io?.disconnect()
        play()
      }
    },
    { threshold: 0.25 }
  )
  if (root.value) io.observe(root.value)
})
onBeforeUnmount(() => {
  clear()
  io?.disconnect()
})

const caretOnCmd = computed(() => typed.value < props.cmd.length || (running.value && shown.value === 0))
</script>

<template>
  <figure ref="root" class="term" :aria-label="`${cmd} — example output`">
    <div class="bar">
      <span class="dots"><i /><i /><i /></span>
      <span class="title">{{ title }}</span>
      <button class="replay" type="button" title="Replay" aria-label="Replay animation" @click="play">↻</button>
    </div>
    <pre class="body"><code><span class="prompt">{{ prompt }} </span><span class="cmd">{{ cmd.slice(0, typed) }}</span><span v-if="caretOnCmd" class="caret" />
<template v-for="(segs, i) in painted" :key="i"><span v-if="i < shown" class="line"><span v-for="(s, j) in segs" :key="j" :class="s.c">{{ s.t }}</span>
</span></template><span v-if="!caretOnCmd && running" class="caret" /></code></pre>
  </figure>
</template>

<style scoped>
.term {
  margin: 20px 0;
  border-radius: 12px;
  border: 1px solid var(--pp-line-strong);
  background: linear-gradient(180deg, #0a0b0e, #050506);
  box-shadow: 0 30px 80px -40px rgba(42, 169, 255, 0.35), inset 0 1px 0 rgba(255, 255, 255, 0.04);
  overflow: hidden;
  transition: border-color 0.3s var(--pp-ease), box-shadow 0.3s var(--pp-ease);
}
.term:hover {
  border-color: rgba(42, 169, 255, 0.35);
}
.bar {
  display: flex;
  align-items: center;
  gap: 12px;
  padding: 10px 14px;
  border-bottom: 1px solid var(--pp-line);
  background: rgba(255, 255, 255, 0.02);
}
.dots {
  display: inline-flex;
  gap: 6px;
}
.dots i {
  width: 10px;
  height: 10px;
  border-radius: 50%;
  background: #2a2e36;
}
.term:hover .dots i:nth-child(1) { background: #ff5f57; }
.term:hover .dots i:nth-child(2) { background: #febc2e; }
.term:hover .dots i:nth-child(3) { background: #28c840; }
.dots i { transition: background 0.3s; }
.title {
  flex: 1;
  text-align: center;
  font-size: 12px;
  color: var(--vp-c-text-3);
  font-family: var(--vp-font-family-mono);
}
.replay {
  color: var(--vp-c-text-3);
  font-size: 14px;
  line-height: 1;
  padding: 2px 6px;
  border-radius: 6px;
  opacity: 0;
  transition: opacity 0.2s, color 0.2s, background 0.2s;
}
.term:hover .replay,
.replay:focus-visible {
  opacity: 1;
}
.replay:hover {
  color: var(--pp-accent);
  background: rgba(42, 169, 255, 0.1);
}
.body {
  margin: 0;
  padding: 16px 18px 18px;
  font-family: var(--vp-font-family-mono);
  font-size: 12.5px;
  line-height: 1.62;
  color: #cfd6e0;
  white-space: pre-wrap;
  word-break: break-word;
  min-height: 3em;
  max-height: 560px;
  overflow: auto;
}
.prompt { color: var(--pp-accent); }
.cmd { color: #fff; font-weight: 600; }
.line { animation: line-in 0.25s var(--pp-ease); }
.rule { color: #4b5563; }
.head { color: #fff; font-weight: 600; }
.dim { color: #6c7380; }
.add { color: #9be29b; }
.ok { color: #4ade80; }
.warn { color: #fbbf24; font-weight: 700; }
.err { color: #f87171; font-weight: 700; }
.info { color: var(--pp-accent-2); font-weight: 700; }
.caret {
  display: inline-block;
  width: 0.55em;
  height: 1.1em;
  margin-left: 1px;
  vertical-align: -0.2em;
  background: var(--pp-accent);
  animation: blink 1s steps(1) infinite;
}
@keyframes blink { 50% { opacity: 0; } }
@keyframes line-in { from { opacity: 0; transform: translateX(-4px); } }
@media (prefers-reduced-motion: reduce) {
  .line, .caret { animation: none; }
}
</style>

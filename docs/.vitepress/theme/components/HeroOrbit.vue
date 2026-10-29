<script setup lang="ts">
// The hero picture is PyPilot's core idea drawn as a dial: CPython 3.7-3.15
// around a ring, one arc for what `requires_python` allows and one for the
// versions that have wheels. Where they overlap is what the project runs on.
// The example is mediapipe 0.10.14 (requires_python not declared,
// wheels cp39-cp312), the case from the README. Pure SVG + CSS.

const versions = ['3.7', '3.8', '3.9', '3.10', '3.11', '3.12', '3.13', '3.14', '3.15']
const C = 200
const R = 128
const START = 135 // degrees, clockwise from +x; the gap sits at the bottom
const STEP = 270 / (versions.length - 1)

const rad = (deg: number) => (deg * Math.PI) / 180
const at = (i: number, r: number) => {
  const a = rad(START + i * STEP)
  return { x: +(C + r * Math.cos(a)).toFixed(2), y: +(C + r * Math.sin(a)).toFixed(2) }
}
const arc = (from: number, to: number, r: number) => {
  const p = at(from - 0.35, r)
  const q = at(to + 0.35, r)
  const large = (to - from + 0.7) * STEP > 180 ? 1 : 0
  return `M ${p.x} ${p.y} A ${r} ${r} 0 ${large} 1 ${q.x} ${q.y}`
}

const nodes = versions.map((v, i) => ({ v, ...at(i, R) }))
// requires_python: not declared, so everything PyPilot tracks; wheels: cp39..cp312.
const requiresArc = arc(0, 8, R + 26)
const wheelsArc = arc(2, 5, R + 44)
const inRange = (i: number) => i >= 2 && i <= 5
</script>

<template>
  <div class="orbit" aria-hidden="true">
    <svg viewBox="0 0 400 400" role="img">
      <defs>
        <radialGradient id="pp-core" cx="50%" cy="50%" r="50%">
          <stop offset="0%" stop-color="#2aa9ff" stop-opacity="0.35" />
          <stop offset="60%" stop-color="#0a7cff" stop-opacity="0.08" />
          <stop offset="100%" stop-color="#000" stop-opacity="0" />
        </radialGradient>
        <linearGradient id="pp-wheels" x1="0" x2="1" y1="0" y2="0">
          <stop offset="0%" stop-color="#5cc2ff" />
          <stop offset="100%" stop-color="#2aa9ff" />
        </linearGradient>
        <filter id="pp-glow" x="-50%" y="-50%" width="200%" height="200%">
          <feGaussianBlur stdDeviation="3.2" result="b" />
          <feMerge>
            <feMergeNode in="b" />
            <feMergeNode in="SourceGraphic" />
          </feMerge>
        </filter>
      </defs>

      <circle :cx="C" :cy="C" r="190" fill="url(#pp-core)" class="core" />

      <!-- Slow outer orbits with a travelling particle. -->
      <g class="spin spin-a">
        <circle :cx="C" :cy="C" r="188" class="ring" />
        <circle :cx="C + 188" :cy="C" r="2.4" class="particle" />
      </g>
      <g class="spin spin-b">
        <ellipse :cx="C" :cy="C" rx="196" ry="70" class="ring faint" />
        <circle :cx="C - 196" :cy="C" r="1.8" class="particle dim" />
      </g>

      <!-- The two constraints. -->
      <path :d="requiresArc" pathLength="1" class="arc arc-requires" />
      <path :d="wheelsArc" pathLength="1" class="arc arc-wheels" filter="url(#pp-glow)" />

      <!-- Python versions. -->
      <circle :cx="C" :cy="C" :r="R" class="track" />
      <g v-for="(n, i) in nodes" :key="n.v" class="node" :class="{ on: inRange(i) }"
         :style="{ '--d': `${0.9 + i * 0.07}s` }">
        <circle :cx="n.x" :cy="n.y" r="15" class="dot" />
        <text :x="n.x" :y="n.y + 3.6" text-anchor="middle">{{ n.v }}</text>
      </g>

      <!-- The answer. -->
      <g class="answer">
        <text :x="C" :y="C - 18" text-anchor="middle" class="caption">runs on</text>
        <text :x="C" :y="C + 14" text-anchor="middle" class="range">3.9 – 3.12</text>
        <text :x="C" :y="C + 36" text-anchor="middle" class="caption small">mediapipe 0.10.14</text>
      </g>

      <!-- Legend under the dial. -->
      <g class="legend">
        <line x1="112" y1="372" x2="132" y2="372" class="lg-requires" />
        <text x="138" y="376">requires_python</text>
        <line x1="236" y1="372" x2="256" y2="372" class="lg-wheels" />
        <text x="262" y="376">wheels</text>
      </g>
    </svg>
  </div>
</template>

<style scoped>
.orbit {
  /* Fill VitePress's hero image box (320px on phones, larger on desktop). */
  position: relative;
  width: 100%;
  max-width: 420px;
  aspect-ratio: 1;
  margin: 0 auto;
}
svg {
  width: 100%;
  height: 100%;
  overflow: visible;
  font-family: var(--vp-font-family-mono);
}

.core {
  animation: breathe 6s ease-in-out infinite;
  transform-origin: 200px 200px;
}
.ring {
  fill: none;
  stroke: rgba(255, 255, 255, 0.07);
  stroke-width: 1;
}
.ring.faint {
  stroke: rgba(42, 169, 255, 0.1);
}
.particle {
  fill: #9fdcff;
  filter: drop-shadow(0 0 6px #2aa9ff);
}
.particle.dim {
  opacity: 0.6;
}
.spin {
  transform-origin: 200px 200px;
}
.spin-a {
  animation: spin 38s linear infinite;
}
.spin-b {
  animation: spin 26s linear infinite reverse;
}

.track {
  fill: none;
  stroke: rgba(255, 255, 255, 0.06);
  stroke-width: 1;
  stroke-dasharray: 2 5;
}

.arc {
  fill: none;
  stroke-linecap: round;
  stroke-dasharray: 1;
  stroke-dashoffset: 1;
}
.arc-requires {
  stroke: #0a7cff;
  stroke-opacity: 0.55;
  stroke-width: 5;
  animation: draw 1.4s var(--pp-ease) 0.2s forwards;
}
.arc-wheels {
  stroke: url(#pp-wheels);
  stroke-width: 6;
  animation: draw 1.2s var(--pp-ease) 0.7s forwards;
}

.node .dot {
  fill: #07080a;
  stroke: rgba(255, 255, 255, 0.14);
  stroke-width: 1;
  transition: stroke 0.4s, fill 0.4s;
}
.node text {
  fill: #6c7380;
  font-size: 9.5px;
  font-weight: 600;
}
.node.on .dot {
  animation: light-up 0.6s var(--pp-ease) var(--d) forwards, pulse 3.2s ease-in-out calc(var(--d) + 0.6s) infinite;
}
.node.on text {
  animation: text-up 0.6s var(--pp-ease) var(--d) forwards;
}

.answer {
  opacity: 0;
  animation: rise 0.8s var(--pp-ease) 1.5s forwards;
}
.range {
  fill: #fff;
  font-family: var(--vp-font-family-base);
  font-size: 30px;
  font-weight: 800;
  letter-spacing: -0.02em;
}
.caption {
  fill: #8fd3ff;
  font-size: 10px;
  letter-spacing: 0.18em;
  text-transform: uppercase;
}
.caption.small {
  fill: #6c7380;
  letter-spacing: 0.04em;
  text-transform: none;
}

.legend text {
  fill: #8a919d;
  font-size: 10px;
}
.legend line {
  stroke-width: 4;
  stroke-linecap: round;
}
.lg-requires {
  stroke: #0a7cff;
  stroke-opacity: 0.7;
}
.lg-wheels {
  stroke: #5cc2ff;
}

@keyframes spin {
  to {
    transform: rotate(360deg);
  }
}
@keyframes breathe {
  0%, 100% { opacity: 0.75; transform: scale(1); }
  50% { opacity: 1; transform: scale(1.04); }
}
@keyframes draw {
  to {
    stroke-dashoffset: 0;
  }
}
@keyframes light-up {
  to {
    fill: rgba(42, 169, 255, 0.18);
    stroke: #2aa9ff;
  }
}
@keyframes text-up {
  to {
    fill: #ffffff;
  }
}
@keyframes pulse {
  0%, 100% { stroke-width: 1; }
  50% { stroke-width: 2.4; }
}
@keyframes rise {
  from { opacity: 0; transform: translateY(6px); }
  to { opacity: 1; transform: none; }
}

@media (prefers-reduced-motion: reduce) {
  .spin-a, .spin-b, .core, .node.on .dot { animation: none; }
  .arc { stroke-dashoffset: 0; animation: none; }
  .node.on .dot { fill: rgba(42, 169, 255, 0.18); stroke: #2aa9ff; }
  .node.on text { fill: #fff; animation: none; }
  .answer { opacity: 1; animation: none; }
}
</style>

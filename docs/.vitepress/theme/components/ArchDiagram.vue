<script setup lang="ts">
// "How it works": editor -> wrapper -> engine -> what the engine asks.
// Hand-drawn SVG instead of a diagram plugin: a few KB, themable, animated
// with CSS only (stroke-dashoffset), static with reduced motion.

type Box = { x: number; y: number; w: number; h: number; title: string; sub: string; kind?: string }

const H = 64
const col = { a: 20, b: 250, c: 500, d: 730 }
const W = 210

const editors: Box[] = [
  { x: col.a, y: 52, w: W, h: H, title: 'Zed', sub: 'language server' },
  { x: col.a, y: 160, w: W, h: H, title: 'VS Code', sub: 'commands, report, status bar' },
  { x: col.a, y: 268, w: W, h: H, title: 'Terminal / Python', sub: 'pypilot doctor, setup, …' }
]
const wrappers: Box[] = [
  { x: col.b, y: 52, w: W, h: H, title: 'Zed extension', sub: 'WASM shim, downloads engine' },
  { x: col.b, y: 160, w: W, h: H, title: 'pypilot-vscode', sub: 'downloads engine, LSP client' },
  { x: col.b, y: 268, w: W, h: H, title: 'pypilot-cli (pip)', sub: 'downloads engine, runs it' }
]
const engine: Box = { x: col.c, y: 124, w: W, h: 136, title: 'pypilot engine', sub: 'Rust · CLI + LSP', kind: 'engine' }
const sources: Box[] = [
  { x: col.d, y: 40, w: W, h: 58, title: 'PyPI', sub: 'wheel tags, requires_python' },
  { x: col.d, y: 124, w: W, h: 58, title: 'NVIDIA driver', sub: 'nvidia-smi + CUDA tables' },
  { x: col.d, y: 208, w: W, h: 58, title: 'conda-forge', sub: 'prefix.dev mapping' },
  { x: col.d, y: 292, w: W, h: 58, title: 'uv · pip · pixi', sub: 'build the environment' }
]

const mid = (b: Box) => b.y + b.h / 2
const link = (from: Box, to: Box, ty = mid(to)) => {
  const x1 = from.x + from.w
  const y1 = mid(from)
  const x2 = to.x
  const dx = (x2 - x1) / 2
  return `M ${x1} ${y1} C ${x1 + dx} ${y1}, ${x2 - dx} ${ty}, ${x2} ${ty}`
}

const edges = [
  ...editors.map((e, i) => link(e, wrappers[i])),
  ...wrappers.map((w, i) => link(w, engine, engine.y + 34 + i * 34)),
  ...sources.map((s) => {
    const x1 = engine.x + engine.w
    const y1 = mid(engine)
    const x2 = s.x
    const y2 = mid(s)
    const dx = (x2 - x1) / 2
    return `M ${x1} ${y1} C ${x1 + dx} ${y1}, ${x2 - dx} ${y2}, ${x2} ${y2}`
  })
]
const all = [...editors, ...wrappers, ...sources]
</script>

<template>
  <figure class="arch reveal">
    <div class="scroll">
      <svg viewBox="0 0 960 362" role="img"
           aria-label="Editors talk to a thin wrapper, which runs the PyPilot engine; the engine reads PyPI, the NVIDIA driver and conda-forge, and drives uv, pip or pixi.">
        <defs>
          <!-- userSpaceOnUse: a bounding-box gradient vanishes on perfectly horizontal paths. -->
          <linearGradient id="pp-edge" gradientUnits="userSpaceOnUse" x1="230" y1="0" x2="730" y2="0">
            <stop offset="0%" stop-color="#2aa9ff" stop-opacity="0.45" />
            <stop offset="100%" stop-color="#5cc2ff" stop-opacity="0.85" />
          </linearGradient>
          <linearGradient id="pp-engine" x1="0" x2="0" y1="0" y2="1">
            <stop offset="0%" stop-color="#0d2a47" />
            <stop offset="100%" stop-color="#05121f" />
          </linearGradient>
        </defs>

        <g class="cols">
          <text :x="col.a" y="24">EDITOR</text>
          <text :x="col.b" y="24">WRAPPER</text>
          <text :x="col.c" y="24">ENGINE</text>
          <text :x="col.d" y="24">ASKS / DRIVES</text>
        </g>

        <g class="edges">
          <path v-for="(d, i) in edges" :key="i" :d="d" class="edge" :style="{ '--i': i }" />
        </g>

        <g v-for="b in all" :key="b.title" class="box">
          <rect :x="b.x" :y="b.y" :width="b.w" :height="b.h" rx="12" />
          <text :x="b.x + 16" :y="b.y + b.h / 2 - 3" class="t">{{ b.title }}</text>
          <text :x="b.x + 16" :y="b.y + b.h / 2 + 15" class="s">{{ b.sub }}</text>
        </g>

        <g class="box engine">
          <rect :x="engine.x" :y="engine.y" :width="engine.w" :height="engine.h" rx="16" />
          <text :x="engine.x + 18" :y="engine.y + 34" class="t big">{{ engine.title }}</text>
          <text :x="engine.x + 18" :y="engine.y + 54" class="s">{{ engine.sub }}</text>
          <text :x="engine.x + 18" :y="engine.y + 84" class="s">intersects requires_python</text>
          <text :x="engine.x + 18" :y="engine.y + 101" class="s">with wheels per platform,</text>
          <text :x="engine.x + 18" :y="engine.y + 118" class="s">matches CUDA, checks imports</text>
        </g>
      </svg>
    </div>
    <figcaption>
      Data tables (CUDA, frameworks, import names, conda-forge) are compiled into the engine
      and refreshed from the repository every <code>data_refresh_days</code> (7 by default).
    </figcaption>
  </figure>
</template>

<style scoped>
.arch {
  margin: 8px 0 0;
}
.scroll {
  overflow-x: auto;
  border: 1px solid var(--pp-line);
  border-radius: 16px;
  background: radial-gradient(600px 240px at 60% 50%, rgba(10, 124, 255, 0.1), transparent 70%), #030304;
  padding: 18px 10px;
}
svg {
  display: block;
  min-width: 720px;
  width: 100%;
  height: auto;
  font-family: var(--vp-font-family-base);
}
.cols text {
  fill: #5b626e;
  font-size: 10.5px;
  font-weight: 700;
  letter-spacing: 0.16em;
}
.edge {
  fill: none;
  stroke: url(#pp-edge);
  stroke-width: 1.6;
  stroke-dasharray: 5 7;
  animation: flow 1.6s linear infinite;
  animation-delay: calc(var(--i) * -0.17s);
}
.box rect {
  fill: #08090c;
  stroke: #23262d;
  stroke-width: 1;
  transition: stroke 0.3s, fill 0.3s;
}
.box:hover rect {
  stroke: #2aa9ff;
  fill: #0a1420;
}
.box .t {
  fill: #f4f6f8;
  font-size: 14px;
  font-weight: 650;
}
.box .t.big {
  font-size: 17px;
}
.box .s {
  fill: #8a919d;
  font-size: 11.5px;
}
.engine rect {
  fill: url(#pp-engine);
  stroke: #2aa9ff;
  stroke-width: 1.4;
  filter: drop-shadow(0 0 18px rgba(42, 169, 255, 0.35));
}
figcaption {
  margin-top: 10px;
  font-size: 13px;
  color: var(--vp-c-text-3);
}
@keyframes flow {
  to {
    stroke-dashoffset: -24;
  }
}
@media (prefers-reduced-motion: reduce) {
  .edge {
    animation: none;
  }
}
</style>

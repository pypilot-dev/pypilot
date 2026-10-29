---
layout: home
title: PyPilot
titleTemplate: The Python versions your project can actually run on

hero:
  name: PyPilot
  text: The Python your project can actually run on
  tagline: PyPilot intersects requires_python with the wheels each dependency publishes for your platform, sets up the environment on a version that works, and names the package that set the limit. It runs locally, with no AI service and no API keys.
  actions:
    - theme: brand
      text: Get started
      link: /guide/getting-started
    - theme: alt
      text: What is PyPilot?
      link: /guide/what-is-pypilot
    - theme: alt
      text: GitHub
      link: https://github.com/pypilot-dev/pypilot

features:
  - icon: 🎯
    title: Wheels, not just metadata
    details: requires_python is often incomplete. PyPilot checks the wheel tags per platform too, so "mediapipe 0.10.14 stops at cp312" is caught before pip fails on 3.13.
    link: /guide/how-it-works
    linkText: How it works
  - icon: 🧭
    title: Names the culprit
    details: When no Python satisfies everything, doctor names both packages and their ranges. explain shows what blocks each Python version.
    link: /engine/commands#explain
    linkText: pypilot explain
  - icon: ⚡
    title: Sets up the environment
    details: uv by default (installs the Python it needs), plain venv + pip when you prefer, pixi in pixi projects. The Python it picks stays pinned.
    link: /engine/commands#setup
    linkText: pypilot setup
  - icon: 🟩
    title: GPU builds that match the driver
    details: Reads nvidia-smi, maps the driver to the newest CUDA it supports and pins the matching torch build (cu124, cu132, …). Apple Silicon gets MPS guidance.
    link: /guide/gpu-cuda
    linkText: GPU & CUDA
  - icon: 🧪
    title: Import diagnostics
    details: Squiggles on imports that are missing or will not run on this Python, quick fixes that install them, and checks for attributes removed from the installed release.
    link: /reference/checks
    linkText: What it reports
  - icon: 📦
    title: pixi when it helps
    details: Detects pixi projects and works through pixi. When a dependency needs native libraries pip cannot install (GDAL, CUDA toolkit), it says so and migrates you.
    link: /pixi/
    linkText: pixi guide
---

<div class="home-section reveal">

## See it work

A project that pins `mediapipe==0.10.14` next to `numpy`. PyPilot reads PyPI and answers in a fraction of a second once metadata is cached:

<TerminalDemo src="doctor" cmd="pypilot doctor" title="~/src/demo" />

</div>

<div class="home-section reveal">

## How it works

The editors and the pip package are thin wrappers. They find or download one native engine, and that engine does all the work: the same answers in Zed, VS Code and the terminal.

<ArchDiagram />

</div>

<div class="home-section home-cta reveal">

## Install it where you work

<div class="actions">
  <a class="VPButton medium brand" href="./zed/">Zed</a>
  <a class="VPButton medium alt" href="./vscode/">VS Code</a>
  <a class="VPButton medium alt" href="./python/">pip install pypilot-cli</a>
  <a class="VPButton medium alt" href="./engine/building">cargo install</a>
</div>

</div>

import { defineConfig } from 'vitepress'

// One site for the whole PyPilot ecosystem: the engine and Zed extension
// (this repo), the VS Code extension and the pypilot-cli package on PyPI.
// Published at https://pypilot-dev.github.io/pypilot/ by .github/workflows/docs.yml.

const ORG = 'https://github.com/pypilot-dev'
const BASE = '/pypilot/'

export default defineConfig({
  title: 'PyPilot',
  description:
    'Finds the Python versions your dependencies can actually run on, and sets up the environment on one of them. Engine, VS Code, Zed and pip docs.',
  lang: 'en-US',
  base: BASE,
  cleanUrls: true,
  lastUpdated: true,
  // The site is designed for black; there is no light theme to switch to.
  appearance: 'force-dark',

  head: [
    ['link', { rel: 'icon', href: `${BASE}favicon.ico`, sizes: '48x48' }],
    ['link', { rel: 'icon', type: 'image/png', href: `${BASE}favicon.png` }],
    ['link', { rel: 'apple-touch-icon', href: `${BASE}logo.png` }],
    ['meta', { name: 'theme-color', content: '#000000' }],
    ['meta', { name: 'color-scheme', content: 'dark' }],
    ['meta', { property: 'og:type', content: 'website' }],
    ['meta', { property: 'og:title', content: 'PyPilot' }],
    [
      'meta',
      {
        property: 'og:description',
        content: 'The Python versions your project can actually run on, from PyPI wheel tags. For Zed, VS Code and the terminal.'
      }
    ],
    ['meta', { property: 'og:image', content: `https://pypilot-dev.github.io${BASE}logo.png` }]
  ],

  markdown: {
    theme: { light: 'github-light', dark: 'github-dark-default' },
    lineNumbers: false
  },

  themeConfig: {
    logo: '/logo.png',
    siteTitle: 'PyPilot',

    nav: [
      { text: 'Guide', link: '/guide/what-is-pypilot', activeMatch: '/guide/' },
      { text: 'Engine (CLI)', link: '/engine/', activeMatch: '/engine/' },
      { text: 'VS Code', link: '/vscode/', activeMatch: '/vscode/' },
      { text: 'Zed', link: '/zed/', activeMatch: '/zed/' },
      { text: 'Python package', link: '/python/', activeMatch: '/python/' },
      { text: 'pixi', link: '/pixi/', activeMatch: '/pixi/' },
      { text: 'Reference', link: '/reference/configuration', activeMatch: '/reference/' },
      { text: 'Contributing', link: '/contributing/', activeMatch: '/contributing/' }
    ],

    sidebar: {
      '/guide/': [
        {
          text: 'Introduction',
          items: [
            { text: 'What is PyPilot?', link: '/guide/what-is-pypilot' },
            { text: 'Getting started', link: '/guide/getting-started' },
            { text: 'How it works', link: '/guide/how-it-works' },
            { text: 'Ecosystem & compatibility', link: '/guide/ecosystem' }
          ]
        },
        {
          text: 'Guides',
          items: [
            { text: 'GPU & CUDA', link: '/guide/gpu-cuda' },
            { text: 'Notebooks', link: '/guide/notebooks' },
            { text: 'Conda projects', link: '/guide/conda' },
            { text: 'Troubleshooting & FAQ', link: '/guide/troubleshooting' },
            { text: 'Security & release integrity', link: '/guide/security' },
            { text: 'Roadmap', link: '/guide/roadmap' }
          ]
        },
        {
          text: 'Elsewhere',
          items: [
            { text: 'pixi', link: '/pixi/' },
            { text: 'Engine CLI', link: '/engine/' }
          ]
        }
      ],
      '/engine/': [
        {
          text: 'Engine (CLI)',
          items: [
            { text: 'Overview', link: '/engine/' },
            { text: 'Command reference', link: '/engine/commands' },
            { text: 'Machine-readable output', link: '/engine/output' },
            { text: 'Dry runs', link: '/engine/dry-run' },
            { text: 'Logs, version & completions', link: '/engine/logs' },
            { text: 'Building from source', link: '/engine/building' }
          ]
        },
        {
          text: 'See also',
          items: [
            { text: 'Configuration', link: '/reference/configuration' },
            { text: 'What PyPilot reports', link: '/reference/checks' }
          ]
        }
      ],
      '/vscode/': [
        {
          text: 'VS Code',
          items: [
            { text: 'Overview', link: '/vscode/' },
            { text: 'Commands & tasks', link: '/vscode/commands' },
            { text: 'Settings', link: '/vscode/settings' }
          ]
        }
      ],
      '/zed/': [
        {
          text: 'Zed',
          items: [{ text: 'Zed extension', link: '/zed/' }]
        }
      ],
      '/python/': [
        {
          text: 'Python package',
          items: [
            { text: 'pypilot-cli', link: '/python/' },
            { text: 'Python API', link: '/python/api' }
          ]
        }
      ],
      '/pixi/': [
        {
          text: 'pixi',
          items: [
            { text: 'Why pixi', link: '/pixi/' },
            { text: 'PyPilot in pixi projects', link: '/pixi/projects' },
            { text: 'The "pixi can help" advisory', link: '/pixi/advisory' },
            { text: 'Migrating to pixi', link: '/pixi/migrate' }
          ]
        }
      ],
      '/reference/': [
        {
          text: 'Reference',
          items: [
            { text: 'Configuration', link: '/reference/configuration' },
            { text: 'What PyPilot reports', link: '/reference/checks' },
            { text: 'Data tables', link: '/reference/data-tables' },
            { text: 'LSP protocol', link: '/reference/lsp' },
            { text: 'Other editors', link: '/reference/editors' }
          ]
        }
      ],
      '/contributing/': [
        {
          text: 'Contributing',
          items: [{ text: 'How to contribute', link: '/contributing/' }]
        }
      ]
    },

    socialLinks: [{ icon: 'github', link: ORG, ariaLabel: 'pypilot-dev on GitHub' }],

    editLink: {
      pattern: `${ORG}/pypilot/edit/main/docs/:path`,
      text: 'Edit this page on GitHub'
    },

    lastUpdated: {
      text: 'Last updated',
      formatOptions: { dateStyle: 'medium' }
    },

    search: { provider: 'local' },

    outline: { level: [2, 3] },

    footer: {
      message:
        'Engine: AGPL-3.0-or-later. Zed and VS Code extensions: Apache-2.0. pypilot-cli: AGPL-3.0-or-later.',
      copyright: 'PyPilot contributors'
    }
  }
})

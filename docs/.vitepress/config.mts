import { defineConfig } from 'vitepress'

export default defineConfig({
  lang: 'en-US',
  title: 'demur',
  description:
    'BYOK adversarial AI code review: even granting every fact in the pull request, there is still no case for merging it.',
  base: '/',
  outDir: 'dist',
  cleanUrls: true,
  head: [
    ['link', { rel: 'icon', type: 'image/svg+xml', href: '/favicon.svg' }],
    ['meta', { name: 'theme-color', content: '#b4232b' }],
    ['meta', { property: 'og:type', content: 'website' }],
    ['meta', { property: 'og:title', content: 'demur' }],
    [
      'meta',
      {
        property: 'og:description',
        content:
          'An adversarial code reviewer that runs on your own provider key, caps its own spend, and argues why the pull request should not be merged.'
      }
    ]
  ],
  themeConfig: {
    logo: { light: '/logo.svg', dark: '/logo-dark.svg', alt: 'demur' },
    siteTitle: 'demur',
    nav: [
      { text: 'Guide', link: '/guide/setup', activeMatch: '/guide/' },
      { text: 'Reference', link: '/reference/configuration', activeMatch: '/reference/' },
      { text: 'Security', link: '/guide/security' }
    ],
    sidebar: [
      {
        text: 'Getting started',
        items: [
          { text: 'What demur is', link: '/guide/what-demur-is' },
          { text: 'Setup', link: '/guide/setup' },
          { text: 'Local CLI', link: '/guide/local-cli' }
        ]
      },
      {
        text: 'How it works',
        items: [
          { text: 'The review pipeline', link: '/guide/pipeline' },
          { text: 'Review rules', link: '/guide/rules' },
          { text: 'Context retrieval', link: '/guide/retrieval' },
          { text: 'Cost and budgets', link: '/guide/cost' },
          { text: 'The resume cache', link: '/guide/cache' },
          { text: 'Delta reviews', link: '/guide/delta-reviews' },
          { text: 'Providers', link: '/guide/providers' },
          { text: 'Security model', link: '/guide/security' }
        ]
      },
      {
        text: 'Reference',
        items: [
          { text: 'Configuration', link: '/reference/configuration' },
          { text: 'Editor schema', link: '/reference/schema' },
          { text: 'CLI', link: '/reference/cli' },
          { text: 'GitHub Action', link: '/reference/action' },
          { text: 'Troubleshooting', link: '/reference/troubleshooting' }
        ]
      }
    ],
    outline: [2, 3],
    search: { provider: 'local' },
    socialLinks: [{ icon: 'github', link: 'https://github.com/qaidvoid/demur' }],
    editLink: {
      pattern: 'https://github.com/qaidvoid/demur/edit/main/docs/:path',
      text: 'Edit this page'
    },
    footer: {
      message: 'MIT or Apache-2.0. No hosted service, no key custody, no metered billing.',
      copyright: 'Bring your own key.'
    },
    docFooter: { prev: 'Previous', next: 'Next' }
  }
})

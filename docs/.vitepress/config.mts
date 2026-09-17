import { defineConfig } from 'vitepress'

export default defineConfig({
  lang: 'en-US',
  title: 'demur',
  description:
    "BYOK adversarial AI code review: even granting every fact in the pull request, there is still no case for merging it.",
  base: '/',
  outDir: 'dist',
  themeConfig: {
    nav: [
      { text: 'Guide', link: '/guide/setup' },
      { text: 'Configuration', link: '/reference/configuration' },
      { text: 'Security', link: '/guide/security' }
    ],
    sidebar: [
      {
        text: 'Guide',
        items: [
          { text: 'Setup', link: '/guide/setup' },
          { text: 'Local CLI', link: '/guide/local-cli' },
          { text: 'Providers', link: '/guide/providers' },
          { text: 'Security model', link: '/guide/security' }
        ]
      },
      {
        text: 'Reference',
        items: [
          { text: 'Configuration', link: '/reference/configuration' }
        ]
      }
    ],
    outline: [2, 3]
  }
})

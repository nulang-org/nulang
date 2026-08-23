import { defineConfig } from 'astro/config';
import sitemap from '@astrojs/sitemap';
import starlight from '@astrojs/starlight';
import tailwindcss from '@astrojs/tailwind';

// https://starlight.astro.build/reference/configuration
export default defineConfig({
  site: 'https://nulang.org',
  head: [
    { tag: 'meta', attrs: { property: 'og:type', content: 'website' } },
    { tag: 'meta', attrs: { property: 'og:image', content: 'https://nulang.org/og-image.png' } },
    { tag: 'meta', attrs: { property: 'og:image:width', content: '1200' } },
    { tag: 'meta', attrs: { property: 'og:image:height', content: '630' } },
    { tag: 'meta', attrs: { name: 'twitter:card', content: 'summary_large_image' } },
    { tag: 'meta', attrs: { name: 'twitter:image', content: 'https://nulang.org/og-image.png' } },
  ],
  integrations: [sitemap({ filter: (page) => !page.endsWith('.md') && !page.endsWith('/llms-full.txt') }), starlight({
      title: 'Nulang',
      description: 'A distributed, actor-based programming language',
      defaultLocale: 'en',
      logo: {
        src: './src/assets/logo.svg',
        alt: 'Nulang',
      },
      favicon: '/favicon.svg',
      lastUpdated: true,
      customCss: ['./src/styles/custom.css'],
      components: {
        Footer: './src/components/Footer.astro',
        Header: './src/components/Header.astro',
        Head: './src/components/Head.astro',
        SiteTitle: './src/components/SiteTitle.astro',
        Hero: './src/components/Hero.astro',
        ThemeProvider: './src/components/ThemeProvider.astro',
        ThemeSelect: './src/components/ThemeSelect.astro',
      },
      editLink: {
        baseUrl: 'https://github.com/nulang-org/nulang/edit/main/docs/',
      },
      // Pagefind search (built-in with Starlight)
      // To migrate to Algolia, replace with:
      //   plugins: [starlightDocSearch({ appId: '...', apiKey: '...', indexName: '...' })]
      pagefind: true,
      expressiveCode: {
        shiki: {
          langAlias: { 'nulang': 'rust' },
        },
      },
      sidebar: [
        {
          label: 'Getting Started',
          collapsed: false,
          items: [
            { label: 'Installation', link: 'getting-started/installation/' },
            { label: 'Quick Start', link: 'getting-started/quick-start/' },
            { label: 'Editor Setup', link: 'getting-started/editor-setup/' },
          ],
        },
        {
          label: 'Tooling',
          collapsed: true,
          items: [
            { label: 'Language Server & Tooling', link: 'tooling/language-server/' },
          ],
        },
        {
          label: 'Language Syntax',
          collapsed: true,
          items: [
            { label: 'Syntax Basics', link: 'language/syntax/' },
            { label: 'Type System', link: 'language/types/' },
            { label: 'Algebraic Effects', link: 'language/effects/' },
            { label: 'Performance', link: 'language/performance/' },
            { label: 'Safety', link: 'language/safety/' },
          ],
        },
        {
          label: 'Distributed Actors',
          collapsed: true,
          items: [
            { label: 'Actor Model', link: 'actors/overview/' },
            { label: 'Distribution & Clustering', link: 'actors/distribution/' },
            { label: 'Supervision Trees', link: 'actors/supervision/' },
          ],
        },
        {
          label: 'Standard Library',
          collapsed: true,
          items: [
            { label: 'Overview', link: 'stdlib/overview/' },
            { label: 'IO', link: 'stdlib/io/' },
            { label: 'Int', link: 'stdlib/int/' },
            { label: 'Timer', link: 'stdlib/timer/' },
            { label: 'Signal', link: 'stdlib/signal/' },
            { label: 'LLM', link: 'stdlib/llm/' },
            { label: 'Actor', link: 'stdlib/actor/' },
            { label: 'Otp', link: 'stdlib/otp/' },
          ],
        },
        {
          label: 'AI Agents',
          collapsed: true,
          items: [
            { label: 'Overview', link: 'ai/overview/' },
            { label: 'Memory', link: 'ai/memory/' },
            { label: 'Multi-Agent Patterns', link: 'ai/multi-agent/' },
          ],
        },
        {
          label: 'Durable Workflows',
          collapsed: true,
          items: [
            { label: 'Overview', link: 'workflows/overview/' },
            { label: 'Signals, Timers & Queries', link: 'workflows/signals-timers/' },
          ],
        },
        {
          label: 'Why Nulang?',
          collapsed: false,
          items: [
            { label: 'Comparisons & Philosophy', link: 'blog/' },
          ],
        },
        {
          label: 'Reference',
          collapsed: true,
          items: [
            { label: 'API Reference', link: 'https://github.com/nulang-org/nulang/blob/main/docs/api.md' },
            { label: 'Changelog', link: 'https://github.com/nulang-org/nulang/blob/main/CHANGELOG.md' },
            { label: 'Governance', link: 'https://github.com/nulang-org/nulang/blob/main/GOVERNANCE.md' },
            { label: 'RFCs', link: 'https://github.com/nulang-org/nulang/tree/main/RFC' },
            { label: 'Language Spec', link: 'https://github.com/nulang-org/nulang/blob/main/SPEC2.md' },
          ],
        },
        {
          label: 'Legal',
          collapsed: true,
          items: [
            { label: 'Terms of Service', link: 'terms/' },
            { label: 'Privacy Policy', link: 'privacy/' },
            { label: 'Contact', link: 'contact/' },
          ],
        },
      ],
      social: [
        { icon: 'github', label: 'GitHub', href: 'https://github.com/nulang-org/nulang' },
      ],
    }),
    tailwindcss(),
  ],
});

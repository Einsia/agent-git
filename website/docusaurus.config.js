// @ts-check
// Docusaurus config for the agit documentation site.
// Two-locale (en, zh) docs-only site, deployed to GitHub Pages at einsia.github.io/agent-git.

import { themes as prismThemes } from 'prism-react-renderer';

/** @type {import('@docusaurus/types').Config} */
const config = {
  title: 'agit',
  tagline: 'Version control for AI coding-agent sessions',
  favicon: 'img/favicon.svg',

  future: {
    v4: true,
  },

  url: 'https://einsia.github.io',
  baseUrl: '/agent-git/',

  organizationName: 'Einsia',
  projectName: 'agent-git',
  trailingSlash: false,

  // Broken links fail the build: the site is the contract, so a dangling cross-link is a bug.
  onBrokenLinks: 'throw',

  markdown: {
    hooks: {
      onBrokenMarkdownLinks: 'throw',
    },
  },

  i18n: {
    defaultLocale: 'en',
    // zh-Hans (not bare zh) so Docusaurus applies its bundled Simplified-Chinese theme UI translations.
    locales: ['en', 'zh-Hans'],
    localeConfigs: {
      en: { label: 'English' },
      'zh-Hans': { label: '中文', htmlLang: 'zh-Hans' },
    },
  },

  presets: [
    [
      'classic',
      /** @type {import('@docusaurus/preset-classic').Options} */
      ({
        docs: {
          // Docs ARE the site: route them at the root, no /docs prefix.
          routeBasePath: '/',
          sidebarPath: './sidebars.js',
          editUrl: 'https://github.com/Einsia/agent-git/tree/main/website/',
        },
        blog: false,
        theme: {
          customCss: './src/css/custom.css',
        },
      }),
    ],
  ],

  themeConfig:
    /** @type {import('@docusaurus/preset-classic').ThemeConfig} */
    ({
      colorMode: {
        defaultMode: 'dark',
        respectPrefersColorScheme: true,
      },
      navbar: {
        title: 'agit',
        logo: {
          alt: 'agit',
          src: 'img/logo.svg',
        },
        items: [
          {
            type: 'docSidebar',
            sidebarId: 'docs',
            position: 'left',
            label: 'Docs',
          },
          {
            type: 'localeDropdown',
            position: 'right',
          },
          {
            href: 'https://github.com/Einsia/agent-git',
            label: 'GitHub',
            position: 'right',
          },
        ],
      },
      footer: {
        style: 'dark',
        links: [
          {
            title: 'Docs',
            items: [
              { label: 'Get started', to: '/' },
              { label: 'Using the CLI', to: '/cli/overview' },
              { label: 'Using the Hub', to: '/hub/overview' },
            ],
          },
          {
            title: 'More',
            items: [
              { label: 'GitHub', href: 'https://github.com/Einsia/agent-git' },
            ],
          },
        ],
        copyright: `agit docs. Built with Docusaurus.`,
      },
      prism: {
        theme: prismThemes.github,
        darkTheme: prismThemes.dracula,
        additionalLanguages: ['bash', 'toml', 'rust', 'json', 'diff', 'nginx', 'ini'],
      },
    }),
};

export default config;

import {themes as prismThemes} from 'prism-react-renderer';
import type {Config} from '@docusaurus/types';
import type * as Preset from '@docusaurus/preset-classic';

// This runs in Node.js - Don't use client-side code here (browser APIs, JSX...)

const config: Config = {
  title: 'Lest',
  tagline: 'Test, lest your code break.',
  favicon: 'img/lest-logo.png',

  future: {
    v4: true,
  },

  url: 'https://lest-luau.github.io',
  // GitHub Pages serves project sites under the repo name; a wrong baseUrl
  // yields a page with no CSS.
  baseUrl: '/lest/',

  organizationName: 'lest-luau',
  projectName: 'lest',

  onBrokenLinks: 'throw',

  // The guides are plain markdown, readable on GitHub. 'detect' parses .md as
  // CommonMark rather than MDX, so prose never needs escaping for JSX.
  markdown: {
    format: 'detect',
  },

  i18n: {
    defaultLocale: 'en',
    locales: ['en'],
  },

  presets: [
    [
      'classic',
      {
        docs: {
          // The guides stay at the repo root, next to the code they document;
          // the site consumes them in place. routeBasePath '/' makes them the
          // whole site (docs-only mode).
          path: '../docs',
          routeBasePath: '/',
          sidebarPath: './sidebars.ts',
          editUrl: 'https://github.com/lest-luau/lest/edit/main/docs/',
        },
        blog: false,
        theme: {
          customCss: './src/css/custom.css',
        },
      } satisfies Preset.Options,
    ],
  ],

  themeConfig: {
    colorMode: {
      respectPrefersColorScheme: true,
    },
    navbar: {
      title: 'Lest',
      items: [
        {
          href: 'https://github.com/lest-luau/lest',
          label: 'GitHub',
          position: 'right',
        },
      ],
    },
    footer: {
      style: 'dark',
      copyright: `Copyright © ${new Date().getFullYear()} lest-luau. MIT licensed.`,
    },
    prism: {
      theme: prismThemes.github,
      darkTheme: prismThemes.dracula,
      // Not bundled with prism-react-renderer. src/theme/prism-include-languages.ts
      // aliases the guides' ```luau and ```console fences onto lua and
      // shell-session.
      additionalLanguages: ['lua', 'toml', 'shell-session'],
    },
  } satisfies Preset.ThemeConfig,
};

export default config;

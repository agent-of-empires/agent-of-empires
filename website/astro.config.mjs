import { defineConfig } from 'astro/config';
import sitemap from '@astrojs/sitemap';

export default defineConfig({
  site: 'https://agent-of-empires.com',
  // The "cockpit" docs were renamed to "structured-view" (the web dashboard's
  // default structured view). Redirect the old URLs so external links and search
  // results keep working. Astro emits static meta-refresh pages for these
  // on `astro build`, which works on the GitHub Pages static host.
  redirects: {
    '/docs/cockpit/': '/docs/structured-view/',
    '/docs/cockpit/setup/': '/docs/structured-view/setup/',
    '/docs/cockpit/interface/': '/docs/structured-view/interface/',
    '/docs/cockpit/controls/': '/docs/structured-view/controls/',
    '/docs/cockpit/persistence/': '/docs/structured-view/persistence/',
    '/docs/cockpit/troubleshooting/': '/docs/structured-view/troubleshooting/',
    '/docs/cockpit/multi-agent/': '/docs/structured-view/multi-agent/',
  },
  integrations: [
    sitemap({
      changefreq: 'weekly',
      priority: 0.7,
    }),
  ],
});

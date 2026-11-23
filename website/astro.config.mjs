import { defineConfig } from 'astro/config';
import sitemap from '@astrojs/sitemap';
import tailwindcss from '@tailwindcss/vite';

export default defineConfig({
  site: 'https://www.agent-of-empires.com',
  vite: {
    plugins: [tailwindcss()],
  },
  // Removed or renamed docs pages.
  redirects: {
    '/docs/cockpit/': '/docs/structured-view/',
    '/docs/cockpit/setup/': '/docs/structured-view/',
    '/docs/cockpit/interface/': '/docs/structured-view/interface/',
    '/docs/cockpit/controls/': '/docs/structured-view/controls/',
    '/docs/cockpit/persistence/': '/docs/structured-view/',
    '/docs/cockpit/troubleshooting/': '/docs/structured-view/troubleshooting/',
    '/docs/cockpit/multi-agent/': '/docs/structured-view/',
    '/guides/podman/': '/guides/sandbox/',
    '/guides/apple-containers/': '/guides/sandbox/',
    '/guides/agent-override/': '/docs/guides/configuration/#agent-command-overrides',
    '/guides/web/diff/': '/guides/diff-view/',
    '/guides/web/settings/': '/guides/web/dashboard/#settings',
    '/guides/session-fork/': '/guides/session-resume/#forking-a-session',
    // Released setup wizards link here.
    '/docs/guides/web-dashboard/': '/guides/web-dashboard/',
  },
  integrations: [
    sitemap({
      changefreq: 'weekly',
      priority: 0.7,
    }),
  ],
});

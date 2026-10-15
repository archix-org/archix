// @ts-check
import { defineConfig } from 'astro/config';
import starlight from '@astrojs/starlight';

export default defineConfig({
	site: 'https://archix.dev',
	trailingSlash: 'ignore',
	integrations: [
		starlight({
			title: 'archix',
			description: 'Declarative, locked, rollback-able system configuration for Arch Linux and Arch-based distros.',
			logo: {
				dark: './src/assets/archix-lockup-dark.svg',
				light: './src/assets/archix-lockup-light.svg',
				replacesTitle: true,
			},
			favicon: '/favicon.svg',
			customCss: ['./src/styles/fonts.css', './src/styles/starlight.css'],
			expressiveCode: {
				defaultProps: { wrap: true },
				styleOverrides: {
					codeFontFamily: "'Martian Mono Variable', ui-monospace, monospace",
					codeFontSize: '0.78rem',
					codeLineHeight: '1.7',
				},
			},
			social: [
				{ icon: 'github', label: 'GitHub', href: 'https://github.com/archix-org/archix' },
				{ icon: 'discord', label: 'Discord', href: 'https://discord.gg/dejSeEHzpV' },
			],
			editLink: { baseUrl: 'https://github.com/archix-org/archix/edit/main/site/' },
			head: [
				{ tag: 'meta', attrs: { property: 'og:image', content: 'https://archix.dev/og.png' } },
				{ tag: 'meta', attrs: { name: 'twitter:card', content: 'summary_large_image' } },
			],
			sidebar: [
				{ label: 'Start here', items: ['docs', 'docs/installation', 'docs/quick-start'] },
				{ label: 'Concepts', items: [{ autogenerate: { directory: 'docs/concepts' } }] },
				{ label: 'Guides', items: [{ autogenerate: { directory: 'docs/guides' } }] },
				{ label: 'Reference', items: [{ autogenerate: { directory: 'docs/reference' } }] },
			],
		}),
	],
});

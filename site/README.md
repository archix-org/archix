# archix.dev

The archix website and documentation, built with [Astro](https://astro.build) and [Starlight](https://starlight.astro.build).

```sh
npm ci
npm run dev      # http://localhost:4321
npm run build    # static output in dist/
```

- Landing page: `src/pages/index.astro` and `src/components/`.
- Docs: `src/content/docs/docs/`, served under `/docs/`. Any change to archix's behaviour, CLI, config syntax or options should update the matching page in the same PR.
- Brand assets come from the archix logo set; colours and fonts are defined in `src/styles/site.css` (landing) and `src/styles/starlight.css` (docs).

Merges to `main` that touch `site/` build `ghcr.io/archix-org/site` and deploy it automatically.

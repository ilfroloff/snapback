# snapback-site

Landing page for [snapback](https://github.com/ilfroloff/snapback), built with
Astro. Lives at `website/` inside the main `snapback` repo and deploys to
`www.if-developer.fyi/snapback`.

## Run locally

```bash
cd website
npm install
npm run dev
```

## How the domain works

`www.if-developer.fyi` is configured as the custom domain on the
`ilfroloff.github.io` **user** site. This repo is a separate **project**
site with no custom domain of its own, so GitHub Pages automatically
serves it at `www.if-developer.fyi/snapback` — no extra DNS record needed.

**Don't** add a custom domain in this repo's Settings → Pages, and don't
commit a `CNAME` file under `website/public/`. Either one would tell
GitHub this project has its *own* domain, which overrides the inherited
path instead of nesting under it.

## Before your first deploy

1. **Add the demo GIF.** Generate it with the `vhs` tape from the main
   project's README, then drop the output at `website/public/demo.gif`.
   Until it's there, the demo panel shows a placeholder instead of a
   broken image.
2. **Enable Pages.** In the repo's Settings → Pages, set Source to
   "GitHub Actions." The workflow at `.github/workflows/deploy.yml`
   (repo root, not inside `website/`) handles the rest — it only
   triggers when something under `website/` changes.

## Structure

Everything lives in `src/pages/index.astro` — one file, scoped styles, no
component sprawl. `public/` holds the favicon set (already included) and
the demo GIF once you've added it.

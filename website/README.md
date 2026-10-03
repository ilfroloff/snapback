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

The demo GIF is already committed at `public/demo.gif`. To regenerate it
after a feature changes what a scene shows, run `npm run demo` from
`website/` (or `website/demo/record.sh`); it needs `vhs`, `ttyd`, `ffmpeg`
and a Rust toolchain, and is never part of `npm run build`.

1. **Enable Pages.** In the repo's Settings → Pages, set Source to
   "GitHub Actions." The workflow at `.github/workflows/deploy.yml`
   (repo root, not inside `website/`) handles the rest — it only
   triggers when something under `website/` changes.

## Structure

Everything lives in `src/pages/index.astro` — one file, scoped styles, no
component sprawl. `public/` holds the favicon set (already included) and
the committed demo GIF.

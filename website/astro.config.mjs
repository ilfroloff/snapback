import { defineConfig } from "astro/config";

export default defineConfig({
  // www.if-developer.fyi is configured as the custom domain on the
  // ilfroloff.github.io USER site. This repo (snapback) is a separate
  // PROJECT site with no custom domain of its own, so it automatically
  // inherits that domain at a subpath: www.if-developer.fyi/snapback.
  // Do not add a CNAME file here or set a custom domain in this repo's
  // Pages settings — either would override the inherited path instead
  // of nesting under it.
  site: "https://www.if-developer.fyi",
  base: "/snapback",
});

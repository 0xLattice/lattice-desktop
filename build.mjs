// Build the webview bundle into dist/ (Tauri's frontendDist). Everything is
// bundled from the pinned nexus-wallet submodule (and its pinned SDK): no
// remote code. Run: `node build.mjs` (add --watch to watch).
import * as esbuild from "esbuild";
import { cpSync, mkdirSync, rmSync } from "node:fs";

const watch = process.argv.includes("--watch");
const outdir = "dist";
const wallet = "vendor/nexus-wallet";

rmSync(outdir, { recursive: true, force: true });
mkdirSync(outdir, { recursive: true });
cpSync(`${wallet}/public/assets`, `${outdir}/assets`, { recursive: true });
cpSync(`${wallet}/src/popup/popup.css`, `${outdir}/popup.css`);
cpSync("src/index.html", `${outdir}/index.html`);
cpSync("src/desktop.css", `${outdir}/desktop.css`);

const ctx = await esbuild.context({
  entryPoints: { main: "src/main.ts" },
  outdir,
  bundle: true,
  format: "esm",
  target: "es2022",
  platform: "browser",
  legalComments: "eof",
  minify: !watch,
  sourcemap: watch,
});

if (watch) {
  await ctx.watch();
  console.log("watching…");
} else {
  await ctx.rebuild();
  await ctx.dispose();
  console.log("built -> dist/");
}

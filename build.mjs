// Build the webview bundle into dist/ (Tauri's frontendDist). Everything is
// bundled from the pinned nexus-wallet and lattice-swap-web submodules (and
// their pinned SDK): no remote code. lattice-swap-web's screens import their
// driver as `lattice-swap-driver`, here src/swap-driver over the shell's swap
// engine; its stylesheet is bundled into main.css. Run: `node build.mjs` (add --watch to watch).
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
  jsx: "automatic",
  jsxImportSource: "preact",
  alias: { "lattice-swap-driver": "./src/swap-driver/index.ts" },
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

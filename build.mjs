// Build the webview bundle into dist/ (Tauri's frontendDist). Everything is
// bundled from the pinned nexus-wallet and lattice-swap-web submodules (and
// their pinned SDK): no remote code. lattice-swap-web's screens import their
// driver as `lattice-swap-driver`, here src/swap-driver over the shell's swap
// engine, and its relay directory module src/swap-driver/directory.ts,
// which the shell reads; its stylesheet is bundled into main.css. Run:
// `node build.mjs` (add --watch to watch).
import * as esbuild from "esbuild";
import { cpSync, mkdirSync, rmSync } from "node:fs";
import { resolve } from "node:path";

const watch = process.argv.includes("--watch");
const outdir = "dist";
const wallet = "vendor/nexus-wallet";

rmSync(outdir, { recursive: true, force: true });
mkdirSync(outdir, { recursive: true });
cpSync(`${wallet}/public/assets`, `${outdir}/assets`, { recursive: true });
cpSync(`${wallet}/src/popup/popup.css`, `${outdir}/popup.css`);
cpSync("src/index.html", `${outdir}/index.html`);
cpSync("src/desktop.css", `${outdir}/desktop.css`);

/** lattice-swap-web's screens import its relay directory module
 * (`./directory`, `../directory`) as src/swap-driver/directory.ts, whose
 * read is the shell's: the page reaches no network. */
const shellDirectory = {
  name: "shell-directory",
  setup(build) {
    build.onResolve({ filter: /^\.\.?\/directory$/ }, ({ importer }) =>
      /lattice-swap-web[\\/]src[\\/]/.test(importer) ? { path: resolve("src/swap-driver/directory.ts") } : undefined);
  },
};

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
  plugins: [shellDirectory],
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

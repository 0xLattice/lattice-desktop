// lattice-swap-web's config.ts reads its build's settings off Vite's
// `import.meta.env` (`buildConfig`, which the desktop never calls).
interface ImportMeta {
  readonly env: Record<string, string>;
}

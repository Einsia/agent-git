import path from "node:path"
import react from "@vitejs/plugin-react"
import tailwindcss from "@tailwindcss/vite"
import { defineConfig } from "vite"

// Build to fixed, UNHASHED asset names so the Rust hub can embed/serve them deterministically
// (router.rs serves the known set from `dist/assets`). The app is CODE-SPLIT: the entry stays small and
// the heavy, lazily-used vendors live in their own chunks that only load with the routes that need them.
export default defineConfig({
  plugins: [react(), tailwindcss()],
  resolve: {
    alias: { "@": path.resolve(__dirname, "./src") },
  },
  build: {
    outDir: "dist",
    cssCodeSplit: false, // one app.css, as before (the hub serves a single stylesheet)
    assetsInlineLimit: 100_000_000, // inline binary assets (fonts/images) into the JS; only .js/.css emit
    rollupOptions: {
      output: {
        // Deterministic, hash-free names: the entry is always app.js; every other chunk is named after
        // its manualChunks group or its lazy module, so router.rs can serve a known, stable set.
        entryFileNames: "assets/app.js",
        chunkFileNames: "assets/[name].js",
        assetFileNames: "assets/app.[ext]",
        manualChunks(id) {
          if (!id.includes("node_modules")) return // app code: the entry + the lazy route chunks
          // The markdown toolchain (react-markdown + remark-gfm and their micromark/mdast/hast/unist
          // subtree) is only reached through the lazily-loaded Session/Diff/MrDetail pages, so isolating
          // it into its own chunk keeps it OUT of the initial bundle — it loads on demand with them.
          if (
            /[\\/]node_modules[\\/](react-markdown|remark-|micromark|mdast|hast|unist|unified|vfile|property-information|hastscript|character-entities|decode-named-character-reference|estree|devlop|mdurl|trim-lines|zwitch|longest-streak|markdown-table|ccount|escape-string-regexp|bail|trough|is-plain-obj|space-separated-tokens|comma-separated-tokens|html-url-attributes|web-namespaces)/.test(
              id
            )
          ) {
            return "markdown-vendor"
          }
          // The window virtualizer is likewise only used by the transcript inside the lazy Session page.
          if (id.includes("@tanstack")) return "virtual-vendor"
          // React, the router, and the remaining small libraries are needed by the entry itself, so they
          // ride in one eager vendor chunk — split out of app.js purely to shrink the entry chunk.
          return "react-vendor"
        },
      },
    },
  },
})

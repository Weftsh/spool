/// <reference types="vitest/config" />
import { fileURLToPath } from "node:url";
import { defineConfig, type Plugin } from "vite";
import react from "@vitejs/plugin-react";
import tailwindcss from "@tailwindcss/vite";

/// Serve the SPA for `/{owner}` and `/{owner}/{repo}/…`, the way
/// `stratum-server`'s asset fallback does.
///
/// Vite only serves under `base`, so without this a public repository
/// address 404s in `vite preview` — and `vite preview` is what the
/// Playwright suite runs against. Every public-route spec would fail for
/// a reason that has nothing to do with the code under test, which is
/// the worst kind of red: it looks like the feature is broken.
///
/// The rule is deliberately the same shape as the server's: anything
/// under `base` or vite's own prefixes is left alone, and everything
/// else is the SPA if its **first** segment could be a namespace.
///
/// First segment, not last, and that distinction was a real bug. The
/// rule used to be "a dot anywhere in the last segment means somebody
/// asked for a file", which is true of `/favicon.ico` and false of
/// `/acme/widget/tree/README.md` — a file *inside a repository*, whose
/// address ends in `.md` and which the server serves the SPA for
/// without hesitating. `forge_spa` in `crates/stratum-server/src/
/// webassets.rs` never looks at the extension at all: it asks whether
/// the first segment is a well-formed, unreserved namespace, and
/// `valid_name` refuses a dot, so `/favicon.ico` falls out for the
/// right reason and a path inside a repository does not.
///
/// Nothing caught it because no spec had ever opened a file page at a
/// public address — the file browser was reached through the dashboard
/// mount, which is under `base` and never touched this middleware. The
/// moment a repository had one address, twenty-six specs went red here
/// for a reason that had nothing to do with the code under test. That
/// is the "passes here, fails there" family from CLAUDE.md with the
/// sides swapped: the harness was wrong about the server, so the
/// product looked broken while the server was right.
function ownerFallback(): Plugin {
  const spa = "/dashboard/index.html";
  const middleware = (
    req: { url?: string },
    _res: unknown,
    next: () => void,
  ) => {
    const path = (req.url ?? "/").split("?")[0];
    const first = path.split("/").filter(Boolean)[0] ?? "";
    const untouched =
      path.startsWith("/dashboard") ||
      path.startsWith("/@") ||
      path.startsWith("/node_modules") ||
      path.startsWith("/src/") ||
      path.startsWith("/v1/") ||
      // Not a name anybody could have, so not a forge address: an asset
      // at the root (`/favicon.ico`), or nothing at all. Answering an
      // image request with an HTML page is how you get a broken-image
      // icon and no idea why.
      first === "" ||
      /\./.test(first);
    if (!untouched) req.url = spa;
    next();
  };
  return {
    name: "stratum-owner-fallback",
    configureServer(server) {
      server.middlewares.use(middleware);
    },
    configurePreviewServer(server) {
      server.middlewares.use(middleware);
    },
  };
}

export default defineConfig({
  // Served by stratum-server at /dashboard/.
  base: "/dashboard/",
  resolve: {
    alias: { "@": fileURLToPath(new URL("./src", import.meta.url)) },
  },
  plugins: [react(), tailwindcss(), ownerFallback()],
  // The highlighting worker (`src/code/surface.tsx`) imports grammars on
  // demand. Vite's default worker format is an iife, which cannot split,
  // so it would inline every grammar into one multi-megabyte file; as an
  // ES module worker it is one small asset plus one chunk per grammar,
  // fetched as file types turn up. `tests/bundle-budget.setup.ts` checks
  // the worker really is a separate asset.
  worker: { format: "es" },
  server: {
    proxy: {
      "/v1": "http://127.0.0.1:8080",
    },
  },
  test: {
    // Playwright specs live in tests/; vitest owns src/ only.
    include: ["src/**/*.test.ts"],
  },
});

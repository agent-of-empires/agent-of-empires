// Pre-React theme bootstrap. Reads the cached ResolvedTheme payload
// from localStorage and applies its CSS variables on documentElement
// before React mounts. Prevents a flash of the default Empire palette
// when the user has picked a different theme. Safe to fail silently:
// useResolvedTheme will refetch and reapply once React boots.
//
// Lives as a static file (not inline in index.html) because the
// dashboard's CSP is `script-src 'self' 'wasm-unsafe-eval'` with no
// `unsafe-inline` / nonce / hash, so the browser would refuse to run
// an inline <script>. Referenced from web/index.html via
// `<script src="/theme-bootstrap.js"></script>`. See #1189.
(function () {
  try {
    var raw = localStorage.getItem("aoe-resolved-theme");
    if (!raw) return;
    var theme = JSON.parse(raw);
    var root = document.documentElement;
    var apply = function (vars) {
      if (!vars) return;
      for (var k in vars) {
        if (Object.prototype.hasOwnProperty.call(vars, k)) {
          root.style.setProperty(k, vars[k]);
        }
      }
    };
    apply(theme && theme.web && theme.web.cssVars);
    apply(theme && theme.terminal && theme.terminal.cssVars);
    if (theme && theme.name) root.dataset.theme = theme.name;
    if (theme && theme.appearance) {
      root.dataset.themeAppearance = theme.appearance;
      root.style.colorScheme = theme.appearance;
    }
  } catch (_) {
    // Quota / parse error; fall through to React-side fetch.
  }
})();

export type Theme = "light" | "dark" | "system";

const KEY = "rivendell:theme";

export function storedTheme(): Theme {
  const v = localStorage.getItem(KEY);
  return v === "dark" || v === "light" || v === "system" ? v : "light";
}

export function resolve(theme: Theme): "light" | "dark" {
  if (theme !== "system") return theme;
  return window.matchMedia("(prefers-color-scheme: dark)").matches ? "dark" : "light";
}

export function apply(theme: Theme) {
  document.documentElement.dataset.theme = resolve(theme);
}

export function setTheme(theme: Theme) {
  localStorage.setItem(KEY, theme);
  apply(theme);
}

/**
 * Runs before React mounts. The CSP forbids inline scripts, so index.html
 * carries the light default statically and this corrects it here — early
 * enough that a dark-mode user sees at most a single frame of light.
 */
export function init() {
  apply(storedTheme());
  window
    .matchMedia("(prefers-color-scheme: dark)")
    .addEventListener("change", () => {
      if (storedTheme() === "system") apply("system");
    });
}

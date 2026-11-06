export type Appearance = "dark" | "light" | "system";

export function savedAppearance(): Appearance {
  const saved = localStorage.getItem("twr.theme");
  return saved === "light" || saved === "system" ? saved : "dark";
}

export function initializeAppearance() {
  document.documentElement.dataset.theme = savedAppearance();
}

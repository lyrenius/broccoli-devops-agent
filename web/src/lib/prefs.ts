import { useCallback, useEffect, useState } from "react";

export type Theme = "light" | "dark";

/** Theme handling that matches Broccoli's: a `light`/`dark` class on <html>, persisted as `theme`. */
export function useTheme(): [Theme, () => void] {
  const [theme, setTheme] = useState<Theme>(() => {
    try {
      const saved = localStorage.getItem("theme");
      if (saved === "light" || saved === "dark") return saved;
    } catch {
      // storage unavailable
    }
    return window.matchMedia("(prefers-color-scheme: dark)").matches ? "dark" : "light";
  });
  useEffect(() => {
    const root = document.documentElement;
    root.classList.remove("light", "dark");
    root.classList.add(theme);
    try {
      localStorage.setItem("theme", theme);
    } catch {
      // storage unavailable
    }
  }, [theme]);
  const toggle = useCallback(() => setTheme((t) => (t === "light" ? "dark" : "light")), []);
  return [theme, toggle];
}

/** The operator's name, recorded with every decision; kept per browser. */
export function loadOperator(): string {
  try {
    return localStorage.getItem("broccoli.operator") ?? "operator";
  } catch {
    return "operator";
  }
}

export function useOperator(): [string, (name: string) => void] {
  const [name, setName] = useState(loadOperator);
  useEffect(() => {
    try {
      localStorage.setItem("broccoli.operator", name);
    } catch {
      // storage unavailable
    }
  }, [name]);
  return [name, setName];
}

/** Relative age of an ISO timestamp, e.g. "3 min ago". */
export function ageOf(iso: string, now = Date.now()): string {
  const seconds = Math.max(0, Math.round((now - new Date(iso).getTime()) / 1000));
  if (seconds < 5) return "just now";
  if (seconds < 90) return `${seconds}s ago`;
  if (seconds < 5400) return `${Math.round(seconds / 60)} min ago`;
  if (seconds < 172800) return `${Math.round(seconds / 3600)} h ago`;
  return `${Math.round(seconds / 86400)} d ago`;
}

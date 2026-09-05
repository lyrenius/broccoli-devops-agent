import { createContext, useCallback, useContext, useEffect, useMemo, useState, type ReactNode } from "react";
import { en, type Key } from "./en";
import { zhCN, zhStatus } from "./zh-CN";

export type Locale = "en" | "zh-CN";
export const LOCALES: Locale[] = ["en", "zh-CN"];

const DICTS: Record<Locale, Record<Key, string>> = { en, "zh-CN": zhCN };
const STORAGE_KEY = "broccoli.locale";

function storedLocale(): Locale | null {
  try {
    const saved = localStorage.getItem(STORAGE_KEY);
    return saved === "en" || saved === "zh-CN" ? saved : null;
  } catch {
    return null;
  }
}

function browserLocale(): Locale {
  return navigator.language.toLowerCase().startsWith("zh") ? "zh-CN" : "en";
}

interface LocaleValue {
  locale: Locale;
  setLocale: (locale: Locale) => void;
  /** Whether the viewer chose a language explicitly (as opposed to a default). */
  explicit: boolean;
}

const LocaleContext = createContext<LocaleValue | null>(null);

/** The console's language: the viewer's stored choice, else the agent's configured language
 *  once known, else the browser's. Changing it here changes only the console; the agent keeps
 *  writing in the language its config named. */
export function LocaleProvider({ agentLanguage, children }: { agentLanguage: string | null; children: ReactNode }) {
  const [explicit, setExplicit] = useState(() => storedLocale() !== null);
  const [locale, setLocaleState] = useState<Locale>(() => storedLocale() ?? browserLocale());

  useEffect(() => {
    if (!explicit && agentLanguage && LOCALES.includes(agentLanguage as Locale)) {
      setLocaleState(agentLanguage as Locale);
    }
  }, [agentLanguage, explicit]);

  useEffect(() => {
    document.documentElement.lang = locale;
  }, [locale]);

  const setLocale = useCallback((next: Locale) => {
    setExplicit(true);
    setLocaleState(next);
    try {
      localStorage.setItem(STORAGE_KEY, next);
    } catch {
      // storage unavailable; the choice still holds for this session
    }
  }, []);

  const value = useMemo(() => ({ locale, setLocale, explicit }), [locale, setLocale, explicit]);
  return <LocaleContext value={value}>{children}</LocaleContext>;
}

export function useLocale(): LocaleValue {
  const value = useContext(LocaleContext);
  if (!value) throw new Error("useLocale must be used inside LocaleProvider");
  return value;
}

type Params = Record<string, string | number>;

function interpolate(template: string, params?: Params): string {
  if (!params) return template;
  return template.replace(/\{(\w+)\}/g, (_, name: string) => (name in params ? String(params[name]) : `{${name}}`));
}

/** Translation helpers bound to the current locale. */
export function useT() {
  const { locale } = useLocale();
  return useMemo(() => {
    const dict = DICTS[locale];
    const t = (key: Key, params?: Params) => interpolate(dict[key] ?? en[key], params);
    /** Display name of an enum value from the API (status, health, mode, priority, cause). */
    const status = (value: string) => (locale === "zh-CN" ? (zhStatus[value] ?? value.replace(/_/g, " ")) : value.replace(/_/g, " "));
    const age = (iso: string, now = Date.now()) => {
      const seconds = Math.max(0, Math.round((now - new Date(iso).getTime()) / 1000));
      if (seconds < 5) return t("age.justNow");
      if (seconds < 90) return t("age.seconds", { n: seconds });
      if (seconds < 5400) return t("age.minutes", { n: Math.round(seconds / 60) });
      if (seconds < 172800) return t("age.hours", { n: Math.round(seconds / 3600) });
      return t("age.days", { n: Math.round(seconds / 86400) });
    };
    const dateTime = (iso: string) => new Date(iso).toLocaleString(locale);
    const time = (iso: string) => new Date(iso).toLocaleTimeString(locale);
    return { t, status, age, dateTime, time, locale };
  }, [locale]);
}

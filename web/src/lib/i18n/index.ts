/**
 * Minimal local localization boundary for the Bamep Presentation shell.
 *
 * Deliberately small: a typed flat catalog plus a translator factory. The
 * boundary is framework-free so it is unit-testable without SvelteKit.
 *
 * `Locale` is the set of locales this build actually ships a catalog for —
 * currently only `pt-BR`. Adding `en-US` later means: add `locales/en-US.ts`
 * (typed `satisfies Record<MessageKey, string>`), register it in `catalogs`,
 * and extend the `Locale` union and `supportedLocales`. Call sites do not
 * change.
 */
import { ptBR } from './locales/pt-BR';

export type Locale = 'pt-BR';

export type MessageKey = keyof typeof ptBR;

export const defaultLocale: Locale = 'pt-BR';

export const supportedLocales: readonly Locale[] = ['pt-BR'];

const catalogs: Record<Locale, Record<MessageKey, string>> = {
	'pt-BR': ptBR
};

export type Translate = (key: MessageKey) => string;

/**
 * Build a translator for `locale`. Falls back to the default locale's catalog
 * for an invalid runtime value, and to the key itself for a missing message so
 * a gap is visible rather than silently blank.
 */
export function createTranslator(locale: Locale = defaultLocale): Translate {
	const catalog = catalogs[locale] ?? catalogs[defaultLocale];
	return (key: MessageKey): string => catalog[key] ?? key;
}

/** Default translator bound to the initial rendered locale. */
export const t: Translate = createTranslator(defaultLocale);

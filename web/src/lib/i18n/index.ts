/**
 * Minimal local localization boundary for the Bamep Presentation client.
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

/** Named `{placeholder}` values substituted into a message template. */
export type MessageParams = Record<string, string | number>;

export const defaultLocale: Locale = 'pt-BR';

export const supportedLocales: readonly Locale[] = ['pt-BR'];

const catalogs: Record<Locale, Record<MessageKey, string>> = {
	'pt-BR': ptBR
};

export type Translate = (key: MessageKey, params?: MessageParams) => string;

const PLACEHOLDER = /\{(\w+)\}/g;

/**
 * Build a translator for `locale`. Falls back to the default locale's catalog
 * for an invalid runtime value, and to the key itself for a missing message so
 * a gap is visible rather than silently blank. `{name}` placeholders in the
 * message are replaced from `params`; an unmatched placeholder is left intact.
 */
export function createTranslator(locale: Locale = defaultLocale): Translate {
	const catalog = catalogs[locale] ?? catalogs[defaultLocale];
	return (key: MessageKey, params?: MessageParams): string => {
		const template = catalog[key] ?? key;
		if (!params) return template;
		return template.replace(PLACEHOLDER, (whole, name: string) =>
			name in params ? String(params[name]) : whole
		);
	};
}

/** Default translator bound to the initial rendered locale. */
export const t: Translate = createTranslator(defaultLocale);

/**
 * Pick the singular or plural message for `count` and pass `{ count }` through.
 * Portuguese plural phrases inflect the noun and often the verb, so a real
 * catalog entry per form is clearer than a runtime rule.
 */
export function tCount(
	translate: Translate,
	oneKey: MessageKey,
	otherKey: MessageKey,
	count: number
): string {
	return translate(count === 1 ? oneKey : otherKey, { count });
}

import { describe, expect, it } from 'vitest';
import { createTranslator, defaultLocale, supportedLocales, t } from './index';

describe('localization boundary', () => {
	it('ships pt-BR as the only supported locale', () => {
		expect(defaultLocale).toBe('pt-BR');
		expect([...supportedLocales]).toEqual(['pt-BR']);
	});

	it('renders pt-BR shell strings', () => {
		const translate = createTranslator('pt-BR');
		expect(translate('app.tagline')).toBe('Console do operador');
		expect(translate('nav.endpoints')).toBe('Endpoints');
		expect(translate('nav.operations')).toBe('Operações');
		expect(translate('nav.attention')).toBe('Atenção');
		expect(translate('nav.settings')).toBe('Configurações');
	});

	it('exposes a default translator bound to pt-BR', () => {
		expect(t('route.placeholderNotice')).toBe(
			'O conteúdo desta área ainda não está disponível.'
		);
	});

	it('falls back to the message key when a message is missing', () => {
		const translate = createTranslator('pt-BR');
		// @ts-expect-error - exercising the missing-key path
		expect(translate('nav.doesNotExist')).toBe('nav.doesNotExist');
	});
});

/**
 * Brazilian Portuguese catalog.
 *
 * `pt-BR` is the initial rendered locale (Product Specification
 * `m0-stack-and-boundaries-baseline.md` "Localization"). Only the strings this
 * foundation WP renders are catalogued here. A future `en-US` catalog adds a
 * sibling file with the same keys; component call sites do not change.
 */
export const ptBR = {
	'app.brand': 'Bamep',
	'app.tagline': 'Console do operador',
	'app.environment': 'Frota simulada · demonstração',

	'shell.primaryNavLabel': 'Navegação principal',
	'shell.skipToContent': 'Pular para o conteúdo',

	'nav.sectionFleet': 'Frota',
	'nav.endpoints': 'Endpoints',
	'nav.operations': 'Operações',
	'nav.attention': 'Atenção',
	'nav.settings': 'Configurações',

	'endpoints.title': 'Endpoints',
	'endpoints.lead': 'Inventário e situação operacional da frota de Endpoints.',

	'operations.title': 'Operações',
	'operations.lead': 'Operações enviadas pelo operador e seu acompanhamento.',

	'attention.title': 'Atenção',
	'attention.lead': 'Endpoints e resultados que precisam de revisão do operador.',

	'settings.title': 'Configurações',
	'settings.lead': 'Preferências do console do operador.',

	'route.placeholderNotice': 'O conteúdo desta área ainda não está disponível.'
} as const;

export type PtBrCatalog = typeof ptBR;

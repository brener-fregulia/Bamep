/**
 * Brazilian Portuguese catalog.
 *
 * `pt-BR` is the initial rendered locale (Product Specification
 * `m0-stack-and-boundaries-baseline.md` "Localization"). Only the strings the
 * implemented UI renders are catalogued here. A future `en-US` catalog adds a
 * sibling file with the same keys; component call sites do not change.
 *
 * `{name}` tokens are interpolation placeholders resolved at call time.
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
	'endpoints.subtitle':
		'Frota simulada de demonstração · {count} Endpoints · dados apenas para avaliação de UX',
	'endpoints.tableCaption':
		'Frota de Endpoints simulados, com situação, atividade atual, hardware e último contato',
	'endpoints.footNote':
		'Dados simulados — não conectado a uma frota real. Seleção interativa para avaliação.',

	'endpoints.column.endpoint': 'Endpoint',
	'endpoints.column.situation': 'Situação',
	'endpoints.column.activity': 'Atividade atual',
	'endpoints.column.hardware': 'Hardware',
	'endpoints.column.lastContact': 'Último contato',

	'endpoints.bench': 'bancada {code}',

	'endpoints.situation.available': 'Disponível',
	'endpoints.situation.working': 'Em operação',
	'endpoints.situation.pendingEnrollment': 'Inclusão pendente',
	'endpoints.situation.attention': 'Requer atenção',
	'endpoints.situation.notReady': 'Não pronto',
	'endpoints.situation.unavailable': 'Sem contato',

	'endpoints.activity.capturingImage': 'Capturar imagem',
	'endpoints.activity.preparingOperation': 'Preparando operação',
	'endpoints.activity.none': '—',
	'endpoints.detail.pendingEnrollment': 'Aguardando aprovação de inclusão',
	'endpoints.detail.notReady': 'Não elegível para uma nova operação agora',
	'endpoints.attention.uncertainResult': 'Resultado incerto',
	'endpoints.attention.uncertainResultHint':
		'operação anterior terminou sem confirmação',

	'endpoints.contact.now': 'agora',
	'endpoints.contact.minutesAgo': 'há {count} min',

	'endpoints.select.all': 'Selecionar todos os Endpoints',
	'endpoints.select.row': 'Selecionar {id}',

	'endpoints.selection.regionLabel': 'Resumo da seleção',
	'endpoints.selection.badge.one': '{count} selecionado',
	'endpoints.selection.badge.other': '{count} selecionados',
	'endpoints.selection.heading.one': '{count} Endpoint selecionado',
	'endpoints.selection.heading.other': '{count} Endpoints selecionados',
	'endpoints.selection.ready.one': '{count} pronto',
	'endpoints.selection.ready.other': '{count} prontos',
	'endpoints.selection.attention.one': '{count} requer atenção',
	'endpoints.selection.attention.other': '{count} requerem atenção',
	'endpoints.selection.other.one': '{count} não pronto',
	'endpoints.selection.other.other': '{count} não prontos',
	'endpoints.selection.note':
		'A elegibilidade é confirmada na próxima etapa. Selecionar não garante que todos os Endpoints sejam aceitos para a operação.',
	'endpoints.selection.clear': 'Limpar seleção',

	'endpoints.newOperation': 'Nova operação',

	'operations.title': 'Operações',
	'operations.lead': 'Operações enviadas pelo operador e seu acompanhamento.',

	'operationsNew.title': 'Nova operação',
	'operationsNew.lead':
		'A configuração da operação e a confirmação dos Endpoints selecionados serão feitas aqui.',

	'attention.title': 'Atenção',
	'attention.lead': 'Endpoints e resultados que precisam de revisão do operador.',

	'settings.title': 'Configurações',
	'settings.lead': 'Preferências do console do operador.',

	'route.placeholderNotice': 'O conteúdo desta área ainda não está disponível.'
} as const;

export type PtBrCatalog = typeof ptBR;

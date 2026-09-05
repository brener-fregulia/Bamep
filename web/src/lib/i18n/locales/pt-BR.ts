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
	'operationsNew.lead': 'Configure o serviço para os Endpoints selecionados antes de revisar.',

	'operationsNew.steps.label': 'Etapas do fluxo',
	'operationsNew.steps.configure': 'Configurar',
	'operationsNew.steps.review': 'Revisar',

	'operationsNew.guard.heading': 'Nenhum Endpoint válido selecionado',
	'operationsNew.guard.body':
		'Volte à lista de Endpoints e selecione ao menos um Endpoint para configurar uma nova operação.',
	'operationsNew.backToEndpoints': 'Voltar para Endpoints',

	'operationsNew.intent.title': 'Tipo de operação',
	'operationsNew.intent.reinstallWindows': 'Reinstalar Windows',
	'operationsNew.intent.reinstallWindowsDesc':
		'Reinstala o Windows nos Endpoints selecionados. Instalação limpa por padrão; os detalhes de execução são resolvidos pelo Bamep.',

	'operationsNew.common.title': 'Configuração comum',
	'operationsNew.common.scope': 'Todos os alvos',
	'operationsNew.common.reinstall': 'Reinstalação do Windows',
	'operationsNew.common.reinstallHint': 'É o serviço desta operação e não pode ser removida aqui.',
	'operationsNew.common.included': 'Incluída no serviço',
	'operationsNew.common.drivers': 'Instalar drivers',
	'operationsNew.common.driversHint': 'Após a reinstalação, conforme o hardware de cada Endpoint.',
	'operationsNew.common.driversToggle': 'Instalar drivers em todos os alvos',

	'operationsNew.adjust.title': 'Ajustes por Endpoint',
	'operationsNew.adjust.scope': 'Somente diferenças',
	'operationsNew.adjust.hint': 'Apenas o que muda em relação à configuração comum.',
	'operationsNew.adjust.none': 'Sem ajustes — segue a configuração comum.',
	'operationsNew.adjust.preserveRestore': 'Preservar e restaurar os dados do usuário',
	'operationsNew.adjust.preserveRestoreHint':
		'Os dados são preservados antes da reinstalação e restaurados ao final.',
	'operationsNew.adjust.preserveRestoreToggle':
		'Preservar e restaurar os dados do usuário em {id}',
	'operationsNew.adjust.debloat': 'Aplicar o debloat configurado',
	'operationsNew.adjust.debloatHint':
		'Remove aplicativos desnecessários conforme o perfil já configurado.',
	'operationsNew.adjust.debloatToggle': 'Aplicar o debloat configurado em {id}',
	'operationsNew.adjust.adjusted.one': '{count} Endpoint com ajustes',
	'operationsNew.adjust.adjusted.other': '{count} Endpoints com ajustes',
	'operationsNew.adjust.commonOnly.one': '{count} segue apenas a configuração comum',
	'operationsNew.adjust.commonOnly.other': '{count} seguem apenas a configuração comum',
	'operationsNew.adjust.allCommon': 'Todos os Endpoints seguem apenas a configuração comum',

	'operationsNew.targets.title': 'Alvos da operação',
	'operationsNew.targets.deltaCommon': 'Configuração comum',
	'operationsNew.targets.deltaPreserve': 'Comum + preservação de dados',
	'operationsNew.targets.deltaDebloat': 'Comum + debloat',
	'operationsNew.targets.note':
		'Selecionar um Endpoint não resolve a condição indicada. A aceitação de cada Endpoint não é garantida.',

	'operationsNew.attention.title': 'Atenção antes de continuar',
	'operationsNew.attention.uncertain':
		'{id} possui um resultado anterior ainda incerto — configurar esta operação não resolve a condição.',
	'operationsNew.attention.notReady': '{id} não está pronto para uma nova operação neste momento.',
	'operationsNew.attention.eligibility':
		'A elegibilidade de cada Endpoint é verificada antes de a operação prosseguir.',

	'operationsNew.review.cta': 'Revisar operação',
	'operationsNew.review.next': 'Próxima etapa: revisar a operação',
	'operationsNew.review.placeholder':
		'A revisão será implementada na próxima etapa. Nada foi enviado ou executado; a configuração permanece apenas neste console.',

	'attention.title': 'Atenção',
	'attention.lead': 'Endpoints e resultados que precisam de revisão do operador.',

	'settings.title': 'Configurações',
	'settings.lead': 'Preferências do console do operador.',

	'route.placeholderNotice': 'O conteúdo desta área ainda não está disponível.'
} as const;

export type PtBrCatalog = typeof ptBR;

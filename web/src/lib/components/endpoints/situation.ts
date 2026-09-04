/**
 * Presentation mapping and selection math for the Endpoints list. Pure, so it
 * is unit-testable without a DOM.
 */
import type { MessageKey } from '$lib/i18n';
import type { EndpointActivity, EndpointSituation, FleetEndpoint } from '$lib/fixtures/endpoints';

export type SituationTone = 'ok' | 'work' | 'enroll' | 'attention' | 'muted' | 'offline';

export interface SituationMeta {
	labelKey: MessageKey;
	tone: SituationTone;
}

const SITUATION_META: Record<EndpointSituation, SituationMeta> = {
	available: { labelKey: 'endpoints.situation.available', tone: 'ok' },
	working: { labelKey: 'endpoints.situation.working', tone: 'work' },
	'pending-enrollment': { labelKey: 'endpoints.situation.pendingEnrollment', tone: 'enroll' },
	attention: { labelKey: 'endpoints.situation.attention', tone: 'attention' },
	'not-ready': { labelKey: 'endpoints.situation.notReady', tone: 'muted' },
	unavailable: { labelKey: 'endpoints.situation.unavailable', tone: 'offline' }
};

export function situationMeta(situation: EndpointSituation): SituationMeta {
	return SITUATION_META[situation];
}

const ACTIVITY_LABEL: Record<EndpointActivity, MessageKey> = {
	'capturing-image': 'endpoints.activity.capturingImage',
	'preparing-operation': 'endpoints.activity.preparingOperation'
};

export function activityLabelKey(activity: EndpointActivity): MessageKey {
	return ACTIVITY_LABEL[activity];
}

export interface SelectionSummary {
	total: number;
	/** Selected Endpoints currently `available`. */
	ready: number;
	/** Selected Endpoints currently needing attention. */
	attention: number;
	/** Every other selected Endpoint (working, pending, not-ready, unavailable). */
	other: number;
}

export function summarizeSelection(selected: readonly FleetEndpoint[]): SelectionSummary {
	let ready = 0;
	let attention = 0;
	let other = 0;
	for (const endpoint of selected) {
		if (endpoint.situation === 'available') ready += 1;
		else if (endpoint.situation === 'attention') attention += 1;
		else other += 1;
	}
	return { total: selected.length, ready, attention, other };
}

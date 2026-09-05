import { describe, expect, it } from 'vitest';
import { fleet } from '$lib/fixtures/endpoints';
import { planTargets } from './configuration';

const byId = (id: string) => {
	const endpoint = fleet.find((candidate) => candidate.id === id);
	if (!endpoint) throw new Error(`missing fixture ${id}`);
	return endpoint;
};

describe('representative mock configuration scenario', () => {
	it('seeds the accepted per-Endpoint differences for the acceptance target set', () => {
		const plans = planTargets([byId('LAB-03'), byId('LAB-07'), byId('LAB-09')]);
		expect(plans.map((plan) => [plan.endpoint.id, plan.adjustment])).toEqual([
			['LAB-03', 'preserve-restore-data'],
			['LAB-07', 'apply-debloat'],
			['LAB-09', undefined]
		]);
	});

	it('keeps every other target on the common configuration only', () => {
		const plans = planTargets([byId('LAB-01'), byId('LAB-12')]);
		expect(plans.every((plan) => plan.adjustment === undefined)).toBe(true);
	});

	it('preserves target order', () => {
		const plans = planTargets([byId('LAB-09'), byId('LAB-03')]);
		expect(plans.map((plan) => plan.endpoint.id)).toEqual(['LAB-09', 'LAB-03']);
	});
});

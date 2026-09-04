import { describe, expect, it } from 'vitest';
import type { FleetEndpoint } from '$lib/fixtures/endpoints';
import { situationMeta, summarizeSelection } from './situation';

const endpoint = (id: string, situation: FleetEndpoint['situation']): FleetEndpoint => ({
	id,
	bench: 'X-00',
	situation,
	hardware: 'test',
	contactMinutesAgo: 0
});

describe('situationMeta', () => {
	it('gives every situation a distinct tone and a label key', () => {
		const situations = [
			'available',
			'working',
			'pending-enrollment',
			'attention',
			'not-ready',
			'unavailable'
		] as const;
		const tones = situations.map((situation) => situationMeta(situation).tone);
		expect(new Set(tones).size).toBe(situations.length);
		for (const situation of situations) {
			expect(situationMeta(situation).labelKey.startsWith('endpoints.situation.')).toBe(true);
		}
	});

	it('separates current work from attention', () => {
		expect(situationMeta('working').tone).toBe('work');
		expect(situationMeta('attention').tone).toBe('attention');
	});
});

describe('summarizeSelection', () => {
	it('buckets the #41 acceptance scenario as 1 ready / 1 attention / 1 other', () => {
		const summary = summarizeSelection([
			endpoint('LAB-03', 'available'),
			endpoint('LAB-07', 'attention'),
			endpoint('LAB-09', 'not-ready')
		]);
		expect(summary).toEqual({ total: 3, ready: 1, attention: 1, other: 1 });
	});

	it('counts working, pending and unavailable endpoints as other', () => {
		const summary = summarizeSelection([
			endpoint('LAB-04', 'working'),
			endpoint('LAB-05', 'pending-enrollment'),
			endpoint('LAB-12', 'unavailable')
		]);
		expect(summary).toEqual({ total: 3, ready: 0, attention: 0, other: 3 });
	});

	it('is empty for no selection', () => {
		expect(summarizeSelection([])).toEqual({ total: 0, ready: 0, attention: 0, other: 0 });
	});
});

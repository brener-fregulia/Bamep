import { describe, expect, it } from 'vitest';
import { fleet, type EndpointSituation } from './endpoints';

describe('endpoints fleet fixture', () => {
	it('has 12 endpoints with unique LAB-01..LAB-12 ids', () => {
		expect(fleet).toHaveLength(12);
		expect(fleet.map((endpoint) => endpoint.id)).toEqual([
			'LAB-01', 'LAB-02', 'LAB-03', 'LAB-04', 'LAB-05', 'LAB-06',
			'LAB-07', 'LAB-08', 'LAB-09', 'LAB-10', 'LAB-11', 'LAB-12'
		]);
	});

	it('exercises every representative situation', () => {
		const situations = new Set<EndpointSituation>(fleet.map((endpoint) => endpoint.situation));
		expect([...situations].sort()).toEqual([
			'attention',
			'available',
			'not-ready',
			'pending-enrollment',
			'unavailable',
			'working'
		]);
	});

	it('places the #41 acceptance-scenario endpoints in distinct situations', () => {
		const by = (id: string) => fleet.find((endpoint) => endpoint.id === id);
		expect(by('LAB-03')?.situation).toBe('available');
		expect(by('LAB-04')?.situation).toBe('working');
		expect(by('LAB-05')?.situation).toBe('pending-enrollment');
		expect(by('LAB-07')?.situation).toBe('attention');
		expect(by('LAB-09')?.situation).toBe('not-ready');
		expect(by('LAB-12')?.situation).toBe('unavailable');
	});

	it('carries activity only for working rows and attention detail only for attention rows', () => {
		for (const endpoint of fleet) {
			if (endpoint.activity) expect(endpoint.situation).toBe('working');
			if (endpoint.attentionKey) expect(endpoint.situation).toBe('attention');
		}
	});
});

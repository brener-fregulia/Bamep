import { describe, expect, it } from 'vitest';
import { fleet } from '$lib/fixtures/endpoints';
import { newOperationHref, resolveTargets } from './targets';

describe('selected-target handoff', () => {
	it('builds the /operations/new continuation with one repeated target per id', () => {
		expect(newOperationHref(['LAB-03', 'LAB-07', 'LAB-09'])).toBe(
			'/operations/new?target=LAB-03&target=LAB-07&target=LAB-09'
		);
	});

	it('builds a plain /operations/new href for an empty selection', () => {
		expect(newOperationHref([])).toBe('/operations/new');
	});

	it('resolves known ids preserving request order', () => {
		const resolved = resolveTargets(['LAB-09', 'LAB-03', 'LAB-07'], fleet);
		expect(resolved.map((endpoint) => endpoint.id)).toEqual(['LAB-09', 'LAB-03', 'LAB-07']);
	});

	it('ignores duplicate ids deterministically, keeping the first occurrence', () => {
		const resolved = resolveTargets(['LAB-07', 'LAB-03', 'LAB-07', 'LAB-03'], fleet);
		expect(resolved.map((endpoint) => endpoint.id)).toEqual(['LAB-07', 'LAB-03']);
	});

	it('ignores ids not present in the local fleet', () => {
		const resolved = resolveTargets(['LAB-99', 'LAB-03', 'SRV-01'], fleet);
		expect(resolved.map((endpoint) => endpoint.id)).toEqual(['LAB-03']);
	});

	it('resolves to an empty set when no requested id is valid', () => {
		expect(resolveTargets(['LAB-99', 'LAB-99'], fleet)).toEqual([]);
		expect(resolveTargets([], fleet)).toEqual([]);
	});
});

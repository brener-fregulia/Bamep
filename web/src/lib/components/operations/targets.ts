/**
 * Presentation-only selected-target handoff between `/endpoints` and
 * `/operations/new` (Issue #51).
 *
 * The repeated `target` query parameter is a navigation mechanism local to the
 * Presentation client: it carries which local fleet fixtures the operator
 * selected across the route boundary. It is NOT an Administrative API wire
 * contract, a submission payload, a request key, a durable identity mechanism,
 * or a permanent URL specification, and it may be replaced once real
 * server-backed state exists. Operation configuration itself is never encoded
 * in the URL.
 */
import type { FleetEndpoint } from '$lib/fixtures/endpoints';

export const TARGET_PARAM = 'target';

/** Build the `/operations/new` continuation for the selected fixture ids. */
export function newOperationHref(targetIds: readonly string[]): string {
	if (targetIds.length === 0) return '/operations/new';
	const params = new URLSearchParams();
	for (const id of targetIds) params.append(TARGET_PARAM, id);
	return `/operations/new?${params.toString()}`;
}

/**
 * Resolve requested target ids against the local fleet: ids not present in the
 * fleet are ignored, duplicates keep only the first occurrence, and request
 * order is preserved deterministically.
 */
export function resolveTargets(
	requestedIds: readonly string[],
	fleet: readonly FleetEndpoint[]
): FleetEndpoint[] {
	const byId = new Map(fleet.map((endpoint) => [endpoint.id, endpoint]));
	const seen = new Set<string>();
	const resolved: FleetEndpoint[] = [];
	for (const id of requestedIds) {
		if (seen.has(id)) continue;
		seen.add(id);
		const endpoint = byId.get(id);
		if (endpoint) resolved.push(endpoint);
	}
	return resolved;
}

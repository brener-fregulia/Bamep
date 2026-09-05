/**
 * Presentation-only mock configuration model for the `Nova operação` surface.
 *
 * `Reinstalar Windows`, data preservation/restore, `debloat`, and driver
 * installation are mock product-level service intents that exist only to
 * exercise the accepted M2 configuration flow (Issue #51). They are NOT
 * JobStep types, production action IDs, backup/restore contracts, driver
 * policy, licensing behavior, or destructive authorization rules, and no
 * API DTO derives from them.
 *
 * The accepted model is deliberately tiny: one fixed product intent, a small
 * common configuration shared by every target, and per-Endpoint adjustments
 * that express only differences from the common configuration.
 */
import type { FleetEndpoint } from '$lib/fixtures/endpoints';
import type { MessageKey } from '$lib/i18n';

export type AdjustmentKind = 'preserve-restore-data' | 'apply-debloat';

export interface AdjustmentText {
	nameKey: MessageKey;
	hintKey: MessageKey;
	/** Accessible toggle label template; takes `{id}`. */
	toggleKey: MessageKey;
	/** Compact "common + difference" chip shown in the targets panel. */
	deltaKey: MessageKey;
}

export const adjustmentText: Record<AdjustmentKind, AdjustmentText> = {
	'preserve-restore-data': {
		nameKey: 'operationsNew.adjust.preserveRestore',
		hintKey: 'operationsNew.adjust.preserveRestoreHint',
		toggleKey: 'operationsNew.adjust.preserveRestoreToggle',
		deltaKey: 'operationsNew.targets.deltaPreserve'
	},
	'apply-debloat': {
		nameKey: 'operationsNew.adjust.debloat',
		hintKey: 'operationsNew.adjust.debloatHint',
		toggleKey: 'operationsNew.adjust.debloatToggle',
		deltaKey: 'operationsNew.targets.deltaDebloat'
	}
};

/**
 * Representative mock scenario (#44/#51): which fleet fixtures arrive with a
 * pre-configured difference from the common configuration. Every other target
 * follows the common configuration only.
 */
const SCENARIO_ADJUSTMENTS: Readonly<Record<string, AdjustmentKind>> = {
	'LAB-03': 'preserve-restore-data',
	'LAB-07': 'apply-debloat'
};

export interface TargetPlan {
	endpoint: FleetEndpoint;
	/** Difference from the common configuration; `undefined` = common only. */
	adjustment?: AdjustmentKind;
}

export function planTargets(targets: readonly FleetEndpoint[]): TargetPlan[] {
	return targets.map((endpoint) => ({
		endpoint,
		adjustment: SCENARIO_ADJUSTMENTS[endpoint.id]
	}));
}

/** One row of the compact target-context panel. */
export interface TargetContextRow {
	endpoint: FleetEndpoint;
	/** Effective service summary chip: common only, or common + difference. */
	deltaKey: MessageKey;
	hasDelta: boolean;
}

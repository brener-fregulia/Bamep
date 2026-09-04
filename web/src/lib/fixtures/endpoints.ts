/**
 * Deterministic local presentation fixtures for the Endpoints list.
 *
 * This is demo data for UX evaluation — NOT live Simulator output, NOT an
 * Administrative API response, NOT a mirror of any Rust/Domain type. The
 * `EndpointSituation` union is a Presentation-only rendering discriminant; it is
 * not an authoritative `EndpointReady`-style enum and carries no backend
 * eligibility semantics.
 *
 * The fleet intentionally mirrors the composition validated in prototype #41 so
 * the production surface is comparable to the accepted UX evidence.
 */
import type { MessageKey } from '$lib/i18n';

export type EndpointSituation =
	| 'available'
	| 'working'
	| 'pending-enrollment'
	| 'attention'
	| 'not-ready'
	| 'unavailable';

export type EndpointActivity = 'capturing-image' | 'preparing-operation';

export interface FleetEndpoint {
	/** Operator-facing identity, e.g. `LAB-03`. */
	id: string;
	/** Physical location hint, e.g. `A-03` (rendered as "bancada A-03"). */
	bench: string;
	situation: EndpointSituation;
	/** Short technical hardware summary, used to recognise the machine. */
	hardware: string;
	/** Whole minutes since last contact; `0` renders as "agora". */
	contactMinutesAgo: number;
	/** Present only while `situation === 'working'`. */
	activity?: EndpointActivity;
	/** Present only while `situation === 'attention'`. */
	attentionKey?: MessageKey;
	attentionHintKey?: MessageKey;
}

export const fleet: readonly FleetEndpoint[] = [
	{ id: 'LAB-01', bench: 'A-01', situation: 'available', hardware: 'Ryzen 5 5600G · 16 GB · 512 GB NVMe', contactMinutesAgo: 0 },
	{ id: 'LAB-02', bench: 'A-02', situation: 'available', hardware: 'Ryzen 5 5600G · 16 GB · 512 GB NVMe', contactMinutesAgo: 1 },
	{ id: 'LAB-03', bench: 'A-03', situation: 'available', hardware: 'Ryzen 7 5700G · 32 GB · 1 TB NVMe', contactMinutesAgo: 0 },
	{ id: 'LAB-04', bench: 'B-01', situation: 'working', hardware: 'Core i5-12400 · 16 GB · 512 GB SSD', contactMinutesAgo: 0, activity: 'capturing-image' },
	{ id: 'LAB-05', bench: 'B-02', situation: 'pending-enrollment', hardware: 'Core i5-10500 · 16 GB · 256 GB SSD', contactMinutesAgo: 2 },
	{ id: 'LAB-06', bench: 'B-03', situation: 'available', hardware: 'Ryzen 5 5600G · 16 GB · 512 GB NVMe', contactMinutesAgo: 1 },
	{
		id: 'LAB-07',
		bench: 'C-01',
		situation: 'attention',
		hardware: 'Ryzen 7 5700G · 32 GB · 1 TB NVMe',
		contactMinutesAgo: 6,
		attentionKey: 'endpoints.attention.uncertainResult',
		attentionHintKey: 'endpoints.attention.uncertainResultHint'
	},
	{ id: 'LAB-08', bench: 'C-02', situation: 'working', hardware: 'Core i7-12700 · 32 GB · 1 TB NVMe', contactMinutesAgo: 0, activity: 'preparing-operation' },
	{ id: 'LAB-09', bench: 'C-03', situation: 'not-ready', hardware: 'Ryzen 5 5600G · 16 GB · 512 GB NVMe', contactMinutesAgo: 3 },
	{ id: 'LAB-10', bench: 'D-01', situation: 'available', hardware: 'Core i5-12400 · 16 GB · 512 GB SSD', contactMinutesAgo: 0 },
	{ id: 'LAB-11', bench: 'D-02', situation: 'available', hardware: 'Ryzen 5 5600G · 16 GB · 512 GB NVMe', contactMinutesAgo: 1 },
	{ id: 'LAB-12', bench: 'D-03', situation: 'unavailable', hardware: 'Ryzen 3 3400G · 8 GB · 256 GB SSD', contactMinutesAgo: 24 }
];

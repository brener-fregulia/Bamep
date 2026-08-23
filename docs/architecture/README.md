# Bamep Architecture

## Purpose

This directory documents Bamep architecture that is **implemented in the current
repository**.

Architecture documentation answers:

> How does Bamep work now?

It does not define planned behavior or preserve decision rationale. Normative behavior
belongs to `docs/specifications/`, and architectural rationale belongs to
`docs/decisions/`.

Code and tests remain the final source of truth for implemented behavior.

## Current implementation

Bamep is currently a Rust workspace with five implemented crates:

| Crate | Implemented responsibility |
| --- | --- |
| `bamep-trusted-bootstrap` | Shared trusted-bootstrap contract primitives, parsing, transcript construction, and verification |
| `bamep-agent-protocol` | Rust wire-model and codec for the currently implemented Agent Protocol v1 message slice |
| `bamep-domain` | Pure Endpoint identity, boot-context, trusted-bootstrap-state, and runtime-credential business logic |
| `bamep-server` | Application services, ports, transport/persistence adapters, and Server-side Agent session handling |
| `bamep-simulator` | Simulated Endpoint/Agent participant using the real trusted-bootstrap and WSS/Agent Protocol path |

Only implemented boundaries are described here. Planned components and behavior remain in
their Specifications, ADRs, Discovery, or GitHub work until corresponding code exists.

## Dependency direction

The implemented dependency direction keeps protocol, trust, and business logic separated
from infrastructure concerns.

```text
bamep-trusted-bootstrap
        │
        ├──────────────► bamep-domain ─────────► bamep-server
        │                                      ▲
        └──────────────► bamep-simulator       │
                                               │
bamep-agent-protocol ──► bamep-simulator       │
        │                                      │
        └──────────────────────────────────────►
```

`bamep-trusted-bootstrap` owns only trusted-bootstrap contract representations and
operations. It does not depend on Domain, Server, Simulator, Agent Protocol, async runtime,
TLS, or WebSocket infrastructure.

`bamep-agent-protocol` contains the shared Rust representation of the wire contract and its
codec. The Markdown Agent Protocol Specification remains normative; the crate implements
that contract rather than redefining it.

`bamep-domain` contains pure business logic. Domain transitions receive time and required
secrets explicitly and return resulting state, domain events, and audit information
without performing I/O or persistence.

`bamep-simulator` depends on the shared trusted-bootstrap and Agent Protocol crates. It does
not depend on Domain or Server and exercises the external Agent-side boundary instead of
calling Server business logic in process.

## Server boundary

`bamep-server` currently preserves three internal responsibility layers:

```text
adapters
   │
   ▼
application
   │
   ▼
ports
   │
   ▼
domain
```

The dependency rule is more important than the physical module count:

- `application` coordinates use cases through `ports` and `bamep-domain`;
- `ports` define infrastructure-facing abstractions required by the application layer;
- `adapters` implement those ports and contain infrastructure-specific dependencies;
- PostgreSQL/SQLx persistence lives under the adapter boundary;
- Agent transport and Agent gateway implementations live under the adapter boundary.

Infrastructure must not leak into Domain transitions.

## Current Agent-side flow

The currently validated Simulator/Server slice uses the real external transport boundary:

1. the Simulator establishes trusted bootstrap from simulated bootstrap material;
2. the expected Server certificate fingerprint is established before Agent Protocol
   authentication;
3. the Simulator connects through WSS with exact Server certificate pinning;
4. Agent Protocol v1 authentication is exchanged over that WSS connection;
5. trusted-bootstrap evidence is sent after session establishment;
6. Server application/domain logic evaluates identity, credential, boot-context, and
   trusted-bootstrap state;
7. durable state is persisted through the PostgreSQL adapter boundary.

The Simulator represents production boot-chain inputs with fixtures where the real
integration environment is not yet implemented, but it does not replace the WSS or Agent
Protocol boundary with an in-process fake.

## Documentation boundary

This directory should describe only architecture visible in implemented code.

When implementation changes:

- inspect code and tests first;
- update Architecture only for durable structural information;
- link to Specifications for normative behavior;
- link to ADRs for decision rationale;
- link to Reference documents for empirical evidence;
- do not copy GitHub execution history into Architecture documentation.

If this documentation disagrees with the current implementation, it is stale and must be
corrected.

# Bamep Architecture

This directory describes architecture **implemented in the current repository**. Code and
tests are final evidence for current behavior; Specifications own normative behavior and
ADRs own decision rationale.

## Current workspace

Bamep currently has five Rust crates:

| Crate | Implemented responsibility |
| --- | --- |
| `bamep-trusted-bootstrap` | Trusted-bootstrap primitives, assertion parsing/transcript, and verification |
| `bamep-agent-protocol` | Rust wire model/codec for the implemented Agent Protocol v1 slice |
| `bamep-domain` | Pure Endpoint identity, boot-context, trusted-bootstrap, and runtime-credential business logic |
| `bamep-server` | Application services, Ports, PostgreSQL/transport Adapters, and Agent session handling |
| `bamep-simulator` | Simulated Agent participant using real trusted-bootstrap and WSS/Agent Protocol boundaries |

Planned components remain outside Architecture until corresponding code exists.

## Dependency boundaries

The implemented structure preserves these rules:

- `bamep-trusted-bootstrap` owns only trusted-bootstrap contract representations/operations;
  it has no Domain, Server, Simulator, Agent Protocol, async-runtime, TLS, or WebSocket
  dependency.
- `bamep-agent-protocol` is a transport-independent Rust representation of the normative
  Agent Protocol Specification.
- `bamep-domain` contains pure business logic: transitions take time/secrets explicitly and
  perform no I/O or persistence.
- `bamep-simulator` depends on Agent Protocol and trusted-bootstrap, not Domain or Server; it
  exercises the external Agent-side boundary.
- `bamep-server` contains `application`, `ports`, and `adapters`; Application coordinates
  through Ports and Domain, while infrastructure-specific dependencies stay in Adapters.
- PostgreSQL/SQLx and Agent transport/gateway implementations are Server Adapter concerns.

Infrastructure must not leak into Domain transitions.

## Implemented Agent-side path

The current Simulator/Server slice:

1. establishes trusted bootstrap from simulated bootstrap material;
2. establishes the expected Server certificate fingerprint before Agent authentication;
3. connects through WSS with exact Server-certificate pinning;
4. exchanges Agent Protocol v1 authentication over the real WebSocket transport;
5. sends retained trusted-bootstrap evidence after session establishment;
6. evaluates Endpoint identity, credential, BootContext, and trusted-bootstrap state through
   Server Application/Domain logic;
7. accepts post-session opaque `InventoryReport` snapshots and records a Server-owned current
   inventory revision only on semantic change;
8. persists durable state and required domain events atomically through the PostgreSQL Adapter
   boundary.

Production boot-chain inputs are still represented by Simulator fixtures where the physical
Integration Environment is not implemented. The WSS and Agent Protocol boundary itself is
not replaced by an in-process fake.

## Maintenance rule

Update this directory only for durable structure visible in implemented code. Do not copy
planned contracts, ADR rationale, empirical evidence, or GitHub execution history here.

If this document disagrees with code/tests, it is stale.

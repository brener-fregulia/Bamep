# M0 — Architecture Baseline Completion Record

Status: **Completed; historical milestone record.**

M0 closed Bamep's architecture-and-contract phase before implementation. At closure, the
required M0 decisions/contracts were accepted or approved and the owner explicitly approved the
baseline.

This record does not claim that implementation, empirical Simulator validation, or physical
Integration Environment work was complete. Later accepted/superseding decisions remain
authoritative over this point-in-time record.

## Completion criteria

M0 required:

1. persisted product boundary, vocabulary, and non-goals;
2. blocking architectural decisions accepted or isolated as explicit empirical work;
3. specified destructive-operation safety invariants;
4. defined simulated vertical-slice contracts and failure scenarios;
5. explicit component responsibilities and boundaries;
6. validation strategy for required behavior;
7. no required architecture hidden inside future implementation work;
8. explicit owner approval.

These are historical milestone criteria, not a second source of current normative behavior.

## Successor slice

The approved first implementation milestone after M0 was the hardware-independent simulated
vertical slice:

```text
Simulated Endpoint connects
-> authenticated/enrolled
-> inventory reported
-> Job created
-> scheduler evaluates resources
-> typed action dispatched
-> simulated transfer executed
-> progress/events persisted
-> disconnect/reconnect handled
-> Job reaches terminal state
-> Web reflects result
```

Its representative high-density scenario targets **20–24 concurrent Simulated Endpoints**.

Execution and validation of this scope are now owned by
`docs/specifications/m1-simulated-vertical-slice-and-baseline-validation.md`; Simulator
fidelity/concurrency semantics are owned by
`docs/specifications/m0-simulator-contract-and-validation-strategy.md`.

## Authority

This file is not authoritative for detailed current behavior. Use:

- individual M0 Specifications for normative contracts;
- ADRs for decision rationale/history;
- `docs/architecture/README.md` for implemented architecture;
- GitHub Issues/Milestones for execution history and current work.

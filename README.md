# Bamep

> Open-source bare-metal provisioning platform.

Bamep (Bare-Metal Provisioning) is a platform for provisioning, recovery, and orchestration of physical endpoints over local networks.

The project is designed around automated bare-metal workflows such as network boot, hardware and storage inventory, backup and recovery, operating system deployment, and post-install automation.

Bamep is a ground-up implementation informed by lessons learned from [FORGE](https://github.com/brener-fregulia/forge), the historical proof of concept that validated the core ideas behind the project.

## Origins

[FORGE](https://github.com/brener-fregulia/forge) was the original proof of concept, exploring and validating ideas around network boot, endpoint orchestration, inventory, backup/recovery, and automated Windows deployment.

Development did not simply continue from FORGE. Bamep was started as a ground-up redesign, built through Specification-Driven Development with explicit architectural contracts. FORGE remains public as a historical and technical reference; its architecture is not authoritative for Bamep.

## Project status

Bamep has moved beyond its initial architecture-and-contract baseline.

- **M0** — architecture and contract baseline: complete.
- **M1** — hardware-independent operational core: established.
- **M2** — Operator Plane: current development phase.

The Web client foundation and physical Endpoint fleet surface are implemented, with the
operator workflow progressing through operation configuration and review.

Physical Integration Environment work has also validated diskless UEFI PXE boot under
Secure Boot through the selected iPXE + wimboot Boot Adapter baseline into functional WinPE.

Bamep remains pre-release. Production provisioning, backup/recovery, the physical
maintenance Agent, and complete production hardware integration are not yet complete.

The initial production target is:

- Windows provisioning;
- UEFI x86-64 endpoints;
- Linux-based Bamep Server;
- browser-based administration;
- standalone, single-server deployments.

## Components

Bamep is structured around independently evolvable components such as:

- **Bamep Server**
- **Bamep Web**
- **Bamep Agent**
- **Bamep Simulator**

The architecture baseline is established through Specification-Driven Development, Accepted Architecture Decision Records, and Approved Specifications. Implementation follows those contracts.

## Development

- [Technical documentation](docs/)
- [Issues](https://github.com/brener-fregulia/Bamep/issues)
- [Bamep Development project](https://github.com/users/brener-fregulia/projects/4)

## License

Bamep is licensed under the [Apache License 2.0](LICENSE).

Copyright 2026 Brener Fregulia.

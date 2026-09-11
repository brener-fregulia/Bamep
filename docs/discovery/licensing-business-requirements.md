# Licensing Business Requirements

Status: Draft  
Scope: Business requirements only — not a software license

## Purpose

This document defines the intended business boundary for future Bamep licensing.

It exists to establish what the licensing model is expected to permit and restrict before
a specific source-available license, commercial agreement, contribution model, or legal
text is selected.

This document is not a license and grants no rights.

## Core principle

Bamep should remain publicly inspectable and freely available for personal,
educational, research, evaluation, and other approved non-commercial purposes.

Commercial use of Bamep should require a commercial license.

The goal is to keep Bamep accessible for learning, experimentation, research, hardware
enablement, and community contribution while ensuring that organizations obtaining
commercial or operational value from the software contribute financially to its
continued development and maintenance.

## Intended free use

The future Community terms should permit, at minimum:

- personal use;
- homelab use;
- hobby projects;
- personal study;
- experimentation;
- academic research;
- non-commercial technical research;
- educational use;
- eligible non-profit institutional use;
- development and testing of Bamep itself;
- development and testing of hardware, boot, and integration adapters;
- non-production evaluation of Bamep before deciding whether to obtain a commercial
  license.

Users should be allowed to inspect and modify the source code for these permitted
purposes.

The exact treatment of educational institutions, public institutions, non-profit
organizations, and commercially funded research must be resolved during Licensing
Discovery.

## Commercial use

Any use of Bamep for a commercial or for-profit operational purpose should require a
separate Commercial License.

Examples include:

- using Bamep as part of the operations of a for-profit business;
- using Bamep to perform paid IT services;
- using Bamep to provision, recover, maintain, or manage devices as part of commercial
  activity;
- using Bamep as production infrastructure inside a commercial organization;
- using Bamep to automate provisioning or recovery of an organization's commercial
  fleet;
- providing managed services powered by Bamep;
- providing provisioning services powered by Bamep;
- selling or leasing a Bamep-based appliance;
- OEM or white-label distribution;
- redistributing Bamep as part of a commercial product;
- offering hosted access to Bamep or a Bamep-derived provisioning platform.

Commercial licensing must apply regardless of whether Bamep is provided directly to
the commercial customer's own users or remains running on infrastructure controlled by
the service provider.

## Evaluation

Commercial organizations should be able to evaluate Bamep before purchasing a license.

The future licensing model should therefore allow reasonable non-production evaluation,
compatibility testing, demonstrations, and proof-of-concept work.

Evaluation rights must not become a substitute for continued production use.

The exact evaluation limits are to be defined later.

## Independent competing software

The licensing model must not attempt to prevent independent competition.

Anyone remains free to create a separate bare-metal provisioning product without using
Bamep source code or other protected Bamep assets.

The commercial restriction concerns use of Bamep itself and Bamep-derived software,
not the general idea of bare-metal provisioning.

## Hardware and adapter ecosystem

Bamep should actively encourage third-party hardware enablement.

The project should make it practical for contributors to add support for hardware and
integration scenarios that the primary maintainer may never personally own or operate.

Examples include:

- network adapters;
- storage controllers;
- HBAs;
- RAID controllers;
- vendor-specific hardware;
- boot integrations;
- storage integrations;
- diagnostic integrations.

The long-term licensing model should distinguish the Bamep product from the adapter
ecosystem.

A likely direction to investigate is:

- Bamep Core: source-available Community + Commercial licensing;
- Adapter SDK/API: contribution-friendly and broadly usable;
- community adapters: terms that encourage distributed improvements to remain
  available to the community;
- first-party commercial add-ons: may remain proprietary.

The exact licenses for these boundaries are not decided by this document.

## Community contributions

Third-party contributions, especially hardware and integration adapters, are desirable.

The future contribution model should preserve the project's ability to offer commercial
licenses while allowing contributors to retain appropriate rights to their work.

Licensing Discovery must therefore evaluate whether a Contributor License Agreement
or another explicit contribution agreement is required.

A contribution policy must be established before substantial third-party contributions
are accepted under the future licensing model.

## Commercial add-ons

The Community/source-available nature of Bamep Core must not require every first-party
feature to be distributed free of charge or with publicly available source code.

Bamep may offer optional proprietary or commercially licensed modules.

Potential examples include:

- advanced SMART reporting;
- backup verification and audit reporting;
- malware scanning workflows;
- compliance reporting;
- advanced historical reporting;
- commercial integrations;
- support and management capabilities.

Community users remain free to independently develop similar functionality where
legally and technically permitted.

The commercial product protects the maintained implementation and its integration,
not the general idea behind a feature.

## Licensing and billing

Software licensing, feature entitlement, contracts, and billing are separate concerns.

Bamep Core may eventually understand whether an optional capability is available, but
billing and customer contract logic should not become part of the core provisioning
domain.

Commercial add-ons should not rely solely on hiding a feature flag inside publicly
available Core source code.

## Source model

If commercial use is prohibited without a separate commercial license, Bamep Core will
not be Open Source under the OSI definition.

Public project language must use an accurate term such as `source-available` once the
licensing transition occurs.

The README, repository description, contribution documentation, and other public
surfaces must be updated accordingly.

## Existing Apache-2.0 releases

Code already published under Apache License 2.0 remains available under the rights
previously granted by that license.

The future licensing transition applies prospectively to later Bamep versions for which
the project has the necessary licensing rights.

Licensing Discovery must establish and record the exact Apache-2.0 cutoff revision.

The project must not attempt to rewrite history or imply that previously granted
Apache-2.0 rights have been revoked.

## Open questions for Licensing Discovery

Before changing the repository license, Licensing Discovery must determine:

1. the exact Apache-2.0 cutoff commit;
2. whether any incorporated third-party contribution affects relicensing rights;
3. the licenses and obligations of Rust dependencies;
4. the licenses and obligations of JavaScript/npm dependencies;
5. the licenses and redistribution obligations of Buildroot, Linux, BusyBox, firmware,
   boot components, and other distributed runtime dependencies;
6. the most appropriate standardized source-available license for Bamep Core;
7. whether a custom commercial-use restriction is necessary;
8. which organizations qualify for free educational, research, public-interest, or
   non-profit use;
9. what constitutes permitted commercial evaluation;
10. the license for the Adapter SDK/API;
11. the license for community adapters;
12. the contribution/CLA model required for dual licensing;
13. the treatment of first-party proprietary add-ons;
14. trademark and product-name policy;
15. required README, NOTICE, CONTRIBUTING, SECURITY, and repository-metadata changes.

## Non-goals

This document does not:

- select a software license;
- draft legal license language;
- define commercial pricing;
- define add-on pricing;
- define billing implementation;
- revoke previous Apache-2.0 grants;
- define the future Adapter API;
- define a commercial contract.

Those decisions follow from the requirements recorded here.
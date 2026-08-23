# CLAUDE.md

@AGENTS.md

## Purpose

This file contains Claude Code-specific instructions for working in Bamep.

Repository-wide guardrails come from `AGENTS.md`. Engineering process and documentation
authority come from the files referenced there.

Do not restate those policies here unless Claude Code requires a tool-specific rule.

## Interaction language

Use Brazilian Portuguese (`pt-BR`) when communicating with the repository owner by
default.

- Prefer natural Brazilian Portuguese for explanations, questions, summaries, and
  workflow output.
- Use another language when explicitly requested or when preserving source text exactly is
  important.
- Keep canonical repository artifacts in the language required by `AGENTS.md`.
- Do not translate commands, paths, identifiers, protocol/API fields, or tool output merely
  for conversational consistency.

## Context management

Build context from the current repository rather than from assumptions about earlier
sessions.

- Start with the task and the closest relevant files.
- Prefer targeted searches using symbols, component names, protocol fields, tests,
  configuration, or observable behavior.
- Follow dependencies only as far as needed to understand the task.
- Expand context when architecture, safety, dependencies, or failing validation require it.
- Avoid loading entire directories or large unrelated documents without a concrete reason.
- Use repository and GitHub evidence to reconstruct persistent project context when needed.

For substantial planning or cross-cutting work, inspect the relevant Specifications,
Architecture, ADRs, Discovery, Reference material, implementation, tests, and GitHub work
that actually affect the task.

## Project skills

Project skills live under `.claude/skills/` and are user-invoked workflows.

When a project skill is invoked:

- read and follow that skill's `SKILL.md`;
- respect its stated scope, restrictions, validation requirements, and output contract;
- treat the invocation as authorization for that workflow only;
- do not infer authorization for Git/GitHub mutation, publication, infrastructure changes,
  destructive operations, or other actions restricted by `AGENTS.md`;
- do not invent or claim to have executed a missing skill.

Project skills intentionally use `disable-model-invocation: true`.

Do not assume a project skill can be invoked automatically.

A skill cannot override `AGENTS.md`.

## Subagents

Use specialized subagents when they provide relevant expertise or isolate a focused
investigation.

Give each subagent:

- a bounded objective;
- relevant starting points;
- explicit scope restrictions;
- required evidence or validation;
- expected output.

The main Claude Code session remains responsible for:

- coordinating the work;
- reconciling conflicting findings;
- preserving the approved scope;
- validating the combined result;
- producing the final response.

Do not delegate the same work repeatedly without a concrete reason.

## Claude Code-specific behavior

- Inspect the closest existing pattern before creating files or abstractions.
- Keep edits focused and preserve surrounding conventions.
- Verify repository commands and paths before using them.
- Do not install system dependencies or change global machine configuration without
  explicit owner permission.
- Prefer repository-local, reproducible tooling over machine-specific setup when both can
  represent the task faithfully.
- When a durable project rule is needed, place it in its proper repository authority
  rather than adding it to this tool-specific file.

Use the authorities referenced by `AGENTS.md` instead of duplicating SDD, workflow,
testing, documentation, architecture, or safety policy here.

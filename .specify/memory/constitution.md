<!--
Sync Impact Report
===================
- Version change: 0.0.0 → 1.0.0 (initial ratification)
- Added principles:
  - I. Code Quality
  - II. Testing Standards
  - III. User Experience Consistency
  - IV. Performance Requirements
- Added sections:
  - Development Standards
  - Quality Gates
  - Governance
- Templates requiring updates:
  - .specify/templates/plan-template.md — ✅ no changes needed
    (Constitution Check section already references constitution file
    dynamically)
  - .specify/templates/spec-template.md — ✅ no changes needed
    (success criteria and requirements sections align with principles)
  - .specify/templates/tasks-template.md — ✅ no changes needed
    (phase structure and test-first guidance compatible)
- Follow-up TODOs: none
-->

# HAProxy SPOA Hub Constitution

## Core Principles

### I. Code Quality

All code MUST be clear, maintainable, and reviewable by humans.
Concrete rules:

- Functions and modules MUST have a single, well-defined responsibility.
- Public APIs MUST be documented with purpose, parameters, return
  values, and error conditions.
- All warnings from the project's configured linters and static
  analysis tools MUST be resolved before merge.
- Dead code, unused imports, and commented-out code MUST NOT be
  committed.
- Error handling MUST be explicit — silent failures and swallowed
  errors are prohibited.

**Rationale**: A SPOA hub is infrastructure software that other
systems depend on. Unclear or fragile code in this layer causes
cascading failures that are hard to diagnose.

### II. Testing Standards

Every behavioral change MUST be accompanied by automated tests.
Concrete rules:

- Bug fixes MUST include a regression test that fails before the
  fix and passes after.
- New features MUST have unit tests covering the primary path and
  at least one error path.
- Integration tests MUST cover the SPOE wire protocol interaction
  between HAProxy and registered agents.
- Test names MUST describe the scenario under test, not the
  implementation detail.
- Tests MUST be deterministic — no reliance on timing, ordering,
  or external network state.

**Rationale**: HAProxy processes production traffic. Untested agent
logic risks silent data corruption or dropped requests under load.

### III. User Experience Consistency

All user-facing interfaces (CLI, configuration, logging, error
messages) MUST behave predictably and uniformly. Concrete rules:

- CLI flags and configuration keys MUST follow a single, documented
  naming convention throughout the project.
- Error messages MUST identify what went wrong, which input caused
  it, and what the user can do about it.
- Log output MUST use structured logging with consistent field names
  and severity levels.
- Configuration changes MUST be validated at startup — not at first
  use — with actionable error messages on failure.
- Breaking changes to CLI or configuration MUST be documented in a
  changelog entry before merge.

**Rationale**: Operators deploy SPOA agents in automated pipelines.
Inconsistent interfaces cause misconfigurations that surface only in
production.

### IV. Performance Requirements

The hub MUST NOT become a bottleneck in the HAProxy request path.
Concrete rules:

- Agent message processing latency MUST stay below 1 ms at p99 for
  pass-through operations under the project's defined benchmark load.
- Memory allocations per request MUST be minimized; hot paths MUST
  avoid heap allocations where the language allows.
- Benchmarks MUST be included for any code on the critical
  request-processing path; regressions fail CI.
- Concurrency primitives MUST be chosen to avoid lock contention
  under parallel agent execution.
- Resource limits (goroutines, threads, file descriptors) MUST be
  configurable and documented with sensible defaults.

**Rationale**: HAProxy is chosen for its performance. An offload
agent that adds measurable latency defeats the purpose of the
architecture.

## Development Standards

- Source files MUST pass all configured linters with zero warnings
  before merge.
- Dependencies MUST be explicitly versioned and pinned. Transitive
  dependency updates MUST be reviewed.
- Commits MUST be atomic — one logical change per commit with a
  descriptive message.

## Quality Gates

Every pull request MUST satisfy the following before merge:

1. All existing and new tests pass.
2. Linter and static analysis checks pass with zero warnings.
3. Performance benchmarks show no regression beyond defined
   tolerance thresholds.
4. At least one human review approving the change.
5. Changelog updated for user-facing changes.

## Governance

This constitution is the authoritative source of project standards.
In case of conflict between this document and any other project
documentation, this constitution takes precedence.

- **Amendments**: Any change to this constitution MUST be proposed
  via pull request, reviewed by at least one maintainer, and merged
  only after approval. The amendment MUST include a version bump and
  updated "Last Amended" date.
- **Versioning**: This constitution follows semantic versioning.
  MAJOR for principle removals or incompatible redefinitions, MINOR
  for new principles or material expansions, PATCH for wording
  clarifications.
- **Compliance**: All pull requests and code reviews MUST verify
  adherence to these principles. Deviations MUST be justified in
  writing in the PR description and approved by a maintainer.

**Version**: 1.0.0 | **Ratified**: 2026-03-30 | **Last Amended**: 2026-03-30

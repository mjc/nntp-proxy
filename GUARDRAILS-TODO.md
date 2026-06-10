# Guardrails TODO (state-constrained connection fate)

Goal: make connection lifecycle misuse impossible inside the crate, not just less visible.

## Planned guardrails

1. Add finalize enforcement for guard-owning operations:
   - drop-bomb / finalize token in debug+tests
   - panic/fail fast if a guard-owning flow exits without explicit finalize.

2. Encode pool-fate decisions with typed outcomes:
   - `BackendHealthy`
   - `BackendDirty`
   - `BackendFailed`
   so connection fate is derived from outcome type, not ad-hoc branching.

3. Ban low-level calls outside the chokepoint module:
   - deny lint(s) for direct `release/retire/remove_*` usage
   - allowlist only the designated lifecycle module.

4. Add regression tests for lifecycle invariants:
   - no implicit guard-drop on timeout/cancel paths
   - client-side write/flush failures do not trigger backend cooldown retirement unless backend is dirty/failed.

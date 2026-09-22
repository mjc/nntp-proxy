# Security Advisories

This project treats both advisory tools as authoritative:

```sh
devenv shell cargo audit
devenv shell cargo deny check advisories
```

Ignored advisories must document:

- whether the dependency path is direct, runtime transitive, or dev-only
- why the dependency is being kept
- the next revisit trigger, such as an upstream release or review date
- an upstream issue or repository link when one exists

Use `devenv tasks run project:audit-advisories` when triaging advisory changes. It runs the
standard advisory checks and prints dependency paths for the currently ignored
crates, including runtime-only paths where Cargo can express them.

Benchmark dependencies are dev-only and are kept separate from the runtime
dependency surface. Divan supplies wall-clock microbenchmarks. Gungraun supplies
deterministic Callgrind instruction, branch, and cache measurements on supported
Linux targets. Keep benchmark-only advisory exceptions tied to the dependency
that is actually present in `Cargo.lock`; do not carry forward exceptions for
removed benchmark frameworks.

For dependency maintenance, run the advisory checks during normal PR validation
and periodically run:

```sh
devenv shell cargo outdated
devenv tasks run project:audit-advisories
```

When `foyer`, Divan, or Gungraun publishes a new version, try the upgrade and
remove any advisory ignores that disappear.

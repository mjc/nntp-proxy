# Security Advisories

This project treats both advisory tools as authoritative:

```sh
nix develop -c cargo audit
nix develop -c cargo deny check advisories
```

Ignored advisories must document:

- whether the dependency path is direct, runtime transitive, or dev-only
- why the dependency is being kept
- the next revisit trigger, such as an upstream release or review date
- an upstream issue or repository link when one exists

Use `scripts/audit-advisories` when triaging advisory changes. It runs the
standard advisory checks and prints dependency paths for the currently ignored
crates, including runtime-only paths where Cargo can express them.

Dev-only performance tooling may be retained when the warning is limited to
benchmark dependencies and the reason is documented. `iai-callgrind` is in that
category: it gives deterministic instruction-count benchmarks for hot paths, so
the current unmaintained transitive crates are tracked rather than removing the
tool. Revisit those ignores when `iai-callgrind` releases a version that drops
`bincode 1` or `proc-macro-error2`.

For dependency maintenance, run the advisory checks during normal PR validation
and periodically run:

```sh
nix develop -c cargo outdated
nix develop -c scripts/audit-advisories
```

When `foyer` or `iai-callgrind` publishes a new version, try the upgrade and
remove any advisory ignores that disappear.

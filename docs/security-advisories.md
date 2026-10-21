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

The remaining ignored advisories concern `bincode 1` and `paste`, which Foyer
0.22.6 still requires for cache serialization and internal code generation.
Recheck these paths when updating Foyer and remove an ignore when the affected
crate leaves the dependency graph. Gungraun 0.20 no longer needs the old
`proc-macro-error3` exception.

For dependency maintenance, run the advisory checks during normal PR validation
and periodically run:

```sh
devenv shell cargo outdated
devenv tasks run project:audit-advisories
```

When `foyer` or `gungraun` publishes a new version, try the upgrade and
remove any advisory ignores that disappear.

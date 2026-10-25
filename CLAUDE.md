# karazhan

## Version bump on every PR

Every PR bumps `version` in `Cargo.toml` (and the `karazhan` entry in
`Cargo.lock`, via `cargo build`) before it is opened:

- `feat:` PR → minor (`0.4.0` → `0.5.0`)
- any other PR (`fix:`, `chore:`, `docs:`, `test:`, ...) → patch (`0.4.0` → `0.4.1`)

Bump from the version on `master` at the time, not from the branch's base,
in case another PR merged in between. Check with `karazhan --version` after
`cargo build`.

Why: release tags (`v*`) are cut from `master`, and the update notice compares
the latest tag against `CARGO_PKG_VERSION`; a stale `Cargo.toml` makes
`--version` and the notice wrong.

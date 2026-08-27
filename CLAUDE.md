# CLAUDE.md

Conventions for this repository. `docs/AGENTS.md` has the full contributor
contract — this file is the short version plus the things that bite.

## Always pass `--offline` to cargo

```bash
cargo build --offline --workspace --all-features
cargo test  --offline --workspace --all-features
cargo clippy --offline --workspace --all-targets --all-features -- -D warnings
```

Every dependency is already in `Cargo.lock` and in the local registry cache, so
nothing here needs the network. `--offline` matters for a reason that is not
obvious: cargo takes a **global, per-user lock** on `~/.cargo/.package-cache`
whenever it may touch the registry. That lock is shared across *every* project on
the machine, so a build in another workspace serialises against this one. Without
`--offline` a full run here has taken 20+ minutes while a second project was
building; with it, cargo skips the registry work and the contention largely goes
away.

If a command genuinely needs a new crate, drop `--offline` for that one
invocation and put it back afterwards.

## Toolchain

- rustc is pinned to **1.97.0** by `rust-toolchain.toml`; MSRV is 1.97.0.
- `wreq` is pinned at **0.15.3** — 0.16 requires rustc 1.98. Do not bump it.
- Member manifests use `<key>.workspace = true`, never a version literal.

## The gate

```bash
cargo build  --offline --workspace --all-features
BROWSER_RS_SKIP_BROWSER_TESTS=1 cargo test --offline --workspace --all-features
cargo clippy --offline --workspace --all-targets --all-features -- -D warnings
cargo fmt --all
```

`cargo deny check` reports `licenses FAILED` for a GPL-3.0 transitive crate. That
is a known open question for a human. Leave it red.

## Things that will waste your time if you do not know them

- **A full `--all-features` test run links ~97 separate test binaries.** Linking
  dominates; running the tests is the fast part. Scope to `-p <crate>` while
  iterating.
- **Do not delete `target/debug/incremental` casually.** It regrows to ~50 GB with
  several concurrent builds, but clearing it forces a cold rebuild that costs
  far more than the disk.
- **`ld: write() failed, errno=28` means the disk is full**, not that your code
  broke. It arrives buried under ~200 lines of harmless `ld: warning:` output and
  reads exactly like a link regression.
- **Stale `cargo` processes at 0.0% CPU are blocked on the package-cache lock.**
  If builds hang, check `ps` before assuming the build is slow.
- Browser tests honour `BROWSER_RS_SKIP_BROWSER_TESTS` and skip cleanly without
  Chrome. Anything launching Chrome inside `browser-cdp` must take a permit from
  `live_u14::LIVE_BROWSERS` — a 3-permit semaphore.

## Naming

The project is **browser-rs**; the façade crate is `browser-rs` with library name
`browser`, so users write `use browser::Document;`. Binaries are `brs`,
`brs-serve` and `brs-mcp`. Do not introduce third-party project names anywhere —
see `docs/AGENTS.md` Rule 0.

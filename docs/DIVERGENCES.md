# Divergences from the Python reference

Every intentional difference in behaviour, in one place, so that none of them is
an accident.

> **Status.** Work unit 20 compiles this file in full from `ARCHITECTURE.md` §9.
> Until then it records only the divergences that already exist in the tree, so
> that no later unit has to remember them.

## Build-level divergences (unit 1)

### `wreq` is pinned to 0.15.x, not 0.16

`ARCHITECTURE.md` §2.3 selects `wreq` 0.16.0 with `wreq-util` 0.2.0. Both of
those releases declare `rust-version = 1.98`, and this workspace is pinned to
1.97.0 in `rust-toolchain.toml`, so the MSRV-aware resolver will not select
them. We pin one minor back:

| Crate | §2 says | Pinned here |
|---|---|---|
| `wreq` | 0.16.0 | **0.15.3** |
| `wreq-util` | 0.2.0 | **0.1.0** |

This matters to work unit 12, which writes the `wreq` backend: `wreq-util` 0.1
exposes a smaller set of browser emulation profiles than 0.2, and the
`Emulation` builder API changed between the two. Check the 0.1 docs rather than
the 0.2 docs. Raising `rust-toolchain.toml` to 1.98 later is the whole of the
upgrade.

### `chromiumoxide` is taken at its default features only

`chromiumoxide`'s `rustls` and `native-tls` features expand to
`chromiumoxide_fetcher/<tls>` using the non-optional syntax, which
*force-enables* the optional `chromiumoxide_fetcher` crate while granting it
only that one feature — never its default `zip8` backend. The result fails to
compile under `--no-default-features`. `browser-cdp` therefore depends on
`chromiumoxide` with its own default (`bytes`) and nothing else; the Chromium
downloader that unit 19 needs comes from a direct `chromiumoxide_fetcher`
dependency in `browser-cli`, with that crate's own defaults intact.

### `serde_json` is built with `preserve_order`

Attribute and key order is load-bearing in several places the reference implementation's tests pin
(`dict_diff` compares ordered key/value tuples; `CrawlStats::to_dict` has an
exact key order; `orjson` preserves insertion order). Enabling
`serde_json/preserve_order` workspace-wide makes `serde_json::Map` an
`IndexMap`, which matches Python `dict` semantics. This is a deliberate global
choice, not an accident of feature unification.

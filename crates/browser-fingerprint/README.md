# browser-fingerprint

Constants and header synthesis shared by the HTTP and browser engines of
`browser-rs`. See `ARCHITECTURE.md` §1.8.

| module | contents |
| --- | --- |
| `impersonate` | `ImpersonateProfile` (38 `curl_cffi` targets), `ALIASES`, `DEPRECATED_ALIASES`, validation |
| `headers` | the precomputed header-profile sampler, `generate_headers`, the `Chrome/[\d.]+` rewrite, driver-version discovery |
| `useragent` | `DEFAULT_UA`, `DEFAULT_BROWSER_UA`, `DEFAULT_CHROME_UA` (`LazyLock`) |
| `ad_domains` | `AD_DOMAINS` (build-time `phf::Set`), `is_domain_blocked`, `DomainBlocklist` |
| `status` | `StatusText`, a 62-entry `phf::Map` with the `"Unknown Status Code"` fallback |
| `domains` | `EXTRA_RESOURCES`, `HARMFUL_ARGS`, `DEFAULT_ARGS`, `STEALTH_ARGS` |

## Assets

Both data tables ship as assets, never as string literals in a `.rs` file:

* `assets/ad_domains.txt` — 3 526 bare domains (Peter Lowe's ad/tracking server
  list, as vendored by the reference implementation). `build.rs` compiles it into a `phf::Set`.
* `assets/header_profiles.json` — 816 self-consistent, correctly ordered header
  sets. `build.rs` validates it; `headers.rs` deserialises it lazily.

Regenerate them with the scripts in `tools/`:

```sh
python3 tools/gen_ad_domains.py /path/to/the reference implementation
python3 tools/gen_profiles.py
```

## Environment hooks

Python locates the host OS and the automation driver through `platform` and
`importlib`; Rust has neither, so those inputs are configurable:

| variable | effect |
| --- | --- |
| `BROWSER_RS_HOST_OS` | override OS detection (`windows`/`macos`/`linux`/`android`/`ios`, or anything else for the "unknown host" branch) |
| `BROWSER_RS_DRIVEN_CHROME_MAJOR` | the Chromium major version to rewrite UAs to, straight up |
| `PLAYWRIGHT_DRIVER_PATH`, `PATCHRIGHT_DRIVER_PATH` | package roots to probe for `driver/package/browsers.json`, in that fallback order |

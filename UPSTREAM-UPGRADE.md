# Codex 0.169.1 / GPT-6.2 upgrade (2026-11-18)

## Verified upstream sources

- Stable CLI: [rust-v0.169.1](https://github.com/openai/codex/releases/tag/rust-v0.169.1), published November 17, 2026. The annotated tag resolves to commit `c376cf7396eff83ce4c6ac61b9553df142e7a0be` (tag object `3f7d3da04eb2afde20a12c59cf4b1b988f64122f`). Do not substitute the newer 0.171 alpha series.
- Models are a separate release: [GPT-6.2 announcement](https://openai.com/index/introducing-gpt-6-2-astra-sol/) and [official changelog](https://learn.chatgpt.com/docs/changelog).
- Exact machine-readable capability source: [bundled models.json](https://github.com/openai/codex/blob/c376cf7396eff83ce4c6ac61b9553df142e7a0be/codex-rs/models-manager/models.json).

The bundled catalogue identifies `gpt-6.2-sol` and `gpt-6.2-astra`, both text/image, with a minimum client version of 0.166.0. Sol declares 500,000 default / 872,000 maximum context; Astra declares 272,000 / 872,000. Both default to low reasoning and list low, medium, high, xhigh, max, ultra. Account access and rollout still come from the authenticated backend catalogue, never a hard-coded synthetic model list.

The wrapper retains its existing ultra filter: upstream describes ultra as automatic delegation, and this wrapper does not run Codex's agent machinery. It does not invent `none` support for either new model, and does not add unmeasured `:long` aliases. Context values remain backend-driven. The existing default request model is deliberately unchanged. No paid model calls or live capability probes were used.

## Source/API audit

All five direct Codex dependencies remain sibling path dependencies. `CODEX_REV` and both websocket fork patches are upgraded together. The dependency tree must remain free of `codex-core`.

Upstream renamed async auth resolution to `apply_auth_headers`, taking and returning the authoritative header map. The wrapper now preserves that map (including session headers), still resolves auth through its existing coordination/tracking, and leaves bounded wrapper-owned 401 recovery unchanged. The newly optional image output selector stays absent, preserving raster-only input/output validation and existing cost/no-retry semantics.

## Reproducible build and test

Use Rust/Cargo 1.99.1 (the existing Dockerfile build version); upstream's toolchain declaration remains 1.95.0. Prepare an isolated directory with the wrapper and the pinned upstream checkout as siblings named `wrapper` and `codex`. Do not switch a shared working checkout just to run tests. Then run:

```sh
cargo test --locked
cargo build --release --locked
cargo tree --locked --prefix none
```

The lockfile is generated against that exact source. The normal container build fetches `CODEX_REV` into its own sibling checkout. No Node tooling is required. The repository references a historical DEPLOY.md, but that file is not present in this checkout.

## Operational constraint: do not overlap shared credentials

ChatGPT AuthManager serializes refresh within a process using a semaphore. In this pin, `login/src/auth/manager.rs::refresh_and_persist_chatgpt_token` exchanges the refresh token before acquiring the persistence transaction's file lock. That lock does not serialize OAuth token exchange across processes. The separate gateway OAuth session lock is not the wrapper's ChatGPT authentication path.

Upstream also acquires `auth.lock` in `login/src/auth/storage.rs::lock_for_write`. That collides with the wrapper's old whole-transaction `auth.lock`, causing a self-deadlock on writes/refresh. The wrapper transaction lock is therefore renamed to `wrapper-auth.lock`: coordinated auth holds it across reload and proactive refresh; 401 recovery holds it across reload, rejected-token comparison, and refresh; login/logout/reminder mutation paths also participate. Existing keyless lock/recovery tests exercise the collision and now use the distinct lock.

**Migration requirement:** do not overlap credential owners from wrapper <=1.11 and >=1.12. Their transaction lock names differ, so mixed-version OAuth refreshes are not serialized. Use a drained stop-before-start/Recreate transition, not a shared-credential rolling canary. Subsequent >=1.12 workers can coordinate with each other if the volume supports advisory file locking. An unrelated Codex CLI is not covered by the wrapper transaction lock. No live canary was run to verify storage semantics.

The old service must still be drained deliberately because this source upgrade does not retroactively add graceful termination to an already running binary. Build/tag/deploy coordination belongs to the release owner; this change performs no deployment or live model replay.

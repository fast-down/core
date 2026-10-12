# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

The workspace publishes four crates that are versioned independently:

- [`fast-steal`](crates/fast-steal)
- [`fast-pull`](crates/fast-pull)
- [`fast-down`](crates/fast-down)
- [`fast-down-api`](crates/fast-down-api)

Earlier history predates this changelog; see the
[commit history](https://github.com/fast-down/core/commits/main) for changes
made before it was introduced.

## [Unreleased]

### Added

- **`fast-steal`**: optional lock backend. `std` builds use `parking_lot::Mutex`;
  `no_std` builds select `spin::Mutex` via the `spin` feature. The crate now
  compiles for genuine `no_std` targets (for example `thumbv7em-none-eabi`).
- **`fast-down-api`**: multi-range progress recording for resumed downloads, so
  an interruption that leaves several written ranges in the `.part` file is
  preserved across a later resume.

### Changed

- **`fast-steal`**: `std`/`spin` are mutually exclusive feature choices; exactly
  one lock backend must be selected.
- Error and event enums (`StateError`, `PlanError`, `HttpError`, `Event`,
  `TerminationReason`, `WorkerState`, `ReqwestResponseError`) are
  `#[non_exhaustive]`, so new variants can be added without a breaking change.
- **`fast-down`**: `reqwest` error types are wrapped behind crate-owned types
  (`BuildClientError`, `RequestError`, `StatusCodeError`, `ReqwestResponseError`)
  instead of leaking `reqwest::Error` from the public API.
- **`fast-down-api`**: `State`/`Ready` three-phase model with `StartPolicy`.

### Removed

- **`fast-down-api`**: the `create_cancellation_token` helper. `CancellationToken`
  is re-exported, so callers can use `CancellationToken::new()` directly.

### Documentation

- `#![warn(missing_docs)]` is enforced workspace-wide, and every public item is
  documented.
- Each crate enables `all-features` for its docs.rs build so feature-gated API is
  visible in the rendered documentation.

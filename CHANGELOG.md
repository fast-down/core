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

## [fast-down 9.0.0] - 2026-11-19

### Added

- **`fast-down`**: `Proxy::System` now honours the operating system's proxy
  settings. The `fast-puller` feature enables `reqwest`'s `system-proxy`, so the
  default proxy mode no longer reads only the environment variables.

### Changed

- **`fast-down`**: every request is pinned to `Accept-Encoding: identity`. A
  content coding would make the body bytes something other than the resource's
  bytes at the offsets `Content-Range` names, so a caller-supplied
  `Accept-Encoding` is overwritten rather than honoured — even when another crate
  enables `reqwest`'s decompression features.
- **`fast-down`**: `accept_invalid_certs` / `accept_invalid_hostnames` without
  the `reqwest-tls` feature now fail with `BuildClientError::MissingFeature`
  instead of being silently ignored.
- **`fast-down`**: `reqwest` error types are wrapped behind crate-owned types
  (`BuildClientError`, `RequestError`, `StatusCodeError`, `ReqwestResponseError`)
  instead of leaking `reqwest::Error` from the public API.
- **`fast-down`**: `BuildClientError` and `ReqwestResponseError` are
  `#[non_exhaustive]`, so new variants can be added without a breaking change.
- **`fast-down`**: file identity is compared with `FileId::matches`, which checks
  a validator only when both sides carry one. A `206` reply that omits
  `Last-Modified` no longer reads as changed content. `FileId` and `UrlInfo` no
  longer implement `PartialEq`/`Eq`, so equality cannot be used by accident.
- **`fast-down`**: a response body that stops before the requested range now
  reports `HttpError::Irrecoverable` instead of ending as a clean EOF. For a
  chunked `206` reply the complete length in `Content-Range` is the expected end,
  so a short body is re-pulled from the cursor the engine already accepted.

### Documentation

- `missing_docs` is enforced workspace-wide, and every public item is documented.
- Each crate enables `all-features` for its docs.rs build so feature-gated API is
  visible in the rendered documentation.

## [fast-pull 8.0.0] - 2026-11-19

### Changed

- The write queue is bounded by what the channel accepts, instead of a queue
  depth that multiplied with the average chunk size.
- A push must match the range it claims, so a body that does not cover the
  requested span is refused instead of written at the wrong offset.
- A pull task that panics cancels the session and reports the panic, instead of
  leaving the download running with one worker gone.
- A single-stream pull gives up after a configurable number of unproductive
  attempts (`max_unproductive_attempts`; `0` disables the guard), so a server
  that keeps truncating no longer restarts from zero forever.
- The progress stamp only moves forward.
- `InvertIter` stays exhausted once its scan ends, so a fused inner iterator no
  longer produces a fabricated gap.

## [fast-steal 8.0.0] - 2026-11-19

### Changed

- Registered tasks are held by value and `WeakTask` is gone; the public surface is
  narrower and a range snapshot is exposed in one place.
- A sweep reclaims a dead worker's range instead of leaving it unassigned.
- Growth slots are spent on real work rather than on tasks that will not run.
- `std` and `spin` are mutually exclusive lock backends: `std` builds use
  `parking_lot::Mutex`, `no_std` builds select `spin::Mutex`, and exactly one must
  be chosen. The crate compiles for genuine `no_std` targets (for example
  `thumbv7em-none-eabi`).

## [fast-down-api 0.2.0] - 2026-11-19

### Added

- Multi-range progress recording for resumed downloads, so an interruption that
  leaves several written ranges in the `.part` file is preserved across a later
  resume.

### Changed

- The `State` → `Ready` → `Run` three-stage model: `State::build` resolves a
  download without writing anything, the returned `Ready` is inspected and then
  started, and a `StartPolicy` decides what happens to an existing `.fd`/`.part`.
- `StateError`, `PlanError`, `Event`, `QueueEvent`, `QueueError`,
  `TerminationReason`, and `WorkerState` are `#[non_exhaustive]`, so new variants
  can be added without a breaking change.
- Resume validation compares the recorded identity with `FileId::matches`, field
  by field, so a `.fd` that recorded `Last-Modified` while the new `200` omits it
  resumes instead of being refused as a changed file.
- Asking for `accept_invalid_certs` / `accept_invalid_hostnames` in a build
  without `reqwest-tls` now surfaces as
  `PlanError::BuildClient(BuildClientError::MissingFeature)` (and the matching
  `Event::BuildClientError`) instead of being silently ignored.

### Removed

- `create_cancellation_token`. `CancellationToken` is re-exported, so callers can
  use `CancellationToken::new()` directly.
- The `brotli`, `deflate`, `gzip`, and `zstd` features on the `reqwest`
  dependency, removing transparent decompression (and its dependencies) from the
  dependency graph. The engine needs the raw resource bytes.

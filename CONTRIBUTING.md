# Contributing

Thanks for your interest in improving `fast-down`. This document covers the
basics of building, testing, and submitting changes.

## Workspace layout

The repository is a Cargo workspace with four published crates, layered from the
bottom up:

| Crate           | Role                                                              |
| --------------- | ----------------------------------------------------------------- |
| `fast-steal`    | `no_std` work-stealing task scheduler.                            |
| `fast-pull`     | Pull/push streaming engine (`Puller` / `Pusher` abstractions).     |
| `fast-down`     | HTTP downloader built on `fast-pull` (`reqwest`, URL info, proxy). |
| `fast-down-api` | High-level, state-machine wrapper for applications and FFI.        |

Changes should generally flow downward: a fix in `fast-steal` should not depend
on anything from a higher layer.

## Requirements

- Rust **1.91** or newer (the workspace MSRV; see `rust-version` in
  `Cargo.toml`).
- A `no_std` target for the `fast-steal` checks, for example:

  ```sh
  rustup target add thumbv7em-none-eabi
  ```

- Optional CI tools used locally: `cargo-hack`, `cargo-udeps` (nightly),
  `cargo-deny`, `cargo-semver-checks`.

## Build, test, and lint

Run the same checks CI runs before opening a pull request. A full local pass:

```sh
# Formatting
cargo fmt --all

# Lints. `fast-steal`'s `std`/`spin` lock backends are mutually exclusive.
cargo hack clippy --all-targets --feature-powerset \
  --mutually-exclusive-features std,spin --at-least-one-of std,spin

# Tests
cargo hack test --feature-powerset --depth 1 \
  --mutually-exclusive-features std,spin --at-least-one-of std,spin
cargo test --all-features

# Documentation
cargo doc --no-deps --all-features
```

`fast-steal` additionally builds on a bare-metal target. Verify the `no_std`
path still compiles when you touch that crate:

```sh
cargo build -p fast-steal --no-default-features --features spin \
  --target thumbv7em-none-eabi
```

The workspace denies warnings (`RUSTFLAGS="-D warnings"`), and clippy runs with
the `all`, `cargo`, `nursery`, and `pedantic` groups at `deny`. Fix lints rather
than silencing them; if a lint genuinely does not apply, add a targeted
`#[allow(...)]` with a short reason.

## Coding conventions

- Keep the public API documented: `missing_docs` is denied workspace-wide.
- Prefer crate-owned error types over leaking third-party types from a public
  signature.
- New enum variants on public error / event types require the enum to be
  `#[non_exhaustive]`.
- `fast-steal` is `no_std`: it must use `core`/`alloc`, never `std`, outside of
  tests.
- Match the surrounding style; `cargo fmt` is authoritative.

## Commits and pull requests

- Keep changes focused; one logical change per pull request where practical.
- Write clear commit messages. The repository uses conventional-commit-style
  prefixes (`feat:`, `fix:`, `refactor:`, `docs:`, `test:`, `chore:`).
- Update [`CHANGELOG.md`](CHANGELOG.md) under `## [Unreleased]` for any
  user-visible change.
- Make sure the checks above pass. CI runs formatting, clippy, tests across
  Linux/macOS/Windows on stable/beta/nightly, `cargo-deny`, `cargo-udeps`,
  documentation, and `cargo-semver-checks`.

## Versioning

Each crate is versioned independently and follows Semantic Versioning. Public API
changes to a published crate must be reflected in its `CHANGELOG.md` entry and
will be checked by `cargo-semver-checks` in CI.

## License

By contributing, you agree that your contributions are licensed under the
[MIT License](LICENSE), the same terms as the project.

# Security Policy

## Supported Versions

The workspace publishes four independently versioned crates. Security fixes are
applied to the latest published release of each. Older major versions are not
maintained.

| Crate           | Supported            |
| --------------- | -------------------- |
| `fast-steal`    | latest `7.x`         |
| `fast-pull`     | latest `7.x`         |
| `fast-down`     | latest `7.x`         |
| `fast-down-api` | latest `0.x`         |

If you are on an older version, please upgrade before reporting, as the issue
may already be fixed.

## Reporting a Vulnerability

**Please do not report security vulnerabilities through public GitHub issues,
pull requests, or discussions.**

Instead, use GitHub's private vulnerability reporting:

1. Go to the repository's **Security** tab.
2. Click **Report a vulnerability**.
3. Describe the issue, including a minimal reproduction if possible.

<https://github.com/fast-down/core/security/advisories/new>

If you cannot use GitHub's private reporting, email the maintainers at
`me@s121.top`.

Please include, where applicable:

- the affected crate and version,
- a description of the impact and the conditions required to trigger it,
- a proof of concept or reproduction steps,
- any suggested fix.

## What to Expect

- We aim to acknowledge a report within a few days.
- We will investigate and keep you updated on the assessment and a fix timeline.
- We will credit you in the release notes once a fix is published, unless you
  prefer to remain anonymous.

## Scope

This project downloads untrusted remote content to local storage and speaks
HTTP(S) to arbitrary servers. Reports that are especially relevant include:

- remote code execution or memory-safety issues,
- path traversal or writing outside the configured save directory,
- TLS verification or certificate-validation bypasses,
- credential leakage (for example `Authorization` / `Cookie` headers sent to a
  different origin after a redirect).

Reports about the behavior of a remote server, or about a service you do not
control, are generally out of scope.

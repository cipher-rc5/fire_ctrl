# Security Policy

## Supported Versions

This project is pre-1.0 and only the latest commit on `master` is supported.
Older tags and branches do not receive security backports.

## Reporting a Vulnerability

Please do not open public GitHub issues for security reports.

The preferred channel is GitHub Security Advisories:

- https://github.com/cipher-rc5/fire_ctrl/security/advisories/new

If you cannot use the advisory form, email the maintainer:

- 157530642+cipher-rc5@users.noreply.github.com

Include in your report:

- A description of the issue and its impact.
- Steps to reproduce, or a proof-of-concept if possible.
- Affected commit hashes or version tags.
- Any suggested remediation.

We aim to acknowledge reports within 7 days.

Once a fix is available, we will coordinate disclosure with the reporter and
credit them in the changelog unless they request anonymity.

## Scope

In scope:

- The `fire_ctrl` binary (API server and worker).
- Configuration parsing and authentication paths.
- Migrations and SQL handling under `migrations/`.

Out of scope:

- Vulnerabilities in upstream dependencies (report those upstream).
- Issues that require an attacker to already have shell access on the host.
- Denial-of-service from unauthenticated traffic on a public deployment with
  no rate limiting configured (operators are responsible for fronting the
  service with appropriate gateways and rate limits).

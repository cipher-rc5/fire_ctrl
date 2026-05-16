# CI and release checks

This repository uses manual GitHub Actions workflows to avoid automatic
billable runs.

## Workflow policy

- `CI` runs only via `workflow_dispatch`
- `Security` runs only via `workflow_dispatch`
- `Branch Protection` runs only via `workflow_dispatch`

## Local pre-PR checklist

Run these before opening or merging a PR:

```bash
cargo fmt --all
cargo check --all-targets
cargo clippy --all-targets --all-features -- -D warnings
cargo test --bins --locked
```

Or use task aliases:

```bash
just fmt
just check
just lint
just test
just security
```

## Manual GitHub checks

When you want hosted verification:

1. Open Actions in GitHub
2. Trigger `CI` manually
3. Trigger `Security` manually

## Release checklist

```bash
cargo fmt --all
cargo check --all-targets
cargo clippy --all-targets --all-features -- -D warnings
cargo test --bins --locked
just security
just docs
```

Then trigger manual `CI` and `Security` workflows for a final hosted pass.

## Release artifacts

Releases are produced manually via the `Release` workflow
(`.github/workflows/release.yml`), triggered with `workflow_dispatch`.

### Why manual-only

`fire_ctrl` is a single-maintainer project. An always-on push/tag trigger
would expose the GitHub Actions billing account to runaway costs from
accidental tag pushes, dependency-bot floods, or compromised CI tokens.
Keeping the workflow `workflow_dispatch`-only means the human maintainer
explicitly opts into each billable build, which is the right tradeoff
for this project's risk profile.

### What the workflow produces

For the user-supplied tag/ref it:

1. Checks out the requested ref.
2. Runs `cargo build --release --locked` (the lockfile gate ensures a
   reproducible dependency set).
3. Computes `sha256sum target/release/fire_ctrl > fire_ctrl.sha256`.
4. Uploads `target/release/fire_ctrl` and `fire_ctrl.sha256` as workflow
   artifacts.

### Verifying a downloaded binary

After downloading both artifacts from the Actions run page:

```bash
sha256sum -c fire_ctrl.sha256
```

The checksum file is generated inside the same workflow run as the
binary, so verification only confirms transit integrity (download was
not corrupted or swapped). For supply-chain provenance, also inspect
the workflow run logs to confirm the source ref, the runner image, and
the Rust toolchain version that produced the binary.

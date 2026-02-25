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
cargo check
cargo clippy -- -D warnings
cargo test
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
just fmt
just check
just lint
just test
just security
just docs
```

Then trigger manual `CI` and `Security` workflows for a final hosted pass.

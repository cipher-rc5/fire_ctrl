# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Removed

- `lapin` (RabbitMQ AMQP client) dependency. RabbitMQ was only used for a
  startup ping and a health-check probe; actual job dispatch already runs
  through Postgres (`nuq.queue_scrape` with `FOR UPDATE SKIP LOCKED`).
- `NUQ_RABBITMQ_URL` configuration variable and the `RabbitMqConfig` struct.
- `rabbitmq` Homebrew package from `just install` and the `rabbit-clean`
  cleanup recipe from the justfile.

### Fixed

- Stale Rust toolchain references in docs (`1.93`/`1.94` -> `1.95`) so
  `Cargo.toml`, `rust-toolchain.toml`, and prose stay in sync.
- Internal consistency of `deny.toml` schema and the branch-protection
  workflow noted by the production-readiness review.

### Changed

- README and `docs/architecture.md` mermaid diagrams now reflect the real
  queueing topology (Postgres-backed `nuq.queue_scrape`), instead of
  showing a non-existent RabbitMQ broker in the dispatch path.
- `/health` endpoint no longer reports a `rabbitmq` component; it checks
  Postgres, Redis, and the LLM provider only.

## [0.1.0] - UNRELEASED

Initial release placeholder.

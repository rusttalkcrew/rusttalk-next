# Development

Run `make` for all targets.

- `make lint`: clippy with warnings as errors, plus `rustfmt --check`.
- `make fmt`: format all code.
- `make test`: unit and integration tests.

CI (`.github/workflows/ci.yml`) runs `make lint test` on Linux and the client unit tests on Windows.

Dev data lives in `.dev/` (gitignored).

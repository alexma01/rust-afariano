# Contributing

Thanks for helping. Bug reports, small fixes and focused features are all
welcome; for anything large, open an issue first so we can agree on the shape.

## Build and test

You need Rust (stable, edition 2024), `git`, and for the server a Postgres you
can reach.

```sh
cargo build --workspace
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all
```

CI runs the same three checks on every pull request; please run them first.

To run the server locally, copy `.env.example` to `.env`, point
`RUST_AFARIANO_DATABASE_URL` at your Postgres, then:

```sh
set -a; . ./.env; set +a
cargo run -p rust-afariano
```

`docker compose up -d` gives you Postgres and a released server instead.

## Layout

| Crate | What it is |
|---|---|
| `crates/server` (`rust-afariano`) | Web UI (axum + askama), git over SSH (russh) and smart HTTP, the JSON and runner APIs, the pipeline scheduler |
| `crates/runner` (`rust-afariano-runner`) | Polls the server and runs jobs with the `docker` or `shell` executor |
| `crates/ci` (`rust-afariano-ci`) | Parses the `.gitlab-ci.yml` subset into a pipeline plan; pure, no I/O |
| `crates/proto` (`rust-afariano-proto`) | Wire types shared by server and runner |

`docs/DESIGN.md` explains the architecture and the CI semantics.

## Pull requests

- One topic per pull request, with a description of what changes for users.
- Add or update tests with behaviour changes; the CI parser in particular is
  covered by tests for every keyword it supports.
- Keep the memory budget in mind: the server is meant to stay well under
  200 MB, so prefer streaming over buffering and avoid new long-lived caches.
- New `.gitlab-ci.yml` keywords should follow GitLab's documented behaviour, or
  fail with a clear message when only partly supported.

No CLA and no sign-off needed. By contributing you agree that your work is
licensed under the project's licence, MIT OR Apache-2.0.

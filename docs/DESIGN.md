# rust-afariano design

Git hosting + CI pipelines, nothing else. Target: the server stays under
**200 MB RAM** (container `--memory 200m`) on a 1 GB VM; jobs run elsewhere.

```
 browser ──HTTPS──┐                         ┌── runner (Mac, Linux…) polls ──┐
 git CLI ──HTTPS──┤  nginx-core :443 ──▶ rust-afariano :8080             │
 git CLI ──SSH :2222──────────────────▶ rust-afariano :2222             │
                                          │   Postgres                     │
                                          │   data dir: repos/ logs/       │
                                          └──────── /api/v1/runner/* ◀────┘
```

## Crates

| Crate | What |
|---|---|
| `rust-afariano-proto` | runner ⇄ server JSON types (`crates/proto/src/lib.rs` is the contract) |
| `rust-afariano-ci` | `.gitlab-ci.yml` subset → pipeline plan. Pure, no I/O |
| `rust-afariano` | axum web UI + git smart HTTP + russh SSH + runner API + scheduler |
| `rust-afariano-runner` | polls the server, runs jobs with `docker` or `shell` executor |

## Auth

- **Passwords**: Argon2id (19 MiB, 2 passes; at most two hashes at once), 5 failed
  attempts per email per 15 minutes, a dummy hash for unknown emails. The first
  administrator is created through a one-time setup link printed at startup while
  there are no accounts.
- **OpenID Connect**: discovery, authorization code + PKCE (S256) + `state` +
  `nonce`. The ID token comes straight from the token endpoint over TLS, so per
  OIDC Core §3.1.3.7 its signature is not checked; `iss`, `aud`, `exp` and `nonce`
  are. `email_verified: false` is refused. Client authentication:
  `client_secret_basic`, or `client_secret_post` when that is all the provider offers.
- **Rich auth servers** (auth.hartwellbridge.com and compatible): a non-standard
  PKCE login (`/api/request-login`, `/auth/token` with `code_verifier`, a `user`
  object in the answer). Enabled with `RUST_AFARIANO_RICH_AUTH_URL`.
- SSO identities map to accounts by email. Unknown emails get an account only when
  `RUST_AFARIANO_ALLOWED_EMAILS` matches (exact or `*@domain`); the first account
  ever created is an administrator.
- Sessions are random tokens stored hashed in Postgres (30 days by default);
  cookies are `HttpOnly`, `SameSite=Lax`, `Secure` on HTTPS. Cookie-authenticated
  POSTs must carry our own `Origin` (or `Referer`).
- git over HTTPS: basic auth, password = personal access token (`rfp_…`), or job
  token (`rfj_…`, user `rust-afariano-ci`, read-only, only the job's repo, only
  while it runs). Without valid credentials every repository path answers 401.
- git over SSH: public keys added in the UI. Disabled accounts lose web, token and
  SSH access at once.
- Runners: `rfr_…` token shown once when the runner is created (admins only).
- All tokens are stored as SHA-256. CI/CD variable values are stored encrypted
  with AES-256-GCM (`enc:v1:` + base64(nonce ‖ ciphertext)).

## CI subset (`rust-afariano-ci`)

Supported keywords — exactly what the existing pipelines use, plus a few cheap ones:

- top level: `workflow: {rules}`, `stages`, `variables`, `default`
  (`image`, `services`, `before_script`, `after_script`, `tags`, `timeout`,
  `interruptible`), and the legacy top-level `image`, `services`,
  `before_script`, `after_script`.
- hidden jobs (`.name`) and `extends` (string or list, recursive, deep-merge of
  maps, later wins, arrays replaced — GitLab semantics); YAML anchors/aliases
  and `<<` merge keys.
- job: `stage` (default `test`), `image` (string or `{name, entrypoint}`),
  `services` (string or `{name, alias, entrypoint, command, variables}`),
  `before_script`/`script`/`after_script` (string or list, nested lists
  flattened), `variables` (scalar or `{value, description, expand}`), `needs`
  (strings or `{job, optional}`), `rules` (`if`, `when`, `allow_failure`,
  `variables`; `changes`/`exists` → error "not supported"), `only`/`except`
  (refs list incl. `branches`, `tags`, `/regex/`), `when` (`on_success`,
  `on_failure`, `always`, `manual`, `never`), `allow_failure` (bool or
  `{exit_codes}` → bool true), `environment` (string or `{name, url}`),
  `resource_group`, `timeout`, `tags`, `interruptible`, `retry` (int or `{max}`).
- unsupported keys that change behaviour (`include`, `trigger`, `parallel`,
  `artifacts`, `cache`, …) fail the pipeline with a clear message; harmless ones
  (`coverage`, `description`) are ignored.
- `stages` default `[build, test, deploy]`; `.pre` first and `.post` last always exist.
- `rules:if` grammar: `$VAR`, `"str"`/`'str'`, `null`, `/re/flags`, `==`, `!=`,
  `=~`, `!~`, `&&`, `||`, `!`, parentheses. Undefined var = null; `$VAR` alone =
  defined and non-empty. Regex = Rust `regex` (RE2 syntax), flag `i` supported.
- Rule evaluation sees: predefined vars + project/pipeline variables (given by
  the caller) + global YAML variables + the job's YAML variables (caller vars win).
- `workflow:rules`: first match decides; no match → no pipeline; `when: never`
  → no pipeline. No `workflow` → pipeline runs.
- Job `rules`: first matching rule applies its `when` (default `on_success`),
  `allow_failure`, `variables`; no match → job not added. `when: manual` from
  rules defaults `allow_failure: false`; a job-level `when: manual` defaults
  `allow_failure: true` (GitLab semantics).
- Timeouts: `30m`, `1h`, `1h 30m`, `90 minutes`, `2 hours`, `3600` (seconds).

## Scheduling (server)

Job statuses: `created` (waiting on deps) → `pending` (runner may take it) →
`running` → `success`/`failed`/`canceled`; also `skipped`, `manual`.

- deps = `needs` jobs if `needs` is given, else every job in earlier stages.
- deps done when all are terminal, or `manual` with `allow_failure`.
- deps ok when none `failed` without `allow_failure`/`canceled`.
- `on_success`: ok → pending, else skipped. `on_failure`: not ok → pending,
  else skipped. `always` → pending. `manual`: ok → manual, else skipped.
- `resource_group`: at most one running job per (repo, group).
- Assignment: one Postgres advisory lock, job tags ⊆ runner tags, untagged jobs
  only to `run_untagged` runners. Runners long-poll; a `tokio::Notify` wakes them.
- Lost runner: a running job with no log/heartbeat for 10 min fails
  (`runner_system_failure`).

## Runner

- Config: `~/.config/rust-afariano-runner/config.toml` (or `/etc/rust-afariano-runner/config.toml`
  as root): `url`, `token`, `executor`, `concurrent`, `builds_dir`,
  `[docker] default_image, privileged, volumes, pull_policy`.
- `register --url --token` (verifies and writes config), `run`, `install`
  (launchd LaunchAgent on macOS so Docker Desktop's socket is reachable; systemd
  unit on Linux), `uninstall`.
- Job steps: prepare build dir → git fetch/checkout at SHA (host git, job token)
  → (docker) network + services + wait for service ports → job container
  (image entrypoint kept unless overridden) running `sh -c`-style generated
  script from a mounted file, stdin `/dev/null` → `after_script` → cleanup.
- Script: `set -e`, each line echoed green as `$ cmd` like GitLab.
- Docker: per-job network `rust-afariano-job-<id>`; services get the job's variables,
  aliases from the image name (`docker:29-dind` → `docker`;
  `reg.io/a/b:1` → `reg.io__a__b` and `reg.io-a-b`) plus `alias`; config
  `volumes` (default `["/certs/client"]`) become per-job volumes mounted into
  job and services; the build dir is bind-mounted at `/builds/<project_path>`
  in job and services; `privileged` applies to both.
- File variables written to a per-job temp dir mounted at `/rust-afariano/files`.
- Logs streamed every ~1 s (`PUT …/log?offset=`), masked values replaced with
  `[MASKED]` (carry-over buffer so a secret split across chunks is still masked).
- Timeout and cancel kill the containers/process group.

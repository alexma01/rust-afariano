# Changelog

All notable changes to this project are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and versions follow
[Semantic Versioning](https://semver.org/).

## [0.1.3] - 2026-11-01

### Fixed

- The SSH server offered password authentication it always rejects, so `ssh` and
  `git` prompted for a password when no registered key matched. It now offers
  public keys only and fails with `Permission denied (publickey)`.
- The **Manage** menu on the Users page opened inside the table's box and was cut
  off, so its actions and password form could not be reached.

## [0.1.2] - 2026-11-01

### Fixed

- The account page (`/-/profile`) repeated its password form inside the page
  title, the sidebar and the breadcrumbs, breaking the layout.
- Long clone URLs no longer overflow the settings column.

## [0.1.1] - 2026-11-01

### Fixed

- The server refused to start when `RUST_AFARIANO_SECRET_KEY` (or other optional
  settings) was set to an empty string, which is what `docker-compose.yml` passes
  when you leave it unset. Empty values now mean "not set".

## [0.1.0] - 2026-11-01

First public release.

### Added

- Git hosting over SSH (embedded server, keys added in the web UI) and smart
  HTTP (personal access tokens), with a web UI to browse code, commits and
  diffs, and README rendering.
- Pipelines from a `.gitlab-ci.yml` subset: `workflow:rules`, `rules:if`,
  `only`/`except`, `stages`, `needs`, `extends` and hidden templates, `default`,
  `image`, `services` (including `docker:dind`), `variables`, `before_script`,
  `after_script`, `environment`, `resource_group`, `timeout`, `tags`,
  `when: manual`, `allow_failure`, `retry`, YAML anchors and merge keys.
  Unsupported keywords fail with a clear message.
- Pipelines start on push, by hand from the UI (with variables), or through the
  API; jobs can be retried, canceled and played; logs stream live.
- CI/CD variables per repository: file, protected, masked and
  environment-scoped, encrypted at rest.
- Runners for Linux and macOS with `docker` and `shell` executors. They only
  make outgoing HTTPS requests, so they work behind NAT; `install` sets them up
  as a systemd unit or a launchd agent.
- Local accounts with passwords, and single sign-on with any OpenID Connect
  provider.
- JSON API with personal access tokens: create repositories and runners, set
  variables, start pipelines and read their status.
- Small footprint: about 10 MB of RAM idle, under 100 MB while serving two
  clones of a 216 MB repository at once.
- Container image, `docker-compose.yml`, and prebuilt runner binaries.

[0.1.3]: https://github.com/frank10gm/rust-afariano/releases/tag/v0.1.3
[0.1.2]: https://github.com/frank10gm/rust-afariano/releases/tag/v0.1.2
[0.1.1]: https://github.com/frank10gm/rust-afariano/releases/tag/v0.1.1
[0.1.0]: https://github.com/frank10gm/rust-afariano/releases/tag/v0.1.0

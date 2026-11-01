# Changelog

All notable changes to this project are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and versions follow
[Semantic Versioning](https://semver.org/).

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

[0.1.0]: https://github.com/frank10gm/rust-afariano/releases/tag/v0.1.0

# Changelog

All notable changes to this project are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and versions follow
[Semantic Versioning](https://semver.org/).

## [0.4.1] - 2026-11-02

### Fixed

- Pushes over HTTPS larger than about 1 MB stalled behind nginx: the server sent
  its response header before reading the push, and nginx (with
  `proxy_request_buffering off`) stops forwarding the request body once it has
  passed the header on. git then waited for the rest of the pack forever and held
  the repository's push lock, so later pushes timed out too. git's answer now
  starts only once it has read the whole request.

## [0.4.0] - 2026-11-02

### Added

- The JSON API covers the rest of the web UI ([docs/API.md](docs/API.md)):
  repository settings (`GET`/`PATCH /api/v1/repos/{ns}/{name}`, and settings on
  create), members, listing and deleting CI/CD variables (never their values), job
  logs, play/retry/cancel for jobs and retry/cancel for pipelines, and for
  administrators users, tokens for service accounts and runners (list, change
  tags, untagged, paused).

### Fixed

- After an upgrade, browsers could keep the previous `app.css`, `app.js` and icons
  for up to an hour (the new eye icon showed as a blank). Their URLs now carry the
  version.

## [0.3.0] - 2026-11-02

### Added

- Maintainers and administrators can see CI/CD variable values: **Show** next to a
  hidden value (masked, file or long) loads it in place, with **Copy** and **Hide**.
  The value is fetched on demand, never embedded in the page or cached, and each
  view is logged with the user and the key. Without JavaScript it opens as plain text.

### Changed

- On narrow screens the variables table moves the environment under the key.

## [0.2.0] - 2026-11-02

### Added

- Per-repository access. Each repository's *Settings → Members* lists who can open
  it, as *read* (browse, clone, see pipelines and job logs), *write* (also push,
  run pipelines and jobs) or *maintain* (also settings, CI/CD variables and
  members). Administrators open every repository. The web UI, git over HTTP and
  SSH, and the JSON API all check it; a repository without access answers 404.

### Changed

- Only administrators create and delete repositories (web and `POST /api/v1/repos`).
- Upgrading keeps everyone's access: every existing account becomes a *maintain*
  member of every existing repository. Accounts added later start with none.

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

[0.4.1]: https://github.com/frank10gm/rust-afariano/releases/tag/v0.4.1
[0.4.0]: https://github.com/frank10gm/rust-afariano/releases/tag/v0.4.0
[0.3.0]: https://github.com/frank10gm/rust-afariano/releases/tag/v0.3.0
[0.2.0]: https://github.com/frank10gm/rust-afariano/releases/tag/v0.2.0
[0.1.2]: https://github.com/frank10gm/rust-afariano/releases/tag/v0.1.2
[0.1.1]: https://github.com/frank10gm/rust-afariano/releases/tag/v0.1.1
[0.1.0]: https://github.com/frank10gm/rust-afariano/releases/tag/v0.1.0

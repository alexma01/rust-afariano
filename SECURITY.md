# Security

## Reporting a vulnerability

Please report vulnerabilities privately through GitHub:
**Security → Report a vulnerability** on
https://github.com/frank10gm/rust-afariano/security/advisories/new.
Do not open a public issue for them.

Include what you found, how to reproduce it, and the version (or commit). You
should get a first answer within a week. Once a fix is released the advisory is
published with credit to you, unless you prefer otherwise.

## Supported versions

Only the latest release receives security fixes.

## Security model

Know these before you deploy:

- **Trusted team.** Every signed-in user can read and write every repository,
  change its CI/CD variables, and manage runners. There are no per-repository
  permissions yet; give accounts only to people you would give push access to
  everything.
- **Runners execute code from the repository.** Any job in `.gitlab-ci.yml`
  runs on whichever runner picks it up, with that runner's access. A runner
  registered with `--privileged` (needed for `docker:dind` services) is
  root-equivalent on its host. Run runners on machines dedicated to CI, and
  never share one between teams that do not trust each other.
- **CI/CD variables** are encrypted at rest with the server's secret key, and
  decrypted when a job starts. Protected variables reach only pipelines on
  protected branches and tags. Masking hides exact values in job logs; it is not
  a guarantee (a job can always print a secret transformed).
- **Tokens** (personal access tokens, runner tokens, job tokens) are stored as
  SHA-256 hashes. Job tokens are valid only while their job runs and only for
  cloning their own repository.
- **Put TLS in front.** The server speaks plain HTTP; run it behind a reverse
  proxy with HTTPS (see `contrib/nginx.conf.example`) and set
  `RUST_AFARIANO_PUBLIC_URL` to the `https://` address so cookies are marked
  `Secure`.

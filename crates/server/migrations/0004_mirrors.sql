-- Pull mirrors: repositories whose code and .gitlab-ci.yml live on another server
-- (GitHub, Bitbucket, …). rust-afariano fetches them itself and refuses pushes to them.
-- NULL: an ordinary repository, pushed to directly.
ALTER TABLE repos ADD COLUMN mirror_url TEXT;

-- GitLab's "Prevent outdated deployment jobs": a job with an environment does not run
-- when a newer pipeline already deployed to that environment (a retry still may: rollback).
ALTER TABLE repos ADD COLUMN prevent_outdated_deploys BOOLEAN NOT NULL DEFAULT TRUE;

CREATE TABLE mirrors (
    repo_id         BIGINT PRIMARY KEY REFERENCES repos(id) ON DELETE CASCADE,
    -- Key pair for ssh URLs: the public half goes on the upstream as a read-only
    -- deploy key / access key; the private half is encrypted like CI/CD variables.
    ssh_private_key TEXT NOT NULL,
    ssh_public_key  TEXT NOT NULL,
    -- The upstream's SSH host keys (known_hosts lines), pinned at the first contact.
    known_hosts     TEXT NOT NULL DEFAULT '',
    -- For https URLs: `token` or `user:token`, encrypted. NULL for a public upstream.
    https_token     TEXT,
    -- Comma separated branch patterns (`*` wildcard) to copy; every tag is copied.
    branches        TEXT NOT NULL DEFAULT '*',
    -- Signs the upstream's webhook calls (encrypted).
    webhook_secret  TEXT NOT NULL,
    -- Token that may write commit statuses on the upstream (encrypted). NULL: none sent.
    status_token    TEXT,
    synced_at       TIMESTAMPTZ,
    checked_at      TIMESTAMPTZ,
    error           TEXT,
    hook_at         TIMESTAMPTZ,
    hook_error      TEXT,
    status_at       TIMESTAMPTZ,
    status_error    TEXT
);
CREATE INDEX mirrors_checked ON mirrors (checked_at NULLS FIRST);

-- The pipeline status last sent to the upstream as a commit status.
ALTER TABLE pipelines ADD COLUMN reported_status TEXT;

CREATE TABLE users (
    id            BIGSERIAL PRIMARY KEY,
    email         TEXT NOT NULL UNIQUE,
    name          TEXT NOT NULL DEFAULT '',
    created_at    TIMESTAMPTZ NOT NULL DEFAULT now(),
    last_login_at TIMESTAMPTZ
);

CREATE TABLE sessions (
    token_hash BYTEA PRIMARY KEY,
    user_id    BIGINT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    expires_at TIMESTAMPTZ NOT NULL
);
CREATE INDEX sessions_expires ON sessions (expires_at);

CREATE TABLE ssh_keys (
    id           BIGSERIAL PRIMARY KEY,
    user_id      BIGINT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    title        TEXT NOT NULL,
    public_key   TEXT NOT NULL,
    fingerprint  TEXT NOT NULL UNIQUE,
    created_at   TIMESTAMPTZ NOT NULL DEFAULT now(),
    last_used_at TIMESTAMPTZ
);

CREATE TABLE access_tokens (
    id           BIGSERIAL PRIMARY KEY,
    user_id      BIGINT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    name         TEXT NOT NULL,
    token_hash   BYTEA NOT NULL UNIQUE,
    created_at   TIMESTAMPTZ NOT NULL DEFAULT now(),
    expires_at   TIMESTAMPTZ,
    last_used_at TIMESTAMPTZ
);

CREATE TABLE repos (
    id                 BIGSERIAL PRIMARY KEY,
    namespace          TEXT NOT NULL,
    name               TEXT NOT NULL,
    description        TEXT NOT NULL DEFAULT '',
    default_branch     TEXT NOT NULL DEFAULT 'main',
    -- Comma separated branch patterns (`*` wildcard); protected variables only
    -- reach pipelines on these refs.
    protected_branches TEXT NOT NULL DEFAULT 'main',
    -- Run a pipeline on every push (the workflow rules still decide).
    pipelines_on_push  BOOLEAN NOT NULL DEFAULT TRUE,
    created_by         BIGINT REFERENCES users(id) ON DELETE SET NULL,
    created_at         TIMESTAMPTZ NOT NULL DEFAULT now(),
    pushed_at          TIMESTAMPTZ,
    pipeline_seq       INTEGER NOT NULL DEFAULT 0,
    UNIQUE (namespace, name)
);

CREATE TABLE variables (
    id                BIGSERIAL PRIMARY KEY,
    repo_id           BIGINT NOT NULL REFERENCES repos(id) ON DELETE CASCADE,
    key               TEXT NOT NULL,
    value             TEXT NOT NULL,
    file              BOOLEAN NOT NULL DEFAULT FALSE,
    protected         BOOLEAN NOT NULL DEFAULT FALSE,
    masked            BOOLEAN NOT NULL DEFAULT FALSE,
    environment_scope TEXT NOT NULL DEFAULT '*',
    created_at        TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (repo_id, key, environment_scope)
);

CREATE TABLE runners (
    id           BIGSERIAL PRIMARY KEY,
    name         TEXT NOT NULL,
    token_hash   BYTEA NOT NULL UNIQUE,
    tags         TEXT[] NOT NULL DEFAULT '{}',
    run_untagged BOOLEAN NOT NULL DEFAULT TRUE,
    paused       BOOLEAN NOT NULL DEFAULT FALSE,
    created_by   BIGINT REFERENCES users(id) ON DELETE SET NULL,
    created_at   TIMESTAMPTZ NOT NULL DEFAULT now(),
    contacted_at TIMESTAMPTZ,
    version      TEXT NOT NULL DEFAULT '',
    platform     TEXT NOT NULL DEFAULT '',
    executor     TEXT NOT NULL DEFAULT '',
    hostname     TEXT NOT NULL DEFAULT ''
);

CREATE TABLE pipelines (
    id          BIGSERIAL PRIMARY KEY,
    repo_id     BIGINT NOT NULL REFERENCES repos(id) ON DELETE CASCADE,
    iid         INTEGER NOT NULL,
    ref_name    TEXT NOT NULL,
    ref_is_tag  BOOLEAN NOT NULL DEFAULT FALSE,
    sha         TEXT NOT NULL,
    before_sha  TEXT NOT NULL DEFAULT '0000000000000000000000000000000000000000',
    source      TEXT NOT NULL,
    status      TEXT NOT NULL,
    -- Variables given when the pipeline was started by hand or via the API.
    variables   JSONB NOT NULL DEFAULT '[]',
    -- The plan's stages and global variables (hw_pipe_ci::Plan without jobs).
    stages      TEXT[] NOT NULL DEFAULT '{}',
    yaml_vars   JSONB NOT NULL DEFAULT '[]',
    error       TEXT,
    commit_title TEXT NOT NULL DEFAULT '',
    created_by  BIGINT REFERENCES users(id) ON DELETE SET NULL,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    started_at  TIMESTAMPTZ,
    finished_at TIMESTAMPTZ,
    UNIQUE (repo_id, iid)
);
CREATE INDEX pipelines_repo ON pipelines (repo_id, id DESC);

CREATE TABLE jobs (
    id               BIGSERIAL PRIMARY KEY,
    pipeline_id      BIGINT NOT NULL REFERENCES pipelines(id) ON DELETE CASCADE,
    repo_id          BIGINT NOT NULL REFERENCES repos(id) ON DELETE CASCADE,
    name             TEXT NOT NULL,
    stage            TEXT NOT NULL,
    stage_idx        INTEGER NOT NULL,
    status           TEXT NOT NULL,
    "when"           TEXT NOT NULL,
    allow_failure    BOOLEAN NOT NULL DEFAULT FALSE,
    needs            TEXT[],
    tags             TEXT[] NOT NULL DEFAULT '{}',
    resource_group   TEXT,
    environment      TEXT,
    -- The planned job (hw_pipe_ci::Job) as JSON.
    spec             JSONB NOT NULL,
    runner_id        BIGINT REFERENCES runners(id) ON DELETE SET NULL,
    token_hash       BYTEA UNIQUE,
    cancel_requested BOOLEAN NOT NULL DEFAULT FALSE,
    exit_code        INTEGER,
    failure_reason   TEXT,
    log_size         BIGINT NOT NULL DEFAULT 0,
    retry_of         BIGINT REFERENCES jobs(id) ON DELETE SET NULL,
    -- Superseded by a retry: hidden from the pipeline graph and status.
    retried          BOOLEAN NOT NULL DEFAULT FALSE,
    started_by       BIGINT REFERENCES users(id) ON DELETE SET NULL,
    created_at       TIMESTAMPTZ NOT NULL DEFAULT now(),
    queued_at        TIMESTAMPTZ,
    started_at       TIMESTAMPTZ,
    finished_at      TIMESTAMPTZ,
    updated_at       TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX jobs_pipeline ON jobs (pipeline_id);
CREATE INDEX jobs_pending ON jobs (id) WHERE status = 'pending';
CREATE INDEX jobs_running ON jobs (repo_id, resource_group) WHERE status = 'running';

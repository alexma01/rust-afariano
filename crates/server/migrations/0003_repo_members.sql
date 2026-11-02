-- Per-repository access. Administrators can open every repository without a row here.
--   read:     browse, clone and fetch, see pipelines and job logs
--   write:    also push, run pipelines, run, retry and cancel jobs
--   maintain: also change settings and CI/CD variables, manage members
CREATE TABLE repo_members (
    repo_id    BIGINT NOT NULL REFERENCES repos(id) ON DELETE CASCADE,
    user_id    BIGINT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    role       TEXT NOT NULL CHECK (role IN ('read', 'write', 'maintain')),
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (repo_id, user_id)
);
CREATE INDEX repo_members_user ON repo_members (user_id);

-- Until now every account could do everything in every repository: keep that.
INSERT INTO repo_members (repo_id, user_id, role)
SELECT r.id, u.id, 'maintain' FROM repos r CROSS JOIN users u;

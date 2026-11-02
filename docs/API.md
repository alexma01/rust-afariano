# JSON API

Every call takes a personal access token: `Authorization: Bearer rfp_…`. Create
one under *SSH keys and tokens* in the web UI, or, as an administrator, for a
service account with `POST /api/v1/users/{user}/tokens`.

The token acts as its account, with the same rules as the web UI:

- **Repository** calls need a role in that repository: *read*, *write* or
  *maintain* (administrators have *maintain* everywhere). Without any role the
  answer is `404`, like a missing repository; with too low a role it is `403`.
- **User and runner** calls, and creating repositories, are for administrators.

Bodies are JSON (`Content-Type: application/json`). Errors are
`{"message": "…"}` with a `4xx`/`5xx` status. `{user}` in a path is an account id
or its email.

```sh
API=https://git.example.com/api/v1
auth=(-H "Authorization: Bearer $TOKEN" -H "Content-Type: application/json")
```

## Repositories

| Call | Role | Body / answer |
|---|---|---|
| `POST /repos` | admin | `{"namespace", "name", "description"?, "default_branch"?, "protected_branches"?: [..], "pipelines_on_push"?}` → `201` repository |
| `GET /repos/{ns}/{name}` | read | repository |
| `PATCH /repos/{ns}/{name}` | maintain | any of `description`, `default_branch`, `protected_branches` (list, replaces), `pipelines_on_push` → repository |

A repository is `{"path", "description", "default_branch", "protected_branches",
"pipelines_on_push", "role", "web_url", "http_url", "ssh_url"}`, where `role` is
the caller's.

```sh
curl "${auth[@]}" -X POST $API/repos -d '{"namespace": "acme", "name": "site",
  "protected_branches": ["main", "staging"], "pipelines_on_push": false}'
```

## Members

| Call | Role | Body / answer |
|---|---|---|
| `GET /repos/{ns}/{name}/members` | maintain | `[{"user_id", "email", "name", "role", "admin"}]` |
| `PUT /repos/{ns}/{name}/members/{user}` | maintain | `{"role": "read" \| "write" \| "maintain"}` → member (adds, or changes the role) |
| `DELETE /repos/{ns}/{name}/members/{user}` | maintain | `204`, `404` if not a member |

## CI/CD variables

| Call | Role | Body / answer |
|---|---|---|
| `GET /repos/{ns}/{name}/variables` | maintain | `[{"key", "environment_scope", "file", "protected", "masked"}]`, never values |
| `PUT /repos/{ns}/{name}/variables/{key}` | maintain | `{"value", "file"?, "protected"?, "masked"?, "environment_scope"?}` → `204` (creates, or replaces the same key and scope) |
| `DELETE /repos/{ns}/{name}/variables/{key}?environment_scope=…` | maintain | `204`; scope defaults to `*` |

Values cannot be read back through the API; maintainers can see them in the web UI.

## Pipelines and jobs

| Call | Role | Body / answer |
|---|---|---|
| `POST /repos/{ns}/{name}/pipelines` | write | `{"ref", "variables"?: [{"key", "value"}]}` → `201 {"iid", "web_url"}` |
| `GET /repos/{ns}/{name}/pipelines/{iid}` | read | pipeline |
| `POST /repos/{ns}/{name}/pipelines/{iid}/retry` | write | retries failed and canceled jobs → pipeline |
| `POST /repos/{ns}/{name}/pipelines/{iid}/cancel` | write | → pipeline |
| `GET /repos/{ns}/{name}/jobs/{id}/log` | read | plain text, colours removed; header `X-Job-Status` |
| `POST /repos/{ns}/{name}/jobs/{id}/play` | write | starts a manual job → job |
| `POST /repos/{ns}/{name}/jobs/{id}/retry` | write | → the new job |
| `POST /repos/{ns}/{name}/jobs/{id}/cancel` | write | → job |

A pipeline is `{"iid", "status", "ref", "sha", "source", "error", "web_url",
"jobs": [{"id", "name", "stage", "status", "when", "environment",
"failure_reason", "web_url"}]}`; a job answer is `{"id", "name", "status", "web_url"}`.

```sh
iid=$(curl -s "${auth[@]}" -X POST $API/repos/acme/site/pipelines -d '{"ref": "staging"}' | jq .iid)
curl -s "${auth[@]}" $API/repos/acme/site/pipelines/$iid | jq '.status, .jobs'
curl -s "${auth[@]}" $API/repos/acme/site/jobs/42/log
```

## Users (administrators)

| Call | Body / answer |
|---|---|
| `GET /users` | `[{"id", "email", "name", "admin", "disabled"}]` |
| `POST /users` | `{"email", "name"?, "password"?, "admin"?}` → `201` user. Without a password the account signs in through SSO only, or not at all (a service account) |
| `POST /users/{user}/tokens` | `{"name", "expires_days"?}` → `201 {"user_id", "email", "name", "token"}`, the token shown once |

Tokens for another administrator are refused: they create their own. Each token
created for someone else is logged (`access token created for another account`).

A service account for a sync job, with access to one repository only:

```sh
curl "${auth[@]}" -X POST $API/users -d '{"email": "sync@example.com", "name": "Sync"}'
curl "${auth[@]}" -X PUT $API/repos/acme/site/members/sync@example.com -d '{"role": "write"}'
curl "${auth[@]}" -X POST $API/users/sync@example.com/tokens -d '{"name": "sync job"}'
```

## Runners (administrators)

| Call | Body / answer |
|---|---|
| `GET /runners` | `[runner]` |
| `POST /runners` | `{"name", "tags"?, "run_untagged"?}` → `201 {"id", "token"}`, the token shown once |
| `PATCH /runners/{id}` | any of `tags` (replaces the list), `run_untagged`, `paused` → runner |

A runner is `{"id", "name", "tags", "run_untagged", "paused", "online",
"contacted_at", "hostname", "platform", "executor", "version"}`.

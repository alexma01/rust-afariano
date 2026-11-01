#!/usr/bin/env bash
# Create a repository on rust-afariano (if missing) and push one ref of a local
# checkout into it. Uses the JSON API with a personal access token.
#
#   URL=https://git.example.com RUST_AFARIANO_TOKEN=rfp_… \
#     contrib/import-repo.sh team/website ../website origin/main
#
# The source checkout is fetched first, so `origin/main` is the upstream main as of now.
# A push to an existing branch only fast-forwards (no --force).
set -euo pipefail
URL=${URL:?set URL to your server, e.g. https://git.example.com}
URL=${URL%/}
TOKEN=${RUST_AFARIANO_TOKEN:?set RUST_AFARIANO_TOKEN (SSH keys and tokens → Create token)}
REPO=${1:?usage: import-repo.sh namespace/name source-dir [ref] }
SRC=${2:?usage: import-repo.sh namespace/name source-dir [ref] }
REF=${3:-origin/main}
BRANCH=${BRANCH:-main}
NS=${REPO%%/*}
NAME=${REPO#*/}

body=$(mktemp)
trap 'rm -f "$body"' EXIT
code=$(curl -s -o "$body" -w '%{http_code}' -H "Authorization: Bearer $TOKEN" -H 'Content-Type: application/json' \
  -d "{\"namespace\":\"$NS\",\"name\":\"$NAME\",\"description\":\"${DESCRIPTION:-}\",\"default_branch\":\"$BRANCH\"}" \
  "$URL/api/v1/repos")
case "$code" in
  201) echo "created $URL/$REPO" ;;
  400) grep -q "already exists" "$body" && echo "$REPO already exists" || { cat "$body" >&2; exit 1; } ;;
  *) echo "HTTP $code: $(cat "$body")" >&2; exit 1 ;;
esac

git -C "$SRC" fetch -q origin
echo "pushing $REF ($(git -C "$SRC" rev-parse --short "$REF")) to $REPO:$BRANCH"
AUTH=$(printf 'rust-afariano:%s' "$TOKEN" | base64 | tr -d '\n')
git -C "$SRC" -c credential.helper= -c http.extraHeader="Authorization: Basic $AUTH" \
  push "$URL/$REPO.git" "$REF:refs/heads/$BRANCH"

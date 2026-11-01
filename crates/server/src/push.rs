//! What happens after `git receive-pack` exits: diff the refs, start pipelines.

use std::collections::BTreeMap;
use std::sync::Arc;

use crate::app::AppState;
use crate::git::ZERO_SHA;
use crate::models::{Repo, User};
use crate::pipelines::{self, Created, NewPipeline};

pub async fn after_push(state: Arc<AppState>, repo: Repo, before: BTreeMap<String, String>, user: User) {
    let git = state.git(repo.id);
    let after = match git.ref_map().await {
        Ok(m) => m,
        Err(e) => {
            tracing::error!(repo = %repo.path(), "reading refs after push: {e:#}");
            return;
        }
    };
    let updated: Vec<(String, String)> = after
        .iter()
        .filter(|(name, sha)| before.get(*name) != Some(*sha))
        .map(|(n, s)| (n.clone(), s.clone()))
        .collect();
    if updated.is_empty() && before.len() == after.len() {
        return;
    }
    let _ = sqlx::query("UPDATE repos SET pushed_at = now() WHERE id = $1").bind(repo.id).execute(&state.db).await;

    let mut repo = repo;
    // First push into an empty repo without the default branch: adopt one, like GitLab.
    let default_ref = format!("refs/heads/{}", repo.default_branch);
    if !after.contains_key(&default_ref) {
        let branches: Vec<&str> = after.keys().filter_map(|k| k.strip_prefix("refs/heads/")).collect();
        let pick = ["main", "master"].into_iter().find(|b| branches.contains(b)).or(branches.first().copied());
        if let Some(b) = pick
            && git.set_head(b).await.is_ok()
        {
            let _ = sqlx::query("UPDATE repos SET default_branch = $2 WHERE id = $1")
                .bind(repo.id)
                .bind(b)
                .execute(&state.db)
                .await;
            repo.default_branch = b.to_string();
        }
    }

    tracing::info!(repo = %repo.path(), user = %user.email, refs = updated.len(), "push");
    if repo.pipelines_on_push {
        for (name, sha) in &updated {
            let (ref_name, is_tag) = match (name.strip_prefix("refs/heads/"), name.strip_prefix("refs/tags/")) {
                (Some(b), _) => (b, false),
                (_, Some(t)) => (t, true),
                _ => continue,
            };
            let np = NewPipeline {
                repo: &repo,
                ref_name: ref_name.to_string(),
                is_tag,
                sha: sha.clone(),
                before_sha: before.get(name).cloned().unwrap_or_else(|| ZERO_SHA.to_string()),
                source: "push",
                user: Some(&user),
                variables: Vec::new(),
            };
            match pipelines::create(&state, np).await {
                Ok(Created::Pipeline(iid)) => tracing::debug!(iid, "push pipeline"),
                Ok(other) => tracing::debug!(?other, "no push pipeline"),
                Err(e) => tracing::error!(repo = %repo.path(), "creating pipeline: {e:#}"),
            }
        }
    }
    // Shares the pack slots with clones so maintenance and clones never stack up.
    if let Ok(_permit) = state.pack_slots.clone().acquire_owned().await {
        git.maintain().await;
    }
}

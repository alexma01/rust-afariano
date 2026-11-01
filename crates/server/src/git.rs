//! Bare repositories on disk, read through the `git` CLI (no libgit2, no gix:
//! one short-lived process per request keeps the server's own heap small).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;

use anyhow::{Context, bail};
use tokio::io::AsyncReadExt;
use tokio::process::Command;

pub const ZERO_SHA: &str = "0000000000000000000000000000000000000000";

/// Config passed to every git process that may build or read packs: bounds the
/// memory one clone can take (defaults are sized for big machines).
///
/// Measured on a 216 MB repository: `core.packedGitLimit` (pack files mapped in
/// memory) dominates. 128m → full clone 130–147 MB, repack 149 MB;
/// 32m → clone 37–55 MB, repack 56 MB.
pub const LOW_MEMORY_CONFIG: &[&str] = &[
    "pack.threads=1",
    "pack.windowMemory=32m",
    "pack.deltaCacheSize=8m",
    "core.packedGitLimit=32m",
    "core.packedGitWindowSize=8m",
    "core.deltaBaseCacheLimit=8m",
    "core.bigFileThreshold=8m",
    "uploadpack.allowFilter=true",
    "uploadpack.allowReachableSHA1InWant=true",
];

pub fn repo_dir(data_dir: &Path, repo_id: i64) -> PathBuf {
    data_dir.join("repos").join(format!("{repo_id}.git"))
}

/// A `git` command with the user's/system config ignored and low-memory settings.
pub fn git_cmd(data_dir: &Path) -> Command {
    let mut cmd = Command::new("git");
    cmd.env("GIT_CONFIG_NOSYSTEM", "1")
        .env("HOME", data_dir)
        .env("XDG_CONFIG_HOME", data_dir.join(".config"))
        .env("GIT_TERMINAL_PROMPT", "0")
        .env_remove("GIT_DIR")
        .kill_on_drop(true);
    for c in LOW_MEMORY_CONFIG {
        cmd.arg("-c").arg(c);
    }
    cmd
}

pub struct Git {
    pub data_dir: PathBuf,
    pub dir: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitRef {
    /// Full name, `refs/heads/main`.
    pub name: String,
    pub sha: String,
}

impl GitRef {
    pub fn short(&self) -> &str {
        self.name.strip_prefix("refs/heads/").or_else(|| self.name.strip_prefix("refs/tags/")).unwrap_or(&self.name)
    }
    pub fn is_tag(&self) -> bool {
        self.name.starts_with("refs/tags/")
    }
}

#[derive(Debug, Clone)]
pub struct Commit {
    pub sha: String,
    pub parents: Vec<String>,
    pub author_name: String,
    pub author_email: String,
    pub time: i64,
    pub subject: String,
    pub body: String,
}

#[derive(Debug, Clone)]
pub struct TreeEntry {
    pub name: String,
    pub kind: String,
    pub size: Option<u64>,
}

impl Git {
    pub fn new(data_dir: &Path, repo_id: i64) -> Self {
        Self { data_dir: data_dir.to_path_buf(), dir: repo_dir(data_dir, repo_id) }
    }

    fn cmd(&self) -> Command {
        let mut c = git_cmd(&self.data_dir);
        c.arg("--git-dir").arg(&self.dir);
        c
    }

    /// Run git, return stdout (capped at `limit` bytes; longer output is cut).
    async fn run(&self, args: &[&str], limit: usize) -> anyhow::Result<(bool, Vec<u8>)> {
        let mut child = self
            .cmd()
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .context("spawning git")?;
        let mut out = Vec::new();
        let mut stdout = child.stdout.take().expect("piped");
        (&mut stdout).take(limit as u64).read_to_end(&mut out).await?;
        drop(stdout);
        let status = child.wait().await?;
        // A cut output kills git with SIGPIPE: report it as success with what we have.
        Ok((status.success() || out.len() >= limit, out))
    }

    async fn run_ok(&self, args: &[&str], limit: usize) -> anyhow::Result<Vec<u8>> {
        let (ok, out) = self.run(args, limit).await?;
        if !ok {
            bail!("git {} failed", args.join(" "));
        }
        Ok(out)
    }

    pub async fn init(&self, default_branch: &str) -> anyhow::Result<()> {
        let status = git_cmd(&self.data_dir)
            .args(["init", "--bare", "--quiet"])
            .arg(format!("--initial-branch={default_branch}"))
            .arg(&self.dir)
            .status()
            .await?;
        if !status.success() {
            bail!("git init failed");
        }
        for (k, v) in [
            ("receive.advertisePushOptions", "false"),
            ("repack.writeBitmaps", "true"),
            ("pack.writeBitmapHashCache", "true"),
            ("core.logAllRefUpdates", "false"),
            ("gc.autoDetach", "false"),
        ] {
            self.run_ok(&["config", k, v], 1024).await?;
        }
        Ok(())
    }

    pub async fn set_head(&self, branch: &str) -> anyhow::Result<()> {
        self.run_ok(&["symbolic-ref", "HEAD", &format!("refs/heads/{branch}")], 1024).await?;
        Ok(())
    }

    pub async fn refs(&self) -> anyhow::Result<Vec<GitRef>> {
        let out = self
            .run_ok(&["for-each-ref", "--format=%(objectname) %(refname)", "refs/heads", "refs/tags"], 8 << 20)
            .await?;
        Ok(String::from_utf8_lossy(&out)
            .lines()
            .filter_map(|l| {
                let (sha, name) = l.split_once(' ')?;
                Some(GitRef { name: name.to_string(), sha: sha.to_string() })
            })
            .collect())
    }

    pub async fn ref_map(&self) -> anyhow::Result<BTreeMap<String, String>> {
        Ok(self.refs().await?.into_iter().map(|r| (r.name, r.sha)).collect())
    }

    /// Commit SHA of `rev` (branch, tag, or SHA), if it exists.
    pub async fn resolve(&self, rev: &str) -> anyhow::Result<Option<String>> {
        if rev.starts_with('-') {
            return Ok(None);
        }
        let (ok, out) = self
            .run(&["rev-parse", "--verify", "--quiet", "--end-of-options", &format!("{rev}^{{commit}}")], 1024)
            .await?;
        let sha = String::from_utf8_lossy(&out).trim().to_string();
        Ok((ok && sha.len() == 40).then_some(sha))
    }

    /// Splits `feature/x/src/main.rs` into (`feature/x`, `src/main.rs`) by the
    /// longest prefix that is a branch or tag (or a full/short SHA).
    pub async fn split_ref_path(&self, rest: &str) -> anyhow::Result<Option<(String, String)>> {
        let rest = rest.trim_matches('/');
        let refs = self.refs().await?;
        let mut best: Option<&str> = None;
        for r in &refs {
            let short = r.short();
            if (rest == short || rest.starts_with(&format!("{short}/"))) && best.is_none_or(|b| short.len() > b.len()) {
                best = Some(short);
            }
        }
        if let Some(b) = best {
            return Ok(Some((b.to_string(), rest[b.len()..].trim_start_matches('/').to_string())));
        }
        let (first, path) = rest.split_once('/').unwrap_or((rest, ""));
        if first.len() >= 7 && first.chars().all(|c| c.is_ascii_hexdigit()) && self.resolve(first).await?.is_some() {
            return Ok(Some((first.to_string(), path.to_string())));
        }
        Ok(None)
    }

    pub async fn object_kind(&self, rev: &str, path: &str) -> anyhow::Result<Option<String>> {
        let spec = format!("{rev}:{path}");
        let (ok, out) = self.run(&["cat-file", "-t", "--end-of-options", &spec], 64).await?;
        Ok(ok.then(|| String::from_utf8_lossy(&out).trim().to_string()))
    }

    pub async fn ls_tree(&self, rev: &str, path: &str) -> anyhow::Result<Vec<TreeEntry>> {
        let spec = if path.is_empty() { rev.to_string() } else { format!("{rev}:{path}") };
        let out = self.run_ok(&["ls-tree", "-z", "--long", "--end-of-options", &spec], 16 << 20).await?;
        let mut entries: Vec<TreeEntry> = out
            .split(|b| *b == 0)
            .filter(|l| !l.is_empty())
            .filter_map(|l| {
                let l = String::from_utf8_lossy(l);
                let (meta, name) = l.split_once('\t')?;
                let mut it = meta.split_whitespace();
                let _mode = it.next()?;
                let kind = it.next()?.to_string();
                let _sha = it.next()?;
                let size = it.next().and_then(|s| s.parse().ok());
                Some(TreeEntry { name: name.to_string(), kind, size })
            })
            .collect();
        entries.sort_by_key(|e| (e.kind != "tree", e.name.to_lowercase()));
        Ok(entries)
    }

    /// Blob content, `None` if missing; content is cut at `limit`.
    pub async fn blob(&self, rev: &str, path: &str, limit: usize) -> anyhow::Result<Option<(u64, Vec<u8>)>> {
        let spec = format!("{rev}:{path}");
        let (ok, size) = self.run(&["cat-file", "-s", "--end-of-options", &spec], 64).await?;
        if !ok {
            return Ok(None);
        }
        let size: u64 = String::from_utf8_lossy(&size).trim().parse().unwrap_or(0);
        let data = self.run_ok(&["cat-file", "blob", "--end-of-options", &spec], limit).await?;
        Ok(Some((size, data)))
    }

    pub async fn log(&self, rev: &str, path: Option<&str>, skip: usize, limit: usize) -> anyhow::Result<Vec<Commit>> {
        let skip = format!("--skip={skip}");
        let max = format!("--max-count={limit}");
        let mut args = vec![
            "log",
            "--format=%H%x1f%P%x1f%an%x1f%ae%x1f%at%x1f%s%x1f%b%x1e",
            &skip,
            &max,
            "--end-of-options",
            rev,
            "--",
        ];
        if let Some(p) = path.filter(|p| !p.is_empty()) {
            args.push(p);
        }
        let out = self.run_ok(&args, 16 << 20).await?;
        Ok(parse_commits(&out))
    }

    pub async fn commit(&self, sha: &str) -> anyhow::Result<Option<Commit>> {
        let (ok, out) = self
            .run(
                &["log", "-1", "--format=%H%x1f%P%x1f%an%x1f%ae%x1f%at%x1f%s%x1f%b%x1e", "--end-of-options", sha, "--"],
                1 << 20,
            )
            .await?;
        Ok(if ok { parse_commits(&out).into_iter().next() } else { None })
    }

    /// Unified diff of a commit against its first parent (or the empty tree).
    pub async fn diff(&self, sha: &str, limit: usize) -> anyhow::Result<Vec<u8>> {
        self.run_ok(
            &[
                "show",
                "--no-color",
                "--format=",
                "--patch",
                "-M",
                "--diff-merges=first-parent",
                "--end-of-options",
                sha,
                "--",
            ],
            limit,
        )
        .await
    }

    pub async fn commit_count(&self, rev: &str) -> anyhow::Result<u64> {
        let out = self.run_ok(&["rev-list", "--count", "--end-of-options", rev, "--"], 64).await?;
        Ok(String::from_utf8_lossy(&out).trim().parse().unwrap_or(0))
    }

    /// After a push: a full repack with a reachability bitmap the first time
    /// (clones then reuse the pack instead of searching deltas), otherwise
    /// `git gc --auto`, which repacks only when packs/loose objects pile up.
    pub async fn maintain(&self) {
        let has_bitmap = std::fs::read_dir(self.dir.join("objects/pack"))
            .map(|d| d.flatten().any(|e| e.path().extension().is_some_and(|x| x == "bitmap")))
            .unwrap_or(false);
        let args: &[&str] = if has_bitmap { &["gc", "--auto", "--quiet"] } else { &["repack", "-a", "-d", "-b", "-q"] };
        let _ = self.cmd().args(args).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).status().await;
    }
}

fn parse_commits(out: &[u8]) -> Vec<Commit> {
    String::from_utf8_lossy(out)
        .split('\x1e')
        .filter_map(|rec| {
            let rec = rec.trim_start_matches('\n');
            if rec.is_empty() {
                return None;
            }
            let f: Vec<&str> = rec.splitn(7, '\x1f').collect();
            if f.len() < 7 {
                return None;
            }
            Some(Commit {
                sha: f[0].to_string(),
                parents: f[1].split_whitespace().map(String::from).collect(),
                author_name: f[2].to_string(),
                author_email: f[3].to_string(),
                time: f[4].parse().unwrap_or(0),
                subject: f[5].to_string(),
                body: f[6].trim().to_string(),
            })
        })
        .collect()
}

/// Is this a binary blob? (NUL in the first 8 KiB, like git.)
pub fn looks_binary(data: &[u8]) -> bool {
    data.iter().take(8000).any(|b| *b == 0)
}

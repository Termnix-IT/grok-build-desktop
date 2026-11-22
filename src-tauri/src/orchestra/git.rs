use std::{collections::BTreeSet, path::Path, process::Stdio, time::Duration};

use serde::Serialize;

use super::runner::no_window;

#[derive(Serialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct Preflight {
    pub is_git: bool,
    pub has_commit: bool,
    pub dirty: bool,
    pub branch: Option<String>,
}

async fn git(dir: &Path, args: &[&str]) -> Option<Vec<u8>> {
    let mut command = std::process::Command::new("git");
    command
        .arg("-C")
        .arg(dir)
        .args(["-c", "core.quotepath=false", "-c", "color.ui=false"])
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    no_window(&mut command);
    let mut command = tokio::process::Command::from(command);
    command.kill_on_drop(true);
    let output = tokio::time::timeout(Duration::from_secs(10), command.output())
        .await
        .ok()?
        .ok()?;
    output.status.success().then_some(output.stdout)
}

async fn text(dir: &Path, args: &[&str]) -> Option<String> {
    git(dir, args)
        .await
        .map(|b| String::from_utf8_lossy(&b).trim().to_owned())
}

async fn untracked(dir: &Path) -> BTreeSet<String> {
    git(dir, &["ls-files", "--others", "--exclude-standard", "-z"])
        .await
        .unwrap_or_default()
        .split(|b| *b == 0)
        .filter(|bytes| !bytes.is_empty())
        .map(|bytes| String::from_utf8_lossy(bytes).into_owned())
        .collect()
}

pub async fn preflight(dir: &Path) -> Preflight {
    let is_git = text(dir, &["rev-parse", "--is-inside-work-tree"])
        .await
        .as_deref()
        == Some("true");
    if !is_git {
        return Preflight {
            is_git: false,
            has_commit: false,
            dirty: false,
            branch: None,
        };
    }
    let (head, status, branch) = tokio::join!(
        text(dir, &["rev-parse", "--verify", "HEAD"]),
        git(dir, &["status", "--porcelain", "-z"]),
        text(dir, &["symbolic-ref", "--quiet", "--short", "HEAD"]),
    );
    Preflight {
        is_git,
        has_commit: head.is_some(),
        dirty: status.is_some_and(|bytes| !bytes.is_empty()),
        branch: branch.filter(|s| !s.is_empty()),
    }
}

pub struct Snapshot {
    pub base: Option<String>,
    pub untracked: BTreeSet<String>,
}

pub async fn snapshot(dir: &Path) -> Snapshot {
    let base = match text(dir, &["stash", "create"])
        .await
        .filter(|s| !s.is_empty())
    {
        Some(base) => Some(base),
        None => text(dir, &["rev-parse", "--verify", "HEAD"]).await,
    };
    Snapshot {
        base,
        untracked: untracked(dir).await,
    }
}

pub struct StepDiff {
    pub stat: String,
    pub patch: String,
    pub truncated: bool,
    pub untracked: Vec<String>,
}

pub async fn diff_since(dir: &Path, snap: &Snapshot, limit_bytes: usize) -> StepDiff {
    let (stat, mut patch) = if let Some(base) = &snap.base {
        let stat_args = [
            "diff",
            "--no-color",
            "--no-ext-diff",
            "--no-textconv",
            "--stat",
            base.as_str(),
            "--",
        ];
        let patch_args = [
            "diff",
            "--no-color",
            "--no-ext-diff",
            "--no-textconv",
            base.as_str(),
            "--",
        ];
        let (stat, patch) = tokio::join!(text(dir, &stat_args), git(dir, &patch_args));
        (
            stat.unwrap_or_default(),
            String::from_utf8_lossy(&patch.unwrap_or_default()).into_owned(),
        )
    } else {
        (String::new(), String::new())
    };
    let truncated = patch.len() > limit_bytes;
    if truncated {
        let mut end = limit_bytes;
        while !patch.is_char_boundary(end) {
            end -= 1;
        }
        patch.truncate(end);
    }
    StepDiff {
        stat,
        patch,
        truncated,
        untracked: untracked(dir)
            .await
            .difference(&snap.untracked)
            .take(200)
            .cloned()
            .collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{fs, path::PathBuf};

    struct Repository(PathBuf);
    impl Drop for Repository {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    async fn repository() -> Option<Repository> {
        if super::super::runner::resolve_program("git").is_err() {
            return None;
        }
        let dir = std::env::temp_dir().join(format!("gbd-git-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&dir).unwrap();
        let repo = Repository(dir);
        assert!(git(&repo.0, &["init", "--quiet"]).await.is_some());
        assert!(git(&repo.0, &["config", "user.name", "t"]).await.is_some());
        assert!(
            git(&repo.0, &["config", "user.email", "t@example.invalid"])
                .await
                .is_some()
        );
        Some(repo)
    }

    async fn commit(dir: &Path) {
        assert!(git(dir, &["add", "."]).await.is_some());
        assert!(
            git(
                dir,
                &[
                    "-c",
                    "user.name=t",
                    "-c",
                    "user.email=t@example.invalid",
                    "-c",
                    "commit.gpgsign=false",
                    "commit",
                    "--quiet",
                    "-m",
                    "initial"
                ]
            )
            .await
            .is_some()
        );
    }

    #[tokio::test]
    async fn compares_only_changes_since_snapshot() {
        let Some(repo) = repository().await else {
            return;
        };
        assert!(!preflight(&repo.0).await.has_commit);
        fs::write(repo.0.join("before.txt"), "original\n").unwrap();
        fs::write(repo.0.join("during.txt"), "original\n").unwrap();
        commit(&repo.0).await;
        let clean = preflight(&repo.0).await;
        assert!(clean.is_git && clean.has_commit && !clean.dirty);
        assert!(clean.branch.is_some());
        fs::write(repo.0.join("before.txt"), "preexisting dirty change\n").unwrap();
        fs::write(repo.0.join("old-untracked.txt"), "old\n").unwrap();
        assert!(preflight(&repo.0).await.dirty);
        let snap = snapshot(&repo.0).await;
        assert!(snap.base.is_some());
        fs::write(repo.0.join("during.txt"), "this step changed 日本語\n").unwrap();
        fs::write(repo.0.join("new-untracked.txt"), "new\n").unwrap();
        let diff = diff_since(&repo.0, &snap, 16384).await;
        assert!(diff.stat.contains("during.txt"));
        assert!(!diff.stat.contains("before.txt"));
        assert!(diff.patch.contains("this step changed 日本語"));
        assert!(!diff.patch.contains("preexisting dirty change"));
        assert_eq!(diff.untracked, ["new-untracked.txt"]);
        assert!(!diff.truncated);
        assert_eq!(text(&repo.0, &["stash", "list"]).await.unwrap(), "");
        assert_eq!(
            fs::read_to_string(repo.0.join("before.txt")).unwrap(),
            "preexisting dirty change\n"
        );
        let truncated = diff_since(&repo.0, &snap, diff.patch.find("日本語").unwrap() + 1).await;
        assert!(truncated.truncated);
        assert!(!truncated.patch.ends_with('日'));
    }

    #[tokio::test]
    async fn absent_repository_is_a_result() {
        let dir = std::env::temp_dir().join(format!("gbd-no-git-{}", uuid::Uuid::new_v4()));
        let check = preflight(&dir).await;
        assert!(!check.is_git && !check.has_commit && !check.dirty && check.branch.is_none());
        let snap = snapshot(&dir).await;
        assert!(snap.base.is_none() && snap.untracked.is_empty());
        let diff = diff_since(&dir, &snap, 1024).await;
        assert!(diff.stat.is_empty() && diff.patch.is_empty() && diff.untracked.is_empty());
    }
}

use std::{collections::BTreeSet, ffi::OsStr, path::Path, process::Stdio, time::Duration};

use serde::{Deserialize, Serialize};

use super::runner::no_window;

#[derive(Serialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct Preflight {
    pub is_git: bool,
    pub has_commit: bool,
    pub dirty: bool,
    pub branch: Option<String>,
}

async fn run_git(dir: &Path, args: &[&str]) -> Result<Vec<u8>, String> {
    run_git_with(dir, args, &[], 10).await
}

async fn run_git_with(
    dir: &Path,
    args: &[&str],
    env: &[(&str, &OsStr)],
    secs: u64,
) -> Result<Vec<u8>, String> {
    let mut command = std::process::Command::new("git");
    command
        .arg("-C")
        .arg(dir)
        .args(["-c", "core.quotepath=false", "-c", "color.ui=false"])
        .args(args)
        .envs(env.iter().copied())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    no_window(&mut command);
    let mut command = tokio::process::Command::from(command);
    command.kill_on_drop(true);
    let output = tokio::time::timeout(Duration::from_secs(secs), command.output())
        .await
        .map_err(|_| format!("git が {secs} 秒以内に終わりませんでした"))?
        .map_err(|err| format!("git を起動できませんでした: {err}"))?;
    if output.status.success() {
        return Ok(output.stdout);
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    let stderr = stderr.trim();
    let count = stderr.chars().count();
    Err(if stderr.is_empty() {
        format!(
            "git が失敗しました（終了コード {:?}）",
            output.status.code()
        )
    } else {
        stderr.chars().skip(count.saturating_sub(500)).collect()
    })
}

async fn git(dir: &Path, args: &[&str]) -> Option<Vec<u8>> {
    run_git(dir, args).await.ok()
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

/// The working tree before a step, as a git tree object. It is also the recovery point:
/// `git restore --source=<tree> --worktree -- .` brings the files back.
pub struct Snapshot {
    pub base: Option<String>,
}

/// Snapshots may hash many files, so they get more time than the quick queries.
const TREE_SECS: u64 = 120;

/// Writes every file git does not ignore (tracked, changed and untracked) as a tree object.
/// It works on a copy of the index, so the real index, the stash list and the files are left
/// alone, and later edits to a file that was already untracked still show up in the diff.
async fn worktree_tree(dir: &Path) -> Result<String, String> {
    let index = run_git(dir, &["rev-parse", "--git-path", "index"]).await?;
    let index = dir.join(String::from_utf8_lossy(&index).trim());
    let temp = std::env::temp_dir().join(format!("gbd-index-{}", uuid::Uuid::new_v4()));
    let result = async {
        let env = [("GIT_INDEX_FILE", temp.as_os_str())];
        if std::fs::copy(&index, &temp).is_err() {
            run_git_with(dir, &["read-tree", "HEAD"], &env, TREE_SECS).await?;
        }
        run_git_with(dir, &["add", "-A"], &env, TREE_SECS).await?;
        let tree = run_git_with(dir, &["write-tree"], &env, TREE_SECS).await?;
        Ok(String::from_utf8_lossy(&tree).trim().to_owned())
    }
    .await;
    let _ = std::fs::remove_file(&temp);
    let _ = std::fs::remove_file(temp.with_extension("lock"));
    result
}

/// Records the tree before a step. A merge with unresolved files is refused: the agent
/// would work in the middle of someone else's conflict resolution.
pub async fn snapshot(dir: &Path) -> Result<Snapshot, String> {
    let unmerged = run_git(dir, &["ls-files", "--unmerged", "-z"]).await?;
    if !unmerged.is_empty() {
        return Err("マージが終わっていないファイルがあります".into());
    }
    Ok(Snapshot {
        base: Some(worktree_tree(dir).await?),
    })
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ChangedFile {
    pub path: String,
    pub change: FileChange,
    pub added: Option<u32>,
    pub deleted: Option<u32>,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum FileChange {
    Modified,
    Added,
    Deleted,
    Renamed,
    Untracked,
}

const DIFF_ARGS: [&str; 6] = [
    "diff",
    "--no-color",
    "--no-ext-diff",
    "--no-textconv",
    "-M",
    "-z",
];

/// What a step changed: the files (at most the limit, `truncated` says more were cut) and the
/// `git diff --stat` text.
#[derive(Default, Debug, PartialEq, Eq)]
pub struct Changes {
    pub files: Vec<ChangedFile>,
    pub truncated: bool,
    pub stat: String,
}

/// Compares the snapshot with the working tree now. A new file that is still outside the
/// index is reported as untracked.
pub async fn changes(dir: &Path, snap: &Snapshot, limit: usize) -> Result<Changes, String> {
    let Some(base) = &snap.base else {
        return Ok(Changes::default());
    };
    let after = worktree_tree(dir).await?;
    let range = [base.as_str(), after.as_str()];
    let status_args: Vec<&str> = DIFF_ARGS
        .iter()
        .copied()
        .chain(["--name-status"])
        .chain(range)
        .collect();
    let numstat_args: Vec<&str> = DIFF_ARGS
        .iter()
        .copied()
        .chain(["--numstat"])
        .chain(range)
        .collect();
    let stat_args: Vec<&str> = [
        "diff",
        "--no-color",
        "--no-ext-diff",
        "--no-textconv",
        "-M",
        "--stat",
    ]
    .into_iter()
    .chain(range)
    .collect();
    let (status, numstat, stat, outside) = tokio::join!(
        run_git(dir, &status_args),
        git(dir, &numstat_args),
        text(dir, &stat_args),
        untracked(dir)
    );
    let counts = parse_numstat(&numstat.unwrap_or_default());
    let mut files: Vec<ChangedFile> = parse_name_status(&status?)
        .into_iter()
        .map(|(change, path)| {
            let (added, deleted) = counts.get(&path).copied().unwrap_or((None, None));
            let change = if change == FileChange::Added && outside.contains(&path) {
                FileChange::Untracked
            } else {
                change
            };
            ChangedFile {
                path,
                change,
                added,
                deleted,
            }
        })
        .collect();
    // Deletions go first so the cut never hides one from the deletion check.
    files.sort_by_key(|file| file.change != FileChange::Deleted);
    let truncated = files.len() > limit;
    files.truncate(limit);
    Ok(Changes {
        files,
        truncated,
        stat: stat.unwrap_or_default(),
    })
}

fn nul_fields(bytes: &[u8]) -> impl Iterator<Item = String> + '_ {
    bytes
        .split(|b| *b == 0)
        .map(|field| String::from_utf8_lossy(field).into_owned())
}

/// `--name-status -z`: `STATUS NUL path NUL`, or `R100 NUL old NUL new NUL` for renames and copies.
fn parse_name_status(bytes: &[u8]) -> Vec<(FileChange, String)> {
    let mut fields = nul_fields(bytes);
    let mut entries = Vec::new();
    while let Some(status) = fields.next() {
        let Some(kind) = status.chars().next() else {
            continue;
        };
        let first = fields.next();
        let path = if matches!(kind, 'R' | 'C') {
            fields.next()
        } else {
            first
        };
        let Some(path) = path.filter(|path| !path.is_empty()) else {
            break;
        };
        let change = match kind {
            'A' | 'C' => FileChange::Added,
            'D' => FileChange::Deleted,
            'R' => FileChange::Renamed,
            _ => FileChange::Modified,
        };
        entries.push((change, path));
    }
    entries
}

type LineCounts = std::collections::HashMap<String, (Option<u32>, Option<u32>)>;

/// `--numstat -z`: `added TAB deleted TAB path NUL`, or `added TAB deleted TAB NUL old NUL new NUL`
/// for renames. Binary files report `-` and get no counts.
fn parse_numstat(bytes: &[u8]) -> LineCounts {
    let mut fields = nul_fields(bytes);
    let mut counts = LineCounts::new();
    while let Some(entry) = fields.next() {
        let mut parts = entry.splitn(3, '\t');
        let (Some(added), Some(deleted), Some(path)) = (parts.next(), parts.next(), parts.next())
        else {
            continue;
        };
        let path = if path.is_empty() {
            let _old = fields.next();
            fields.next().unwrap_or_default()
        } else {
            path.to_owned()
        };
        counts.insert(path, (added.parse().ok(), deleted.parse().ok()));
    }
    counts
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
        let snap = snapshot(&repo.0).await.unwrap();
        let base = snap.base.clone().unwrap();
        assert_eq!(
            text(&repo.0, &["cat-file", "-t", &base]).await.unwrap(),
            "tree"
        );
        fs::write(repo.0.join("during.txt"), "this step changed 日本語\n").unwrap();
        fs::write(repo.0.join("new-untracked.txt"), "new\n").unwrap();
        // A file that was untracked before the step still counts when the step edits it.
        fs::write(
            repo.0.join("old-untracked.txt"),
            "old\nedited by the step\n",
        )
        .unwrap();
        let changes = changes(&repo.0, &snap, 200).await.unwrap();
        assert!(changes.stat.contains("during.txt"));
        assert!(!changes.stat.contains("before.txt"));
        let kinds: Vec<_> = changes
            .files
            .iter()
            .map(|file| (file.path.as_str(), file.change))
            .collect();
        assert_eq!(
            kinds,
            [
                ("during.txt", FileChange::Modified),
                ("new-untracked.txt", FileChange::Untracked),
                ("old-untracked.txt", FileChange::Modified),
            ]
        );
        // Neither the real index nor the stash list nor the files were touched.
        assert_eq!(text(&repo.0, &["stash", "list"]).await.unwrap(), "");
        assert_eq!(
            text(&repo.0, &["diff", "--cached", "--name-only"])
                .await
                .unwrap(),
            ""
        );
        assert_eq!(
            fs::read_to_string(repo.0.join("before.txt")).unwrap(),
            "preexisting dirty change\n"
        );
        // The snapshot restores the files as they were before the step.
        assert!(
            git(
                &repo.0,
                &["restore", "--source", &base, "--worktree", "--", "."]
            )
            .await
            .is_some()
        );
        // A global core.autocrlf may write CRLF on restore; the content is what matters.
        let read = |name: &str| {
            fs::read_to_string(repo.0.join(name))
                .unwrap()
                .replace("\r\n", "\n")
        };
        assert_eq!(read("old-untracked.txt"), "old\n");
        assert_eq!(read("during.txt"), "original\n");
    }

    #[tokio::test]
    async fn the_cut_keeps_deletions() {
        let Some(repo) = repository().await else {
            return;
        };
        fs::write(repo.0.join("zz-last.txt"), "kept until the step\n").unwrap();
        commit(&repo.0).await;
        let snap = snapshot(&repo.0).await.unwrap();
        for index in 0..5 {
            fs::write(repo.0.join(format!("a{index}.txt")), "new\n").unwrap();
        }
        fs::remove_file(repo.0.join("zz-last.txt")).unwrap();
        let changes = changes(&repo.0, &snap, 2).await.unwrap();
        assert!(changes.truncated);
        assert_eq!(changes.files.len(), 2);
        assert_eq!(changes.files[0].path, "zz-last.txt");
        assert_eq!(changes.files[0].change, FileChange::Deleted);
    }

    #[tokio::test]
    async fn absent_repository_is_a_result() {
        let dir = std::env::temp_dir().join(format!("gbd-no-git-{}", uuid::Uuid::new_v4()));
        let check = preflight(&dir).await;
        assert!(!check.is_git && !check.has_commit && !check.dirty && check.branch.is_none());
        assert!(snapshot(&dir).await.is_err());
        let snap = Snapshot { base: None };
        assert_eq!(changes(&dir, &snap, 10).await.unwrap(), Changes::default());
    }

    #[tokio::test]
    async fn lists_changed_files_by_kind() {
        let Some(repo) = repository().await else {
            return;
        };
        // Different contents, so rename detection cannot pair the deleted file with the new one.
        for name in ["modify.txt", "delete.txt", "rename-old.txt"] {
            fs::write(repo.0.join(name), format!("one\ntwo\nthree\n{name}\n")).unwrap();
        }
        fs::write(repo.0.join("image.bin"), [0u8, 1, 2, 3]).unwrap();
        fs::write(repo.0.join(".gitignore"), "build/\n").unwrap();
        commit(&repo.0).await;
        let snap = snapshot(&repo.0).await.unwrap();
        fs::create_dir_all(repo.0.join("build")).unwrap();
        fs::write(repo.0.join("build").join("out.txt"), "ignored\n").unwrap();
        fs::write(
            repo.0.join("modify.txt"),
            "one\nTWO\nthree\nmodify.txt\nfour\n",
        )
        .unwrap();
        fs::remove_file(repo.0.join("delete.txt")).unwrap();
        fs::rename(repo.0.join("rename-old.txt"), repo.0.join("rename-new.txt")).unwrap();
        fs::write(repo.0.join("image.bin"), [9u8, 0, 8, 0, 7]).unwrap();
        fs::write(repo.0.join("staged.txt"), "a\nb\n").unwrap();
        fs::write(repo.0.join("untracked 日本語.txt"), "x\n").unwrap();
        assert!(
            git(
                &repo.0,
                &["add", "staged.txt", "rename-new.txt", "rename-old.txt"]
            )
            .await
            .is_some()
        );
        let Changes {
            files,
            truncated,
            stat,
        } = changes(&repo.0, &snap, 200).await.unwrap();
        assert!(!truncated);
        assert!(stat.contains("modify.txt"));
        assert!(!files.iter().any(|file| file.path.starts_with("build")));
        let find = |path: &str| files.iter().find(|file| file.path == path).cloned();
        assert_eq!(
            find("modify.txt"),
            Some(ChangedFile {
                path: "modify.txt".into(),
                change: FileChange::Modified,
                added: Some(2),
                deleted: Some(1),
            })
        );
        assert_eq!(find("delete.txt").unwrap().change, FileChange::Deleted);
        assert_eq!(find("rename-new.txt").unwrap().change, FileChange::Renamed);
        assert!(find("rename-old.txt").is_none());
        let binary = find("image.bin").unwrap();
        assert_eq!(
            (binary.change, binary.added, binary.deleted),
            (FileChange::Modified, None, None)
        );
        assert_eq!(find("staged.txt").unwrap().change, FileChange::Added);
        assert_eq!(
            find("untracked 日本語.txt").unwrap().change,
            FileChange::Untracked
        );
        let cut = changes(&repo.0, &snap, 2).await.unwrap();
        assert!(cut.truncated && cut.files.len() == 2);
    }

    #[tokio::test]
    async fn snapshot_fails_with_unmerged_paths() {
        let Some(repo) = repository().await else {
            return;
        };
        fs::write(repo.0.join("a.txt"), "base\n").unwrap();
        commit(&repo.0).await;
        let base = text(&repo.0, &["symbolic-ref", "--short", "HEAD"])
            .await
            .unwrap();
        assert!(
            git(&repo.0, &["checkout", "--quiet", "-b", "other"])
                .await
                .is_some()
        );
        fs::write(repo.0.join("a.txt"), "other\n").unwrap();
        commit(&repo.0).await;
        assert!(
            git(&repo.0, &["checkout", "--quiet", &base])
                .await
                .is_some()
        );
        fs::write(repo.0.join("a.txt"), "main\n").unwrap();
        commit(&repo.0).await;
        assert!(git(&repo.0, &["merge", "other"]).await.is_none());
        let error = snapshot(&repo.0).await.err().unwrap();
        assert!(!error.is_empty());
    }

    #[test]
    fn parses_nul_separated_output() {
        let status = b"M\0a.txt\0R087\0old name.txt\0new name.txt\0D\0gone.txt\0A\0new.txt\0";
        assert_eq!(
            parse_name_status(status),
            vec![
                (FileChange::Modified, "a.txt".into()),
                (FileChange::Renamed, "new name.txt".into()),
                (FileChange::Deleted, "gone.txt".into()),
                (FileChange::Added, "new.txt".into()),
            ]
        );
        let numstat = b"3\t1\ta.txt\0-\t-\tbin.dat\0" as &[u8];
        let rename = b"0\t0\t\0old name.txt\0new name.txt\0" as &[u8];
        let counts = parse_numstat(&[numstat, rename].concat());
        assert_eq!(counts["a.txt"], (Some(3), Some(1)));
        assert_eq!(counts["bin.dat"], (None, None));
        assert_eq!(counts["new name.txt"], (Some(0), Some(0)));
        assert!(parse_name_status(b"").is_empty());
    }
}

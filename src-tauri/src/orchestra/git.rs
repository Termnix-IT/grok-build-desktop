use std::{collections::BTreeSet, ffi::OsStr, path::Path, process::Stdio, time::Duration};

use serde::{Deserialize, Serialize};

use super::runner::{child_path, no_window};

#[derive(Serialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct Preflight {
    pub is_git: bool,
    pub has_commit: bool,
    pub branch: Option<String>,
}

async fn run_git_with(
    dir: &Path,
    config: &[(String, String)],
    args: &[&str],
    env: &[(&str, &OsStr)],
    secs: u64,
) -> Result<Vec<u8>, String> {
    // An agent can write `.git/config` and `.git/hooks`. Git started here runs outside the
    // agent's sandbox, so it must not start the repository's fsmonitor or hooks, or fetch
    // (a missing object in a partial clone would fetch with the repository's ssh command).
    // The hooks folder is a random path that is never created.
    let hooks = std::env::temp_dir().join(format!("gbd-no-hooks-{}", uuid::Uuid::new_v4()));
    let mut command = std::process::Command::new("git");
    command
        .arg("-C")
        .arg(dir)
        .args(["-c", "core.quotepath=false", "-c", "color.ui=false"])
        .args([
            "-c",
            "core.fsmonitor=false",
            "-c",
            "protocol.allow=never",
            "-c",
        ])
        .arg(format!("core.hooksPath={}", hooks.display()))
        .args(args)
        // Read-only commands must not refresh and rewrite the real index.
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("GIT_NO_LAZY_FETCH", "1")
        .envs(env.iter().copied())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // `-c key=value` splits at the first `=`, so a driver named `a=b` could not be switched
    // off that way. The environment keeps the key and the value apart.
    if !config.is_empty() {
        command.env("GIT_CONFIG_COUNT", config.len().to_string());
        for (index, (key, value)) in config.iter().enumerate() {
            command
                .env(format!("GIT_CONFIG_KEY_{index}"), key)
                .env(format!("GIT_CONFIG_VALUE_{index}"), value);
        }
    }
    // The filters that stay on (Git LFS) run with the repository as their current folder.
    child_path(&mut command);
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

/// Git in one folder. Filter drivers that an agent could have configured are switched off for
/// every command, since `git add`, `write-tree` and `diff` may all start them. See
/// [`filters_off`] for which ones stay on.
struct Repo<'a> {
    dir: &'a Path,
    filters_off: Vec<(String, String)>,
}

impl<'a> Repo<'a> {
    async fn open(dir: &'a Path) -> Result<Self, String> {
        let list = run_git_with(
            dir,
            &[],
            &["config", "--list", "--show-scope", "--show-origin", "-z"],
            &[],
            10,
        )
        .await?;
        Ok(Self {
            dir,
            filters_off: filters_off(&list, dir),
        })
    }

    async fn run(&self, args: &[&str]) -> Result<Vec<u8>, String> {
        self.run_with(args, &[], 10).await
    }

    async fn run_with(
        &self,
        args: &[&str],
        env: &[(&str, &OsStr)],
        secs: u64,
    ) -> Result<Vec<u8>, String> {
        run_git_with(self.dir, &self.filters_off, args, env, secs).await
    }

    async fn text(&self, args: &[&str]) -> Option<String> {
        self.run(args)
            .await
            .ok()
            .map(|b| String::from_utf8_lossy(&b).trim().to_owned())
    }

    async fn untracked(&self) -> BTreeSet<String> {
        self.run(&["ls-files", "--others", "--exclude-standard", "-z"])
            .await
            .unwrap_or_default()
            .split(|b| *b == 0)
            .filter(|bytes| !bytes.is_empty())
            .map(|bytes| String::from_utf8_lossy(bytes).into_owned())
            .collect()
    }
}

/// The settings that switch filter drivers off. An entry is trusted only when it comes from
/// the system or the user's config, read from a file outside the project folder (a global
/// `include` may point into it). A driver with any untrusted entry is switched off. Git LFS
/// runs the `lfs.extension.*` commands of the repository's config while it cleans, so an
/// untrusted extension switches every driver off, Git LFS included.
///
/// `--list --show-scope --show-origin -z` prints `scope NUL origin NUL key LF value NUL`.
fn filters_off(list: &[u8], dir: &Path) -> Vec<(String, String)> {
    let project = dir.canonicalize().ok();
    let mut outside = std::collections::HashMap::<Vec<u8>, bool>::new();
    let mut trusted = |origin: &[u8]| {
        *outside.entry(origin.to_vec()).or_insert_with(|| {
            let Some(file) = origin.strip_prefix(b"file:") else {
                return false;
            };
            let file = Path::new(std::str::from_utf8(file).unwrap_or_default());
            let Ok(file) = dir.join(file).canonicalize() else {
                return false;
            };
            project
                .as_deref()
                .is_some_and(|project| !file.starts_with(project))
        })
    };
    let mut fields = list.split(|b| *b == 0);
    let mut all = BTreeSet::new();
    let mut untrusted = BTreeSet::new();
    let mut extension = false;
    while let (Some(scope), Some(origin), Some(entry)) =
        (fields.next(), fields.next(), fields.next())
    {
        let key = String::from_utf8_lossy(entry.split(|b| *b == b'\n').next().unwrap_or_default());
        let safe = matches!(scope, b"system" | b"global") && trusted(origin);
        if key.starts_with("lfs.extension.") && !safe {
            extension = true;
        }
        // Git prints the section and the variable in lowercase; the driver name keeps its case.
        if let Some((name, _variable)) = key
            .strip_prefix("filter.")
            .and_then(|rest| rest.rsplit_once('.'))
            && !name.is_empty()
        {
            all.insert(name.to_owned());
            if !safe {
                untrusted.insert(name.to_owned());
            }
        }
    }
    let drivers = if extension { all } else { untrusted };
    drivers
        .into_iter()
        .flat_map(|name| {
            [
                ("clean", ""),
                ("smudge", ""),
                ("process", ""),
                ("required", "false"),
            ]
            .map(|(setting, value)| (format!("filter.{name}.{setting}"), value.to_owned()))
        })
        .collect()
}

pub async fn preflight(dir: &Path) -> Preflight {
    let absent = Preflight {
        is_git: false,
        has_commit: false,
        branch: None,
    };
    let Ok(repo) = Repo::open(dir).await else {
        return absent;
    };
    if repo
        .text(&["rev-parse", "--is-inside-work-tree"])
        .await
        .as_deref()
        != Some("true")
    {
        return absent;
    }
    let (head, branch) = tokio::join!(
        repo.text(&["rev-parse", "--verify", "HEAD"]),
        repo.text(&["symbolic-ref", "--quiet", "--short", "HEAD"]),
    );
    Preflight {
        is_git: true,
        has_commit: head.is_some(),
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
async fn worktree_tree(repo: &Repo<'_>) -> Result<String, String> {
    let index = repo.run(&["rev-parse", "--git-path", "index"]).await?;
    let index = repo.dir.join(String::from_utf8_lossy(&index).trim());
    let temp = std::env::temp_dir().join(format!("gbd-index-{}", uuid::Uuid::new_v4()));
    let result = async {
        let env = [("GIT_INDEX_FILE", temp.as_os_str())];
        if std::fs::copy(&index, &temp).is_err() {
            repo.run_with(&["read-tree", "HEAD"], &env, TREE_SECS)
                .await?;
        }
        repo.run_with(&["add", "-A"], &env, TREE_SECS).await?;
        let tree = repo.run_with(&["write-tree"], &env, TREE_SECS).await?;
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
    let repo = Repo::open(dir).await?;
    let unmerged = repo.run(&["ls-files", "--unmerged", "-z"]).await?;
    if !unmerged.is_empty() {
        return Err("マージが終わっていないファイルがあります".into());
    }
    Ok(Snapshot {
        base: Some(worktree_tree(&repo).await?),
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
    let repo = Repo::open(dir).await?;
    let after = worktree_tree(&repo).await?;
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
        repo.run(&status_args),
        repo.run(&numstat_args),
        repo.text(&stat_args),
        repo.untracked()
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

    async fn git(dir: &Path, args: &[&str]) -> Option<Vec<u8>> {
        run_git_with(dir, &[], args, &[], 10).await.ok()
    }

    async fn text(dir: &Path, args: &[&str]) -> Option<String> {
        git(dir, args)
            .await
            .map(|b| String::from_utf8_lossy(&b).trim().to_owned())
    }

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
        assert!(clean.is_git && clean.has_commit);
        assert!(clean.branch.is_some());
        fs::write(repo.0.join("before.txt"), "preexisting dirty change\n").unwrap();
        fs::write(repo.0.join("old-untracked.txt"), "old\n").unwrap();
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
    async fn repository_programs_do_not_run() {
        let Some(repo) = repository().await else {
            return;
        };
        fs::write(repo.0.join("a.txt"), "a\n").unwrap();
        commit(&repo.0).await;
        let marker = std::env::temp_dir().join(format!("gbd-marker-{}", uuid::Uuid::new_v4()));
        let append = |label: &str| {
            format!(
                "sh -c \"echo {label} >> '{}'\"",
                marker.to_string_lossy().replace('\\', "/")
            )
        };
        let clean = format!("{}; cat", append("clean"));
        for (key, value) in [
            ("core.fsmonitor", append("fsmonitor")),
            ("filter.Evil.clean", clean.clone()),
            ("filter.Evil.process", append("process")),
            ("filter.Evil.required", "true".into()),
            // `-c filter.a=b.clean=` would set `filter.a` instead.
            ("filter.a=b.clean", clean),
        ] {
            assert!(git(&repo.0, &["config", key, &value]).await.is_some());
        }
        let hooks = repo.0.join(".git").join("hooks");
        fs::create_dir_all(&hooks).unwrap();
        fs::write(
            hooks.join("post-index-change"),
            format!("#!/bin/sh\n{}\n", append("hook")),
        )
        .unwrap();
        fs::write(
            repo.0.join(".gitattributes"),
            "*.txt filter=Evil\n*.md filter=a=b\n",
        )
        .unwrap();
        let snap = snapshot(&repo.0).await.unwrap();
        fs::write(repo.0.join("b.txt"), "b\n").unwrap();
        fs::write(repo.0.join("c.md"), "c\n").unwrap();
        let changes = changes(&repo.0, &snap, 200).await.unwrap();
        assert_eq!(changes.files.len(), 2);
        assert!(preflight(&repo.0).await.has_commit);
        let ran = fs::read_to_string(&marker).unwrap_or_default();
        let _ = fs::remove_file(&marker);
        assert_eq!(ran, "");
    }

    #[test]
    fn only_trusted_filters_stay_on() {
        let dir = std::env::temp_dir().join(format!("gbd-filters-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("inside.inc"), "").unwrap();
        let outside =
            std::env::temp_dir().join(format!("gbd-outside-{}.inc", uuid::Uuid::new_v4()));
        fs::write(&outside, "").unwrap();
        let entry = |scope: &str, origin: &str, key: &str| format!("{scope}\0{origin}\0{key}\nv\0");
        let global = format!("file:{}", outside.display());
        let inside = format!("file:{}", dir.join("inside.inc").display());
        let list = [
            entry("global", &global, "filter.lfs.clean"),
            entry("global", &global, "filter.lfs.process"),
            entry("local", "file:.git/config", "filter.My.Driver.clean"),
            entry("worktree", "file:.git/config.worktree", "filter.w.required"),
            // A global include that reads a file inside the project.
            entry("global", &inside, "filter.included.clean"),
            entry("command", "command line:", "filter.cmd.clean"),
            entry("local", "file:.git/config", "core.bare"),
        ]
        .concat();
        let names = |off: &[(String, String)]| -> BTreeSet<String> {
            off.iter()
                .map(|(key, _)| key.rsplit_once('.').unwrap().0.to_owned())
                .collect()
        };
        let off = filters_off(list.as_bytes(), &dir);
        assert_eq!(
            names(&off),
            [
                "filter.My.Driver",
                "filter.cmd",
                "filter.included",
                "filter.w"
            ]
            .map(str::to_owned)
            .into()
        );
        assert!(off.contains(&("filter.w.process".into(), String::new())));
        assert!(off.contains(&("filter.w.required".into(), "false".into())));
        // An extension from the repository's config switches Git LFS off too.
        let extension = format!(
            "{list}{}",
            entry("local", "file:.git/config", "lfs.extension.x.clean")
        );
        assert!(names(&filters_off(extension.as_bytes(), &dir)).contains("filter.lfs"));
        let _ = fs::remove_dir_all(&dir);
        let _ = fs::remove_file(&outside);
    }

    #[tokio::test]
    async fn absent_repository_is_a_result() {
        let dir = std::env::temp_dir().join(format!("gbd-no-git-{}", uuid::Uuid::new_v4()));
        let check = preflight(&dir).await;
        assert!(!check.is_git && !check.has_commit && check.branch.is_none());
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

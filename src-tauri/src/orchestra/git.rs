use std::{
    collections::BTreeSet,
    ffi::OsStr,
    path::{Path, PathBuf},
    process::Stdio,
    sync::OnceLock,
    time::Duration,
};

use serde::{Deserialize, Serialize};

use super::runner::{child_path, no_window, resolve_program};

/// git.exe as an absolute path. Searching by name could fall back to the app's PATH, whose
/// relative entries would be resolved against the app's current folder. Only `git.exe` itself
/// is used: a `git.cmd` earlier in PATH would run through cmd.exe, which parses the arguments
/// again.
fn git_program() -> Result<PathBuf, String> {
    static GIT: OnceLock<PathBuf> = OnceLock::new();
    if let Some(git) = GIT.get() {
        return Ok(git.clone());
    }
    let name = if cfg!(windows) { "git.exe" } else { "git" };
    let git = resolve_program(name)?;
    if !git
        .file_name()
        .is_some_and(|file| file.eq_ignore_ascii_case(name))
    {
        return Err(format!("{name} が見つかりません"));
    }
    Ok(GIT.get_or_init(|| git).clone())
}

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
    run_git_input(dir, config, args, env, None, secs).await
}

async fn run_git_input(
    dir: &Path,
    config: &[(String, String)],
    args: &[&str],
    env: &[(&str, &OsStr)],
    input: Option<&[u8]>,
    secs: u64,
) -> Result<Vec<u8>, String> {
    // An agent can write `.git/config` and `.git/hooks`. Git started here runs outside the
    // agent's sandbox, so it must not start the repository's fsmonitor or hooks, or fetch
    // (a missing object in a partial clone would fetch with the repository's ssh command).
    // The hooks folder is a random path that is never created.
    let hooks = std::env::temp_dir().join(format!("gbd-no-hooks-{}", uuid::Uuid::new_v4()));
    let mut command = std::process::Command::new(git_program()?);
    // Variables from the app's environment that would make git read another config, repository,
    // index or object store than the one listed by `Repo::open`.
    for name in [
        "GIT_CONFIG",
        "GIT_CONFIG_COUNT",
        "GIT_CONFIG_PARAMETERS",
        "GIT_DIR",
        "GIT_WORK_TREE",
        "GIT_COMMON_DIR",
        "GIT_INDEX_FILE",
        "GIT_OBJECT_DIRECTORY",
        "GIT_ALTERNATE_OBJECT_DIRECTORIES",
    ] {
        command.env_remove(name);
    }
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
        // `protocol.<name>.allow` in the repository's config beats `protocol.allow=never`;
        // an empty allow list beats both.
        .env("GIT_ALLOW_PROTOCOL", "")
        .envs(env.iter().copied())
        .stdin(if input.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
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
    let run = async {
        let mut child = command
            .spawn()
            .map_err(|err| format!("git を起動できませんでした: {err}"))?;
        let stdin = child.stdin.take();
        // Written while git runs, so a full output pipe cannot stall the write. Dropping the
        // handle closes git's input. On Windows the last write only reports back on flush.
        let write = async move {
            match (stdin, input) {
                (Some(mut stdin), Some(input)) => {
                    use tokio::io::AsyncWriteExt;
                    stdin.write_all(input).await?;
                    stdin.flush().await
                }
                _ => Ok(()),
            }
        };
        let (written, output) = tokio::join!(write, child.wait_with_output());
        output
            .map(|output| (written, output))
            .map_err(|err| format!("git を起動できませんでした: {err}"))
    };
    let (written, output) = tokio::time::timeout(Duration::from_secs(secs), run)
        .await
        .map_err(|_| format!("git が {secs} 秒以内に終わりませんでした"))??;
    if output.status.success() {
        // Git would also succeed on part of its input, so a failed write fails the call.
        written.map_err(|err| format!("git に入力を渡せませんでした: {err}"))?;
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
    /// The top of the working tree. The project may be a folder inside it.
    top: PathBuf,
    /// The git folder found from `dir`. Named outright when git runs elsewhere, since git
    /// started at `top` could find another repository (with `core.worktree`).
    git_dir: PathBuf,
    filters_off: Vec<(String, String)>,
}

impl<'a> Repo<'a> {
    async fn open(dir: &'a Path) -> Result<Self, String> {
        let (list, places) = tokio::join!(
            run_git_with(
                dir,
                &[],
                &["config", "--list", "--show-scope", "--show-origin", "-z"],
                &[],
                10,
            ),
            run_git_with(
                dir,
                &[],
                &[
                    "rev-parse",
                    "--path-format=absolute",
                    "--show-toplevel",
                    "--git-dir",
                    "--git-common-dir",
                ],
                &[],
                10,
            ),
        );
        // The project may be a subfolder of the working tree, and a linked worktree keeps its
        // git folder elsewhere. Config read from any of these places is not trusted, so a
        // folder that cannot be found makes the whole repository unusable.
        let unknown = || "git の作業ツリーのフォルダを読み取れませんでした".to_owned();
        let places = places?;
        let places: Vec<PathBuf> = std::str::from_utf8(&places)
            .map_err(|_| unknown())?
            .lines()
            .map(PathBuf::from)
            .collect();
        let [top, git_dir, _] = places.as_slice() else {
            return Err(unknown());
        };
        if places.iter().any(|place| place.canonicalize().is_err()) {
            return Err(unknown());
        }
        let (top, git_dir) = (top.clone(), git_dir.clone());
        let mut writable = vec![dir.to_owned()];
        writable.extend(places);
        Ok(Self {
            dir,
            // Git runs from the top of the working tree and prints relative origins from there.
            filters_off: filters_off(&list?, &top, &writable),
            top,
            git_dir,
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

    /// Runs at the top of the working tree, where paths are relative to the whole tree, in the
    /// same repository that `open` found.
    async fn run_at_top(
        &self,
        args: &[&str],
        env: &[(&str, &OsStr)],
        input: Option<&[u8]>,
        secs: u64,
    ) -> Result<Vec<u8>, String> {
        let git_dir = format!("--git-dir={}", self.git_dir.display());
        let work_tree = format!("--work-tree={}", self.top.display());
        let args: Vec<&str> = [git_dir.as_str(), work_tree.as_str()]
            .into_iter()
            .chain(args.iter().copied())
            .collect();
        run_git_input(&self.top, &self.filters_off, &args, env, input, secs).await
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
/// the system or the user's config, read from a file outside every `writable` folder (the
/// project, the working tree, the git folders), since a global `include` may point into them.
/// A driver with any untrusted entry is switched off. Git LFS runs the `lfs.extension.*`
/// commands of the repository's config while it cleans, so an untrusted extension switches
/// every driver off, Git LFS included.
///
/// `--list --show-scope --show-origin -z` prints `scope NUL origin NUL key LF value NUL`.
/// Relative origins are relative to `top`, the top of the working tree.
fn filters_off(list: &[u8], top: &Path, writable: &[PathBuf]) -> Vec<(String, String)> {
    // A folder that cannot be found could hold any origin, so then nothing is trusted.
    let writable: Option<Vec<PathBuf>> = writable
        .iter()
        .map(|folder| top.join(folder).canonicalize().ok())
        .collect();
    let mut outside = std::collections::HashMap::<Vec<u8>, bool>::new();
    let mut trusted = |origin: &[u8]| {
        *outside.entry(origin.to_vec()).or_insert_with(|| {
            let (Some(writable), Some(file)) = (&writable, origin.strip_prefix(b"file:")) else {
                return false;
            };
            let Ok(file) = std::str::from_utf8(file) else {
                return false;
            };
            let Ok(file) = top.join(file).canonicalize() else {
                return false;
            };
            !writable.is_empty() && !writable.iter().any(|folder| file.starts_with(folder))
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
    /// The filters switched off when the tree was written. The tree after the step must be
    /// hashed the same way, or files would differ only in how they were stored.
    filters_off: Vec<(String, String)>,
}

/// Snapshots may hash many files, so they get more time than the quick queries.
const TREE_SECS: u64 = 120;

/// Writes every file git does not ignore (tracked, changed and untracked) as a tree object.
/// It works on a copy of the index, so the real index, the stash list and the files are left
/// alone, and later edits to a file that was already untracked still show up in the diff.
/// With filters switched off, the copy is not used: its entries were cleaned by the filters
/// (Git LFS pointers), and only the files git hashes again would lose that, so untouched files
/// would look changed. Instead the index starts from `HEAD` without file times, and every path
/// of the real index is hashed again, so a file added with `--force` despite an ignore rule
/// stays in. Entries outside a sparse checkout have no file to hash, so they are copied from
/// the real index as they are, as the copy would have kept them.
async fn worktree_tree(repo: &Repo<'_>) -> Result<String, String> {
    let index = repo.run(&["rev-parse", "--git-path", "index"]).await?;
    let index = std::str::from_utf8(&index)
        .ok()
        .and_then(|index| index.lines().next())
        .filter(|index| !index.is_empty())
        .ok_or("git の index の場所を読み取れませんでした")?;
    let index = repo.dir.join(index);
    let temp = std::env::temp_dir().join(format!("gbd-index-{}", uuid::Uuid::new_v4()));
    let result = async {
        let env = [("GIT_INDEX_FILE", temp.as_os_str())];
        if !repo.filters_off.is_empty() {
            let listed = repo
                .run_at_top(&["ls-files", "-z", "-t", "-s"], &[], None, TREE_SECS)
                .await?;
            let tracked = split_tracked(&listed);
            repo.run_with(&["read-tree", "HEAD"], &env, TREE_SECS)
                .await?;
            if !tracked.hash.is_empty() {
                repo.run_at_top(
                    // `--replace`: a file in `HEAD` may be a folder in the index, or the reverse.
                    &[
                        "update-index",
                        "--add",
                        "--remove",
                        "--replace",
                        "-z",
                        "--stdin",
                    ],
                    &env,
                    Some(&tracked.hash),
                    TREE_SECS,
                )
                .await?;
            }
            if !tracked.keep.is_empty() {
                repo.run_at_top(
                    &["update-index", "-z", "--index-info"],
                    &env,
                    Some(&tracked.keep),
                    TREE_SECS,
                )
                .await?;
                repo.run_at_top(
                    &["update-index", "--skip-worktree", "-z", "--stdin"],
                    &env,
                    Some(&tracked.keep_paths),
                    TREE_SECS,
                )
                .await?;
            }
        } else if std::fs::copy(&index, &temp).is_err() {
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

/// The real index split for [`worktree_tree`], each list NUL-terminated as `update-index -z`
/// reads it.
#[derive(Default, Debug, PartialEq, Eq)]
struct Tracked {
    /// Paths to hash again from the working tree.
    hash: Vec<u8>,
    /// Entries outside a sparse checkout, as `mode SP oid SP stage TAB path` for `--index-info`.
    keep: Vec<u8>,
    /// The paths of `keep`, to mark skip-worktree again.
    keep_paths: Vec<u8>,
}

/// `ls-files -z -t -s` prints `TAG SP mode SP oid SP stage TAB path NUL`, where the tag `S`
/// marks an entry outside a sparse checkout (skip-worktree).
fn split_tracked(listed: &[u8]) -> Tracked {
    let mut tracked = Tracked::default();
    for entry in listed.split(|b| *b == 0) {
        let Some((tag, rest)) = entry.split_first() else {
            continue;
        };
        let Some(rest) = rest.strip_prefix(b" ") else {
            continue;
        };
        let Some(tab) = rest.iter().position(|b| *b == b'\t') else {
            continue;
        };
        let path = &rest[tab + 1..];
        if *tag == b'S' {
            tracked.keep.extend_from_slice(rest);
            tracked.keep.push(0);
            tracked.keep_paths.extend_from_slice(path);
            tracked.keep_paths.push(0);
        } else {
            tracked.hash.extend_from_slice(path);
            tracked.hash.push(0);
        }
    }
    tracked
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
        filters_off: repo.filters_off,
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
    if repo.filters_off != snap.filters_off {
        return Err(
            "作業中に filter の設定が変わったため、作業前と同じ方法で比べられません".into(),
        );
    }
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
        // The project is a subfolder of the working tree.
        let top = std::env::temp_dir().join(format!("gbd-filters-{}", uuid::Uuid::new_v4()));
        let project = top.join("project");
        let gitdir = top.join(".git");
        fs::create_dir_all(&project).unwrap();
        fs::create_dir_all(&gitdir).unwrap();
        fs::write(project.join("inside.inc"), "").unwrap();
        fs::write(top.join("top.inc"), "").unwrap();
        fs::write(gitdir.join("git.inc"), "").unwrap();
        let outside =
            std::env::temp_dir().join(format!("gbd-outside-{}.inc", uuid::Uuid::new_v4()));
        fs::write(&outside, "").unwrap();
        let entry = |scope: &str, origin: &str, key: &str| format!("{scope}\0{origin}\0{key}\nv\0");
        let file = |path: &Path| format!("file:{}", path.display());
        let global = file(&outside);
        let list = [
            entry("global", &global, "filter.lfs.clean"),
            entry("global", &global, "filter.lfs.process"),
            entry("local", "file:.git/config", "filter.My.Driver.clean"),
            entry("worktree", "file:.git/config.worktree", "filter.w.required"),
            // Global includes that read files the agent can write.
            entry(
                "global",
                &file(&project.join("inside.inc")),
                "filter.included.clean",
            ),
            // Relative to the top of the working tree, outside the project folder.
            entry("global", "file:top.inc", "filter.top.clean"),
            entry(
                "global",
                &file(&gitdir.join("git.inc")),
                "filter.gitdir.clean",
            ),
            entry(
                "global",
                &file(&top.join("missing.inc")),
                "filter.missing.clean",
            ),
            entry("command", "command line:", "filter.cmd.clean"),
            entry("local", "file:.git/config", "core.bare"),
        ]
        .concat();
        let writable = [project.clone(), top.clone(), gitdir.clone()];
        let names = |off: &[(String, String)]| -> BTreeSet<String> {
            off.iter()
                .map(|(key, _)| key.rsplit_once('.').unwrap().0.to_owned())
                .collect()
        };
        let off = filters_off(list.as_bytes(), &top, &writable);
        assert_eq!(
            names(&off),
            [
                "filter.My.Driver",
                "filter.cmd",
                "filter.gitdir",
                "filter.included",
                "filter.missing",
                "filter.top",
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
        assert!(names(&filters_off(extension.as_bytes(), &top, &writable)).contains("filter.lfs"));
        let _ = fs::remove_dir_all(&top);
        let _ = fs::remove_file(&outside);
    }

    /// A filter that stores files differently (as Git LFS stores pointers) must not make
    /// untouched files look changed once the filter is switched off.
    #[tokio::test]
    async fn filtered_files_compare_the_same_way() {
        let Some(repo) = repository().await else {
            return;
        };
        fs::write(repo.0.join(".gitattributes"), "*.dat filter=p\n").unwrap();
        assert!(
            git(
                &repo.0,
                &["config", "filter.p.clean", "cat >/dev/null; echo pointer"]
            )
            .await
            .is_some()
        );
        let data = repo.0.join("data.dat");
        let touch = |when: std::time::SystemTime| {
            fs::File::options()
                .write(true)
                .open(&data)
                .unwrap()
                .set_modified(when)
                .unwrap();
        };
        fs::write(&data, "real content\n").unwrap();
        commit(&repo.0).await;
        // As long after the commit as a real step: the index entry matches the file, so git
        // would keep the stored pointer instead of hashing the file again.
        touch(std::time::SystemTime::now() - Duration::from_secs(100));
        assert!(
            git(&repo.0, &["update-index", "-q", "--refresh"])
                .await
                .is_some()
        );
        // A file added with `--force` despite an ignore rule, not committed yet.
        fs::write(repo.0.join(".gitignore"), "*.log\n").unwrap();
        fs::write(repo.0.join("forced.log"), "kept\n").unwrap();
        assert!(git(&repo.0, &["add", "-f", "forced.log"]).await.is_some());
        let snap = snapshot(&repo.0).await.unwrap();
        // The step rewrites the file with the same content, so git hashes it again.
        fs::write(&data, "real content\n").unwrap();
        touch(std::time::SystemTime::now() + Duration::from_secs(10));
        fs::write(repo.0.join("b.txt"), "b\n").unwrap();
        fs::remove_file(repo.0.join("forced.log")).unwrap();
        let found = changes(&repo.0, &snap, 200).await.unwrap();
        let paths: Vec<_> = found
            .files
            .iter()
            .map(|file| (file.path.as_str(), file.change))
            .collect();
        assert_eq!(
            paths,
            [
                ("forced.log", FileChange::Deleted),
                ("b.txt", FileChange::Untracked)
            ]
        );
        // A filter set up during the step changes how files are hashed, so the diff is refused.
        assert!(
            git(&repo.0, &["config", "filter.New.clean", "cat"])
                .await
                .is_some()
        );
        assert!(changes(&repo.0, &snap, 200).await.is_err());
    }

    /// A project in a folder of the working tree, with a filter switched off, so the paths of
    /// the real index are hashed again from the top of the working tree.
    #[tokio::test]
    async fn a_project_inside_the_working_tree() {
        let Some(repo) = repository().await else {
            return;
        };
        let sub = repo.0.join("sub");
        fs::create_dir_all(&sub).unwrap();
        fs::write(sub.join("a.txt"), "a\n").unwrap();
        fs::write(repo.0.join("top.txt"), "top\n").unwrap();
        commit(&repo.0).await;
        assert!(
            git(&repo.0, &["config", "filter.x.clean", "cat"])
                .await
                .is_some()
        );
        // Outside the project folder, added with `--force` despite an ignore rule.
        fs::write(repo.0.join(".gitignore"), "*.log\n").unwrap();
        fs::write(repo.0.join("outside.log"), "kept\n").unwrap();
        assert!(git(&repo.0, &["add", "-f", "outside.log"]).await.is_some());
        let snap = snapshot(&sub).await.unwrap();
        fs::write(sub.join("a.txt"), "changed\n").unwrap();
        fs::remove_file(repo.0.join("outside.log")).unwrap();
        let found = changes(&sub, &snap, 200).await.unwrap();
        let paths: Vec<_> = found
            .files
            .iter()
            .map(|file| (file.path.as_str(), file.change))
            .collect();
        assert_eq!(
            paths,
            [
                ("outside.log", FileChange::Deleted),
                ("sub/a.txt", FileChange::Modified)
            ]
        );
    }

    /// With a filter switched off, a file in `HEAD` that the index already has as a folder
    /// still gives a snapshot.
    #[tokio::test]
    async fn a_file_replaced_by_a_folder() {
        let Some(repo) = repository().await else {
            return;
        };
        fs::write(repo.0.join("p"), "file\n").unwrap();
        commit(&repo.0).await;
        assert!(
            git(&repo.0, &["config", "filter.x.clean", "cat"])
                .await
                .is_some()
        );
        fs::remove_file(repo.0.join("p")).unwrap();
        fs::create_dir_all(repo.0.join("p")).unwrap();
        fs::write(repo.0.join("p").join("f"), "inside\n").unwrap();
        assert!(git(&repo.0, &["add", "-A"]).await.is_some());
        let snap = snapshot(&repo.0).await.unwrap();
        fs::write(repo.0.join("p").join("f"), "changed\n").unwrap();
        let found = changes(&repo.0, &snap, 200).await.unwrap();
        let paths: Vec<_> = found
            .files
            .iter()
            .map(|file| (file.path.as_str(), file.change))
            .collect();
        assert_eq!(paths, [("p/f", FileChange::Modified)]);
    }

    #[test]
    fn splits_sparse_entries_from_the_rest() {
        let listed =
            b"H 100644 aaaa 0\ta.txt\0S 100644 bbbb 0\tdir/s t.txt\0M 100644 cccc 2\tm.txt\0";
        assert_eq!(
            split_tracked(listed),
            Tracked {
                hash: b"a.txt\0m.txt\0".to_vec(),
                keep: b"100644 bbbb 0\tdir/s t.txt\0".to_vec(),
                keep_paths: b"dir/s t.txt\0".to_vec(),
            }
        );
        assert_eq!(split_tracked(b""), Tracked::default());
    }

    /// With a filter switched off, a file outside a sparse checkout stays in the snapshot, and
    /// bringing it into the working tree during the step is not a change.
    #[tokio::test]
    async fn sparse_files_stay_in_the_snapshot() {
        let Some(repo) = repository().await else {
            return;
        };
        fs::write(repo.0.join("a.txt"), "a\n").unwrap();
        fs::write(repo.0.join("s.txt"), "s\n").unwrap();
        commit(&repo.0).await;
        assert!(
            git(&repo.0, &["config", "filter.x.clean", "cat"])
                .await
                .is_some()
        );
        assert!(
            git(&repo.0, &["update-index", "--skip-worktree", "s.txt"])
                .await
                .is_some()
        );
        fs::remove_file(repo.0.join("s.txt")).unwrap();
        let snap = snapshot(&repo.0).await.unwrap();
        let base = snap.base.clone().unwrap();
        let names = text(&repo.0, &["ls-tree", "--name-only", &base])
            .await
            .unwrap();
        assert!(names.lines().any(|name| name == "s.txt"));
        assert!(
            git(&repo.0, &["update-index", "--no-skip-worktree", "s.txt"])
                .await
                .is_some()
        );
        assert!(git(&repo.0, &["checkout", "--", "s.txt"]).await.is_some());
        let found = changes(&repo.0, &snap, 200).await.unwrap();
        assert_eq!(found.files, []);
    }

    #[tokio::test]
    async fn absent_repository_is_a_result() {
        let dir = std::env::temp_dir().join(format!("gbd-no-git-{}", uuid::Uuid::new_v4()));
        let check = preflight(&dir).await;
        assert!(!check.is_git && !check.has_commit && check.branch.is_none());
        assert!(snapshot(&dir).await.is_err());
        let snap = Snapshot {
            base: None,
            filters_off: Vec::new(),
        };
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

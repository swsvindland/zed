//! Subversion working copies, exposed through the [`GitRepository`] trait so that the git
//! panel, the diff gutter, blame, and history work with them unchanged.
//!
//! Subversion has no staging area: `svn commit` sends every versioned change. To mirror
//! that, versioned changes start out staged, and unstaging a file puts it in the
//! `ignore-on-commit` changelist, which TortoiseSVN also leaves out of commits by default.
//! Unversioned files show up as untracked, and staging one runs `svn add`. Missing files
//! are staged deletions that get `svn delete`d right before they're committed.
//!
//! Since "staged" means "included in the next commit" rather than "copied into an index",
//! the index text of every file is its `BASE` text, so the diff gutter shows all local
//! changes.

use crate::blame::{Blame, BlameEntry};
use crate::repository::{
    AskPassDelegate, Branch, BranchesScanResult, CommitData, CommitDataReader, CommitDetails,
    CommitDiff, CommitFile, CommitOptions, CommitSummary, CreateWorktreeTarget, DiffStatType,
    DiffType, FetchOptions, FileHistoryChangedFileSets, GitCommitTemplate, GitRepository,
    GitRepositoryCheckpoint, InitialGraphCommitData, LogOrder, LogSource, PushOptions,
    REMOTE_CANCELLED_BY_USER, Remote, RemoteCommandOutput, RepoPath, ResetMode, SearchCommitArgs,
    Upstream, UpstreamTracking, UpstreamTrackingStatus, Worktree,
};
use crate::stash::GitStash;
use crate::status::{
    DiffStat, DiffTreeType, FileStatus, GitDiffStat, GitStatus, StatusCode, TrackedStatus,
    TreeDiff, UnmergedStatus, UnmergedStatusCode,
};
use crate::{DOT_GIT, DOT_SVN, Oid, RunHook};
use anyhow::{Context as _, Result, anyhow, bail};
use askpass::IKnowWhatIAmDoingAndIHaveReadTheDocs;
use async_channel::Sender;
use collections::{HashMap, HashSet};
use futures::future::{BoxFuture, Either};
use futures::{AsyncWriteExt as _, FutureExt as _};
use globset::{Glob, GlobSet, GlobSetBuilder};
use gpui::{AppContext as _, AsyncApp, BackgroundExecutor, SharedString, Task};
use parking_lot::Mutex;
use quick_xml::events::{BytesStart, Event};
use rope::Rope;
use std::collections::BTreeSet;
use std::ffi::{OsStr, OsString};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::Output;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, SystemTime};
use text::LineEnding;
use util::ResultExt as _;
use util::command::{Stdio, new_command};

/// The changelist that holds unstaged files. TortoiseSVN leaves files in a changelist with
/// this name out of commits by default, so it also works outside of Zed.
pub const IGNORE_ON_COMMIT_CHANGELIST: &str = "ignore-on-commit";

/// The working copy database, which Subversion writes on every change to the working copy.
pub const WC_DB: &str = "wc.db";

const REMOTE_NAME: &str = "svn";
const NOT_COMMITTED_AUTHOR: &str = "Not Committed Yet";
const HISTORY_PAGE_SIZE: usize = 500;
const MAX_HISTORY_LENGTH: usize = 10_000;
const WHOLE_FILES_ONLY: &str = "Subversion commits whole files, so individual hunks can't be staged. Stage or unstage the whole file instead.";

/// Subversion's built-in `global-ignores`, used when the user's config doesn't set any.
const DEFAULT_GLOBAL_IGNORES: &str = "*.o *.lo *.la *.al .libs *.so *.so.[0-9]* *.a *.pyc *.pyo __pycache__ *.rej *~ #*# .#* .*.swp .DS_Store [Tt]humbs.db";

/// Returned by `svn` when an operation targets a path inside an unversioned directory.
const NODE_NOT_FOUND_ERROR: &str = "155010";
const AUTHENTICATION_ERRORS: &[&str] = &["E170001", "E215004"];

pub fn is_svn_admin_dir(path: &Path) -> bool {
    path.file_name() == Some(OsStr::new(DOT_SVN))
}

/// Returns the root of the Subversion working copy containing `path`, if any.
pub fn working_copy_root(path: &Path) -> Option<PathBuf> {
    path.ancestors()
        .find(|ancestor| ancestor.join(DOT_SVN).is_dir())
        .map(Path::to_path_buf)
}

/// Moves `from` to `to` with `svn move` so that Subversion records the move, and
/// returns `false` without touching anything when `from` isn't versioned or the
/// destination isn't in the same working copy.
pub async fn move_versioned_path(from: &Path, to: &Path) -> Result<bool> {
    let Some(root) = working_copy_root(from) else {
        return Ok(false);
    };
    let Ok(svn_binary_path) = which::which("svn") else {
        return Ok(false);
    };
    let Some(destination_parent) = to.parent() else {
        return Ok(false);
    };
    let destination_root = destination_parent
        .ancestors()
        .find(|ancestor| ancestor.exists())
        .and_then(working_copy_root);
    if destination_root.as_deref() != Some(root.as_path()) {
        return Ok(false);
    }

    let svn = SvnBinary {
        path: svn_binary_path,
        working_directory: root.clone(),
    };
    let from_relative = relative_path_string(&root, from)?;
    let to_relative = relative_path_string(&root, to)?;

    // `svn info` fails for anything that isn't versioned.
    let info = svn
        .output_with_args(
            [OsString::from("info"), "--".into()],
            [path_target(&from_relative)],
        )
        .await?;
    if !info.status.success() {
        return Ok(false);
    }

    // The destination is taken literally by `svn move`, so it doesn't get a peg revision.
    svn.run_with_args(
        [
            OsString::from("move"),
            "--parents".into(),
            "--allow-mixed-revisions".into(),
            "--".into(),
        ],
        [path_target(&from_relative), OsString::from(&to_relative)],
    )
    .await?;
    Ok(true)
}

fn relative_path_string(root: &Path, path: &Path) -> Result<String> {
    let relative = path
        .strip_prefix(root)
        .with_context(|| format!("{path:?} is not inside {root:?}"))?;
    let relative = relative
        .to_str()
        .with_context(|| format!("{relative:?} is not valid UTF-8"))?;
    Ok(if cfg!(windows) {
        relative.replace('\\', "/")
    } else {
        relative.to_string()
    })
}

pub struct SvnRepository {
    admin_dir: PathBuf,
    svn: SvnBinary,
    executor: BackgroundExecutor,
    global_ignores: Arc<GlobSet>,
    state: Arc<Mutex<SvnState>>,
    is_trusted: Arc<AtomicBool>,
}

#[derive(Default)]
struct SvnState {
    incoming_revisions: Option<u32>,
    last_commit: Option<SvnCommit>,
    /// Revisions committed from this working copy since it was opened. Until the next
    /// `svn update`, the working copy's root still has an older revision, so these would
    /// otherwise look like they're waiting on the server.
    committed_revisions: HashSet<u64>,
    /// Commits loaded with `svn log`, which needs the server, so they're kept around.
    commits: HashMap<u64, SvnCommit>,
    /// The revision before each revision in the history of the last path whose history
    /// was loaded.
    parents: HashMap<u64, u64>,
    requested_messages: HashSet<u64>,
}

impl SvnState {
    fn insert_commits(&mut self, commits: impl IntoIterator<Item = SvnCommit>) {
        for commit in commits {
            self.commits.insert(commit.revision, commit);
        }
    }
}

impl SvnRepository {
    pub fn new(
        admin_dir: &Path,
        svn_binary_path: Option<PathBuf>,
        executor: BackgroundExecutor,
    ) -> Result<Self> {
        let working_directory = admin_dir
            .parent()
            .with_context(|| format!("{admin_dir:?} has no parent directory"))?
            .to_path_buf();
        let svn_binary_path = svn_binary_path
            .or_else(|| which::which("svn").ok())
            .context("Subversion command-line client (svn) not found on $PATH")?;
        Ok(Self {
            admin_dir: admin_dir.to_path_buf(),
            svn: SvnBinary {
                path: svn_binary_path,
                working_directory,
            },
            executor,
            global_ignores: Arc::new(global_ignores()),
            state: Default::default(),
            is_trusted: Arc::new(AtomicBool::new(false)),
        })
    }

    fn working_directory(&self) -> &Path {
        &self.svn.working_directory
    }

    fn unsupported<T: Send + 'static>(&self, operation: &str) -> BoxFuture<'static, Result<T>> {
        let message = format!("{operation} is not supported in Subversion working copies");
        async move { Err(anyhow!(message)) }.boxed()
    }

    fn head_commit(&self) -> BoxFuture<'static, Result<SvnCommit>> {
        let svn = self.svn.clone();
        let state = self.state.clone();
        async move {
            let info = parse_info(&svn.run(["info", "--xml"]).await?)?;
            let mut commit = info.last_changed.context("working copy has no commits")?;
            let state = state.lock();
            if let Some(last_commit) = &state.last_commit
                && last_commit.revision >= commit.revision
            {
                commit = last_commit.clone();
            }
            if commit.message.is_empty()
                && let Some(cached) = state.commits.get(&commit.revision)
            {
                commit.message = cached.message.clone();
            }
            Ok(commit)
        }
        .boxed()
    }

    /// Loads a commit message from the server, since the working copy doesn't store them.
    /// Gives up waiting after a moment so that a slow server doesn't hold up the UI,
    /// leaving the message to finish loading in the background.
    async fn request_commit_message(&self, revision: u64) -> Option<String> {
        if !self.state.lock().requested_messages.insert(revision) {
            return None;
        }
        let load = self.load_commit_message(revision);
        let timeout = self.executor.timer(Duration::from_secs(1));
        match futures::future::select(load, timeout).await {
            Either::Left((commit, _)) => commit.log_err().map(|commit| commit.message),
            Either::Right((_, load)) => {
                self.executor
                    .spawn(async move {
                        load.await.log_err();
                    })
                    .detach();
                None
            }
        }
    }

    fn load_commit_message(&self, revision: u64) -> BoxFuture<'static, Result<SvnCommit>> {
        let svn = self.svn.clone();
        let state = self.state.clone();
        async move {
            let revision_arg = revision.to_string();
            let log = svn
                .run(["log", "--xml", "-r", revision_arg.as_str(), "^/"])
                .await?;
            let commit = parse_log(&log)?
                .into_iter()
                .next()
                .with_context(|| format!("revision {revision} not found"))?;
            state.lock().insert_commits([commit.clone()]);
            Ok(commit)
        }
        .boxed()
    }
}

#[derive(Clone)]
struct SvnBinary {
    path: PathBuf,
    working_directory: PathBuf,
}

impl SvnBinary {
    fn command<I, S>(&self, args: I) -> util::command::Command
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        // This runs `svn`, so git's safety flags don't apply.
        #[allow(clippy::disallowed_methods)]
        let mut command = new_command(&self.path);
        command
            .current_dir(&self.working_directory)
            .arg("--non-interactive")
            .args(args);
        // Without a UTF-8 locale, `svn` refuses non-ASCII paths and commit messages.
        if cfg!(unix)
            && ["LC_ALL", "LC_CTYPE", "LANG"]
                .iter()
                .all(|name| std::env::var_os(name).is_none_or(|value| value.is_empty()))
        {
            command.env(
                "LC_CTYPE",
                if cfg!(target_os = "macos") {
                    "en_US.UTF-8"
                } else {
                    "C.UTF-8"
                },
            );
        }
        command
    }

    async fn output<I, S>(&self, args: I) -> Result<Output>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        self.command(args)
            .output()
            .await
            .with_context(|| format!("failed to run {:?}", self.path))
    }

    async fn output_with_args(
        &self,
        args: impl IntoIterator<Item = OsString>,
        targets: impl IntoIterator<Item = OsString>,
    ) -> Result<Output> {
        self.output(args.into_iter().chain(targets)).await
    }

    async fn run<I, S>(&self, args: I) -> Result<String>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let output = self.output(args).await?;
        check_output(&output)?;
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    }

    async fn run_with_args(
        &self,
        args: impl IntoIterator<Item = OsString>,
        targets: impl IntoIterator<Item = OsString>,
    ) -> Result<String> {
        self.run(args.into_iter().chain(targets)).await
    }

    /// Runs a command that talks to the server, asking for credentials when Subversion
    /// doesn't have any cached and retrying once with them.
    async fn run_with_credentials(
        &self,
        args: Vec<OsString>,
        askpass: &AskPassDelegate,
        env: &HashMap<String, String>,
    ) -> Result<Output> {
        let output = self
            .command(&args)
            .envs(env.iter())
            .output()
            .await
            .with_context(|| format!("failed to run {:?}", self.path))?;
        let stderr = String::from_utf8_lossy(&output.stderr);
        if output.status.success()
            || !AUTHENTICATION_ERRORS
                .iter()
                .any(|error| stderr.contains(error))
        {
            return Ok(output);
        }

        let server = self
            .run(["info", "--show-item", "repos-root-url"])
            .await
            .map(|url| url.trim().to_string())
            .unwrap_or_else(|_| "the Subversion server".to_string());
        let username = ask_for_credential(askpass, format!("Username for {server}:")).await?;
        let password = ask_for_credential(askpass, format!("Password for '{username}':")).await?;

        let mut command = self.command(&args);
        command
            .envs(env.iter())
            .arg("--username")
            .arg(&username)
            .arg("--password-from-stdin")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = command
            .spawn()
            .with_context(|| format!("failed to run {:?}", self.path))?;
        let mut stdin = child.stdin.take().context("svn has no stdin")?;
        stdin.write_all(password.as_bytes()).await?;
        stdin.write_all(b"\n").await?;
        stdin.flush().await?;
        drop(stdin);
        Ok(child.output().await?)
    }
}

async fn ask_for_credential(askpass: &AskPassDelegate, prompt: String) -> Result<String> {
    let credential = askpass
        .ask_password(prompt)
        .await
        .context(REMOTE_CANCELLED_BY_USER)?;
    credential.decrypt(IKnowWhatIAmDoingAndIHaveReadTheDocs)
}

fn check_output(output: &Output) -> Result<()> {
    if output.status.success() {
        Ok(())
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("{}", stderr.trim())
    }
}

/// Formats a working-copy path as a command-line target. Subversion treats everything
/// after the last `@` as a peg revision, so paths containing one get an empty peg.
fn path_target(path: &str) -> OsString {
    if path.is_empty() {
        ".".into()
    } else if path.contains('@') {
        format!("{path}@").into()
    } else {
        path.into()
    }
}

fn repo_path_targets(paths: &[RepoPath]) -> Vec<OsString> {
    if paths.is_empty() || paths.iter().any(|path| path.is_empty()) {
        vec![".".into()]
    } else {
        paths
            .iter()
            .map(|path| path_target(path.as_unix_str()))
            .collect()
    }
}

fn is_within_any(path: &str, prefixes: &[RepoPath]) -> bool {
    prefixes.is_empty()
        || prefixes.iter().any(|prefix| {
            let prefix = prefix.as_unix_str();
            prefix.is_empty()
                || path == prefix
                || path
                    .strip_prefix(prefix)
                    .is_some_and(|rest| rest.starts_with('/'))
        })
}

/// Runs a command against `prefixes`, falling back to the whole working copy when one
/// of them is inside an unversioned directory, which Subversion can't operate on.
async fn run_for_prefixes(svn: &SvnBinary, args: &[&str], prefixes: &[RepoPath]) -> Result<String> {
    let args = args.iter().map(OsString::from).collect::<Vec<_>>();
    let targets = repo_path_targets(prefixes);
    let is_whole_working_copy = targets.len() == 1 && targets[0] == ".";
    let output = svn.output_with_args(args.iter().cloned(), targets).await?;
    let stderr = String::from_utf8_lossy(&output.stderr);
    if is_whole_working_copy || !stderr.contains(NODE_NOT_FOUND_ERROR) {
        check_output(&output)?;
        return Ok(String::from_utf8_lossy(&output.stdout).into_owned());
    }
    svn.run_with_args(args, [OsString::from(".")]).await
}

async fn status_entries(svn: &SvnBinary, prefixes: &[RepoPath]) -> Result<Vec<StatusEntry>> {
    let output = run_for_prefixes(
        svn,
        &["status", "--xml", "--ignore-externals", "--"],
        prefixes,
    )
    .await?;
    Ok(parse_status(&output)?
        .into_iter()
        .filter(|entry| {
            is_within_any(&entry.path, prefixes)
                // An unversioned directory stands in for everything inside it.
                || (entry.item == ItemStatus::Unversioned
                    && prefixes.iter().any(|prefix| {
                        prefix
                            .as_unix_str()
                            .strip_prefix(entry.path.as_str())
                            .is_some_and(|rest| rest.starts_with('/'))
                    }))
        })
        .collect())
}

impl GitRepository for SvnRepository {
    fn load_blob_content(&self, _oid: Oid) -> BoxFuture<'_, Result<Vec<u8>>> {
        self.unsupported("Loading objects by id")
    }

    fn set_index_text(
        &self,
        path: RepoPath,
        content: Option<Vec<u8>>,
        env: Arc<HashMap<String, String>>,
        _is_executable: bool,
    ) -> BoxFuture<'_, anyhow::Result<()>> {
        async move {
            let worktree_path = self.working_directory().join(path.as_std_path());
            let worktree_content = std::fs::read(worktree_path).ok();
            if content == worktree_content {
                return self.stage_paths(vec![path], env).await;
            }
            let base_content = base_text(&self.svn, path.as_unix_str()).await?;
            if content == base_content {
                return self.unstage_paths(vec![path], env).await;
            }
            Err(anyhow!(WHOLE_FILES_ONLY))
        }
        .boxed()
    }

    fn remote_urls(&self) -> BoxFuture<'_, HashMap<String, String>> {
        async move {
            let mut urls = HashMap::default();
            if let Some(info) = self
                .svn
                .run(["info", "--xml"])
                .await
                .and_then(|xml| parse_info(&xml))
                .log_err()
            {
                urls.insert(REMOTE_NAME.to_string(), info.url);
            }
            urls
        }
        .boxed()
    }

    fn revparse_batch(&self, revs: Vec<String>) -> BoxFuture<'_, Result<Vec<Option<String>>>> {
        async move {
            let mut head = None;
            if revs.iter().any(|rev| rev == "HEAD") {
                head = self
                    .head_commit()
                    .await
                    .ok()
                    .map(|commit| Oid::from_svn_revision(commit.revision).to_string());
            }
            Ok(revs
                .iter()
                .map(|rev| (rev == "HEAD").then(|| head.clone()).flatten())
                .collect())
        }
        .boxed()
    }

    fn load_revisions(
        &self,
        revisions: Vec<String>,
    ) -> BoxFuture<'_, Result<Vec<Option<Vec<u8>>>>> {
        async move {
            let mut base_texts: HashMap<String, Option<Vec<u8>>> = HashMap::default();
            let mut results = Vec::with_capacity(revisions.len());
            for revision in revisions {
                // The index has no meaning of its own here, so it's always the base text.
                let base_path = revision
                    .strip_prefix("HEAD:")
                    .or_else(|| revision.strip_prefix(':'));
                if let Some(path) = base_path {
                    if !base_texts.contains_key(path) {
                        let text = base_text(&self.svn, path).await?;
                        base_texts.insert(path.to_string(), text);
                    }
                    results.push(base_texts.get(path).cloned().flatten());
                } else if let Some((rev, path)) = revision.split_once(':')
                    && let Some(revision_number) = parse_revision(rev)
                {
                    results.push(
                        text_at_revision(&self.svn, path, revision_number)
                            .await
                            .ok(),
                    );
                } else {
                    results.push(None);
                }
            }
            Ok(results)
        }
        .boxed()
    }

    fn merge_message(&self) -> BoxFuture<'_, Option<String>> {
        async { None }.boxed()
    }

    fn status(&self, path_prefixes: &[RepoPath]) -> Task<Result<GitStatus>> {
        let svn = self.svn.clone();
        let global_ignores = self.global_ignores.clone();
        let prefixes = path_prefixes.to_vec();
        self.executor.spawn(async move {
            let entries = status_entries(&svn, &prefixes).await?;
            Ok(git_status_from_entries(
                &entries,
                &svn.working_directory,
                &global_ignores,
                &prefixes,
            ))
        })
    }

    fn diff_tree(&self, _request: DiffTreeType) -> BoxFuture<'_, Result<TreeDiff>> {
        self.unsupported("Comparing branches")
    }

    fn stash_entries(&self) -> BoxFuture<'static, Result<GitStash>> {
        async { Ok(GitStash::default()) }.boxed()
    }

    fn branches(&self) -> BoxFuture<'_, Result<BranchesScanResult>> {
        async move {
            let info = parse_info(&self.svn.run(["info", "--xml"]).await?)?;
            let name = branch_name(&info.relative_url);
            let behind = self.state.lock().incoming_revisions.unwrap_or(0);
            let mut head_commit = self.head_commit().await.ok();
            if let Some(commit) = head_commit.as_mut()
                && commit.message.is_empty()
                && let Some(message) = self.request_commit_message(commit.revision).await
            {
                commit.message = message;
            }
            let most_recent_commit = head_commit.map(|commit| {
                let subject = match commit.message.lines().next() {
                    Some(subject) if !subject.trim().is_empty() => subject.to_string(),
                    _ => format!("r{}", commit.revision),
                };
                CommitSummary {
                    sha: Oid::from_svn_revision(commit.revision).to_string().into(),
                    subject: subject.into(),
                    commit_timestamp: commit.timestamp,
                    author_name: commit.author.into(),
                    // Subversion commits can't be undone, so don't offer to.
                    has_parent: false,
                }
            });
            Ok(vec![Branch {
                is_head: true,
                ref_name: format!("refs/heads/{name}").into(),
                upstream: Some(Upstream {
                    ref_name: format!("refs/remotes/{REMOTE_NAME}/{name}").into(),
                    tracking: UpstreamTracking::Tracked(UpstreamTrackingStatus {
                        ahead: 0,
                        behind,
                    }),
                }),
                most_recent_commit,
            }]
            .into())
        }
        .boxed()
    }

    fn change_branch(&self, _name: String) -> BoxFuture<'_, Result<()>> {
        self.unsupported("Switching branches")
    }

    fn create_branch(
        &self,
        _name: String,
        _base_branch: Option<String>,
    ) -> BoxFuture<'_, Result<()>> {
        self.unsupported("Creating branches")
    }

    fn rename_branch(&self, _branch: String, _new_name: String) -> BoxFuture<'_, Result<()>> {
        self.unsupported("Renaming branches")
    }

    fn delete_branch(
        &self,
        _is_remote: bool,
        _name: String,
        _force: bool,
    ) -> BoxFuture<'_, Result<()>> {
        self.unsupported("Deleting branches")
    }

    fn worktrees(&self) -> BoxFuture<'_, Result<Vec<Worktree>>> {
        async { Ok(Vec::new()) }.boxed()
    }

    fn worktree_created_at(
        &self,
        _worktree_path: PathBuf,
    ) -> BoxFuture<'_, Result<Option<SystemTime>>> {
        async { Ok(None) }.boxed()
    }

    fn create_worktree(
        &self,
        _target: CreateWorktreeTarget,
        _path: PathBuf,
    ) -> BoxFuture<'_, Result<()>> {
        self.unsupported("Creating worktrees")
    }

    fn checkout_branch_in_worktree(
        &self,
        _branch_name: String,
        _worktree_path: PathBuf,
        _create: bool,
    ) -> BoxFuture<'_, Result<()>> {
        self.unsupported("Worktrees")
    }

    fn remove_worktree(&self, _path: PathBuf, _force: bool) -> BoxFuture<'_, Result<()>> {
        self.unsupported("Removing worktrees")
    }

    fn rename_worktree(&self, _old_path: PathBuf, _new_path: PathBuf) -> BoxFuture<'_, Result<()>> {
        self.unsupported("Renaming worktrees")
    }

    fn reset(
        &self,
        _commit: String,
        _mode: ResetMode,
        _env: Arc<HashMap<String, String>>,
    ) -> BoxFuture<'_, Result<()>> {
        async {
            bail!(
                "Subversion commits are made directly on the server, so they can't be undone locally"
            )
        }
        .boxed()
    }

    fn checkout_files(
        &self,
        commit: String,
        paths: Vec<RepoPath>,
        env: Arc<HashMap<String, String>>,
    ) -> BoxFuture<'_, Result<()>> {
        let svn = self.svn.clone();
        self.executor
            .spawn(async move {
                if commit != "HEAD" {
                    bail!("Subversion can only restore files to their checked-out revision");
                }
                if paths.is_empty() {
                    return Ok(());
                }
                let entries = status_entries(&svn, &[]).await?;
                let removed_paths = entries
                    .iter()
                    .filter(|entry| matches!(entry.item, ItemStatus::Missing | ItemStatus::Deleted))
                    .map(|entry| entry.path.as_str())
                    .collect::<HashSet<_>>();

                // A file in a removed directory can only be restored along with the directory.
                let mut targets = BTreeSet::new();
                for path in &paths {
                    let path = path.as_unix_str();
                    targets.insert(path.to_string());
                    for ancestor in path_ancestors(path) {
                        if removed_paths.contains(ancestor) {
                            targets.insert(ancestor.to_string());
                        }
                    }
                }
                let mut args = vec![
                    OsString::from("revert"),
                    "--depth".into(),
                    "empty".into(),
                    "--".into(),
                ];
                args.extend(targets.iter().map(|path| path_target(path)));
                let output = svn.command(args).envs(env.iter()).output().await?;
                check_output(&output)?;

                let in_changelist = entries
                    .iter()
                    .filter(|entry| {
                        entry.is_excluded_from_commit() && targets.contains(&entry.path)
                    })
                    .map(|entry| path_target(&entry.path))
                    .collect::<Vec<_>>();
                if !in_changelist.is_empty() {
                    svn.run_with_args(
                        [OsString::from("changelist"), "--remove".into(), "--".into()],
                        in_changelist,
                    )
                    .await?;
                }
                Ok(())
            })
            .boxed()
    }

    fn show(&self, commit: String) -> BoxFuture<'_, Result<CommitDetails>> {
        async move {
            let commit = if commit == "HEAD" {
                self.head_commit().await?
            } else {
                let revision = parse_revision(&commit)
                    .with_context(|| format!("unknown revision {commit}"))?;
                self.load_commit_message(revision).await?
            };
            Ok(CommitDetails {
                sha: Oid::from_svn_revision(commit.revision).to_string().into(),
                message: commit.message.into(),
                commit_timestamp: commit.timestamp,
                author_email: SharedString::default(),
                author_name: commit.author.into(),
            })
        }
        .boxed()
    }

    fn load_commit(
        &self,
        commit: String,
        _ignore_shallow_boundary: bool,
        cx: AsyncApp,
    ) -> BoxFuture<'_, Result<CommitDiff>> {
        let svn = self.svn.clone();
        cx.background_spawn(async move {
            let revision =
                parse_revision(&commit).with_context(|| format!("unknown revision {commit}"))?;
            let info = parse_info(&svn.run(["info", "--xml"]).await?)?;
            let working_copy_prefix = info.relative_url.trim_start_matches('^').to_string();
            let revision_arg = revision.to_string();
            let log = svn
                .run(["log", "--xml", "-v", "-r", revision_arg.as_str(), "."])
                .await?;
            let changed_paths = parse_changed_paths(&log)?;

            let mut files = Vec::new();
            for changed_path in changed_paths {
                if changed_path.kind == "dir" {
                    continue;
                }
                let Some(relative) = changed_path
                    .path
                    .strip_prefix(&working_copy_prefix)
                    .and_then(|relative| relative.strip_prefix('/'))
                else {
                    continue;
                };
                let Ok(repo_path) = RepoPath::new(relative) else {
                    continue;
                };
                let url = format!("{}{}", info.repository_root, changed_path.path);
                let new_content = if changed_path.action == "D" {
                    None
                } else {
                    cat_url(&svn, &url, revision).await.log_err()
                };
                let old_content = if changed_path.action == "A" || revision == 0 {
                    None
                } else {
                    cat_url(&svn, &url, revision - 1).await.ok()
                };
                let is_binary = [&old_content, &new_content]
                    .into_iter()
                    .flatten()
                    .any(|content| content.contains(&0));
                files.push(CommitFile {
                    path: repo_path,
                    old_content,
                    new_content,
                    is_binary,
                });
            }
            Ok(CommitDiff {
                files,
                is_shallow_boundary: false,
            })
        })
        .boxed()
    }

    fn blame(
        &self,
        path: RepoPath,
        content: Rope,
        line_ending: LineEnding,
    ) -> BoxFuture<'_, Result<Blame>> {
        let svn = self.svn.clone();
        let state = self.state.clone();
        self.executor
            .spawn(async move {
                let current_text =
                    text::chunks_with_line_ending(&content, line_ending).collect::<String>();
                let base = base_text(&svn, path.as_unix_str())
                    .await?
                    .context("file is not under version control")?;
                let base_text = String::from_utf8_lossy(&base).into_owned();
                let blame_output = svn
                    .run_with_args(
                        [OsString::from("blame"), "--xml".into(), "--".into()],
                        [path_target(path.as_unix_str())],
                    )
                    .await?;
                let blame_lines = parse_blame(&blame_output)?;
                let messages = load_messages(&svn, &state, path.as_unix_str(), &blame_lines).await;
                Ok(build_blame(
                    &path,
                    &blame_lines,
                    &base_text,
                    &current_text,
                    messages,
                ))
            })
            .boxed()
    }

    fn blame_at_revision(&self, path: RepoPath, revision: Oid) -> BoxFuture<'_, Result<Blame>> {
        let svn = self.svn.clone();
        let state = self.state.clone();
        self.executor
            .spawn(async move {
                let revision = revision
                    .svn_revision()
                    .context("not a Subversion revision")?;
                let target = format!("{}@{revision}", path.as_unix_str());
                let blame_output = svn
                    .run_with_args(
                        [OsString::from("blame"), "--xml".into(), "--".into()],
                        [OsString::from(&target)],
                    )
                    .await?;
                let blame_lines = parse_blame(&blame_output)?;
                let text = text_at_revision(&svn, path.as_unix_str(), revision).await?;
                let text = String::from_utf8_lossy(&text).into_owned();
                let messages = load_messages(&svn, &state, &target, &blame_lines).await;
                Ok(build_blame(&path, &blame_lines, &text, &text, messages))
            })
            .boxed()
    }

    fn path(&self) -> PathBuf {
        self.admin_dir.clone()
    }

    fn main_repository_path(&self) -> PathBuf {
        self.admin_dir.clone()
    }

    fn stage_paths(
        &self,
        paths: Vec<RepoPath>,
        env: Arc<HashMap<String, String>>,
    ) -> BoxFuture<'_, Result<()>> {
        let svn = self.svn.clone();
        self.executor
            .spawn(async move {
                if paths.is_empty() {
                    return Ok(());
                }
                let entries = status_entries(&svn, &paths).await?;
                let entries_by_path = entries
                    .iter()
                    .map(|entry| (entry.path.as_str(), entry))
                    .collect::<HashMap<_, _>>();

                let mut to_add = Vec::new();
                let mut to_resolve = Vec::new();
                let mut to_include = Vec::new();
                for path in &paths {
                    let path = path.as_unix_str();
                    match entries_by_path.get(path) {
                        Some(entry) => {
                            if entry.item == ItemStatus::Unversioned {
                                to_add.push(path_target(path));
                                continue;
                            }
                            if entry.change() == Some(Change::Conflicted) {
                                to_resolve.push(path_target(path));
                            }
                            if entry.is_excluded_from_commit() {
                                to_include.push(path_target(path));
                            }
                        }
                        // Files inside unversioned directories aren't reported individually.
                        None if path_ancestors(path).any(|ancestor| {
                            entries_by_path
                                .get(ancestor)
                                .is_some_and(|entry| entry.item == ItemStatus::Unversioned)
                        }) =>
                        {
                            to_add.push(path_target(path));
                        }
                        None => {}
                    }
                }

                let run = |args: &[&str], targets: Vec<OsString>| {
                    let mut command = svn.command(args.iter().chain(&["--"]));
                    command.args(targets).envs(env.iter());
                    async move {
                        let output = command.output().await?;
                        check_output(&output)
                    }
                };
                if !to_add.is_empty() {
                    run(&["add", "--parents", "--depth", "empty"], to_add).await?;
                }
                if !to_resolve.is_empty() {
                    run(&["resolve", "--accept", "working"], to_resolve).await?;
                }
                if !to_include.is_empty() {
                    run(&["changelist", "--remove"], to_include).await?;
                }
                Ok(())
            })
            .boxed()
    }

    fn unstage_paths(
        &self,
        paths: Vec<RepoPath>,
        env: Arc<HashMap<String, String>>,
    ) -> BoxFuture<'_, Result<()>> {
        let svn = self.svn.clone();
        self.executor
            .spawn(async move {
                if paths.is_empty() {
                    return Ok(());
                }
                let entries = status_entries(&svn, &paths).await?;
                let requested = paths
                    .iter()
                    .map(|path| path.as_unix_str())
                    .collect::<HashSet<_>>();
                let to_exclude = entries
                    .iter()
                    .filter(|entry| {
                        requested.contains(entry.path.as_str())
                            && !entry.is_excluded_from_commit()
                            && matches!(
                                entry.change(),
                                Some(
                                    Change::Added
                                        | Change::Deleted
                                        | Change::Missing
                                        | Change::Modified
                                        | Change::TypeChanged
                                )
                            )
                    })
                    .map(|entry| path_target(&entry.path))
                    .collect::<Vec<_>>();
                if to_exclude.is_empty() {
                    return Ok(());
                }
                let mut command = svn.command(["changelist", IGNORE_ON_COMMIT_CHANGELIST, "--"]);
                command.args(to_exclude).envs(env.iter());
                check_output(&command.output().await?)
            })
            .boxed()
    }

    fn run_hook(
        &self,
        _hook: RunHook,
        _env: Arc<HashMap<String, String>>,
    ) -> BoxFuture<'_, Result<()>> {
        async { Ok(()) }.boxed()
    }

    fn commit(
        &self,
        message: SharedString,
        _name_and_email: Option<(SharedString, SharedString)>,
        options: CommitOptions,
        askpass: AskPassDelegate,
        env: Arc<HashMap<String, String>>,
    ) -> BoxFuture<'_, Result<()>> {
        let svn = self.svn.clone();
        let state = self.state.clone();
        let executor = self.executor.clone();
        async move {
            if options.amend {
                bail!("Subversion commits can't be amended");
            }
            let plan = executor
                .spawn({
                    let svn = svn.clone();
                    async move {
                        let entries = status_entries(&svn, &[]).await?;
                        anyhow::Ok(plan_commit(&entries, &svn.working_directory))
                    }
                })
                .await?;
            if plan.targets.is_empty() {
                bail!("No changes to commit");
            }
            if !plan.to_revert.is_empty() {
                let targets = plan.to_revert.iter().map(String::as_str).map(path_target);
                svn.run_with_args(
                    [
                        OsString::from("revert"),
                        "--depth".into(),
                        "empty".into(),
                        "--".into(),
                    ],
                    targets.collect::<Vec<_>>(),
                )
                .await?;
            }
            if !plan.to_delete.is_empty() {
                let targets = plan.to_delete.iter().map(String::as_str).map(path_target);
                svn.run_with_args(
                    [OsString::from("delete"), "--".into()],
                    targets.collect::<Vec<_>>(),
                )
                .await?;
            }

            let mut message_file = tempfile::NamedTempFile::new()?;
            message_file.write_all(message.as_bytes())?;
            message_file.flush()?;
            let mut targets_file = tempfile::NamedTempFile::new()?;
            for target in &plan.targets {
                targets_file.write_all(path_target(target).as_encoded_bytes())?;
                targets_file.write_all(b"\n")?;
            }
            targets_file.flush()?;

            let output = svn
                .run_with_credentials(
                    vec![
                        "commit".into(),
                        "--depth".into(),
                        "empty".into(),
                        "--encoding".into(),
                        "UTF-8".into(),
                        "-F".into(),
                        message_file.path().into(),
                        "--targets".into(),
                        targets_file.path().into(),
                    ],
                    &askpass,
                    &env,
                )
                .await?;
            check_output(&output)?;

            let stdout = String::from_utf8_lossy(&output.stdout);
            if let Some(revision) = parse_committed_revision(&stdout) {
                {
                    let mut state = state.lock();
                    state.committed_revisions.insert(revision);
                    state.last_commit = Some(SvnCommit {
                        revision,
                        author: String::new(),
                        timestamp: time::OffsetDateTime::now_utc().unix_timestamp(),
                        message: message.to_string(),
                    });
                }
                // Fill in the author as recorded by the server.
                let revision_arg = revision.to_string();
                if let Some(commit) = svn
                    .run(["log", "--xml", "-r", revision_arg.as_str(), "^/"])
                    .await
                    .and_then(|log| parse_log(&log))
                    .log_err()
                    .and_then(|commits| commits.into_iter().next())
                {
                    state.lock().last_commit = Some(commit);
                }
            }
            Ok(())
        }
        .boxed()
    }

    fn stash_paths(
        &self,
        _paths: Vec<RepoPath>,
        _message: Option<String>,
        _env: Arc<HashMap<String, String>>,
    ) -> BoxFuture<'_, Result<()>> {
        self.unsupported("Stashing")
    }

    fn stash_staged(
        &self,
        _message: Option<String>,
        _env: Arc<HashMap<String, String>>,
    ) -> BoxFuture<'_, Result<()>> {
        self.unsupported("Stashing")
    }

    fn stash_pop(
        &self,
        _index: Option<usize>,
        _env: Arc<HashMap<String, String>>,
    ) -> BoxFuture<'_, Result<()>> {
        self.unsupported("Stashing")
    }

    fn stash_apply(
        &self,
        _index: Option<usize>,
        _env: Arc<HashMap<String, String>>,
    ) -> BoxFuture<'_, Result<()>> {
        self.unsupported("Stashing")
    }

    fn stash_drop(
        &self,
        _index: Option<usize>,
        _env: Arc<HashMap<String, String>>,
    ) -> BoxFuture<'_, Result<()>> {
        self.unsupported("Stashing")
    }

    fn push(
        &self,
        _branch_name: String,
        _remote_branch_name: String,
        _upstream_name: String,
        _options: Option<PushOptions>,
        _askpass: AskPassDelegate,
        _env: Arc<HashMap<String, String>>,
        _cx: AsyncApp,
    ) -> BoxFuture<'_, Result<RemoteCommandOutput>> {
        async {
            Ok(RemoteCommandOutput {
                stdout: "Subversion sends commits to the server when they're made, so there's nothing to push.".into(),
                stderr: String::new(),
            })
        }
        .boxed()
    }

    fn pull(
        &self,
        _branch_name: Option<String>,
        _upstream_name: String,
        _rebase: bool,
        askpass: AskPassDelegate,
        env: Arc<HashMap<String, String>>,
        _cx: AsyncApp,
    ) -> BoxFuture<'_, Result<RemoteCommandOutput>> {
        let svn = self.svn.clone();
        let state = self.state.clone();
        async move {
            let output = svn
                .run_with_credentials(vec!["update".into()], &askpass, &env)
                .await?;
            check_output(&output)?;
            let mut state = state.lock();
            state.incoming_revisions = Some(0);
            state.committed_revisions.clear();
            Ok(RemoteCommandOutput {
                stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
                stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
            })
        }
        .boxed()
    }

    fn fetch(
        &self,
        _fetch_options: FetchOptions,
        askpass: AskPassDelegate,
        env: Arc<HashMap<String, String>>,
        _cx: AsyncApp,
    ) -> BoxFuture<'_, Result<RemoteCommandOutput>> {
        let svn = self.svn.clone();
        let state = self.state.clone();
        async move {
            let info = parse_info(&svn.run(["info", "--xml"]).await?)?;
            let base_revision = info.revision.context("working copy has no revision")?;
            let output = svn
                .run_with_credentials(
                    vec![
                        "log".into(),
                        "--xml".into(),
                        "-r".into(),
                        "BASE:HEAD".into(),
                        ".".into(),
                    ],
                    &askpass,
                    &env,
                )
                .await?;
            check_output(&output)?;
            let commits = parse_log(&String::from_utf8_lossy(&output.stdout))?;
            let mut state = state.lock();
            let incoming = commits
                .iter()
                .filter(|commit| {
                    commit.revision > base_revision
                        && !state.committed_revisions.contains(&commit.revision)
                })
                .count();
            state.insert_commits(commits);
            state.incoming_revisions = Some(incoming as u32);
            let stdout = match incoming {
                0 => "The working copy is up to date.".to_string(),
                1 => "1 new revision on the server.".to_string(),
                count => format!("{count} new revisions on the server."),
            };
            Ok(RemoteCommandOutput {
                stdout,
                stderr: String::new(),
            })
        }
        .boxed()
    }

    fn get_push_remote(&self, _branch: String) -> BoxFuture<'_, Result<Option<Remote>>> {
        async { Ok(Some(svn_remote())) }.boxed()
    }

    fn get_branch_remote(&self, _branch: String) -> BoxFuture<'_, Result<Option<Remote>>> {
        async { Ok(Some(svn_remote())) }.boxed()
    }

    fn get_all_remotes(&self) -> BoxFuture<'_, Result<Vec<Remote>>> {
        async { Ok(vec![svn_remote()]) }.boxed()
    }

    fn remove_remote(&self, _name: String) -> BoxFuture<'_, Result<()>> {
        self.unsupported("Removing remotes")
    }

    fn create_remote(&self, _name: String, _url: String) -> BoxFuture<'_, Result<()>> {
        self.unsupported("Adding remotes")
    }

    fn check_for_pushed_commit(&self) -> BoxFuture<'_, Result<Vec<SharedString>>> {
        async { Ok(vec![SharedString::from(REMOTE_NAME)]) }.boxed()
    }

    fn diff(&self, diff: DiffType) -> BoxFuture<'_, Result<String>> {
        let svn = self.svn.clone();
        self.executor
            .spawn(async move {
                match diff {
                    DiffType::HeadToWorktree => {
                        svn.run(["diff", "--internal-diff", "--ignore-properties"])
                            .await
                    }
                    DiffType::HeadToIndex => {
                        let all = svn
                            .run(["diff", "--internal-diff", "--ignore-properties"])
                            .await?;
                        let excluded = excluded_paths(&svn).await?;
                        Ok(split_diff_by_file(&all)
                            .into_iter()
                            .filter(|(path, _)| !excluded.contains(*path))
                            .map(|(_, section)| section)
                            .collect())
                    }
                    DiffType::MergeBase { .. } => {
                        bail!("Comparing branches is not supported in Subversion working copies")
                    }
                }
            })
            .boxed()
    }

    fn diff_stat(
        &self,
        diff: DiffStatType,
        path_prefixes: &[RepoPath],
    ) -> BoxFuture<'static, Result<GitDiffStat>> {
        let svn = self.svn.clone();
        let prefixes = path_prefixes.to_vec();
        self.executor
            .spawn(async move {
                let diff_args = ["diff", "--internal-diff", "--ignore-properties", "--"];
                let output = run_for_prefixes(&svn, &diff_args, &prefixes).await?;
                let stats = diff_stats(&output);
                let entries = match diff {
                    DiffStatType::HeadToWorktree => stats,
                    DiffStatType::HeadToIndex | DiffStatType::IndexToWorktree => {
                        let excluded = excluded_paths(&svn).await?;
                        let want_excluded = matches!(diff, DiffStatType::IndexToWorktree);
                        stats
                            .into_iter()
                            .filter(|(path, _)| excluded.contains(path) == want_excluded)
                            .collect()
                    }
                };
                Ok(GitDiffStat {
                    entries: entries
                        .into_iter()
                        .filter(|(path, _)| is_within_any(path, &prefixes))
                        .filter_map(|(path, stat)| Some((RepoPath::new(&path).ok()?, stat)))
                        .collect(),
                })
            })
            .boxed()
    }

    fn checkpoint(&self) -> BoxFuture<'static, Result<GitRepositoryCheckpoint>> {
        self.unsupported("Checkpoints")
    }

    fn restore_checkpoint(
        &self,
        _checkpoint: GitRepositoryCheckpoint,
    ) -> BoxFuture<'_, Result<()>> {
        self.unsupported("Checkpoints")
    }

    fn create_archive_checkpoint(&self) -> BoxFuture<'_, Result<(String, String)>> {
        self.unsupported("Checkpoints")
    }

    fn restore_archive_checkpoint(
        &self,
        _staged_sha: String,
        _unstaged_sha: String,
    ) -> BoxFuture<'_, Result<()>> {
        self.unsupported("Checkpoints")
    }

    fn compare_checkpoints(
        &self,
        _left: GitRepositoryCheckpoint,
        _right: GitRepositoryCheckpoint,
    ) -> BoxFuture<'_, Result<bool>> {
        self.unsupported("Checkpoints")
    }

    fn diff_checkpoints(
        &self,
        _base_checkpoint: GitRepositoryCheckpoint,
        _target_checkpoint: GitRepositoryCheckpoint,
    ) -> BoxFuture<'_, Result<String>> {
        self.unsupported("Checkpoints")
    }

    fn load_commit_template(&self) -> BoxFuture<'_, Result<Option<GitCommitTemplate>>> {
        async { Ok(None) }.boxed()
    }

    fn default_branch(
        &self,
        _include_remote_name: bool,
    ) -> BoxFuture<'_, Result<Option<SharedString>>> {
        async { Ok(None) }.boxed()
    }

    fn initial_graph_data(
        &self,
        log_source: LogSource,
        _log_order: LogOrder,
        request_tx: Sender<Vec<Arc<InitialGraphCommitData>>>,
    ) -> BoxFuture<'_, Result<()>> {
        let svn = self.svn.clone();
        let state = self.state.clone();
        async move {
            let (target, start) = match &log_source {
                // The working copy's root keeps its revision until `svn update`, even after
                // committing files inside it, so its history starts at the server's latest
                // revision instead.
                LogSource::All | LogSource::Branch(_) => {
                    let latest = svn
                        .run(["info", "--show-item", "revision", "-r", "HEAD", "--", "."])
                        .await;
                    let start = match latest {
                        Ok(revision) => revision.trim().to_string(),
                        Err(error) => {
                            log::warn!("failed to get the latest revision: {error:#}");
                            "BASE".to_string()
                        }
                    };
                    (".".into(), start)
                }
                LogSource::Sha(oid) => (
                    ".".into(),
                    oid.svn_revision()
                        .context("not a Subversion revision")?
                        .to_string(),
                ),
                LogSource::Path(path) => (path_target(path.as_unix_str()), "BASE".to_string()),
            };

            load_history(&svn, &state, target, start, HISTORY_PAGE_SIZE, &request_tx).await
        }
        .boxed()
    }

    fn search_commits(
        &self,
        _log_source: LogSource,
        _search_args: SearchCommitArgs,
        _request_tx: Sender<Oid>,
    ) -> BoxFuture<'_, Result<()>> {
        self.unsupported("Searching commits")
    }

    fn file_history_changed_files(
        &self,
        _paths: Vec<RepoPath>,
        _commit_limit: usize,
    ) -> BoxFuture<'_, Result<Vec<FileHistoryChangedFileSets>>> {
        self.unsupported("File history")
    }

    fn commit_data_reader(&self) -> Result<CommitDataReader> {
        let svn = self.svn.clone();
        let state = self.state.clone();
        Ok(CommitDataReader::new(&self.executor, move |sha| {
            let svn = svn.clone();
            let state = state.clone();
            async move {
                let revision = sha.svn_revision().context("not a Subversion revision")?;
                let cached = state.lock().commits.get(&revision).cloned();
                let commit = match cached {
                    Some(commit) => commit,
                    None => {
                        let revision_arg = revision.to_string();
                        let log = svn
                            .run(["log", "--xml", "-r", revision_arg.as_str(), "^/"])
                            .await?;
                        let commit = parse_log(&log)?
                            .into_iter()
                            .next()
                            .with_context(|| format!("revision {revision} not found"))?;
                        state.lock().insert_commits([commit.clone()]);
                        commit
                    }
                };
                let parent = state.lock().parents.get(&revision).copied();
                Ok(CommitData {
                    sha,
                    parents: parent.map(Oid::from_svn_revision).into_iter().collect(),
                    author_name: commit.author.into(),
                    author_email: SharedString::default(),
                    commit_timestamp: commit.timestamp,
                    subject: commit
                        .message
                        .lines()
                        .next()
                        .unwrap_or_default()
                        .to_string()
                        .into(),
                    message: commit.message.into(),
                })
            }
        }))
    }

    fn create_ref(&self, _ref_name: String, _commit: String) -> BoxFuture<'_, Result<()>> {
        self.unsupported("Creating refs")
    }

    fn update_ref(&self, _ref_name: String, _commit: String) -> BoxFuture<'_, Result<()>> {
        self.unsupported("Updating refs")
    }

    fn delete_ref(&self, _ref_name: String) -> BoxFuture<'_, Result<()>> {
        self.unsupported("Deleting refs")
    }

    fn repair_worktrees(&self) -> BoxFuture<'_, Result<()>> {
        async { Ok(()) }.boxed()
    }

    fn set_trusted(&self, trusted: bool) {
        self.is_trusted.store(trusted, Ordering::Release);
    }

    fn is_trusted(&self) -> bool {
        self.is_trusted.load(Ordering::Acquire)
    }
}

/// Sends the history of `target`, starting at `start`, to `request_tx` for the commit
/// graph.
async fn load_history(
    svn: &SvnBinary,
    state: &Mutex<SvnState>,
    target: OsString,
    mut start: String,
    page_size: usize,
    request_tx: &Sender<Vec<Arc<InitialGraphCommitData>>>,
) -> Result<()> {
    // Subversion history is linear, so each revision's parent is the next older
    // revision that touched the same path. History comes from the server, so it's
    // loaded a page at a time.
    let graph_commit = |revision: u64, parent: Option<u64>| {
        Arc::new(InitialGraphCommitData {
            sha: Oid::from_svn_revision(revision),
            parents: parent.map(Oid::from_svn_revision).into_iter().collect(),
            ref_names: Vec::new(),
        })
    };
    // A revision is sent once the next older one is known, since that's its parent.
    let mut newest_unsent: Option<u64> = None;
    let mut loaded = 0;
    loop {
        let range = format!("{start}:1");
        let page_size_arg = page_size.to_string();
        let log = svn
            .run_with_args(
                [
                    OsString::from("log"),
                    "--xml".into(),
                    "-l".into(),
                    page_size_arg.into(),
                    "-r".into(),
                    range.into(),
                    "--".into(),
                ],
                [target.clone()],
            )
            .await?;
        let commits = parse_log(&log)?;
        let Some(oldest) = commits.last().map(|commit| commit.revision) else {
            break;
        };
        let page_len = commits.len();
        let mut graph_commits = Vec::with_capacity(page_len);
        {
            let mut state = state.lock();
            for commit in &commits {
                if let Some(newer) = newest_unsent {
                    state.parents.insert(newer, commit.revision);
                    graph_commits.push(graph_commit(newer, Some(commit.revision)));
                }
                newest_unsent = Some(commit.revision);
            }
            state.insert_commits(commits);
        }
        if !graph_commits.is_empty() && request_tx.send(graph_commits).await.is_err() {
            return Ok(());
        }
        loaded += page_len;
        if page_len < page_size || oldest <= 1 || loaded >= MAX_HISTORY_LENGTH {
            break;
        }
        start = (oldest - 1).to_string();
    }
    if let Some(oldest) = newest_unsent {
        request_tx.send(vec![graph_commit(oldest, None)]).await.ok();
    }
    Ok(())
}

fn svn_remote() -> Remote {
    Remote {
        name: REMOTE_NAME.into(),
    }
}

/// Names the checked-out location after its path in the repository, e.g. `trunk` or
/// `branches/release-1.2`.
fn branch_name(relative_url: &str) -> String {
    let name = relative_url.trim_start_matches('^').trim_matches('/');
    if name.is_empty() {
        "root".to_string()
    } else {
        name.to_string()
    }
}

/// Accepts the revision ids produced by [`Oid::from_svn_revision`], as well as plain or
/// `r`-prefixed revision numbers.
fn parse_revision(revision: &str) -> Option<u64> {
    if let Ok(oid) = revision.parse::<Oid>()
        && let Some(revision) = oid.svn_revision()
    {
        return Some(revision);
    }
    revision.strip_prefix('r').unwrap_or(revision).parse().ok()
}

fn parse_committed_revision(output: &str) -> Option<u64> {
    output.lines().find_map(|line| {
        line.trim()
            .strip_prefix("Committed revision ")?
            .trim_end_matches('.')
            .parse()
            .ok()
    })
}

async fn base_text(svn: &SvnBinary, path: &str) -> Result<Option<Vec<u8>>> {
    let output = svn
        .output_with_args(
            [
                OsString::from("cat"),
                "-r".into(),
                "BASE".into(),
                "--".into(),
            ],
            [path_target(path)],
        )
        .await?;
    // Unversioned and newly added files have no base text.
    Ok(output.status.success().then_some(output.stdout))
}

async fn text_at_revision(svn: &SvnBinary, path: &str, revision: u64) -> Result<Vec<u8>> {
    let output = svn
        .output_with_args(
            [OsString::from("cat"), "--".into()],
            [OsString::from(format!("{path}@{revision}"))],
        )
        .await?;
    check_output(&output)?;
    Ok(output.stdout)
}

async fn cat_url(svn: &SvnBinary, url: &str, revision: u64) -> Result<Vec<u8>> {
    let output = svn
        .output_with_args(
            [OsString::from("cat"), "--".into()],
            [OsString::from(format!("{url}@{revision}"))],
        )
        .await?;
    check_output(&output)?;
    Ok(output.stdout)
}

async fn excluded_paths(svn: &SvnBinary) -> Result<HashSet<String>> {
    let output = svn
        .run([
            "status",
            "--xml",
            "--ignore-externals",
            "--changelist",
            IGNORE_ON_COMMIT_CHANGELIST,
            ".",
        ])
        .await?;
    Ok(parse_status(&output)?
        .into_iter()
        .filter(|entry| entry.is_excluded_from_commit())
        .map(|entry| entry.path)
        .collect())
}

async fn load_messages(
    svn: &SvnBinary,
    state: &Mutex<SvnState>,
    target: &str,
    blame_lines: &[BlameLine],
) -> HashMap<u64, String> {
    let revisions = blame_lines
        .iter()
        .filter_map(|line| line.revision)
        .collect::<BTreeSet<_>>();
    let (Some(first), Some(last)) = (revisions.first(), revisions.last()) else {
        return HashMap::default();
    };
    let missing = {
        let state = state.lock();
        revisions
            .iter()
            .any(|revision| !state.commits.contains_key(revision))
    };
    if missing {
        let range = format!("{first}:{last}");
        let target = if target.contains('@') {
            target.to_string()
        } else {
            format!("{target}@")
        };
        if let Some(commits) = svn
            .run(["log", "--xml", "-r", range.as_str(), "--", target.as_str()])
            .await
            .and_then(|log| parse_log(&log))
            .log_err()
        {
            state.lock().insert_commits(commits);
        }
    }
    let state = state.lock();
    revisions
        .into_iter()
        .filter_map(|revision| Some((revision, state.commits.get(&revision)?.message.clone())))
        .collect()
}

fn global_ignores() -> GlobSet {
    let patterns = user_global_ignores().unwrap_or_else(|| DEFAULT_GLOBAL_IGNORES.to_string());
    let mut builder = GlobSetBuilder::new();
    for pattern in patterns.split_whitespace() {
        if let Some(glob) = Glob::new(pattern).log_err() {
            builder.add(glob);
        }
    }
    builder.build().log_err().unwrap_or_else(GlobSet::empty)
}

/// Reads `global-ignores` from the `[miscellany]` section of the user's Subversion config.
fn user_global_ignores() -> Option<String> {
    let config_path = if cfg!(windows) {
        PathBuf::from(std::env::var_os("APPDATA")?).join("Subversion/config")
    } else {
        util::paths::home_dir().join(".subversion/config")
    };
    let config = std::fs::read_to_string(config_path).ok()?;
    let mut in_miscellany = false;
    for line in config.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            in_miscellany = line == "[miscellany]";
        } else if in_miscellany
            && let Some((key, value)) = line.split_once('=')
            && key.trim() == "global-ignores"
        {
            return Some(value.trim().to_string());
        }
    }
    None
}

fn path_ancestors(path: &str) -> impl Iterator<Item = &str> {
    path.match_indices('/')
        .map(move |(index, _)| &path[..index])
}

fn has_descendant(paths: &BTreeSet<&str>, path: &str) -> bool {
    let prefix = format!("{path}/");
    paths
        .range(prefix.as_str()..)
        .next()
        .is_some_and(|candidate| candidate.starts_with(&prefix))
}

fn is_directory(working_directory: &Path, paths: &BTreeSet<&str>, path: &str) -> bool {
    std::fs::symlink_metadata(working_directory.join(path)).is_ok_and(|metadata| metadata.is_dir())
        || has_descendant(paths, path)
}

fn git_status_from_entries(
    entries: &[StatusEntry],
    working_directory: &Path,
    global_ignores: &GlobSet,
    prefixes: &[RepoPath],
) -> GitStatus {
    let paths = entries
        .iter()
        .map(|entry| entry.path.as_str())
        .collect::<BTreeSet<_>>();
    let mut statuses = Vec::new();
    for entry in entries {
        let Some(change) = entry.change() else {
            continue;
        };
        if entry.path.is_empty() || entry.path == "." {
            continue;
        }
        if is_directory(working_directory, &paths, &entry.path) {
            // Like `git status --untracked-files=all`, list the files in new directories.
            if change == Change::Untracked {
                collect_unversioned_files(
                    &working_directory.join(&entry.path),
                    &entry.path,
                    global_ignores,
                    &mut statuses,
                );
            }
            continue;
        }
        if let Ok(repo_path) = RepoPath::new(&entry.path) {
            statuses.push((repo_path, file_status(entry, change)));
        }
    }
    statuses.retain(|(path, _)| is_within_any(path.as_unix_str(), prefixes));
    statuses.sort_unstable_by(|(a, _), (b, _)| a.cmp(b));
    statuses.dedup_by(|(a, _), (b, _)| a == b);
    GitStatus {
        entries: statuses.into(),
    }
}

fn collect_unversioned_files(
    directory: &Path,
    relative_directory: &str,
    global_ignores: &GlobSet,
    statuses: &mut Vec<(RepoPath, FileStatus)>,
) {
    let Some(children) = std::fs::read_dir(directory).log_err() else {
        return;
    };
    for child in children.flatten() {
        let file_name = child.file_name();
        let Some(name) = file_name.to_str() else {
            continue;
        };
        if global_ignores.is_match(name) {
            continue;
        }
        let Ok(file_type) = child.file_type() else {
            continue;
        };
        let relative_path = format!("{relative_directory}/{name}");
        if file_type.is_dir() {
            let child_path = child.path();
            // Nested working copies and repositories are separate repositories.
            if child_path.join(DOT_SVN).exists() || child_path.join(DOT_GIT).exists() {
                continue;
            }
            collect_unversioned_files(&child_path, &relative_path, global_ignores, statuses);
        } else if let Ok(repo_path) = RepoPath::new(&relative_path) {
            statuses.push((repo_path, FileStatus::Untracked));
        }
    }
}

fn file_status(entry: &StatusEntry, change: Change) -> FileStatus {
    let code = match change {
        Change::Untracked => return FileStatus::Untracked,
        Change::Conflicted => {
            return UnmergedStatus {
                first_head: UnmergedStatusCode::Updated,
                second_head: UnmergedStatusCode::Updated,
            }
            .into();
        }
        Change::Added => StatusCode::Added,
        Change::Deleted | Change::Missing => StatusCode::Deleted,
        Change::Modified => StatusCode::Modified,
        Change::TypeChanged => StatusCode::TypeChanged,
    };
    if entry.is_excluded_from_commit() {
        TrackedStatus {
            index_status: StatusCode::Unmodified,
            worktree_status: code,
        }
    } else {
        TrackedStatus {
            index_status: code,
            worktree_status: StatusCode::Unmodified,
        }
    }
    .into()
}

#[derive(Debug, Default, PartialEq)]
struct CommitPlan {
    /// Paths to pass to `svn commit --depth empty`.
    targets: Vec<String>,
    /// Missing paths to schedule for deletion before committing.
    to_delete: Vec<String>,
    /// Paths that were added and then deleted from disk, which have nothing to commit.
    to_revert: Vec<String>,
}

fn plan_commit(entries: &[StatusEntry], working_directory: &Path) -> CommitPlan {
    let paths = entries
        .iter()
        .map(|entry| entry.path.as_str())
        .collect::<BTreeSet<_>>();
    let entries_by_path = entries
        .iter()
        .map(|entry| (entry.path.as_str(), entry))
        .collect::<HashMap<_, _>>();
    let is_committable = |entry: &StatusEntry| {
        !entry.is_excluded_from_commit()
            && matches!(
                entry.change(),
                Some(
                    Change::Added
                        | Change::Deleted
                        | Change::Missing
                        | Change::Modified
                        | Change::TypeChanged
                )
            )
    };

    let mut plan = CommitPlan::default();
    let mut targets = BTreeSet::new();
    for entry in entries {
        if entry.item == ItemStatus::Missing && entry.was_added {
            plan.to_revert.push(entry.path.clone());
            continue;
        }
        if !is_committable(entry) {
            continue;
        }
        let path = if entry.path.is_empty() {
            "."
        } else {
            entry.path.as_str()
        };
        // Directories are committed along with the files in them, unless nothing in them
        // changed, as with a new empty directory or a property change.
        if path == "." || is_directory(working_directory, &paths, path) {
            if !has_descendant(&paths, path) {
                targets.insert(path);
            }
            continue;
        }
        targets.insert(path);
        // New and deleted directories have to be committed with the files in them.
        for ancestor in path_ancestors(path) {
            if entries_by_path
                .get(ancestor)
                .is_some_and(|ancestor_entry| is_committable(ancestor_entry))
            {
                targets.insert(ancestor);
            }
        }
    }

    let is_missing = |path: &str| {
        entries_by_path
            .get(path)
            .is_some_and(|entry| entry.item == ItemStatus::Missing)
    };
    // Deleting a missing directory deletes everything in it, so only the topmost one is
    // passed to `svn delete` and `svn commit`.
    let topmost_missing = |path: &str| {
        path_ancestors(path)
            .find(|ancestor| is_missing(ancestor))
            .unwrap_or(path)
            .to_string()
    };
    let mut to_delete = BTreeSet::new();
    for target in &targets {
        if is_missing(target) {
            to_delete.insert(topmost_missing(target));
        }
    }
    plan.targets = targets
        .into_iter()
        .filter(|target| !path_ancestors(target).any(|ancestor| to_delete.contains(ancestor)))
        .map(str::to_string)
        .collect();
    plan.to_delete = to_delete.into_iter().collect();
    plan
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ItemStatus {
    Added,
    Conflicted,
    Deleted,
    External,
    Ignored,
    Incomplete,
    Merged,
    Missing,
    Modified,
    None,
    Normal,
    Obstructed,
    Replaced,
    Unversioned,
}

impl ItemStatus {
    fn parse(value: &str) -> Self {
        match value {
            "added" => Self::Added,
            "conflicted" => Self::Conflicted,
            "deleted" => Self::Deleted,
            "external" => Self::External,
            "ignored" => Self::Ignored,
            "incomplete" => Self::Incomplete,
            "merged" => Self::Merged,
            "missing" => Self::Missing,
            "modified" => Self::Modified,
            "none" => Self::None,
            "obstructed" => Self::Obstructed,
            "replaced" => Self::Replaced,
            "unversioned" => Self::Unversioned,
            _ => Self::Normal,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Change {
    Untracked,
    Conflicted,
    Added,
    Deleted,
    Missing,
    Modified,
    TypeChanged,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct StatusEntry {
    path: String,
    item: ItemStatus,
    props: String,
    tree_conflicted: bool,
    /// Whether the entry was scheduled for addition and so has no base revision.
    was_added: bool,
    changelist: Option<String>,
}

impl StatusEntry {
    fn change(&self) -> Option<Change> {
        if self.tree_conflicted || self.item == ItemStatus::Conflicted || self.props == "conflicted"
        {
            return Some(Change::Conflicted);
        }
        match self.item {
            ItemStatus::Unversioned => Some(Change::Untracked),
            ItemStatus::Added => Some(Change::Added),
            ItemStatus::Deleted => Some(Change::Deleted),
            // An added file that was deleted from disk has nothing left to commit.
            ItemStatus::Missing if self.was_added => None,
            ItemStatus::Missing => Some(Change::Missing),
            ItemStatus::Modified | ItemStatus::Merged | ItemStatus::Replaced => {
                Some(Change::Modified)
            }
            ItemStatus::Obstructed => Some(Change::TypeChanged),
            ItemStatus::Normal | ItemStatus::None if self.props == "modified" => {
                Some(Change::Modified)
            }
            _ => None,
        }
    }

    fn is_excluded_from_commit(&self) -> bool {
        self.changelist.as_deref() == Some(IGNORE_ON_COMMIT_CHANGELIST)
    }
}

fn parse_status(xml: &str) -> Result<Vec<StatusEntry>> {
    let document = parse_xml(xml)?;
    let mut entries = Vec::new();
    for status in document.children_named("status") {
        for group in &status.children {
            let changelist = (group.name == "changelist")
                .then(|| group.attribute("name").map(str::to_string))
                .flatten();
            for entry in group.children_named("entry") {
                let Some(wc_status) = entry.child("wc-status") else {
                    continue;
                };
                let path = entry.attribute("path").unwrap_or_default();
                let path = if cfg!(windows) {
                    path.replace('\\', "/")
                } else {
                    path.to_string()
                };
                let path = if path == "." { String::new() } else { path };
                let item = ItemStatus::parse(wc_status.attribute("item").unwrap_or_default());
                let revision = wc_status.attribute("revision");
                entries.push(StatusEntry {
                    path,
                    item,
                    props: wc_status.attribute("props").unwrap_or_default().to_string(),
                    tree_conflicted: wc_status.attribute("tree-conflicted") == Some("true"),
                    was_added: item == ItemStatus::Added
                        || (wc_status.child("commit").is_none()
                            && revision.is_none_or(|revision| revision == "-1")),
                    changelist: changelist.clone(),
                });
            }
        }
    }
    Ok(entries)
}

#[derive(Clone, Debug, Default, PartialEq)]
struct SvnInfo {
    url: String,
    relative_url: String,
    repository_root: String,
    revision: Option<u64>,
    last_changed: Option<SvnCommit>,
}

#[derive(Clone, Debug, Default, PartialEq)]
struct SvnCommit {
    revision: u64,
    author: String,
    timestamp: i64,
    message: String,
}

fn parse_info(xml: &str) -> Result<SvnInfo> {
    let document = parse_xml(xml)?;
    let entry = document
        .child("info")
        .and_then(|info| info.child("entry"))
        .context("svn info returned no entry")?;
    let text_of = |element: Option<&XmlElement>| {
        element
            .map(|element| element.text.trim().to_string())
            .unwrap_or_default()
    };
    Ok(SvnInfo {
        url: text_of(entry.child("url")),
        relative_url: text_of(entry.child("relative-url")),
        repository_root: text_of(
            entry
                .child("repository")
                .and_then(|repository| repository.child("root")),
        ),
        revision: entry
            .attribute("revision")
            .and_then(|revision| revision.parse().ok()),
        last_changed: entry.child("commit").and_then(parse_commit),
    })
}

fn parse_commit(commit: &XmlElement) -> Option<SvnCommit> {
    Some(SvnCommit {
        revision: commit.attribute("revision")?.parse().ok()?,
        author: commit
            .child("author")
            .map(|author| author.text.trim().to_string())
            .unwrap_or_default(),
        timestamp: commit
            .child("date")
            .and_then(|date| parse_date(&date.text))
            .unwrap_or_default(),
        message: commit
            .child("msg")
            .map(|message| message.text.trim_end().to_string())
            .unwrap_or_default(),
    })
}

fn parse_date(date: &str) -> Option<i64> {
    time::OffsetDateTime::parse(date.trim(), &time::format_description::well_known::Rfc3339)
        .ok()
        .map(|date| date.unix_timestamp())
}

fn parse_log(xml: &str) -> Result<Vec<SvnCommit>> {
    let document = parse_xml(xml)?;
    Ok(document
        .children_named("log")
        .flat_map(|log| log.children_named("logentry"))
        .filter_map(parse_commit)
        .collect())
}

#[derive(Debug, PartialEq)]
struct ChangedPath {
    path: String,
    action: String,
    kind: String,
}

fn parse_changed_paths(xml: &str) -> Result<Vec<ChangedPath>> {
    let document = parse_xml(xml)?;
    Ok(document
        .children_named("log")
        .flat_map(|log| log.children_named("logentry"))
        .flat_map(|entry| entry.children_named("paths"))
        .flat_map(|paths| paths.children_named("path"))
        .map(|path| ChangedPath {
            path: path.text.trim().to_string(),
            action: path.attribute("action").unwrap_or_default().to_string(),
            kind: path.attribute("kind").unwrap_or_default().to_string(),
        })
        .collect())
}

#[derive(Clone, Debug, Default, PartialEq)]
struct BlameLine {
    revision: Option<u64>,
    author: Option<String>,
    timestamp: Option<i64>,
}

fn parse_blame(xml: &str) -> Result<Vec<BlameLine>> {
    let document = parse_xml(xml)?;
    let target = document
        .child("blame")
        .and_then(|blame| blame.child("target"))
        .context("svn blame returned no target")?;
    let mut lines = target
        .children_named("entry")
        .filter_map(|entry| {
            let line_number: usize = entry.attribute("line-number")?.parse().ok()?;
            let commit = entry.child("commit").and_then(parse_commit);
            Some((
                line_number,
                BlameLine {
                    revision: commit.as_ref().map(|commit| commit.revision),
                    author: commit.as_ref().map(|commit| commit.author.clone()),
                    timestamp: commit.map(|commit| commit.timestamp),
                },
            ))
        })
        .collect::<Vec<_>>();
    lines.sort_by_key(|(line_number, _)| *line_number);
    Ok(lines.into_iter().map(|(_, line)| line).collect())
}

/// Builds a [`Blame`] for `current_text` from the blame of `base_text`, treating lines
/// that differ from the base as not committed yet, like `git blame --contents` does.
fn build_blame(
    path: &RepoPath,
    base_lines: &[BlameLine],
    base_text: &str,
    current_text: &str,
    messages: HashMap<u64, String>,
) -> Blame {
    let uncommitted = BlameLine {
        revision: None,
        author: Some(NOT_COMMITTED_AUTHOR.to_string()),
        timestamp: Some(time::OffsetDateTime::now_utc().unix_timestamp()),
    };
    let base_text = base_text.replace("\r\n", "\n");
    let current_text = current_text.replace("\r\n", "\n");
    let current_line_count = current_text.lines().count();
    let mut current_lines = vec![(uncommitted, 0u32); current_line_count];
    let diff = similar::TextDiff::from_lines(&base_text, &current_text);
    for operation in diff.ops() {
        if let similar::DiffOp::Equal {
            old_index,
            new_index,
            len,
        } = *operation
        {
            for offset in 0..len {
                if let (Some(base_line), Some(current_line)) = (
                    base_lines.get(old_index + offset),
                    current_lines.get_mut(new_index + offset),
                ) {
                    *current_line = (base_line.clone(), (old_index + offset + 1) as u32);
                }
            }
        }
    }

    let mut entries: Vec<BlameEntry> = Vec::new();
    for (line_index, (line, original_line_number)) in current_lines.into_iter().enumerate() {
        let line_index = line_index as u32;
        let sha = line
            .revision
            .map(Oid::from_svn_revision)
            .unwrap_or_else(Oid::zero);
        if let Some(last) = entries.last_mut()
            && last.sha == sha
            && last.range.end == line_index
        {
            last.range.end += 1;
            continue;
        }
        entries.push(BlameEntry {
            sha,
            range: line_index..line_index + 1,
            original_line_number,
            author: line.author.clone(),
            author_mail: None,
            author_time: line.timestamp,
            author_tz: Some("+0000".to_string()),
            committer_name: line.author,
            committer_email: None,
            committer_time: line.timestamp,
            committer_tz: Some("+0000".to_string()),
            summary: line
                .revision
                .and_then(|revision| messages.get(&revision))
                .and_then(|message| message.lines().next())
                .map(str::to_string),
            previous: None,
            filename: path.as_unix_str().to_string(),
            boundary: false,
        });
    }

    Blame {
        entries,
        messages: messages
            .into_iter()
            .map(|(revision, message)| (Oid::from_svn_revision(revision), message))
            .collect(),
        tag_names: HashMap::default(),
    }
}

/// Splits `svn diff` output into one section per file, keyed by path.
fn split_diff_by_file(diff: &str) -> Vec<(&str, String)> {
    let mut sections: Vec<(&str, String)> = Vec::new();
    for line in diff.split_inclusive('\n') {
        if let Some(path) = line.strip_prefix("Index: ") {
            sections.push((path.trim_end(), String::new()));
        }
        if let Some((_, section)) = sections.last_mut() {
            section.push_str(line);
        }
    }
    sections
}

/// Counts added and deleted lines per file in `svn diff` output, using the hunk headers
/// so that content lines starting with `---` or `+++` aren't mistaken for headers.
fn diff_stats(diff: &str) -> Vec<(String, DiffStat)> {
    let mut stats: Vec<(String, DiffStat)> = Vec::new();
    let mut old_remaining = 0u32;
    let mut new_remaining = 0u32;
    for line in diff.lines() {
        if old_remaining > 0 || new_remaining > 0 {
            let Some((_, stat)) = stats.last_mut() else {
                break;
            };
            match line.as_bytes().first() {
                Some(b'+') => {
                    stat.added += 1;
                    new_remaining = new_remaining.saturating_sub(1);
                }
                Some(b'-') => {
                    stat.deleted += 1;
                    old_remaining = old_remaining.saturating_sub(1);
                }
                Some(b'\\') => {}
                _ => {
                    old_remaining = old_remaining.saturating_sub(1);
                    new_remaining = new_remaining.saturating_sub(1);
                }
            }
            continue;
        }
        if let Some(path) = line.strip_prefix("Index: ") {
            stats.push((path.trim_end().to_string(), DiffStat::default()));
        } else if let Some(header) = line.strip_prefix("@@ ")
            && let Some((old_range, new_range)) = parse_hunk_header(header)
        {
            old_remaining = old_range;
            new_remaining = new_range;
        }
    }
    stats
}

fn parse_hunk_header(header: &str) -> Option<(u32, u32)> {
    let mut ranges = header.split_whitespace();
    let old = ranges.next()?.strip_prefix('-')?;
    let new = ranges.next()?.strip_prefix('+')?;
    let line_count = |range: &str| match range.split_once(',') {
        Some((_, count)) => count.parse().ok(),
        None => Some(1),
    };
    Some((line_count(old)?, line_count(new)?))
}

#[derive(Debug, Default)]
struct XmlElement {
    name: String,
    attributes: Vec<(String, String)>,
    children: Vec<XmlElement>,
    text: String,
}

impl XmlElement {
    fn attribute(&self, name: &str) -> Option<&str> {
        self.attributes
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.as_str())
    }

    fn child(&self, name: &str) -> Option<&XmlElement> {
        self.children.iter().find(|child| child.name == name)
    }

    fn children_named<'a>(&'a self, name: &'a str) -> impl Iterator<Item = &'a XmlElement> {
        self.children.iter().filter(move |child| child.name == name)
    }
}

/// Parses `svn --xml` output into a tree whose root holds the document's top-level
/// elements as children.
fn parse_xml(xml: &str) -> Result<XmlElement> {
    fn element(start: &BytesStart) -> Result<XmlElement> {
        let mut element = XmlElement {
            name: String::from_utf8_lossy(start.name().as_ref()).into_owned(),
            ..Default::default()
        };
        for attribute in start.attributes() {
            let attribute = attribute?;
            element.attributes.push((
                String::from_utf8_lossy(attribute.key.as_ref()).into_owned(),
                attribute
                    .normalized_value(quick_xml::XmlVersion::Explicit1_0)?
                    .into_owned(),
            ));
        }
        Ok(element)
    }

    let mut reader = quick_xml::Reader::from_str(xml);
    let mut stack = vec![XmlElement::default()];
    loop {
        match reader.read_event()? {
            Event::Start(start) => stack.push(element(&start)?),
            Event::Empty(start) => {
                let element = element(&start)?;
                stack
                    .last_mut()
                    .context("unbalanced XML")?
                    .children
                    .push(element);
            }
            Event::End(_) => {
                anyhow::ensure!(stack.len() > 1, "unbalanced XML");
                let element = stack.pop().context("unbalanced XML")?;
                stack
                    .last_mut()
                    .context("unbalanced XML")?
                    .children
                    .push(element);
            }
            Event::Text(text) => {
                let text = text.xml10_content()?;
                if let Some(element) = stack.last_mut() {
                    element.text.push_str(&text);
                }
            }
            Event::CData(data) => {
                let text = data.decode()?;
                if let Some(element) = stack.last_mut() {
                    element.text.push_str(&text);
                }
            }
            Event::GeneralRef(reference) => {
                let text = match reference.resolve_char_ref()? {
                    Some(character) => character.to_string(),
                    None => {
                        let name = reference.decode()?;
                        quick_xml::escape::resolve_predefined_entity(&name)
                            .with_context(|| format!("unknown XML entity {name}"))?
                            .to_string()
                    }
                };
                if let Some(element) = stack.last_mut() {
                    element.text.push_str(&text);
                }
            }
            Event::Eof => break,
            _ => {}
        }
    }
    anyhow::ensure!(stack.len() == 1, "unbalanced XML");
    stack.pop().context("empty XML document")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::repository::repo_path;
    use gpui::TestAppContext;
    use pretty_assertions::assert_eq;
    use std::fs;

    fn staged(code: StatusCode) -> FileStatus {
        TrackedStatus {
            index_status: code,
            worktree_status: StatusCode::Unmodified,
        }
        .into()
    }

    fn unstaged(code: StatusCode) -> FileStatus {
        TrackedStatus {
            index_status: StatusCode::Unmodified,
            worktree_status: code,
        }
        .into()
    }

    const STATUS_XML: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<status>
<target
   path=".">
<entry
   path="a.txt">
<wc-status props="none" item="modified" revision="2">
<commit revision="2"><author>sam</author><date>2026-10-08T00:50:23.117378Z</date></commit>
</wc-status>
</entry>
<entry
   path="new &amp; improved.txt">
<wc-status item="unversioned" props="none">
</wc-status>
</entry>
<entry
   path="gone.txt">
<wc-status item="missing" revision="2" props="none">
<commit revision="2"><author>sam</author><date>2026-10-08T00:50:23.117378Z</date></commit>
</wc-status>
</entry>
<entry
   path="added-then-removed.txt">
<wc-status props="none" item="missing" revision="-1">
</wc-status>
</entry>
<entry
   path="conflict.txt">
<wc-status item="conflicted" props="none" revision="2">
</wc-status>
</entry>
<entry
   path="props.txt">
<wc-status item="normal" props="modified" revision="2">
</wc-status>
</entry>
</target>
<changelist
   name="ignore-on-commit">
<entry
   path="b.txt">
<wc-status item="modified" props="none" revision="2">
</wc-status>
</entry>
</changelist>
</status>
"#;

    #[test]
    fn test_parse_status() {
        let entries = parse_status(STATUS_XML).unwrap();
        let summary = entries
            .iter()
            .map(|entry| {
                (
                    entry.path.as_str(),
                    entry.change(),
                    entry.is_excluded_from_commit(),
                )
            })
            .collect::<Vec<_>>();
        assert_eq!(
            summary,
            [
                ("a.txt", Some(Change::Modified), false),
                ("new & improved.txt", Some(Change::Untracked), false),
                ("gone.txt", Some(Change::Missing), false),
                ("added-then-removed.txt", None, false),
                ("conflict.txt", Some(Change::Conflicted), false),
                ("props.txt", Some(Change::Modified), false),
                ("b.txt", Some(Change::Modified), true),
            ]
        );

        let directory = tempfile::tempdir().unwrap();
        let status = git_status_from_entries(&entries, directory.path(), &global_ignores(), &[]);
        assert_eq!(
            status.entries.to_vec(),
            [
                (repo_path("a.txt"), staged(StatusCode::Modified)),
                (repo_path("b.txt"), unstaged(StatusCode::Modified)),
                (
                    repo_path("conflict.txt"),
                    UnmergedStatus {
                        first_head: UnmergedStatusCode::Updated,
                        second_head: UnmergedStatusCode::Updated,
                    }
                    .into()
                ),
                (repo_path("gone.txt"), staged(StatusCode::Deleted)),
                (repo_path("new & improved.txt"), FileStatus::Untracked),
                (repo_path("props.txt"), staged(StatusCode::Modified)),
            ]
        );
    }

    #[test]
    fn test_parse_info_and_log() {
        let info = parse_info(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<info>
<entry path="." revision="7" kind="dir">
<url>https://svn.example.com/repo/branches/release-1.2</url>
<relative-url>^/branches/release-1.2</relative-url>
<repository>
<root>https://svn.example.com/repo</root>
<uuid>e1ba859e-f45d-4850-ae01-700e1f2e241d</uuid>
</repository>
<commit revision="5">
<author>sam</author>
<date>2026-10-08T00:50:23.117378Z</date>
</commit>
</entry>
</info>"#,
        )
        .unwrap();
        assert_eq!(info.revision, Some(7));
        assert_eq!(info.repository_root, "https://svn.example.com/repo");
        assert_eq!(branch_name(&info.relative_url), "branches/release-1.2");
        assert_eq!(branch_name("^/"), "root");
        let last_changed = info.last_changed.unwrap();
        assert_eq!(last_changed.revision, 5);
        assert_eq!(last_changed.author, "sam");
        assert_eq!(last_changed.timestamp, 1791420623);

        let log = parse_log(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<log>
<logentry revision="9">
<author>sam</author>
<date>2026-10-08T00:50:23.117378Z</date>
<msg>Fix the &lt;thing&gt; &#8211; finally

Details</msg>
</logentry>
</log>"#,
        )
        .unwrap();
        assert_eq!(log[0].revision, 9);
        assert_eq!(log[0].message, "Fix the <thing> – finally\n\nDetails");
    }

    #[test]
    fn test_parse_revision() {
        assert_eq!(parse_revision("r42"), Some(42));
        assert_eq!(parse_revision("42"), Some(42));
        assert_eq!(
            parse_revision(&Oid::from_svn_revision(42).to_string()),
            Some(42)
        );
        assert_eq!(parse_revision("HEAD"), None);
        assert_eq!(
            parse_committed_revision("Sending        a.txt\nCommitted revision 12.\n"),
            Some(12)
        );
    }

    #[test]
    fn test_diff_stats() {
        let diff = "\
Index: a.txt
===================================================================
--- a.txt\t(revision 2)
+++ a.txt\t(working copy)
@@ -1,3 +1,3 @@
 one
--- a line that starts with dashes
+++ a line that starts with pluses
 three
@@ -10 +10,2 @@
-ten
+TEN
+eleven
\\ No newline at end of file
Index: image.png
===================================================================
Cannot display: file marked as a binary type.
";
        assert_eq!(
            diff_stats(diff),
            [
                (
                    "a.txt".to_string(),
                    DiffStat {
                        added: 3,
                        deleted: 2
                    }
                ),
                ("image.png".to_string(), DiffStat::default()),
            ]
        );
        let sections = split_diff_by_file(diff);
        assert_eq!(sections.len(), 2);
        assert!(sections[1].1.starts_with("Index: image.png\n"));
    }

    #[test]
    fn test_plan_commit() {
        let entry = |path: &str, item: ItemStatus| StatusEntry {
            path: path.to_string(),
            item,
            props: "none".to_string(),
            tree_conflicted: false,
            was_added: item == ItemStatus::Added,
            changelist: None,
        };
        let directory = tempfile::tempdir().unwrap();
        let entries = [
            entry("a.txt", ItemStatus::Modified),
            StatusEntry {
                changelist: Some(IGNORE_ON_COMMIT_CHANGELIST.to_string()),
                ..entry("excluded.txt", ItemStatus::Modified)
            },
            entry("new", ItemStatus::Added),
            entry("new/file.txt", ItemStatus::Added),
            entry("gone", ItemStatus::Missing),
            entry("gone/one.txt", ItemStatus::Missing),
            entry("gone/two.txt", ItemStatus::Missing),
            entry("untracked.txt", ItemStatus::Unversioned),
            StatusEntry {
                was_added: true,
                ..entry("added-then-removed.txt", ItemStatus::Missing)
            },
        ];
        assert_eq!(
            plan_commit(&entries, directory.path()),
            CommitPlan {
                targets: vec![
                    "a.txt".to_string(),
                    "gone".to_string(),
                    "new".to_string(),
                    "new/file.txt".to_string(),
                ],
                to_delete: vec!["gone".to_string()],
                to_revert: vec!["added-then-removed.txt".to_string()],
            }
        );
    }

    #[test]
    fn test_build_blame() {
        let line = |revision: u64| BlameLine {
            revision: Some(revision),
            author: Some("sam".to_string()),
            timestamp: Some(1),
        };
        let blame = build_blame(
            &repo_path("a.txt"),
            &[line(1), line(2), line(1)],
            "one\ntwo\nthree\n",
            "one\ninserted\ntwo\nthree\n",
            HashMap::from_iter([(2, "Second commit\n\nDetails".to_string())]),
        );
        let entries = blame
            .entries
            .iter()
            .map(|entry| {
                (
                    entry.sha.display_short(),
                    entry.range.clone(),
                    entry.summary.clone(),
                )
            })
            .collect::<Vec<_>>();
        assert_eq!(
            entries,
            [
                ("r1".to_string(), 0..1, None),
                (Oid::zero().display_short(), 1..2, None),
                ("r2".to_string(), 2..3, Some("Second commit".to_string())),
                ("r1".to_string(), 3..4, None),
            ]
        );
    }

    #[allow(clippy::disallowed_methods)]
    #[track_caller]
    fn run(directory: &Path, program: &str, args: &[&str]) -> String {
        let output = std::process::Command::new(program)
            .current_dir(directory)
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{program} {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).into_owned()
    }

    fn svn_available() -> bool {
        let available = which::which("svn").is_ok() && which::which("svnadmin").is_ok();
        if !available {
            eprintln!("skipping test because svn isn't installed");
        }
        available
    }

    fn write_file(root: &Path, path: &str, contents: &str) {
        let path = root.join(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, contents).unwrap();
    }

    /// Creates a repository whose trunk contains `files`, and returns its URL along
    /// with a checkout of it.
    fn create_working_copy(root: &Path, files: &[(&str, &str)]) -> (String, PathBuf) {
        let repository = root.join("repository");
        run(root, "svnadmin", &["create", repository.to_str().unwrap()]);
        let url = format!(
            "file://{}/trunk",
            repository.to_str().unwrap().replace('\\', "/")
        );
        run(root, "svn", &["mkdir", "-q", "-m", "Create trunk", &url]);
        let working_copy = root.join("working-copy");
        run(
            root,
            "svn",
            &["checkout", "-q", &url, working_copy.to_str().unwrap()],
        );
        for (path, contents) in files {
            write_file(&working_copy, path, contents);
        }
        run(&working_copy, "svn", &["add", "-q", "--force", "."]);
        run(
            &working_copy,
            "svn",
            &["commit", "-q", "-m", "Initial commit"],
        );
        run(&working_copy, "svn", &["update", "-q"]);
        (url, working_copy)
    }

    fn open(working_copy: &Path, cx: &TestAppContext) -> SvnRepository {
        SvnRepository::new(&working_copy.join(DOT_SVN), None, cx.executor()).unwrap()
    }

    async fn statuses(
        repository: &SvnRepository,
        prefixes: &[RepoPath],
    ) -> Vec<(String, FileStatus)> {
        repository
            .status(prefixes)
            .await
            .unwrap()
            .entries
            .iter()
            .map(|(path, status)| (path.as_unix_str().to_string(), *status))
            .collect()
    }

    fn askpass(cx: &mut TestAppContext) -> AskPassDelegate {
        AskPassDelegate::new(&mut cx.to_async(), |_, _, _| {})
    }

    #[gpui::test]
    async fn test_status_stage_and_commit(cx: &mut TestAppContext) {
        if !svn_available() {
            return;
        }
        cx.executor().allow_parking();
        let root = tempfile::tempdir().unwrap();
        let (_, working_copy) = create_working_copy(
            root.path(),
            &[
                ("a.txt", "one\ntwo\n"),
                ("b.txt", "bee\n"),
                ("gone.txt", "gone\n"),
                ("dir/c.txt", "sea\n"),
                ("deleted-dir/d.txt", "dee\n"),
            ],
        );
        let repository = open(&working_copy, cx);
        let env = Arc::new(HashMap::default());

        write_file(&working_copy, "a.txt", "one\ntwo\nthree\n");
        write_file(&working_copy, "b.txt", "BEE\n");
        write_file(&working_copy, "new.txt", "new\n");
        write_file(&working_copy, "new@2x.txt", "at sign\n");
        write_file(&working_copy, "new-dir/nested/n.txt", "nested\n");
        write_file(&working_copy, "new-dir/.DS_Store", "ignored\n");
        fs::remove_file(working_copy.join("gone.txt")).unwrap();
        fs::remove_dir_all(working_copy.join("deleted-dir")).unwrap();

        assert_eq!(
            statuses(&repository, &[]).await,
            [
                ("a.txt".to_string(), staged(StatusCode::Modified)),
                ("b.txt".to_string(), staged(StatusCode::Modified)),
                ("deleted-dir/d.txt".to_string(), staged(StatusCode::Deleted)),
                ("gone.txt".to_string(), staged(StatusCode::Deleted)),
                ("new-dir/nested/n.txt".to_string(), FileStatus::Untracked),
                ("new.txt".to_string(), FileStatus::Untracked),
                ("new@2x.txt".to_string(), FileStatus::Untracked),
            ]
        );
        // A path inside an unversioned directory can be refreshed on its own.
        assert_eq!(
            statuses(&repository, &[repo_path("new-dir/nested/n.txt")]).await,
            [("new-dir/nested/n.txt".to_string(), FileStatus::Untracked)]
        );
        assert_eq!(
            statuses(&repository, &[repo_path("a.txt")]).await,
            [("a.txt".to_string(), staged(StatusCode::Modified))]
        );

        repository
            .stage_paths(
                vec![
                    repo_path("new.txt"),
                    repo_path("new@2x.txt"),
                    repo_path("new-dir/nested/n.txt"),
                ],
                env.clone(),
            )
            .await
            .unwrap();
        repository
            .unstage_paths(vec![repo_path("b.txt")], env.clone())
            .await
            .unwrap();
        assert_eq!(
            statuses(&repository, &[]).await,
            [
                ("a.txt".to_string(), staged(StatusCode::Modified)),
                ("b.txt".to_string(), unstaged(StatusCode::Modified)),
                ("deleted-dir/d.txt".to_string(), staged(StatusCode::Deleted)),
                ("gone.txt".to_string(), staged(StatusCode::Deleted)),
                (
                    "new-dir/nested/n.txt".to_string(),
                    staged(StatusCode::Added)
                ),
                ("new.txt".to_string(), staged(StatusCode::Added)),
                ("new@2x.txt".to_string(), staged(StatusCode::Added)),
            ]
        );

        // The diff base is the checked-out text, regardless of what's staged.
        assert_eq!(
            repository
                .load_revisions(vec![
                    "HEAD:a.txt".into(),
                    ":a.txt".into(),
                    ":new.txt".into()
                ])
                .await
                .unwrap(),
            [
                Some(b"one\ntwo\n".to_vec()),
                Some(b"one\ntwo\n".to_vec()),
                None
            ]
        );

        let diff_stats = |diff_type| {
            let future = repository.diff_stat(diff_type, &[]);
            async move {
                future
                    .await
                    .unwrap()
                    .entries
                    .iter()
                    .map(|(path, stat)| (path.as_unix_str().to_string(), stat.added, stat.deleted))
                    .collect::<Vec<_>>()
            }
        };
        assert_eq!(
            diff_stats(DiffStatType::IndexToWorktree).await,
            [("b.txt".to_string(), 1, 1)]
        );
        assert!(
            diff_stats(DiffStatType::HeadToIndex)
                .await
                .contains(&("a.txt".to_string(), 1, 0))
        );

        repository
            .commit(
                "Commit – with unicode".into(),
                None,
                CommitOptions::default(),
                askpass(cx),
                env.clone(),
            )
            .await
            .unwrap();

        // Only the unstaged file is left.
        assert_eq!(
            statuses(&repository, &[]).await,
            [("b.txt".to_string(), unstaged(StatusCode::Modified))]
        );
        // XML output is always UTF-8, whatever the test's locale is.
        let log = run(
            &working_copy,
            "svn",
            &["log", "--xml", "-v", "-l", "1", "^/"],
        );
        assert!(log.contains("Commit – with unicode"), "{log}");
        for path in [
            "/trunk/a.txt",
            "/trunk/gone.txt",
            "/trunk/deleted-dir",
            "/trunk/new.txt",
            "/trunk/new@2x.txt",
            "/trunk/new-dir/nested/n.txt",
        ] {
            assert!(log.contains(path), "{path} missing from {log}");
        }
        assert!(!log.contains("/trunk/b.txt"), "{log}");
        assert!(!log.contains(".DS_Store"), "{log}");

        let head = repository.show("HEAD".into()).await.unwrap();
        assert_eq!(head.message.as_ref(), "Commit – with unicode");
        assert_eq!(
            head.sha.parse::<Oid>().unwrap().display_short(),
            "r3".to_string()
        );

        // Discarding changes restores the file and takes it out of the changelist.
        repository
            .checkout_files("HEAD".into(), vec![repo_path("b.txt")], env.clone())
            .await
            .unwrap();
        assert_eq!(
            fs::read_to_string(working_copy.join("b.txt")).unwrap(),
            "bee\n"
        );
        write_file(&working_copy, "b.txt", "changed again\n");
        assert_eq!(
            statuses(&repository, &[]).await,
            [("b.txt".to_string(), staged(StatusCode::Modified))]
        );
    }

    #[gpui::test]
    async fn test_move_and_blame(cx: &mut TestAppContext) {
        if !svn_available() {
            return;
        }
        cx.executor().allow_parking();
        let root = tempfile::tempdir().unwrap();
        let (_, working_copy) =
            create_working_copy(root.path(), &[("src/lib.rs", "one\ntwo\nthree\n")]);
        write_file(&working_copy, "src/lib.rs", "one\nTWO\nthree\n");
        run(&working_copy, "svn", &["commit", "-q", "-m", "Shout two"]);
        run(&working_copy, "svn", &["update", "-q"]);
        let repository = open(&working_copy, cx);

        assert!(
            move_versioned_path(
                &working_copy.join("src/lib.rs"),
                &working_copy.join("renamed/main.rs"),
            )
            .await
            .unwrap()
        );
        assert!(working_copy.join("renamed/main.rs").exists());
        assert_eq!(
            statuses(&repository, &[]).await,
            [
                ("renamed/main.rs".to_string(), staged(StatusCode::Added)),
                ("src/lib.rs".to_string(), staged(StatusCode::Deleted)),
            ]
        );
        // The moved file's diff base is the file it was moved from.
        assert_eq!(
            repository
                .load_revisions(vec!["HEAD:renamed/main.rs".into()])
                .await
                .unwrap(),
            [Some(b"one\nTWO\nthree\n".to_vec())]
        );
        // Unversioned files are left alone.
        write_file(&working_copy, "scratch.txt", "scratch\n");
        assert!(
            !move_versioned_path(
                &working_copy.join("scratch.txt"),
                &working_copy.join("scratch2.txt"),
            )
            .await
            .unwrap()
        );

        repository
            .commit(
                "Move lib.rs".into(),
                None,
                CommitOptions::default(),
                askpass(cx),
                Arc::new(HashMap::default()),
            )
            .await
            .unwrap();
        let log = run(&working_copy, "svn", &["log", "-v", "-l", "1", "^/"]);
        assert!(
            log.contains("/trunk/renamed/main.rs (from /trunk/src/lib.rs"),
            "{log}"
        );
        run(&working_copy, "svn", &["update", "-q"]);

        let blame = repository
            .blame(
                repo_path("renamed/main.rs"),
                Rope::from("one\nTWO\nlocal edit\nthree\n"),
                LineEnding::Unix,
            )
            .await
            .unwrap();
        let entries = blame
            .entries
            .iter()
            .map(|entry| {
                (
                    entry.sha.display_short(),
                    entry.range.clone(),
                    entry.summary.clone(),
                )
            })
            .collect::<Vec<_>>();
        assert_eq!(
            entries,
            [
                ("r2".to_string(), 0..1, Some("Initial commit".to_string())),
                ("r3".to_string(), 1..2, Some("Shout two".to_string())),
                (Oid::zero().display_short(), 2..3, None),
                ("r2".to_string(), 3..4, Some("Initial commit".to_string())),
            ]
        );
    }

    #[gpui::test]
    async fn test_move_directory(cx: &mut TestAppContext) {
        if !svn_available() {
            return;
        }
        cx.executor().allow_parking();
        let root = tempfile::tempdir().unwrap();
        let (_, working_copy) = create_working_copy(
            root.path(),
            &[("Services/A.cs", "a\n"), ("Services/B.cs", "b\n")],
        );
        let repository = open(&working_copy, cx);
        write_file(&working_copy, "Services/A.cs", "a changed\n");
        write_file(&working_copy, "Services/Scratch.cs", "scratch\n");

        assert!(
            move_versioned_path(
                &working_copy.join("Services"),
                &working_copy.join("Core/Services"),
            )
            .await
            .unwrap()
        );
        assert!(!working_copy.join("Services").exists());
        assert_eq!(
            fs::read_to_string(working_copy.join("Core/Services/A.cs")).unwrap(),
            "a changed\n"
        );
        assert_eq!(
            statuses(&repository, &[]).await,
            [
                (
                    "Core/Services/A.cs".to_string(),
                    staged(StatusCode::Modified)
                ),
                (
                    "Core/Services/Scratch.cs".to_string(),
                    FileStatus::Untracked
                ),
                ("Services".to_string(), staged(StatusCode::Deleted)),
            ]
        );

        repository
            .commit(
                "Move Services".into(),
                None,
                CommitOptions::default(),
                askpass(cx),
                Arc::new(HashMap::default()),
            )
            .await
            .unwrap();
        assert_eq!(
            statuses(&repository, &[]).await,
            [(
                "Core/Services/Scratch.cs".to_string(),
                FileStatus::Untracked
            )]
        );
        let listing = run(&working_copy, "svn", &["list", "-R", "^/trunk"]);
        assert_eq!(
            listing.lines().collect::<Vec<_>>(),
            [
                "Core/",
                "Core/Services/",
                "Core/Services/A.cs",
                "Core/Services/B.cs"
            ]
        );
    }

    #[gpui::test]
    async fn test_history(cx: &mut TestAppContext) {
        if !svn_available() {
            return;
        }
        cx.executor().allow_parking();
        let root = tempfile::tempdir().unwrap();
        let (_, working_copy) = create_working_copy(root.path(), &[("a.txt", "one\n")]);
        write_file(&working_copy, "a.txt", "two\n");
        run(
            &working_copy,
            "svn",
            &["commit", "-q", "-m", "Change a\n\nDetails"],
        );
        write_file(&working_copy, "b.txt", "bee\n");
        run(&working_copy, "svn", &["add", "-q", "b.txt"]);
        run(&working_copy, "svn", &["commit", "-q", "-m", "Add b"]);
        run(&working_copy, "svn", &["update", "-q"]);
        let repository = open(&working_copy, cx);

        async fn history(
            repository: &SvnRepository,
            log_source: LogSource,
        ) -> Vec<(String, Vec<String>)> {
            let (request_tx, request_rx) = async_channel::unbounded();
            repository
                .initial_graph_data(log_source, LogOrder::default(), request_tx)
                .await
                .unwrap();
            let mut commits = Vec::new();
            while let Ok(batch) = request_rx.try_recv() {
                for commit in batch {
                    commits.push((
                        commit.sha.display_short(),
                        commit
                            .parents
                            .iter()
                            .map(|parent| parent.display_short())
                            .collect(),
                    ));
                }
            }
            commits
        }
        let commit = |revision: &str, parent: Option<&str>| {
            (
                revision.to_string(),
                parent.map(str::to_string).into_iter().collect::<Vec<_>>(),
            )
        };

        assert_eq!(
            history(&repository, LogSource::Branch("trunk".into())).await,
            [
                commit("r4", Some("r3")),
                commit("r3", Some("r2")),
                commit("r2", Some("r1")),
                commit("r1", None),
            ]
        );
        assert_eq!(
            history(&repository, LogSource::Path(repo_path("a.txt"))).await,
            [commit("r3", Some("r2")), commit("r2", None)]
        );

        // Pages are stitched together.
        let (request_tx, request_rx) = async_channel::unbounded();
        load_history(
            &repository.svn,
            &repository.state,
            ".".into(),
            "BASE".into(),
            2,
            &request_tx,
        )
        .await
        .unwrap();
        let mut revisions = Vec::new();
        while let Ok(batch) = request_rx.try_recv() {
            revisions.extend(batch.iter().map(|commit| {
                (
                    commit.sha.svn_revision().unwrap(),
                    commit
                        .parents
                        .first()
                        .and_then(|parent| parent.svn_revision()),
                )
            }));
        }
        assert_eq!(
            revisions,
            [(4, Some(3)), (3, Some(2)), (2, Some(1)), (1, None)]
        );

        let reader = repository.commit_data_reader().unwrap();
        let data = reader.read(Oid::from_svn_revision(3)).await.unwrap();
        assert_eq!(data.subject.as_ref(), "Change a");
        assert_eq!(data.message.as_ref(), "Change a\n\nDetails");
        assert_eq!(
            data.parents
                .first()
                .and_then(|parent| parent.svn_revision()),
            Some(2)
        );

        let commit_diff = repository
            .load_commit(Oid::from_svn_revision(3).to_string(), false, cx.to_async())
            .await
            .unwrap();
        assert_eq!(commit_diff.files.len(), 1);
        assert_eq!(commit_diff.files[0].path, repo_path("a.txt"));
        assert_eq!(
            commit_diff.files[0].old_content.as_deref(),
            Some(&b"one\n"[..])
        );
        assert_eq!(
            commit_diff.files[0].new_content.as_deref(),
            Some(&b"two\n"[..])
        );
    }

    #[gpui::test]
    async fn test_update_conflict(cx: &mut TestAppContext) {
        if !svn_available() {
            return;
        }
        cx.executor().allow_parking();
        let root = tempfile::tempdir().unwrap();
        let (url, working_copy) =
            create_working_copy(root.path(), &[("dir with space/naïve.txt", "one\ntwo\n")]);
        let repository = open(&working_copy, cx);
        let env = Arc::new(HashMap::default());

        let other_working_copy = root.path().join("other");
        run(
            root.path(),
            "svn",
            &["checkout", "-q", &url, other_working_copy.to_str().unwrap()],
        );
        write_file(
            &other_working_copy,
            "dir with space/naïve.txt",
            "one\ntheirs\n",
        );
        run(
            &other_working_copy,
            "svn",
            &["commit", "-q", "-m", "Theirs"],
        );

        write_file(&working_copy, "dir with space/naïve.txt", "one\nmine\n");
        repository
            .pull(
                None,
                REMOTE_NAME.into(),
                false,
                askpass(cx),
                env.clone(),
                cx.to_async(),
            )
            .await
            .unwrap();
        let conflicted = fs::read_to_string(working_copy.join("dir with space/naïve.txt")).unwrap();
        assert!(conflicted.contains("<<<<<<< .mine"), "{conflicted}");
        let statuses_after_update = statuses(&repository, &[]).await;
        assert!(
            statuses_after_update.contains(&(
                "dir with space/naïve.txt".to_string(),
                UnmergedStatus {
                    first_head: UnmergedStatusCode::Updated,
                    second_head: UnmergedStatusCode::Updated,
                }
                .into()
            )),
            "{statuses_after_update:?}"
        );

        // Staging a conflicted file marks it as resolved.
        write_file(&working_copy, "dir with space/naïve.txt", "one\nmerged\n");
        repository
            .stage_paths(vec![repo_path("dir with space/naïve.txt")], env.clone())
            .await
            .unwrap();
        assert_eq!(
            statuses(&repository, &[]).await,
            [(
                "dir with space/naïve.txt".to_string(),
                staged(StatusCode::Modified)
            )]
        );
        repository
            .commit(
                "Merge".into(),
                None,
                CommitOptions::default(),
                askpass(cx),
                env,
            )
            .await
            .unwrap();
        assert_eq!(statuses(&repository, &[]).await, []);
    }

    async fn behind(repository: &SvnRepository) -> u32 {
        let branches = repository.branches().await.unwrap().branches;
        assert_eq!(branches.len(), 1);
        assert_eq!(branches[0].name(), "trunk");
        match branches[0].upstream.as_ref().unwrap().tracking {
            UpstreamTracking::Tracked(status) => status.behind,
            UpstreamTracking::Gone => panic!("upstream is gone"),
        }
    }

    #[gpui::test]
    async fn test_fetch_and_pull(cx: &mut TestAppContext) {
        if !svn_available() {
            return;
        }
        cx.executor().allow_parking();
        let root = tempfile::tempdir().unwrap();
        let (url, working_copy) = create_working_copy(root.path(), &[("a.txt", "one\n")]);
        let repository = open(&working_copy, cx);
        let env = Arc::new(HashMap::default());

        let other_working_copy = root.path().join("other");
        run(
            root.path(),
            "svn",
            &["checkout", "-q", &url, other_working_copy.to_str().unwrap()],
        );
        write_file(&other_working_copy, "a.txt", "one\ntwo\n");
        run(
            &other_working_copy,
            "svn",
            &["commit", "-q", "-m", "From elsewhere"],
        );

        // Committing from this working copy doesn't count as an incoming change.
        write_file(&working_copy, "b.txt", "bee\n");
        repository
            .stage_paths(vec![repo_path("b.txt")], env.clone())
            .await
            .unwrap();
        repository
            .commit(
                "Add b".into(),
                None,
                CommitOptions::default(),
                askpass(cx),
                env.clone(),
            )
            .await
            .unwrap();

        assert_eq!(behind(&repository).await, 0);
        let output = repository
            .fetch(FetchOptions::All, askpass(cx), env.clone(), cx.to_async())
            .await
            .unwrap();
        assert_eq!(output.stdout, "1 new revision on the server.");
        assert_eq!(behind(&repository).await, 1);

        repository
            .pull(
                None,
                REMOTE_NAME.into(),
                false,
                askpass(cx),
                env.clone(),
                cx.to_async(),
            )
            .await
            .unwrap();
        assert_eq!(behind(&repository).await, 0);
        assert_eq!(
            fs::read_to_string(working_copy.join("a.txt")).unwrap(),
            "one\ntwo\n"
        );
    }
}

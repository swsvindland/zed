use crate::init_test;
use fs::RealFs;
use futures::StreamExt as _;
use git::Oid;
use git::repository::{AskPassDelegate, CommitOptions, LogOrder, LogSource, repo_path};
use git::status::{FileStatus, StatusCode, TrackedStatus};
use gpui::{Entity, TestAppContext};
use project::{
    Project, ProjectPath,
    git_store::{CommitDataState, Repository, RepositoryEvent},
};
use std::{
    path::{Path, PathBuf},
    time::Duration,
};
use util::rel_path::rel_path;
use worktree::WorktreeModelHandle as _;

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
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, contents).unwrap();
}

fn create_working_copy(root: &Path, files: &[(&str, &str)]) -> PathBuf {
    let repository = root.join("repository");
    run(root, "svnadmin", &["create", repository.to_str().unwrap()]);
    let url = format!("file://{}/trunk", repository.to_str().unwrap());
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
    // Canonicalize, since temporary directories are behind a symlink on macOS.
    std::fs::canonicalize(working_copy).unwrap()
}

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

fn statuses(repository: &Entity<Repository>, cx: &TestAppContext) -> Vec<(String, FileStatus)> {
    repository.read_with(cx, |repository, _| {
        repository
            .cached_status()
            .map(|entry| (entry.repo_path.as_unix_str().to_string(), entry.status))
            .collect()
    })
}

/// Waits for the repository's statuses to change to `expected` in response to file
/// system events.
async fn wait_for_statuses(
    repository: &Entity<Repository>,
    expected: &[(&str, FileStatus)],
    cx: &mut TestAppContext,
) {
    let expected = expected
        .iter()
        .map(|(path, status)| (path.to_string(), *status))
        .collect::<Vec<_>>();
    let mut events = cx.events::<RepositoryEvent, _>(repository);
    let timeout = futures::FutureExt::fuse(cx.background_executor.timer(Duration::from_secs(10)));
    futures::pin_mut!(timeout);
    loop {
        cx.executor().run_until_parked();
        let actual = statuses(repository, cx);
        if actual == expected {
            return;
        }
        futures::select_biased! {
            _ = events.next() => {}
            _ = timeout => panic!("timed out waiting for statuses {expected:?}, got {actual:?}"),
        }
    }
}

async fn open_project(
    path: &Path,
    cx: &mut TestAppContext,
) -> (Entity<Project>, Entity<Repository>) {
    let project = Project::test(RealFs::new(None, cx.executor()), [path], cx).await;
    let tree = project.read_with(cx, |project, cx| project.worktrees(cx).next().unwrap());
    tree.flush_fs_events(cx).await;
    project
        .update(cx, |project, cx| project.git_scans_complete(cx))
        .await;
    cx.executor().run_until_parked();
    let repository = project.read_with(cx, |project, cx| {
        let repositories = project.repositories(cx);
        assert_eq!(repositories.len(), 1, "expected one repository");
        repositories.values().next().unwrap().clone()
    });
    (project, repository)
}

#[gpui::test]
async fn test_svn_working_copy(cx: &mut TestAppContext) {
    if !svn_available() {
        return;
    }
    init_test(cx);
    cx.executor().allow_parking();
    let root = tempfile::tempdir().unwrap();
    let working_copy = create_working_copy(
        root.path(),
        &[
            ("src/main.rs", "fn main() {\n    println!(\"hi\");\n}\n"),
            ("README.md", "# Readme\n"),
        ],
    );
    write_file(
        &working_copy,
        "src/main.rs",
        "fn main() {\n    println!(\"hello\");\n}\n",
    );
    write_file(&working_copy, "src/new.rs", "// new\n");

    let (project, repository) = open_project(&working_copy, cx).await;
    repository.read_with(cx, |repository, _| {
        assert_eq!(
            repository.work_directory_abs_path.as_ref(),
            working_copy.as_path()
        );
        assert_eq!(
            repository.branch.as_ref().map(|branch| branch.name()),
            Some("trunk")
        );
        assert!(repository.head_commit.is_some());
    });
    wait_for_statuses(
        &repository,
        &[
            ("src/main.rs", staged(StatusCode::Modified)),
            ("src/new.rs", FileStatus::Untracked),
        ],
        cx,
    )
    .await;

    // The diff gutter compares against the checked-out revision.
    let buffer = project
        .update(cx, |project, cx| {
            project.open_local_buffer(working_copy.join("src/main.rs"), cx)
        })
        .await
        .unwrap();
    let diff = project
        .update(cx, |project, cx| {
            project.open_uncommitted_diff(buffer.clone(), cx)
        })
        .await
        .unwrap();
    cx.executor().run_until_parked();
    let hunk_count = diff.read_with(cx, |diff, cx| {
        let snapshot = buffer.read(cx).snapshot();
        diff.snapshot(cx).hunks(&snapshot).count()
    });
    assert_eq!(hunk_count, 1);

    // Staging an unversioned file adds it, and unstaging puts a file in the
    // `ignore-on-commit` changelist. Both are picked up through `.svn/wc.db` changes.
    repository
        .update(cx, |repository, cx| {
            repository.stage_entries(vec![repo_path("src/new.rs")], cx)
        })
        .await
        .unwrap();
    repository
        .update(cx, |repository, cx| {
            repository.unstage_entries(vec![repo_path("src/main.rs")], cx)
        })
        .await
        .unwrap();
    wait_for_statuses(
        &repository,
        &[
            ("src/main.rs", unstaged(StatusCode::Modified)),
            ("src/new.rs", staged(StatusCode::Added)),
        ],
        cx,
    )
    .await;
    let changelist = run(
        &working_copy,
        "svn",
        &["status", "--changelist", "ignore-on-commit"],
    );
    assert!(changelist.contains("src/main.rs"), "{changelist}");

    let askpass = AskPassDelegate::new(&mut cx.to_async(), |_, _, _| {});
    repository
        .update(cx, |repository, cx| {
            repository.commit(
                "Add new.rs".into(),
                None,
                CommitOptions::default(),
                askpass,
                cx,
            )
        })
        .await
        .unwrap()
        .unwrap();
    wait_for_statuses(
        &repository,
        &[("src/main.rs", unstaged(StatusCode::Modified))],
        cx,
    )
    .await;
    let log = run(&working_copy, "svn", &["log", "-v", "-l", "1", "^/"]);
    assert!(log.contains("A /trunk/src/new.rs"), "{log}");
    assert!(!log.contains("/trunk/src/main.rs"), "{log}");

    // History comes from `svn log`.
    let timeout = cx.background_executor.timer(Duration::from_secs(10));
    let history_loaded = async {
        loop {
            let (revisions, is_loading, error) = repository.update(cx, |repository, cx| {
                let response = repository.graph_data(
                    LogSource::Branch("trunk".into()),
                    LogOrder::DateOrder,
                    0..usize::MAX,
                    cx,
                );
                (
                    response
                        .commits
                        .iter()
                        .map(|commit| commit.sha.display_short())
                        .collect::<Vec<_>>(),
                    response.is_loading,
                    response.error,
                )
            });
            assert_eq!(error, None);
            if !is_loading {
                break revisions;
            }
            cx.background_executor
                .timer(Duration::from_millis(50))
                .await;
            cx.executor().run_until_parked();
        }
    };
    let revisions = futures::select_biased! {
        revisions = futures::FutureExt::fuse(history_loaded) => revisions,
        _ = futures::FutureExt::fuse(timeout) => panic!("timed out loading history"),
    };
    assert_eq!(revisions, ["r3", "r2", "r1"]);
    let commit_data = repository.update(cx, |repository, cx| {
        match repository.fetch_commit_data(Oid::from_svn_revision(3), true, cx) {
            CommitDataState::Loading(Some(receiver)) => receiver.clone(),
            _ => panic!("expected commit data to load"),
        }
    });
    let commit_data = commit_data.await.unwrap();
    assert_eq!(commit_data.subject.as_ref(), "Add new.rs");

    // Renaming a versioned file in the project records an `svn move`.
    let entry_id = project.read_with(cx, |project, cx| {
        project
            .entry_for_path(
                &ProjectPath {
                    worktree_id: project.worktrees(cx).next().unwrap().read(cx).id(),
                    path: rel_path("README.md").into(),
                },
                cx,
            )
            .unwrap()
            .id
    });
    let worktree_id = project.read_with(cx, |project, cx| {
        project.worktrees(cx).next().unwrap().read(cx).id()
    });
    project
        .update(cx, |project, cx| {
            project.rename_entry(
                entry_id,
                ProjectPath {
                    worktree_id,
                    path: rel_path("docs/README.md").into(),
                },
                cx,
            )
        })
        .await
        .unwrap();
    wait_for_statuses(
        &repository,
        &[
            ("README.md", staged(StatusCode::Deleted)),
            ("docs/README.md", staged(StatusCode::Added)),
            ("src/main.rs", unstaged(StatusCode::Modified)),
        ],
        cx,
    )
    .await;
    let status = run(&working_copy, "svn", &["status"]);
    assert!(status.contains("A  +    docs/README.md"), "{status}");
}

#[gpui::test]
async fn test_svn_working_copy_opened_from_subdirectory(cx: &mut TestAppContext) {
    if !svn_available() {
        return;
    }
    init_test(cx);
    cx.executor().allow_parking();
    let root = tempfile::tempdir().unwrap();
    let working_copy = create_working_copy(
        root.path(),
        &[
            ("app/src/lib.rs", "pub fn f() {}\n"),
            ("other.txt", "other\n"),
        ],
    );
    write_file(&working_copy, "app/src/lib.rs", "pub fn g() {}\n");
    write_file(&working_copy, "other.txt", "changed\n");

    let (_project, repository) = open_project(&working_copy.join("app"), cx).await;
    repository.read_with(cx, |repository, _| {
        assert_eq!(
            repository.work_directory_abs_path.as_ref(),
            working_copy.as_path()
        );
    });
    wait_for_statuses(
        &repository,
        &[
            ("app/src/lib.rs", staged(StatusCode::Modified)),
            ("other.txt", staged(StatusCode::Modified)),
        ],
        cx,
    )
    .await;

    // Changes made outside of Zed are picked up too.
    run(&working_copy, "svn", &["revert", "other.txt"]);
    wait_for_statuses(
        &repository,
        &[("app/src/lib.rs", staged(StatusCode::Modified))],
        cx,
    )
    .await;
}

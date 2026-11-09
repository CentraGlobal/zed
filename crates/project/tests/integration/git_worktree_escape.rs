use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

use git::repository::CreateWorktreeTarget;
use gpui::{BorrowAppContext, Entity, TestAppContext};
use project::{
    Project, RealFs, git_store::Repository, project_settings::ProjectSettings,
    worktrees_directory_for_repo,
};
use settings::Settings as _;
use util::{paths::PathStyle, test::TempTree};
use worktree::WorktreeModelHandle as _;

const PAYLOAD: &str = "payload-marker\n";
const TOCTOU_TEST: &str =
    "git_worktree_escape::guest_worktree_symlink_swap_before_mkdir_does_not_escape";

fn init(cx: &mut TestAppContext) {
    super::init_test(cx);
    cx.update(|cx| {
        ProjectSettings::register(cx);
    });
    cx.executor().allow_parking();
}

struct HostRepo {
    _root: TempTree,
    repo: PathBuf,
}

fn host_repo() -> HostRepo {
    let root = TempTree::new(serde_json::json!({
        "repo": {
            ".git": {},
            "payload.txt": PAYLOAD,
        }
    }));
    let repo = root.path().join("repo");
    git_commit(&repo);
    HostRepo { _root: root, repo }
}

#[allow(clippy::disallowed_methods)]
fn git(repo: &Path, args: &[&str]) {
    let output = Command::new("git")
        .args(args)
        .current_dir(repo)
        .env("GIT_CONFIG_GLOBAL", "")
        .env("GIT_CONFIG_SYSTEM", "")
        .env("GIT_AUTHOR_NAME", "test")
        .env("GIT_AUTHOR_EMAIL", "test@example.com")
        .env("GIT_COMMITTER_NAME", "test")
        .env("GIT_COMMITTER_EMAIL", "test@example.com")
        .output()
        .expect("git");
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn git_commit(repo: &Path) {
    git(repo, &["add", "payload.txt"]);
    git(repo, &["commit", "-m", "payload"]);
}

async fn open_repository(repo: &Path, cx: &mut TestAppContext) -> Entity<Repository> {
    let project = Project::test(Arc::new(RealFs::new(None, cx.executor())), [repo], cx).await;
    let tree = project.read_with(cx, |project, cx| project.worktrees(cx).next().unwrap());
    tree.flush_fs_events(cx).await;
    project
        .update(cx, |project, cx| project.git_scans_complete(cx))
        .await;
    cx.executor().run_until_parked();
    project
        .read_with(cx, |project, cx| {
            project.repositories(cx).values().next().cloned()
        })
        .expect("host repository was not opened")
}

async fn create_worktree(
    repository: &Entity<Repository>,
    target: CreateWorktreeTarget,
    directory: PathBuf,
    cx: &mut TestAppContext,
) -> anyhow::Result<()> {
    cx.update(|cx| {
        repository.update(cx, |repository, _| {
            repository.create_worktree(target, directory)
        })
    })
    .await
    .expect("worktree job was dropped")
}

fn assert_rejected(result: anyhow::Result<()>, escaped_root: &Path) {
    assert!(
        result.is_err(),
        "worktree creation succeeded; {} exists={}",
        escaped_root.display(),
        escaped_root.exists()
    );
    assert!(
        !escaped_root.exists(),
        "rejected worktree request still created {}",
        escaped_root.display()
    );
}

fn configured_parent(repo: &Path, setting: &str) -> PathBuf {
    worktrees_directory_for_repo(repo, setting, PathStyle::local()).unwrap()
}

#[gpui::test]
async fn guest_worktree_absolute_path_outside_project_is_rejected(cx: &mut TestAppContext) {
    init(cx);
    let host = host_repo();
    let repository = open_repository(&host.repo, cx).await;
    let escaped_root = host._root.path().join("outside-absolute");
    let directory = escaped_root.join("nested").join("checkout");

    let result = create_worktree(
        &repository,
        CreateWorktreeTarget::NewBranch {
            branch_name: "escape-absolute".into(),
            base_sha: Some("HEAD".into()),
        },
        directory,
        cx,
    )
    .await;

    assert_rejected(result, &escaped_root);
}

#[gpui::test]
async fn guest_worktree_each_target_outside_project_is_rejected(cx: &mut TestAppContext) {
    init(cx);
    let host = host_repo();
    git(&host.repo, &["branch", "existing-branch"]);
    let repository = open_repository(&host.repo, cx).await;
    let root = host._root.path();

    let cases = [
        (
            CreateWorktreeTarget::NewBranch {
                branch_name: "escape-new".into(),
                base_sha: None,
            },
            root.join("outside-new"),
        ),
        (
            CreateWorktreeTarget::ExistingBranch {
                branch_name: "existing-branch".into(),
            },
            root.join("outside-existing"),
        ),
        (
            CreateWorktreeTarget::Detached {
                base_sha: Some("HEAD".into()),
            },
            root.join("outside-detached"),
        ),
    ];

    for (target, escaped_root) in cases {
        let directory = escaped_root.join("nested").join("checkout");
        let result = create_worktree(&repository, target, directory, cx).await;
        assert_rejected(result, &escaped_root);
    }
}

#[gpui::test]
async fn guest_worktree_dotdot_path_outside_project_is_rejected(cx: &mut TestAppContext) {
    init(cx);
    let host = host_repo();
    let repository = open_repository(&host.repo, cx).await;
    let escaped_root = host._root.path().join("outside-dotdot");
    fs::create_dir_all(host.repo.join("nested")).unwrap();
    let directory = host
        .repo
        .join("nested")
        .join("..")
        .join("..")
        .join("outside-dotdot")
        .join("checkout");

    let result = create_worktree(
        &repository,
        CreateWorktreeTarget::NewBranch {
            branch_name: "escape-dotdot".into(),
            base_sha: None,
        },
        directory,
        cx,
    )
    .await;

    assert_rejected(result, &escaped_root);
}

#[gpui::test]
async fn guest_worktree_relative_parent_escape_is_rejected(cx: &mut TestAppContext) {
    init(cx);
    let host = host_repo();
    let repository = open_repository(&host.repo, cx).await;
    let escaped_root = host._root.path().join("outside-relative");

    let result = create_worktree(
        &repository,
        CreateWorktreeTarget::NewBranch {
            branch_name: "escape-relative".into(),
            base_sha: None,
        },
        PathBuf::from("../outside-relative/checkout"),
        cx,
    )
    .await;

    assert_rejected(result, &escaped_root);
}

#[gpui::test]
#[cfg(unix)]
async fn guest_worktree_symlink_under_project_is_rejected(cx: &mut TestAppContext) {
    init(cx);
    let host = host_repo();
    let escaped_root = host._root.path().join("outside-symlink");
    std::os::unix::fs::symlink(&escaped_root, host.repo.join("link")).unwrap();
    let repository = open_repository(&host.repo, cx).await;

    let result = create_worktree(
        &repository,
        CreateWorktreeTarget::NewBranch {
            branch_name: "escape-symlink".into(),
            base_sha: None,
        },
        host.repo.join("link").join("checkout"),
        cx,
    )
    .await;

    assert_rejected(result, &escaped_root);
}

#[gpui::test]
#[cfg(unix)]
async fn guest_worktree_symlink_under_configured_parent_is_rejected(cx: &mut TestAppContext) {
    init(cx);
    let host = host_repo();
    let parent = configured_parent(&host.repo, "../worktrees");
    fs::create_dir_all(&parent).unwrap();
    let escaped_root = host._root.path().join("outside-configured-symlink");
    std::os::unix::fs::symlink(&escaped_root, parent.join("link")).unwrap();
    let repository = open_repository(&host.repo, cx).await;

    let result = create_worktree(
        &repository,
        CreateWorktreeTarget::NewBranch {
            branch_name: "escape-configured-symlink".into(),
            base_sha: None,
        },
        parent.join("link").join("checkout"),
        cx,
    )
    .await;

    assert_rejected(result, &escaped_root);
}

#[gpui::test]
async fn guest_worktree_inside_repository_is_allowed(cx: &mut TestAppContext) {
    init(cx);
    let host = host_repo();
    let repository = open_repository(&host.repo, cx).await;
    let directory = host.repo.join("inside-worktree");

    create_worktree(
        &repository,
        CreateWorktreeTarget::NewBranch {
            branch_name: "inside-ok".into(),
            base_sha: Some("HEAD".into()),
        },
        directory.clone(),
        cx,
    )
    .await
    .expect("worktree inside the repository");

    let payload = fs::read_to_string(directory.join("payload.txt")).unwrap();
    assert_eq!(payload, PAYLOAD);
}

#[gpui::test]
async fn guest_worktree_configured_parent_is_allowed_and_other_parent_is_rejected(
    cx: &mut TestAppContext,
) {
    init(cx);
    cx.update(|cx| {
        cx.update_global::<settings::SettingsStore, _>(|store, cx| {
            store.update_user_settings(cx, |settings| {
                settings
                    .git
                    .get_or_insert_with(Default::default)
                    .worktree_directory = Some("../custom-wt".to_string());
            });
        });
    });

    let host = host_repo();
    let repository = open_repository(&host.repo, cx).await;
    let allowed = configured_parent(&host.repo, "../custom-wt");
    let allowed_checkout = allowed.join("feature");
    create_worktree(
        &repository,
        CreateWorktreeTarget::NewBranch {
            branch_name: "custom-parent".into(),
            base_sha: None,
        },
        allowed_checkout.clone(),
        cx,
    )
    .await
    .expect("worktree under the configured parent");
    assert_eq!(
        fs::read_to_string(allowed_checkout.join("payload.txt")).unwrap(),
        PAYLOAD
    );

    let default_parent = configured_parent(&host.repo, "../worktrees");
    let escaped_root = default_parent;
    let result = create_worktree(
        &repository,
        CreateWorktreeTarget::NewBranch {
            branch_name: "default-parent".into(),
            base_sha: None,
        },
        escaped_root.join("other"),
        cx,
    )
    .await;
    assert_rejected(result, &escaped_root);
}

#[gpui::test]
async fn guest_git_init_outside_project_is_rejected(cx: &mut TestAppContext) {
    init(cx);
    let host = host_repo();
    let project = Project::test(
        Arc::new(RealFs::new(None, cx.executor())),
        [host.repo.as_path()],
        cx,
    )
    .await;
    let outside = host._root.path().join("existing-outside");
    fs::create_dir_all(&outside).unwrap();
    let git_store = project.read_with(cx, |project, _| project.git_store().clone());

    let result = cx
        .update(|cx| {
            git_store.update(cx, |store, cx| {
                store.git_init(outside.clone().into(), "main".into(), cx)
            })
        })
        .await;

    assert!(
        result.is_err(),
        "git init succeeded in {}",
        outside.display()
    );
    assert!(
        !outside.join(".git").exists(),
        "rejected git init still created {}",
        outside.join(".git").display()
    );
}

/// The symlink points inside the allowed tree when it is first looked up.
/// `mkdir` retargets it before the directory is created, which is the window
/// between a check and `create_dir_all` / `git worktree add`.
#[gpui::test]
#[cfg(unix)]
async fn guest_worktree_symlink_swap_before_mkdir_does_not_escape(cx: &mut TestAppContext) {
    if std::env::var_os("ZED_TOCTOU_INNER").is_none() {
        run_toctou_child();
        return;
    }

    init(cx);
    assert_mkdir_hook_is_loaded();

    let host = host_repo();
    let outside = host._root.path().join("outside-toctou");
    let safe = host.repo.join("safe");
    fs::create_dir_all(&outside).unwrap();
    fs::create_dir_all(&safe).unwrap();
    let gate = host.repo.join("gate");
    std::os::unix::fs::symlink(&safe, &gate).unwrap();

    unsafe {
        std::env::set_var("ZED_TOCTOU_GATE", &gate);
        std::env::set_var("ZED_TOCTOU_LINK", &gate);
        std::env::set_var("ZED_TOCTOU_TARGET", &outside);
    }

    let repository = open_repository(&host.repo, cx).await;
    let directory = gate.join("nested").join("checkout");
    let result = create_worktree(
        &repository,
        CreateWorktreeTarget::NewBranch {
            branch_name: "escape-toctou".into(),
            base_sha: None,
        },
        directory,
        cx,
    )
    .await;

    let escaped = outside.join("nested");
    assert!(
        !escaped.exists(),
        "worktree checkout escaped through a swapped symlink at {}",
        escaped.display()
    );
    if result.is_ok() {
        let inside = safe.join("nested").join("checkout").join("payload.txt");
        assert_eq!(fs::read_to_string(inside).unwrap(), PAYLOAD);
    }
}

fn assert_mkdir_hook_is_loaded() {
    let root = std::env::temp_dir().join(format!("zed-toctou-probe-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    let safe = root.join("safe");
    let outside = root.join("outside");
    let gate = root.join("gate");
    fs::create_dir_all(&safe).unwrap();
    fs::create_dir_all(&outside).unwrap();
    std::os::unix::fs::symlink(&safe, &gate).unwrap();
    unsafe {
        std::env::set_var("ZED_TOCTOU_GATE", &gate);
        std::env::set_var("ZED_TOCTOU_LINK", &gate);
        std::env::set_var("ZED_TOCTOU_TARGET", &outside);
    }
    let _ = fs::create_dir(gate.join("child"));
    let target = fs::read_link(&gate).unwrap();
    let _ = fs::remove_dir_all(&root);
    assert_eq!(
        target, outside,
        "LD_PRELOAD mkdir hook did not retarget the symlink"
    );
}

#[allow(clippy::disallowed_methods)]
fn run_toctou_child() {
    let source = r#"
#define _GNU_SOURCE
#include <dlfcn.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <unistd.h>

static void maybe_flip(const char *path) {
    const char *gate = getenv("ZED_TOCTOU_GATE");
    const char *target = getenv("ZED_TOCTOU_TARGET");
    const char *link = getenv("ZED_TOCTOU_LINK");
    if (!gate || !target || !link || !path) return;
    size_t n = strlen(gate);
    if (strncmp(path, gate, n) != 0) return;
    if (path[n] != '\0' && path[n] != '/') return;
    unlink(link);
    symlink(target, link);
    unsetenv("ZED_TOCTOU_GATE");
}

int mkdir(const char *path, mode_t mode) {
    static int (*real_mkdir)(const char *, mode_t) = 0;
    if (!real_mkdir) real_mkdir = dlsym(RTLD_NEXT, "mkdir");
    maybe_flip(path);
    return real_mkdir(path, mode);
}
"#;
    let dir = std::env::temp_dir().join(format!("zed-toctou-hook-{}", std::process::id()));
    fs::create_dir_all(&dir).unwrap();
    let source_path = dir.join("mkdir_hook.c");
    let library = dir.join("mkdir_hook.so");
    fs::write(&source_path, source).unwrap();
    let compiled = Command::new("gcc")
        .args([
            "-shared",
            "-fPIC",
            "-o",
            library.to_str().unwrap(),
            source_path.to_str().unwrap(),
            "-ldl",
        ])
        .status()
        .expect("gcc");
    assert!(compiled.success(), "failed to compile mkdir hook");

    let output = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", TOCTOU_TEST, "--test-threads=1", "--nocapture"])
        .env("ZED_TOCTOU_INNER", "1")
        .env("LD_PRELOAD", &library)
        .output()
        .expect("re-exec test");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stdout.contains("running 1 test"),
        "toctou child did not run the test\n{stdout}\n{stderr}"
    );
    assert!(
        output.status.success(),
        "toctou child failed\n{stdout}\n{stderr}"
    );
}

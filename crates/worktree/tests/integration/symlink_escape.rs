use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::{io, path::Path, path::PathBuf, pin::Pin};

use anyhow::Result;
use async_tar::Archive;
use async_trait::async_trait;
use futures::AsyncRead;
use futures::Stream;
use git::repository::GitRepository;
use gpui::TestAppContext;
use language::LocalFile;
use serde_json::json;
use text::Rope;
use util::{
    rel_path::rel_path,
    test::TempTree,
};
use worktree::{File, Worktree};

use fs::{
    CopyOptions, CreateOptions, Fs, Metadata, PathEvent, RealFs, RemoveOptions, RenameOptions,
    TrashId, TrashRestoreError,
};

const OUTSIDE_FILE_MARKER: &str = "outside-file-marker\n";
const OUTSIDE_DIR_MARKER: &str = "outside-dir-marker\n";
const OUTSIDE_NESTED_MARKER: &str = "outside-nested-marker\n";
const INSIDE_FILE_MARKER: &str = "inside-file-marker\n";
const INSIDE_DIR_MARKER: &str = "inside-dir-marker\n";

struct SymlinkRace {
    link: PathBuf,
    outside_target: PathBuf,
    swapped: AtomicBool,
}

/// Filesystem wrapper around [`RealFs`].
///
/// `escaped` becomes true when a read, write, or directory listing resolves
/// under `outside_root`. When `armed`, the first canonicalize of a raced
/// symlink returns the target that was current at the start of that call, and
/// the link is then retargeted. A later read or write of the same path swaps
/// first if canonicalize did not run.
struct SymlinkProbeFs {
    inner: Arc<dyn Fs>,
    outside_root: PathBuf,
    armed: AtomicBool,
    escaped: AtomicBool,
    races: Vec<SymlinkRace>,
}

impl SymlinkProbeFs {
    fn new(inner: Arc<dyn Fs>, outside_root: PathBuf, races: Vec<(PathBuf, PathBuf)>) -> Arc<Self> {
        Arc::new(Self {
            inner,
            outside_root: std::fs::canonicalize(outside_root).unwrap(),
            armed: AtomicBool::new(false),
            escaped: AtomicBool::new(false),
            races: races
                .into_iter()
                .map(|(link, outside_target)| SymlinkRace {
                    link,
                    outside_target,
                    swapped: AtomicBool::new(false),
                })
                .collect(),
        })
    }

    fn arm(&self) {
        self.armed.store(true, Ordering::SeqCst);
    }

    fn escaped(&self) -> bool {
        self.escaped.load(Ordering::SeqCst)
    }

    fn retarget_if_armed(&self, path: &Path) {
        if !self.armed.load(Ordering::SeqCst) {
            return;
        }
        for race in &self.races {
            if path == race.link || path.starts_with(&race.link) {
                if race
                    .swapped
                    .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
                    .is_ok()
                {
                    retarget_symlink(&race.link, &race.outside_target);
                }
                return;
            }
        }
    }

    fn before_use(&self, path: &Path) {
        self.retarget_if_armed(path);
        if self.resolves_outside(path) {
            self.escaped.store(true, Ordering::SeqCst);
        }
    }

    fn resolves_outside(&self, path: &Path) -> bool {
        let candidate = if path.exists() {
            std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
        } else if let (Some(parent), Some(name)) = (path.parent(), path.file_name()) {
            std::fs::canonicalize(parent)
                .unwrap_or_else(|_| parent.to_path_buf())
                .join(name)
        } else {
            path.to_path_buf()
        };
        candidate.starts_with(&self.outside_root)
    }
}

fn retarget_symlink(link: &Path, target: &Path) {
    let file_name = link.file_name().unwrap().to_string_lossy();
    let temporary = link
        .parent()
        .unwrap()
        .join(format!(".{file_name}.retarget"));
    let _ = std::fs::remove_file(&temporary);
    std::os::unix::fs::symlink(target, &temporary).unwrap();
    std::fs::rename(&temporary, link).unwrap();
}

#[async_trait]
impl Fs for SymlinkProbeFs {
    async fn create_dir(&self, path: &Path) -> Result<()> {
        self.before_use(path);
        self.inner.create_dir(path).await
    }

    async fn create_symlink(&self, path: &Path, target: PathBuf) -> Result<()> {
        self.inner.create_symlink(path, target).await
    }

    async fn create_file(&self, path: &Path, options: CreateOptions) -> Result<()> {
        self.before_use(path);
        self.inner.create_file(path, options).await
    }

    async fn create_file_with(
        &self,
        path: &Path,
        content: Pin<&mut (dyn AsyncRead + Send)>,
    ) -> Result<()> {
        self.before_use(path);
        self.inner.create_file_with(path, content).await
    }

    async fn extract_tar_file(
        &self,
        path: &Path,
        content: Archive<Pin<&mut (dyn AsyncRead + Send)>>,
    ) -> Result<()> {
        self.before_use(path);
        self.inner.extract_tar_file(path, content).await
    }

    async fn copy_file(&self, source: &Path, target: &Path, options: CopyOptions) -> Result<()> {
        self.before_use(source);
        self.before_use(target);
        self.inner.copy_file(source, target, options).await
    }

    async fn rename(&self, source: &Path, target: &Path, options: RenameOptions) -> Result<()> {
        self.inner.rename(source, target, options).await
    }

    async fn remove_dir(&self, path: &Path, options: RemoveOptions) -> Result<()> {
        self.inner.remove_dir(path, options).await
    }

    async fn trash(&self, path: &Path, options: RemoveOptions) -> Result<TrashId> {
        self.inner.trash(path, options).await
    }

    async fn remove_file(&self, path: &Path, options: RemoveOptions) -> Result<()> {
        self.inner.remove_file(path, options).await
    }

    async fn open_handle(&self, path: &Path) -> Result<Arc<dyn fs::FileHandle>> {
        self.before_use(path);
        self.inner.open_handle(path).await
    }

    async fn open_sync(&self, path: &Path) -> Result<Box<dyn io::Read + Send + Sync>> {
        self.before_use(path);
        self.inner.open_sync(path).await
    }

    async fn load(&self, path: &Path) -> Result<String> {
        self.before_use(path);
        self.inner.load(path).await
    }

    async fn load_bytes(&self, path: &Path) -> Result<Vec<u8>> {
        self.before_use(path);
        self.inner.load_bytes(path).await
    }

    async fn atomic_write(&self, path: PathBuf, text: String) -> Result<()> {
        self.before_use(&path);
        self.inner.atomic_write(path, text).await
    }

    async fn save(&self, path: &Path, text: &Rope, line_ending: text::LineEnding) -> Result<()> {
        self.before_use(path);
        self.inner.save(path, text, line_ending).await
    }

    async fn write(&self, path: &Path, content: &[u8]) -> Result<()> {
        self.before_use(path);
        self.inner.write(path, content).await
    }

    async fn canonicalize(&self, path: &Path) -> Result<PathBuf> {
        let canonical = self.inner.canonicalize(path).await?;
        self.retarget_if_armed(path);
        Ok(canonical)
    }

    async fn is_file(&self, path: &Path) -> bool {
        self.inner.is_file(path).await
    }

    async fn is_dir(&self, path: &Path) -> bool {
        self.inner.is_dir(path).await
    }

    async fn metadata(&self, path: &Path) -> Result<Option<Metadata>> {
        self.inner.metadata(path).await
    }

    async fn read_link(&self, path: &Path) -> Result<PathBuf> {
        self.inner.read_link(path).await
    }

    async fn read_dir(
        &self,
        path: &Path,
    ) -> Result<Pin<Box<dyn Send + Stream<Item = Result<PathBuf>>>>> {
        self.before_use(path);
        self.inner.read_dir(path).await
    }

    async fn watch(
        &self,
        path: &Path,
        latency: std::time::Duration,
    ) -> (
        Pin<Box<dyn Send + Stream<Item = Vec<PathEvent>>>>,
        Arc<dyn fs::Watcher>,
    ) {
        self.inner.watch(path, latency).await
    }

    fn open_repo(
        &self,
        abs_dot_git: &Path,
        system_git_binary_path: Option<&Path>,
    ) -> Result<Arc<dyn GitRepository>> {
        self.inner.open_repo(abs_dot_git, system_git_binary_path)
    }

    async fn git_init(
        &self,
        abs_work_directory: &Path,
        fallback_branch_name: String,
    ) -> Result<()> {
        self.inner
            .git_init(abs_work_directory, fallback_branch_name)
            .await
    }

    async fn git_clone(&self, abs_work_directory: &Path, repo_url: &str) -> Result<()> {
        self.inner.git_clone(abs_work_directory, repo_url).await
    }

    async fn git_config(&self, abs_work_directory: &Path, args: Vec<String>) -> Result<String> {
        self.inner.git_config(abs_work_directory, args).await
    }

    fn is_fake(&self) -> bool {
        false
    }

    async fn is_case_sensitive(&self) -> bool {
        self.inner.is_case_sensitive().await
    }

    fn subscribe_to_jobs(&self) -> fs::JobEventReceiver {
        self.inner.subscribe_to_jobs()
    }

    fn original_path_for_trash_id(&self, trash_id: TrashId) -> Option<PathBuf> {
        self.inner.original_path_for_trash_id(trash_id)
    }

    async fn restore(&self, trash_id: TrashId) -> Result<PathBuf, TrashRestoreError> {
        self.inner.restore(trash_id).await
    }
}

struct OutsideTree {
    root: TempTree,
    project: PathBuf,
    outside_file: PathBuf,
    outside_dir_file: PathBuf,
    outside_nested: PathBuf,
}

fn outside_tree() -> OutsideTree {
    let root = TempTree::new(json!({
        "project": {
            "kept.txt": "kept\n",
        },
        "outside": {
            "secret.txt": OUTSIDE_FILE_MARKER,
            "tree": {
                "secret.txt": OUTSIDE_DIR_MARKER,
                "sub": {
                    "nested.txt": OUTSIDE_NESTED_MARKER,
                }
            }
        }
    }));
    let project = root.path().join("project");
    std::os::unix::fs::symlink("../outside/secret.txt", project.join("notes.txt")).unwrap();
    std::os::unix::fs::symlink("../outside/tree", project.join("alias")).unwrap();
    OutsideTree {
        outside_file: root.path().join("outside/secret.txt"),
        outside_dir_file: root.path().join("outside/tree/secret.txt"),
        outside_nested: root.path().join("outside/tree/sub/nested.txt"),
        project,
        root,
    }
}

async fn open_worktree(
    cx: &mut TestAppContext,
    project: &Path,
    fs: Arc<dyn Fs>,
) -> gpui::Entity<Worktree> {
    Worktree::local(
        project,
        true,
        fs,
        Default::default(),
        true,
        settings::WorktreeId::from_proto(0),
        &mut cx.to_async(),
    )
    .await
    .unwrap()
}

async fn scan(cx: &mut TestAppContext, tree: &gpui::Entity<Worktree>) {
    cx.read(|cx| tree.read(cx).as_local().unwrap().scan_complete())
        .await;
}

fn assert_marker_absent(text: &str, marker: &str) {
    assert!(
        !text.contains(marker.trim_end()),
        "contents from outside the worktree were returned: {text:?}"
    );
}

fn assert_untouched(path: &Path, marker: &str) {
    let actual = std::fs::read_to_string(path).unwrap();
    assert_eq!(
        actual, marker,
        "file outside the worktree at {} was modified",
        path.display()
    );
}

#[gpui::test]
async fn test_shared_worktree_update_lists_symlink_targets_outside_the_root(
    cx: &mut TestAppContext,
) {
    cx.executor().allow_parking();
    super::init_test(cx);
    let tree = outside_tree();
    let probe = SymlinkProbeFs::new(
        Arc::new(RealFs::new(None, cx.executor())),
        tree.root.path().join("outside"),
        Vec::new(),
    );
    let worktree = open_worktree(cx, &tree.project, probe.clone()).await;
    scan(cx, &worktree).await;

    let updates = Arc::new(parking_lot::Mutex::new(Vec::new()));
    worktree.update(cx, |worktree, cx| {
        let updates = updates.clone();
        worktree.observe_updates(1, cx, move |update| {
            updates.lock().push(update);
            async move { true }
        });
    });
    let alias_id = worktree.read_with(cx, |worktree, _| {
        worktree.entry_for_path(rel_path("alias")).unwrap().id
    });
    worktree
        .update(cx, |worktree, cx| worktree.expand_entry(alias_id, cx))
        .unwrap()
        .await
        .unwrap();
    cx.executor().run_until_parked();

    let project_root = std::fs::canonicalize(&tree.project).unwrap();
    let updates = updates.lock().clone();
    assert!(
        !updates.is_empty(),
        "sharing a project produced no worktree update"
    );
    for update in &updates {
        for entry in &update.updated_entries {
            if let Some(canonical) = &entry.canonical_path {
                let canonical = PathBuf::from(canonical);
                assert!(
                    canonical.starts_with(&project_root),
                    "shared entry {} has canonical path {} outside the worktree",
                    entry.path,
                    canonical.display()
                );
            }
            assert!(
                !entry.is_external,
                "shared entry {} is outside the worktree",
                entry.path
            );
        }
    }
    assert!(
        !probe.escaped(),
        "expanding a symlink scanned a directory outside the worktree"
    );
    assert!(
        !worktree.read_with(cx, |worktree, _| {
            worktree
                .entry_for_path(rel_path("alias/secret.txt"))
                .is_some()
        }),
        "refresh listed a file that exists only outside the worktree"
    );
}

#[gpui::test]
async fn test_load_file_and_binary_follow_symlink_outside_the_worktree(cx: &mut TestAppContext) {
    cx.executor().allow_parking();
    super::init_test(cx);
    let tree = outside_tree();
    let probe = SymlinkProbeFs::new(
        Arc::new(RealFs::new(None, cx.executor())),
        tree.root.path().join("outside"),
        Vec::new(),
    );
    let worktree = open_worktree(cx, &tree.project, probe.clone()).await;
    scan(cx, &worktree).await;
    assert!(
        !probe.escaped(),
        "scanning the project read a file outside the worktree"
    );

    let loaded = worktree
        .update(cx, |worktree, cx| {
            worktree.load_file(rel_path("notes.txt"), cx)
        })
        .await;
    match loaded {
        Ok(loaded) => {
            assert_marker_absent(&loaded.text, OUTSIDE_FILE_MARKER);
            assert!(
                loaded.file.is_private,
                "buffer for a symlink out of the worktree would be shared"
            );
        }
        Err(_) => {}
    }
    assert!(
        !probe.escaped(),
        "load_file read a file outside the worktree"
    );

    let binary = worktree
        .update(cx, |worktree, cx| {
            worktree.load_binary_file(rel_path("notes.txt"), cx)
        })
        .await;
    if let Ok(binary) = binary {
        let text = String::from_utf8_lossy(&binary.content);
        assert_marker_absent(&text, OUTSIDE_FILE_MARKER);
        assert!(binary.file.is_private);
    }
    assert!(
        !probe.escaped(),
        "load_binary_file read a file outside the worktree"
    );

    let entry = worktree.read_with(cx, |worktree, _| {
        worktree.entry_for_path(rel_path("notes.txt")).unwrap().clone()
    });
    let file = worktree.update(cx, |_, cx| File::for_entry(entry, cx.entity()));
    let text = cx.update(|cx| file.load(cx)).await;
    if let Ok(text) = text {
        assert_marker_absent(&text, OUTSIDE_FILE_MARKER);
    }
    let bytes = cx.update(|cx| file.load_bytes(cx)).await;
    if let Ok(bytes) = bytes {
        assert_marker_absent(&String::from_utf8_lossy(&bytes), OUTSIDE_FILE_MARKER);
    }
    assert!(
        !probe.escaped(),
        "File::load read a file outside the worktree"
    );
}

#[gpui::test]
async fn test_refresh_of_symlink_child_reads_outside_directory(cx: &mut TestAppContext) {
    cx.executor().allow_parking();
    super::init_test(cx);
    let tree = outside_tree();
    let probe = SymlinkProbeFs::new(
        Arc::new(RealFs::new(None, cx.executor())),
        tree.root.path().join("outside"),
        Vec::new(),
    );
    let worktree = open_worktree(cx, &tree.project, probe.clone()).await;
    scan(cx, &worktree).await;

    let loaded = worktree
        .update(cx, |worktree, cx| {
            worktree.load_file(rel_path("alias/sub/nested.txt"), cx)
        })
        .await;
    if let Ok(loaded) = loaded {
        assert_marker_absent(&loaded.text, OUTSIDE_NESTED_MARKER);
        assert!(loaded.file.is_private);
    }
    assert!(
        !probe.escaped(),
        "refreshing a path under a symlinked directory scanned or read outside the worktree"
    );
    assert_untouched(&tree.outside_nested, OUTSIDE_NESTED_MARKER);
}

#[gpui::test]
async fn test_write_and_create_follow_symlink_outside_the_worktree(cx: &mut TestAppContext) {
    cx.executor().allow_parking();
    super::init_test(cx);
    let tree = outside_tree();
    let probe = SymlinkProbeFs::new(
        Arc::new(RealFs::new(None, cx.executor())),
        tree.root.path().join("outside"),
        Vec::new(),
    );
    let worktree = open_worktree(cx, &tree.project, probe.clone()).await;
    scan(cx, &worktree).await;

    let _ = worktree
        .update(cx, |worktree, cx| {
            worktree.write_file(
                rel_path("notes.txt").into(),
                "guest-rewrite\n".into(),
                text::LineEnding::Unix,
                encoding_rs::UTF_8,
                false,
                cx,
            )
        })
        .await;
    assert_untouched(&tree.outside_file, OUTSIDE_FILE_MARKER);
    assert!(
        !probe.escaped(),
        "write_file followed a symlink and wrote outside the worktree"
    );
}

#[gpui::test]
async fn test_create_entry_follows_directory_symlink(cx: &mut TestAppContext) {
    cx.executor().allow_parking();
    super::init_test(cx);
    let tree = outside_tree();
    let probe = SymlinkProbeFs::new(
        Arc::new(RealFs::new(None, cx.executor())),
        tree.root.path().join("outside"),
        Vec::new(),
    );
    let worktree = open_worktree(cx, &tree.project, probe.clone()).await;
    scan(cx, &worktree).await;

    let planted = tree.root.path().join("outside/tree/planted.txt");
    assert!(!planted.exists());
    let _ = worktree
        .update(cx, |worktree, cx| {
            worktree.create_entry(
                rel_path("alias/planted.txt").into(),
                false,
                Some(b"guest-created\n".to_vec()),
                cx,
            )
        })
        .await;
    assert!(
        !planted.exists(),
        "create_entry created {} outside the worktree",
        planted.display()
    );
    assert_untouched(&tree.outside_dir_file, OUTSIDE_DIR_MARKER);
    assert!(
        !probe.escaped(),
        "create_entry followed a directory symlink and wrote outside the worktree"
    );
}

#[gpui::test]
async fn test_symlink_retargeted_between_check_and_use_is_not_read_or_written(
    cx: &mut TestAppContext,
) {
    cx.executor().allow_parking();
    super::init_test(cx);

    let root = TempTree::new(json!({
        "project": {
            "inside.txt": INSIDE_FILE_MARKER,
            "inbox": {
                "secret.txt": INSIDE_DIR_MARKER,
                "only-inside.txt": "only-inside\n",
            }
        },
        "outside": {
            "secret.txt": OUTSIDE_FILE_MARKER,
            "tree": {
                "secret.txt": OUTSIDE_DIR_MARKER,
                "only-outside.txt": "only-outside\n",
            }
        }
    }));
    let project = root.path().join("project");
    let link = project.join("link.txt");
    let alias = project.join("alias");
    std::os::unix::fs::symlink("inside.txt", &link).unwrap();
    std::os::unix::fs::symlink("inbox", &alias).unwrap();
    let outside_file = root.path().join("outside/secret.txt");

    let probe = SymlinkProbeFs::new(
        Arc::new(RealFs::new(None, cx.executor())),
        root.path().join("outside"),
        vec![
            (link.clone(), std::fs::canonicalize(&outside_file).unwrap()),
            (
                alias.clone(),
                std::fs::canonicalize(root.path().join("outside/tree")).unwrap(),
            ),
        ],
    );
    let worktree = open_worktree(cx, &project, probe.clone()).await;
    scan(cx, &worktree).await;
    assert!(!probe.escaped());
    probe.arm();

    let loaded = worktree
        .update(cx, |worktree, cx| worktree.load_file(rel_path("link.txt"), cx))
        .await;
    if let Ok(loaded) = loaded {
        assert_marker_absent(&loaded.text, OUTSIDE_FILE_MARKER);
    }
    assert!(
        !probe.escaped(),
        "load followed a symlink that changed after the containment check"
    );
    assert_untouched(&outside_file, OUTSIDE_FILE_MARKER);
}

#[gpui::test]
async fn test_directory_symlink_retargeted_between_check_and_scan(cx: &mut TestAppContext) {
    cx.executor().allow_parking();
    super::init_test(cx);

    let root = TempTree::new(json!({
        "project": {
            "inbox": {
                "secret.txt": INSIDE_DIR_MARKER,
                "only-inside.txt": "only-inside\n",
            }
        },
        "outside": {
            "tree": {
                "secret.txt": OUTSIDE_DIR_MARKER,
                "only-outside.txt": "only-outside\n",
            }
        }
    }));
    let project = root.path().join("project");
    let alias = project.join("alias");
    std::os::unix::fs::symlink("inbox", &alias).unwrap();
    let probe = SymlinkProbeFs::new(
        Arc::new(RealFs::new(None, cx.executor())),
        root.path().join("outside"),
        vec![(
            alias,
            std::fs::canonicalize(root.path().join("outside/tree")).unwrap(),
        )],
    );
    let worktree = open_worktree(cx, &project, probe.clone()).await;
    scan(cx, &worktree).await;
    assert!(!probe.escaped());
    probe.arm();

    let nested = worktree
        .update(cx, |worktree, cx| {
            worktree.load_file(rel_path("alias/secret.txt"), cx)
        })
        .await;
    if let Ok(nested) = nested {
        assert_marker_absent(&nested.text, OUTSIDE_DIR_MARKER);
    }
    assert!(
        !probe.escaped(),
        "load of a child followed a directory symlink that changed after the containment check"
    );
}

#[gpui::test]
async fn test_write_follows_symlink_retargeted_after_check(cx: &mut TestAppContext) {
    cx.executor().allow_parking();
    super::init_test(cx);

    let root = TempTree::new(json!({
        "project": {
            "inside.txt": INSIDE_FILE_MARKER,
        },
        "outside": {
            "secret.txt": OUTSIDE_FILE_MARKER,
        }
    }));
    let project = root.path().join("project");
    let link = project.join("link.txt");
    std::os::unix::fs::symlink("inside.txt", &link).unwrap();
    let outside_file = root.path().join("outside/secret.txt");
    let probe = SymlinkProbeFs::new(
        Arc::new(RealFs::new(None, cx.executor())),
        root.path().join("outside"),
        vec![(link, std::fs::canonicalize(&outside_file).unwrap())],
    );
    let worktree = open_worktree(cx, &project, probe.clone()).await;
    scan(cx, &worktree).await;
    probe.arm();

    let _ = worktree
        .update(cx, |worktree, cx| {
            worktree.write_file(
                rel_path("link.txt").into(),
                "guest-rewrite\n".into(),
                text::LineEnding::Unix,
                encoding_rs::UTF_8,
                false,
                cx,
            )
        })
        .await;
    assert_untouched(&outside_file, OUTSIDE_FILE_MARKER);
    assert!(
        !probe.escaped(),
        "save followed a symlink that changed after the containment check"
    );
}

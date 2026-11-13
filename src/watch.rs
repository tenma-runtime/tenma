//! `tenma --watch`: reloads the workers when a file the script loaded changes.
//! Workers report the files PHP loaded, also later ones (classes the autoloader loads on the first
//! request). A thread watches their directories, not the files: many editors save by writing a
//! new file and renaming it over the old one. Only a different content counts as a change.
//!
//! The dev thread (the hooks of tenma.json, src/dev.rs) reports its files as well (PHP hooks), and
//! the directories its hooks read:
//! changes there go to the hooks, which match them against their globs. `.git`, `node_modules`
//! and `vendor` are no input (see IGNORED).

use std::cell::Cell;
use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::{OnceLock, mpsc};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use notify::Watcher;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};

/// Saving often writes several times, `git checkout` changes many files: one reload for all, once
/// no event came for SETTLE, at the latest after MAX_SETTLE.
const SETTLE: Duration = Duration::from_millis(100);
const MAX_SETTLE: Duration = Duration::from_secs(1);

/// Who loaded a file: the workers running the script, or the dev thread running the hooks.
const APP: u8 = 1;
const DEV: u8 = 2;

enum Message {
    /// A file PHP loaded, by whom, and the earliest time it may have been loaded.
    Loaded(PathBuf, u8, SystemTime),
    /// Directories the hooks read, recursively or not, and the project they belong to.
    Roots(Vec<(PathBuf, bool)>, PathBuf),
    /// A file the script is about to write (Watch::writing()) and its content's fingerprint.
    Writing(PathBuf, u64),
    Event(notify::Result<notify::Event>),
}

/// What changed once the writing settled: the first file of the script and of the hooks.
#[derive(Default)]
pub struct Changes {
    pub app: Option<PathBuf>,
    pub dev: Option<PathBuf>,
}

/// The way to the watcher thread; only set with --watch.
static WATCHER: OnceLock<mpsc::Sender<Message>> = OnceLock::new();
/// Changed files below the hooks' directories with their modification time, for the dev thread.
static HOOKS: OnceLock<tokio::sync::Mutex<UnboundedReceiver<Vec<(PathBuf, f64)>>>> =
    OnceLock::new();

thread_local! {
    /// How many of this interpreter's loaded files are already reported.
    static REPORTED: Cell<usize> = const { Cell::new(0) };
    /// Since when the files not yet reported may have been loaded: the last report, or the start.
    static SINCE: Cell<SystemTime> = const { Cell::new(UNIX_EPOCH) };
    static GROUP: Cell<u8> = const { Cell::new(APP) };
}

unsafe extern "C" {
    fn tenma_set_watch();
    fn tenma_report_included(from: usize) -> usize;
}

#[unsafe(no_mangle)]
extern "C" fn tenma_rs_included(path: *const u8, len: usize) {
    crate::journal::loaded(crate::php::bytes(path, len));
    if let Some(watcher) = WATCHER.get() {
        let path = Path::new(std::ffi::OsStr::from_bytes(crate::php::bytes(path, len)));
        let _ = watcher.send(Message::Loaded(
            path.to_path_buf(),
            GROUP.get(),
            SINCE.get(),
        ));
    }
}

/// Watch::writing(): the script is about to write the file itself, such as a compiled template it
/// loads. Sent before the write, so the watcher knows the content before any event of it comes.
#[unsafe(no_mangle)]
extern "C" fn tenma_rs_writing(
    path: *const u8,
    len: usize,
    content: *const u8,
    content_len: usize,
) {
    if let Some(watcher) = WATCHER.get() {
        let path = Path::new(std::ffi::OsStr::from_bytes(crate::php::bytes(path, len)));
        // Named like PHP names the files it loads: resolved, through its directory while it is new
        let path = path.canonicalize().unwrap_or_else(|_| {
            match (
                path.parent().and_then(|dir| dir.canonicalize().ok()),
                path.file_name(),
            ) {
                (Some(dir), Some(name)) => dir.join(name),
                _ => path.to_path_buf(),
            }
        });
        let content = crate::php::bytes(content, content_len);
        let _ = watcher.send(Message::Writing(path, fingerprint_of(content)));
    }
}

/// Reports the files PHP loaded since the last call. Cheap when nothing is new: compares a count.
pub fn report() {
    if WATCHER.get().is_some() {
        let now = SystemTime::now();
        REPORTED.set(unsafe { tenma_report_included(REPORTED.get()) });
        SINCE.set(now);
    }
}

/// A new interpreter starts with nothing reported.
pub fn forget() {
    REPORTED.set(0);
    SINCE.set(SystemTime::now());
}

/// This thread runs the hooks: its files restart it, not the workers.
pub fn dev_thread() {
    GROUP.set(DEV);
}

/// The dev thread with --watch, as opposed to `tenma build`.
pub fn is_dev_thread() -> bool {
    WATCHER.get().is_some() && GROUP.get() == DEV
}

/// The tenma.json with the hooks: a change reloads them.
pub fn hooks_file(path: &Path) {
    if let Some(watcher) = WATCHER.get() {
        let _ = watcher.send(Message::Loaded(path.to_path_buf(), DEV, SystemTime::now()));
    }
}

/// Watches the directories the hooks read. Changes still queued from the previous interpreter are
/// dropped: the hooks run once anyway when they start.
pub async fn roots(roots: Vec<(PathBuf, bool)>, project: PathBuf) {
    let (Some(watcher), Some(hooks)) = (WATCHER.get(), HOOKS.get()) else {
        return;
    };
    let mut hooks = hooks.lock().await;
    while hooks.try_recv().is_ok() {}
    let _ = watcher.send(Message::Roots(roots, project));
}

/// The next changed files below the hooks' directories with their modification time (0: gone).
pub async fn hook_changes() -> Option<Vec<(PathBuf, f64)>> {
    HOOKS.get()?.lock().await.recv().await
}

/// Before `php::start`: settings for watch mode (opcache checks timestamps on every load).
pub fn configure() {
    unsafe { tenma_set_watch() }
}

/// The change behind the last reload of the script, for Watch::reloadedBy().
static RELOADED_BY: std::sync::Mutex<Option<PathBuf>> = std::sync::Mutex::new(None);

thread_local! {
    /// What this worker's current run of the script was reloaded by: a change, or nothing (the first
    /// run, a restart after a fatal error or --max-requests).
    static RUN_RELOADED_BY: std::cell::RefCell<Option<PathBuf>> = const { std::cell::RefCell::new(None) };
}

/// Called before the workers run the script again because `path` changed.
pub fn reloaded_by(path: &Path) {
    *RELOADED_BY
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(path.to_path_buf());
}

/// A worker runs the script again: because of the last change, or (`changed` false) for another reason.
pub fn rerun(changed: bool) {
    let path = if changed {
        RELOADED_BY
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    } else {
        None
    };
    RUN_RELOADED_BY.with_borrow_mut(|reloaded| *reloaded = path);
}

#[unsafe(no_mangle)]
extern "C" fn tenma_rs_reloaded_by(buffer: *mut u8, capacity: usize) -> usize {
    RUN_RELOADED_BY.with_borrow(|reloaded| copy_path(reloaded.as_deref(), buffer, capacity))
}

/// The path's length; copied only if it fits, otherwise the caller asks again with that much room.
fn copy_path(reloaded: Option<&Path>, buffer: *mut u8, capacity: usize) -> usize {
    let Some(path) = reloaded else {
        return usize::MAX;
    };
    let bytes = path.as_os_str().as_bytes();
    if bytes.len() <= capacity {
        // SAFETY: the shim passes a buffer of `capacity` bytes
        unsafe { std::ptr::copy_nonoverlapping(bytes.as_ptr(), buffer, bytes.len()) };
    }
    bytes.len()
}

/// Removes the Problems::report() lists that `group` ("app": the script, "dev": the hooks) published in
/// the project of `file`: when that group reloads, a fixed problem goes without its reporter clearing it, and what is
/// still wrong is published again. The stdlib finds the project the same way (Diagnostics::project).
pub fn clear_diagnostics(file: &Path, group: &str) {
    let file = file.canonicalize().unwrap_or_else(|_| file.to_path_buf());
    let directory = file.parent().unwrap_or(Path::new("."));
    let project = directory
        .ancestors()
        .find(|at| at.join("composer.json").is_file())
        .unwrap_or(directory);
    let Ok(entries) = std::fs::read_dir(project.join(".tenma/diagnostics")) else {
        return;
    };
    let prefix = format!("{group}.");
    for entry in entries.flatten() {
        if entry.file_name().to_string_lossy().starts_with(&prefix) {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

/// Starts the watcher thread. The receiver gets the changes of each settled batch.
pub fn start() -> Result<UnboundedReceiver<Changes>, String> {
    let (sender, messages) = mpsc::channel();
    let events = sender.clone();
    let watcher = notify::recommended_watcher(move |event| {
        let _ = events.send(Message::Event(event));
    })
    .map_err(|error| format!("--watch: {error}"))?;
    let _ = WATCHER.set(sender);
    let (hooks, hook_receiver) = unbounded_channel();
    let _ = HOOKS.set(tokio::sync::Mutex::new(hook_receiver));
    let (changes, receiver) = unbounded_channel();
    std::thread::Builder::new()
        .name("tenma-watch".into())
        .spawn(move || watch(watcher, &messages, &changes, &hooks))
        .map_err(|error| format!("--watch: {error}"))?;
    Ok(receiver)
}

/// Below a hook root watched recursively: watching the directory on its own again would replace
/// that watch on macOS (FSEvents keeps one mode per path) and lose its subdirectories.
fn covered(directory: &Path, roots: &[(PathBuf, bool)]) -> bool {
    roots.iter().any(|(root, recursive)| {
        *recursive
            && (directory.starts_with(root)
                || root
                    .canonicalize()
                    .is_ok_and(|root| directory.starts_with(root)))
    })
}

fn watch(
    mut watcher: notify::RecommendedWatcher,
    messages: &mpsc::Receiver<Message>,
    changes: &UnboundedSender<Changes>,
    hooks: &UnboundedSender<Vec<(PathBuf, f64)>>,
) {
    // Fingerprints of the content of the files seen so far (None: missing)
    let mut files: HashMap<PathBuf, Option<u64>> = HashMap::new();
    // Loaded files and who loaded them
    let mut loaded: HashMap<PathBuf, u8> = HashMap::new();
    let mut directories = HashSet::new();
    // Directories that could not be watched (inotify's max_user_watches): reported once each
    let mut failed = HashSet::new();
    let mut roots: Vec<(PathBuf, bool)> = Vec::new();
    let mut project = PathBuf::new();
    let mut candidates = HashSet::new();
    // Since when events come, and until when to wait for more
    let mut settle: Option<(Instant, Instant)> = None;
    loop {
        let message = match settle {
            Some((_, deadline)) => {
                messages.recv_timeout(deadline.saturating_duration_since(Instant::now()))
            }
            None => messages
                .recv()
                .map_err(|_| mpsc::RecvTimeoutError::Disconnected),
        };
        match message {
            Ok(Message::Loaded(path, group, since)) => {
                // On every report, not only the first: workers report all their files again after a
                // reload, so a directory whose watch was lost or failed is watched again then
                if let Some(directory) = path.parent()
                    && !covered(directory, &roots)
                {
                    watch_directory(&mut watcher, &mut directories, &mut failed, directory);
                }
                let groups = loaded.entry(path.clone()).or_insert(0);
                if *groups & group != 0 {
                    continue;
                }
                *groups |= group;
                if let std::collections::hash_map::Entry::Vacant(entry) = files.entry(path.clone())
                {
                    // Reported only after the request that loaded it: saved since then, the worker may
                    // run the old content, and the fingerprint now would already be the new one
                    if std::fs::metadata(&path)
                        .and_then(|meta| meta.modified())
                        .is_ok_and(|modified| modified >= since)
                    {
                        entry.insert(None);
                        candidates.insert(path);
                        wait_for_more(&mut settle);
                    } else {
                        entry.insert(fingerprint(&path));
                    }
                }
            }
            Ok(Message::Roots(new, hooks_project)) => {
                project = hooks_project;
                for root in new {
                    if roots.contains(&root) {
                        continue;
                    }
                    let mode = if root.1 {
                        notify::RecursiveMode::Recursive
                    } else {
                        notify::RecursiveMode::NonRecursive
                    };
                    // ponytail: a directory missing now is not watched once it appears; restart tenma
                    // (or touch tenma.json) after creating it. Upgrade: watch the nearest parent.
                    if let Err(error) = watcher.watch(&root.0, mode) {
                        eprintln!(
                            "tenma: cannot watch {} for hooks: {error}",
                            root.0.display()
                        );
                    }
                    roots.push(root);
                }
            }
            // The content to come: the events of that write compare equal and reload nothing, and
            // loading it later is no first sight that counts as a change
            Ok(Message::Writing(path, coming)) => {
                files.insert(path, Some(coming));
            }
            Ok(Message::Event(Ok(event))) if event.need_rescan() => {
                candidates.extend(files.keys().cloned());
                wait_for_more(&mut settle);
            }
            Ok(Message::Event(Ok(event))) => {
                // A watched directory deleted or moved away (git checkout, composer install): inotify
                // drops or keeps following its watch, so it is watched anew at the next report
                if matches!(
                    event.kind,
                    notify::EventKind::Remove(_)
                        | notify::EventKind::Modify(notify::event::ModifyKind::Name(_))
                ) {
                    for path in &event.paths {
                        if directories.remove(path) {
                            let _ = watcher.unwatch(path);
                        }
                    }
                }
                for path in event.paths {
                    if let Some(path) = relevant(&loaded, &roots, &project, path) {
                        candidates.insert(path);
                        wait_for_more(&mut settle);
                    }
                }
            }
            // Events were lost (inotify's queue overflowed, FSEvents says MustScanSubDirs, or an
            // error): every known file is compared again
            Ok(Message::Event(Err(error))) => {
                eprintln!("tenma: --watch: {error}, checking all files");
                candidates.extend(files.keys().cloned());
                wait_for_more(&mut settle);
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                settle = None;
                // Compared only now that the writing has settled: no reload for a half-written file
                let mut batch = Changes::default();
                let mut hook_paths = Vec::new();
                for path in candidates.drain() {
                    let now = fingerprint(&path);
                    // A file seen for the first time counts as changed
                    if files.insert(path.clone(), now) == Some(now) {
                        continue;
                    }
                    let groups = loaded.get(&path).copied().unwrap_or(0);
                    if groups & APP != 0 {
                        batch.app.get_or_insert(path.clone());
                    }
                    if groups & DEV != 0 {
                        batch.dev.get_or_insert(path.clone());
                    }
                    if below(&roots, &path) && !ignored(&project, &path) {
                        let modified = std::fs::metadata(&path)
                            .and_then(|meta| meta.modified())
                            .ok();
                        hook_paths.push((
                            path,
                            modified
                                .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
                                .map_or(0.0, |since| since.as_secs_f64()),
                        ));
                    }
                }
                if !hook_paths.is_empty() {
                    let _ = hooks.send(hook_paths);
                }
                if (batch.app.is_some() || batch.dev.is_some()) && changes.send(batch).is_err() {
                    return;
                }
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => return,
        }
    }
}

/// Watches a directory of loaded files unless it already is; a failure is reported once.
fn watch_directory(
    watcher: &mut notify::RecommendedWatcher,
    directories: &mut HashSet<PathBuf>,
    failed: &mut HashSet<PathBuf>,
    directory: &Path,
) {
    if directories.contains(directory) {
        return;
    }
    match watcher.watch(directory, notify::RecursiveMode::NonRecursive) {
        Ok(()) => {
            directories.insert(directory.to_path_buf());
        }
        // Gone meanwhile (a checkout in progress): the next report tries again
        Err(error) => {
            if directory.exists() && failed.insert(directory.to_path_buf()) {
                eprintln!(
                    "tenma: --watch cannot watch {}, its files do not reload: {error}",
                    directory.display()
                );
            }
        }
    }
}

/// Something to compare came: wait SETTLE for more, at most MAX_SETTLE since the first.
fn wait_for_more(settle: &mut Option<(Instant, Instant)>) {
    let now = Instant::now();
    let since = settle.map_or(now, |(since, _)| since);
    *settle = Some((since, (now + SETTLE).min(since + MAX_SETTLE)));
}

fn below(roots: &[(PathBuf, bool)], path: &Path) -> bool {
    roots.iter().any(|(root, recursive)| {
        if *recursive {
            path.starts_with(root)
        } else {
            path.parent() == Some(root)
        }
    })
}

/// The path of an event if it is a loaded file or an input below a hook directory. Events may
/// name it by another path than PHP (a symlinked directory such as /tmp on macOS): then the
/// resolved directory decides.
fn relevant(
    loaded: &HashMap<PathBuf, u8>,
    roots: &[(PathBuf, bool)],
    project: &Path,
    path: PathBuf,
) -> Option<PathBuf> {
    let wanted =
        |path: &Path| loaded.contains_key(path) || (below(roots, path) && !ignored(project, path));
    if wanted(&path) {
        return Some(path);
    }
    let resolved = path.parent()?.canonicalize().ok()?.join(path.file_name()?);
    wanted(&resolved).then_some(resolved)
}

/// Below the hooks' directories but no input: a glob like `**/*.php` neither runs a hook for nor
/// reads every file in `vendor/`.
const IGNORED: [&str; 3] = [".git", "node_modules", "vendor"];

/// A path in the project with an ignored directory or file name in it.
fn ignored(project: &Path, path: &Path) -> bool {
    path.strip_prefix(project).is_ok_and(|relative| {
        relative
            .iter()
            .any(|part| IGNORED.iter().any(|name| part == *name))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ignores_git_node_modules_and_vendor_in_the_project() {
        let project = Path::new("/p");
        assert!(ignored(project, Path::new("/p/vendor/a/b.php")));
        assert!(ignored(project, Path::new("/p/x/node_modules/y.js")));
        assert!(!ignored(project, Path::new("/p/src/a.php")));
        assert!(!ignored(project, Path::new("/elsewhere/vendor/a.php")));
    }

    #[test]
    fn a_rescan_compares_every_file() {
        let file = std::env::temp_dir().join(format!("tenma-rescan-{}.php", std::process::id()));
        std::fs::write(&file, "<?php // one").unwrap();
        // A watcher that drops its own events: only the rescan below can find the change
        let watcher = notify::recommended_watcher(|_| {}).unwrap();
        let (sender, messages) = mpsc::channel();
        let (changes, mut receiver) = unbounded_channel();
        let (hooks, _hook_receiver) = unbounded_channel();
        std::thread::spawn(move || watch(watcher, &messages, &changes, &hooks));
        sender
            .send(Message::Loaded(
                file.clone(),
                APP,
                SystemTime::now() + Duration::from_secs(3600),
            ))
            .unwrap();
        // The watcher thread takes its fingerprint first
        std::thread::sleep(Duration::from_millis(100));
        std::fs::write(&file, "<?php // two").unwrap();
        sender
            .send(Message::Event(Ok(notify::Event::new(
                notify::EventKind::Other,
            )
            .set_flag(notify::event::Flag::Rescan))))
            .unwrap();
        let started = Instant::now();
        let changed = loop {
            match receiver.try_recv() {
                Ok(changed) => break changed.app,
                Err(_) if started.elapsed() < Duration::from_secs(3) => {
                    std::thread::sleep(Duration::from_millis(20))
                }
                Err(_) => break None,
            }
        };
        let _ = std::fs::remove_file(&file);
        assert_eq!(changed, Some(file));
    }
}

fn fingerprint(path: &Path) -> Option<u64> {
    Some(fingerprint_of(&std::fs::read(path).ok()?))
}

fn fingerprint_of(content: &[u8]) -> u64 {
    let mut hasher = std::hash::DefaultHasher::new();
    content.hash(&mut hasher);
    hasher.finish()
}

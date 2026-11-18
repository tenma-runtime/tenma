//! The hooks of tenma.json (`tenma --watch`, `tenma build`). Rust checks the list, watches the
//! hooks' directories, delivers the changes and runs command hooks as processes of their own; the
//! stdlib orders the hooks, decides which run and calls the PHP ones (`Tenma\Internal\dev`).

use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::{OnceLock, mpsc};
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncRead, BufReader};

use crate::ops;
use crate::php::{OpResult, bytes};

/// The file `tenma build` reads by default.
pub const FILE: &str = "tenma.json";
/// The key of the hooks in tenma.json.
pub const KEY: &str = "hooks";

/// Signals once the hooks' first run is over (or the dev script failed): the workers start after
/// it, so the first request already finds the hooks' output.
static SETTLED: OnceLock<mpsc::Sender<()>> = OnceLock::new();

/// Whether the tenma.json at `path` lists hooks.
pub fn has_hooks(path: &Path) -> bool {
    std::fs::read(path)
        .ok()
        .and_then(|text| serde_json::from_slice::<serde_json::Value>(&text).ok())
        .is_some_and(|config| config.get(KEY).is_some())
}

/// Checks the hooks of a tenma.json: a list of objects with `inputs`, either `run` (a command) or
/// `php` (a file returning a callable), and optionally `outputs` and `name`.
pub fn check(hooks: &serde_json::Value) -> Result<(), String> {
    let list = hooks.as_array().ok_or("hooks: a list")?;
    for (index, hook) in list.iter().enumerate() {
        let at = format!("hooks[{index}]");
        let hook = hook
            .as_object()
            .ok_or(format!("{at}: an object with inputs and run or php"))?;
        for (key, value) in hook {
            let (fine, wanted) = match key.as_str() {
                "inputs" | "outputs" => (strings(value), "a list of strings"),
                "run" => (
                    strings(value) && value.as_array().is_some_and(|run| !run.is_empty()),
                    "the program and its arguments, a non-empty list of strings",
                ),
                "php" | "name" => (value.is_string(), "a string"),
                _ => return Err(format!("{at}: unknown key {key}")),
            };
            if !fine {
                return Err(format!("{at}: {key}: {wanted}"));
            }
        }
        if !hook.contains_key("inputs") {
            return Err(format!("{at}: inputs is missing"));
        }
        if hook.contains_key("run") == hook.contains_key("php") {
            return Err(format!("{at}: either run (a command) or php (a file)"));
        }
    }
    Ok(())
}

fn strings(value: &serde_json::Value) -> bool {
    value
        .as_array()
        .is_some_and(|items| items.iter().all(serde_json::Value::is_string))
}

/// PHP code that runs the hooks of the tenma.json at `path`: `watch` keeps it running and waiting
/// for changes.
pub fn code(path: &Path, watch: bool) -> Result<String, String> {
    let shown = path.display();
    let text = std::fs::read_to_string(path).map_err(|error| format!("{shown}: {error}"))?;
    let config: serde_json::Value =
        serde_json::from_str(&text).map_err(|error| format!("{shown}: {error}"))?;
    let hooks = config.get(KEY).cloned().unwrap_or(serde_json::json!([]));
    check(&hooks).map_err(|message| format!("{shown}: {message}"))?;
    let directory = path.parent().unwrap_or(Path::new("."));
    Ok(format!(
        "\\Tenma\\Internal\\dev({}, {}, {watch});",
        crate::php_string(&directory.to_string_lossy()),
        crate::php_string(&hooks.to_string())
    ))
}

/// A receiver that hears once the first run of the hooks is over.
pub fn settled_receiver() -> mpsc::Receiver<()> {
    let (sender, receiver) = mpsc::channel();
    let _ = SETTLED.set(sender);
    receiver
}

pub fn settled() {
    if let Some(sender) = SETTLED.get() {
        let _ = sender.send(());
    }
}

struct Command {
    name: String,
    command: Vec<String>,
    directory: PathBuf,
}

fn string(value: &serde_json::Value) -> String {
    value.as_str().unwrap_or_default().to_owned()
}

/// exit() in a PHP hook with --watch: tenma ends with EXIT_STATUS. Apart from php::EXIT, which the
/// workers wait on for exit() in the script.
pub static EXIT: std::sync::LazyLock<tokio::sync::Notify> =
    std::sync::LazyLock::new(tokio::sync::Notify::new);
static EXIT_STATUS: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(-1);

pub fn request_exit(status: u8) {
    EXIT_STATUS.store(i32::from(status), std::sync::atomic::Ordering::Relaxed);
    EXIT.notify_one();
}

/// The status of exit() in a hook, if it ran.
pub fn exit_status() -> Option<u8> {
    u8::try_from(EXIT_STATUS.load(std::sync::atomic::Ordering::Relaxed)).ok()
}

/// A command hook's process group gets SIGTERM, whatever still runs after this SIGKILL.
const KILL_AFTER: Duration = Duration::from_secs(2);
/// A process the hook left in the background may keep its pipes open: no longer waited for.
const OUTPUT_AFTER_EXIT: Duration = Duration::from_millis(100);

#[unsafe(no_mangle)]
extern "C" fn tenma_rs_dev(op: *const u8, op_len: usize, arg: *const u8, arg_len: usize) -> u64 {
    // PHP hooks load their file and classes as they run (autoloader): those files reload the hooks too
    crate::watch::report();
    let arg = bytes(arg, arg_len).to_vec();
    match bytes(op, op_len) {
        // {"roots": [[path, recursive], …], "directory": …}
        b"watch" => {
            let value: serde_json::Value = serde_json::from_slice(&arg).unwrap_or_default();
            let roots: Vec<(PathBuf, bool)> =
                serde_json::from_value(value["roots"].clone()).unwrap_or_default();
            let directory = PathBuf::from(string(&value["directory"]));
            ops::spawn("dev", "watch".into(), async move {
                crate::watch::roots(roots, directory).await;
                OpResult::Null
            })
        }
        b"settled" => {
            settled();
            ops::spawn("dev", "settled".into(), async { OpResult::Null })
        }
        // Changed paths below the hooks' directories and their modification times, all separated
        // by null bytes
        b"next" => ops::spawn("dev", "next".into(), async {
            match crate::watch::hook_changes().await {
                Some(paths) => {
                    let mut fields = Vec::new();
                    for (path, modified) in paths {
                        fields.extend_from_slice(path.as_os_str().as_bytes());
                        fields.push(0);
                        fields.extend_from_slice(modified.to_string().as_bytes());
                        fields.push(0);
                    }
                    OpResult::Text(fields)
                }
                None => std::future::pending().await,
            }
        }),
        // {"name": …, "command": [program, argument, …], "directory": …}
        b"run" => {
            let value: serde_json::Value = serde_json::from_slice(&arg).unwrap_or_default();
            let command = Command {
                name: string(&value["name"]),
                command: value["command"]
                    .as_array()
                    .map(|parts| parts.iter().map(string).collect())
                    .unwrap_or_default(),
                directory: string(&value["directory"]).into(),
            };
            let detail = command.name.clone();
            ops::spawn("dev", detail, run(command))
        }
        _ => 0,
    }
}

/// Ends a command hook's processes once the hook is over or dropped (Ctrl+C, reload), also those
/// it started itself (`sh -c`, background jobs): SIGTERM to its process group, SIGKILL after
/// KILL_AFTER to whatever is left.
struct Group(i32);

impl Drop for Group {
    fn drop(&mut self) {
        let group = -self.0;
        // Fails once nothing of the group is left, the usual case
        if unsafe { libc::kill(group, libc::SIGTERM) } == 0 {
            // ponytail: if tenma exits meanwhile, only SIGTERM was sent
            std::thread::spawn(move || {
                std::thread::sleep(KILL_AFTER);
                unsafe { libc::kill(group, libc::SIGKILL) };
            });
        }
    }
}

/// Runs a command hook without a shell; its output appears with the hook's name in front. The
/// exit status is the result.
async fn run(command: Command) -> OpResult {
    let Some((program, arguments)) = command.command.split_first() else {
        return OpResult::Error("empty command".into());
    };
    // Relative to the project, not to tenma's working directory
    let program = if program.contains('/') {
        command.directory.join(program)
    } else {
        PathBuf::from(program)
    };
    let child = tokio::process::Command::new(&program)
        .args(arguments)
        .current_dir(&command.directory)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .process_group(0)
        .spawn();
    let mut child = match child {
        Ok(child) => child,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return OpResult::Error(format!("{} not found", program.display()));
        }
        Err(error) => return OpResult::Error(format!("{}: {error}", program.display())),
    };
    let _group = child.id().map(|pid| Group(pid as i32));
    let stdout = prefixed(child.stdout.take(), command.name.clone(), false);
    let stderr = prefixed(child.stderr.take(), command.name, true);
    let output = tokio::spawn(async move { tokio::join!(stdout, stderr) });
    let status = child.wait().await;
    let _ = tokio::time::timeout(OUTPUT_AFTER_EXIT, output).await;
    match status {
        Ok(status) => OpResult::Text(status.code().unwrap_or(1).to_string().into_bytes()),
        Err(error) => OpResult::Error(error.to_string()),
    }
}

async fn prefixed(pipe: Option<impl AsyncRead + Unpin>, name: String, error: bool) {
    let Some(pipe) = pipe else { return };
    // Bytes, not lines(): that fails on output that is no UTF-8, and the closed pipe kills the hook
    let mut pipe = BufReader::new(pipe);
    let mut buffer = Vec::new();
    while pipe
        .read_until(b'\n', &mut buffer)
        .await
        .is_ok_and(|read| read > 0)
    {
        let line = String::from_utf8_lossy(&buffer);
        let line = line.trim_end_matches(['\n', '\r']);
        if error {
            eprintln!("[{name}] {line}");
        } else {
            println!("[{name}] {line}");
        }
        buffer.clear();
    }
}

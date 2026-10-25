//! Tenma: PHP as a long-running process. Each worker is a thread with its own PHP interpreter
//! and its own event loop; the script runs once per worker, then the registered servers handle
//! requests until Ctrl+C. The main thread accepts connections and hands them to the workers
//! round-robin.
//!
//! A fatal PHP error only breaks the interpreter of one worker: the worker restarts it and runs
//! the script again. Ctrl+C or SIGTERM stop accepting connections and let running requests
//! finish. With --watch, all workers restart that way when a file the script loaded changes.

mod composer;
mod db;
mod dev;
mod durable;
mod files;
mod fs;
mod inspect;
mod journal;
mod net;
mod ops;
mod pdo;
mod php;
mod plugins;
mod pubsub;
mod replay;
mod stdlib;
mod streams;
mod tls;
mod watch;

use std::cell::{Cell, RefCell};
use std::collections::{BTreeSet, HashMap};
use std::convert::Infallible;
use std::process::ExitCode;
use std::rc::Rc;
use std::sync::{Arc, OnceLock, mpsc};
use std::thread::JoinHandle;
use std::time::Duration;

use bytes::Bytes;
use http_body_util::{BodyExt, Either, Full};
use hyper::body::Incoming;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Notify;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};
use tokio::task::LocalSet;

const USAGE: &str = "Usage: tenma [--watch] [--inspect[-wait|-brk][=[HOST:]PORT]] [--workers N|auto] [--max-body SIZE] [--max-requests N] [--db-pool N] [-d name=value] [-c php.ini] [-r code] [--server-name NAME] [--request-timeout SECONDS] [--max-connections N] [--pubsub redis://HOST] [--pdo-pgsql native] [--durable postgres://URL] [--plugin FILE] [--config tenma.json] [--stop-timeout SECONDS] [--record DIR [--record-on error] [--record-max-size SIZE] [--record-max-body SIZE] [--record-redact HEADER,...]] [--tls-cert FILE --tls-key FILE [--tls-port N]] <script.php> [arguments …]\n       tenma build [tenma.json]   (runs the hooks of tenma.json once)\n       tenma check [script.php|tenma.json]   (checks the options, hooks and plugins of tenma.json without starting)\n       tenma --stubs [DIR]   (the PHP API as stubs for IDEs and PHPStan; with DIR one file per class)\n       tenma install|update|remove composer   (Composer for tenma composer)\n       tenma composer [arguments …]   (runs Composer)\n       tenma lsp   (the language server for editors, on stdin and stdout)\n       tenma docs [<path>...] [--out <dir>]   (Markdown reference pages for PHP classes and functions; without paths for Tenma's API)\n       tenma replay FILE [--until N | --request N] [--check]   (plays a recorded journal into a fresh interpreter, without I/O)\n       tenma replay FILE --list   (the requests of a journal, numbered for --request)\n       tenma --version";
/// How long running requests may take to finish on shutdown or restart.
const DRAIN: Duration = Duration::from_secs(10);
/// With --watch: an open SSE or WebSocket connection should not hold up every reload for 10 s.
const WATCH_DRAIN: Duration = Duration::from_secs(1);
/// How long the script waits for the first run of the hooks: a hook that never ends (a command
/// with its own --watch) must not keep the app from starting.
const HOOKS_FIRST_RUN: Duration = Duration::from_secs(10);
/// PHP expects the large stack of a main thread, not the 2 MB of a Rust thread.
const WORKER_STACK: usize = 8 * 1024 * 1024;

/// An accepted connection and the port it came in on.
type Connection = (u16, std::net::TcpStream);

/// What a worker reports after running the script: its servers or the exit status.
type Ready = Result<Vec<php::Listener>, u8>;

struct Worker {
    connections: UnboundedSender<Connection>,
    /// --watch: a file changed, the worker runs the script again.
    change: Arc<Notify>,
    /// Returns the exit status when a script without servers has finished.
    thread: JoinHandle<u8>,
}

#[derive(Clone, Copy)]
struct Options {
    workers: usize,
    /// Largest request body in bytes, 413 above that.
    max_body: usize,
    /// After this many requests a worker restarts its interpreter, against slow leaks.
    max_requests: Option<u64>,
    /// How long the headers and body of a request may take to arrive (against Slowloris), and how
    /// long a write to a client may stall.
    request_timeout: Duration,
    /// Open connections per worker; more get 503.
    max_connections: usize,
    /// Reload when a file the script loaded changes.
    watch: bool,
    /// How long running requests may take to finish on shutdown or restart.
    drain: Duration,
    /// --inspect: the address of the debugger (DAP).
    inspect: Option<std::net::SocketAddr>,
    /// --inspect-wait (`Some(false)`) and --inspect-brk (`Some(true)`): wait for the debugger, and
    /// whether the script then stops at its first statement unless the editor decides.
    inspect_wait: Option<bool>,
    /// --record: every worker writes its journal there.
    record: Option<&'static journal::Settings>,
    /// `tenma replay`: the one worker plays this instead of serving.
    replay: Option<&'static replay::Plan>,
    /// How long the stop handlers of Process\Shutdown may take when tenma stops.
    stop_timeout: Duration,
}

/// Why a worker's event loop ends.
enum Stop {
    /// No more connections: tenma shuts down.
    Closed,
    /// Restart the interpreter and run the script again.
    Restart,
    /// --watch: the script failed or ended, run it again after the next change.
    Wait,
}

thread_local! {
    /// Wakes the worker's event loop so it restarts its interpreter.
    static RESTART: Rc<Notify> = Rc::new(Notify::new());
    /// --watch: a file changed. Apart from RESTART, which a fatal error or --max-requests sends
    /// and which does not outlive the interpreter it was meant for.
    static CHANGE: RefCell<Arc<Notify>> = RefCell::new(Arc::new(Notify::new()));
    /// After a fatal error: this thread's PHP must not run again until the restart.
    static BROKEN: Cell<bool> = const { Cell::new(false) };
    /// Requests since the interpreter last started.
    static HANDLED: Cell<u64> = const { Cell::new(0) };
}

/// This worker's interpreter is unusable after a fatal error: restart it right away.
pub fn interpreter_broken() {
    eprintln!("tenma: fatal PHP error, restarting worker");
    BROKEN.set(true);
    journal::record_fatal();
    // Their fibers are gone with the interpreter: pending requests get their 500 and streams end
    // now, not after the drain
    php::forget_waiting();
    streams::clear();
    RESTART.with(|restart| restart.notify_one());
}

pub fn broken() -> bool {
    BROKEN.get()
}

/// The worker's event loop should end and its interpreter start over: a replay stepping back.
pub fn request_restart() {
    RESTART.with(|restart| restart.notify_one());
}

fn main() -> ExitCode {
    // Composer runs @php and Symfony Process runs PHP through PHP_BINARY: point it at tenma unless
    // set. Before any thread, so setting the environment is safe
    if std::env::var_os("PHP_BINARY").is_none()
        && let Ok(binary) = std::env::current_exe()
    {
        unsafe { std::env::set_var("PHP_BINARY", binary) };
    }
    match std::env::args().nth(1).as_deref() {
        Some("--stubs") => return stubs(),
        Some("lsp") => return tool(stdlib::Tool::Lsp, "Lsp"),
        Some("docs") => return tool(stdlib::Tool::Docs, "Docs"),
        Some("check") => return check(),
        Some(action @ ("install" | "update" | "remove"))
            if std::env::args().nth(2).as_deref() == Some("composer") =>
        {
            return match composer::manage(action) {
                Ok(message) => {
                    println!("tenma: {message}");
                    ExitCode::SUCCESS
                }
                Err(message) => {
                    eprintln!("tenma: {message}");
                    ExitCode::FAILURE
                }
            };
        }
        Some("--version" | "-v") => {
            println!(
                "tenma {} (PHP {})",
                env!("CARGO_PKG_VERSION"),
                env!("TENMA_PHP_VERSION")
            );
            return ExitCode::SUCCESS;
        }
        _ => {}
    }
    let mut given: Vec<String> = std::env::args().skip(1).collect();
    // tenma composer …: the installed Composer runs as the script, the rest are its arguments
    if given.first().is_some_and(|first| first == "composer") {
        match composer::script() {
            Ok(path) => given[0] = path,
            Err(message) => {
                eprintln!("tenma: {message}");
                return ExitCode::FAILURE;
            }
        }
    }
    let (argv, options) = match parse_args(&given) {
        Ok(parsed) => parsed,
        Err(message) => {
            eprintln!("{message}\n{USAGE}");
            return ExitCode::from(2);
        }
    };
    streams::MAX_MESSAGE.store(options.max_body, std::sync::atomic::Ordering::Relaxed);
    // A replay runs with the PHP settings of the recording: code that asks for tenma.watch or a -d
    // setting takes the same branches
    if argv.first().is_some_and(|first| first == "replay")
        && let Some(file) = argv.get(1)
    {
        match replay::recorded_settings(file) {
            Ok((watch, defines, ini_path)) => {
                for define in defines {
                    let _ = php::define_ini(&define);
                }
                if let Some(path) = ini_path {
                    let _ = php::set_ini_path(&path);
                }
                if watch {
                    watch::configure();
                }
            }
            Err(message) => {
                eprintln!("tenma: {message}");
                return ExitCode::from(2);
            }
        }
    }
    if options.watch {
        watch::configure();
    }
    if options.inspect.is_some() {
        inspect::configure();
    }
    if let Err(message) = php::start() {
        eprintln!("tenma: {message}");
        return ExitCode::FAILURE;
    }
    // Before the editor connects: its initialize asks whether stepping back works
    if options.inspect.is_some() && argv.first().is_some_and(|first| first == "replay") {
        inspect::replaying();
    }
    if let Some(address) = options.inspect
        && let Err(message) = inspect::listen(address, options.inspect_wait)
    {
        eprintln!("tenma: {message}");
        php::stop();
        return ExitCode::FAILURE;
    }
    let status = run(&argv, options);
    // Here and not in run(): a script without servers and a failed start end there early
    plugins::shutdown();
    php::stop();
    ExitCode::from(status)
}

/// `tenma --stubs [DIR]`: loads the standard library without a script and prints its public API as
/// stubs, or writes them to DIR as one file per class.
fn stubs() -> ExitCode {
    let directory = match std::env::args().nth(2) {
        Some(directory) => php_string(&directory),
        None => "null".to_owned(),
    };
    let call = format!("\\Tenma\\Internal\\Stubs\\main({directory});");

    let version = format!(
        "namespace Tenma\\Internal\\Stubs; const VERSION = '{}';",
        env!("CARGO_PKG_VERSION")
    );
    // On the main thread php_embed_init() has already started a request: no thread_start() here
    let printed = php::start().and_then(|()| {
        stdlib::load(Some(stdlib::Tool::Stubs))
            .and_then(|()| php::eval(&version, "tenma:version"))
            .and_then(|()| php::eval(&call, "tenma:stubs-main"))
    });
    php::stop();
    match printed {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("tenma: {message}");
            ExitCode::FAILURE
        }
    }
}

/// A PHP string literal in single quotes.
fn php_string(text: &str) -> String {
    let escaped = text.replace('\\', "\\\\").replace('\'', "\\'");
    format!("'{escaped}'")
}

/// A command written in PHP: `tenma lsp`, the language server of stdlib/Tenma/Internal/Lsp on stdin and
/// stdout (`tenma lsp --worker DIR` is its own child process, the one that loads the project), and
/// `tenma docs` of stdlib/Tenma/Internal/Docs. Runs `Tenma\Internal\<namespace>\main($binary, $args)`
/// after the standard library.
fn tool(tool: stdlib::Tool, namespace: &str) -> ExitCode {
    let binary = std::env::current_exe().map_or_else(
        |_| "tenma".to_owned(),
        |path| path.to_string_lossy().into_owned(),
    );
    let args: Vec<String> = std::env::args()
        .skip(2)
        .map(|arg| php_string(&arg))
        .collect();
    let setup = format!(
        "namespace Tenma\\Internal\\{namespace}; const VERSION = '{}';",
        env!("CARGO_PKG_VERSION")
    );
    // A RuntimeException is the command refusing (a bad option, a page it must not overwrite): one
    // line on stderr and status 1, not an uncaught exception with its trace
    let command = namespace.to_lowercase();
    let call = format!(
        "try {{ \\Tenma\\Internal\\{namespace}\\main({}, [{}]); }} \
         catch (\\RuntimeException $error) {{ \
         file_put_contents('php://stderr', 'tenma {command}: ' . $error->getMessage() . PHP_EOL); exit(1); }}",
        php_string(&binary),
        args.join(", ")
    );
    // stdout carries the protocol: PHP's warnings go to stderr, which editors show in their log
    let ran = php::define_ini("display_errors=stderr")
        .and_then(|()| php::start())
        .and_then(|()| {
            stdlib::load(Some(tool))
                .and_then(|()| php::eval(&setup, "tenma:version"))
                .and_then(|()| {
                    php::eval(&call, &format!("tenma:{}-main", namespace.to_lowercase()))
                })
        });
    let status = php::php_exit_status();
    php::stop();
    match ran {
        Ok(()) => ExitCode::SUCCESS,
        // exit() or a fatal error: PHP has said why
        Err(_) if status != 0 => ExitCode::from(status),
        Err(message) => {
            eprintln!("tenma: {message}");
            ExitCode::FAILURE
        }
    }
}

/// Options for tenma, then the script; everything after it belongs to the script ($argv), as with `php script.php …`.
fn parse_args(given: &[String]) -> Result<(Vec<String>, Options), String> {
    let mut args = with_config(given)?.into_iter();
    let mut options = Options {
        workers: 1,
        max_body: 10 << 20,
        max_requests: None,
        request_timeout: Duration::from_secs(30),
        max_connections: 1024,
        watch: false,
        drain: DRAIN,
        inspect: None,
        inspect_wait: None,
        record: None,
        replay: None,
        stop_timeout: Duration::from_secs(10),
    };
    let mut argv = Vec::new();
    let mut pool = None;
    let mut certificate = None;
    let mut key = None;
    let mut tls_ports = Vec::new();
    let mut record: Option<journal::Settings> = None;
    let mut plugins = Vec::new();
    while let Some(arg) = args.next() {
        let mut value = |name: &str| args.next().ok_or(format!("{name} needs a value"));
        match arg.as_str() {
            "--watch" => {
                options.watch = true;
                options.drain = WATCH_DRAIN;
            }
            "--inspect" | "--inspect-wait" | "--inspect-brk" => {
                options.inspect = Some(inspect::DEFAULT);
                options.inspect_wait = inspect::wait(&arg);
            }
            // --inspect=9230 or --inspect=host:port
            inspect
                if matches!(
                    inspect.split_once('='),
                    Some(("--inspect" | "--inspect-wait" | "--inspect-brk", _))
                ) =>
            {
                let (option, address) = inspect.split_once('=').unwrap_or_default();
                options.inspect_wait = inspect::wait(option);
                let parsed = match address.parse::<u16>() {
                    Ok(port) => Some(std::net::SocketAddr::new(inspect::DEFAULT.ip(), port)),
                    Err(_) => std::net::ToSocketAddrs::to_socket_addrs(address)
                        .ok()
                        .and_then(|mut addresses| addresses.next()),
                };
                options.inspect =
                    Some(parsed.ok_or(format!("--inspect: not an address: {address}"))?);
            }
            "--workers" | "-w" => {
                options.workers = match value("--workers")?.as_str() {
                    "auto" => std::thread::available_parallelism().map_or(1, usize::from),
                    number => number
                        .parse()
                        .ok()
                        .filter(|&count| count > 0)
                        .ok_or(format!("--workers {number}: not a number above 0"))?,
                };
            }
            "--max-body" => options.max_body = parse_size(&value("--max-body")?)?,
            "--db-pool" => {
                let number = value("--db-pool")?;
                let size = number
                    .parse()
                    .ok()
                    .filter(|&count: &usize| count > 0)
                    .ok_or(format!("--db-pool {number}: not a number above 0"))?;
                pool = Some(size);
            }
            // As with php: -d name=value or -dname=value, -c path
            "-d" => php::define_ini(&value("-d")?)?,
            define if define.starts_with("-d") => php::define_ini(&define[2..])?,
            "-c" => php::set_ini_path(&value("-c")?)?,
            // As php -r: the code instead of a script, the rest are its arguments
            "-r" => {
                let _ = CODE.set(value("-r")?);
                argv.push("Standard input code".to_owned());
                argv.extend(args);
                break;
            }
            "--request-timeout" => {
                let seconds = value("--request-timeout")?;
                options.request_timeout = seconds
                    .parse::<f64>()
                    .ok()
                    .filter(|&s| s > 0.0)
                    .and_then(|s| Duration::try_from_secs_f64(s).ok())
                    .ok_or(format!("--request-timeout {seconds}: not a number above 0"))?;
            }
            "--max-connections" => {
                let number = value("--max-connections")?;
                options.max_connections = number
                    .parse()
                    .ok()
                    .filter(|&count| count > 0)
                    .ok_or(format!("--max-connections {number}: not a number above 0"))?;
            }
            "--server-name" => php::set_server_name(&value("--server-name")?)?,
            "--pubsub" => pubsub::use_redis(&value("--pubsub")?)?,
            "--record" => {
                let directory = value("--record")?;
                record.get_or_insert_with(Default::default).directory = directory.into();
            }
            "--record-on" => match value("--record-on")?.as_str() {
                "error" => record.get_or_insert_with(Default::default).only_on_error = true,
                "always" => record.get_or_insert_with(Default::default).only_on_error = false,
                other => return Err(format!("--record-on {other}: error or always")),
            },
            "--record-max-size" => {
                record.get_or_insert_with(Default::default).max_size =
                    parse_size(&value("--record-max-size")?)? as u64;
            }
            "--record-max-body" => {
                record.get_or_insert_with(Default::default).max_body =
                    parse_size(&value("--record-max-body")?)?;
            }
            "--record-redact" => {
                record.get_or_insert_with(Default::default).redact = value("--record-redact")?
                    .split(',')
                    .map(|name| name.trim().to_ascii_lowercase())
                    .filter(|name| !name.is_empty())
                    .collect();
            }
            "--plugin" => plugins.push(value("--plugin")?),
            // Already applied by with_config
            "--config" => {
                value("--config")?;
            }
            "--stop-timeout" => {
                let seconds = value("--stop-timeout")?;
                options.stop_timeout = seconds
                    .parse::<f64>()
                    .ok()
                    .filter(|&s| s >= 0.0)
                    .and_then(|s| Duration::try_from_secs_f64(s).ok())
                    .ok_or(format!("--stop-timeout {seconds}: not a number of seconds"))?;
            }
            "--durable" => {
                let url = value("--durable")?;
                if !url.starts_with("postgres://") && !url.starts_with("postgresql://") {
                    return Err(format!("--durable {url}: a postgres:// URL"));
                }
                durable::configure(&url);
            }
            "--pdo-pgsql" => match value("--pdo-pgsql")?.as_str() {
                "native" => php::pdo_native(),
                "tenma" => {}
                other => return Err(format!("--pdo-pgsql {other}: native or tenma")),
            },
            "--tls-cert" => certificate = Some(value("--tls-cert")?),
            "--tls-key" => key = Some(value("--tls-key")?),
            "--tls-port" => {
                let number = value("--tls-port")?;
                let port = number
                    .parse()
                    .ok()
                    .filter(|&port: &u16| port > 0)
                    .ok_or(format!("--tls-port {number}: not a port"))?;
                tls_ports.push(port);
            }
            "--max-requests" => {
                let number = value("--max-requests")?;
                options.max_requests = Some(
                    number
                        .parse()
                        .ok()
                        .filter(|&count| count > 0)
                        .ok_or(format!("--max-requests {number}: not a number above 0"))?,
                );
            }
            option if option.starts_with('-') => return Err(format!("Unknown option: {option}")),
            _ => {
                argv.push(arg);
                argv.extend(args);
                break;
            }
        }
    }
    if argv.is_empty() {
        return Err("No script given".into());
    }
    // Requests would land in different interpreters; while debugging that only confuses
    if options.inspect.is_some() && options.workers > 1 {
        eprintln!("tenma: --inspect runs one worker");
        options.workers = 1;
    }
    match (certificate, key) {
        (Some(certificate), Some(key)) => tls::configure(certificate, key, tls_ports)?,
        (None, None) if tls_ports.is_empty() => {}
        (None, None) => return Err("--tls-port needs --tls-cert and --tls-key".into()),
        _ => return Err("--tls-cert and --tls-key go together".into()),
    }
    // About 20 connections in all by default, Postgres allows 100: ten workers with ten each ran out of them
    db::POOL_SIZE.store(
        pool.unwrap_or((20 / options.workers).clamp(2, 10)),
        std::sync::atomic::Ordering::Relaxed,
    );
    if !plugins.is_empty() {
        plugins::load(&plugins)?;
    }
    if let Some(settings) = record {
        if settings.directory.as_os_str().is_empty() {
            return Err(
                "--record-on, --record-max-body and --record-redact need --record DIR".into(),
            );
        }
        options.record = Some(Box::leak(Box::new(settings)));
    }
    Ok((argv, options))
}

/// `tenma check [script.php | tenma.json]`: reads the tenma.json the script gets (default
/// index.php), checks its options and loads its plugins as a start would, without running PHP. The
/// first problem ends it with status 1.
fn check() -> ExitCode {
    let target = std::env::args()
        .nth(2)
        .unwrap_or_else(|| "index.php".to_owned());
    let path = std::path::Path::new(&target);
    let config = if path
        .extension()
        .is_some_and(|extension| extension == "json")
    {
        path.to_path_buf()
    } else {
        path.parent()
            .filter(|directory| !directory.as_os_str().is_empty())
            .unwrap_or(std::path::Path::new("."))
            .join("tenma.json")
    };
    // The usual slip: a tenma.json written in the working directory, next to another script
    let here = std::fs::canonicalize("tenma.json").ok();
    if here.is_some() && here != std::fs::canonicalize(&config).ok() {
        eprintln!("tenma check: warning: ./tenma.json is not the one {target} gets");
    }
    if !config.is_file() {
        println!("tenma check: no {}, nothing to check", config.display());
        return ExitCode::SUCCESS;
    }

    let given = [
        "--config".to_owned(),
        config.display().to_string(),
        target.clone(),
    ];
    match parse_args(&given) {
        Ok(_) => {
            println!("tenma check: {} is fine", config.display());
            ExitCode::SUCCESS
        }
        Err(message) => {
            eprintln!("tenma check: {message}");
            ExitCode::FAILURE
        }
    }
}

/// `-r`: the code that runs instead of a script.
static CODE: OnceLock<String> = OnceLock::new();

/// The tenma.json the script got, if any: its hooks run with --watch.
static CONFIG: OnceLock<std::path::PathBuf> = OnceLock::new();

/// The options of tenma.json (next to the script, or `--config FILE`) put in front of the command line
/// as arguments: the command line wins for single values, lists add up. Keys are the long option
/// names; `plugins` and `d` take lists, `tls-port` too. Paths in the file count from its directory.
/// `hooks` are no option: they are checked here and run with --watch (src/dev.rs).
fn with_config(given: &[String]) -> Result<Vec<String>, String> {
    let mut file = None;
    let mut script = None;
    let mut i = 0;
    while i < given.len() {
        let arg = &given[i];
        if arg == "--config" {
            file = given.get(i + 1).cloned();
            i += 2;
            continue;
        }
        if !arg.starts_with('-') {
            script = Some(arg.clone());
            break;
        }
        // Code instead of a script: no tenma.json belongs to it
        if arg == "-r" {
            break;
        }
        // Options with a value, as parse_args reads them
        let takes_value = !matches!(arg.as_str(), "--watch" | "--stubs")
            && !arg.starts_with("--inspect")
            && !(arg.starts_with("-d") && arg.len() > 2);
        i += if takes_value { 2 } else { 1 };
    }
    let path = match (file, script) {
        (Some(file), _) => std::path::PathBuf::from(file),
        (None, Some(script)) if !matches!(script.as_str(), "build" | "lsp" | "docs" | "replay") => {
            let next_to = std::path::Path::new(&script)
                .parent()
                .map_or_else(
                    || std::path::PathBuf::from("."),
                    std::path::Path::to_path_buf,
                )
                .join("tenma.json");
            if !next_to.is_file() {
                return Ok(given.to_vec());
            }
            next_to
        }
        _ => return Ok(given.to_vec()),
    };
    let _ = CONFIG.set(path.clone());
    let text =
        std::fs::read_to_string(&path).map_err(|error| format!("{}: {error}", path.display()))?;
    let config: serde_json::Map<String, serde_json::Value> =
        serde_json::from_str(&text).map_err(|error| format!("{}: {error}", path.display()))?;
    let directory = path.parent().map_or_else(
        || std::path::PathBuf::from("."),
        std::path::Path::to_path_buf,
    );
    let relative = |value: &str| -> String {
        if value.starts_with('/') {
            value.to_owned()
        } else {
            directory.join(value).display().to_string()
        }
    };
    // A single value the command line also gives: the file's one is left out, so options that take
    // only one value (--pubsub, --durable, --pdo-pgsql) see the command line's
    let on_command_line: Vec<&str> = given
        .iter()
        .filter_map(|arg| arg.strip_prefix("--"))
        .collect();
    let mut front = Vec::new();
    for (key, value) in &config {
        if !matches!(key.as_str(), "plugins" | "tls-port" | "d")
            && on_command_line.contains(&key.as_str())
        {
            continue;
        }
        // -d and -c as on the command line, the rest as long options
        let option = if key.len() == 1 {
            format!("-{key}")
        } else {
            format!("--{key}")
        };
        let is_path = matches!(
            key.as_str(),
            "plugins" | "tls-cert" | "tls-key" | "c" | "record"
        );
        let scalar = |value: &serde_json::Value| -> Result<String, String> {
            match value {
                serde_json::Value::String(text) => Ok(if is_path {
                    relative(text)
                } else {
                    text.clone()
                }),
                serde_json::Value::Number(number) => Ok(number.to_string()),
                serde_json::Value::Bool(flag) => Ok(flag.to_string()),
                other => Err(format!("{}: {key}: not a value: {other}", path.display())),
            }
        };
        match key.as_str() {
            "workers" | "max-body" | "max-requests" | "max-connections" | "request-timeout"
            | "db-pool" | "pubsub" | "pdo-pgsql" | "durable" | "server-name" | "tls-cert"
            | "tls-key" | "c" | "record" | "record-on" | "record-max-body" | "record-max-size"
            | "record-redact" | "stop-timeout" => {
                front.push(option);
                front.push(scalar(value)?);
            }
            dev::KEY => {
                dev::check(value).map_err(|message| format!("{}: {message}", path.display()))?
            }
            "watch" => {
                if value.as_bool() == Some(true) {
                    front.push(option);
                }
            }
            "plugins" | "tls-port" | "d" => {
                let option = if key == "plugins" {
                    "--plugin".to_owned()
                } else {
                    option
                };
                for item in value
                    .as_array()
                    .ok_or(format!("{}: {key}: a list", path.display()))?
                {
                    front.push(option.clone());
                    front.push(scalar(item)?);
                }
            }
            other => return Err(format!("{}: unknown key {other}", path.display())),
        }
    }
    front.extend(given.iter().cloned());
    Ok(front)
}

/// Bytes with an optional unit: 512, 64k, 10m, 1g.
fn parse_size(text: &str) -> Result<usize, String> {
    let lower = text.to_ascii_lowercase();
    let unit = |suffix: char| lower.strip_suffix(suffix);
    let (number, shift) = if let Some(number) = unit('k') {
        (number, 10)
    } else if let Some(number) = unit('m') {
        (number, 20)
    } else if let Some(number) = unit('g') {
        (number, 30)
    } else {
        (lower.as_str(), 0)
    };
    number
        .parse::<usize>()
        .ok()
        .and_then(|count| count.checked_mul(1 << shift))
        .ok_or(format!("Not a size: {text}"))
}

/// `argv[0]` is the script, the rest are its arguments.
fn run(argv: &[String], options: Options) -> u8 {
    if argv[0] == "build" {
        return build(argv.get(1).map_or(dev::FILE, String::as_str), options);
    }
    if argv[0] == "replay" {
        if argv.get(2).is_some_and(|arg| arg == "--list") {
            return match argv.get(1).map(|file| replay::list(file)) {
                Some(Ok(listing)) => {
                    print!("{listing}");
                    0
                }
                Some(Err(message)) => {
                    eprintln!("tenma: {message}");
                    2
                }
                None => 2,
            };
        }
        return replay_journal(&argv[1..], options);
    }
    let mut changes = if options.watch {
        match watch::start() {
            Ok(changes) => Some(changes),
            Err(message) => {
                eprintln!("tenma: {message}");
                return 1;
            }
        }
    } else {
        None
    };
    let script = std::path::PathBuf::from(&argv[0]);
    let dev_file = changes
        .as_ref()
        .and(CONFIG.get())
        .and_then(|path| path.canonicalize().ok())
        .filter(|path| dev::has_hooks(path));
    // What the last run published is stale: the script and the hooks publish again what is still wrong
    if changes.is_some() {
        watch::clear_diagnostics(&script, "app");
    }
    if let Some(path) = &dev_file {
        watch::clear_diagnostics(path, "dev");
    }
    // The hooks first: the first request should find what they write
    let dev_worker = dev_file.clone().map(|path| {
        watch::hooks_file(&path);
        let settled = dev::settled_receiver();
        let (worker, ready) = spawn_worker(
            &[path.to_string_lossy().into_owned()],
            options,
            Some(BTreeSet::new()),
            true,
        );
        if ready.is_ok()
            && settled.recv_timeout(HOOKS_FIRST_RUN) == Err(mpsc::RecvTimeoutError::Timeout)
        {
            eprintln!(
                "tenma: hooks still running after {} s, starting the script anyway",
                HOOKS_FIRST_RUN.as_secs()
            );
        }
        worker
    });
    // The first worker runs the script alone. If it registers no server, it was an ordinary
    // script and runs only once; with --watch, the worker runs it again after every change.
    let (first, ready) = spawn_worker(argv, options, None, false);
    let listeners = match ready {
        Ok(listeners) => listeners,
        Err(status) => {
            stop_workers([first].into_iter().chain(dev_worker).collect());
            return status;
        }
    };
    // A script without servers: its status, or that of exit() in timers and background work
    if listeners.is_empty() && !options.watch {
        return first.thread.join().unwrap_or(1);
    }
    let ports: BTreeSet<u16> = listeners.iter().map(|listener| listener.port).collect();
    let serving = !ports.is_empty();
    let mut workers = vec![first];
    for _ in 1..if serving { options.workers } else { 1 } {
        let (worker, ready) = spawn_worker(argv, options, Some(ports.clone()), false);
        workers.push(worker);
        if let Err(status) = ready {
            // Collect all workers first: shutting PHP down while one is still running would crash
            workers.extend(dev_worker);
            stop_workers(workers);
            return status;
        }
    }

    let senders: Vec<_> = workers
        .iter()
        .map(|worker| worker.connections.clone())
        .collect();
    let changed: Vec<_> = workers.iter().map(|worker| worker.change.clone()).collect();
    let dev_changed = dev_worker.as_ref().map(|worker| worker.change.clone());
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("tokio does not start");
    runtime.block_on(async move {
        for &port in tls::ports() {
            if !ports.contains(&port) {
                eprintln!("tenma: --tls-port {port}: the script serves no such port");
            }
        }
        if tls::enabled() {
            tokio::spawn(reload_certificate_on_hangup());
        }
        for port in ports {
            tokio::spawn(accept(port, senders.clone()));
        }
        let shutdown = shutdown_signal();
        tokio::pin!(shutdown);
        loop {
            tokio::select! {
                () = &mut shutdown => break,
                // exit() outside a request also ends a server; a script without servers waits for changes
                () = php::EXIT.notified(), if serving => {
                    eprintln!("tenma: exit() outside a request, shutting down");
                    break;
                }
                () = dev::EXIT.notified(), if dev_changed.is_some() => {
                    eprintln!("tenma: exit() in a hook, shutting down");
                    break;
                }
                Some(batch) = next_change(&mut changes) => {
                    if let (Some(path), Some(change)) = (batch.dev, &dev_changed) {
                        eprintln!("tenma: {} changed, reloading the hooks", shown(&path));
                        if let Some(file) = &dev_file {
                            watch::clear_diagnostics(file, "dev");
                        }
                        change.notify_one();
                    }
                    if let Some(path) = batch.app {
                        eprintln!("tenma: {} changed, reloading", shown(&path));
                        watch::clear_diagnostics(&script, "app");
                        watch::reloaded_by(&path);
                        for change in &changed {
                            change.notify_one();
                        }
                    }
                }
            }
        }
    });
    drop(runtime);
    inspect::release();
    workers.extend(dev_worker);
    stop_workers(workers);
    dev::exit_status().or(php::exit_status()).unwrap_or(0)
}

/// `tenma replay FILE`: one worker plays the journal with the recorded script, environment and
/// directory; no port opens.
fn replay_journal(args: &[String], mut options: Options) -> u8 {
    let plan = match replay::plan(args) {
        Ok(plan) => plan,
        Err(message) => {
            eprintln!("tenma: {message}");
            return 2;
        }
    };
    let (argv, env, cwd) = match replay::recorded_run(&plan) {
        Ok(run) => run,
        Err(message) => {
            eprintln!("tenma: {message}");
            return 2;
        }
    };
    // The script sees what it saw then: before any worker thread, so setting the environment is safe
    for (key, value) in env {
        unsafe { std::env::set_var(key, value) };
    }
    if !cwd.is_empty() && std::env::set_current_dir(&cwd).is_err() {
        eprintln!("tenma: cannot change to the recorded directory {cwd}");
    }
    options.replay = Some(Box::leak(Box::new(plan)));
    options.workers = 1;
    let (worker, ready) = spawn_worker(&argv, options, Some(BTreeSet::new()), false);
    if let Err(status) = ready {
        stop_workers(vec![worker]);
        return status;
    }
    let status = worker.thread.join().unwrap_or(1);
    inspect::release();
    status
}

/// `tenma build`: runs the hooks of a tenma.json once, in order; 1 if one fails.
fn build(path: &str, options: Options) -> u8 {
    let Ok(path) = std::path::Path::new(path).canonicalize() else {
        eprintln!("tenma: {path} not found");
        return 1;
    };
    let (worker, ready) = spawn_worker(&[path.to_string_lossy().into_owned()], options, None, true);
    let status = worker.thread.join().unwrap_or(1);
    ready.err().unwrap_or(status)
}

/// The next change --watch reports; never without --watch.
async fn next_change(
    changes: &mut Option<UnboundedReceiver<watch::Changes>>,
) -> Option<watch::Changes> {
    match changes {
        Some(changes) => changes.recv().await,
        None => std::future::pending().await,
    }
}

/// A path for messages: relative to the working directory if it lies below it.
fn shown(path: &std::path::Path) -> String {
    let relative = std::env::current_dir().ok().and_then(|cwd| {
        path.strip_prefix(cwd.canonicalize().ok()?)
            .ok()
            .map(std::path::Path::to_path_buf)
    });
    relative.as_deref().unwrap_or(path).display().to_string()
}

/// Closed channels end the workers. Close all first, then wait: this way they shut down at the
/// same time, not one after another with up to the drain time of waiting each.
fn stop_workers(workers: Vec<Worker>) {
    let threads: Vec<_> = workers.into_iter().map(|worker| worker.thread).collect();
    for thread in threads {
        let _ = thread.join();
    }
}

/// Starts a worker thread and waits until its script has run.
/// `ports`: those the main thread listens on, None for the first worker, which determines them.
/// `dev`: the worker runs the hooks of the tenma.json `argv[0]`.
fn spawn_worker(
    argv: &[String],
    options: Options,
    ports: Option<BTreeSet<u16>>,
    dev: bool,
) -> (Worker, Ready) {
    let argv = argv.to_vec();
    let (ready_sender, ready) = mpsc::channel();
    let (connections, receiver) = unbounded_channel();
    let change = Arc::new(Notify::new());
    let worker_change = change.clone();
    let thread = std::thread::Builder::new()
        .stack_size(WORKER_STACK)
        .spawn(move || {
            worker(
                &argv,
                &ready_sender,
                receiver,
                worker_change,
                ports,
                dev,
                options,
            )
        })
        .expect("worker thread does not start");
    let ready = ready.recv().unwrap_or(Err(1));
    (
        Worker {
            connections,
            change,
            thread,
        },
        ready,
    )
}

/// Ctrl+C or SIGTERM, as sent by Docker, systemd and Kubernetes to stop.
/// `kill -HUP`, as after a renewal: the certificate files are read again.
async fn reload_certificate_on_hangup() {
    let hangup = tokio::signal::unix::SignalKind::hangup();
    let Ok(mut signals) = tokio::signal::unix::signal(hangup) else {
        return;
    };
    while signals.recv().await.is_some() {
        tls::reload();
    }
}

async fn shutdown_signal() {
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .expect("cannot catch SIGTERM");
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {}
        _ = terminate.recv() => {}
    }
    eprintln!("tenma: shutting down");
    // A second Ctrl+C or SIGTERM ends tenma at once, without waiting for the drain
    unsafe {
        libc::signal(libc::SIGINT, libc::SIG_DFL);
        libc::signal(libc::SIGTERM, libc::SIG_DFL);
    }
}

fn worker(
    argv: &[String],
    ready: &mpsc::Sender<Ready>,
    mut connections: UnboundedReceiver<Connection>,
    change: Arc<Notify>,
    mut ports: Option<BTreeSet<u16>>,
    dev: bool,
    options: Options,
) -> u8 {
    CHANGE.set(change);
    if dev {
        watch::dev_thread();
    }
    static WORKERS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let index = WORKERS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let mut first = true;
    let mut status = 0;
    loop {
        if let Some(settings) = options.record
            && !dev
            && let Err(message) =
                journal::start(settings, index, journal::header(argv, options.watch))
        {
            eprintln!("tenma: {message}");
            if first {
                let _ = ready.send(Err(1));
            }
            return 1;
        }
        if let Err(message) = php::thread_start() {
            eprintln!("tenma: {message}");
            if !first {
                // A worker missing after a restart would silently swallow every Nth connection
                eprintln!("tenma: worker does not restart, exiting");
                std::process::exit(1);
            }
            let _ = ready.send(Err(1));
            return 1;
        }
        if options.inspect.is_some() && !dev {
            inspect::thread();
        }
        if durable::enabled() && !dev {
            journal::set_mode(1);
        }
        BROKEN.set(false);
        HANDLED.set(0);
        watch::forget();
        if let Some(plan) = options.replay {
            replay::prepare(plan);
        }
        let loaded = load(argv, dev, options);
        // Also after a failure: the file with the syntax error is watched too
        watch::report();
        let stop = match loaded {
            Ok(listeners) => {
                if first {
                    let _ = ready.send(Ok(listeners.clone()));
                }
                let now: BTreeSet<u16> = listeners.iter().map(|listener| listener.port).collect();
                // No servers at all is no port change: an editor that truncates before writing, or a
                // script that no longer serves, waits for the next change instead of restarting tenma.
                // A first run without servers is remembered as none: servers added later restart it
                if options.watch && !dev {
                    let before = ports.get_or_insert_with(|| now.clone());
                    if !now.is_empty() && *before != now {
                        // ponytail: the whole process restarts; the listeners would have to move to new ports otherwise
                        eprintln!("tenma: the script's ports changed, restarting tenma");
                        restart_process();
                    }
                }
                if options.replay.is_some() {
                    status = replay_run(options);
                    if inspect::replay_again() {
                        Stop::Restart
                    } else {
                        Stop::Closed
                    }
                } else if listeners.is_empty() {
                    // If the script ended with exit() or die(), tenma starts nothing it had scheduled
                    // With --watch, exit() in a hook ends tenma through dev::EXIT
                    let ended = if php::script_exited() {
                        Ok(status)
                    } else {
                        run_until_idle(&mut connections, !(dev && options.watch), options)
                    };
                    match ended {
                        // exit() in a hook: tenma is ending
                        Ok(_) if dev && dev::exit_status().is_some() => Stop::Closed,
                        Ok(ended) => {
                            status = ended;
                            if options.watch {
                                if dev {
                                    eprintln!(
                                        "tenma: hooks stopped, waiting for a change to tenma.json"
                                    );
                                } else {
                                    eprintln!(
                                        "tenma: script ended with status {status}, waiting for changes"
                                    );
                                }
                                Stop::Wait
                            } else {
                                Stop::Closed
                            }
                        }
                        Err(stop) => stop,
                    }
                } else {
                    serve(&listeners, &mut connections, options)
                }
            }
            Err(failed) => {
                status = failed;
                if options.watch {
                    // Even on the first run: tenma waits for a fix, and restarts if it brings servers
                    if first {
                        let _ = ready.send(Ok(Vec::new()));
                    }
                    ports.get_or_insert_with(BTreeSet::new);
                    eprintln!(
                        "tenma: {} failed, waiting for changes",
                        if dev { "hooks" } else { "script" }
                    );
                    Stop::Wait
                } else if first {
                    let _ = ready.send(Err(status));
                    Stop::Closed
                } else {
                    // The script failed on restart: without the worker, carrying on does not help
                    eprintln!("tenma: script fails on worker restart, exiting");
                    std::process::exit(1);
                }
            }
        };
        if journal::recording() {
            watch::report();
            journal::finish(journal::loaded_files());
        }
        php::thread_stop();
        first = false;
        match stop {
            Stop::Closed => return status,
            Stop::Restart => {}
            Stop::Wait => {
                if dev {
                    dev::settled();
                }
                // exit() of this run must not end the next one
                php::forget_exit();
                if let Stop::Closed = unavailable(&mut connections, options) {
                    return status;
                }
            }
        }
    }
}

/// Replaces the process with a fresh tenma with the same arguments.
fn restart_process() -> ! {
    use std::os::unix::process::CommandExt;
    let error =
        std::process::Command::new(std::env::current_exe().unwrap_or_else(|_| "tenma".into()))
            .args(std::env::args_os().skip(1))
            .exec();
    eprintln!("tenma: cannot restart: {error}");
    std::process::exit(1);
}

/// --watch: the script failed or ended. Requests get 503 until the next change restarts the
/// worker; the error is on the console.
fn unavailable(connections: &mut UnboundedReceiver<Connection>, options: Options) -> Stop {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("tokio does not start");
    let change = CHANGE.with_borrow(Arc::clone);
    runtime.block_on(async {
        let mut answering = tokio::task::JoinSet::new();
        let stop = loop {
            tokio::select! {
                connection = connections.recv() => {
                    let Some((port, stream)) = connection else {
                        break Stop::Closed;
                    };
                    if let Some(refused) = refuse(port, stream, options, "The script failed to load, see the console. Waiting for changes.\n") {
                        answering.spawn(refused);
                    }
                    while answering.try_join_next().is_some() {}
                }
                () = change.notified() => {
                    watch::rerun(true);
                    break Stop::Restart;
                }
            }
        };
        // Connections accepted just before the change still get their answer: dropping the runtime
        // would reset them while their request is already on the way
        let _ = tokio::time::timeout(options.drain, async { while answering.join_next().await.is_some() {} }).await;
        stop
    })
}

/// A connection, plain or TLS, where its type does not matter.
trait Io: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send {}

impl<T: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send> Io for T {}

/// Answers a connection no script serves with 503, without keep-alive: it gets its answer and ends.
/// With --tls-cert after the handshake, or the client could not read the answer.
fn refuse(
    port: u16,
    stream: std::net::TcpStream,
    options: Options,
    message: &'static str,
) -> Option<impl std::future::Future<Output = ()> + Send + 'static> {
    let stream = TcpStream::from_std(stream).ok()?;
    let answer = move |stream: Box<dyn Io>| {
        let service = service_fn(move |_| async move {
            Ok::<_, Infallible>(plain(StatusCode::SERVICE_UNAVAILABLE, message))
        });
        http1::Builder::new()
            .timer(hyper_util::rt::TokioTimer::new())
            .header_read_timeout(options.request_timeout)
            .keep_alive(false)
            .serve_connection(TokioIo::new(stream), service)
    };
    Some(async move {
        let Some(acceptor) = tls::acceptor(port) else {
            let _ = answer(Box::new(stream)).await;
            return;
        };
        let handshake = tokio::time::timeout(options.request_timeout, acceptor.accept(stream));
        if let Ok(Ok(stream)) = handshake.await {
            let _ = answer(Box::new(stream)).await;
        }
    })
}

/// A script without servers: the event loop runs while timers or other ops are pending.
/// Ends with the status of exit() in timers or background work, 255 after a fatal error. With
/// --watch, a change (Restart) or the end of tenma (Closed) stops it early.
fn run_until_idle(
    connections: &mut UnboundedReceiver<Connection>,
    exits: bool,
    options: Options,
) -> Result<u8, Stop> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("tokio does not start");
    let restart = RESTART.with(Rc::clone);
    let change = CHANGE.with_borrow(Arc::clone);
    let ended = ops::in_loop(|| {
        LocalSet::new().block_on(&runtime, async {
            if php::started().is_err() {
                return Ok(255);
            }
            loop {
                tokio::select! {
                    // In order: if exit() also ends the last op, the status of exit() counts
                    biased;
                    () = php::EXIT.notified(), if exits => break Ok(php::exit_status().unwrap_or(0)),
                    // A fatal error: without servers there is no restart, the script has failed
                    () = restart.notified() => break Ok(255),
                    () = ops::idle() => break Ok(0),
                    () = change.notified() => {
                        watch::rerun(true);
                        break Err(Stop::Restart);
                    }
                    connection = connections.recv() => match connection {
                        None => break Err(Stop::Closed),
                        // With --watch the ports stay open while an edited script serves nothing
                        Some((port, stream)) => {
                            if let Some(refused) = refuse(port, stream, options, "The script serves no requests right now, see the console.\n") {
                                tokio::task::spawn_local(refused);
                            }
                        }
                    },
                }
            }
        })
    });
    let status = ended?;
    Ok(if BROKEN.get() {
        255
    } else {
        php::exit_status().unwrap_or(status)
    })
}

/// A replay's event loop: the journal drives PHP until its end; with the debugger stepping back, the
/// loop ends early and the worker starts over.
fn replay_run(options: Options) -> u8 {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("tokio does not start");
    let restart = RESTART.with(Rc::clone);
    let _ = futures_util::FutureExt::now_or_never(restart.notified());
    let local = LocalSet::new();
    let status = ops::in_loop(|| {
        local.block_on(&runtime, async {
            tokio::select! {
                biased;
                () = restart.notified() => 1,
                status = replay::drive() => status,
            }
        })
    });
    drop(local);
    runtime.shutdown_timeout(Duration::from_millis(100));
    if options.replay.is_none() { 1 } else { status }
}

/// Loads the standard library and the script (or the hooks of tenma.json) into this thread's PHP interpreter.
fn load(argv: &[String], dev: bool, options: Options) -> Result<Vec<php::Listener>, u8> {
    if let Err(message) = stdlib::load(None) {
        eprintln!("tenma: {message}");
        return Err(1);
    }
    if dev {
        let code = dev::code(std::path::Path::new(&argv[0]), options.watch);
        return match code.and_then(|code| php::eval(&code, "tenma:dev")) {
            Ok(()) => Ok(Vec::new()),
            Err(message) => {
                eprintln!("tenma: {message}");
                Err(1)
            }
        };
    }
    match php::execute(argv, CODE.get().map(String::as_str)) {
        Ok(0) => Ok(php::listeners()),
        Ok(status) => Err(status as u8),
        Err(message) => {
            eprintln!("tenma: {message}");
            Err(1)
        }
    }
}

/// A worker's event loop: PHP may only run on this thread, hence current_thread and LocalSet.
/// Ends when no more connections come or the interpreter should restart; running requests may
/// still finish then, except after a fatal error.
fn serve(
    listeners: &[php::Listener],
    connections: &mut UnboundedReceiver<Connection>,
    options: Options,
) -> Stop {
    let handlers: HashMap<u16, (u32, std::sync::Arc<Vec<files::Mount>>)> = listeners
        .iter()
        .map(|listener| (listener.port, (listener.handler, listener.mounts.clone())))
        .collect();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("tokio does not start");
    let local = LocalSet::new();
    // Each connection holds a receiver; drain sends the stop signal and waits until all are gone
    let (drain, drained) = tokio::sync::watch::channel(());
    let open = Rc::new(Cell::new(0));
    let restart = RESTART.with(Rc::clone);
    // A restart signal from the previous interpreter (e.g. a fatal error and max-requests at once)
    // does not apply to this one; a change does, it may have come while the script was loading
    let _ = futures_util::FutureExt::now_or_never(restart.notified());
    let change = CHANGE.with_borrow(Arc::clone);
    let stop = ops::in_loop(|| {
        local.block_on(&runtime, async {
        if php::started().is_err() {
            // If the startup work already fails, every restart would fail again: tenma shuts down
            // cleanly, the other workers still finish their requests
            eprintln!("tenma: fatal error in the script's startup work, shutting down");
            php::request_exit(255);
            return Stop::Closed;
        }
        // One worker per process fires the timers and signals of durable functions
        // One worker per process fires the timers and signals of durable functions, again after
        // every restart of its interpreter
        static CLAIMED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
        thread_local! {
            static POLLER: Cell<bool> = const { Cell::new(false) };
        }
        if durable::enabled()
            && (POLLER.get() || !CLAIMED.swap(true, std::sync::atomic::Ordering::Relaxed))
        {
            POLLER.set(true);
            tokio::task::spawn_local(durable::poll());
        }
        loop {
            tokio::select! {
                // A restart first: the response that asked for it may already be out, and the client's
                // next connection waiting too. That one belongs to the fresh interpreter: here it would
                // land in the old one, broken after a fatal error, and the drain would reset it unread
                biased;
                // Watch::reloadedBy() tells the next run why it runs: not for a change here
                () = restart.notified() => {
                    watch::rerun(false);
                    break Stop::Restart;
                }
                () = change.notified() => {
                    watch::rerun(true);
                    break Stop::Restart;
                }
                connection = connections.recv() => {
                    let Some((port, stream)) = connection else {
                        break Stop::Closed;
                    };
                    // Every connection holds memory and a file descriptor: beyond the limit a short answer
                    if open.get() >= options.max_connections {
                        if let Some(refused) = refuse(port, stream, options, "Too many connections, try again later.\n") {
                            tokio::task::spawn_local(refused);
                        }
                        continue;
                    }
                    let (Ok(remote), Ok(local)) = (stream.peer_addr(), stream.local_addr()) else {
                        continue;
                    };
                    let (Some((handler, mounts)), Ok(stream)) = (handlers.get(&port), TcpStream::from_std(stream)) else {
                        continue;
                    };
                    let (handler, mounts) = (*handler, mounts.clone());
                    let peers = (remote, local);
                    let counter = Rc::clone(&open);
                    // The streams of an HTTP/2 connection share one --max-body for their request bodies
                    let buffered = Rc::new(Cell::new(0));
                    let service = service_fn(move |request| {
                        respond(handler, mounts.clone(), request, options, peers, Rc::clone(&counter), Rc::clone(&buffered))
                    });
                    let drained = drained.clone();
                    let open = Open::count(&open);
                    tokio::task::spawn_local(async move {
                        let _open = open;
                        let Some(acceptor) = tls::acceptor(port) else {
                            serve_http(stream, service, options, drained).await;
                            return;
                        };
                        // A client that never finishes the handshake is gone after the request timeout
                        let handshake = tokio::time::timeout(options.request_timeout, acceptor.accept(stream));
                        let Ok(Ok(stream)) = handshake.await else {
                            return;
                        };
                        if stream.get_ref().1.alpn_protocol() == Some(b"h2") {
                            serve_h2(stream, service, options, drained).await;
                        } else {
                            serve_http(stream, service, options, drained).await;
                        }
                    });
                }
            }
        }
    })
    });
    drop(drained);
    // Running responses may finish, even after a fatal error (e.g. the error page): every path
    // into PHP checks BROKEN, so broken PHP does not run again
    // ponytail: new connections wait meanwhile; an open event stream holds a --max-requests
    // restart up to the full drain. Upgrade: end event streams and WebSockets here, clients reconnect
    ops::in_loop(|| {
        local.block_on(&runtime, async {
            // tenma stops: the handlers of Process\Shutdown first, up to --stop-timeout
            if matches!(stop, Stop::Closed) && !BROKEN.get() {
                if php::stopping().is_err() {
                    interpreter_broken();
                } else if tokio::time::timeout(options.stop_timeout, php::stop_handlers_done())
                    .await
                    .is_err()
                {
                    eprintln!(
                        "tenma: stop handlers still running after {} s, going on",
                        options.stop_timeout.as_secs_f64()
                    );
                }
            }
            let _ = drain.send(());
            let _ = tokio::time::timeout(options.drain, drain.closed()).await;
        })
    });
    // Dropping the runtime would wait for every blocking task: a file op that hangs (a FIFO without
    // writer, a dead network mount) would keep the worker from restarting or tenma from ending
    drop(local);
    runtime.shutdown_timeout(Duration::from_millis(100));
    stop
}

/// Serves HTTP on one connection, plain or after the TLS handshake, until it ends; once the worker
/// drains, it finishes the running request and closes.
async fn serve_http<S, H>(
    stream: S,
    service: H,
    options: Options,
    mut drained: tokio::sync::watch::Receiver<()>,
) where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
    H: hyper::service::HttpService<Incoming, ResBody = ResponseBody>,
    H::Error: Into<Box<dyn std::error::Error + Send + Sync>>,
{
    let stream = WriteTimeout::new(stream, options.request_timeout);
    let connection = http1::Builder::new()
        .timer(hyper_util::rt::TokioTimer::new())
        .header_read_timeout(options.request_timeout)
        .serve_connection(TokioIo::new(stream), service)
        .with_upgrades();
    tokio::pin!(connection);
    tokio::select! {
        _ = connection.as_mut() => {}
        _ = drained.changed() => {
            connection.as_mut().graceful_shutdown();
            let _ = connection.await;
        }
    }
}

/// Streams one HTTP/2 connection may have open at once, each a request on the worker.
const MAX_H2_STREAMS: u32 = 32;

/// Serves HTTP/2 on a TLS connection whose client chose it. Each request is a stream of its own, on
/// this worker's thread; once the worker drains, the client is told to open no new streams.
async fn serve_h2<S, H>(
    stream: S,
    service: H,
    options: Options,
    mut drained: tokio::sync::watch::Receiver<()>,
) where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + 'static,
    H: hyper::service::HttpService<Incoming, ResBody = ResponseBody>,
    LocalExecutor: hyper::rt::bounds::Http2ServerConnExec<H::Future, ResponseBody>,
{
    // ponytail: --request-timeout covers stalled writes here, not slow request headers; hyper's
    // HTTP/2 server has no header timeout per stream, a keep-alive ping would close dead connections
    let stream = WriteTimeout::new(stream, options.request_timeout);
    let connection = hyper::server::conn::http2::Builder::new(LocalExecutor)
        .timer(hyper_util::rt::TokioTimer::new())
        .max_concurrent_streams(MAX_H2_STREAMS)
        .serve_connection(TokioIo::new(stream), service);
    tokio::pin!(connection);
    tokio::select! {
        _ = connection.as_mut() => {}
        _ = drained.changed() => {
            connection.as_mut().graceful_shutdown();
            let _ = connection.await;
        }
    }
}

/// Runs the streams of an HTTP/2 connection on the worker's thread, where PHP is.
#[derive(Clone, Copy)]
struct LocalExecutor;

impl<F> hyper::rt::Executor<F> for LocalExecutor
where
    F: std::future::Future + 'static,
{
    fn execute(&self, future: F) {
        tokio::task::spawn_local(future);
    }
}

/// Accepts connections and hands them to the workers round-robin.
// ponytail: round-robin without looking at load; least busy workers first if requests differ a lot in cost
async fn accept(port: u16, workers: Vec<UnboundedSender<Connection>>) {
    let socket = match TcpListener::bind(("0.0.0.0", port)).await {
        Ok(socket) => socket,
        Err(error) => {
            eprintln!("tenma: port {port} not available: {error}");
            std::process::exit(1);
        }
    };
    let mut next = 0;
    loop {
        let stream = match socket.accept().await {
            Ok((stream, _)) => stream,
            // Out of file descriptors (EMFILE): accept fails right away again; give connections time to close
            Err(_) => {
                tokio::time::sleep(Duration::from_millis(50)).await;
                continue;
            }
        };
        if let Ok(stream) = stream.into_std() {
            let _ = workers[next].send((port, stream));
            next = (next + 1) % workers.len();
        }
    }
}

/// Counts a worker's open connections while it lives.
struct Open(Rc<Cell<usize>>);

impl Open {
    fn count(open: &Rc<Cell<usize>>) -> Self {
        open.set(open.get() + 1);
        Self(Rc::clone(open))
    }
}

impl Drop for Open {
    fn drop(&mut self) {
        self.0.set(self.0.get() - 1);
    }
}

/// A connection whose writes may stall at most $limit: a client that stops reading is disconnected
/// instead of holding its task, its buffers and a stream writer that waits for room forever.
struct WriteTimeout<S> {
    stream: S,
    limit: Duration,
    stalled: Option<std::pin::Pin<Box<tokio::time::Sleep>>>,
}

impl<S> WriteTimeout<S> {
    fn new(stream: S, limit: Duration) -> Self {
        Self {
            stream,
            limit,
            stalled: None,
        }
    }

    /// Pending starts the clock, or fails once it ran out; any progress stops it.
    fn check<T>(
        &mut self,
        context: &mut std::task::Context<'_>,
        poll: std::task::Poll<std::io::Result<T>>,
    ) -> std::task::Poll<std::io::Result<T>> {
        if poll.is_ready() {
            self.stalled = None;
            return poll;
        }
        let limit = self.limit;
        let stalled = self
            .stalled
            .get_or_insert_with(|| Box::pin(tokio::time::sleep(limit)));
        match stalled.as_mut().poll(context) {
            std::task::Poll::Ready(()) => {
                std::task::Poll::Ready(Err(std::io::ErrorKind::TimedOut.into()))
            }
            std::task::Poll::Pending => std::task::Poll::Pending,
        }
    }
}

impl<S: tokio::io::AsyncRead + Unpin> tokio::io::AsyncRead for WriteTimeout<S> {
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        context: &mut std::task::Context<'_>,
        buffer: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.stream).poll_read(context, buffer)
    }
}

impl<S: tokio::io::AsyncWrite + Unpin> tokio::io::AsyncWrite for WriteTimeout<S> {
    fn poll_write(
        mut self: std::pin::Pin<&mut Self>,
        context: &mut std::task::Context<'_>,
        data: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        let poll = std::pin::Pin::new(&mut self.stream).poll_write(context, data);
        self.check(context, poll)
    }

    fn poll_write_vectored(
        mut self: std::pin::Pin<&mut Self>,
        context: &mut std::task::Context<'_>,
        data: &[std::io::IoSlice<'_>],
    ) -> std::task::Poll<std::io::Result<usize>> {
        let poll = std::pin::Pin::new(&mut self.stream).poll_write_vectored(context, data);
        self.check(context, poll)
    }

    fn is_write_vectored(&self) -> bool {
        self.stream.is_write_vectored()
    }

    fn poll_flush(
        mut self: std::pin::Pin<&mut Self>,
        context: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        let poll = std::pin::Pin::new(&mut self.stream).poll_flush(context);
        self.check(context, poll)
    }

    fn poll_shutdown(
        mut self: std::pin::Pin<&mut Self>,
        context: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.stream).poll_shutdown(context)
    }
}

enum BodyError {
    TooLarge,
    Broken,
}

/// The part of a connection's request bodies one request holds, given back when the request ends.
struct Held {
    buffered: Rc<Cell<usize>>,
    bytes: usize,
}

impl Drop for Held {
    fn drop(&mut self) {
        self.buffered.set(self.buffered.get() - self.bytes);
    }
}

/// Reads a request body into memory. All requests of the connection together stay under `max_body`.
async fn collect(mut body: Incoming, held: &mut Held, max_body: usize) -> Result<Bytes, BodyError> {
    let mut data = Vec::new();
    while let Some(frame) = body.frame().await {
        let frame = frame.map_err(|_| BodyError::Broken)?;
        let Ok(chunk) = frame.into_data() else {
            continue;
        };
        if held.buffered.get() + chunk.len() > max_body {
            return Err(BodyError::TooLarge);
        }
        held.buffered.set(held.buffered.get() + chunk.len());
        held.bytes += chunk.len();
        data.extend_from_slice(&chunk);
    }
    Ok(Bytes::from(data))
}

/// Body of a response: complete or piece by piece (from PHP or a file).
pub type ResponseBody = Either<Full<Bytes>, streams::ChannelBody>;

/// `peers`: remote and local address of the connection. `open`: the worker's open connections.
/// `buffered`: the bytes of request bodies the connection holds right now.
async fn respond(
    handler: u32,
    mounts: std::sync::Arc<Vec<files::Mount>>,
    request: Request<Incoming>,
    options: Options,
    peers: (std::net::SocketAddr, std::net::SocketAddr),
    open: Rc<Cell<usize>>,
    buffered: Rc<Cell<usize>>,
) -> Result<Response<ResponseBody>, Infallible> {
    // The file first, if there is one; otherwise the handler
    if !mounts.is_empty()
        && let Some(response) =
            files::serve(&mounts, request.method(), request.uri(), request.headers()).await
    {
        return Ok(close_if_restarting(response, options));
    }
    let (mut parts, body) = request.into_parts();
    // ponytail: body fully in memory up to the limit; streams when large uploads come
    let mut held = Held { buffered, bytes: 0 };
    let collected = collect(body, &mut held, options.max_body);
    let body = match tokio::time::timeout(options.request_timeout, collected).await {
        Ok(Ok(body)) => body,
        Ok(Err(BodyError::TooLarge)) => {
            return Ok(plain(StatusCode::PAYLOAD_TOO_LARGE, "Payload Too Large\n"));
        }
        // Aborted upload, broken chunked encoding
        Ok(Err(BodyError::Broken)) => return Ok(plain(StatusCode::BAD_REQUEST, "Bad Request\n")),
        Err(_) => return Ok(plain(StatusCode::REQUEST_TIMEOUT, "Request Timeout\n")),
    };
    if BROKEN.get() {
        return Ok(close_if_restarting(
            plain(StatusCode::SERVICE_UNAVAILABLE, "Service Unavailable\n"),
            options,
        ));
    }
    let upgrade = parts.extensions.remove::<hyper::upgrade::OnUpgrade>();
    // HTTP/2 sends scheme and host in the request line (:scheme, :authority), not as a Host header
    let uri = parts
        .uri
        .path_and_query()
        .map_or("/", |path| path.as_str())
        .to_owned();
    if !parts.headers.contains_key(hyper::header::HOST)
        && let Some(authority) = parts.uri.authority()
        && let Ok(host) = hyper::header::HeaderValue::from_str(authority.as_str())
    {
        parts.headers.insert(hyper::header::HOST, host);
    }
    let protocol = match parts.version {
        hyper::Version::HTTP_2 => "HTTP/2",
        hyper::Version::HTTP_10 => "HTTP/1.0",
        _ => "HTTP/1.1",
    };
    let request = php::Request {
        method: parts.method.as_str().as_bytes(),
        uri: uri.as_bytes(),
        headers: parts
            .headers
            .iter()
            .map(|(name, value)| (name.as_str().as_bytes(), value.as_bytes()))
            .collect(),
        body: &body,
        remote: peers.0,
        local: peers.1,
        https: tls::acceptor(peers.1.port()).is_some(),
        protocol: protocol.as_bytes(),
    };
    // PHP runs on the loop thread until the handler responds or its fiber waits for an op
    let failed = || {
        php::Reply::Full(php::Response {
            status: 500,
            headers: Vec::new(),
            body: b"Internal Server Error\n".to_vec(),
        })
    };
    let reply = match php::handle(handler, &request) {
        php::Outcome::Pending(pending) => pending.await.unwrap_or_else(|_| failed()),
        php::Outcome::Failed => failed(),
        php::Outcome::Fatal => {
            interpreter_broken();
            failed()
        }
        php::Outcome::Broken(receiver) => {
            interpreter_broken();
            receiver.await.unwrap_or_else(|_| failed())
        }
    };
    HANDLED.set(HANDLED.get() + 1);
    if options.max_requests.is_some_and(|max| HANDLED.get() == max) {
        RESTART.with(|restart| restart.notify_one());
    }
    // ponytail: checked after requests only; a file only a timer loads is watched from the next request on
    watch::report();
    let (status, headers, body) = match reply {
        php::Reply::Full(response) => (
            response.status,
            response.headers,
            Either::Left(Full::new(Bytes::from(response.body))),
        ),
        php::Reply::Stream {
            status,
            headers,
            body,
        } => (status, headers, Either::Right(body)),
        php::Reply::Upgrade(pipes) => {
            return Ok(accept_websocket(&parts.headers, upgrade, pipes, &open));
        }
    };
    let mut builder = Response::builder()
        .status(StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR));
    for (name, value) in headers {
        builder = builder.header(name, value);
    }
    Ok(close_if_restarting(
        builder.body(body).unwrap_or_else(|_| {
            plain(
                StatusCode::INTERNAL_SERVER_ERROR,
                "Invalid response headers\n",
            )
        }),
        options,
    ))
}

/// After a fatal error or the last of --max-requests this interpreter takes no more requests: a
/// client that kept the connection would send its next one into the drain (reset, or a 503 from
/// the broken interpreter). Connection: close sends it to a new connection, served by the fresh one.
fn close_if_restarting(
    mut response: Response<ResponseBody>,
    options: Options,
) -> Response<ResponseBody> {
    if BROKEN.get() || options.max_requests.is_some_and(|max| HANDLED.get() >= max) {
        response.headers_mut().insert(
            hyper::header::CONNECTION,
            hyper::header::HeaderValue::from_static("close"),
        );
    }
    response
}

/// Responds with 101 and hands the connection to PHP after the upgrade.
fn accept_websocket(
    headers: &hyper::HeaderMap,
    upgrade: Option<hyper::upgrade::OnUpgrade>,
    pipes: streams::Pipes,
    open: &Rc<Cell<usize>>,
) -> Response<ResponseBody> {
    let (Some(key), Some(upgrade)) = (headers.get("sec-websocket-key"), upgrade) else {
        return plain(StatusCode::BAD_REQUEST, "Not a WebSocket request\n");
    };
    let accept = tokio_tungstenite::tungstenite::handshake::derive_accept_key(key.as_bytes());
    // The HTTP connection's count ends with the 101: the socket counts on its own
    let open = Open::count(open);
    tokio::task::spawn_local(async move {
        let _open = open;
        streams::run_websocket(upgrade, pipes).await;
    });
    Response::builder()
        .status(StatusCode::SWITCHING_PROTOCOLS)
        .header("connection", "upgrade")
        .header("upgrade", "websocket")
        .header("sec-websocket-accept", accept)
        .body(Either::Left(Full::new(Bytes::new())))
        .unwrap_or_else(|_| plain(StatusCode::INTERNAL_SERVER_ERROR, "WebSocket failed\n"))
}

fn plain(status: StatusCode, text: &'static str) -> Response<ResponseBody> {
    let mut response = Response::new(Either::Left(Full::new(Bytes::from_static(text.as_bytes()))));
    *response.status_mut() = status;
    response
}

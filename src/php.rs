//! The boundary to PHP: safe wrappers around the functions of the C layer (shim/tenma.c).
//! PHP may only run on the thread that called `start`.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::ffi::{CString, c_char, c_int};

use tokio::sync::oneshot;

#[repr(C)]
struct Str {
    ptr: *const u8,
    len: usize,
}

impl Str {
    fn of(bytes: &[u8]) -> Self {
        Str {
            ptr: bytes.as_ptr(),
            len: bytes.len(),
        }
    }
}

#[repr(C)]
struct Header {
    name: Str,
    value: Str,
}

#[repr(C)]
struct RawRequest {
    method: Str,
    uri: Str,
    body: Str,
    headers: *const Header,
    header_count: usize,
    remote_addr: Str,
    remote_port: u16,
    server_addr: Str,
    server_port: u16,
    https: bool,
    protocol: Str,
}

#[repr(C)]
struct RawResponse {
    status: u16,
    body: *mut u8,
    body_len: usize,
    headers: *mut u8,
    headers_len: usize,
}

unsafe extern "C" {
    fn tenma_start() -> c_int;
    fn tenma_define_ini(entry: *const c_char);
    fn tenma_set_ini_path(path: *const c_char);
    fn tenma_set_server_name(name: *const c_char);
    fn tenma_set_pdo_native();
    fn tenma_stopping() -> c_int;
    fn tenma_current_request() -> u64;
    fn tenma_stop_handlers_running() -> c_int;
    fn tenma_stop();
    fn tenma_thread_start() -> c_int;
    fn tenma_thread_stop();
    fn tenma_started() -> c_int;
    fn tenma_script_exited() -> c_int;
    fn tenma_exit_status() -> c_int;
    fn tenma_eval(code: *const c_char, len: usize, name: *const c_char) -> c_int;
    fn tenma_execute(path: *const c_char, argv: *const *const c_char, argc: c_int) -> c_int;
    fn tenma_handle(handler: u32, rid: u64, request: *const RawRequest) -> c_int;
    fn tenma_response_free(response: *mut RawResponse);
    fn tenma_complete_null(op: u64) -> c_int;
    fn tenma_memory_room() -> usize;
    fn tenma_request_gone(rid: u64) -> c_int;
    fn tenma_complete_string(op: u64, text: *const u8, len: usize) -> c_int;
    fn tenma_complete_error(op: u64, message: *const u8, len: usize, code: c_int) -> c_int;
    fn tenma_complete_http(
        op: u64,
        status: u16,
        headers: *const u8,
        headers_len: usize,
        body: *const u8,
        body_len: usize,
    ) -> c_int;
}

/// A server the script registered with `tenma_listen`.
#[derive(Clone)]
pub struct Listener {
    pub port: u16,
    pub handler: u32,
    /// Prefixes with static files, longest first; Rust serves files in them without PHP.
    pub mounts: std::sync::Arc<Vec<crate::files::Mount>>,
}

thread_local! {
    static LISTENERS: RefCell<Vec<Listener>> = const { RefCell::new(Vec::new()) };
    /// Requests whose response is still pending: PHP responds via `tenma_respond`, often from a fiber.
    static WAITING: RefCell<HashMap<u64, oneshot::Sender<Reply>>> = RefCell::new(HashMap::new());
    static NEXT_ID: Cell<u64> = const { Cell::new(1) };
}

/// Bytes from C as a slice; null or length 0 give an empty slice.
pub fn bytes<'a>(ptr: *const u8, len: usize) -> &'a [u8] {
    if ptr.is_null() || len == 0 {
        &[]
    } else {
        unsafe { std::slice::from_raw_parts(ptr, len) }
    }
}

/// Bytes from C as text, invalid UTF-8 replaced.
pub fn text(ptr: *const u8, len: usize) -> String {
    String::from_utf8_lossy(bytes(ptr, len)).into_owned()
}

/// A new ID for requests and ops, never 0.
pub fn next_id() -> u64 {
    NEXT_ID.replace(NEXT_ID.get() + 1)
}

/// The request whose code runs right now; 0 for none (startup, timers, detached tasks). Only while
/// PHP runs on this thread.
pub fn current_request() -> u64 {
    unsafe { tenma_current_request() }
}

/// The bytes a result may still have so that it fits under memory_limit. Only while PHP runs on this
/// thread.
pub fn memory_room() -> usize {
    unsafe { tenma_memory_room() }
}

/// A replay gives requests and ops the IDs the journal recorded.
pub fn set_next_id(id: u64) {
    NEXT_ID.set(id);
}

/// Called from PHP via `tenma_respond`: passes the response to the waiting request.
#[unsafe(no_mangle)]
extern "C" fn tenma_rs_respond(rid: u64, raw: *mut RawResponse) {
    let response = unsafe { take_response(raw) };
    reply(rid, Reply::Full(response));
}

/// What PHP answers to a waiting request.
pub enum Reply {
    Full(Response),
    /// Head right away, the body follows piece by piece from the channel (Response::stream, Response::events).
    Stream {
        status: u16,
        headers: Vec<(Vec<u8>, Vec<u8>)>,
        body: crate::streams::ChannelBody,
    },
    /// The connection becomes a WebSocket (Response::websocket).
    Upgrade(crate::streams::Pipes),
}

/// Gives a waiting request its response. False if it is not (or no longer) waiting.
pub fn reply(rid: u64, reply: Reply) -> bool {
    if crate::journal::recording() {
        crate::journal::record_response(
            rid,
            match &reply {
                Reply::Full(response) => crate::journal::Reply::Full {
                    status: response.status,
                    headers: response.headers.clone(),
                    body: response.body.clone(),
                },
                Reply::Stream {
                    status, headers, ..
                } => crate::journal::Reply::Stream {
                    status: *status,
                    headers: headers.clone(),
                },
                Reply::Upgrade(_) => crate::journal::Reply::Upgrade,
            },
        );
    }
    WAITING
        .with_borrow_mut(|waiting| waiting.remove(&rid))
        .is_some_and(|sender| sender.send(reply).is_ok())
}

/// Header lines "Name: Value", separated by null bytes, as pairs.
pub fn parse_header_lines(packed: &[u8]) -> Vec<(Vec<u8>, Vec<u8>)> {
    packed
        .split(|&byte| byte == 0)
        .filter_map(|line| {
            let colon = line.iter().position(|&byte| byte == b':')?;
            Some((
                line[..colon].trim_ascii().to_vec(),
                line[colon + 1..].trim_ascii().to_vec(),
            ))
        })
        .collect()
}

/// Copies the response out of C memory and frees it.
unsafe fn take_response(raw: *mut RawResponse) -> Response {
    let raw_ref = unsafe { &*raw };
    let body = bytes(raw_ref.body, raw_ref.body_len).to_vec();
    let headers = parse_header_lines(bytes(raw_ref.headers, raw_ref.headers_len));
    let status = raw_ref.status;
    unsafe { tenma_response_free(raw) };
    Response {
        status,
        headers,
        body,
    }
}

/// Called from PHP via `tenma_listen`. Only records the server; it starts after the script.
/// 1: registered. 0: invalid port. -1: a target for static files is neither a directory nor a
/// file. $mounts: prefix and absolute path alternating, each terminated by a null byte.
#[unsafe(no_mangle)]
extern "C" fn tenma_rs_listen(
    port: i64,
    handler: u32,
    mounts: *const u8,
    mounts_len: usize,
) -> c_int {
    let Ok(port @ 1..) = u16::try_from(port) else {
        return 0;
    };
    let packed = bytes(mounts, mounts_len);
    let fields: Vec<&[u8]> = packed.split(|&byte| byte == 0).collect();
    let mut parsed = Vec::new();
    // Not stored canonicalized: files::serve resolves the paths per request, so a deploy by
    // swapping a symlink takes effect without a restart
    for pair in fields.chunks_exact(2) {
        let target = std::path::Path::new(
            <std::ffi::OsStr as std::os::unix::ffi::OsStrExt>::from_bytes(pair[1]),
        );
        let Some(mount) =
            crate::files::Mount::new(String::from_utf8_lossy(pair[0]).into_owned(), target)
        else {
            return -1;
        };
        parsed.push(mount);
    }
    parsed.sort_by_key(|mount| std::cmp::Reverse(mount.prefix.len()));
    LISTENERS.with_borrow_mut(|listeners| {
        listeners.push(Listener {
            port,
            handler,
            mounts: std::sync::Arc::new(parsed),
        })
    });
    1
}

pub fn listeners() -> Vec<Listener> {
    LISTENERS.with_borrow(Clone::clone)
}

pub struct Request<'a> {
    pub method: &'a [u8],
    pub uri: &'a [u8],
    pub headers: Vec<(&'a [u8], &'a [u8])>,
    pub body: &'a [u8],
    /// Remote and local address of the connection.
    pub remote: std::net::SocketAddr,
    pub local: std::net::SocketAddr,
    /// Whether the connection came over TLS.
    pub https: bool,
    /// HTTP/1.1, HTTP/2 or HTTP/1.0.
    pub protocol: &'a [u8],
}

pub struct Response {
    pub status: u16,
    pub headers: Vec<(Vec<u8>, Vec<u8>)>,
    pub body: Vec<u8>,
}

pub enum Outcome {
    /// The response comes once PHP calls `tenma_respond`; maybe right away, maybe after ops.
    Pending(Pending),
    /// Exception, PHP has already printed the error.
    Failed,
    /// Fatal error: PHP is unusable afterwards.
    Fatal,
    /// A classic request with a fatal error still responded (e.g. with the error page of a
    /// shutdown function); PHP is unusable afterwards.
    Broken(oneshot::Receiver<Reply>),
}

/// Result of an op that goes to the waiting fiber.
#[derive(Clone, Debug, PartialEq)]
pub enum OpResult {
    Null,
    Text(Vec<u8>),
    /// An error with a code (e.g. errno) for the exception code in PHP.
    Failure {
        message: String,
        code: i32,
    },
    Error(String),
    Http {
        status: u16,
        headers: Vec<String>,
        body: Vec<u8>,
    },
}

/// Resumes the fiber waiting on `op`. After a fatal error PHP is unusable.
pub fn complete(op: u64, result: OpResult) -> Result<(), ()> {
    let code = match result {
        OpResult::Null => unsafe { tenma_complete_null(op) },
        OpResult::Text(text) => unsafe { tenma_complete_string(op, text.as_ptr(), text.len()) },
        OpResult::Error(message) => unsafe {
            tenma_complete_error(op, message.as_ptr(), message.len(), 0)
        },
        OpResult::Failure { message, code } => unsafe {
            tenma_complete_error(op, message.as_ptr(), message.len(), code)
        },
        OpResult::Http {
            status,
            headers,
            body,
        } => {
            let packed: Vec<u8> = headers
                .iter()
                .flat_map(|line| line.bytes().chain([0]))
                .collect();
            unsafe {
                tenma_complete_http(
                    op,
                    status,
                    packed.as_ptr(),
                    packed.len(),
                    body.as_ptr(),
                    body.len(),
                )
            }
        }
    };
    if code < 0 { Err(()) } else { Ok(()) }
}

pub fn start() -> Result<(), String> {
    if unsafe { tenma_start() } == 0 {
        Ok(())
    } else {
        Err("PHP does not start".into())
    }
}

/// `--pdo-pgsql native`: PDO keeps libpq for Postgres instead of tenma's driver. Only before `start`.
pub fn pdo_native() {
    unsafe { tenma_set_pdo_native() };
}

/// A setting like `php -d name=value`. Only before `start`.
/// The PHP settings of the command line (-d, -c), as a journal's header records them.
pub static SETTINGS: std::sync::Mutex<(Vec<String>, Option<String>)> =
    std::sync::Mutex::new((Vec::new(), None));

pub fn define_ini(entry: &str) -> Result<(), String> {
    SETTINGS.lock().unwrap().0.push(entry.to_owned());
    let entry = CString::new(entry).map_err(|_| "setting contains a null byte")?;
    unsafe { tenma_define_ini(entry.as_ptr()) };
    Ok(())
}

/// A php.ini like `php -c path`. Only before `start`.
pub fn set_ini_path(path: &str) -> Result<(), String> {
    SETTINGS.lock().unwrap().1 = Some(
        std::fs::canonicalize(path)
            .map_or_else(|_| path.to_owned(), |path| path.display().to_string()),
    );
    let path = CString::new(path).map_err(|_| "path contains a null byte")?;
    unsafe { tenma_set_ini_path(path.as_ptr()) };
    Ok(())
}

/// `$_SERVER['SERVER_NAME']` in classic mode. Only before `start`.
pub fn set_server_name(name: &str) -> Result<(), String> {
    let name = CString::new(name).map_err(|_| "name contains a null byte")?;
    unsafe { tenma_set_server_name(name.as_ptr()) };
    Ok(())
}

pub fn stop() {
    unsafe { tenma_stop() }
}

/// Own PHP interpreter for this thread. Only after `start` in the main thread.
pub fn thread_start() -> Result<(), String> {
    if unsafe { tenma_thread_start() } == 0 {
        Ok(())
    } else {
        Err("PHP does not start in the worker".into())
    }
}

/// Did the last executed script end with exit() or die()? Then tenma starts nothing it scheduled.
pub fn script_exited() -> bool {
    unsafe { tenma_script_exited() != 0 }
}

/// exit() outside a request (timer, background, script): tenma ends with this status.
pub static EXIT_STATUS: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(-1);
pub static EXIT: std::sync::LazyLock<tokio::sync::Notify> =
    std::sync::LazyLock::new(tokio::sync::Notify::new);

#[unsafe(no_mangle)]
extern "C" fn tenma_rs_exit(status: c_int) {
    request_exit((status & 0xff) as u8);
}

/// tenma should end with this status: the server shuts down cleanly, a script ends.
pub fn request_exit(status: u8) {
    if crate::watch::is_dev_thread() {
        return crate::dev::request_exit(status);
    }
    EXIT_STATUS.store(i32::from(status), std::sync::atomic::Ordering::Relaxed);
    EXIT.notify_one();
}

/// --watch: the script runs again, its last exit() is forgotten.
pub fn forget_exit() {
    EXIT_STATUS.store(-1, std::sync::atomic::Ordering::Relaxed);
    let _ = futures_util::FutureExt::now_or_never(EXIT.notified());
}

/// The status PHP itself would exit with: from exit(), 255 after a fatal error, otherwise 0.
pub fn php_exit_status() -> u8 {
    (unsafe { tenma_exit_status() } & 0xff) as u8
}

/// The status of exit() outside a request, if it ran.
pub fn exit_status() -> Option<u8> {
    u8::try_from(EXIT_STATUS.load(std::sync::atomic::Ordering::Relaxed)).ok()
}

/// The event loop is running: PHP starts what the script scheduled at startup (timers, Task::run()).
/// Err if PHP fails fatally while doing so.
pub fn started() -> Result<(), ()> {
    if crate::journal::recording() {
        crate::journal::record(crate::journal::Entry::Started);
    }
    if unsafe { tenma_started() } < 0 {
        Err(())
    } else {
        Ok(())
    }
}

/// tenma stops: the stop handlers of Process\Shutdown start. Err after a fatal error.
pub fn stopping() -> Result<(), ()> {
    if unsafe { tenma_stopping() } < 0 {
        Err(())
    } else {
        Ok(())
    }
}

/// Resolves once every stop handler has finished; polls PHP's count, which the handlers keep.
pub async fn stop_handlers_done() {
    while unsafe { tenma_stop_handlers_running() } > 0 {
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
}

/// Drops the pending requests: they end with 500.
pub fn forget_waiting() {
    WAITING.with_borrow_mut(HashMap::clear);
}

/// Shuts down this thread's interpreter. Registered servers and waiting requests belong to it and are dropped.
pub fn thread_stop() {
    LISTENERS.with_borrow_mut(Vec::clear);
    forget_waiting();
    crate::streams::clear();
    crate::db::clear();
    crate::pubsub::clear();
    crate::ops::clear();
    unsafe { tenma_thread_stop() }
}

pub fn eval(code: &str, name: &str) -> Result<(), String> {
    let name = CString::new(name).unwrap();
    match unsafe { tenma_eval(code.as_ptr().cast(), code.len(), name.as_ptr()) } {
        0 => Ok(()),
        _ => Err(format!("{} could not be loaded", name.to_string_lossy())),
    }
}

/// Runs `argv[0]` as a script, with `argv` as `$argv`, and returns the exit status.
pub fn execute(argv: &[String]) -> Result<i32, String> {
    let path = std::fs::canonicalize(&argv[0]).map_err(|error| format!("{}: {error}", argv[0]))?;
    let path =
        CString::new(path.to_string_lossy().as_bytes()).map_err(|_| "path contains a null byte")?;
    let args: Vec<CString> = argv
        .iter()
        .map(|arg| CString::new(arg.as_bytes()))
        .collect::<Result<_, _>>()
        .map_err(|_| "argument contains a null byte")?;
    let pointers: Vec<*const c_char> = args.iter().map(|arg| arg.as_ptr()).collect();
    Ok(unsafe { tenma_execute(path.as_ptr(), pointers.as_ptr(), pointers.len() as c_int) })
}

/// PHP's response to a request. If it is dropped before PHP responded (hyper drops it when the
/// client closes the connection), the request's tasks get Tenma\Cancelled: running ops are
/// aborted instead of working on for nobody.
///
/// ponytail: hyper only notices the close with an empty read buffer. After pipelining (second request
/// already in the buffer) the handler runs to completion; a client that only closes its sending side
/// (shutdown(SHUT_WR)) counts as gone. Browsers do neither.
pub struct Pending {
    rid: u64,
    receiver: oneshot::Receiver<Reply>,
}

impl std::future::Future for Pending {
    type Output = Result<Reply, oneshot::error::RecvError>;

    fn poll(
        mut self: std::pin::Pin<&mut Self>,
        context: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Self::Output> {
        std::pin::Pin::new(&mut self.receiver).poll(context)
    }
}

impl Drop for Pending {
    fn drop(&mut self) {
        // Already answered: nothing to cancel
        if WAITING
            .with_borrow_mut(|waiting| waiting.remove(&self.rid))
            .is_some()
        {
            gone(self.rid);
        }
    }
}

/// The client left before the request was done: its tasks get Tenma\Cancelled. Not if the
/// interpreter is broken or the loop is over (shutdown after the deadline): then the cleanup in PHP
/// could not start any more ops.
pub fn gone(rid: u64) {
    if crate::broken() || !crate::ops::looping() {
        return;
    }
    if crate::journal::recording() {
        crate::journal::record(crate::journal::Entry::Gone { rid });
    }
    if unsafe { tenma_request_gone(rid) } < 0 {
        crate::interpreter_broken();
    }
}

pub fn handle(handler: u32, request: &Request) -> Outcome {
    let headers: Vec<Header> = request
        .headers
        .iter()
        .map(|(name, value)| Header {
            name: Str::of(name),
            value: Str::of(value),
        })
        .collect();
    let (remote_ip, local_ip) = (
        request.remote.ip().to_canonical().to_string(),
        request.local.ip().to_canonical().to_string(),
    );
    let raw = RawRequest {
        method: Str::of(request.method),
        uri: Str::of(request.uri),
        body: Str::of(request.body),
        headers: headers.as_ptr(),
        header_count: headers.len(),
        remote_addr: Str::of(remote_ip.as_bytes()),
        remote_port: request.remote.port(),
        server_addr: Str::of(local_ip.as_bytes()),
        server_port: request.local.port(),
        https: request.https,
        protocol: Str::of(request.protocol),
    };
    let rid = next_id();
    if crate::journal::recording() {
        crate::journal::record_request(
            rid,
            crate::journal::RawRequest {
                handler,
                method: request.method.to_vec(),
                uri: request.uri.to_vec(),
                headers: request
                    .headers
                    .iter()
                    .map(|(name, value)| (name.to_vec(), value.to_vec()))
                    .collect(),
                body: request.body.to_vec(),
                remote: request.remote.to_string(),
                local: request.local.to_string(),
                https: request.https,
                protocol: request.protocol.to_vec(),
            },
        );
    }
    let (sender, receiver) = oneshot::channel();
    WAITING.with_borrow_mut(|waiting| waiting.insert(rid, sender));
    let code = unsafe { tenma_handle(handler, rid, &raw) };
    if code == 0 || code == -1 {
        WAITING.with_borrow_mut(|waiting| waiting.remove(&rid));
    }
    match code {
        1 => Outcome::Pending(Pending { rid, receiver }),
        0 => Outcome::Failed,
        -2 => Outcome::Broken(receiver),
        _ => Outcome::Fatal,
    }
}

//! Async ops that PHP starts through the C layer. Each op runs as a task on the loop thread and
//! then resumes the fiber waiting on it (or fires a timer). Ops can only be started inside the
//! event loop. Pending ops keep a script without servers alive.

use std::cell::RefCell;
use std::collections::HashMap;
use std::ffi::c_int;
use std::os::unix::ffi::OsStrExt;
use std::rc::Rc;
use std::time::Duration;

use tokio::sync::Notify;
use tokio::task::AbortHandle;

use crate::php::{self, OpResult, text};

thread_local! {
    /// Is a worker's event loop running right now? Only then can ops be started.
    static IN_LOOP: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    /// Running ops, for cancelling and for knowing when nothing is pending any more.
    /// None: the op cannot be cancelled (file system), it runs to completion.
    static RUNNING: RefCell<HashMap<u64, Option<AbortHandle>>> = RefCell::new(HashMap::new());
    /// Signals when the last op is done.
    static IDLE: Rc<Notify> = Rc::new(Notify::new());
}

/// Removes the op from RUNNING when its task ends, even if it is aborted.
struct Finished(u64);

impl Drop for Finished {
    fn drop(&mut self) {
        forget(self.0);
    }
}

fn forget(op: u64) {
    let empty = RUNNING.with_borrow_mut(|running| {
        running.remove(&op);
        running.is_empty()
    });
    if empty {
        IDLE.with(|idle| idle.notify_one());
    }
}

/// Waits until no op is running.
pub async fn idle() {
    let idle = IDLE.with(Rc::clone);
    loop {
        let notified = idle.notified();
        if RUNNING.with_borrow(HashMap::is_empty) {
            return;
        }
        notified.await;
    }
}

/// Cancels a running op; the fiber or timer waiting on it hears nothing more of it.
#[unsafe(no_mangle)]
extern "C" fn tenma_rs_op_cancel(op: u64) {
    if let Some(Some(handle)) = RUNNING.with_borrow(|running| running.get(&op).cloned()) {
        // The task's Finished guard cleans up RUNNING once the task is dropped
        handle.abort();
    }
}

/// Runs `run` with ops allowed: a worker's event loop.
/// Is this thread's event loop running right now? Outside of it, no ops work.
pub fn looping() -> bool {
    IN_LOOP.get()
}

/// For the C layer: whether a wait can be an op right now.
#[unsafe(no_mangle)]
extern "C" fn tenma_rs_looping() -> c_int {
    c_int::from(IN_LOOP.get())
}

pub fn in_loop<T>(run: impl FnOnce() -> T) -> T {
    IN_LOOP.set(true);
    let result = run();
    IN_LOOP.set(false);
    result
}

/// Starts an op and reports its result to PHP. A fatal error while resuming makes the worker
/// unusable. 0 if no event loop is running, e.g. at script startup or while the interpreter is
/// torn down; spawn_local would abort the process there.
/// `kind` and `detail` say what the op is (sleep, fetch GET https://…): the journal records them with the
/// result, and a replay checks them against what it recorded.
pub fn spawn(
    kind: &'static str,
    detail: String,
    future: impl Future<Output = OpResult> + 'static,
) -> u64 {
    spawn_with(kind, detail, future, true)
}

/// Like spawn, but tenma_op_cancel does not cancel the op: it runs to completion and keeps a script
/// alive until then. For ops that cannot be interrupted cleanly (file system on background threads).
pub fn spawn_uncancellable(
    kind: &'static str,
    detail: String,
    future: impl Future<Output = OpResult> + 'static,
) -> u64 {
    spawn_with(kind, detail, future, false)
}

/// An op of the durable machinery itself: never answered from a journal, never recorded in one.
pub fn spawn_plain(
    kind: &'static str,
    detail: String,
    future: impl Future<Output = OpResult> + 'static,
) -> u64 {
    spawn_task(kind, detail, future, true, None)
}

fn spawn_with(
    kind: &'static str,
    detail: String,
    future: impl Future<Output = OpResult> + 'static,
    abortable: bool,
) -> u64 {
    if !IN_LOOP.get() {
        return 0;
    }
    // A replay, or a durable function with entries left, answers from the journal instead of running the op
    if let Some(op) = crate::replay::intercept(kind, &detail) {
        return op;
    }
    if let Some(op) = crate::durable::intercept(kind, &detail) {
        return op;
    }
    spawn_task(kind, detail, future, abortable, crate::durable::starting())
}

fn spawn_task(
    kind: &'static str,
    detail: String,
    future: impl Future<Output = OpResult> + 'static,
    abortable: bool,
    durable: Option<String>,
) -> u64 {
    if !IN_LOOP.get() {
        return 0;
    }
    let op = php::next_id();
    let request = php::current_request();
    // The task owns the guard from the start: even a task aborted before it ever ran cleans up
    // RUNNING
    let finished = Finished(op);
    let task = tokio::task::spawn_local(async move {
        let _finished = finished;
        let mut result = future.await;
        // A durable function sees the result only once its journal holds it
        if let Some(key) = durable
            && let Err(error) = crate::durable::record(&key, kind, &detail, &result).await
        {
            result = OpResult::Error(format!("cannot write the durable journal: {error}"));
        }
        crate::journal::record_op(op, request, kind, &detail, &result);
        // After a fatal error the interpreter is unusable until the worker has restarted
        if !crate::broken() && php::complete(op, result).is_err() {
            crate::interpreter_broken();
        }
    });
    RUNNING.with_borrow_mut(|running| running.insert(op, abortable.then(|| task.abort_handle())));
    op
}

#[unsafe(no_mangle)]
extern "C" fn tenma_rs_sleep(milliseconds: i64) -> u64 {
    spawn("sleep", format!("{milliseconds} ms"), async move {
        tokio::time::sleep(Duration::from_millis(milliseconds.max(0) as u64)).await;
        OpResult::Null
    })
}

thread_local! {
    /// New per interpreter: its connection pool is tied to the event loop, which ends with the interpreter.
    static CLIENT: std::cell::RefCell<Option<reqwest::Client>> = const { std::cell::RefCell::new(None) };
}

/// Forgets the HTTP client and the running ops, e.g. when a worker restarts.
pub fn clear() {
    CLIENT.set(None);
}

fn client() -> reqwest::Client {
    CLIENT.with_borrow_mut(|client| {
        client
            .get_or_insert_with(|| {
                reqwest::Client::builder()
                    .connect_timeout(Duration::from_secs(10))
                    .build()
                    .expect("HTTP client does not start")
            })
            .clone()
    })
}

#[unsafe(no_mangle)]
extern "C" fn tenma_rs_fetch(
    method: *const u8,
    method_len: usize,
    url: *const u8,
    url_len: usize,
    headers: *const u8,
    headers_len: usize,
    body: *const u8,
    body_len: usize,
    timeout_ms: u64,
    path: *const u8,
    path_len: usize,
) -> u64 {
    let method = text(method, method_len);
    let url = text(url, url_len);
    let headers = text(headers, headers_len);
    let body = php::bytes(body, body_len).to_vec();
    // Empty: the body comes back as a string; otherwise it goes into this file
    let path = std::path::PathBuf::from(std::ffi::OsStr::from_bytes(php::bytes(path, path_len)));
    let detail = format!("{method} {url}");
    let request = {
        let client = client();
        // A typo must not turn into a GET that may well succeed
        let Ok(method) = reqwest::Method::from_bytes(method.as_bytes()) else {
            return spawn("fetch", detail, async {
                OpResult::Error("invalid method".into())
            });
        };
        let mut request = client
            .request(method, &url)
            .body(body)
            .timeout(Duration::from_millis(timeout_ms.max(1)));
        for line in headers.split('\0').filter(|line| !line.is_empty()) {
            if let Some((name, value)) = line.split_once(':') {
                request = request.header(name.trim(), value.trim());
            }
        }
        request
    };
    // The body stops where it would no longer fit under memory_limit, before Rust holds all of it
    let room = php::memory_room();
    spawn("fetch", detail, async move {
        let mut response = match request.send().await {
            Ok(response) => response,
            Err(error) => return OpResult::Error(fetch_error(&error, timeout_ms)),
        };
        if !path.as_os_str().is_empty() {
            return download(response, path, timeout_ms)
                .await
                .unwrap_or_else(|error| error);
        }
        let status = response.status().as_u16();
        let headers = response
            .headers()
            .iter()
            .map(|(name, value)| format!("{}: {}", name, String::from_utf8_lossy(value.as_bytes())))
            .collect();
        let too_big = || OpResult::Error("the response does not fit under memory_limit".into());
        if response
            .content_length()
            .is_some_and(|length| length > room as u64)
        {
            return too_big();
        }
        let mut body = Vec::new();
        loop {
            match response.chunk().await {
                Ok(Some(chunk)) if body.len() + chunk.len() > room => return too_big(),
                Ok(Some(chunk)) => body.extend_from_slice(&chunk),
                Ok(None) => break,
                Err(error) => return OpResult::Error(fetch_error(&error, timeout_ms)),
            }
        }
        OpResult::Http {
            status,
            headers,
            body,
        }
    })
}

/// Removes a download's file unless it is complete: on an error, a timeout or a cancelled request.
struct Partial(Option<std::path::PathBuf>);

impl Drop for Partial {
    fn drop(&mut self) {
        if let Some(path) = self.0.take() {
            let _ = std::fs::remove_file(path);
        }
    }
}

/// Streams the body into the file chunk by chunk, so memory holds one chunk at a time. Any status
/// is written; the caller checks it.
async fn download(
    mut response: reqwest::Response,
    path: std::path::PathBuf,
    timeout_ms: u64,
) -> Result<OpResult, OpResult> {
    use tokio::io::AsyncWriteExt;

    let status = response.status().as_u16();
    let headers = response
        .headers()
        .iter()
        .map(|(name, value)| format!("{}: {}", name, String::from_utf8_lossy(value.as_bytes())))
        .collect();
    let url = response.url().clone();
    let label = path.display().to_string();
    let io_error = |error: std::io::Error| OpResult::Error(format!("{label}: {error}"));

    let mut file = tokio::fs::File::create(&path).await.map_err(io_error)?;
    let mut partial = Partial(Some(path));
    loop {
        let chunk = response
            .chunk()
            .await
            .map_err(|error| error.with_url(url.clone()))
            .map_err(|error| OpResult::Error(fetch_error(&error, timeout_ms)))?;
        let Some(chunk) = chunk else {
            break;
        };
        file.write_all(&chunk).await.map_err(io_error)?;
    }
    file.flush().await.map_err(io_error)?;
    partial.0 = None;

    Ok(OpResult::Http {
        status,
        headers,
        body: Vec::new(),
    })
}

/// reqwest's own message names the URL but not what went wrong: say whether the request ran out of
/// time or found no server, with the cause from the bottom of the chain.
fn fetch_error(error: &reqwest::Error, timeout_ms: u64) -> String {
    let url = error.url().map_or_else(String::new, |url| url.to_string());
    let mut cause: &dyn std::error::Error = error;
    while let Some(source) = cause.source() {
        cause = source;
    }
    if error.is_connect() {
        format!("{url}: cannot connect: {cause}")
    } else if error.is_timeout() {
        let seconds = timeout_ms as f64 / 1000.0;
        format!("{url}: timed out after {seconds} s")
    } else {
        format!("{url}: {cause}")
    }
}

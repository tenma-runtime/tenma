//! Native plugins: shared libraries loaded with dlopen before any worker starts, talking to tenma
//! through the C ABI of tenma-plugin/tenma_plugin.h (ABI version 1). PHP reaches a plugin with
//! Tenma\Plugin::call(), await() and adopt(); a plugin reaches PHP with api->post() (a message to
//! the subscriptions of a channel) and writes to adopted WebSockets with api->send() from any thread.
//! A plugin lives as long as the process: it survives worker restarts and --watch reloads.

use std::ffi::{CStr, CString, c_char, c_int, c_void};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use tokio::sync::{mpsc, oneshot};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::protocol::CloseFrame;
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;

use crate::ops;
use crate::php::{OpResult, bytes, text};

pub const ABI: u32 = 1;

/// The result buffer of a synchronous call, filled through api->reply.
#[repr(C)]
pub struct Reply(Vec<u8>);

/// The token of an asynchronous call: the sender of its result, completed once.
#[repr(C)]
pub struct Token(oneshot::Sender<Result<Vec<u8>, String>>);

#[repr(C)]
pub struct Api {
    send: extern "C" fn(u64, *const u8, usize, c_int) -> c_int,
    buffered: extern "C" fn(u64) -> usize,
    close: extern "C" fn(u64, u16),
    post: extern "C" fn(*const c_char, usize, *const u8, usize),
    complete: extern "C" fn(*mut Token, *const u8, usize, c_int),
    reply: extern "C" fn(*mut Reply, *const u8, usize),
    log: extern "C" fn(c_int, *const c_char, usize),
}

#[repr(C)]
pub struct Table {
    name: *const c_char,
    call: Option<extern "C" fn(*const c_char, usize, *const u8, usize, *mut Reply) -> c_int>,
    call_async: Option<extern "C" fn(*const c_char, usize, *const u8, usize, *mut Token)>,
    adopt: Option<extern "C" fn(u64)>,
    message: Option<extern "C" fn(u64, *const u8, usize, c_int)>,
    closed: Option<extern "C" fn(u64)>,
    shutdown: Option<extern "C" fn()>,
}

/// A loaded plugin; the table lives in the library for the life of the process.
struct Plugin {
    name: String,
    path: String,
    table: &'static Table,
}

unsafe impl Send for Plugin {}
unsafe impl Sync for Plugin {}

static PLUGINS: OnceLock<Vec<Plugin>> = OnceLock::new();

/// A WebSocket a plugin adopted: where its writes go, and how much waits there.
struct Adopted {
    outgoing: mpsc::Sender<Message>,
    queued: Arc<AtomicUsize>,
}

static ADOPTED: Mutex<Option<std::collections::HashMap<u64, Adopted>>> = Mutex::new(None);

/// Connection IDs count for the whole process: request IDs count per worker and would collide.
static NEXT_CONN: AtomicU64 = AtomicU64::new(1);

fn adopted<T>(f: impl FnOnce(&mut std::collections::HashMap<u64, Adopted>) -> T) -> T {
    let mut guard = ADOPTED.lock().unwrap();
    f(guard.get_or_insert_with(Default::default))
}

static API: Api = Api {
    send: api_send,
    buffered: api_buffered,
    close: api_close,
    post: api_post,
    complete: api_complete,
    reply: api_reply,
    log: api_log,
};

extern "C" fn api_send(conn: u64, ptr: *const u8, len: usize, binary: c_int) -> c_int {
    let data = bytes(ptr, len).to_vec();
    let message = if binary != 0 {
        Message::binary(data)
    } else {
        match String::from_utf8(data) {
            Ok(text) => Message::text(text),
            Err(error) => Message::binary(error.into_bytes()),
        }
    };
    let size = message.len();
    adopted(|map| match map.get(&conn) {
        None => 2,
        Some(socket) => match socket.outgoing.try_send(message) {
            Ok(()) => {
                socket.queued.fetch_add(size, Ordering::Relaxed);
                0
            }
            Err(mpsc::error::TrySendError::Full(_)) => 1,
            Err(mpsc::error::TrySendError::Closed(_)) => {
                map.remove(&conn);
                2
            }
        },
    })
}

extern "C" fn api_buffered(conn: u64) -> usize {
    adopted(|map| {
        map.get(&conn)
            .map_or(0, |socket| socket.queued.load(Ordering::Relaxed))
    })
}

extern "C" fn api_close(conn: u64, code: u16) {
    adopted(|map| {
        if let Some(socket) = map.remove(&conn) {
            let _ = socket.outgoing.try_send(Message::Close(Some(CloseFrame {
                code: CloseCode::from(code),
                reason: "".into(),
            })));
        }
    });
}

extern "C" fn api_post(channel: *const c_char, channel_len: usize, ptr: *const u8, len: usize) {
    crate::pubsub::deliver(bytes(channel.cast(), channel_len), bytes(ptr, len));
}

extern "C" fn api_complete(token: *mut Token, ptr: *const u8, len: usize, error: c_int) {
    if token.is_null() {
        return;
    }
    let token = unsafe { Box::from_raw(token) };
    let data = bytes(ptr, len).to_vec();
    let _ = token.0.send(if error == 0 {
        Ok(data)
    } else {
        Err(String::from_utf8_lossy(&data).into_owned())
    });
}

extern "C" fn api_reply(out: *mut Reply, ptr: *const u8, len: usize) {
    if let Some(reply) = unsafe { out.as_mut() } {
        reply.0 = bytes(ptr, len).to_vec();
    }
}

extern "C" fn api_log(level: c_int, ptr: *const c_char, len: usize) {
    let text = String::from_utf8_lossy(bytes(ptr.cast(), len));
    let level = match level {
        0 => "error",
        1 => "warning",
        _ => "info",
    };
    eprintln!("tenma: plugin {level}: {text}");
}

/// Loads every library and calls its entry point. Once, before the workers start; a library that does
/// not load or speaks another ABI is an error.
pub fn load(paths: &[String]) -> Result<(), String> {
    let mut plugins: Vec<Plugin> = Vec::new();
    for path in paths {
        let canonical = std::fs::canonicalize(path)
            .map_err(|error| format!("--plugin {path}: {error}"))?
            .display()
            .to_string();
        if plugins.iter().any(|plugin| plugin.path == canonical) {
            continue;
        }
        let file =
            CString::new(canonical.as_str()).map_err(|_| "plugin path contains a null byte")?;
        let handle = unsafe { libc::dlopen(file.as_ptr(), libc::RTLD_NOW | libc::RTLD_LOCAL) };
        if handle.is_null() {
            let reason = unsafe { CStr::from_ptr(libc::dlerror()) }
                .to_string_lossy()
                .into_owned();
            return Err(format!("--plugin {path}: {reason}"));
        }
        let symbol = unsafe { libc::dlsym(handle, c"tenma_plugin_init".as_ptr()) };
        if symbol.is_null() {
            return Err(format!("--plugin {path}: no tenma_plugin_init"));
        }
        let init: extern "C" fn(*const Api, u32) -> *const Table = unsafe {
            std::mem::transmute::<*mut c_void, extern "C" fn(*const Api, u32) -> *const Table>(
                symbol,
            )
        };
        let table = init(&API, ABI);
        if table.is_null() {
            return Err(format!(
                "--plugin {path}: the plugin refused ABI version {ABI} (built for another tenma?)"
            ));
        }
        let table: &'static Table = unsafe { &*table };
        if table.name.is_null() {
            return Err(format!("--plugin {path}: the plugin has no name"));
        }
        let name = unsafe { CStr::from_ptr(table.name) }
            .to_string_lossy()
            .into_owned();
        if plugins.iter().any(|plugin| plugin.name == name) {
            return Err(format!(
                "--plugin {path}: a plugin named {name} is already loaded"
            ));
        }
        eprintln!("tenma: plugin {name} loaded from {path}");
        plugins.push(Plugin {
            name,
            path: canonical,
            table,
        });
    }
    PLUGINS
        .set(plugins)
        .map_err(|_| "plugins loaded twice".to_owned())
}

fn find(name: &str) -> Result<&'static Plugin, String> {
    PLUGINS
        .get()
        .and_then(|plugins| plugins.iter().find(|plugin| plugin.name == name))
        .ok_or_else(|| format!("no plugin named {name} is loaded (tenma --plugin)"))
}

/// tenma stops, after PHP's stop handlers: every plugin's shutdown(), in loading order.
pub fn shutdown() {
    for plugin in PLUGINS.get().into_iter().flatten() {
        if let Some(shutdown) = plugin.table.shutdown {
            shutdown();
        }
    }
}

/// A synchronous call: the result bytes, or Err with the plugin's message.
pub fn call(name: &str, function: &str, input: &[u8]) -> Result<Vec<u8>, String> {
    let plugin = find(name)?;
    let Some(call) = plugin.table.call else {
        return Err(format!("plugin {name} has no call()"));
    };
    let mut reply = Reply(Vec::new());
    let status = call(
        function.as_ptr().cast(),
        function.len(),
        input.as_ptr(),
        input.len(),
        &mut reply,
    );
    if status == 0 {
        Ok(reply.0)
    } else {
        Err(if reply.0.is_empty() {
            format!("plugin {name}: {function} failed with status {status}")
        } else {
            String::from_utf8_lossy(&reply.0).into_owned()
        })
    }
}

/// `tenma_plugin_call`: into a buffer the C layer frees with tenma_rs_plugin_free; status 0 or 1 (error message in the buffer).
#[unsafe(no_mangle)]
extern "C" fn tenma_rs_plugin_call(
    name: *const u8,
    name_len: usize,
    function: *const u8,
    function_len: usize,
    input: *const u8,
    input_len: usize,
    out: *mut *mut u8,
    out_len: *mut usize,
) -> c_int {
    let result = call(
        &text(name, name_len),
        &text(function, function_len),
        bytes(input, input_len),
    );
    let (data, status) = match result {
        Ok(data) => (data, 0),
        Err(message) => (message.into_bytes(), 1),
    };
    let mut data = std::mem::ManuallyDrop::new(data.into_boxed_slice());
    unsafe {
        *out = data.as_mut_ptr();
        *out_len = data.len();
    }
    status
}

#[unsafe(no_mangle)]
extern "C" fn tenma_rs_plugin_free(ptr: *mut u8, len: usize) {
    if !ptr.is_null() {
        drop(unsafe { Box::from_raw(std::ptr::slice_from_raw_parts_mut(ptr, len)) });
    }
}

/// `tenma_op_plugin_await`: starts call_async and an op that waits for api->complete.
#[unsafe(no_mangle)]
extern "C" fn tenma_rs_plugin_await(
    name: *const u8,
    name_len: usize,
    function: *const u8,
    function_len: usize,
    input: *const u8,
    input_len: usize,
) -> u64 {
    let name = text(name, name_len);
    let function = text(function, function_len);
    let input = bytes(input, input_len).to_vec();
    let detail = format!("{name} {function}");
    let plugin = match find(&name) {
        Ok(plugin) => plugin,
        Err(message) => {
            return ops::spawn("plugin", detail, async move { OpResult::Error(message) });
        }
    };
    let Some(call_async) = plugin.table.call_async else {
        return ops::spawn("plugin", detail, async move {
            OpResult::Error(format!("plugin {name} has no call_async()"))
        });
    };
    let (sender, receiver) = oneshot::channel();
    let token = Box::into_raw(Box::new(Token(sender)));
    call_async(
        function.as_ptr().cast(),
        function.len(),
        input.as_ptr(),
        input.len(),
        token,
    );
    ops::spawn("plugin", detail, async move {
        match receiver.await {
            Ok(Ok(data)) => OpResult::Text(data),
            Ok(Err(message)) => OpResult::Error(message),
            Err(_) => OpResult::Error("the plugin dropped the call".into()),
        }
    })
}

/// `tenma_plugin_adopt`: hands the WebSocket of request `rid` to the plugin. The connection ID, unique
/// in the process, or 0 with the reason in the log.
#[unsafe(no_mangle)]
extern "C" fn tenma_rs_plugin_adopt(name: *const u8, name_len: usize, rid: u64) -> u64 {
    let name = text(name, name_len);
    let plugin = match find(&name) {
        Ok(plugin) => plugin,
        Err(message) => {
            eprintln!("tenma: {message}");
            return 0;
        }
    };
    let (Some(adopt), Some(message), Some(closed)) = (
        plugin.table.adopt,
        plugin.table.message,
        plugin.table.closed,
    ) else {
        eprintln!("tenma: plugin {name} does not adopt connections");
        return 0;
    };
    let Some((outgoing, incoming, queued, unread)) = crate::streams::take_socket(rid) else {
        eprintln!("tenma: request {rid} has no open WebSocket to adopt");
        return 0;
    };
    let conn = NEXT_CONN.fetch_add(1, Ordering::Relaxed);
    adopted(|map| map.insert(conn, Adopted { outgoing, queued }));
    adopt(conn);
    // Incoming messages go to the plugin on the worker's loop; the plugin writes from wherever it likes.
    // The guard tells the plugin when the connection is over, also when the worker restarts and drops
    // this task
    tokio::task::spawn_local(async move {
        let _over = Over { conn, closed };
        let mut incoming = incoming;
        while let Some((data, binary)) = incoming.recv().await {
            // Read: the limit on unread bytes counts down as for PHP's receive()
            unread.set(unread.get().saturating_sub(data.len()));
            message(conn, data.as_ptr(), data.len(), c_int::from(binary));
        }
    });
    conn
}

/// An adopted connection ends, however its task ends: the plugin hears closed() once.
struct Over {
    conn: u64,
    closed: extern "C" fn(u64),
}

impl Drop for Over {
    fn drop(&mut self) {
        adopted(|map| map.remove(&self.conn));
        (self.closed)(self.conn);
    }
}

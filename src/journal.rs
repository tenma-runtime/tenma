//! The event journal: everything a worker received from outside, in the order PHP saw it. A request,
//! the result of every op, a client leaving, and the values of time() and random_int() that the C layer
//! asks for. `--record DIR` writes one file per worker life (src/main.rs), `tenma replay FILE` plays it
//! into a fresh interpreter without any real I/O (src/replay.rs), and durable functions keep a journal
//! per function in Postgres (src/durable.rs).
//!
//! Format: entries one after another, each as a tag byte, a u32 length and the payload; strings and
//! byte fields inside as u32 length and bytes, numbers little-endian. The first entry is the header.
//! Ops and values carry the request they belong to (0: none), so one request can be played alone.
//!
//! A journal is cut at --record-max-size: the worker goes on in a new file that starts with the same
//! header (marked partial) and the startup part of the old one, everything up to the first request.
//! Such a file plays one request at a time (`tenma replay FILE --request N`).

use std::cell::RefCell;
use std::io::Write;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};

use crate::php::{OpResult, bytes};

unsafe extern "C" {
    fn tenma_set_journal_mode(mode: std::ffi::c_int);
}

/// Tells the C layer what this thread does with the wrapped functions: 0 nothing, 1 record, 2 replay.
pub fn set_mode(mode: i32) {
    unsafe { tenma_set_journal_mode(mode) }
}

/// A value a wrapped PHP function returned, serialized, while recording.
#[unsafe(no_mangle)]
extern "C" fn tenma_rs_journal_value(
    name: *const u8,
    name_len: usize,
    value: *const u8,
    len: usize,
) {
    let name = String::from_utf8_lossy(bytes(name, name_len)).into_owned();
    let value = bytes(value, len);
    crate::durable::keep_value(&name, value);
    if recording() {
        record(Entry::Value {
            request: crate::php::current_request(),
            name,
            value: value.to_vec(),
        });
    }
}

/// A replay: the recorded value of the wrapped function `name`, if the journal has it next; the C
/// layer frees it with tenma_rs_journal_free.
#[unsafe(no_mangle)]
extern "C" fn tenma_rs_journal_take(
    name: *const u8,
    name_len: usize,
    value: *mut *mut u8,
    len: *mut usize,
) -> std::ffi::c_int {
    let name = String::from_utf8_lossy(bytes(name, name_len)).into_owned();
    match crate::durable::take_value(&name).or_else(|| crate::replay::take_value(&name)) {
        Some(bytes) => {
            let mut bytes = std::mem::ManuallyDrop::new(bytes.into_boxed_slice());
            unsafe {
                *value = bytes.as_mut_ptr();
                *len = bytes.len();
            }
            1
        }
        None => 0,
    }
}

#[unsafe(no_mangle)]
extern "C" fn tenma_rs_journal_free(value: *mut u8, len: usize) {
    if !value.is_null() {
        drop(unsafe { Box::from_raw(std::ptr::slice_from_raw_parts_mut(value, len)) });
    }
}

/// A blocking function ran: noted while recording, a warning while replaying.
#[unsafe(no_mangle)]
extern "C" fn tenma_rs_journal_note(name: *const u8, name_len: usize) {
    let name = String::from_utf8_lossy(bytes(name, name_len)).into_owned();
    if recording() {
        with(|recorder| {
            if recorder.blocking.insert(name.clone()) {
                recorder.write(&Entry::Note(format!(
                    "{name}() blocks the worker and is not replayed: its effects may differ"
                )));
            }
        });
    } else {
        crate::replay::blocking(&name);
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct RawRequest {
    pub handler: u32,
    pub method: Vec<u8>,
    pub uri: Vec<u8>,
    pub headers: Vec<(Vec<u8>, Vec<u8>)>,
    pub body: Vec<u8>,
    pub remote: String,
    pub local: String,
    pub https: bool,
    pub protocol: Vec<u8>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Reply {
    Full {
        status: u16,
        headers: Vec<(Vec<u8>, Vec<u8>)>,
        body: Vec<u8>,
    },
    /// The head of a streamed response; its chunks are the writes that follow.
    Stream {
        status: u16,
        headers: Vec<(Vec<u8>, Vec<u8>)>,
    },
    /// A WebSocket.
    Upgrade,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Entry {
    /// JSON: tenma and PHP version, script, argv, env, the time.
    Header(String),
    /// The event loop runs: what the script scheduled starts.
    Started,
    Request {
        rid: u64,
        request: RawRequest,
    },
    Response {
        rid: u64,
        reply: Reply,
    },
    /// An op finished: what it was, the request it belongs to (0: none) and what PHP got.
    Op {
        op: u64,
        request: u64,
        kind: String,
        detail: String,
        result: OpResult,
    },
    /// The client of a request left.
    Gone {
        rid: u64,
    },
    /// A value PHP got from a function the C layer wraps (time(), random_int()), serialized, and the
    /// request whose code asked (0: none).
    Value {
        request: u64,
        name: String,
        value: Vec<u8>,
    },
    /// The files PHP loaded, with their SHA-256; written at the end.
    Files(Vec<(String, String)>),
    /// Something the reader should know: a cut body, a blocking function, an incomplete journal.
    Note(String),
}

const HEADER: u8 = 1;
const STARTED: u8 = 2;
const REQUEST: u8 = 3;
const RESPONSE: u8 = 4;
const OP: u8 = 5;
const GONE: u8 = 6;
const VALUE: u8 = 7;
const FILES: u8 = 8;
const NOTE: u8 = 9;
/// Op and value with their request; 5 and 7 are the same without, from the first journals of 0.2.0.
const OP_OF_REQUEST: u8 = 10;
const VALUE_OF_REQUEST: u8 = 11;

fn put_u32(out: &mut Vec<u8>, value: u32) {
    out.extend_from_slice(&value.to_le_bytes());
}

fn put_u64(out: &mut Vec<u8>, value: u64) {
    out.extend_from_slice(&value.to_le_bytes());
}

fn put_bytes(out: &mut Vec<u8>, bytes: &[u8]) {
    put_u32(out, bytes.len() as u32);
    out.extend_from_slice(bytes);
}

fn put_pairs(out: &mut Vec<u8>, pairs: &[(Vec<u8>, Vec<u8>)]) {
    put_u32(out, pairs.len() as u32);
    for (name, value) in pairs {
        put_bytes(out, name);
        put_bytes(out, value);
    }
}

fn put_result(out: &mut Vec<u8>, result: &OpResult) {
    match result {
        OpResult::Null => out.push(0),
        OpResult::Text(text) => {
            out.push(1);
            put_bytes(out, text);
        }
        OpResult::Error(message) => {
            out.push(2);
            put_bytes(out, message.as_bytes());
        }
        OpResult::Failure { message, code } => {
            out.push(3);
            put_bytes(out, message.as_bytes());
            out.extend_from_slice(&code.to_le_bytes());
        }
        OpResult::Http {
            status,
            headers,
            body,
        } => {
            out.push(4);
            out.extend_from_slice(&status.to_le_bytes());
            put_u32(out, headers.len() as u32);
            for line in headers {
                put_bytes(out, line.as_bytes());
            }
            put_bytes(out, body);
        }
    }
}

/// One entry as bytes, with its tag and length.
pub fn encode(entry: &Entry) -> Vec<u8> {
    let mut payload = Vec::new();
    let tag = match entry {
        Entry::Header(json) => {
            put_bytes(&mut payload, json.as_bytes());
            HEADER
        }
        Entry::Started => STARTED,
        Entry::Request { rid, request } => {
            put_u64(&mut payload, *rid);
            put_u32(&mut payload, request.handler);
            put_bytes(&mut payload, &request.method);
            put_bytes(&mut payload, &request.uri);
            put_pairs(&mut payload, &request.headers);
            put_bytes(&mut payload, &request.body);
            put_bytes(&mut payload, request.remote.as_bytes());
            put_bytes(&mut payload, request.local.as_bytes());
            payload.push(u8::from(request.https));
            put_bytes(&mut payload, &request.protocol);
            REQUEST
        }
        Entry::Response { rid, reply } => {
            put_u64(&mut payload, *rid);
            match reply {
                Reply::Full {
                    status,
                    headers,
                    body,
                } => {
                    payload.push(0);
                    payload.extend_from_slice(&status.to_le_bytes());
                    put_pairs(&mut payload, headers);
                    put_bytes(&mut payload, body);
                }
                Reply::Stream { status, headers } => {
                    payload.push(1);
                    payload.extend_from_slice(&status.to_le_bytes());
                    put_pairs(&mut payload, headers);
                }
                Reply::Upgrade => payload.push(2),
            }
            RESPONSE
        }
        Entry::Op {
            op,
            request,
            kind,
            detail,
            result,
        } => {
            put_u64(&mut payload, *op);
            put_u64(&mut payload, *request);
            put_bytes(&mut payload, kind.as_bytes());
            put_bytes(&mut payload, detail.as_bytes());
            put_result(&mut payload, result);
            OP_OF_REQUEST
        }
        Entry::Gone { rid } => {
            put_u64(&mut payload, *rid);
            GONE
        }
        Entry::Value {
            request,
            name,
            value,
        } => {
            put_u64(&mut payload, *request);
            put_bytes(&mut payload, name.as_bytes());
            put_bytes(&mut payload, value);
            VALUE_OF_REQUEST
        }
        Entry::Files(files) => {
            put_u32(&mut payload, files.len() as u32);
            for (path, hash) in files {
                put_bytes(&mut payload, path.as_bytes());
                put_bytes(&mut payload, hash.as_bytes());
            }
            FILES
        }
        Entry::Note(text) => {
            put_bytes(&mut payload, text.as_bytes());
            NOTE
        }
    };
    let mut out = Vec::with_capacity(payload.len() + 5);
    out.push(tag);
    put_u32(&mut out, payload.len() as u32);
    out.extend_from_slice(&payload);
    out
}

/// Reads fields out of a payload.
struct Cursor<'a> {
    data: &'a [u8],
    at: usize,
}

impl<'a> Cursor<'a> {
    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        let slice = self.data.get(self.at..self.at + n)?;
        self.at += n;
        Some(slice)
    }

    fn u8(&mut self) -> Option<u8> {
        self.take(1).map(|b| b[0])
    }

    fn u16(&mut self) -> Option<u16> {
        self.take(2).map(|b| u16::from_le_bytes([b[0], b[1]]))
    }

    fn u32(&mut self) -> Option<u32> {
        self.take(4)
            .map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }

    fn u64(&mut self) -> Option<u64> {
        self.take(8)
            .map(|b| u64::from_le_bytes(b.try_into().unwrap()))
    }

    fn bytes(&mut self) -> Option<Vec<u8>> {
        let len = self.u32()? as usize;
        self.take(len).map(<[u8]>::to_vec)
    }

    fn string(&mut self) -> Option<String> {
        self.bytes()
            .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
    }

    fn pairs(&mut self) -> Option<Vec<(Vec<u8>, Vec<u8>)>> {
        let count = self.u32()? as usize;
        (0..count)
            .map(|_| Some((self.bytes()?, self.bytes()?)))
            .collect()
    }

    fn result(&mut self) -> Option<OpResult> {
        Some(match self.u8()? {
            0 => OpResult::Null,
            1 => OpResult::Text(self.bytes()?),
            2 => OpResult::Error(self.string()?),
            3 => OpResult::Failure {
                message: self.string()?,
                code: i32::from_le_bytes(self.take(4)?.try_into().unwrap()),
            },
            4 => {
                let status = self.u16()?;
                let count = self.u32()? as usize;
                let headers = (0..count)
                    .map(|_| self.string())
                    .collect::<Option<Vec<_>>>()?;
                OpResult::Http {
                    status,
                    headers,
                    body: self.bytes()?,
                }
            }
            _ => return None,
        })
    }
}

/// The next entry of `data` from `at`, and where the one after it starts. None at the end or when
/// the data is cut off (a worker that was killed while writing).
pub fn decode(data: &[u8], at: usize) -> Option<(Entry, usize)> {
    let mut head = Cursor { data, at };
    let tag = head.u8()?;
    let len = head.u32()? as usize;
    let payload = head.take(len)?;
    let next = head.at;
    let mut c = Cursor {
        data: payload,
        at: 0,
    };
    let entry = match tag {
        HEADER => Entry::Header(c.string()?),
        STARTED => Entry::Started,
        REQUEST => Entry::Request {
            rid: c.u64()?,
            request: RawRequest {
                handler: c.u32()?,
                method: c.bytes()?,
                uri: c.bytes()?,
                headers: c.pairs()?,
                body: c.bytes()?,
                remote: c.string()?,
                local: c.string()?,
                https: c.u8()? != 0,
                protocol: c.bytes()?,
            },
        },
        RESPONSE => {
            let rid = c.u64()?;
            let reply = match c.u8()? {
                0 => Reply::Full {
                    status: c.u16()?,
                    headers: c.pairs()?,
                    body: c.bytes()?,
                },
                1 => Reply::Stream {
                    status: c.u16()?,
                    headers: c.pairs()?,
                },
                _ => Reply::Upgrade,
            };
            Entry::Response { rid, reply }
        }
        OP => Entry::Op {
            op: c.u64()?,
            request: 0,
            kind: c.string()?,
            detail: c.string()?,
            result: c.result()?,
        },
        OP_OF_REQUEST => Entry::Op {
            op: c.u64()?,
            request: c.u64()?,
            kind: c.string()?,
            detail: c.string()?,
            result: c.result()?,
        },
        GONE => Entry::Gone { rid: c.u64()? },
        VALUE => Entry::Value {
            request: 0,
            name: c.string()?,
            value: c.bytes()?,
        },
        VALUE_OF_REQUEST => Entry::Value {
            request: c.u64()?,
            name: c.string()?,
            value: c.bytes()?,
        },
        FILES => {
            let count = c.u32()? as usize;
            Entry::Files(
                (0..count)
                    .map(|_| Some((c.string()?, c.string()?)))
                    .collect::<Option<Vec<_>>>()?,
            )
        }
        NOTE => Entry::Note(c.string()?),
        _ => return None,
    };
    Some((entry, next))
}

/// The request an entry belongs to, if it belongs to one.
pub fn request_of(entry: &Entry) -> Option<u64> {
    match entry {
        Entry::Request { rid, .. } | Entry::Response { rid, .. } | Entry::Gone { rid } => {
            Some(*rid)
        }
        Entry::Op { request, .. } | Entry::Value { request, .. } if *request != 0 => Some(*request),
        _ => None,
    }
}

/// Every entry of a journal file; stops at the first entry that is cut off.
pub fn read_all(data: &[u8]) -> Vec<Entry> {
    let mut entries = Vec::new();
    let mut at = 0;
    while let Some((entry, next)) = decode(data, at) {
        entries.push(entry);
        at = next;
    }
    entries
}

/// Where this worker writes its journal, if `--record` is on.
pub struct Recorder {
    file: std::io::BufWriter<std::fs::File>,
    path: std::path::PathBuf,
    /// `--record-on error`: the file stays only if a request failed or PHP did.
    only_on_error: bool,
    errored: bool,
    /// Request bodies and op results above this are cut, and the entry marked.
    max_body: usize,
    /// Headers whose values are replaced by "[redacted]".
    redact: Vec<String>,
    /// Blocking functions already noted.
    blocking: std::collections::HashSet<String>,
    /// The header, and every entry before the first request: the start of each file after a cut.
    header: String,
    prefix: Vec<u8>,
    prefix_entries: usize,
    prefix_done: bool,
    /// Bytes in the current file, those of its header and startup part, and where --record-max-size
    /// cuts it: the startup part does not count, or a large one would cut after every response.
    size: u64,
    startup_size: u64,
    max_size: u64,
    /// The file name without ".trace", and the number of the current file.
    base: std::path::PathBuf,
    part: u32,
    /// The file before the current one, kept unless --record-on error deletes it: two files of history.
    previous: Option<std::path::PathBuf>,
}

thread_local! {
    static RECORDER: RefCell<Option<Recorder>> = const { RefCell::new(None) };
    /// The files PHP loaded in this interpreter life, as the C layer reports them.
    static LOADED: RefCell<Vec<String>> = const { RefCell::new(Vec::new()) };
}

/// A file PHP loaded, from the C layer.
pub fn loaded(path: &[u8]) {
    LOADED.with_borrow_mut(|loaded| loaded.push(String::from_utf8_lossy(path).into_owned()));
}

/// The loaded files with their hashes, and the list starts over for the next interpreter life.
pub fn loaded_files() -> Vec<(String, String)> {
    LOADED
        .with_borrow_mut(std::mem::take)
        .into_iter()
        .map(|path| {
            let hash = hash_file(&path);
            (path, hash)
        })
        .collect()
}

/// `--record`: what the worker writes with.
#[derive(Clone)]
pub struct Settings {
    pub directory: std::path::PathBuf,
    pub only_on_error: bool,
    pub max_body: usize,
    pub redact: Vec<String>,
    /// A file is cut at this size; the worker goes on in the next one.
    pub max_size: u64,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            directory: std::path::PathBuf::new(),
            only_on_error: false,
            max_body: 1 << 20,
            redact: Vec::new(),
            max_size: 50 << 20,
        }
    }
}

/// The header of a journal: what ran where, as JSON.
pub fn header(argv: &[String], watch: bool) -> String {
    let (defines, ini_path) = crate::php::SETTINGS.lock().unwrap().clone();
    let env: std::collections::BTreeMap<String, String> = std::env::vars().collect();
    let script = std::fs::canonicalize(&argv[0])
        .map_or_else(|_| argv[0].clone(), |path| path.display().to_string());
    serde_json::json!({
        "tenma": env!("CARGO_PKG_VERSION"),
        "php": env!("TENMA_PHP_VERSION"),
        "script": script,
        "argv": argv,
        "env": env,
        "cwd": std::env::current_dir().map_or(String::new(), |dir| dir.display().to_string()),
        // What the PHP code can tell from its settings: --watch (tenma.watch), -d and -c
        "watch": watch,
        "defines": defines,
        "ini_path": ini_path,
        "time": std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |since| since.as_secs()),
    })
    .to_string()
}

/// A journal file only its owner can read: it holds the environment, headers and bodies.
fn create(path: &std::path::Path) -> std::io::Result<std::fs::File> {
    std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)
}

/// Opens this worker's journal for a new interpreter life.
pub fn start(settings: &Settings, worker: usize, header: String) -> Result<(), String> {
    let started = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| since.as_millis());
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&settings.directory)
        .map_err(|error| format!("--record {}: {error}", settings.directory.display()))?;
    let base = settings
        .directory
        .join(format!("{}-{worker}-{started}", std::process::id()));
    let path = base.with_extension("trace");
    let file = create(&path).map_err(|error| format!("{}: {error}", path.display()))?;
    let mut recorder = Recorder {
        file: std::io::BufWriter::new(file),
        path,
        only_on_error: settings.only_on_error,
        errored: false,
        max_body: settings.max_body,
        redact: settings.redact.clone(),
        blocking: std::collections::HashSet::new(),
        header: header.clone(),
        prefix: Vec::new(),
        prefix_entries: 0,
        prefix_done: false,
        size: 0,
        startup_size: 0,
        max_size: settings.max_size,
        base,
        part: 1,
        previous: None,
    };
    recorder.write(&Entry::Header(header));
    recorder.prefix.clear();
    recorder.prefix_entries = 0;
    RECORDER.with_borrow_mut(|slot| *slot = Some(recorder));
    set_mode(1);
    Ok(())
}

impl Recorder {
    fn write(&mut self, entry: &Entry) {
        let bytes = encode(entry);
        let _ = self.file.write_all(&bytes);
        self.size += bytes.len() as u64;
        if !self.prefix_done {
            self.prefix.extend_from_slice(&bytes);
            self.prefix_entries += 1;
        }
    }

    /// The file is at --record-max-size: the worker goes on in the next one, which starts with the same
    /// header, marked partial, and the startup part. Of the files before, the last one stays; one with a
    /// failure always stays, and with --record-on error one without is deleted.
    fn cut(&mut self) {
        let _ = self.file.flush();
        let old = self.path.clone();
        if self.errored {
            eprintln!("tenma: journal written to {}", old.display());
        } else if self.only_on_error {
            let _ = std::fs::remove_file(&old);
        } else if let Some(before) = self.previous.replace(old.clone()) {
            let _ = std::fs::remove_file(before);
        }
        self.part += 1;
        let name = format!(
            "{}-{}.trace",
            self.base
                .file_name()
                .map_or_else(String::new, |name| name.to_string_lossy().into_owned()),
            self.part
        );
        self.path = self.base.with_file_name(name);
        let Ok(file) = create(&self.path) else {
            return;
        };
        self.file = std::io::BufWriter::new(file);
        self.size = 0;
        self.errored = false;
        let mut header: serde_json::Value = serde_json::from_str(&self.header).unwrap_or_default();
        header["partial"] = serde_json::json!(true);
        header["part"] = serde_json::json!(self.part);
        header["continues"] = serde_json::json!(
            old.file_name()
                .map(|name| name.to_string_lossy().into_owned())
        );
        // Where the startup part ends: what follows are requests that began before the cut, and theirs
        header["startup_entries"] = serde_json::json!(self.prefix_entries);
        let header = encode(&Entry::Header(header.to_string()));
        let _ = self.file.write_all(&header);
        let prefix = std::mem::take(&mut self.prefix);
        let _ = self.file.write_all(&prefix);
        self.size += (header.len() + prefix.len()) as u64;
        self.startup_size = self.size;
        self.prefix = prefix;
        self.write(&Entry::Note(format!(
            "the requests before this file are in {}: play one request at a time with --request",
            old.display()
        )));
        let _ = self.file.flush();
    }
}

/// Whether this worker records.
pub fn recording() -> bool {
    RECORDER.with_borrow(Option::is_some)
}

fn with(f: impl FnOnce(&mut Recorder)) {
    RECORDER.with_borrow_mut(|slot| {
        if let Some(recorder) = slot {
            f(recorder);
        }
    });
}

pub fn record(entry: Entry) {
    with(|recorder| recorder.write(&entry));
}

/// A request as it enters PHP; a body above --record-max-body is cut and the request marked.
pub fn record_request(rid: u64, mut request: RawRequest) {
    with(|recorder| {
        if request.body.len() > recorder.max_body {
            request.body.truncate(recorder.max_body);
            recorder.write(&Entry::Note(format!(
                "request {rid}: body cut at {} bytes, not replayable",
                recorder.max_body
            )));
        }
        for (name, value) in &mut request.headers {
            if recorder
                .redact
                .iter()
                .any(|redacted| redacted.as_bytes().eq_ignore_ascii_case(name))
            {
                *value = b"[redacted]".to_vec();
            }
        }
        // The startup part ends with the first request
        if !recorder.prefix_done {
            recorder.startup_size = recorder.size;
        }
        recorder.prefix_done = true;
        recorder.write(&Entry::Request { rid, request });
    });
}

pub fn record_response(rid: u64, reply: Reply) {
    with(|recorder| {
        if let Reply::Full { status, .. } | Reply::Stream { status, .. } = &reply
            && *status >= 500
        {
            recorder.errored = true;
        }
        recorder.write(&Entry::Response { rid, reply });
        // Whole up to here on disk: a journal is worth reading while its worker still runs
        let _ = recorder.file.flush();
        if recorder.size.saturating_sub(recorder.startup_size) > recorder.max_size {
            recorder.cut();
        }
    });
}

/// An op finished; a result above --record-max-body is cut and marked.
pub fn record_op(op: u64, request: u64, kind: &str, detail: &str, result: &OpResult) {
    with(|recorder| {
        let max = recorder.max_body;
        let too_big = match result {
            OpResult::Text(text) => text.len() > max,
            OpResult::Http { body, .. } => body.len() > max,
            _ => false,
        };
        if too_big {
            recorder.write(&Entry::Note(format!(
                "op {op} ({kind}): result above {max} bytes, cut, not replayable"
            )));
        }
        let result = match result {
            OpResult::Text(text) if too_big => OpResult::Text(text[..max].to_vec()),
            OpResult::Http {
                status,
                headers,
                body,
            } if too_big => OpResult::Http {
                status: *status,
                headers: headers.clone(),
                body: body[..max].to_vec(),
            },
            other => other.clone(),
        };
        recorder.write(&Entry::Op {
            op,
            request,
            kind: kind.to_owned(),
            detail: detail.to_owned(),
            result,
        });
    });
}

/// PHP failed fatally: with --record-on error the journal stays.
pub fn record_fatal() {
    with(|recorder| {
        recorder.errored = true;
        recorder.write(&Entry::Note("fatal error".into()));
        let _ = recorder.file.flush();
    });
}

/// The interpreter life ends: the files it loaded go in, then the journal is closed, and with
/// --record-on error deleted unless something failed.
pub fn finish(files: Vec<(String, String)>) {
    let Some(mut recorder) = RECORDER.with_borrow_mut(Option::take) else {
        return;
    };
    set_mode(0);
    recorder.write(&Entry::Files(files));
    let _ = recorder.file.flush();
    if recorder.only_on_error && !recorder.errored {
        let _ = std::fs::remove_file(&recorder.path);
    } else {
        eprintln!("tenma: journal written to {}", recorder.path.display());
    }
    // Durable functions still need the wrapped functions to report
    if crate::durable::enabled() {
        set_mode(1);
    }
}

/// A URL or connection string with its password replaced: what goes into an op's detail.
pub fn redact(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find("://") {
        let (before, after) = rest.split_at(at + 3);
        out.push_str(before);
        let authority_end = after.find(['/', '?', ' ']).unwrap_or(after.len());
        let authority = &after[..authority_end];
        if let Some(at_sign) = authority.rfind('@')
            && let Some(colon) = authority[..at_sign].find(':')
        {
            out.push_str(&authority[..colon + 1]);
            out.push_str("***");
            out.push_str(&authority[at_sign..]);
        } else {
            out.push_str(authority);
        }
        rest = &after[authority_end..];
    }
    out.push_str(rest);
    // libpq style: password=secret
    if let Some(at) = out.find("password=") {
        let end = out[at + 9..].find(' ').map_or(out.len(), |n| at + 9 + n);
        out.replace_range(at + 9..end, "***");
    }
    out
}

/// SHA-256 of a file as hex, or "" if it cannot be read.
pub fn hash_file(path: &str) -> String {
    match std::fs::read(path) {
        Ok(data) => {
            let digest = ring::digest::digest(&ring::digest::SHA256, &data);
            digest
                .as_ref()
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect()
        }
        Err(_) => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn entries_survive_a_round_trip() {
        let entries = vec![
            Entry::Header("{\"tenma\":\"0.2.0\"}".into()),
            Entry::Started,
            Entry::Request {
                rid: 7,
                request: RawRequest {
                    handler: 0,
                    method: b"POST".to_vec(),
                    uri: b"/a?b=c".to_vec(),
                    headers: vec![(b"host".to_vec(), b"x".to_vec())],
                    body: b"\0body".to_vec(),
                    remote: "127.0.0.1:5".into(),
                    local: "127.0.0.1:80".into(),
                    https: true,
                    protocol: b"HTTP/2".to_vec(),
                },
            },
            Entry::Op {
                op: 8,
                request: 7,
                kind: "fetch".into(),
                detail: "GET http://x".into(),
                result: OpResult::Http {
                    status: 200,
                    headers: vec!["a: b".into()],
                    body: b"ok".to_vec(),
                },
            },
            Entry::Op {
                op: 9,
                request: 0,
                kind: "fs".into(),
                detail: "read x".into(),
                result: OpResult::Failure {
                    message: "missing".into(),
                    code: 2,
                },
            },
            Entry::Response {
                rid: 7,
                reply: Reply::Full {
                    status: 201,
                    headers: vec![],
                    body: b"done".to_vec(),
                },
            },
            Entry::Gone { rid: 7 },
            Entry::Value {
                request: 7,
                name: "time".into(),
                value: b"i:5;".to_vec(),
            },
            Entry::Files(vec![("a.php".into(), "ff".into())]),
            Entry::Note("end".into()),
        ];
        let mut data = Vec::new();
        for entry in &entries {
            data.extend(encode(entry));
        }
        assert_eq!(read_all(&data), entries);
        // Cut off in the middle of the last entry: everything before it is still readable
        assert_eq!(read_all(&data[..data.len() - 2]).len(), entries.len() - 1);
    }
}

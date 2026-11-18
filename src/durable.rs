//! Durable functions (Threading\Durable): a PHP function that survives crashes, restarts and
//! deployments. Its journal lives in Postgres (`--durable postgres://…`): every op result and every
//! value of time() or random_int() goes there before the function sees it. Resuming the function runs
//! its closure again and answers each op from the journal while entries remain, then live.
//!
//! A long wait (Task::delay above DURABLE_TIMER, Signal::receive) ends the fiber: a row in
//! tenma_timers names when, or on which signal, to wake it; the poller of the first worker appends
//! the wait's result to the journal and resumes the function, in whichever process runs. One owner per
//! key: a lease with an expiry, renewed while it runs.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::OnceLock;
use std::time::Duration;

use serde_json::{Value, json};

use crate::journal::{self, Entry};
use crate::php::{self, OpResult, bytes};

const LEASE: &str = "30 seconds";
const RENEW: Duration = Duration::from_secs(10);
const POLL: Duration = Duration::from_secs(1);
const LEASE_LOST: &str = "the lease expired and another worker took the function over";

static URL: OnceLock<String> = OnceLock::new();

/// `--durable postgres://…`: once, before the workers start.
pub fn configure(url: &str) {
    let _ = URL.set(url.to_owned());
}

pub fn enabled() -> bool {
    URL.get().is_some()
}

/// A durable function this worker runs right now: its journal and where it is in it.
struct Run {
    /// Recorded entries not yet replayed.
    entries: std::collections::VecDeque<Entry>,
    /// The next sequence number to persist.
    next: i64,
    /// Values of wrapped functions since the last persisted op, written with the next one.
    pending: Vec<Entry>,
    /// Stops the lease renewal.
    renewal: Option<tokio::task::AbortHandle>,
}

thread_local! {
    static RUNS: RefCell<HashMap<String, Rc<RefCell<Run>>>> = RefCell::new(HashMap::new());
    /// The durable function whose fiber runs PHP right now, set by the scheduler.
    static CONTEXT: RefCell<Option<String>> = const { RefCell::new(None) };
}

/// The scheduler tells which durable function's fiber runs; empty for none.
#[unsafe(no_mangle)]
extern "C" fn tenma_rs_durable_context(key: *const u8, len: usize) {
    let key = bytes(key, len);
    CONTEXT.with_borrow_mut(|context| {
        *context = if key.is_empty() {
            None
        } else {
            Some(String::from_utf8_lossy(key).into_owned())
        }
    });
}

fn context() -> Option<String> {
    CONTEXT.with_borrow(Clone::clone)
}

fn run_of(key: &str) -> Option<Rc<RefCell<Run>>> {
    RUNS.with_borrow(|runs| runs.get(key).cloned())
}

fn owner() -> String {
    format!(
        "{}:{}",
        std::process::id(),
        std::thread::current().name().unwrap_or("worker")
    )
}

async fn sql(mode: &str, sql: &str, params: Value) -> Result<Value, String> {
    let url = URL
        .get()
        .ok_or("durable functions need --durable postgres://…")?;
    let json = crate::db::run_sql(url, mode, sql, params)
        .await
        .map_err(|error| crate::db::describe(error.as_ref()))?;
    serde_json::from_str(&json).map_err(|error| error.to_string())
}

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS tenma_durable (
    key text PRIMARY KEY, name text NOT NULL, version int NOT NULL, status text NOT NULL,
    lease_until timestamptz, owner text, error text,
    created timestamptz NOT NULL DEFAULT now(), updated timestamptz NOT NULL DEFAULT now());
CREATE TABLE IF NOT EXISTS tenma_journal (
    key text NOT NULL, seq int NOT NULL, entry bytea NOT NULL, PRIMARY KEY (key, seq));
CREATE TABLE IF NOT EXISTS tenma_timers (
    key text PRIMARY KEY, due timestamptz NOT NULL, signal text, payload text);
CREATE INDEX IF NOT EXISTS tenma_timers_due ON tenma_timers (due);
CREATE TABLE IF NOT EXISTS tenma_signals (
    signal text PRIMARY KEY, payload text NOT NULL, sent timestamptz NOT NULL DEFAULT now());
";

static SCHEMA_READY: std::sync::Mutex<bool> = std::sync::Mutex::new(false);

async fn ensure_schema() -> Result<(), String> {
    if *SCHEMA_READY.lock().unwrap() {
        return Ok(());
    }
    sql("script", SCHEMA, json!([])).await?;
    *SCHEMA_READY.lock().unwrap() = true;
    Ok(())
}

/// Takes the lease of a function if nobody holds it: its version if taken.
async fn claim(key: &str, name: &str, version: i64) -> Result<Option<(i64, String)>, String> {
    ensure_schema().await?;
    sql(
        "execute",
        "INSERT INTO tenma_durable (key, name, version, status) VALUES ($1, $2, $3, 'new') ON CONFLICT (key) DO NOTHING",
        json!([key, name, version]),
    )
    .await?;
    let rows = sql(
        "query",
        &format!(
            "UPDATE tenma_durable SET lease_until = now() + interval '{LEASE}', owner = $2, status = CASE WHEN status = 'new' THEN 'running' ELSE status END, updated = now() \
             WHERE key = $1 AND status IN ('new', 'running') AND (lease_until IS NULL OR lease_until < now()) RETURNING version, status"
        ),
        json!([key, owner()]),
    )
    .await?;
    Ok(rows.as_array().and_then(|rows| rows.first()).map(|row| {
        (
            row["version"].as_i64().unwrap_or(1),
            row["status"].as_str().unwrap_or("running").to_owned(),
        )
    }))
}

async fn load_journal(key: &str) -> Result<(Vec<Entry>, i64), String> {
    let rows = sql(
        "query",
        "SELECT seq, encode(entry, 'hex') AS entry FROM tenma_journal WHERE key = $1 ORDER BY seq",
        json!([key]),
    )
    .await?;
    let mut entries = Vec::new();
    let mut next = 0;
    for row in rows.as_array().into_iter().flatten() {
        next = row["seq"].as_i64().unwrap_or(next) + 1;
        let encoded = row["entry"].as_str().unwrap_or("");
        let data = from_hex(encoded);
        if let Some((entry, _)) = journal::decode(&data, 0) {
            entries.push(entry);
        }
    }
    Ok((entries, next))
}

fn from_hex(text: &str) -> Vec<u8> {
    let digits = text.as_bytes().chunks_exact(2);
    digits
        .filter_map(|pair| u8::from_str_radix(std::str::from_utf8(pair).ok()?, 16).ok())
        .collect()
}

/// Appends entries to the journal of `key`, in order, as one statement.
async fn persist(key: &str, from: i64, entries: &[Entry]) -> Result<(), String> {
    if entries.is_empty() {
        return Ok(());
    }
    let mut values = Vec::new();
    let mut params = vec![json!(key)];
    for (index, entry) in entries.iter().enumerate() {
        params.push(json!(from + index as i64));
        params.push(json!(format!("\\x{}", crate::hex(&journal::encode(entry)))));
        values.push(format!(
            "($1, ${}, ${}::bytea)",
            params.len() - 1,
            params.len()
        ));
    }
    sql(
        "execute",
        &format!(
            "INSERT INTO tenma_journal (key, seq, entry) VALUES {}",
            values.join(", ")
        ),
        Value::Array(params),
    )
    .await?;
    Ok(())
}

/// Starts or resumes the function `name` for `key`: "run" once the lease is taken and the journal
/// loaded, "busy" if another process holds it or it is done, or an error. PHP then runs the closure.
async fn start(key: String, name: String, version: i64) -> Result<String, String> {
    let Some((recorded, status)) = claim(&key, &name, version).await? else {
        return Ok("busy".into());
    };
    if status != "running" {
        return Ok("busy".into());
    }
    if recorded != version {
        let error = format!("journal of version {recorded}, the code is version {version}");
        let _ = sql(
            "execute",
            "UPDATE tenma_durable SET status = 'failed', error = $2, lease_until = NULL, updated = now() WHERE key = $1",
            json!([key, error]),
        )
        .await;
        return Err(error);
    }
    let (entries, next) = load_journal(&key).await?;
    let renewal = {
        let key = key.clone();
        tokio::task::spawn_local(async move {
            loop {
                tokio::time::sleep(RENEW).await;
                let _ = sql(
                    "execute",
                    &format!("UPDATE tenma_durable SET lease_until = now() + interval '{LEASE}' WHERE key = $1 AND owner = $2"),
                    json!([key, owner()]),
                )
                .await;
            }
        })
        .abort_handle()
    };
    RUNS.with_borrow_mut(|runs| {
        runs.insert(
            key,
            Rc::new(RefCell::new(Run {
                entries: entries.into(),
                next,
                pending: Vec::new(),
                renewal: Some(renewal),
            })),
        )
    });
    Ok("run".into())
}

/// The function ended: completed, or failed with `error`. Its lease is released and the run forgotten.
async fn finish(key: String, error: Option<String>) -> Result<(), String> {
    let run = RUNS.with_borrow_mut(|runs| runs.remove(&key));
    if let Some(run) = run {
        let mut run = run.borrow_mut();
        if let Some(renewal) = run.renewal.take() {
            renewal.abort();
        }
        let pending = std::mem::take(&mut run.pending);
        let next = run.next;
        drop(run);
        persist(&key, next, &pending).await?;
    }
    let status = if error.is_some() {
        "failed"
    } else {
        "completed"
    };
    let finished = sql(
        "execute",
        "UPDATE tenma_durable SET status = $2, error = $3, lease_until = NULL, updated = now() WHERE key = $1 AND owner = $4",
        json!([key, status, error, owner()]),
    )
    .await?;
    if finished.as_i64() == Some(0) {
        return Err(LEASE_LOST.into());
    }
    Ok(())
}

/// The function waits for a timer or a signal: "done" with the payload if the journal already holds
/// the result or a signal arrived before, "suspend" once the wait is in the database and the fiber
/// must end.
async fn wait(key: String, what: String, due: f64) -> Result<OpResult, String> {
    let Some(run) = run_of(&key) else {
        return Err("not a durable function".into());
    };
    let detail = what.clone();
    // Replaying: the wait is over, its result is next
    let recorded = run.borrow_mut().entries.pop_front();
    match recorded {
        Some(Entry::Op {
            kind,
            detail: recorded,
            result,
            ..
        }) if kind == "durable.wait" && recorded == detail => {
            return Ok(result);
        }
        Some(other) => {
            return Err(format!(
                "the function diverged from its journal: it waits for {detail}, the journal has {}",
                describe(&other)
            ));
        }
        None => {}
    }
    let (next, pending) = {
        let mut run = run.borrow_mut();
        (run.next, std::mem::take(&mut run.pending))
    };
    persist(&key, next, &pending).await?;
    run.borrow_mut().next += pending.len() as i64;
    let signal = what.strip_prefix("signal:");
    // The timer row first: a send() from now on finds it. Then the signals kept from before: a send()
    // in between went to one of the two, never past both
    sql(
        "execute",
        "INSERT INTO tenma_timers (key, due, signal) VALUES ($1, to_timestamp($2), $3) ON CONFLICT (key) DO UPDATE SET due = EXCLUDED.due, signal = EXCLUDED.signal, payload = NULL",
        json!([key, due, signal]),
    )
    .await?;
    if let Some(signal) = signal {
        let rows = sql(
            "query",
            "DELETE FROM tenma_signals WHERE signal = $1 RETURNING payload",
            json!([signal]),
        )
        .await?;
        if let Some(payload) = rows
            .as_array()
            .and_then(|rows| rows.first())
            .and_then(|row| row["payload"].as_str())
        {
            let deleted = sql(
                "execute",
                "DELETE FROM tenma_timers WHERE key = $1 AND payload IS NULL",
                json!([key]),
            )
            .await?;
            // Hit meanwhile by a send(): the poller delivers that one
            if deleted.as_i64().unwrap_or(0) == 1 {
                let result = OpResult::Text(payload.as_bytes().to_vec());
                append(&run, &key, "durable.wait", &detail, &result).await?;
                return Ok(result);
            }
        }
    }
    // The poller may have fired the timer already: then the function stays 'running' and is resumed
    sql(
        "execute",
        "UPDATE tenma_durable SET status = CASE WHEN EXISTS (SELECT 1 FROM tenma_timers WHERE key = $1) THEN 'waiting' ELSE 'running' END, \
         lease_until = NULL, updated = now() WHERE key = $1 AND owner = $2",
        json!([key, owner()]),
    )
    .await?;
    if let Some(run) = RUNS.with_borrow_mut(|runs| runs.remove(&key))
        && let Some(renewal) = run.borrow_mut().renewal.take()
    {
        renewal.abort();
    }
    Ok(OpResult::Text(b"suspend".to_vec()))
}

/// Persists an op result (with the values before it) and remembers the sequence.
async fn append(
    run: &Rc<RefCell<Run>>,
    key: &str,
    kind: &str,
    detail: &str,
    result: &OpResult,
) -> Result<(), String> {
    let (next, mut entries) = {
        let mut run = run.borrow_mut();
        (run.next, std::mem::take(&mut run.pending))
    };
    entries.push(Entry::Op {
        op: 0,
        request: 0,
        kind: kind.to_owned(),
        detail: detail.to_owned(),
        result: result.clone(),
    });
    persist(key, next, &entries).await?;
    run.borrow_mut().next += entries.len() as i64;
    Ok(())
}

/// Signal::send(): wakes a function waiting for it, in any process, or keeps the payload for the
/// next receive.
async fn send(signal: String, payload: String) -> Result<(), String> {
    ensure_schema().await?;
    let woken = sql(
        "execute",
        "UPDATE tenma_timers SET due = now(), payload = $2 WHERE signal = $1 AND payload IS NULL",
        json!([signal, payload]),
    )
    .await?;
    if woken.as_i64().unwrap_or(0) == 0 {
        sql(
            "execute",
            "INSERT INTO tenma_signals (signal, payload) VALUES ($1, $2) ON CONFLICT (signal) DO UPDATE SET payload = EXCLUDED.payload, sent = now()",
            json!([signal, payload]),
        )
        .await?;
    }
    Ok(())
}

fn describe(entry: &Entry) -> String {
    match entry {
        Entry::Op { kind, detail, .. } => format!("op {kind} {detail}"),
        Entry::Value { name, .. } => format!("a value of {name}()"),
        other => format!("{other:?}"),
    }
}

/// An op PHP starts inside a durable function: answered from the journal while it has entries.
/// Values of wrapped functions that the journal has before the op count as consumed.
pub fn intercept(kind: &str, detail: &str) -> Option<u64> {
    let key = context()?;
    let run = run_of(&key)?;
    let next = {
        let run = run.borrow();
        run.entries.front().cloned()
    };
    match next {
        Some(Entry::Op {
            kind: recorded_kind,
            detail: recorded,
            result,
            ..
        }) => {
            let op = php::next_id();
            let result = if recorded_kind == kind && recorded == detail {
                run.borrow_mut().entries.pop_front();
                result
            } else {
                run.borrow_mut().entries.clear();
                OpResult::Error(format!(
                    "the durable function diverged from its journal: it started {kind} {detail}, the journal has {recorded_kind} {recorded}"
                ))
            };
            // After PHP has returned from starting the op
            tokio::task::spawn_local(async move {
                if !crate::broken() && php::complete(op, result).is_err() {
                    crate::interpreter_broken();
                }
            });
            Some(op)
        }
        Some(Entry::Value { name, .. }) => {
            // A value the code no longer asks for
            run.borrow_mut().entries.clear();
            let op = php::next_id();
            let error = OpResult::Error(format!(
                "the durable function diverged from its journal: the journal has a value of {name}() before {kind} {detail}"
            ));
            tokio::task::spawn_local(async move {
                if !crate::broken() && php::complete(op, error).is_err() {
                    crate::interpreter_broken();
                }
            });
            Some(op)
        }
        _ => None,
    }
}

/// A live op of a durable function finished: its result goes to the journal before PHP gets it.
/// The key is taken when the op starts, since the context has changed by the time it ends.
pub fn starting() -> Option<String> {
    let key = context()?;
    run_of(&key).map(|_| key)
}

pub async fn record(key: &str, kind: &str, detail: &str, result: &OpResult) -> Result<(), String> {
    let Some(run) = run_of(key) else {
        return Ok(());
    };
    append(&run, key, kind, detail, result).await
}

/// time() and the other wrapped functions inside a durable function: the recorded value while
/// replaying, otherwise None and the value is kept for the journal.
pub fn take_value(name: &str) -> Option<Vec<u8>> {
    let key = context()?;
    let run = run_of(&key)?;
    let mut run = run.borrow_mut();
    match run.entries.front() {
        Some(Entry::Value {
            name: recorded,
            value,
            ..
        }) if recorded == name => {
            let value = value.clone();
            run.entries.pop_front();
            Some(value)
        }
        _ => None,
    }
}

pub fn keep_value(name: &str, value: &[u8]) -> bool {
    let Some(key) = context() else {
        return false;
    };
    let Some(run) = run_of(&key) else {
        return false;
    };
    run.borrow_mut().pending.push(Entry::Value {
        request: 0,
        name: name.to_owned(),
        value: value.to_vec(),
    });
    true
}

/// `Tenma\Internal\durable($op, $key, $name, $number)`: an op whose result is a string.
/// "start": "run" or "busy"; "finish": $name is the error or ""; "wait": $name is "timer" or
/// "signal:<name>", $number the due time; "send": $key is the signal, $name the payload.
#[unsafe(no_mangle)]
extern "C" fn tenma_rs_durable(
    op: *const u8,
    op_len: usize,
    key: *const u8,
    key_len: usize,
    name: *const u8,
    name_len: usize,
    number: f64,
) -> u64 {
    let op = String::from_utf8_lossy(bytes(op, op_len)).into_owned();
    let key = String::from_utf8_lossy(bytes(key, key_len)).into_owned();
    let name = String::from_utf8_lossy(bytes(name, name_len)).into_owned();
    let detail = format!("{op} {key}");
    crate::ops::spawn_plain("durable", detail, async move {
        let result = match op.as_str() {
            "start" => start(key, name, number as i64)
                .await
                .map(|text| OpResult::Text(text.into_bytes())),
            "finish" => finish(key, (!name.is_empty()).then_some(name))
                .await
                .map(|()| OpResult::Null),
            "wait" => wait(key, name, number).await,
            "send" => send(key, name).await.map(|()| OpResult::Null),
            other => Err(format!("unknown durable op {other}")),
        };
        result.unwrap_or_else(OpResult::Error)
    })
}

unsafe extern "C" {
    fn tenma_resume_durable(
        name: *const u8,
        name_len: usize,
        key: *const u8,
        key_len: usize,
    ) -> std::ffi::c_int;
}

/// Fires one due timer in a single statement: the wait's result joins the journal, the timer row goes
/// and the function is 'running' again, or nothing of that happens. A timer that another poller took
/// or that a send() changed since it was read matches no row.
async fn fire(timer: &Value) -> Result<(), String> {
    let key = timer["key"].as_str().unwrap_or_default();
    let signal = timer["signal"].as_str();
    let payload = timer["payload"].as_str();
    let entry = Entry::Op {
        op: 0,
        request: 0,
        kind: "durable.wait".into(),
        detail: signal.map_or("timer".to_owned(), |signal| format!("signal:{signal}")),
        result: payload.map_or(OpResult::Null, |payload| {
            OpResult::Text(payload.as_bytes().to_vec())
        }),
    };
    let encoded = format!("\\x{}", crate::hex(&journal::encode(&entry)));
    sql(
        "execute",
        "WITH timer AS ( \
             DELETE FROM tenma_timers \
             WHERE key = $1 AND due <= now() AND payload IS NOT DISTINCT FROM $3::text \
             RETURNING key \
         ), entry AS ( \
             INSERT INTO tenma_journal (key, seq, entry) \
             SELECT key, (SELECT coalesce(max(seq), -1) + 1 FROM tenma_journal WHERE key = $1), $2::bytea \
             FROM timer \
         ) \
         UPDATE tenma_durable SET status = 'running', updated = now() \
         WHERE key IN (SELECT key FROM timer) AND status = 'waiting'",
        json!([key, encoded, payload]),
    )
    .await?;
    Ok(())
}

/// The first worker's poller: fires due timers and signals, resumes functions whose process died.
pub async fn poll() {
    loop {
        tokio::time::sleep(POLL).await;
        if crate::broken() {
            continue;
        }
        let due = sql(
            "query",
            "SELECT key, signal, payload FROM tenma_timers WHERE due <= now()",
            json!([]),
        )
        .await;
        for timer in due
            .ok()
            .as_ref()
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let _ = fire(timer).await;
        }
        // Functions to run: woken ones, and those whose owner died while they ran
        let stale = sql(
            "query",
            "SELECT key, name FROM tenma_durable WHERE status = 'running' AND (lease_until IS NULL OR lease_until < now())",
            json!([]),
        )
        .await;
        for row in stale
            .ok()
            .as_ref()
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let (key, name) = (
                row["key"].as_str().unwrap_or_default(),
                row["name"].as_str().unwrap_or_default(),
            );
            if run_of(key).is_some() {
                continue;
            }
            let code =
                unsafe { tenma_resume_durable(name.as_ptr(), name.len(), key.as_ptr(), key.len()) };
            if code < 0 {
                crate::interpreter_broken();
                continue;
            }
        }
    }
}

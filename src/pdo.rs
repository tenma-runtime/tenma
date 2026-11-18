//! The PDO driver "pgsql" of tenma: PDO objects for Postgres talk to tokio-postgres instead of libpq,
//! so that inside a fiber only the fiber waits for a query. The C layer (shim/tenma.c) implements the
//! driver interface of PDO and calls here for everything that touches the server.
//!
//! Every PDO object is one connection, as with libpq. The connections live on a helper thread with its
//! own runtime: inside a fiber the worker's loop awaits the answer in an op, outside a fiber (script
//! startup, classic mode) the worker thread blocks on it, as PHP would.
//!
//! Results travel to C as one buffer: the columns with name and type OID, then every row with its
//! values as the text Postgres prints, so PDO sees what libpq's text mode would give it.

use std::collections::HashMap;
use std::ffi::c_int;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use tokio::sync::{Mutex as AsyncMutex, mpsc, oneshot};
use tokio_postgres::{Client, NoTls, Statement};

use crate::db::{TextParam, describe};
use crate::ops;
use crate::php::{OpResult, bytes, text};

type Failure = Box<dyn std::error::Error + Send + Sync>;
type Job = std::pin::Pin<Box<dyn Future<Output = ()> + Send>>;

/// One PDO object: its connection and the statements prepared on it, by SQL.
struct Handle {
    client: AsyncMutex<Client>,
    /// Original and wrapped statement per SQL text, so a PDOStatement executed again costs no prepare
    statements: AsyncMutex<HashMap<String, (Statement, Statement)>>,
}

static HANDLES: Mutex<Option<HashMap<u64, Arc<Handle>>>> = Mutex::new(None);
static NEXT_HANDLE: AtomicU64 = AtomicU64::new(1);
/// The helper thread's inbox; created with the thread on first use.
static JOBS: OnceLock<mpsc::UnboundedSender<Job>> = OnceLock::new();

fn jobs() -> &'static mpsc::UnboundedSender<Job> {
    JOBS.get_or_init(|| {
        let (sender, mut receiver) = mpsc::unbounded_channel::<Job>();
        std::thread::Builder::new()
            .name("tenma-pdo".into())
            .spawn(move || {
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("PDO runtime does not start");
                runtime.block_on(async move {
                    while let Some(job) = receiver.recv().await {
                        tokio::spawn(job);
                    }
                });
            })
            .expect("PDO thread does not start");
        sender
    })
}

fn handle(id: u64) -> Result<Arc<Handle>, Failure> {
    HANDLES
        .lock()
        .unwrap()
        .as_ref()
        .and_then(|handles| handles.get(&id).cloned())
        .ok_or_else(|| "the connection is closed".into())
}

/// The SQLSTATE and message of an error as "SQLSTATE|message"; errors not from the server get 08006
/// (connection failure) or HY000.
fn sqlstate(error: &Failure) -> String {
    if let Some(error) = error.downcast_ref::<tokio_postgres::Error>() {
        if let Some(db) = error.as_db_error() {
            return format!("{}|{}", db.code().code(), db.message());
        }
        if error.is_closed() {
            return format!("08006|{}", describe(error));
        }
    }
    format!("HY000|{}", describe(error.as_ref()))
}

async fn connect(conninfo: &str) -> Result<Vec<u8>, Failure> {
    let mut config: tokio_postgres::Config = conninfo.parse()?;
    if config.get_connect_timeout().is_none() {
        config.connect_timeout(std::time::Duration::from_secs(30));
    }
    // ponytail: no TLS, as Data\Postgres; sslmode in the DSN fails with a clear message
    let (client, connection) = config.connect(NoTls).await?;
    tokio::spawn(async move {
        let _ = connection.await;
    });
    let id = NEXT_HANDLE.fetch_add(1, Ordering::Relaxed);
    HANDLES
        .lock()
        .unwrap()
        .get_or_insert_with(HashMap::new)
        .insert(
            id,
            Arc::new(Handle {
                client: AsyncMutex::new(client),
                statements: AsyncMutex::new(HashMap::new()),
            }),
        );
    Ok(id.to_string().into_bytes())
}

fn push_u32(out: &mut Vec<u8>, value: u32) {
    out.extend_from_slice(&value.to_le_bytes());
}

fn push_u64(out: &mut Vec<u8>, value: u64) {
    out.extend_from_slice(&value.to_le_bytes());
}

/// The buffer for C: column count, per column name length, name and OID; row count; affected rows;
/// then every value as length and bytes, length u32::MAX for NULL.
fn result_buffer(
    columns: &[(String, u32)],
    rows: Vec<Vec<Option<Vec<u8>>>>,
    affected: u64,
) -> Vec<u8> {
    let mut out = Vec::new();
    push_u32(&mut out, columns.len() as u32);
    for (name, oid) in columns {
        push_u32(&mut out, name.len() as u32);
        out.extend_from_slice(name.as_bytes());
        push_u32(&mut out, *oid);
    }
    push_u64(&mut out, rows.len() as u64);
    push_u64(&mut out, affected);
    for row in rows {
        for value in row {
            match value {
                Some(bytes) => {
                    push_u32(&mut out, bytes.len() as u32);
                    out.extend_from_slice(&bytes);
                }
                None => push_u32(&mut out, u32::MAX),
            }
        }
    }
    out
}

/// A value as the text Postgres printed, whatever its type: the wrapped statement casts every column
/// to text, so the binary result holds the text output format.
struct Printed(Option<Vec<u8>>);

impl<'a> tokio_postgres::types::FromSql<'a> for Printed {
    fn from_sql(_: &tokio_postgres::types::Type, raw: &'a [u8]) -> Result<Self, Failure> {
        Ok(Printed(Some(raw.to_vec())))
    }

    fn from_sql_null(_: &tokio_postgres::types::Type) -> Result<Self, Failure> {
        Ok(Printed(None))
    }

    fn accepts(_: &tokio_postgres::types::Type) -> bool {
        true
    }
}

/// Runs a statement with parameters: prepared on first use, the rows come through a wrapper that
/// casts every column to text. A statement without result columns runs as it is.
async fn query(handle: &Handle, sql: &str, params: Vec<TextParam>) -> Result<Vec<u8>, Failure> {
    let client = handle.client.lock().await;
    if crate::db::modifying_with(sql) {
        return modifying(&client, sql, params).await;
    }
    let mut statements = handle.statements.lock().await;
    if !statements.contains_key(sql) {
        let original = client.prepare(sql).await?;
        let wrapped = if original.columns().is_empty() {
            original.clone()
        } else {
            let names: Vec<String> = (1..=original.columns().len())
                .map(|i| format!("c{i}"))
                .collect();
            let casts: Vec<String> = names.iter().map(|name| format!("{name}::text")).collect();
            // Newline before the parenthesis: a --comment at the end of the query would swallow it
            let wrapper = format!(
                "WITH tenma_pdo({}) AS ({}\n) SELECT {} FROM tenma_pdo",
                names.join(", "),
                sql.trim().trim_end_matches(';'),
                casts.join(", ")
            );
            client.prepare(&wrapper).await?
        };
        // ponytail: the cache only grows while a PDO object lives; an app preparing thousands of
        // distinct statements on one connection holds them all, as libpq would
        statements.insert(sql.to_owned(), (original, wrapped));
    }
    let (original, wrapped) = statements.get(sql).cloned().expect("inserted above");
    drop(statements);
    let refs: Vec<&(dyn tokio_postgres::types::ToSql + Sync)> = params
        .iter()
        .map(|param| param as &(dyn tokio_postgres::types::ToSql + Sync))
        .collect();
    let columns: Vec<(String, u32)> = original
        .columns()
        .iter()
        .map(|column| (column.name().to_owned(), column.type_().oid()))
        .collect();
    if columns.is_empty() {
        let affected = client.execute(&original, &refs).await?;
        return Ok(result_buffer(&[], Vec::new(), affected));
    }
    let rows = client.query(&wrapped, &refs).await?;
    let values = rows
        .iter()
        .map(|row| {
            (0..columns.len())
                .map(|index| row.get::<_, Printed>(index).0)
                .collect()
        })
        .collect::<Vec<Vec<Option<Vec<u8>>>>>();
    let count = values.len() as u64;
    Ok(result_buffer(&columns, values, count))
}

/// A WITH that modifies data must stay at the top level, so the wrapper cannot take it: its rows go
/// into a temporary table first, then out as text. Three round trips, for the rare statement.
async fn modifying(client: &Client, sql: &str, params: Vec<TextParam>) -> Result<Vec<u8>, Failure> {
    let original = client.prepare(sql).await?;
    let refs: Vec<&(dyn tokio_postgres::types::ToSql + Sync)> = params
        .iter()
        .map(|param| param as &(dyn tokio_postgres::types::ToSql + Sync))
        .collect();
    let columns: Vec<(String, u32)> = original
        .columns()
        .iter()
        .map(|column| (column.name().to_owned(), column.type_().oid()))
        .collect();
    if columns.is_empty() {
        let affected = client.execute(&original, &refs).await?;
        return Ok(result_buffer(&[], Vec::new(), affected));
    }
    let names: Vec<String> = (1..=columns.len()).map(|i| format!("c{i}")).collect();
    let casts: Vec<String> = names.iter().map(|name| format!("{name}::text")).collect();
    client
        .execute(
            &format!(
                "CREATE TEMP TABLE tenma_pdo ({}) AS {}\n",
                names.join(", "),
                sql.trim().trim_end_matches(';')
            ),
            &refs,
        )
        .await?;
    let rows = client
        .query(&format!("SELECT {} FROM tenma_pdo", casts.join(", ")), &[])
        .await;
    client.batch_execute("DROP TABLE pg_temp.tenma_pdo").await?;
    let values = rows?
        .iter()
        .map(|row| {
            (0..columns.len())
                .map(|index| row.get::<_, Printed>(index).0)
                .collect()
        })
        .collect::<Vec<Vec<Option<Vec<u8>>>>>();
    let count = values.len() as u64;
    Ok(result_buffer(&columns, values, count))
}

/// SHOW, EXPLAIN, FETCH, several statements at once, PDO::exec(): the simple query protocol, which
/// returns text and takes no parameters. Every column counts as text.
async fn simple(handle: &Handle, sql: &str) -> Result<Vec<u8>, Failure> {
    let client = handle.client.lock().await;
    let mut columns: Vec<(String, u32)> = Vec::new();
    let mut rows = Vec::new();
    let mut affected = 0;
    for message in client.simple_query(sql).await? {
        match message {
            tokio_postgres::SimpleQueryMessage::Row(row) => {
                if columns.is_empty() {
                    columns = row
                        .columns()
                        .iter()
                        .map(|column| (column.name().to_owned(), 25))
                        .collect();
                }
                rows.push(
                    (0..row.len())
                        .map(|index| row.get(index).map(|text| text.as_bytes().to_vec()))
                        .collect(),
                );
            }
            tokio_postgres::SimpleQueryMessage::CommandComplete(count) => affected += count,
            _ => {}
        }
    }
    if !rows.is_empty() {
        affected = rows.len() as u64;
    }
    Ok(result_buffer(&columns, rows, affected))
}

/// Whether the statement cannot stand inside a WITH and takes no parameters anyway.
fn needs_simple_protocol(sql: &str) -> bool {
    let first = sql
        .trim_start()
        .split(|c: char| !c.is_ascii_alphanumeric())
        .next()
        .unwrap_or_default()
        .to_ascii_lowercase();
    matches!(
        first.as_str(),
        "show"
            | "explain"
            | "fetch"
            | "begin"
            | "start"
            | "commit"
            | "rollback"
            | "set"
            | "reset"
            | "listen"
            | "notify"
            | "vacuum"
            | "analyze"
            | "discard"
            | "copy"
            | "declare"
            | "close"
            | "move"
            | "deallocate"
            | "prepare"
            | "execute"
            | "savepoint"
            | "release"
            | "create"
            | "drop"
            | "alter"
            | "truncate"
            | "grant"
            | "revoke"
            | "comment"
    )
}

async fn run(op: String, id: u64, sql: String, params: Vec<TextParam>) -> Result<Vec<u8>, Failure> {
    match op.as_str() {
        "connect" => connect(&sql).await,
        "query" if params.is_empty() && needs_simple_protocol(&sql) => {
            simple(handle(id)?.as_ref(), &sql).await
        }
        "query" => query(handle(id)?.as_ref(), &sql, params).await,
        "exec" => simple(handle(id)?.as_ref(), &sql).await,
        other => Err(format!("unknown PDO op {other}").into()),
    }
}

/// Parameters from PHP: each as 4 bytes of length and its bytes, length u32::MAX for NULL.
fn params(packed: &[u8]) -> Vec<TextParam> {
    let mut params = Vec::new();
    let mut rest = packed;
    while rest.len() >= 4 {
        let len = u32::from_le_bytes([rest[0], rest[1], rest[2], rest[3]]);
        rest = &rest[4..];
        if len == u32::MAX {
            params.push(TextParam(None));
            continue;
        }
        let len = (len as usize).min(rest.len());
        params.push(TextParam(Some(rest[..len].to_vec())));
        rest = &rest[len..];
    }
    params
}

/// A result buffer or an error for the C layer's blocking path; the buffer is freed with tenma_rs_pdo_free.
#[repr(C)]
pub struct PdoResult {
    ptr: *mut u8,
    len: usize,
    error: c_int,
}

/// Starts a PDO op: "connect" (sql is the connection string), "query" (sql and params as JSON),
/// "exec" (sql only); params as `params` reads them. Inside a fiber with the loop running, returns the op whose value is the result
/// buffer and whose error is "SQLSTATE|message". Otherwise (blocking != 0 or no loop) runs the op to
/// completion, fills `out` and returns 0.
#[unsafe(no_mangle)]
extern "C" fn tenma_rs_pdo(
    op: *const u8,
    op_len: usize,
    id: u64,
    sql: *const u8,
    sql_len: usize,
    packed: *const u8,
    params_len: usize,
    blocking: c_int,
    out: *mut PdoResult,
) -> u64 {
    let op = text(op, op_len);
    let sql = std::str::from_utf8(bytes(sql, sql_len)).map(str::to_owned);
    let params = params(bytes(packed, params_len));
    let detail = format!(
        "{op} {id} {}",
        crate::journal::redact(sql.as_deref().unwrap_or_default())
    );
    let future = async move {
        let sql = sql.map_err(|_| "the statement is not valid UTF-8")?;
        run(op, id, sql, params).await
    };
    if blocking == 0 && ops::looping() {
        let (sender, receiver) = oneshot::channel::<Result<Vec<u8>, String>>();
        jobs()
            .send(Box::pin(async move {
                let _ = sender.send(future.await.map_err(|error| sqlstate(&error)));
            }))
            .expect("PDO thread gone");
        return ops::spawn("pdo", detail, async move {
            match receiver.await {
                Ok(Ok(buffer)) => OpResult::Text(buffer),
                Ok(Err(error)) => OpResult::Error(error),
                Err(_) => OpResult::Error("HY000|the PDO thread stopped".into()),
            }
        });
    }
    // Outside a fiber the worker thread waits, as PHP would; a std channel does not mind the runtime
    let (sender, receiver) = std::sync::mpsc::sync_channel::<Result<Vec<u8>, String>>(1);
    jobs()
        .send(Box::pin(async move {
            let _ = sender.send(future.await.map_err(|error| sqlstate(&error)));
        }))
        .expect("PDO thread gone");
    let result = receiver
        .recv()
        .unwrap_or_else(|_| Err("HY000|the PDO thread stopped".into()));
    let (buffer, error) = match result {
        Ok(buffer) => (buffer, 0),
        Err(error) => (error.into_bytes(), 1),
    };
    let mut buffer = std::mem::ManuallyDrop::new(buffer.into_boxed_slice());
    unsafe {
        *out = PdoResult {
            ptr: buffer.as_mut_ptr(),
            len: buffer.len(),
            error,
        };
    }
    0
}

#[unsafe(no_mangle)]
extern "C" fn tenma_rs_pdo_free(ptr: *mut u8, len: usize) {
    if !ptr.is_null() {
        drop(unsafe { Box::from_raw(std::ptr::slice_from_raw_parts_mut(ptr, len)) });
    }
}

/// Closes the connection of a PDO object; a statement still running finishes on its own.
#[unsafe(no_mangle)]
extern "C" fn tenma_rs_pdo_close(id: u64) {
    if let Some(handles) = HANDLES.lock().unwrap().as_mut() {
        handles.remove(&id);
    }
}

//! Async databases for PHP: Postgres and Redis. The fiber waits for the result, the event loop
//! serves other requests in the meantime. Results go to PHP as JSON.
//!
//! Postgres behaves like PDO: parameters go as text, the server infers their type from the
//! query ('42' fits an int column, dates as strings). Postgres converts results itself with
//! json_agg, so timestamps, numeric, uuid and json arrive too.

use std::cell::RefCell;
use std::collections::HashMap;
use std::error::Error;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use bytes::BytesMut;
use serde_json::Value;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio_postgres::error::SqlState;
use tokio_postgres::types::{Format, IsNull, Kind, ToSql, Type, to_sql_checked};
use tokio_postgres::{Client, NoTls, Statement};

use crate::ops;
use crate::php::{OpResult, bytes, text};

type Failure = Box<dyn Error + Send + Sync>;

/// The statements prepared on one connection, by SQL. They stay as long as the connection.
type Statements = RefCell<HashMap<String, Statement>>;

/// At most this many statements stay prepared per connection; beyond that a query prepares its
/// statement for itself, as without the cache. Apps that build SQL from values fill it with one-offs.
const STATEMENTS: usize = 256;

/// At most this many Postgres connections per worker and URL (--db-pool, by default 20 / workers, from 2 to 10);
/// anyone beyond that waits.
pub static POOL_SIZE: AtomicUsize = AtomicUsize::new(10);

thread_local! {
    /// Idle Postgres connections per URL.
    static IDLE: RefCell<HashMap<String, Vec<(Client, Statements)>>> = RefCell::new(HashMap::new());
    /// Permits per URL: one per connection currently in use.
    static PERMITS: RefCell<HashMap<String, Arc<Semaphore>>> = RefCell::new(HashMap::new());
    /// Open transactions: a connection nobody else gets until commit or rollback.
    static TRANSACTIONS: RefCell<HashMap<u64, Transaction>> = RefCell::new(HashMap::new());
    static REDIS: RefCell<HashMap<String, redis::aio::MultiplexedConnection>> = RefCell::new(HashMap::new());
}

struct Transaction {
    url: String,
    /// The request that began it; if it ends with exit(), tenma_rs_db_abandon discards it.
    owner: u64,
    client: Rc<RefCell<Slot>>,
}

/// Where a transaction's connection currently is.
enum Slot {
    Idle(Lease),
    /// A query is running and holds the connection.
    Busy,
    /// The running query was cancelled (Tenma\all, Task::cancel, client gone): Postgres stops it and
    /// the cleanup rolls the transaction back.
    Lost,
}

/// Holds a connection during a query, with the transaction's slot if it belongs to one. If the
/// query is cancelled, the guard is dropped: Postgres is told to stop the query, and only then is
/// the connection cleaned and pooled again. Until then it keeps its permit, so the pool never has
/// more busy backends than --db-pool. A transaction's slot becomes Lost.
struct Busy {
    url: String,
    slot: Option<Rc<RefCell<Slot>>>,
    lease: Option<Lease>,
}

impl Busy {
    fn lease(&self) -> &Lease {
        self.lease.as_ref().expect("held until done")
    }

    /// The query is done: the connection goes back to its transaction, or to the pool.
    fn done(mut self) {
        if let Some(lease) = self.lease.take() {
            match &self.slot {
                Some(slot) => *slot.borrow_mut() = Slot::Idle(lease),
                None => release(&self.url, lease),
            }
        }
    }
}

impl Drop for Busy {
    fn drop(&mut self) {
        let Some(lease) = self.lease.take() else {
            return;
        };
        if let Some(slot) = &self.slot {
            *slot.borrow_mut() = Slot::Lost;
        }
        let url = std::mem::take(&mut self.url);
        tokio::task::spawn_local(async move {
            // Once the cancel request is through, the backend stops the query or, if it was done
            // already, ignores it while idle: it cannot hit the cleanup sent after it
            let _ = lease.client.cancel_token().cancel_query(NoTls).await;
            release(&url, lease);
        });
    }
}

/// A connection with its prepared statements, plus its permit from the pool; release returns both.
struct Lease {
    client: Client,
    statements: Statements,
    _permit: OwnedSemaphorePermit,
}

/// Forgets all connections of this worker, e.g. on restart.
pub fn clear() {
    IDLE.with_borrow_mut(HashMap::clear);
    PERMITS.with_borrow_mut(HashMap::clear);
    TRANSACTIONS.with_borrow_mut(HashMap::clear);
    REDIS.with_borrow_mut(HashMap::clear);
}

/// A parameter as text; the server decides the type, as with PDO.
#[derive(Debug)]
pub struct TextParam(pub Option<String>);

impl ToSql for TextParam {
    fn to_sql(&self, _: &Type, out: &mut BytesMut) -> Result<IsNull, Failure> {
        match &self.0 {
            Some(text) => {
                out.extend_from_slice(text.as_bytes());
                Ok(IsNull::No)
            }
            None => Ok(IsNull::Yes),
        }
    }

    fn accepts(_: &Type) -> bool {
        true
    }

    fn encode_format(&self, _: &Type) -> Format {
        Format::Text
    }

    to_sql_checked!();
}

/// A parameter from Data\Postgres: its text, and for a PHP list also the Postgres array literal, which is
/// what goes out when the statement's parameter is an array (`$1::text[]`, `= ANY ($1)`). Any other type
/// gets the text: a list for a json or jsonb parameter stays JSON, as before.
#[derive(Debug)]
struct ValueParam {
    text: Option<String>,
    array: Option<String>,
}

impl ToSql for ValueParam {
    fn to_sql(&self, ty: &Type, out: &mut BytesMut) -> Result<IsNull, Failure> {
        let text = match (ty.kind(), &self.array) {
            (Kind::Array(_), Some(array)) => Some(array),
            _ => self.text.as_ref(),
        };
        match text {
            Some(text) => {
                out.extend_from_slice(text.as_bytes());
                Ok(IsNull::No)
            }
            None => Ok(IsNull::Yes),
        }
    }

    fn accepts(_: &Type) -> bool {
        true
    }

    fn encode_format(&self, _: &Type) -> Format {
        Format::Text
    }

    to_sql_checked!();
}

fn text_params(params: &[Value]) -> Vec<ValueParam> {
    params
        .iter()
        .map(|value| ValueParam {
            text: match value {
                Value::Null => None,
                Value::Bool(flag) => Some(if *flag { "1" } else { "0" }.into()),
                Value::String(text) => Some(text.clone()),
                other => Some(other.to_string()),
            },
            array: match value {
                Value::Array(items) => Some(array_literal(items)),
                _ => None,
            },
        })
        .collect()
}

/// A JSON list as a Postgres array literal: `{"a","b \"c\"",NULL,1,t}`; nested lists as nested arrays,
/// objects as their JSON text.
fn array_literal(items: &[Value]) -> String {
    let elements: Vec<String> = items
        .iter()
        .map(|item| match item {
            Value::Null => "NULL".to_owned(),
            Value::Bool(flag) => if *flag { "t" } else { "f" }.to_owned(),
            Value::Number(number) => number.to_string(),
            Value::Array(inner) => array_literal(inner),
            Value::String(text) => quoted(text),
            Value::Object(_) => quoted(&item.to_string()),
        })
        .collect();
    format!("{{{}}}", elements.join(","))
}

fn quoted(text: &str) -> String {
    format!("\"{}\"", text.replace('\\', "\\\\").replace('"', "\\\""))
}

const POOL_WAIT: std::time::Duration = std::time::Duration::from_secs(10);

async fn connect(url: &str) -> Result<Lease, Failure> {
    let permits = PERMITS.with_borrow_mut(|permits| {
        Arc::clone(
            permits
                .entry(url.to_owned())
                .or_insert_with(|| Arc::new(Semaphore::new(POOL_SIZE.load(Ordering::Relaxed)))),
        )
    });
    // Waiting forever would hang the worker's database access, for example when every connection
    // belongs to a transaction that calls the pool from inside it
    let permit = tokio::time::timeout(POOL_WAIT, permits.acquire_owned()).await.map_err(|_| {
        format!(
            "no connection of the pool came free in {} s (--db-pool {}); a transaction that queries through the outer connection instead of its own waits for itself",
            POOL_WAIT.as_secs(),
            POOL_SIZE.load(Ordering::Relaxed)
        )
    })??;
    let idle = IDLE.with_borrow_mut(|idle| {
        idle.get_mut(url).and_then(|clients| {
            std::iter::from_fn(|| clients.pop()).find(|(client, _)| !client.is_closed())
        })
    });
    let (client, statements) = match idle {
        Some(pooled) => pooled,
        None => {
            // ponytail: no TLS; postgres-native-tls once databases outside the own network come along
            let mut config: tokio_postgres::Config = url.parse()?;
            if config.get_connect_timeout().is_none() {
                config.connect_timeout(std::time::Duration::from_secs(10));
            }
            let (client, connection) = config.connect(NoTls).await?;
            tokio::task::spawn_local(connection);
            (client, Statements::default())
        }
    };
    Ok(Lease {
        client,
        statements,
        _permit: permit,
    })
}

/// What DISCARD ALL does, except DEALLOCATE ALL: that would also drop the statements tokio-postgres
/// prepared once per connection for looking up the app's own types (enums, domains), and the next
/// new type on this connection would fail with "prepared statement does not exist"; the same goes
/// for the app's statements in Lease::statements. DISCARD PLANS keeps them and only plans them anew. ROLLBACK first
/// ends a transaction the request left open; outside one it only warns.
// ponytail: a statement made with SQL PREPARE stays on the connection; DEALLOCATE by name if apps use them
const CLEANUP: &str = "ROLLBACK; CLOSE ALL; SET SESSION AUTHORIZATION DEFAULT; RESET ALL; UNLISTEN *; \
                       SELECT pg_advisory_unlock_all(); DISCARD PLANS; DISCARD TEMP; DISCARD SEQUENCES";

/// Returns the connection, but cleaned first (CLEANUP): what the request left behind (an open
/// transaction, SET ROLE, set_config for row-level security, temporary tables, advisory locks) is
/// gone. Runs in the background, the response does not wait; the pool permit is held until then.
fn release(url: &str, lease: Lease) {
    if lease.client.is_closed() {
        return;
    }
    let url = url.to_owned();
    // Both moved by name: an async block captures only the fields it uses, and a permit left
    // behind would free its place while the cleanup still runs, so the pool opens a connection more
    let Lease {
        client,
        statements,
        _permit: permit,
    } = lease;
    tokio::task::spawn_local(async move {
        if client.batch_execute(CLEANUP).await.is_ok() {
            IDLE.with_borrow_mut(|idle| idle.entry(url).or_default().push((client, statements)));
        }
        drop(permit);
    });
}

/// Runs a query: "query" returns the rows as a JSON array, "execute" the number of affected rows,
/// "script" runs several statements without parameters (simple query protocol). `cached` keeps the
/// statement prepared on the connection (outside transactions).
async fn run(
    lease: &Lease,
    cached: bool,
    mode: &str,
    sql: &str,
    params: &[Value],
) -> Result<String, Failure> {
    let client = &lease.client;
    if mode == "script" {
        client.batch_execute(sql).await?;
        return Ok("null".to_owned());
    }
    // SHOW, EXPLAIN and FETCH return rows but cannot stand inside a WITH
    if mode == "query" && matches!(first_word(sql).as_str(), "show" | "explain" | "fetch") {
        return utility(client, sql, params).await;
    }
    let params = text_params(params);
    let refs: Vec<&(dyn ToSql + Sync)> = params
        .iter()
        .map(|param| param as &(dyn ToSql + Sync))
        .collect();
    if mode == "execute" {
        let affected = prepared(lease, cached, sql, |statement| {
            let refs = &refs;
            async move { client.execute(&statement, refs).await }
        })
        .await?;
        return Ok(affected.to_string());
    }
    let sql = sql.trim().trim_end_matches(';');
    // Postgres turns the rows into JSON, so every type comes out as it does in json_agg
    const AGGREGATE: &str = "SELECT coalesce(json_agg(tenma_query), '[]')::text FROM tenma_query";
    if modifying_with(sql) {
        // A data-modifying WITH must stay at the top level, so it cannot go inside another WITH.
        // CREATE TABLE AS takes it, with parameters; the temporary table keeps the rows in order
        client
            .execute(&format!("CREATE TEMP TABLE tenma_query AS {sql}\n"), &refs)
            .await?;
        let row = client.query_one(AGGREGATE, &[]).await;
        client
            .batch_execute("DROP TABLE pg_temp.tenma_query")
            .await?;
        return Ok(exact_numbers(row?.get(0)));
    }
    // Newline before the parenthesis: otherwise a --comment at the end of the query swallows it.
    // MATERIALIZED: the rows come in the order the query returns them (ORDER BY)
    let wrapped = format!("WITH tenma_query AS MATERIALIZED ({sql}\n) {AGGREGATE}");
    let row = prepared(lease, cached, &wrapped, |statement| {
        let refs = &refs;
        async move { client.query_one(&statement, refs).await }
    })
    .await?;
    Ok(exact_numbers(row.get(0)))
}

/// Runs `work` with the statement for `sql`. With `cached` it is prepared once per connection and
/// kept, which saves a round trip and the parsing in Postgres; otherwise, or with a full cache, it is
/// prepared for this call only. A kept statement whose result type changed since (ALTER TABLE under a
/// SELECT *) fails with feature_not_supported: it is prepared anew and runs once more. Inside a
/// transaction that error would abort the transaction, so transactions never use the cache.
async fn prepared<T, F, Fut>(lease: &Lease, cached: bool, sql: &str, work: F) -> Result<T, Failure>
where
    F: Fn(Statement) -> Fut,
    Fut: std::future::Future<Output = Result<T, tokio_postgres::Error>>,
{
    if !cached {
        let statement = lease.client.prepare(sql).await?;
        return Ok(work(statement).await?);
    }
    for attempt in 0..2 {
        let kept = lease.statements.borrow().get(sql).cloned();
        let statement = match kept {
            Some(statement) => statement,
            None => {
                let statement = lease.client.prepare(sql).await?;
                let mut statements = lease.statements.borrow_mut();
                if statements.len() < STATEMENTS {
                    statements.insert(sql.to_owned(), statement.clone());
                }
                statement
            }
        };
        match work(statement).await {
            Err(error)
                if attempt == 0 && error.code() == Some(&SqlState::FEATURE_NOT_SUPPORTED) =>
            {
                lease.statements.borrow_mut().remove(sql);
            }
            result => return Ok(result?),
        }
    }
    unreachable!("the second attempt returns")
}

/// SHOW, EXPLAIN or FETCH: through the simple query protocol, which takes no parameters and returns
/// every value as text. The rows as a JSON array of objects, columns in their order.
async fn utility(client: &Client, sql: &str, params: &[Value]) -> Result<String, Failure> {
    if !params.is_empty() {
        return Err("SHOW, EXPLAIN and FETCH take no parameters".into());
    }
    let mut rows = Vec::new();
    for message in client.simple_query(sql).await? {
        let tokio_postgres::SimpleQueryMessage::Row(row) = message else {
            continue;
        };
        let columns: Vec<String> = row
            .columns()
            .iter()
            .enumerate()
            .map(|(index, column)| {
                let name = serde_json::Value::from(column.name());
                let value = serde_json::Value::from(row.get(index));
                format!("{name}:{value}")
            })
            .collect();
        rows.push(format!("{{{}}}", columns.join(",")));
    }
    Ok(format!("[{}]", rows.join(",")))
}

/// The first word of the statement, lowercase, after comments.
fn first_word(sql: &str) -> String {
    words(skip_comments(sql))
        .into_iter()
        .next()
        .unwrap_or_default()
}

fn skip_comments(sql: &str) -> &str {
    let mut rest = sql;
    loop {
        rest = rest.trim_start();
        if let Some(comment) = rest.strip_prefix("--") {
            rest = comment.split_once('\n').map_or("", |(_, after)| after);
        } else if let Some(comment) = rest.strip_prefix("/*") {
            rest = comment.split_once("*/").map_or("", |(_, after)| after);
        } else {
            return rest;
        }
    }
}

fn words(text: &str) -> Vec<String> {
    text.split(|c: char| !c.is_ascii_alphanumeric() && c != '_')
        .filter(|word| !word.is_empty())
        .map(str::to_ascii_lowercase)
        .collect()
}

/// Whether the statement starts with WITH and mentions INSERT, UPDATE, DELETE or MERGE. One that only
/// mentions them (in a string, a comment) takes the CREATE TABLE AS path as well, with the same result.
pub fn modifying_with(sql: &str) -> bool {
    let words = words(skip_comments(sql));
    words.first().is_some_and(|word| word == "with")
        && words
            .iter()
            .any(|word| matches!(word.as_str(), "insert" | "update" | "delete" | "merge"))
}

/// PHP decodes a JSON number with a fraction or exponent as float. A `numeric` value a float cannot
/// hold exactly (123456789012345678.90) goes to PHP as a string instead, as PDO returns it; one it
/// can hold (9.99, a float8) stays a number. Integers stay: JSON_BIGINT_AS_STRING covers them.
fn exact_numbers(json: String) -> String {
    let bytes = json.as_bytes();
    let mut out = String::with_capacity(json.len());
    let (mut start, mut i, mut in_string) = (0, 0, false);
    while i < bytes.len() {
        let byte = bytes[i];
        if in_string {
            match byte {
                b'\\' => i += 1,
                b'"' => in_string = false,
                _ => {}
            }
            i += 1;
        } else if byte == b'"' {
            in_string = true;
            i += 1;
        } else if byte == b'-' || byte.is_ascii_digit() {
            let end = i + bytes[i..]
                .iter()
                .position(|b| !matches!(b, b'0'..=b'9' | b'-' | b'+' | b'.' | b'e' | b'E'))
                .unwrap_or(bytes.len() - i);
            let number = &json[i..end];
            if number.contains(['.', 'e', 'E'])
                && !number
                    .parse::<f64>()
                    .is_ok_and(|float| decimal(number) == decimal(&float.to_string()))
            {
                out.push_str(&json[start..i]);
                out.push('"');
                out.push_str(number);
                out.push('"');
                start = end;
            }
            i = end;
        } else {
            i += 1;
        }
    }
    out.push_str(&json[start..]);
    out
}

/// A decimal number as sign, significant digits and power of ten: 1.10, 1.1 and 11e-1 are equal.
fn decimal(number: &str) -> (bool, String, i64) {
    let (negative, number) = number
        .strip_prefix('-')
        .map_or((false, number), |rest| (true, rest));
    let (mantissa, exponent) = number
        .split_once(['e', 'E'])
        .map_or((number, 0), |(mantissa, exponent)| {
            (mantissa, exponent.parse().unwrap_or(0))
        });
    let (whole, fraction) = mantissa.split_once('.').unwrap_or((mantissa, ""));
    let digits = format!("{whole}{fraction}");
    let trimmed = digits.trim_start_matches('0').trim_end_matches('0');
    if trimmed.is_empty() {
        return (false, String::new(), 0);
    }
    let trailing = (digits.len() - digits.trim_end_matches('0').len()) as i64;
    (
        negative,
        trimmed.to_owned(),
        exponent - fraction.len() as i64 + trailing,
    )
}

#[cfg(test)]
mod tests {
    use super::{array_literal, exact_numbers};

    #[test]
    fn lists_become_postgres_array_literals() {
        // The values: a, b "c", back\slash, null, 1, true, [2, 3], {"k": 1}
        let list: serde_json::Value = serde_json::from_str(
            r#"["a", "b \"c\"", "back\\slash", null, 1, true, [2, 3], {"k": 1}]"#,
        )
        .unwrap();
        assert_eq!(
            array_literal(list.as_array().unwrap()),
            r#"{"a","b \"c\"","back\\slash",NULL,1,t,{2,3},"{\"k\":1}"}"#
        );
        assert_eq!(array_literal(&[]), "{}");
    }

    #[test]
    fn numbers_a_float_cannot_hold_become_strings() {
        assert_eq!(
            exact_numbers(r#"[{"a": 9.99, "b": 123456789012345678.90, "c": 42, "d": "1.234567890123456789", "e": 0.30000000000000004, "f": 1.10, "g": 1e+20, "h": -0.0000000000000000000123456789012345678901}]"#.into()),
            r#"[{"a": 9.99, "b": "123456789012345678.90", "c": 42, "d": "1.234567890123456789", "e": 0.30000000000000004, "f": 1.10, "g": 1e+20, "h": "-0.0000000000000000000123456789012345678901"}]"#
        );
        assert_eq!(
            exact_numbers(r#"[{"s": "say \"1.00000000000000000001\""}]"#.into()),
            r#"[{"s": "say \"1.00000000000000000001\""}]"#
        );
    }
}

async fn postgres(
    mode: String,
    target: String,
    sql: String,
    params: Vec<Value>,
) -> Result<String, Failure> {
    // Queries in a transaction use its connection
    if let Some(id) = target
        .strip_prefix("tx:")
        .and_then(|id| id.parse::<u64>().ok())
    {
        let Some((url, cell)) = TRANSACTIONS.with_borrow(|txs| {
            txs.get(&id)
                .map(|tx| (tx.url.clone(), Rc::clone(&tx.client)))
        }) else {
            return Err("transaction has already ended".into());
        };
        let ending = mode == "commit" || mode == "rollback";
        let slot = std::mem::replace(&mut *cell.borrow_mut(), Slot::Busy);
        let lease = match slot {
            Slot::Idle(lease) => lease,
            Slot::Busy => return Err("a query is already running in this transaction".into()),
            Slot::Lost => {
                *cell.borrow_mut() = Slot::Lost;
                if ending {
                    TRANSACTIONS.with_borrow_mut(|txs| txs.remove(&id));
                }
                // A rollback has already happened; nothing else works any more
                return if mode == "rollback" {
                    Ok("null".into())
                } else {
                    Err("the transaction was aborted".into())
                };
            }
        };
        let busy = Busy {
            url: url.clone(),
            slot: Some(Rc::clone(&cell)),
            lease: Some(lease),
        };
        let client = &busy.lease().client;
        let result = if ending {
            client
                .batch_execute(if mode == "commit" {
                    "COMMIT"
                } else {
                    "ROLLBACK"
                })
                .await
                .map(|()| "null".to_owned())
                .map_err(Failure::from)
        } else {
            run(busy.lease(), false, &mode, &sql, &params).await
        };
        if ending {
            TRANSACTIONS.with_borrow_mut(|txs| txs.remove(&id));
            // Ended: the connection goes back to the pool, not to the transaction
            let mut busy = busy;
            busy.slot = None;
            busy.done();
        } else {
            busy.done();
        }
        return result;
    }
    let lease = connect(&target).await?;
    if mode == "begin" {
        lease.client.batch_execute("BEGIN").await?;
        // PHP picks the ID: if it is cancelled while this finishes, it can still roll back
        let owner = params.first().and_then(Value::as_u64).unwrap_or(0);
        let id = params
            .get(1)
            .and_then(Value::as_u64)
            .unwrap_or_else(crate::php::next_id);
        TRANSACTIONS.with_borrow_mut(|txs| {
            txs.insert(
                id,
                Transaction {
                    url: target,
                    owner,
                    client: Rc::new(RefCell::new(Slot::Idle(lease))),
                },
            )
        });
        return Ok(id.to_string());
    }
    let busy = Busy {
        url: target,
        slot: None,
        lease: Some(lease),
    };
    let result = run(busy.lease(), true, &mode, &sql, &params).await;
    busy.done();
    result
}

/// For the durable machinery: a query ("query", rows as JSON), a statement ("execute", the count) or a
/// script on the pool of `url`.
pub async fn run_sql(url: &str, mode: &str, sql: &str, params: Value) -> Result<String, Failure> {
    let params = match params {
        Value::Array(list) => list,
        _ => Vec::new(),
    };
    postgres(mode.to_owned(), url.to_owned(), sql.to_owned(), params).await
}

/// $args: the command and its arguments as "<length>:<bytes>" one after another, so that any bytes
/// pass. The reply comes back in PHP's serialize() format, binary-safe as well.
async fn redis_command(url: String, args: Vec<u8>) -> Result<Vec<u8>, Failure> {
    let existing = REDIS.with_borrow(|connections| connections.get(&url).cloned());
    let mut connection = match existing {
        Some(connection) => connection,
        None => {
            let config = redis::AsyncConnectionConfig::new()
                .set_connection_timeout(Some(std::time::Duration::from_secs(10)))
                .set_response_timeout(Some(std::time::Duration::from_secs(30)));
            let connection = redis::Client::open(url.as_str())?
                .get_multiplexed_async_connection_with_config(&config)
                .await?;
            REDIS
                .with_borrow_mut(|connections| connections.insert(url.clone(), connection.clone()));
            connection
        }
    };
    let mut command = redis::Cmd::new();
    let mut rest = args.as_slice();
    while !rest.is_empty() {
        let colon = rest
            .iter()
            .position(|&byte| byte == b':')
            .ok_or("malformed Redis arguments")?;
        let length: usize = std::str::from_utf8(&rest[..colon])
            .ok()
            .and_then(|length| length.parse().ok())
            .ok_or("malformed Redis arguments")?;
        let arg = rest[colon + 1..]
            .get(..length)
            .ok_or("malformed Redis arguments")?;
        command.arg(arg);
        rest = &rest[colon + 1 + length..];
    }
    let value: redis::Value = match command.query_async(&mut connection).await {
        Ok(value) => value,
        Err(error) => {
            // After a Redis restart the connection would otherwise stay dead forever: reconnect next time.
            // An error Redis itself answers (WRONGTYPE, a script error) leaves the connection as it is
            if error.is_unrecoverable_error() || error.is_io_error() || error.is_timeout() {
                REDIS.with_borrow_mut(|connections| connections.remove(&url));
            }
            return Err(error.into());
        }
    };
    let mut serialized = Vec::new();
    serialize(value, &mut serialized);
    Ok(serialized)
}

/// A Redis reply as PHP's serialize() writes it; a map becomes a list of [key, value] pairs.
fn serialize(value: redis::Value, out: &mut Vec<u8>) {
    use std::io::Write;
    let string = |out: &mut Vec<u8>, bytes: &[u8]| {
        let _ = write!(out, "s:{}:\"", bytes.len());
        out.extend_from_slice(bytes);
        out.extend_from_slice(b"\";");
    };
    let list = |out: &mut Vec<u8>, items: Vec<redis::Value>| {
        let _ = write!(out, "a:{}:{{", items.len());
        for (index, item) in items.into_iter().enumerate() {
            let _ = write!(out, "i:{index};");
            serialize(item, out);
        }
        out.push(b'}');
    };
    match value {
        redis::Value::Nil => out.extend_from_slice(b"N;"),
        redis::Value::Int(number) => {
            let _ = write!(out, "i:{number};");
        }
        redis::Value::BulkString(bytes) => string(out, &bytes),
        redis::Value::SimpleString(text) => string(out, text.as_bytes()),
        redis::Value::Okay => string(out, b"OK"),
        redis::Value::Array(items) | redis::Value::Set(items) => list(out, items),
        redis::Value::Boolean(flag) => {
            let _ = write!(out, "b:{};", u8::from(flag));
        }
        redis::Value::Double(number) if number.is_nan() => out.extend_from_slice(b"d:NAN;"),
        redis::Value::Double(number) if number.is_infinite() => {
            out.extend_from_slice(if number > 0.0 { b"d:INF;" } else { b"d:-INF;" })
        }
        redis::Value::Double(number) => {
            let _ = write!(out, "d:{number:?};");
        }
        redis::Value::Map(pairs) => list(
            out,
            pairs
                .into_iter()
                .map(|(key, value)| redis::Value::Array(vec![key, value]))
                .collect(),
        ),
        other => string(out, format!("{other:?}").as_bytes()),
    }
}

/// Starts a database op. $op: "pg.query", "pg.execute", "pg.script", "pg.begin", "pg.commit", "pg.rollback" or
/// "redis"; $target: URL or "tx:<id>"; $params: JSON array (Redis: see redis_command). The result comes as JSON text.
#[unsafe(no_mangle)]
extern "C" fn tenma_rs_db(
    op: *const u8,
    op_len: usize,
    target: *const u8,
    target_len: usize,
    sql: *const u8,
    sql_len: usize,
    params: *const u8,
    params_len: usize,
) -> u64 {
    let (op, target, sql) = (
        text(op, op_len),
        text(target, target_len),
        text(sql, sql_len),
    );
    // JSON for Postgres, raw bytes for Redis
    let params = bytes(params, params_len).to_vec();
    // COMMIT and ROLLBACK run to completion even if the caller is cancelled: cancelled midway,
    // it would be unclear whether the transaction holds, and its entry would stay forever
    let spawn = if matches!(op.as_str(), "pg.commit" | "pg.rollback") {
        ops::spawn_uncancellable
    } else {
        ops::spawn
    };
    let detail = format!("{op} {} {sql}", crate::journal::redact(&target));
    spawn("db", detail, async move {
        let result = match op.split_once('.') {
            Some(("pg", mode)) => postgres(
                mode.to_owned(),
                target,
                sql,
                serde_json::from_slice(&params).unwrap_or_default(),
            )
            .await
            .map(String::into_bytes),
            _ if op == "redis" => redis_command(target, params).await,
            _ => Err(format!("unknown database op {op}").into()),
        };
        match result {
            Ok(result) => OpResult::Text(result),
            Err(error) => OpResult::Error(describe(error.as_ref())),
        }
    })
}

/// Discards the open transactions of a request that ended without commit or rollback (exit()).
/// The connection is closed, Postgres aborts the transaction and releases its locks.
#[unsafe(no_mangle)]
extern "C" fn tenma_rs_db_abandon(rid: u64) {
    TRANSACTIONS.with_borrow_mut(|txs| txs.retain(|_, tx| tx.owner != rid));
}

/// The error message with all causes: otherwise tokio-postgres only says "db error".
pub fn describe(error: &(dyn Error + 'static)) -> String {
    let mut message = error.to_string();
    let mut source = error.source();
    while let Some(cause) = source {
        message = format!("{message}: {cause}");
        source = cause.source();
    }
    message
}

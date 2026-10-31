---
title: Plain PHP under tenma
description: Which of PHP's own functions wait without blocking the worker, which still block, and how PDO for Postgres runs.
order: 115
---

Code written for blocking PHP runs under tenma without changes, and most of what waits for the
network no longer holds up the worker. Inside a fiber (a handler of `Http\Server`, a task, a timer)
only that fiber pauses; other requests of the worker keep running. Outside a fiber (the top level of
the script, classic mode) the same calls block, exactly as in PHP.

Nothing in the code tells the two apart. `fsockopen()`, `file_get_contents('https://...')`, PDO
with MySQL or Postgres, mysqli and every library built on PHP's streams, such as an SMTP, Redis or
AMQP client written in PHP, do the right thing in both places.

## What waits without blocking

| Through                               | Examples                                                                                                          |
| ------------------------------------- | ----------------------------------------------------------------------------------------------------------------- |
| PHP's socket streams                  | `fsockopen()`, `stream_socket_client()`, `tcp://`, `unix://`, `ssl://`, `tls://`, `stream_socket_enable_crypto()` |
| the `http://` and `https://` wrappers | `file_get_contents()`, `fopen()`, `file()` on a URL                                                               |
| `stream_select()`                     | waiting for the first of several sockets                                                                          |
| mysqlnd                               | PDO with `mysql:` DSNs, mysqli                                                                                    |
| tenma's PDO driver for Postgres       | PDO with `pgsql:` DSNs                                                                                            |
| libraries on PHP streams              | SMTP, Redis, AMQP, Memcached clients written in PHP                                                               |
| tenma's own classes                   | `Http\Client`, `Data\Postgres`, `Data\Redis`, `FileSystem\FileInfo`, `Threading\Task::delay()`                    |

Timeouts come from where PHP takes them: `default_socket_timeout`, `stream_set_timeout()`, the
`timeout` argument of `fsockopen()`, PDO's `PDO::ATTR_TIMEOUT`. A read that runs out of time returns
`false` with `timed_out` set in `stream_get_meta_data()`, as in PHP. TLS, certificate checks, SNI and
the `ssl` context options stay with ext/openssl. When the client of a request leaves, the fiber is
cancelled where it waits, also inside `fgets()` or a PDO query: `Threading\Cancelled` is thrown there.

## What still blocks

| Call                                                       | Why                                       | Instead                                                      |
| ---------------------------------------------------------- | ----------------------------------------- | ------------------------------------------------------------ |
| curl                                                       | its own sockets, outside PHP's streams    | `Http\Client` or the `https://` wrapper                      |
| `socket_*()` of ext/sockets                                | raw sockets, outside PHP's streams        | `stream_socket_client()`                                     |
| files on disk: `fopen()`, `file_get_contents()`, `include` | the disk has no readiness to wait for     | `FileSystem\FileInfo` for large files; small reads are quick |
| `sleep()`, `usleep()`                                      | the process sleeps                        | `Threading\Task::delay()`                                    |
| PDO with SQLite                                            | a library call, no socket                 | fine for small databases                                     |
| `password_hash()`, `password_verify()`                     | Argon2 computes 100 ms or more            | `Security\Password`, on a background thread                  |
| `--pdo-pgsql native`, ext/pgsql (`pg_*()`)                 | libpq brings its own socket               | PDO without the option, `Data\Postgres`                      |
| name resolution                                            | `getaddrinfo()` runs on the worker thread | an IP address, or a resolver that answers fast               |

A long computation blocks too: a fiber gives way only when it waits.

## PDO for Postgres

`new PDO('pgsql:host=...;dbname=...')` talks to the server through tenma's own driver, built on the
same client as `Data\Postgres`, so a query inside a fiber waits like every other tenma call. The driver
behaves as ext/pdo_pgsql does: `?` and `:name` placeholders, prepared statements, `PDO::ATTR_EMULATE_PREPARES`,
transactions, `lastInsertId()`, `quote()`, `errorInfo()` with the SQLSTATE, `getColumnMeta()` with
`pgsql:oid` and `native_type`, bools, integers and floats as PHP values, `bytea` as a stream, every
other type as the text Postgres prints. Every PDO object is one connection, as with libpq, and holds
its prepared statements. Scrollable cursors work on the fetched rows, so every `PDO::FETCH_ORI_*` is
cheap.

Not covered: large objects, `COPY`, unbuffered fetching, TLS to the server and the `Pdo\Pgsql` class
with its `pgsql*()` methods. A script that needs them runs with `tenma --pdo-pgsql native file.php`,
which keeps ext/pdo_pgsql and libpq; its queries then block the worker.

## Behaviour

- A socket stream's descriptor is always non-blocking; `stream_set_blocking($stream, false)` still
  makes reads and writes return at once, as in PHP.
- A host name with several addresses (`localhost`: `::1` and `127.0.0.1`) is tried without blocking
  on its first address only; if that one fails, PHP's own connect tries the rest and blocks meanwhile.
- `stream_select()` on streams without a descriptor, or with data in their read buffer, runs PHP's own
  implementation.
- Servers (`stream_socket_server()`, `stream_socket_accept()`) keep PHP's blocking behaviour.
- The mysqlnd settings `mysqlnd.net_read_timeout` and the connect timeout of PDO apply unchanged.

## See also

- [Event loop and fibers](event-loop.md): how waiting works
- [Outgoing HTTP](fetch.md), [Postgres, Redis and pub/sub](database.md): tenma's own clients

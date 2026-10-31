# Architecture

How tenma is built, what it measured, and what it does not do yet. The [README](README.md) has the
commands, the [documentation](docs/index.md) the features.

## Three layers

- **Rust** (`src/`) runs the process: threads, event loops, HTTP, and every operation that waits.
- **C** (`shim/tenma.c`) embeds PHP. It calls the PHP handlers, converts values between PHP and C, and
  catches fatal errors. It also wraps PHP's socket transports and `stream_select()` so that they wait in
  ops, and implements PDO's driver interface for Postgres. Rust never sees a PHP value directly.
- **PHP** (`stdlib/`) is the API the scripts use: `Http\Server`, `Request`, `Response`, `Threading\Task`,
  `Data\Postgres`, `FileSystem\FileInfo`. It is embedded in the binary.

## Files

Rust:

| File              | Does                                                                                                                        |
| ----------------- | --------------------------------------------------------------------------------------------------------------------------- |
| `src/main.rs`     | command line, worker threads, accepting connections, the HTTP server                                                        |
| `src/php.rs`      | safe wrappers around the C layer; pending requests, resuming fibers                                                         |
| `src/stdlib.rs`   | loads the embedded PHP files of `stdlib/`                                                                                   |
| `src/ops.rs`      | ops PHP waits for: `sleep`, `fetch`                                                                                         |
| `src/db.rs`       | Postgres (a pool per worker) and Redis (one connection per URL)                                                             |
| `src/fs.rs`       | file operations for `FileSystem\FileInfo` and `DirectoryInfo`, on background threads                                        |
| `src/password.rs` | Argon2id hashes for `Security\Password`, on background threads, at most one per CPU                                         |
| `src/pubsub.rs`   | `Messaging\Channel::publish()` and `subscribe()`, through Redis with `--pubsub`                                             |
| `src/net.rs`      | waits for PHP's own sockets (fsockopen, mysqlnd, the http wrapper) and `stream_select()` without blocking                   |
| `src/pdo.rs`      | the PDO driver for Postgres on tokio-postgres, on a helper thread; the C layer implements PDO's driver interface            |
| `src/files.rs`    | static files: prefixes, ETag and 304, ranges                                                                                |
| `src/tls.rs`      | `--tls-cert`, `--tls-key` and `--tls-port`: the TLS handshake and ALPN (HTTP/2 or HTTP/1.1), SIGHUP reloads the certificate |
| `src/streams.rs`  | streamed responses and WebSockets                                                                                           |
| `src/watch.rs`    | `--watch`: which loaded files really changed (content, not timestamps)                                                      |
| `src/dev.rs`      | the hooks of `tenma.json`                                                                                                   |
| `src/inspect.rs`  | `--inspect`: the debugger, stops a worker inside PHP's statement handler                                                    |
| `src/journal.rs`  | `--record`: the event journal of a worker life, its format, and the values of the wrapped PHP functions                     |
| `src/replay.rs`   | `tenma replay`: plays a journal into a fresh interpreter, answers every op from it, checks responses                        |
| `src/durable.rs`  | `--durable`: the journal of a durable function in Postgres, its lease, its timers and signals, the poller                   |
| `src/plugins.rs`  | `--plugin`: loads native plugins with dlopen and implements the API of `tenma-plugin/tenma_plugin.h`                        |

PHP, in `stdlib/`: one file per class, folders by namespace, the functions of a namespace in its
`functions.php`. `build.rs` embeds every file; `src/stdlib.rs` loads them.

| Path                          | Does                                              |
| ----------------------------- | ------------------------------------------------- |
| `stdlib/Http/` and so on      | the public API, one folder per namespace: `Http`, |
|                               | `Threading`, `FileSystem`, `Messaging`, `Data`,   |
|                               | `Development`, `Lsp`                              |
| `stdlib/Tenma/Internal/`      | the scheduler, the dev hooks, diagnostics         |
| `stdlib/Tenma/Internal/Lsp`   | `tenma lsp`, loaded only for that command         |
| `stdlib/Tenma/Internal/Docs`  | `tenma docs`, loaded only for that command        |
| `stdlib/Tenma/Internal/Stubs` | `tenma --stubs`, loaded only for that command     |

Everything else:

| Path                 | Does                                                                              |
| -------------------- | --------------------------------------------------------------------------------- |
| `tenma-plugin/`      | the C ABI of native plugins: `tenma_plugin.h` and the Rust crate of the same name |
| `tests/plugins/echo` | the plugin of the test suite, in C, built by `cargo test`                         |
| `php-build/build.sh` | builds PHP 8.5 as a static library (embed SAPI, ZTS) with its extensions          |
| `demo/io/`           | an Io app that runs under tenma and unchanged under `php -S`                      |
| `tests/http.rs`      | starts the binary and sends real requests                                         |
| `tests/lsp.rs`       | talks LSP to `tenma lsp` on `tests/fixtures/lsp`                                  |
| `tests/docs.rs`      | runs `tenma docs` on `tests/fixtures/docs` and on tenma's own API                 |

`build.sh` lists the extensions and the libraries they need. On macOS the libraries come from the SDK
and Homebrew, on Linux from the distribution's `-dev` packages. They are linked dynamically.

## Startup

1. The main thread starts PHP.
2. The first worker thread gets its own interpreter, loads the standard library and runs the script.
3. If the script registered no server, it was an ordinary script and tenma ends.
4. Otherwise the other workers start and run the script too. `Http\Server::start()` only registers
   the handler.
5. The main thread opens the ports, accepts connections and hands them to the workers in turn. A
   connection stays with its worker.

## A request

1. The worker calls the PHP handler on its event loop thread. The handler runs in a fiber.
2. When the handler waits, for example with `Threading\Task::delay()`, PHP starts an op. Rust starts a task for it
   and returns an op ID, and the fiber suspends.
3. The event loop serves other requests meanwhile.
4. When the task is done, Rust calls `Tenma\Internal\complete()`, which resumes the fiber with the
   result.
5. The finished handler answers with `tenma_respond()`, and Rust sends the response.

Classic mode has no fibers. Superglobals, headers and output buffers exist once per interpreter, so
requests run one after another, and functions that wait throw.

## Errors and exit

- An exception in a handler gives a 500. The server keeps running.
- A fatal error restarts the interpreter of its worker only.
- `exit()` and `die()` end PHP the same way a fatal error does. The C layer tells them apart by the last
  PHP error: without a fatal error the request is answered and the worker keeps running.
- Errors and warnings go to stderr, never into a response.

## Classic mode output

- tenma collects the output where PHP writes it, not in an `ob_start()` buffer. So a framework that
  flushes every output buffer still writes into the response.
- At the end of a request, every buffer still open is flushed, also those that cannot be removed.
- Before every request, the C layer resets headers, status and the "headers already sent" state.

## Releases

A tag `v*` builds the binaries for macOS arm64 and Linux x64 and arm64 as a GitHub release, and the
image `ghcr.io/tenma-runtime/tenma` (`.github/workflows/build.yml`). The Linux binaries link the libraries of
Debian 13. On another distribution, use the image.

## Measurements

Measured on an M-series Mac with `ab`.

| What                                                      | Result                          |
| --------------------------------------------------------- | ------------------------------- |
| a simple handler, `ab -k`                                 | about 100,000 requests/s        |
| Io demo `GET /waypoints/1`, `ab -k`, 1 worker             | 55,000 requests/s               |
| the same with 4 workers                                   | 165,000 requests/s              |
| the same with 10 workers                                  | 192,000 requests/s              |
| the same, 4 × `ab` in parallel                            | 207,000 requests/s at 570 % CPU |
| without keep-alive, any number of workers                 | about 45,000 requests/s         |
| one Postgres query per request, 200 at once, 10 conns     | about 25,000 requests/s         |
| four Redis commands per request                           | about 42,000 requests/s         |
| `Threading\Task::delay(0.5)` per request, 500 connections | about 900 requests/s            |
| `fetch` to the same server per request                    | about 37,000 requests/s         |
| 500 streams of 0.6 s each, 1 worker                       | about 780 streams/s, 58 MB      |
| WebSocket echo, 1 connection                              | 46,000 round trips/s            |
| WebSocket echo, 4 connections                             | about 118,000 round trips/s     |
| 5 million requests against the Io demo                    | the process stays at 22.8 MB    |
| 2.1 million requests with fibers and `fetch`              | 26.8 MB                         |

- Without keep-alive, setting up connections and the single accepting thread are the limit.
- With `Threading\Task::delay(0.5)`, 900 requests per second means all 500 are awake at the same time.
- Opcache is on. It adds about 6 % throughput, because each worker compiles the code only once anyway.
- PHP builds no JIT with ZTS on Apple Silicon. On Linux it is built in: `-d opcache.jit=tracing`.

## Not yet

- TLS for Postgres connections.
- Name resolution: `getaddrinfo()` runs on the worker thread. Blocking PHP functions that do not go
  through PHP's streams (curl, `socket_*()`, files) hold up their worker.
- Waiting at the top level of a script. `Task::delay()` during startup throws; wrap it in
  `Task::run()`.
- Streaming large request bodies. Up to `--max-body` they are held in memory.
- A timeout for handlers that compute instead of wait. An endless loop holds up its worker.
- Several accepting threads (`SO_REUSEPORT`) for many short connections without keep-alive.
- A dedicated I/O thread for the WebSockets a native plugin adopted: their reads wait while their
  worker is busy in PHP, and a worker restart ends them.

---
title: Tenma
description: PHP 8.5 as a long-running process with an event loop, fibers and a built-in HTTP server.
order: 1
---

Tenma runs PHP 8.5 as one long-running process with a built-in HTTP server. The process has worker threads, and each worker has its own PHP interpreter and its own event loop. Every request runs in its own fiber, so a request that waits does not hold up the others.

Tenma is a single binary with PHP 8.5 (ZTS) and its usual extensions built in. Its PHP API lives in the top-level namespaces `Http`, `Threading`, `FileSystem`, `Messaging`, `Data`, `Development` and `Lsp`. It is part of the binary, not a Composer package.

## Install

```bash
curl -fsSL https://raw.githubusercontent.com/tenma-runtime/tenma/main/packaging/install.sh | sh
```

The script installs the Homebrew formula on macOS (Apple Silicon), a `.deb` on Debian 13 and Ubuntu 24.04, and the plain binary in `~/.local/bin` on other Linux systems. To install by hand:

| System                  | Command                                                                                                       |
| ----------------------- | ------------------------------------------------------------------------------------------------------------- |
| macOS on Apple Silicon  | `brew install tenma-runtime/tap/tenma`                                                                        |
| Debian 13, Ubuntu 24.04 | `apt install ./tenma_<version>_amd64.deb` from the [release](https://github.com/tenma-runtime/tenma/releases) |
| other Linux             | `tenma-linux-x64.tar.gz` from the release, binary into `~/.local/bin`                                         |
| Docker                  | `docker run -p 8080:8080 -v "$PWD:/app" ghcr.io/tenma-runtime/tenma /app/index.php`                           |

`tenma --version` prints the Tenma and PHP versions.

## Hello world

Save this as `app.php`:

```php
<?php

new Http\Server(8080, fn () => "Hello, world\n")->start();

echo "Listening on http://localhost:8080\n";
```

Run it:

```bash
tenma app.php
```

The script runs once. It registers the server and prints the line, then Tenma opens the port. `curl localhost:8080` answers `Hello, world`. During development, `tenma --watch app.php` reloads the script when a file it loaded changes.

## The model

- **Process**: one `tenma` process serves the app. Its main thread accepts connections. See [Process and workers](guides/process.md).
- **Worker**: a thread with its own PHP interpreter. The script runs once per worker, and what it builds stays in memory. See [State in memory](guides/state.md).
- **Event loop**: each worker has one. It runs PHP code whenever something the code waits for is ready. See [Event loop and fibers](guides/event-loop.md).
- **Fiber**: every request, task and timer callback runs in its own fiber. A fiber that waits is suspended, and the others keep running. See [Lifetime of a request](guides/request-lifetime.md).
- **Op**: an operation Tenma runs outside PHP, such as a sleep, an HTTP request, a query or a file read. A fiber waits for its op without blocking the worker.

## How these docs are organised

The [guides](guides/index.md) come in four parts:

- **Concepts**: the process, the event loop, the lifetime of a request, state in memory, fiber mode and classic mode.
- **APIs**: one page per module, such as HTTP, outgoing requests, tasks and timers, streams, WebSockets, files and databases.
- **Tooling**: watch mode, the debugger, the language server, diagnostics and reference pages.
- **Operations**: the command line, workers, limits and deployment.

The [reference](reference/index.md) has one page per class of the PHP API, generated from the code with `tenma docs`.

A complete example app, [Io Bakery](https://github.com/io-framework/bakery), runs on Tenma.

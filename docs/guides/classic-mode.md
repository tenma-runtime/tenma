---
title: Fiber mode and classic mode
description: Serve requests in fibers with Http\Server, or run code written for PHP-FPM unchanged with Http\ClassicServer.
order: 140
---

Tenma serves HTTP in two modes. Fiber mode (`Http\Server`) runs many requests at once in each worker. Classic mode (`Http\ClassicServer`) runs code written for PHP-FPM unchanged, one request at a time.

|                          | Fiber mode: `Http\Server`                                                         | Classic mode: `Http\ClassicServer`                                  |
| ------------------------ | --------------------------------------------------------------------------------- | ------------------------------------------------------------------- |
| Request                  | a `Request` argument                                                              | `$_GET`, `$_POST`, `$_SERVER`, `$_FILES`, `$_COOKIE`, `php://input` |
| Response                 | the handler's return value                                                        | `header()`, `http_response_code()`, `echo`                          |
| Concurrency              | many requests at once, each in its own fiber                                      | one request after another per worker                                |
| Waiting                  | `Task::delay()`, `Http\Client`, Postgres, Redis, `FileSystem` suspend one request | the functions that wait throw; PDO and curl block as under FPM      |
| Streams, SSE, WebSockets | yes                                                                               | no                                                                  |
| Sessions                 | build your own (cookie and Redis or Postgres)                                     | `session_start()` and `$_SESSION` as usual                          |

In both modes the script runs once per worker. Autoloading, configuration and the container are built once, not per request.

## Classic mode

`Http\ClassicServer` calls `$app` for every request. `$app` reads the superglobals and writes the response with `header()` and `echo`, as under FPM. It also gets the raw body as its argument.

A framework's front controller runs this way:

```php
<?php

use Http\ClassicServer;

require __DIR__ . '/vendor/autoload.php';

$app = require __DIR__ . '/app.php';   // built once per worker

new ClassicServer(8080, function (string $rawBody) use ($app) {
    $app->handleHttp();   // reads the superglobals, calls header() and echo
})->start();
```

A self-contained example:

```php
<?php

use Http\ClassicServer;

$started = date('H:i:s');

new ClassicServer(8080, function () use ($started) {
    session_start();
    $visits = ($_SESSION['visits'] ?? 0) + 1;
    $_SESSION['visits'] = $visits;

    if (($_GET['format'] ?? '') === 'json') {
        header('Content-Type: application/json');
        echo json_encode(['visits' => $visits, 'started' => $started]);
        return;
    }

    if ($_SERVER['REQUEST_METHOD'] === 'POST') {
        http_response_code(201);
        echo 'Saved ', htmlspecialchars($_POST['name'] ?? ''), "\n";
        exit;   // ends only this request
    }

    echo "Visit $visits in this session, worker started at $started\n";
})->start();
```

## What classic mode sets up

Before every request:

- `$_SERVER`, `$_GET`, `$_POST`, `$_FILES`, `$_COOKIE`, `$_REQUEST` (POST over GET) and `php://input` are ready.
- Uploads go through PHP's own multipart parser, so `move_uploaded_file()` works. Temporary files disappear with the next request.
- `$_COOKIE` comes from PHP's own parser. The first cookie of a name wins.
- `REMOTE_ADDR` and `SERVER_PORT` come from the connection.
- `SERVER_NAME` comes from `--server-name` (default `localhost`), never from the Host header.
- `SCRIPT_NAME` and `PHP_SELF` are `/index.php`, and `DOCUMENT_ROOT` is the script's directory.
- Headers that contain underscores are dropped, so `x_forwarded_for` cannot pose as `X-Forwarded-For`.
- Headers, the status and the "headers already sent" state are reset.

During and after every request:

- `header()`, `http_response_code()` and all output become the response.
- Tenma collects the output where PHP writes it. A framework that flushes every output buffer still writes into the response.
- Output buffers still open at the end are flushed.
- `exit()` and `die()` end only the request.
- `ini_set()` and `set_time_limit()` changes are undone. `php://input` handles are closed.
- `register_shutdown_function()` runs after every request, as under FPM, with `error_get_last()` available for error pages. Its output is still part of the response.

## Sessions

- `session_start()` and `$_SESSION` work as usual. `$_SESSION` exists only after `session_start()`.
- At the end of every request Tenma writes and closes the session and forgets its ID. No visitor sees the previous visitor's session.
- With several workers, all share the session files. Concurrent requests of one session wait for the file lock, as under FPM.

## Behaviour

- Superglobals, headers and output buffers exist once per interpreter. So classic requests run one after another in each worker.
- Run more workers (`--workers auto`) for more parallel requests.
- A slow PDO query holds up its worker, as it would hold up an FPM child. Keep request work short.
- The functions that wait (`Task::delay()`, `Http\Client`, `FileSystem\FileInfo`, `Data\Postgres`, `Data\Redis`) throw a `LogicException` in classic mode.
- PHP errors and warnings go to the log on stderr, never into a response.
- After a fatal error the request gets a 500, then the worker restarts.
- Shutdown functions registered at script startup run only when the worker ends.

## Fiber mode

In fiber mode the handler gets a `Request` and returns a response. Thousands of requests can wait at the same time in one worker. [Event loop and fibers](event-loop.md) explains how.

Write new code for fiber mode. Move an existing app over when it waits a lot: APIs, databases, streams, WebSockets.

Rules when moving code to fiber mode:

- Read the request from the `Request` object (`$request->query`, `form()`, `files()`, `cookies()`, `header()`), never from superglobals.
- The connection's addresses are in the `Request` too: `remoteAddress`, `serverPort`, `host()`.
- Return a `Response` instead of calling `header()` or `echo`.
- Keep per-request state out of statics, globals and singletons ([State in memory](state.md)).
- Use the clients that wait without blocking: [`Data\Postgres`](database.md) instead of PDO, [`Http\Client`](fetch.md) instead of curl, [`FileSystem\FileInfo`](files.md) instead of `file_get_contents()`.
- Shutdown functions run only when the worker ends, and they accumulate until then.

## Integrations

A PSR-15 app runs in fiber mode through [tenma/psr](https://github.com/tenma-runtime/psr):

```php
new Http\Server(3000, Tenma\Psr\Psr::handler($app, new Nyholm\Psr7\Factory\Psr17Factory()))->start();
```

A [Waypoint](https://github.com/systonia/Waypoint) app is a handler itself and runs with `new Http\Server(8080, $app)->start()`.

See also: [`Http\ClassicServer`](../reference/Http/ClassicServer.md), [`Http\Server`](../reference/Http/Server.md), [Serving HTTP](http.md).

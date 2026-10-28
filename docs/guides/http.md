---
title: HTTP server
description: Start a server with Http\Server, read requests, build responses, set cookies, take uploads and serve static files.
order: 200
---

`Http\Server` starts an HTTP server on a port and calls a handler for every request. Each request runs in its own fiber. The handler gets a `Request` and returns a response.

| Class or method                                                                       | Does                                                                      |
| ------------------------------------------------------------------------------------- | ------------------------------------------------------------------------- |
| [`new Http\Server($port, $handler, static: …)->start()`](../reference/Http/Server.md) | registers a server; it starts once the script has run to its end          |
| [`Request`](../reference/Http/Request.md)                                             | method, URI, path, query, headers, body, cookies, form fields and uploads |
| [`Response`](../reference/Http/Response.md)                                           | status, headers and body; `json()`, `html()`, `redirect()`, `cookie()`    |
| [`UploadedFile`](../reference/Http/UploadedFile.md)                                   | one uploaded file, held in memory                                         |
| [`Response::stream()`, `Response::events()`](streams.md)                              | a body sent bit by bit, server-sent events                                |
| [`Response::websocket()`](websockets.md)                                              | turns the request into a WebSocket                                        |

## Starting a server

```php
<?php

use Http\{Request, Server};

new Server(8080, function (Request $request): string {
    return "{$request->method} {$request->path}\n";
})->start();
```

`tenma server.php` runs the script, then opens port 8080. The script itself does not block: `start()` only registers the server. A script may start several servers on different ports.

Code outside the handler runs once, when the script starts. Put the expensive setup there: configuration, routes, templates, database connections.

The handler returns one of these:

| Return value                                 | Sent as                                               |
| -------------------------------------------- | ----------------------------------------------------- |
| a string                                     | status 200, `Content-Type: text/plain; charset=utf-8` |
| a `Response`                                 | its status, headers and body                          |
| `Response::stream()` or `Response::events()` | a [streamed body](streams.md)                         |
| `Response::websocket()`                      | a [WebSocket](websockets.md)                          |

## Reading the request

```php
<?php

use Http\{Request, Response, Server};

new Server(8080, function (Request $request): Response {
    return Response::json([
        'method' => $request->method,              // GET, POST, ...
        'uri' => $request->uri,                    // /users?page=2
        'path' => $request->path,                  // /users
        'query' => $request->query,                // ['page' => '2']
        'agent' => $request->header('user-agent'), // null if missing
        'host' => $request->host(),                // Host header without port
        'client' => $request->remoteAddress,       // IP of the client or proxy
        'session' => $request->cookie('session'),  // null if not set
    ]);
})->start();
```

| Property or method            | Holds                                                                                  |
| ----------------------------- | -------------------------------------------------------------------------------------- |
| `method`, `uri`, `path`       | the request line; `uri` includes the query string                                      |
| `query`                       | the query string, parsed like `$_GET`                                                  |
| `headers`, `header($name)`    | every header, names in lowercase; `header()` ignores case and returns null if missing  |
| `host()`                      | the Host header without port, otherwise the server address                             |
| `remoteAddress`, `remotePort` | the client, or the proxy in front of it                                                |
| `serverAddress`, `serverPort` | the address and port the connection arrived on                                         |
| `https`                       | whether the connection came over TLS ([HTTPS](deployment.md#https))                    |
| `protocol`                    | the version the client spoke: `HTTP/1.1`, `HTTP/2` (over TLS) or `HTTP/1.0`            |
| `body`                        | the raw body                                                                           |
| `json()`                      | the body decoded as JSON into arrays                                                   |
| `form()`                      | fields of `application/x-www-form-urlencoded` and `multipart/form-data`, like `$_POST` |
| `files()`                     | uploads from `multipart/form-data`, by field name                                      |
| `cookies()`, `cookie($name)`  | the cookies, parsed like `$_COOKIE`                                                    |

`form()` follows the rules of `$_POST`: `tags[]` appends, `address[city]` nests. The client controls the Host header. Check `host()` against a list of your hosts before you use it in links or redirects.

## Building responses

```php
<?php

use Http\{Request, Response, Server};

new Server(8080, function (Request $request): Response {
    if ($request->path === '/login' && $request->method === 'POST') {
        $name = $request->form()['name'] ?? 'guest';
        $response = Response::redirect('/', 303);

        return $response->cookie('user', $name, expires: time() + 3600);
    }

    if ($request->path === '/users') {
        $response = Response::json([['id' => 1, 'name' => 'Ada']]);
        $response->header('cache-control', 'no-store');

        return $response->header('vary', 'cookie', append: true);
    }

    if ($request->path === '/') {
        $user = htmlspecialchars($request->cookie('user') ?? 'guest');

        return Response::html("<p>Hello, $user</p>");
    }

    return new Response("Not found\n", 404);
})->start();
```

| Method                                   | Does                                                                              |
| ---------------------------------------- | --------------------------------------------------------------------------------- |
| `new Response($body, $status, $headers)` | any response; the default `Content-Type` is `text/plain; charset=utf-8`           |
| `Response::json($data, $status)`         | encodes with `JSON_PRETTY_PRINT` and `JSON_UNESCAPED_UNICODE`, `application/json` |
| `Response::html($html, $status)`         | `text/html; charset=utf-8`                                                        |
| `Response::redirect($location, $status)` | 302 by default; 303 after a POST, 301 or 308 for permanent moves                  |
| `->header($name, $value, append: false)` | replaces a header; with `append: true` it adds another value                      |
| `->cookie($name, $value, ...)`           | sets a cookie like `setcookie()`                                                  |

`header()` and `cookie()` change the response and return it.

## Cookies

`cookie()` takes `expires`, `path` (default `/`), `domain`, `secure` (default `false`), `httpOnly` (default `true`) and `sameSite` (default `Lax`). The value is URL-encoded. Without `expires`, or with 0, the cookie lasts until the browser session ends.

Reading works with `$request->cookie($name)` and `$request->cookies()`. Of several cookies with the same name, the first wins.

## Uploads

```php
<?php

use Http\{Request, Response, Server};

$uploads = __DIR__ . '/uploads';
@mkdir($uploads);

$form = '<form method="post" enctype="multipart/form-data">'
    . '<input type="file" name="file"><button>Upload</button></form>';

new Server(8080, function (Request $request) use ($uploads, $form) {
    if ($request->method === 'GET') {
        return Response::html($form);
    }

    $file = $request->files()['file'] ?? null;
    if ($file === null || $file->size() === 0) {
        return new Response("No file\n", 422);
    }

    $name = bin2hex(random_bytes(8));
    $file->moveTo("$uploads/$name");

    return Response::json(['stored' => $name, 'type' => $file->type, 'bytes' => $file->size()]);
})->start();
```

`curl -F file=@photo.jpg localhost:8080` uploads a file. An `UploadedFile` has:

- `name`: the file name the client sent, without directory. Never use it unchecked as a path.
- `type`: the type the client states. It is not trustworthy.
- `size()` and `contents()`.
- `moveTo($path)`: writes the file without blocking the server.

`files()` names fields like `form()`: `files[]` becomes a list, `files[a]` nests.

## Static files

With `static:`, Tenma serves files itself, without calling PHP.

```php
<?php

use Http\Server;

new Server(8080, fn () => "from PHP\n", static: [
    '/assets' => __DIR__ . '/public/assets',
    '/favicon.ico' => __DIR__ . '/public/favicon.ico',
])->start();
```

- Under a directory prefix, Tenma looks in the directory: `/assets/app.css` is `public/assets/app.css`.
- A single file matches only its exact path. The longest prefix wins.
- A request outside every prefix goes to the handler without touching the file system.
- A missing file under a prefix also goes to the handler, so the app can answer the 404.
- `static: __DIR__ . '/public'` is short for `['/' => __DIR__ . '/public']`. Then every request is checked against the directory first. That costs PHP routes about a third of their throughput, so prefixes are cheaper.

What Tenma sends:

- `Content-Type` from the extension, `ETag` and `Last-Modified`. A cached copy gets 304.
- A single `Range` gets 206. Files go out in chunks, not through memory.
- A directory answers with its `index.html`. A directory path without a trailing `/` is redirected.
- Only GET and HEAD are served.

What Tenma never serves:

- hidden files and directories (`.env`, `.git`),
- PHP sources (`.php`, `.phtml`, `.phar`, `.phps`, `.pht`, `.php5`, `.php7`),
- files that a symlink places outside the directory,
- Tenma's own files, wherever the directory holds them: the script, its `tenma.json`, the `php.ini` of `-c`, the TLS
  certificate and key, and the `--record` directory.

So a front controller can sit next to the assets. The directory is resolved per request: switching a `current` symlink during a deploy takes effect at once. `open_basedir` applies.

## State between requests

The script runs once, so variables and objects live as long as the process. That makes in-memory caches cheap.

```php
<?php

use Http\{Request, Server};

$squares = [];

new Server(8080, function (Request $request) use (&$squares) {
    $n = (int) ($request->query['n'] ?? 0);

    return $squares[$n] ??= ($n * $n) . "\n";   // computed once per worker
})->start();
```

Many requests run at the same time in one process. Never keep the state of one request in a global, a static or a singleton: another request would see it. Pass it along as an argument, or use [`AsyncLocal`](async.md#values-that-follow-a-request).

With `--workers N`, each worker has its own copy of the state: see [workers](deployment.md#workers).

## Behaviour

- `start()` does not block. The server starts once the script has run to its end and keeps the process alive.
- The body is read completely before the handler runs. A body above `--max-body` (default 10 MB) gets 413.
- Headers and body must arrive within `--request-timeout` (default 30 s), otherwise the client is disconnected.
- A worker accepts at most `--max-connections` connections (default 1024). More get 503 at once.
- An exception that leaves the handler becomes `500 Internal Server Error`. The message goes to stderr, never to the client. The server keeps running.
- A handler that returns something else than the types above also gets 500, with a `TypeError` in the log.
- A fatal error (out of memory, stack overflow) answers 500. The worker then restarts its interpreter and runs the script again.
- `header()` and `cookie()` throw a `ValueError` for invalid names or values (line breaks, control characters). `cookie()` also throws for `sameSite: 'None'` without `secure: true`.
- `json()` throws a `JsonException` on invalid JSON.
- `exit()` in a handler ends only that request, with 500.
- When the client disconnects, the request and its work are cancelled: see [Lifetime of a request](request-lifetime.md).

See also: [`Http\Server`](../reference/Http/Server.md), [`Request`](../reference/Http/Request.md), [`Response`](../reference/Http/Response.md), [`UploadedFile`](../reference/Http/UploadedFile.md), [Classic mode](classic-mode.md).

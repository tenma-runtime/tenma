---
title: HTTP client
description: Send HTTP requests with Http\Client without blocking the server, and read the ClientResponse.
order: 210
---

`Http\Client` sends HTTP requests and returns a `ClientResponse` for each. While it waits, only the calling fiber is suspended. Other requests keep running.

| Class or method                                                            | Does                                                   |
| -------------------------------------------------------------------------- | ------------------------------------------------------ |
| [`new Http\Client(timeout: 30.0)`](../reference/Http/Client.md)            | a client; `timeout` is seconds for the whole request   |
| [`get($url, $headers)`](../reference/Http/Client.md#get)                   | sends a GET and waits for the complete response        |
| [`post($url, $body, $headers)`](../reference/Http/Client.md#post)          | sends a POST with a raw body                           |
| [`send($method, $url, $headers, $body)`](../reference/Http/Client.md#send) | any method                                             |
| [`download($url, $path, $headers)`](../reference/Http/Client.md#download)  | sends a GET and streams the body into a file           |
| [`ClientResponse`](../reference/Http/ClientResponse.md)                    | `status`, `headers`, `body`, `header($name)`, `json()` |

## Sending a request

```php
<?php

use Http\Client;
use Threading\Task;

Task::run(function () {
    $response = new Client()->get('https://httpbin.org/get', [
        'accept' => 'application/json',
    ]);

    echo $response->status, "\n";
    echo $response->header('content-type'), "\n";
    echo $response->json()['url'], "\n";
});
```

This prints `200`, `application/json` and the URL. At the top level of a script, a request needs `Task::run()` around it. In a handler, a task or a timer it works directly.

| Parameter  | Default | Meaning                                                                  |
| ---------- | ------- | ------------------------------------------------------------------------ |
| `timeout`  | `30.0`  | seconds for the whole request, response body included; set on the client |
| `$url`     |         | an `http://` or `https://` URL                                           |
| `$headers` | `[]`    | `['name' => 'value']`                                                    |
| `$body`    | `''`    | the raw request body                                                     |
| `$method`  |         | any HTTP method, for `send()`                                            |

A client holds only its timeout, so one `new Client()` can serve a whole script.

## Reading the response

| Member          | Holds                                                       |
| --------------- | ----------------------------------------------------------- |
| `status`        | the HTTP status code                                        |
| `headers`       | a list of `Name: Value` lines                               |
| `body`          | the raw body                                                |
| `header($name)` | the first value of a header, name case-insensitive, or null |
| `json()`        | the body decoded as JSON into arrays                        |

## Sending JSON

```php
<?php

use Http\Client;
use Threading\Task;

Task::run(function () {
    $payload = json_encode(['name' => 'Ada', 'role' => 'admin']);

    $response = new Client()->post('https://httpbin.org/post', $payload, [
        'content-type' => 'application/json',
    ]);

    echo $response->json()['json']['name'], "\n";   // Ada
});
```

## Handling errors

HTTP error statuses are no exceptions: a 404 or a 500 comes back as a response with that `status`. Network errors and timeouts throw a `RuntimeException`.

```php
<?php

use Http\{Client, Response, Server};

new Server(8080, function () {
    try {
        $response = new Client(timeout: 1.0)->get('https://httpbin.org/delay/5');
    } catch (RuntimeException $e) {
        return new Response("upstream failed: {$e->getMessage()}\n", 502);
    }

    if ($response->status !== 200) {
        return new Response("upstream answered {$response->status}\n", 502);
    }

    return $response->body;
})->start();
```

## Downloading to a file

`download()` writes the body into a file chunk by chunk, so a file far larger than `memory_limit` never sits in memory. The response it returns has `status` and `headers`, its `body` is empty.

```php
<?php

use Http\Client;
use Threading\Task;

Task::run(function () {
    $path = sys_get_temp_dir() . '/dump.jsonl';
    $response = new Client(timeout: 600)->download('https://example.com/dump.jsonl', $path);

    if ($response->status !== 200) {
        throw new RuntimeException("download failed: {$response->status}");
    }

    foreach (new SplFileObject($path) as $line) {
        // one line at a time
    }
});
```

- The file is created or replaced; its directory must exist.
- Every status is written to the file, an error page too. Check `status`.
- On a network error, a timeout or a cancelled request the file is removed, and the call throws a `RuntimeException`.
- `timeout` covers the whole download.

## Several requests at once

Independent requests belong in [`Task::all()`](async.md#several-things-at-once). The total time is that of the slowest, not the sum.

```php
<?php

use Http\{Client, Response, Server};
use Threading\Task;

$client = new Client(timeout: 5.0);

new Server(8080, function () use ($client) {
    $answers = Task::all(
        uuid: fn () => $client->get('https://httpbin.org/uuid'),
        ip: fn () => $client->get('https://httpbin.org/ip'),
    );

    $statuses = array_map(fn ($answer) => $answer->status, $answers);

    return Response::json($statuses);   // {"uuid": 200, "ip": 200}
})->start();
```

To avoid a request per page view, keep the last answer in memory and refresh it with a timer: see [background work](async.md#background-work).

## Behaviour

- `get()`, `post()` and `send()` wait until the whole response body is there. Only the calling fiber waits. The body must fit under `memory_limit`; for larger ones use `download()`.
- HTTP statuses never throw. Check `status`.
- A `RuntimeException` is thrown for DNS errors, refused connections and TLS errors.
- It is also thrown when the connection is not established within 10 seconds, or the response is not complete within `timeout`.
- The message starts with the URL and names the cause: `https://example.com/slow: timed out after 2.5 s`, or `http://127.0.0.1:1/: cannot connect: Connection refused (os error 61)`.
- `json()` throws a `JsonException` on invalid JSON.
- An invalid header name or value (a line break, a control character) throws a `ValueError`.
- Redirects are followed.
- `Http\Client` works only in asynchronous code: handlers, tasks, timers. At the top level of a script and in [classic mode](classic-mode.md) it throws a `LogicException`. Use curl there.
- An outgoing request belongs to the handler request that started it. If the client disconnects, it is cancelled: see [Lifetime of a request](request-lifetime.md).

See also: [`Http\Client`](../reference/Http/Client.md), [`ClientResponse`](../reference/Http/ClientResponse.md).

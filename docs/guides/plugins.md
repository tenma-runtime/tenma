---
title: Native plugins
description: A native library (Rust, C, Zig) runs next to PHP in the tenma process, with threads of its own, and talks to PHP through Tenma\Plugin.
order: 270
---

A native plugin is a shared library that tenma loads into its process: a game engine that ticks its
worlds in their own threads and writes their snapshots straight to the players' WebSockets, an audio
mixer, an image pipeline. tenma does not know what it does. PHP talks to it by name, with bytes, and
hands WebSockets over to it; the plugin answers calls, posts events to PHP and writes to its sockets
from any thread.

```bash
tenma --plugin ./native/libvirgo.so app.php
```

Or in [`tenma.json`](configuration.md#tenmajson): `{"plugins": ["./native/libvirgo.so"]}`. A plugin
loads before any worker starts and lives as long as the process: it survives worker restarts after a
fatal error and `--watch` reloads, so a world keeps ticking while PHP code reloads. Plugins are native
code with every right of the process; they load only from the command line and the config file.

## From PHP

```php
<?php

use Http\{Request, Response, Server, WebSocket};
use Messaging\Channel;
use Tenma\Plugin;

Plugin::call('virgo', 'world_load', $bytes);            // synchronous, on this thread: quick calls
$result = Plugin::await('virgo', 'world_save', $bytes);  // the fiber waits, the plugin answers from any thread

new Channel('virgo:world:42')->subscribe(function (string $event) {
    // what the plugin posts
});

new Server(8080, function (Request $request) {
    return Response::websocket(function (WebSocket $socket) {
        // ticket checked: from here on the plugin owns the connection
        Plugin::adopt('virgo', $socket);
    });
})->start();
```

| Call                                                                                | Does                                                                                                 |
| ----------------------------------------------------------------------------------- | ---------------------------------------------------------------------------------------------------- |
| [`Plugin::call($plugin, $function, $input)`](../reference/Tenma/Plugin.md#call)     | the plugin's `call()` on the worker thread; the result bytes, or `RuntimeException` with its message |
| [`Plugin::await($plugin, $function, $input)`](../reference/Tenma/Plugin.md#await)   | the plugin's `call_async()`; the fiber waits until the plugin completes it, from any thread          |
| [`Plugin::adopt($plugin, $socket)`](../reference/Tenma/Plugin.md#adopt)             | hands an accepted WebSocket over; returns the connection ID; PHP can no longer use the socket        |
| `Channel::subscribe()`                                                              | receives what the plugin posts with `api->post()`                                                    |
| [`Process\Shutdown::register($handler)`](../reference/Process/Shutdown.md#register) | runs when tenma stops, before the plugins shut down: ask the plugin for a final save                 |

Arguments and results are bytes; the plugin and the PHP package around it agree on the format
(MessagePack, JSON, a struct). A plugin ships a small PHP package with typed functions over these
calls, so editors, the language server and PHPStan see real signatures.

## The boundary

One header, `tenma-plugin/tenma_plugin.h`, and one Rust crate, `tenma-plugin`, describe the ABI
(version 1). A plugin exports one symbol:

```c
const TenmaPlugin *tenma_plugin_init(const TenmaApi *api, uint32_t abi);
```

It returns its table, or NULL if `abi` is not the version it was built for; tenma then stops with a
message. Memory crosses the boundary only as pointer and length, valid for the duration of the call.

`TenmaApi`, tenma to plugin, callable from any thread:

| Function                           | Does                                                                                                 |
| ---------------------------------- | ---------------------------------------------------------------------------------------------------- |
| `send(conn, ptr, len, binary)`     | queues a message on an adopted connection, never waits: `TENMA_SENT`, `TENMA_FULL` or `TENMA_CLOSED` |
| `buffered(conn)`                   | bytes queued on the connection and not yet written, for back pressure                                |
| `close(conn, code)`                | closes the connection                                                                                |
| `post(channel, len, ptr, len)`     | a message to the PHP subscriptions of the channel in this process, never through Redis               |
| `complete(token, ptr, len, error)` | finishes a `call_async()`: the fiber gets the bytes, or throws with them as the message              |
| `reply(out, ptr, len)`             | the result of a synchronous `call()`                                                                 |
| `log(level, ptr, len)`             | a line in tenma's log                                                                                |

`TenmaPlugin`, plugin to tenma:

| Field                                 | Called                                                                              |
| ------------------------------------- | ----------------------------------------------------------------------------------- |
| `name`                                | the name PHP uses, and for errors                                                   |
| `call(fn, len, in, len, out)`         | on the worker thread, synchronous; 0 on success, otherwise the reply is the error   |
| `call_async(fn, len, in, len, token)` | on the worker thread; returns at once and completes the token later from any thread |
| `adopt(conn)`                         | a PHP handler handed this connection over                                           |
| `message(conn, ptr, len, binary)`     | an incoming message on an adopted connection, on tenma's I/O thread: return quickly |
| `closed(conn)`                        | the client or tenma closed an adopted connection                                    |
| `shutdown()`                          | once, when tenma stops, after PHP has run its stop handlers                         |

`adopt`, `message`, `closed` and `shutdown` may be NULL. The test suite's plugin,
`tests/plugins/echo/echo.c`, shows all of it in a hundred lines of C; the crate's documentation shows
the same in Rust.

## Stopping

On Ctrl+C or SIGTERM tenma first runs the handlers of `Process\Shutdown::register()` in every worker,
so PHP can ask the plugin for a final save and write it, then calls every plugin's `shutdown()`, then
ends. `--stop-timeout SECONDS` (default 10, also `stop-timeout` in tenma.json) bounds the handlers of
all workers together: after it tenma logs that some had not finished and goes on. A second Ctrl+C
skips the rest and stops at once. The handlers are useful without plugins too: flush a queue, close a
connection pool cleanly.

A script that starts no server calls every plugin's `shutdown()` too, when it ends; so does a start
that fails.

## Behaviour

- Every plugin name is unique; the same library given twice loads once.
- Adopted sockets live on a thread of their own, the I/O thread: a worker busy in PHP holds up
  neither their reads nor their writes, and they outlive a restart of its interpreter. `message()`
  and `closed()` come on that thread, one at a time for all connections; messages that arrived
  before the hand-over come first, on the worker. When tenma stops, the plugin hears `closed()` for
  every connection it still has, then `shutdown()`.
- An adopted socket no longer counts for `--max-connections`: how many a plugin keeps is up to it.
- `send()` reports `TENMA_FULL` once 256 messages wait for the client; the plugin decides whether to
  drop or retry. `buffered()` counts the bytes of those messages.
- `post()` is unbounded, like `Channel::publish()` within a process: send events, not snapshots.
- A `call_async()` the plugin never completes leaves the fiber waiting until its request is
  cancelled, as with every op.

## See also

- [WebSockets](websockets.md)
- [Configuration and php.ini](configuration.md#tenmajson): `tenma.json`

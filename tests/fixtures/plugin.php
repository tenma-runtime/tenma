<?php
// For tests/http.rs: the echo plugin of tests/plugins/echo through Tenma\Plugin, and a stop handler
use Http\{Request, Response, Server, WebSocket};
use Messaging\Channel;
use Tenma\Plugin;
use Process\Shutdown;
use Threading\Task;

$events = [];
new Channel('echo:events')->subscribe(function (string $event) use (&$events) {
    $events[] = $event;
});
Shutdown::register(function () {
    Task::delay(0.1);
    file_put_contents((string) getenv('ECHO_SHUTDOWN_FILE'), "php stop handler ran\n", FILE_APPEND);
});

new Server((int) getenv('PORT'), function (Request $request) use (&$events) {
    switch ($request->path) {
        case '/call':
            return Plugin::call('echo', 'upper', $request->query['text'] ?? '') . "\n";
        case '/call-error':
            try {
                Plugin::call('echo', 'nope');
                return "no error\n";
            } catch (RuntimeException $error) {
                return $error->getMessage() . "\n";
            }
        case '/await':
            // Two calls at once: the plugin answers each from a thread of its own after 0.1 s
            $started = microtime(true);
            $results = Task::all(fn () => Plugin::await('echo', 'later', 'a'), fn () => Plugin::await('echo', 'later', 'b'));
            return implode(',', $results) . ' ' . round(microtime(true) - $started, 1) . "s\n";
        case '/await-error':
            try {
                Plugin::await('echo', 'fail', 'x');
                return "no error\n";
            } catch (RuntimeException $error) {
                return $error->getMessage() . "\n";
            }
        case '/missing':
            try {
                Plugin::call('nope', 'echo');
                return "no error\n";
            } catch (RuntimeException $error) {
                return $error->getMessage() . "\n";
            }
        case '/ws':
            return Response::websocket(function (WebSocket $socket) {
                $socket->send('hello from php');
                // Handed over: the handler returns, the plugin keeps the connection
                Plugin::adopt('echo', $socket);
            });
        case '/busy':
            // Computes instead of waiting: the worker hears nothing meanwhile
            $until = microtime(true) + 0.5;
            while (microtime(true) < $until) {
            }
            return "done\n";
        case '/fatal':
            // More than memory_limit: a fatal error, the interpreter starts anew
            return str_repeat('x', 512 * 1024 * 1024);
        case '/events':
            return Response::json($events);
    }
    return "ok\n";
})->start();

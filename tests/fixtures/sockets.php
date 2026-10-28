<?php
// For tests/http.rs: PHP's own socket streams suspend only the fiber. ECHO_PORT answers a line after
// half a second; the server's own TLS port (with --tls-cert) serves /slow the same way.
use Http\{Request, Server};
use Threading\Task;

$echo = (int) getenv('ECHO_PORT');
$port = (int) getenv('PORT');
$ask = function (string $name) use ($echo): string {
    $socket = fsockopen('127.0.0.1', $echo, $errno, $error, 2);
    if ($socket === false) {
        return "fail $error";
    }
    fwrite($socket, "$name\n");
    $line = fgets($socket);
    fclose($socket);
    return trim((string) $line);
};
// Outside a fiber the stream blocks, as in PHP
$started = microtime(true);
echo $ask('startup'), ' ', round(microtime(true) - $started, 1), "s\n";

new Server($port, function (Request $request) use ($ask, $port, $echo) {
    $started = microtime(true);
    $seconds = fn () => round(microtime(true) - $started, 1) . 's';
    switch ($request->path) {
        case '/slow':
            Task::delay(0.5);
            return "slow\n";
        case '/three':
            $replies = Task::all(fn () => $ask('a'), fn () => $ask('b'), fn () => $ask('c'));
            return implode(',', $replies) . ' ' . $seconds() . "\n";
        case '/select':
            $x = fsockopen('127.0.0.1', $echo);
            $y = fsockopen('127.0.0.1', $echo);
            fwrite($x, "x\n");
            fwrite($y, "y\n");
            $read = [$x, $y];
            $write = null;
            $except = null;
            $ready = stream_select($read, $write, $except, 3);
            // One or both answers may be in by now; the array keeps only the ready ones, under their keys
            $which = $ready >= 1 && count($read) === $ready && array_filter($read, fn ($s) => $s !== $x && $s !== $y) === [] ? 'ok' : 'wrong';
            return "select $which " . $seconds() . "\n";
        case '/select-except':
            // An except set goes to PHP's own select: ordinary data is not out-of-band
            $socket = fsockopen('127.0.0.1', $echo);
            fwrite($socket, "e\n");
            $read = null;
            $write = null;
            $except = [$socket];
            $ready = stream_select($read, $write, $except, 0, 300000);
            return "except $ready " . $seconds() . "\n";
        case '/timeout':
            $socket = fsockopen('127.0.0.1', $echo);
            stream_set_timeout($socket, 0, 100000);
            fwrite($socket, "late\n");
            $line = fgets($socket);
            $meta = stream_get_meta_data($socket);
            return var_export($line, true) . ' timed_out=' . var_export($meta['timed_out'], true) . ' ' . $seconds() . "\n";
        case '/https':
            $context = stream_context_create(['ssl' => ['cafile' => __DIR__ . '/tls/ca.pem']]);
            $bodies = Task::all(
                fn () => file_get_contents("https://localhost:$port/slow", false, $context),
                fn () => file_get_contents("https://localhost:$port/slow", false, $context),
            );
            return implode('', array_map('trim', $bodies)) . ' ' . $seconds() . "\n";
        case '/tls':
            $context = stream_context_create(['ssl' => ['cafile' => __DIR__ . '/tls/ca.pem']]);
            $socket = stream_socket_client("tls://localhost:$port", $errno, $error, 2, STREAM_CLIENT_CONNECT, $context);
            if ($socket === false) {
                return "fail $error\n";
            }
            fwrite($socket, "GET /slow HTTP/1.0\r\nHost: localhost\r\n\r\n");
            $response = stream_get_contents($socket);
            return 'tls ' . trim(substr($response, -5)) . ' ' . $seconds() . "\n";
        case '/tls-async':
            // An asynchronous connect of a TLS client shakes hands too, before the first write
            $context = stream_context_create(['ssl' => ['cafile' => __DIR__ . '/tls/ca.pem']]);
            $socket = stream_socket_client("tls://localhost:$port", $errno, $error, 2, STREAM_CLIENT_CONNECT | STREAM_CLIENT_ASYNC_CONNECT, $context);
            if ($socket === false) {
                return "fail $error\n";
            }
            $meta = stream_get_meta_data($socket);
            fwrite($socket, "GET /slow HTTP/1.0\r\nHost: localhost\r\n\r\n");
            $response = stream_get_contents($socket);
            return 'tls ' . trim(substr($response, -5)) . ' crypto=' . ($meta['crypto']['protocol'] ?? 'none') . "\n";
        case '/tls-async-plain':
            // Against a server without TLS the handshake fails: nothing goes out in cleartext
            $socket = @stream_socket_client("tls://127.0.0.1:$echo", $errno, $error, 2, STREAM_CLIENT_CONNECT | STREAM_CLIENT_ASYNC_CONNECT);
            if ($socket === false) {
                return "refused\n";
            }
            @fwrite($socket, "secret\n");
            return 'connected ' . var_export(@fgets($socket), true) . "\n";
        case '/unverified':
            $socket = @stream_socket_client("tls://localhost:$port", $errno, $error, 2);
            return ($socket === false ? 'refused' : 'connected') . "\n";
        case '/refused':
            $socket = @fsockopen('127.0.0.1', 1, $errno, $error, 2);
            return ($socket === false ? "refused $errno" : 'connected') . ' ' . $seconds() . "\n";
        case '/cancelled':
            // The client leaves while the handler waits in fgets(): the fiber is cancelled there
            $socket = fsockopen('127.0.0.1', $echo);
            fwrite($socket, "never\n");
            try {
                fgets($socket);
                return "read\n";
            } catch (Threading\Cancelled $cancelled) {
                file_put_contents(getenv('CANCEL_LOG'), "cancelled in fgets\n");
                throw $cancelled;
            }
    }
    return "ok\n";
})->start();

<?php

namespace Http;

/**
 * The rules hyper checks only after the handler: a token as the name, no control character but tab
 * in the value. Line breaks would also start a header of their own (response splitting).
 */
function checkHeader(string $name, string $value): void
{
    if (!preg_match('/\A[!#$%&\'*+.^_`|~0-9A-Za-z-]+\z/', $name) || preg_match('/[\x00-\x08\x0a-\x1f\x7f]/', $value)) {
        throw new \ValueError("Invalid header: $name");
    }
}

/**
 * @param array<string, string|list<string>> $headers
 * @return list<string>
 */
function headerLines(array $headers): array
{
    $lines = [];
    foreach ($headers as $name => $values) {
        foreach ((array) $values as $value) {
            checkHeader((string) $name, (string) $value);
            $lines[] = "$name: $value";
        }
    }
    return $lines;
}

/** Runs the handler and sends what it returns: a response, a stream or a WebSocket. */
function respond(int $rid, Request $request, callable $handler): void
{
    // An exception in the handler ends only this request, not the server
    try {
        $response = $handler($request);
    } catch (\Threading\Cancelled) {
        // Cancelled: after exit() or without a client the request is already answered, then this does nothing.
        // Otherwise (a cancelled task the handler was waiting for) the client gets 500
        \tenma_respond($rid, [500, ['Content-Type: text/plain; charset=utf-8'], "Internal Server Error\n"]);
        return;
    } catch (\Throwable $error) {
        error_log('Uncaught ' . $error);
        $response = new Response("Internal Server Error\n", 500);
    }
    // A wrong response too (for example a forgotten return) ends only this request
    try {
        match (true) {
            $response instanceof StreamedResponse => runStream($rid, $response),
            $response instanceof WebSocketResponse => runWebSocket($rid, $request, $response),
            $response instanceof Response => \tenma_respond($rid, [$response->status, headerLines($response->headers), $response->body]),
            is_string($response) => \tenma_respond($rid, [200, headerLines((new Response())->headers), $response]),
            default => throw new \TypeError('An Http\\Server handler must return Response, StreamedResponse, WebSocketResponse or string, not ' . get_debug_type($response)),
        };
    } catch (\Throwable $error) {
        error_log('Uncaught ' . $error);
        \tenma_respond($rid, [500, ['Content-Type: text/plain; charset=utf-8'], "Internal Server Error\n"]);
    }
}

function runStream(int $rid, StreamedResponse $response): void
{
    if (!\tenma_stream_start($rid, $response->status, headerLines($response->headers))) {
        return;
    }
    // Only a writer that returns ends the stream normally; otherwise the client sees it cut off
    $finished = false;
    try {
        ($response->writer)(new Stream($rid));
        $finished = true;
    } catch (ConnectionClosed | \Threading\Cancelled) {
        // The client left or the request was cancelled; not an error for the server
    } catch (\Throwable $error) {
        error_log('Uncaught ' . $error);
    } finally {
        \tenma_stream_end($rid, $finished);
    }
}

function runWebSocket(int $rid, Request $request, WebSocketResponse $response): void
{
    if (strcasecmp($request->header('upgrade') ?? '', 'websocket') !== 0) {
        \tenma_respond($rid, [426, ['Content-Type: text/plain; charset=utf-8', 'Upgrade: websocket'], "Upgrade Required\n"]);
        return;
    }
    // RFC 6455 4.2: only a GET opens a WebSocket, and only version 13 exists
    if ($request->method !== 'GET') {
        \tenma_respond($rid, [405, ['Content-Type: text/plain; charset=utf-8', 'Allow: GET'], "Method Not Allowed\n"]);
        return;
    }
    if ($request->header('sec-websocket-version') !== '13') {
        \tenma_respond($rid, [426, ['Content-Type: text/plain; charset=utf-8', 'Sec-WebSocket-Version: 13'], "Upgrade Required\n"]);
        return;
    }
    if (!\tenma_ws_accept($rid)) {
        return;
    }
    $socket = new WebSocket($rid);
    // An error in the handler closes with 1011 instead of a normal close
    $finished = false;
    try {
        ($response->handler)($socket);
        $finished = true;
    } catch (ConnectionClosed | \Threading\Cancelled) {
        // The client left or the request was cancelled; not an error for the server
    } catch (\Throwable $error) {
        error_log('Uncaught ' . $error);
    } finally {
        \tenma_ws_close($rid, $finished);
    }
}

/**
 * Prepares a classic request like FPM: superglobals, php://input, uploads, output buffers.
 *
 * @param array<string, mixed> $server
 */
function startClassic(array $server, Request $request, int $rid): void
{
    \tenma_set_input($request->body);
    $contentType = $request->header('content-type') ?? '';
    $_GET = $request->query;
    $_POST = [];
    $_FILES = [];
    // Media types ignore case, as in PHP's SAPI
    if (stripos($contentType, 'application/x-www-form-urlencoded') === 0) {
        parse_str($request->body, $_POST);
    } elseif (stripos($contentType, 'multipart/form-data') === 0) {
        [$_POST, $_FILES] = \tenma_parse_multipart($contentType, $request->body);
    }
    // As under FPM, $_SESSION exists only after session_start(), never from the previous request
    unset($_SESSION);
    $_COOKIE = \tenma_parse_cookies($request->header('cookie') ?? '', true);
    // Like request_order=GP: POST overwrites GET, keys stay as they are
    $_REQUEST = array_replace($_GET, $_POST);
    $_SERVER = [
        ...$server,
        'REQUEST_METHOD' => $request->method,
        'REQUEST_URI' => $request->uri,
        'QUERY_STRING' => explode('?', $request->uri, 2)[1] ?? '',
        'SERVER_PROTOCOL' => $request->protocol,
        'SERVER_SOFTWARE' => 'tenma',
        'REQUEST_SCHEME' => $request->https ? 'https' : 'http',
        'REMOTE_ADDR' => $request->remoteAddress,
        'REMOTE_PORT' => (string) $request->remotePort,
        'SERVER_ADDR' => $request->serverAddress,
        'SERVER_PORT' => (string) $request->serverPort,
        // The configured name (--server-name), never the Host header the client controls
        'SERVER_NAME' => $server['SERVER_NAME'] ?? 'localhost',
        // As under FPM with a front controller: the script sits in the site's root directory
        'DOCUMENT_ROOT' => dirname($server['SCRIPT_FILENAME']),
        'SCRIPT_NAME' => '/' . basename($server['SCRIPT_FILENAME']),
        'PHP_SELF' => '/' . basename($server['SCRIPT_FILENAME']),
        'REQUEST_TIME' => time(),
        'REQUEST_TIME_FLOAT' => microtime(true),
    ];
    // As under FPM: HTTPS is set only for a TLS connection
    if ($request->https) {
        $_SERVER['HTTPS'] = 'on';
    }
    foreach ($request->headers as $name => $value) {
        // Headers with an underscore are dropped, otherwise x_forwarded_for would overwrite X-Forwarded-For
        if (str_contains($name, '_')) {
            continue;
        }
        $key = strtoupper(str_replace('-', '_', $name));
        $_SERVER[in_array($key, ['CONTENT_TYPE', 'CONTENT_LENGTH'], true) ? $key : "HTTP_$key"] = $value;
    }

    \Tenma\Internal\Current::$classic = $rid;
    \Tenma\Internal\Current::$scriptContext = \Tenma\Internal\Scheduler::$rootContext;
    // From here on Tenma collects everything PHP outputs, even through flushed output buffers
    \tenma_classic_begin();
}

/**
 * What PHP otherwise does at the end of every request: save and close the session. Then Tenma forgets
 * its ID, otherwise the next visitor would take it over.
 */
function endSession(): void
{
    if (session_status() === PHP_SESSION_ACTIVE) {
        session_write_close();
    }
    \tenma_forget_session();
}

/**
 * Answers the current classic request. Tenma calls this when no PHP code is running anymore and
 * the request's shutdown functions are done; their output still belongs to the response, as under FPM.
 * After an exception there is a 500 without the output so far, after a fatal error the output with 500.
 */
function finishClassic(bool $fatal): void
{
    $rid = \Tenma\Internal\Current::$classic;
    if ($rid === null) {
        return;
    }
    $failed = \Tenma\Internal\Current::$failed;
    \Tenma\Internal\Current::$classic = null;
    \Tenma\Internal\Current::$failed = false;
    \Tenma\Internal\Scheduler::$rootContext = \Tenma\Internal\Current::$scriptContext;
    $body = \tenma_take_output();
    endSession();

    if ($failed) {
        \tenma_respond($rid, [500, ['Content-Type: text/plain; charset=utf-8'], "Internal Server Error\n"]);
        return;
    }
    $headers = headers_list();
    if (!array_filter($headers, fn ($line) => stripos($line, 'content-type:') === 0)) {
        $headers[] = 'Content-Type: text/html; charset=UTF-8';
    }
    $status = http_response_code() ?: 200;
    // tenma_respond() would throw here, outside the script: that would end the worker
    if ($status < 100 || $status > 999) {
        error_log("HTTP status $status is not between 100 and 999");
        $status = 500;
    }
    \tenma_respond($rid, [$fatal && $status === 200 ? 500 : $status, $headers, $body]);
}

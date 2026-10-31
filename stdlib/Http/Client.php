<?php

namespace Http;

/**
 * Outgoing HTTP requests, without blocking the server: only the Fiber waits. Network errors and a
 * response that is not complete within $timeout seconds throw; HTTP error codes are not errors, they
 * are in the response's status.
 */
final class Client
{
    public function __construct(public readonly float $timeout = 30.0) {}

    /** @param array<string, string> $headers */
    public function get(string $url, array $headers = []): ClientResponse
    {
        return $this->send('GET', $url, $headers);
    }

    /** @param array<string, string> $headers */
    public function post(string $url, string $body = '', array $headers = []): ClientResponse
    {
        return $this->send('POST', $url, $headers, $body);
    }

    /**
     * Sends a GET and streams the response body into the file at $path, which it creates or replaces;
     * the directory must exist. The body is never held in memory whole, so the file may be far larger
     * than memory_limit. The returned response has status and headers and an empty body. Every status
     * is written, check it. On a network error or a timeout the file is removed and the call throws.
     *
     * @param array<string, string> $headers
     */
    public function download(string $url, string $path, array $headers = []): ClientResponse
    {
        \Tenma\Internal\requireFiber();
        $lines = headerLines($headers);
        $timeout = (int) ceil($this->timeout * 1000);
        $op = \tenma_op_fetch('GET', $url, $lines, '', $timeout, $path);
        [$status, $responseHeaders] = \Tenma\Internal\await($op);
        return new ClientResponse($status, $responseHeaders, '');
    }

    /** @param array<string, string> $headers */
    public function send(string $method, string $url, array $headers = [], string $body = ''): ClientResponse
    {
        \Tenma\Internal\requireFiber();
        $lines = headerLines($headers);
        [$status, $responseHeaders, $responseBody] = \Tenma\Internal\await(\tenma_op_fetch($method, $url, $lines, $body, (int) ceil($this->timeout * 1000)));
        return new ClientResponse($status, $responseHeaders, $responseBody);
    }
}

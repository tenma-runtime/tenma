---
title: Deployment
description: Run Tenma in production with Docker, systemd or Homebrew behind a reverse proxy, with signals, restarts, limits and logs.
order: 410
---

In production Tenma is one process that serves the app with one or more workers. This page covers how to start it, stop it, put it behind a proxy, and what limits apply. Every option is on the [Command line](command-line.md) page.

## Workers

`--workers N` runs several workers, each with its own interpreter and memory. How they share work and state is on the [Process and workers](process.md#workers) page.

## Restarts and signals

- **Ctrl+C or SIGTERM**: Tenma stops accepting connections. Running requests get up to 10 seconds to finish, then Tenma exits with status 0. A second signal ends it at once.
- **A fatal PHP error** (out of memory, stack overflow) answers 500. Only the interpreter of that worker restarts and runs the script again. The other workers keep serving.
- **`--max-requests N`** restarts a worker's interpreter after N requests. It helps with code that leaks memory.
- **A script that fails on a restart** ends Tenma with status 1, since a missing worker would drop connections.

PHP's own signal handlers are disabled, and `pcntl` is not built in. Signals belong to Tenma.

## Limits

| Limit                                        | Default | Set with                | Above it                                       |
| -------------------------------------------- | ------- | ----------------------- | ---------------------------------------------- |
| request body and WebSocket message           | 10 MB   | `--max-body`            | 413                                            |
| headers and body arriving                    | 30 s    | `--request-timeout`     | 408 or disconnect                              |
| a client that stops reading                  | 30 s    | `--request-timeout`     | disconnect; the writer gets `ConnectionClosed` |
| open connections per worker                  | 1024    | `--max-connections`     | 503 at once                                    |
| `Http\Client` response                       | 30 s    | its `timeout` parameter | the call fails                                 |
| connecting to Postgres, Redis, `Http\Client` | 10 s    | fixed                   | the call fails                                 |
| draining on shutdown                         | 10 s    | fixed                   | running requests are cut off                   |

The request timeout protects against slowloris clients. A streaming writer or `WebSocket::send()` that waits for a client which stopped reading gets `ConnectionClosed`.

There is no execution timeout for CPU work. A handler that computes forever (an endless loop) is not interrupted and holds up its worker.

## HTTPS

With a certificate and its key, every port the script serves speaks HTTPS:

```bash
tenma --tls-cert /etc/app/fullchain.pem --tls-key /etc/app/privkey.pem --workers auto index.php
```

- Both files are PEM. The certificate file holds the chain: the server certificate first, then the intermediates.
- `kill -HUP` reads both files again, for a renewed certificate. New connections get it, running ones keep theirs. If the new files are broken, Tenma logs it and keeps the previous certificate.
- A wrong path, an empty file, or a key that does not belong to the certificate stops Tenma at the start with exit code 2.
- TLS 1.2 and 1.3. A client that offers HTTP/2 (every browser does) gets it, others get HTTP/1.1. Over HTTP/2 many requests share one connection and run at the same time, each in its own fiber.
- WebSockets work as `wss://`. They use HTTP/1.1: a browser opens a connection of its own for them.
- `$request->https` is `true`. In classic mode, `$_SERVER['HTTPS']` is `on` and `$_SERVER['REQUEST_SCHEME']` is `https`.
- A client that speaks plain HTTP to such a port gets no answer. Each worker does the handshakes of its own connections, within `--request-timeout`.
- Ports 80 and 443 need root or the capability `CAP_NET_BIND_SERVICE` on Linux.

With `--tls-port`, only the ports named speak HTTPS; the others stay plain HTTP. This serves the app on 443 and sends plain requests on 80 there:

```php
<?php

use Http\{Request, Response};

new Http\Server(80, function (Request $request) {
    return Response::redirect('https://' . $request->host() . $request->uri, 301);
})->start();
new Http\Server(443, fn () => "Served over HTTPS\n")->start();
```

```bash
tenma --tls-cert fullchain.pem --tls-key privkey.pem --tls-port 443 index.php
```

`--tls-port` can be given several times. A port the script does not serve gets a warning at the start.

## Behind a reverse proxy

Tenma speaks HTTP/1.1, and HTTP/2 over TLS. A proxy such as Caddy or nginx in front of it adds HTTP/3, compression and certificates that renew on their own. A Caddyfile:

```caddyfile
example.com {
    reverse_proxy 127.0.0.1:8080
}
```

`$request->remoteAddress` is then the proxy's address. The client's address is in the header the proxy sets (`X-Forwarded-For`). Trust that header only when the request comes from your proxy.

To deploy without dropping requests, start the new release on another port, switch the proxy, then stop the old one.

## Docker

The image `ghcr.io/tenma-runtime/tenma` runs `tenma` as its entrypoint. It runs as the unprivileged user `tenma` (uid 1000), sets `PORT=8080` and stops with SIGTERM:

```bash
docker run -p 8080:8080 -v "$PWD:/app" ghcr.io/tenma-runtime/tenma /app/index.php
```

A production image for an app:

```dockerfile
FROM composer:2 AS vendor
WORKDIR /app
COPY composer.json composer.lock ./
RUN composer install --no-dev --classmap-authoritative --no-scripts

FROM ghcr.io/tenma-runtime/tenma
WORKDIR /app
COPY --from=vendor /app/vendor vendor
COPY . .
CMD ["--workers", "auto", "/app/index.php"]
```

With [build hooks](watch.md), run `tenma build` in a build stage, so the image contains their outputs.

## systemd

`/etc/systemd/system/app.service`:

```ini
[Unit]
Description=app (tenma)
After=network-online.target postgresql.service
Wants=network-online.target

[Service]
User=app
WorkingDirectory=/srv/app/current
Environment=PORT=8080
Environment=APP_ENV=production
Environment=DATABASE_URL=postgres://app@localhost/app
# secrets, readable by root only
EnvironmentFile=/etc/app/secrets.env
ExecStartPre=/usr/bin/tenma index.php db:migrate
ExecStart=/usr/bin/tenma --workers auto -c /etc/app/php.ini index.php
Restart=always
KillSignal=SIGTERM
TimeoutStopSec=15

[Install]
WantedBy=multi-user.target
```

- `systemctl enable --now app` starts it. Logs go to the journal: `journalctl -u app -f`.
- `ExecStartPre` runs a task of the same script before every start, here the migrations.
- With `--tls-cert`, add `ExecReload=/bin/kill -HUP $MAINPID`: `systemctl reload app` then reads a renewed certificate without a restart. Without `--tls-cert`, SIGHUP ends Tenma.
- `TimeoutStopSec` is above Tenma's 10-second drain, so running requests can finish.
- The `.deb` installs the binary as `/usr/bin/tenma`. The tarball installs it into `~/.local/bin`; use that path instead.

## Homebrew

On macOS (Apple Silicon):

```bash
brew install tenma-runtime/tap/tenma
brew upgrade tenma
```

The formula installs the binary and the libraries it links. For a background service on a Mac, a `launchd` agent with `KeepAlive` runs `tenma` like the systemd unit above.

## Logs

- **stderr**: PHP errors and warnings, uncaught exceptions with their stack trace, `error_log()`, and Tenma's own messages (`tenma: shutting down`).
- **stdout**: `echo`, except inside a request in [classic mode](classic-mode.md), where it becomes the response.
- **Responses** never contain errors: `display_errors` is off.

See also: [Command line](command-line.md), [Process and workers](process.md), [Configuration and php.ini](configuration.md), [Watch mode and build hooks](watch.md).

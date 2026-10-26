---
title: Command line
description: Every tenma subcommand and option with its default, the script's $argv, exit codes, and the PHP stubs for IDEs.
order: 300
---

`tenma` runs a PHP script. The script may start servers with `Http\Server` or simply run and exit. Subcommands build assets, run the language server, write reference pages and print stubs.

```text
tenma [options] <script.php> [arguments …]
tenma [options] build [tenma.json]
tenma install|update|remove composer
tenma composer [arguments …]
tenma lsp
tenma docs [<path>...] [--out <dir>]
tenma --stubs [DIR]
tenma --version
```

| Command                                                  | Does                                                                                                                     |
| -------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------ |
| `tenma script.php`                                       | runs the script; it serves if it starts an `Http\Server`                                                                 |
| `tenma build [tenma.json]`                               | runs the [build hooks](watch.md#tenma-build) once, `./tenma.json` by default                                             |
| `tenma check [script.php\|tenma.json]`                   | checks the options, hooks and plugins of [tenma.json](configuration.md#tenmajson) without starting                       |
| `tenma install composer`                                 | fetches the latest stable [Composer](#composer-and-other-php-tools)                                                      |
| `tenma update composer`                                  | fetches the latest stable Composer again                                                                                 |
| `tenma remove composer`                                  | deletes the installed Composer                                                                                           |
| `tenma composer [arguments …]`                           | runs the installed Composer                                                                                              |
| `tenma lsp`                                              | the [language server](language-server.md), on stdin and stdout                                                           |
| `tenma docs [paths] [--out <dir>]`                       | writes [reference pages](reference-docs.md), into `docs/reference` by default                                            |
| `tenma --stubs [DIR]`                                    | prints Tenma's PHP API as stubs, or writes one file per class to DIR (see [Stubs for IDEs](#stubs-for-ides-and-phpstan)) |
| `tenma replay FILE [--until N \| --request N] [--check]` | plays a journal from `--record` into a fresh interpreter ([Record and replay](replay.md))                                |
| `tenma replay FILE --list`                               | the requests of a journal, numbered for `--request`                                                                      |
| `tenma --version`, `tenma -v`                            | prints the Tenma and PHP versions, such as `tenma 0.2.10 (PHP 8.5.12)`                                                   |

`install`, `update`, `remove`, `composer`, `lsp`, `docs`, `--stubs` and `--version` only count as the first argument. `build` comes after the options, so `tenma -d memory_limit=1G build` works.

## Options

Options come before the script. Each takes its value as the next argument.

| Option                         | Default                         | Does                                                                                               |
| ------------------------------ | ------------------------------- | -------------------------------------------------------------------------------------------------- |
| `--workers N`, `-w N`          | `1`                             | worker threads, each with its own PHP interpreter and state; `auto` starts one per CPU core        |
| `--max-body SIZE`              | `10m`                           | largest request body and WebSocket message; bytes, or with `k`, `m`, `g`; 413 above it             |
| `--max-requests N`             | none                            | a worker restarts its interpreter after N requests                                                 |
| `--max-connections N`          | `1024`                          | open connections per worker; more get a 503 at once                                                |
| `--request-timeout SECONDS`    | `30`                            | time for headers and body to arrive (408 or disconnect), and for a stalled write to a client       |
| `--db-pool N`                  | 20 / workers, 2–10              | Postgres connections per worker and URL                                                            |
| `--pubsub redis://HOST`        | none                            | messages of `Messaging\Channel` go through Redis and reach every process                           |
| `--pdo-pgsql native`           | `tenma`                         | PDO keeps ext/pdo_pgsql and libpq for `pgsql:` DSNs, see [Plain PHP under tenma](compatibility.md) |
| `--durable postgres://URL`     | none                            | the database of [durable functions](durable.md)                                                    |
| `--plugin FILE`                | none                            | loads a [native plugin](plugins.md); any number of times                                           |
| `--config FILE`                | `tenma.json` next to the script | the options as a file, see [tenma.json](configuration.md#tenmajson)                                |
| `--stop-timeout SECONDS`       | `10`                            | how long the handlers of `Process\Shutdown` may take when tenma stops                              |
| `--tls-cert FILE`              | none                            | serves HTTPS with this certificate chain (PEM); needs `--tls-key` ([HTTPS](deployment.md#https))   |
| `--tls-key FILE`               | none                            | the private key of the certificate (PEM)                                                           |
| `--tls-port N`                 | every port                      | HTTPS only on this port, plain HTTP on the others; any number of times                             |
| `--server-name NAME`           | `localhost`                     | `$_SERVER['SERVER_NAME']` in [classic mode](classic-mode.md)                                       |
| `-d name=value`                |                                 | a PHP setting, also written `-dname=value`; any number of times                                    |
| `-c FILE\|DIR`                 |                                 | the `php.ini` to read, or the directory that holds it                                              |
| `-r CODE`                      |                                 | runs CODE (without `<?php`) instead of a script; the arguments after it go to `$argv`              |
| `--watch`                      |                                 | reloads on changes and runs the hooks of `tenma.json` ([Watch mode](watch.md))                     |
| `--inspect[=[HOST:]PORT]`      | `127.0.0.1:9229`                | starts the [debugger](debugger.md)                                                                 |
| `--inspect-wait[=[HOST:]PORT]` | `127.0.0.1:9229`                | the debugger; the script waits until the editor has set its breakpoints                            |
| `--inspect-brk[=[HOST:]PORT]`  | `127.0.0.1:9229`                | the debugger; the script waits, then stops at its first line                                       |
| `--record DIR`                 | none                            | every worker writes its journal there ([Record and replay](replay.md))                             |
| `--record-on error`            | always                          | keeps a journal only after a 5xx or a fatal error                                                  |
| `--record-max-size SIZE`       | `50m`                           | cuts a worker's journal and goes on in the next file                                               |
| `--record-max-body SIZE`       | `1m`                            | cuts request bodies and op results above this in the journal                                       |
| `--record-redact HEADER,...`   | none                            | replaces these request headers in the journal                                                      |

Numbers must be above 0. `--request-timeout` also takes fractions such as `0.5`. With `--inspect` and more than one worker, Tenma prints `tenma: --inspect runs one worker` and runs one. [Configuration and php.ini](configuration.md) explains `-d` and `-c`.

## Arguments for the script

Everything after the script belongs to the script. Tenma does not read it, even when it starts with `-`. The script finds it in `$argv`, with its own path first:

```php
<?php

var_dump($argv);
```

```bash
tenma args.php migrate --force
```

```text
array(3) {
  [0]=>
  string(8) "args.php"
  [1]=>
  string(7) "migrate"
  [2]=>
  string(7) "--force"
}
```

One script can serve and run tasks this way: `tenma index.php db:migrate` runs a task, `tenma index.php` serves. See [Scripts without a server](process.md#scripts-without-a-server).

## Composer and other PHP tools

Tenma runs Composer, so a project needs no PHP of its own. Install it once:

```bash
tenma install composer
tenma composer install
tenma composer require psr/log
```

`tenma install composer` fetches the latest stable `composer.phar` from getcomposer.org, checks it against the SHA-256 checksum published there and keeps it in `~/.local/share/tenma` (or `$XDG_DATA_HOME/tenma`). `tenma update composer` fetches it again, `tenma remove composer` deletes it.

Other PHP tools run as they come: Tenma skips the `#!` line of a script and has the phar extension, so `tenma vendor/bin/phpstan` or `tenma tool.phar` work.

Tenma sets `PHP_BINARY` to its own path unless it is already set. Composer runs `@php` scripts through it, and so does Symfony Process, so `@php -r "…"` and `@php bin/console` run with Tenma too.

## The port

A server listens on every interface (`0.0.0.0`) on the port given to `Http\Server`. No option sets it. Read it from the environment so the same script runs everywhere:

```php
<?php

$port = (int) (getenv('PORT') ?: 8080);

new Http\Server($port, fn () => "ok\n")->start();
```

## Exit codes

| Status | When                                                                                                  |
| ------ | ----------------------------------------------------------------------------------------------------- |
| `0`    | the script ended, or a server stopped on Ctrl+C or SIGTERM                                            |
| `N`    | `exit(N)` outside a request: in the script, a timer or background work; it also ends a running server |
| `255`  | a fatal error or an uncaught exception while the script starts                                        |
| `1`    | Tenma itself failed: the script does not exist, a build hook failed, a worker could not restart       |
| `2`    | the command line is wrong: an unknown option, a missing value, no script; Tenma prints the usage      |

`exit()` inside a request ends only that request.

## Stubs for IDEs and PHPStan

Tenma's classes exist only inside the binary, so an IDE or PHPStan does not know them. `tenma --stubs` prints them as a PHP file with every class, method and docblock. With a directory, it writes one file per class instead, in folders by namespace (`Http/Request.php`, `Threading/Task.php`):

```bash
tenma --stubs > tenma.php
tenma --stubs stubs/
```

The single file needs a namespace block per namespace. Some editors handle one class per file better.

The same stubs come as the Composer package `tenma/stubs`, published for every release:

```bash
composer require --dev tenma/stubs
```

The stubs are never autoloaded. They only exist for tools that read the code.

See also: [Deployment](deployment.md), [Configuration and php.ini](configuration.md), [Watch mode](watch.md), [Debugger](debugger.md).

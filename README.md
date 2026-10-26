# tenma

[tenma.dev](https://tenma.dev) · [Documentation](docs/index.md) · [Reference](docs/reference/index.md)

PHP 8.5 as a long-running process. tenma runs your script once and keeps it in memory. Every request
runs in its own fiber, so waiting for a database or an HTTP call never blocks the others.

```php
new Http\Server(8080, fn (Http\Request $request) => "Sonnenkorn is open\n")->start();
```

```
tenma index.php
```

The [guides](docs/guides/index.md) explain each feature with scripts that run as they are. How tenma
is built, measurements and known limits are in [ARCHITECTURE.md](ARCHITECTURE.md).

## Install

```
curl -fsSL https://raw.githubusercontent.com/tenma-runtime/tenma/main/packaging/install.sh | sh
```

The script picks the way that fits your system:

| System                 | By hand                                                                                      |
| ---------------------- | -------------------------------------------------------------------------------------------- |
| macOS on Apple Silicon | `brew install tenma-runtime/tap/tenma`                                                       |
| Debian 13              | `apt install ./tenma_<version>_amd64.deb` from the release                                   |
| Ubuntu 24.04           | `apt install ./tenma_<version>_ubuntu24.04_amd64.deb` from the release                       |
| Enterprise Linux 10    | `dnf install epel-release`, then `dnf install ./tenma-<version>-1.el10.x86_64.rpm`           |
| other Linux            | `tenma-linux-x64.tar.gz` from the [release](https://github.com/tenma-runtime/tenma/releases) |
| Docker                 | `docker run -p 8080:8080 -v "$PWD:/app" ghcr.io/tenma-runtime/tenma /app/index.php`          |

The script installs the latest release. On Linux it can install an older one instead:

```
curl -fsSL https://raw.githubusercontent.com/tenma-runtime/tenma/main/packaging/install.sh | TENMA_VERSION=0.1.17 sh
```

Homebrew on macOS always installs the latest. `tenma --version` prints the tenma and PHP versions.

For your editor and PHPStan, add the stubs as a dev dependency:

```
composer require --dev tenma/stubs
```

## Command line

```
tenma [options] <script.php> [arguments …]
tenma build [tenma.json]             # run the build hooks once
tenma lsp                            # language server for editors
tenma docs [<path>...] [--out <dir>] # Markdown reference pages
tenma --stubs [DIR]                  # the PHP API as stubs
tenma replay FILE [--check]          # play a journal from --record again
tenma replay FILE --list             # its requests, for --request N
tenma --version
```

Everything after the script goes to the script's `$argv`.

| Option                      | Default                         |                                                                |
| --------------------------- | ------------------------------- | -------------------------------------------------------------- |
| `--workers N`, `-w N`       | 1                               | worker threads; `auto` = one per CPU core                      |
| `--max-body SIZE`           | `10m`                           | largest request body and WebSocket message                     |
| `--max-requests N`          | none                            | a worker restarts its interpreter after N requests             |
| `--max-connections N`       | 1024                            | open connections per worker; more get a 503                    |
| `--request-timeout SECONDS` | 30                              | time for headers and body, and for a stalled write             |
| `--db-pool N`               | 20 / workers, 2–10              | Postgres connections per worker and URL                        |
| `--pubsub redis://HOST`     | none                            | `Messaging\Channel::publish()` through Redis, to every process |
| `--pdo-pgsql native`        | `tenma`                         | PDO keeps libpq for Postgres instead of tenma's driver         |
| `--record DIR`              | none                            | every worker writes its journal there, for `tenma replay`      |
| `--durable postgres://URL`  | none                            | the database of durable functions (`Threading\Durable`)        |
| `--plugin FILE`             | none                            | loads a native plugin (`Tenma\Plugin`); any number of times    |
| `--config FILE`             | `tenma.json` next to the script | the options as JSON                                            |
| `--stop-timeout SECONDS`    | `10`                            | how long stop handlers may take when tenma stops               |
| `--tls-cert FILE`           | none                            | HTTPS with this certificate chain (PEM)                        |
| `--tls-key FILE`            | none                            | the private key for `--tls-cert` (PEM)                         |
| `--tls-port N`              | every port                      | HTTPS only on this port; any number of times                   |
| `--server-name NAME`        | `localhost`                     | `$_SERVER['SERVER_NAME']` in classic mode                      |
| `-d name=value`             |                                 | a PHP setting, as with `php -d`                                |
| `-c FILE\|DIR`              |                                 | a `php.ini`, as with `php -c`                                  |
| `--watch`                   |                                 | reload on changes, run the hooks of `tenma.json`               |
| `--inspect[=[HOST:]PORT]`   | `127.0.0.1:9229`                | the debugger; also `--inspect-wait` and `--inspect-brk`        |

More in [Command line and deployment](docs/guides/deployment.md).

## Development

Build and test:

```
brew install pkgconf openssl@3 libpq icu4c libsodium libzip gmp oniguruma libpng jpeg-turbo webp freetype
php-build/build.sh                     # once, about 5 minutes
cargo build --release                  # binary: target/release/tenma
cargo test --release
```

Run the demo:

```
(cd demo/io && composer install)
./target/release/tenma demo/io/index.php
```

Format before a commit. CI checks all of it; long PHP lines are only warnings for now.

```
cargo fmt                              # Rust
prettier --write .                     # Markdown, JSON, YAML
php-cs-fixer fix                       # PHP, PER-CS 2.0
phpcs                                  # PHP lines longer than 100 characters
```

Install the formatters with `npm install -g prettier` and
`composer global require friendsofphp/php-cs-fixer squizlabs/php_codesniffer`. Composer puts the PHP
tools in `~/.composer/vendor/bin`.

On Linux, build and test in Docker:

```
docker build -t tenma .
docker run -p 8080:8080 -v "$PWD:/app" tenma /app/index.php
docker run --rm -e USER=root $(docker build -q --target build .) cargo test --release
```

## License

Tenma is licensed under either of [MIT](LICENSE-MIT) or [Apache-2.0](LICENSE-APACHE), at your option.
Binaries embed PHP (PHP License 3.01), see [NOTICE](NOTICE).

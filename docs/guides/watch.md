---
title: Watch mode and build hooks
description: Reload the app on every change with --watch, run the build steps of tenma.json, and run them once with tenma build.
order: 310
---

`tenma --watch` reloads the app whenever a file it loaded changes. Build steps such as a stylesheet or a cache are the `hooks` of the [tenma.json](configuration.md) next to the script; watch mode runs them on every change, and `tenma build` runs them once.

```bash
tenma --watch app.php    # serve and reload on changes
tenma build              # run the hooks of ./tenma.json once
```

| Feature                                                                           | Does                                                                     |
| --------------------------------------------------------------------------------- | ------------------------------------------------------------------------ |
| `--watch`                                                                         | reloads the workers when a loaded file changes                           |
| [`Development\Watch::reloadedBy()`](../reference/Development/Watch.md#reloadedby) | the file whose change caused the current run, or `null` on the first run |
| [`Development\Watch::written()`](../reference/Development/Watch.md#written)       | a file the script wrote itself, whose change reloads nothing             |
| `hooks` in `tenma.json`                                                           | build steps                                                              |
| `tenma build [tenma.json]`                                                        | runs every hook once, for CI and Docker                                  |
| [`Development\Problems::report()`](diagnostics.md)                                | under `--watch`, problems the app reports appear in the editor           |

`--watch` is for development. In production, deploy and restart instead.

## Reloading

Tenma watches every file the script loaded. That includes classes the autoloader loads later and templates read with `include`. When one of them changes, the workers' interpreters restart and run the script again.

A change means different content, not just a new timestamp. Saving without changes does not reload, and neither does switching git branches back and forth.

`Development\Watch::reloadedBy()` returns the file whose change caused the current run. A framework can use it to rebuild only what that file affects:

```php
<?php

use Development\Watch;

$changed = Watch::reloadedBy();

if ($changed !== null) {
    error_log("reloaded because $changed changed");
}

new Http\Server(8080, fn () => "ok\n")->start();
```

### Files the script writes

A framework that compiles templates into PHP files and loads them with `include` changes a loaded file itself. Without more, that reloads the workers and cuts off the requests running at the time. `Development\Watch::written()` after the write tells `--watch` that this change needs no reload:

```php
<?php

use Development\Watch;

file_put_contents($compiled, $php);
Watch::written($compiled);

include $compiled;
```

Without `--watch` the call does nothing.

## Build hooks

The `hooks` of the `tenma.json` next to the script are a list of build steps. Only `--watch` and `tenma build` run them.

| Key       | Does                                                                               |
| --------- | ---------------------------------------------------------------------------------- |
| `inputs`  | globs relative to `tenma.json`: `*` and `?` within a name, `**` across directories |
| `run`     | a command: the program and its arguments, as a list of strings                     |
| `php`     | instead of `run`: a PHP file that returns a callable, which gets the changed paths |
| `outputs` | the files the hook writes; they never trigger it again                             |
| `name`    | the prefix of its log lines; default: the file name of the program or PHP file     |

A command hook runs the program as its own process, without a shell, in the directory of `tenma.json`. This one rebuilds a stylesheet from every template that uses its classes:

```json
{
  "hooks": [
    {
      "name": "css",
      "inputs": ["assets/app.css", "views/**/*.php", "src/**/*.php"],
      "outputs": ["public/app.css"],
      "run": [
        "tailwindcss",
        "-i",
        "assets/app.css",
        "-o",
        "public/app.css",
        "--minify"
      ]
    }
  ]
}
```

Any program works the same way, such as a script of the project that writes a cache:

```json
{
  "hooks": [
    {
      "name": "routes",
      "inputs": ["src/**/*.php"],
      "outputs": ["var/routes.php"],
      "run": ["php", "bin/routes.php"]
    }
  ]
}
```

A PHP hook gets the changed paths, an empty list on the first run. It may use Tenma's async functions and [`Development\Problems::report()`](diagnostics.md). This one writes a list of icons for the app to `include`:

```json
{
  "hooks": [
    {
      "name": "icons",
      "inputs": ["public/icons/*.svg"],
      "outputs": ["var/icons.php"],
      "php": "bin/icons.php"
    }
  ]
}
```

```php
<?php
// bin/icons.php

use FileSystem\FileInfo;

return function (array $changed): void {
    $icons = [];
    foreach (glob(dirname(__DIR__) . '/public/icons/*.svg') as $file) {
        $icons[basename($file, '.svg')] = '/icons/' . basename($file);
    }

    $code = '<?php return ' . var_export($icons, true) . ';';
    new FileInfo(dirname(__DIR__) . '/var/icons.php')->write($code);
};
```

A glob like `**/*.php` never looks into `.git`, `node_modules` or `vendor`.

## tenma build

```bash
tenma build                    # the hooks of ./tenma.json
tenma build path/tenma.json
```

`tenma build` runs every hook once, in dependency order, then exits. Use it in CI and in a Dockerfile, so the production image contains the built files without the hooks ever running there:

```dockerfile
RUN tenma build
```

## Problems in the editor

Under `--watch`, problems the app reports with `Development\Problems::report()` appear in the editor next to the code. See [Diagnostics](diagnostics.md).

## Behaviour

During a reload:

- The port stays open. Requests that arrive wait and get answered by the new code.
- Running requests get 1 second to finish (10 at shutdown). Open streams and WebSockets close after that.
- In-memory state starts over, since the script runs again.
- A script that fails to load answers 503 until a change fixes it.
- A script whose ports changed restarts Tenma as a whole. A script without a server simply runs again.
- `opcache.revalidate_freq` defaults to 0, so opcache never serves the old code.

Hooks:

- Each hook runs once at startup, before the first request. It runs again whenever a file matching its `inputs` changes.
- The script starts once the first run of the hooks is over, at the latest after 10 seconds. A command with its own watch mode, which never ends, therefore does not block it.
- Tenma orders hooks by their files. A hook that reads another one's `outputs` runs after it, and only if that output really changed. A cycle is rejected at startup.
- A hook that writes files into its own inputs without declaring them as `outputs` is stopped after five runs in a row.
- A failing hook (an exception, or an exit status other than 0) skips the hooks after it. The server keeps running.
- A command's processes, including those it started itself, get SIGTERM when the hook ends or reloads, and SIGKILL 2 seconds later.
- Changing `tenma.json`, or a file a PHP hook loaded, reloads the hooks.
- `exit()` in a PHP hook ends Tenma.
- `tenma build` exits with status 1 if a hook fails, the list is invalid or the hooks form a cycle. [`tenma check`](configuration.md) checks the list without running it.

See also: [Configuration](configuration.md), [`Development\Watch::reloadedBy()`](../reference/Development/Watch.md#reloadedby), [Command line](command-line.md), [Debugger](debugger.md).

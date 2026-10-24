---
title: Debugger
description: Debug a running app with --inspect over the Debug Adapter Protocol, from VS Code, Neovim, Zed, Helix or Emacs.
order: 320
---

`tenma --inspect` starts a debugger inside Tenma that speaks the Debug Adapter Protocol (DAP). An editor connects to it to set breakpoints, step, look at variables and evaluate PHP in a stopped request. Every request, timer and task shows up as a thread of its own.

```bash
tenma --inspect index.php          # debugger on 127.0.0.1:9229, the app starts at once
tenma --inspect-wait index.php     # waits until the editor has set its breakpoints
tenma --inspect-brk index.php      # waits, then stops at the script's first line
tenma --inspect=9230 index.php     # another port
tenma --inspect=0.0.0.0:9229 index.php  # another address
tenma --inspect --watch index.php  # keeps the editor connected across reloads
```

| Feature                                                                       | Does                                                                 |
| ----------------------------------------------------------------------------- | -------------------------------------------------------------------- |
| [Breakpoints](#breakpoints)                                                   | on lines, with conditions, hit counts and logpoints                  |
| [`Development\Debugger::break()`](../reference/Development/Debugger.md#break) | stops at that line when an editor is connected                       |
| [Exceptions](#exceptions)                                                     | stops where an uncaught (or any) exception is thrown                 |
| [Stepping](#stepping-and-threads)                                             | continue, step over, into, out, within one request                   |
| [Threads](#stepping-and-threads)                                              | every request, timer and `Threading\Task::run()` task                |
| [Variables](#variables-and-evaluation)                                        | every frame, expandable and editable                                 |
| [Evaluate](#variables-and-evaluation)                                         | watch expressions, hovers and the debug console run PHP in the frame |

## VS Code and VSCodium

Install the [Tenma-VSCode](https://github.com/tenma-runtime/vscode) extension. It starts the script on F5, or attaches to a running `--inspect`. It also starts the [language server](language-server.md). A `.vscode/launch.json`:

```json
{
  "version": "0.2.0",
  "configurations": [
    {
      "type": "tenma",
      "request": "launch",
      "name": "Tenma",
      "program": "${workspaceFolder}/index.php",
      "watch": true
    },
    {
      "type": "tenma",
      "request": "attach",
      "name": "Attach to tenma --inspect",
      "port": 9229
    }
  ]
}
```

| Request  | Attributes                                                                                      |
| -------- | ----------------------------------------------------------------------------------------------- |
| `launch` | `program`, `args`, `cwd`, `env`, `watch`, `stopOnEntry`, `runtimeExecutable` (default: `tenma`) |
| `attach` | `host`, `port`, `stopOnEntry`                                                                   |

## Other editors

Any DAP client connects to the TCP port as a server adapter. Start `tenma --inspect-wait index.php` in a terminal, then attach.

**Neovim** with [nvim-dap](https://github.com/mfussenegger/nvim-dap):

```lua
local dap = require('dap')
dap.adapters.tenma = { type = 'server', host = '127.0.0.1', port = 9229 }
dap.configurations.php = {
  { type = 'tenma', request = 'attach', name = 'Attach to tenma --inspect' },
}
```

**Zed, Helix, Emacs (dape)** and other DAP clients: configure a debug adapter that connects over TCP to `127.0.0.1:9229`. There is no adapter process to start. Send an `attach` request. Tenma treats `launch` and `attach` alike: both connect to the running app.

## Breakpoints

- A line breakpoint may have a condition (`$id === 42`), a hit count (`5`, `>= 5`, `% 3`), or be a logpoint (`user {$user->id} on {$request->path}`).
- A breakpoint on a line without its own statement moves to the next statement. Examples: a method's head, the middle of a multi-line `return [`.
- A breakpoint on a function's closing brace stops whenever it returns. The return value is among the variables.
- `Development\Debugger::break()` in the code stops there when an editor is connected. It does nothing otherwise.

## Exceptions

- An exception the app does not catch stops where it is thrown, with its frames still alive.
- The filter "All exceptions" also stops at caught ones.
- A condition on a filter limits it to classes, such as `DomainException, App\NotFound`. Subclasses count too.
- `Threading\Cancelled`, `exit()` and a reload that ends waiting requests never stop.

## Stepping and threads

Continue, step over, step into and step out work as usual. A step stays in its request. Stepping over `Threading\Task::delay()` or a query lets other requests run meanwhile, then stops at the next line of the same request.

Every request, timer and `Threading\Task::run()` task is a thread, named after its method and URI. While stopped, the editor lists them and shows where each waiting one waits.

In a replay (`tenma --inspect-wait replay FILE`) the editor also offers step back and reverse continue: tenma replays the journal again up to the statement before, see [Record and replay](replay.md).

## Variables and evaluation

- The variables of every frame show, with arrays and objects expandable.
- Variables are editable. A shared array is copied first, as PHP would do. Property types and `readonly` apply.
- Watch expressions, hovers and the debug console evaluate PHP in the selected frame, with its variables and `$this`. The console also runs statements.
- `echo` and the error log appear in the debug console.

## Behaviour

- The debugger listens on 127.0.0.1 by default, since it can run any code in the app. An address reachable from the network prints a warning.
- `--inspect` runs one worker and disables opcache. The JIT would skip the statements the debugger stops at.
- A stopped request stops its worker, with its other requests and timers.
- Evaluated code cannot wait: `Threading\Task::delay()` throws a `FiberError` there.
- A warning or notice during evaluation is its error, such as `Warning: Undefined property: App\User::$total`.
- Tenma's own standard library is never stepped into.
- With `--watch`, breakpoints keep applying across reloads. A change saved while stopped reloads after you continue.
- The editor's `stopOnEntry` overrides `--inspect-wait` and `--inspect-brk`.

See also: [`Development\Debugger::break()`](../reference/Development/Debugger.md#break), [Language server](language-server.md), [Command line](command-line.md).

<?php

// The standard library of Tenma, embedded in the binary and loaded before the script.
// It builds on the raw functions of the C layer (tenma_listen, tenma_respond, tenma_op_*)
// and gives them a PHP API in the namespaces Http, Threading, FileSystem, Messaging, Data,
// Development, Lsp, Process and Security; Tenma\Internal is the machinery behind them.
//
// Asynchronous without an await keyword: every request runs in its own Fiber.
// When the code starts an op (sleep, fetch), the Fiber pauses, the event loop serves other
// requests, and once the op is done, execution continues at the same spot. To the code this
// looks synchronous.

namespace Tenma\Internal;

/** The namespaces of the public API: what `tenma --stubs` and `tenma docs` cover. */
const NAMESPACES = ['Http', 'Threading', 'FileSystem', 'Messaging', 'Data', 'Development', 'Lsp', 'Process', 'Security'];

/** Whether $name is a class of the public API: the namespaces above, and Tenma\Plugin. */
function isPublic(string $name): bool
{
    if ($name === 'Tenma\\Plugin') {
        return true;
    }
    foreach (NAMESPACES as $namespace) {
        if (str_starts_with($name, $namespace . '\\')) {
            return true;
        }
    }
    return false;
}

/** The task whose Fiber is running code right now; null in the script or in a custom Fiber. */
function currentTask(): ?\Threading\Task
{
    $fiber = \Fiber::getCurrent();
    return $fiber === null ? null : Scheduler::$taskOfFiber[spl_object_id($fiber)] ?? null;
}

/** @return array<int, mixed> The Threading\AsyncLocal values in effect right now: the ones a new task inherits. */
function context(): array
{
    return currentTask()?->context ?? Scheduler::$rootContext;
}

/** The running Fiber; asynchronous functions only work inside one. */
function requireFiber(): \Fiber
{
    return \Fiber::getCurrent() ?? throw new \LogicException('Asynchronous functions only work in an Http\\Server handler, in tasks and in timers');
}

/** A new token for suspend() and wake(). */
function token(string $kind): string
{
    return $kind . ':' . ++Scheduler::$tokens;
}

/**
 * Pauses the running Fiber until it is woken with $token; a woken error is thrown.
 * If a cancellation is pending for the Fiber's task, it is thrown here instead of waiting ($onCancel
 * then cleans up, for example an op that has already started).
 */
function suspend(string $token, ?\Closure $onCancel = null): mixed
{
    $fiber = requireFiber();
    $id = spl_object_id($fiber);
    $cancelled = (Scheduler::$taskOfFiber[$id] ?? null)?->takeCancellation();
    if ($cancelled !== null) {
        $onCancel && $onCancel();
        throw $cancelled;
    }
    Scheduler::$waiting[$id] = [$token, Current::$request, Current::$durable];
    try {
        $value = \Fiber::suspend();
    } finally {
        unset(Scheduler::$waiting[$id]);
    }
    return $value instanceof \Throwable ? throw $value : $value;
}

/** Queues $fiber for resuming; drain() resumes it if it is still waiting for $token by then. */
function wake(\Fiber $fiber, string $token, mixed $value = null): void
{
    Scheduler::$ready[] = [$fiber, $token, $value];
}

/**
 * Resumes woken Fibers until none is left waiting. Tenma calls this after every call into PHP
 * when no Fiber is running; the state lives in Scheduler so that an exit() midway loses nothing.
 */
function drain(): void
{
    // Not unset(): count() would shrink and skip Fibers queued later
    while (Scheduler::$head < count(Scheduler::$ready)) {
        [$fiber, $token, $value] = Scheduler::$ready[Scheduler::$head];
        Scheduler::$ready[Scheduler::$head] = null;
        Scheduler::$head++;
        [$waitingFor, $request, $durable] = Scheduler::$waiting[spl_object_id($fiber)] ?? [null, null, null];
        if ($waitingFor === $token && $fiber->isSuspended()) {
            Current::$request = $request;
            enterDurable($durable);
            $fiber->resume($value);
            enterDurable(null);
        }
    }
    Scheduler::$ready = [];
    Scheduler::$head = 0;
}

/**
 * Interrupts whatever $fiber is waiting for and wakes it with $error: an op is cancelled, waiting
 * for a task or an all() ends.
 */
function interrupt(\Fiber $fiber, \Throwable $error): bool
{
    $id = spl_object_id($fiber);
    [$token, $request, $durable] = Scheduler::$waiting[$id] ?? [null, null, null];
    if ($token === null) {
        return false;
    }
    if (str_starts_with($token, 'op:')) {
        $op = (int) substr($token, 3);
        unset(Waiting::$ops[$op]);
        \tenma_op_cancel($op);
    }
    // A fresh token: a wake already queued with the old one (for example the op's result)
    // is now stale and does not overtake the cancellation
    $fresh = token('interrupt');
    Scheduler::$waiting[$id] = [$fresh, $request, $durable];
    wake($fiber, $fresh, $error);
    return true;
}

/**
 * Ends everything that belongs to a request: its tasks throw $error wherever they are waiting,
 * once, even if they were cancelled before. What its remaining work starts afterwards ends right away.
 */
function cancelRequest(int $request, \Throwable $error, ?\Threading\Task $except = null): void
{
    if (empty(Scheduler::$ofRequest[$request])) {
        return;
    }
    Scheduler::$ended[$request] ??= $error;
    foreach (Scheduler::$ofRequest[$request] as $entry) {
        if ($entry !== $except) {
            $entry instanceof \Threading\Task ? $entry->endWithRequest($error) : $entry->cancel();
        }
    }
}

/** Registers a task, timer or subscription of a request; if the request has ended, it ends right away. */
function own(int $request, \Threading\Task|\Threading\Timer|\Messaging\Subscription $entry): void
{
    Scheduler::$ofRequest[$request][spl_object_id($entry)] = $entry;
    if (isset(Scheduler::$ended[$request])) {
        $entry instanceof \Threading\Task ? $entry->endWithRequest(Scheduler::$ended[$request]) : $entry->cancel();
    }
}

function disown(int $request, \Threading\Task|\Threading\Timer|\Messaging\Subscription $entry): void
{
    unset(Scheduler::$ofRequest[$request][spl_object_id($entry)]);
    if (empty(Scheduler::$ofRequest[$request])) {
        unset(Scheduler::$ofRequest[$request], Scheduler::$ended[$request]);
    }
}

/**
 * The response is out, or a stream or WebSocket is over: the request's tasks, timers and
 * subscriptions end with it, as they do when a client leaves before the response.
 */
function connectionEnded(int $request): void
{
    cancelRequest($request, new \Threading\Cancelled('The connection ended'), currentTask());
}

/**
 * Called by Tenma when the client left before the response. Unlike after exit(), catch and
 * finally still run and may wait: transactions roll themselves back there. Work that has to keep
 * running is started by the handler with Task::detach().
 */
function requestGone(int $rid): void
{
    cancelRequest($rid, new \Threading\Cancelled('The client closed the connection'));
}

/** Waits in the running Fiber for an op and returns its result. */
function await(int $op): mixed
{
    $fiber = requireFiber();
    if ($op === 0) {
        throw new \RuntimeException('Tenma cannot start asynchronous calls right now');
    }
    Waiting::$ops[$op] = $fiber;
    [$ok, $value] = suspend("op:$op", function () use ($op): void {
        unset(Waiting::$ops[$op]);
        \tenma_op_cancel($op);
    });
    // Errors arrive as [message, code]; for files the code is errno, for example
    return $ok ? $value : throw new \RuntimeException(...(array) $value);
}

/** Called by Tenma when an op is done: wakes the waiting Fiber or fires the timer. */
function complete(int $op, bool $ok, mixed $value): void
{
    $timer = Waiting::$timers[$op] ?? null;
    if ($timer !== null) {
        unset(Waiting::$timers[$op]);
        Current::$request = null;
        $timer->fire();
        return;
    }
    $subscription = Waiting::$subscriptions[$op] ?? null;
    if ($subscription !== null) {
        unset(Waiting::$subscriptions[$op]);
        Current::$request = null;
        $subscription->fire($ok, $value);
        return;
    }
    if (isset(Waiting::$ops[$op])) {
        $fiber = Waiting::$ops[$op];
        unset(Waiting::$ops[$op]);
        wake($fiber, "op:$op", [$ok, $value]);
    }
}

/** Runs $start once the event loop is running; if it already is, right away. */
function whenRunning(\Closure $start): void
{
    if (Loop::$running) {
        $start();
    } else {
        Loop::$pending[] = $start;
    }
}

/** Called by Tenma when the event loop is running: starts timers and tasks from the script's startup. */
function started(): void
{
    Loop::$running = true;
    while (Loop::$pending) {
        $start = array_shift(Loop::$pending);
        $start();
    }
}

/**
 * Called by Tenma after exit() or die() in a Fiber. Inside a request only the request ends:
 * it gets 500, its streams, sockets and transactions are closed, its remaining tasks are
 * cancelled (exit() skips finally blocks). Outside a request (timer, background,
 * script) Tenma exits, like PHP.
 */
function exited(): void
{
    $rid = Current::$request;
    if ($rid === null) {
        \tenma_exit_process();
        return;
    }
    error_log('exit() in an Http\\Server handler only ends the request');
    \tenma_respond($rid, [500, ['Content-Type: text/plain; charset=utf-8'], "Internal Server Error\n"]);
    \tenma_stream_end($rid, false);
    \tenma_ws_close($rid, false);
    \tenma_db_abandon($rid);
    cancelRequest($rid, new \Threading\Cancelled('exit() ended the request'));
    Current::$request = null;
}

/**
 * @internal Behind FileSystem\FileInfo and DirectoryInfo: one file system op on a background thread.
 * $second is the destination for rename and copy; $flag means recursive for mkdir and removeDir.
 */
function fs(string $op, string $path, string $second = '', bool $flag = false, string $data = ''): mixed
{
    requireFiber();
    $result = await(\tenma_op_fs($op, $path, $second, $flag, $data));
    return match ($op) {
        'read' => $result,
        // Kind and name as bytes, entries separated by null bytes: file names need not be valid UTF-8
        'readDir' => $result === '' ? [] : explode("\0", $result),
        'exists', 'stat', 'copy' => json_decode($result, true, flags: JSON_THROW_ON_ERROR),
        default => null,
    };
}

/**
 * Called by tenma when it stops (Ctrl+C, SIGTERM), before the plugins shut down and the interpreter
 * ends: every handler of Process\Shutdown::register() runs in a task of its own; tenma waits for them
 * up to --stop-timeout.
 */
function stopping(): void
{
    $handlers = Shutdown::$handlers;
    Shutdown::$handlers = [];
    foreach ($handlers as $handler) {
        \tenma_stop_handlers(1);
        \Threading\Task::start(static function () use ($handler): void {
            try {
                $handler();
            } catch (\Throwable $error) {
                error_log('Error in a stop handler: ' . $error);
            } finally {
                \tenma_stop_handlers(-1);
            }
        }, null, null, 'Shutdown');
    }
}

/** The fiber of the durable function $key runs now (null: none): the ops and values it asks for belong to its journal. */
function enterDurable(?string $key): void
{
    Current::$durable = $key;
    \tenma_durable_context($key ?? '');
}

/** Seconds for timers and sleep() as milliseconds, validated: INF, NAN and negative values are errors. */
function milliseconds(float $seconds, int $minimum = 0): int
{
    if (!is_finite($seconds) || $seconds < 0) {
        throw new \ValueError("Invalid wait time: $seconds seconds");
    }
    // At most about 31 years: more does not fit safely into a timer
    return max($minimum, (int) round(min($seconds, 1e9) * 1000));
}

/**
 * Runs the hooks of tenma.json in $directory (src/dev.rs, which checked $hooks): orders them,
 * then runs each once and, with $watch, again whenever their inputs change. Without $watch
 * (`tenma build`) a failed hook makes tenma exit with 1.
 */
function dev(string $directory, string $hooks, bool $watch): void
{
    Dev::$directory = $directory;
    // What the hooks publish stays when the script reloads, and the other way round
    Diagnostics::$group = 'dev';
    try {
        Dev::load(json_decode($hooks, true, flags: JSON_THROW_ON_ERROR));
    } catch (DevError $error) {
        // A mistake in the list, not in code: the message is enough, no stack trace
        error_log('tenma: ' . $error->getMessage());
        Dev::$hooks = [];
        Dev::$failed = true;
    }
    \Threading\Task::run(fn () => Dev::run($watch));
}

/** @param list<mixed> $list */
function diagnostics(string $source, array $list): void
{
    if (preg_match('/^[\w.:-]+$/', $source) !== 1) {
        throw new \InvalidArgumentException("Problems::report(): '$source' is no source name: letters, digits, . : - and _");
    }
    $problems = [];
    foreach ($list as $i => $problem) {
        if (!is_array($problem) || !is_string($problem['file'] ?? null) || !is_int($problem['line'] ?? null) || !is_string($problem['message'] ?? null)) {
            throw new \InvalidArgumentException("Problems::report(): entry $i needs file, line and message");
        }
        $problems[] = [
            // Relative as for PHP's file functions, from the working directory: the editor needs the whole path
            'file' => str_starts_with($problem['file'], '/') ? $problem['file'] : getcwd() . '/' . $problem['file'],
            'line' => max(1, $problem['line']),
            'column' => max(1, is_int($problem['column'] ?? null) ? $problem['column'] : 1),
            'severity' => in_array($problem['severity'] ?? null, ['warning', 'info'], true) ? $problem['severity'] : 'error',
            'message' => $problem['message'],
        ];
    }
    // The setting --watch makes (shim/tenma.c): without it there is no editor to tell
    if (get_cfg_var('tenma.watch') !== '1') {
        return;
    }
    // The hooks run without a script of their own (eval): their project is the one of tenma.json, which is
    // also where --watch clears their lists
    $script = $_SERVER['SCRIPT_FILENAME'] ?? null;
    $from = match (true) {
        Diagnostics::$group === 'dev' => Dev::$directory,
        is_string($script) => dirname(realpath($script) ?: $script),
        default => (string) getcwd(),
    };
    $directory = Diagnostics::directory(Diagnostics::project($from));
    if (!is_dir($directory)) {
        @mkdir($directory, 0777, true);
        // Nothing of it belongs in the repository
        @file_put_contents(dirname($directory) . '/.gitignore', "*\n");
    }
    $file = "$directory/" . Diagnostics::$group . '.' . $source . '.json';
    // ponytail: blocking file access, only under --watch and only when something reports
    $before = is_file($file) ? json_decode((string) file_get_contents($file), true) : [];
    $known = array_map(static fn (mixed $p) => is_array($p) ? json_encode($p) : '', is_array($before) ? $before : []);
    // Every worker publishes the same: only what is new in the file goes to stderr, once
    foreach ($problems as $problem) {
        if (!in_array(json_encode($problem), $known, true)) {
            file_put_contents('php://stderr', "{$problem['file']}:{$problem['line']}:{$problem['column']}: {$problem['severity']}: {$problem['message']}\n");
        }
    }
    // Whole or not at all for the reader: written next to it, then renamed over it
    $temporary = "$file." . getmypid() . '.' . bin2hex(random_bytes(4));
    if (@file_put_contents($temporary, json_encode($problems, JSON_UNESCAPED_SLASHES | JSON_INVALID_UTF8_SUBSTITUTE)) !== false) {
        @rename($temporary, $file);
    }
}

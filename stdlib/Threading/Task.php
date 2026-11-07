<?php

namespace Threading;

/**
 * Work that runs in its own Fiber while the caller carries on; await() fetches its result.
 * Created with run(), all() or detach(). Belongs to the request it was created in.
 */
final class Task
{
    public readonly ?int $request;
    private bool $done = false;
    private mixed $result = null;
    private ?\Throwable $error = null;
    private bool $observed = false;
    /** A cancellation not yet delivered: the task's next wait throws it. */
    private ?\Throwable $cancelled = null;
    private bool $cancelRequested = false;
    private bool $requestEnded = false;
    private ?\Fiber $fiber = null;
    /** @var array<int, \Closure(self): void> */
    private array $listeners = [];
    private int $nextListener = 0;
    /** @internal @var array<int, mixed> AsyncLocal values, copied from the creator at start */
    public array $context;
    /** @internal What tenma --inspect calls the task: method and URI of a request, Timer, Task. */
    public string $label = 'Task';
    /** @internal The durable function this task runs, by key. */
    public ?string $durable = null;

    /** @param array<int, mixed>|null $context null: that of the running code */
    private function __construct(?int $request, ?array $context = null)
    {
        $this->request = $request;
        $this->context = $context ?? \Tenma\Internal\context();
    }

    /**
     * Starts $work in its own Fiber and returns the task; await() fetches the result later.
     * Called at script startup, $work starts once the event loop is running: background work.
     *
     * @param callable(): mixed $work
     */
    public static function run(callable $work): self
    {
        if (!\Tenma\Internal\Loop::$running) {
            return self::later($work);
        }
        \Tenma\Internal\requireFiber();
        return self::start($work);
    }

    /**
     * Runs everything concurrently and returns the results with the same keys:
     * Task::all(user: fn () => …, stats: fn () => …). If one fails, all() cancels
     * the others, waits until they have cleaned up, and then throws the first error: nothing is left
     * behind in the background that still uses connections, transactions or the stream. A Task passed
     * in is not cancelled: all() only stops waiting for it, as await() does.
     *
     * @param callable(): mixed|self ...$tasks
     * @return array<array-key, mixed>
     */
    public static function all(callable|self ...$tasks): array
    {
        $fiber = \Tenma\Internal\requireFiber();
        foreach ($tasks as $task) {
            if ($task instanceof self && $task->fiber === $fiber && !$task->done) {
                throw new \LogicException('A task cannot wait for itself');
            }
        }
        // Only the tasks started here are cancelled; passed-in ones (detach()!) are only waited for
        $owned = [];
        $started = [];
        foreach ($tasks as $key => $task) {
            if ($task instanceof self) {
                $started[$key] = $task;
                continue;
            }
            $started[$key] = $owned[] = self::start($task);
        }
        $pending = count($started);
        $failed = null;
        $token = \Tenma\Internal\token('all');
        $woken = false;
        $listeners = [];
        foreach ($started as $key => $task) {
            $listeners[$key] = $task->onSettled(function (self $task) use (&$pending, &$failed, &$woken, $fiber, $token): void {
                $pending--;
                if ($failed === null && $task->failed()) {
                    $failed = $task;
                }
                if (!$woken && ($pending === 0 || $failed !== null)) {
                    $woken = true;
                    \Tenma\Internal\wake($fiber, $token);
                }
            });
        }
        if ($pending > 0 && $failed === null) {
            try {
                \Tenma\Internal\suspend($token);
            } catch (\Throwable $error) {
                // all() itself was cancelled: cancel the children too, and only continue once they have cleaned up
                self::stopWaiting($started, $listeners);
                self::cancelAndWait($owned);
                throw $error;
            }
        }
        if ($failed !== null) {
            // Only the first error counts; the others are consequences of the cancellation
            self::stopWaiting($started, $listeners);
            $cancelled = self::cancelAndWait($owned);
            if ($cancelled !== null) {
                throw $cancelled;
            }
            $failed->await();
        }
        return array_map(fn (self $task) => $task->await(), $started);
    }

    /**
     * Like run(), but the task belongs to no request: if the client leaves or the request ends with
     * exit(), it keeps running. For work that must not be left half done (a payment and its
     * record, an email after the response). A waiter that gets cancelled only stops waiting.
     *
     * @param callable(): mixed $work
     */
    public static function detach(callable $work): self
    {
        \Tenma\Internal\requireFiber();
        return self::start($work, null);
    }

    /**
     * Waits $seconds without blocking the server. Other requests keep running. Inside a durable
     * function, a wait of a second or more is kept in the database: the function goes on after it even
     * if the process stops meanwhile.
     */
    public static function delay(float $seconds): void
    {
        \Tenma\Internal\requireFiber();
        if (\Tenma\Internal\Current::$durable !== null && $seconds >= Durable::TIMER) {
            Durable::wait('timer', microtime(true) + $seconds);
            return;
        }
        \Tenma\Internal\await(\tenma_op_sleep(\Tenma\Internal\milliseconds($seconds)));
    }

    /**
     * Removes the listeners all() put on its tasks, so a passed-in task that keeps running does not
     * hold the waiting Fiber.
     *
     * @param array<array-key, self> $tasks
     * @param array<array-key, int> $listeners
     */
    private static function stopWaiting(array $tasks, array $listeners): void
    {
        foreach ($tasks as $key => $task) {
            $task->offSettled($listeners[$key]);
        }
    }

    /**
     * Cancels the tasks and waits until they have cleaned up. Returns a cancellation of the
     * waiting task itself that came meanwhile: it is delivered only once, so the caller rethrows it.
     *
     * @param list<self> $tasks
     */
    private static function cancelAndWait(array $tasks): ?\Throwable
    {
        $cancelled = null;
        foreach ($tasks as $task) {
            $task->cancel();
        }
        foreach ($tasks as $task) {
            while (!$task->isDone()) {
                try {
                    $task->await();
                } catch (\Throwable $error) {
                    // Thrown while the task still runs: not its error, the waiting task was interrupted
                    if (!$task->isDone()) {
                        $cancelled ??= $error;
                    }
                }
            }
        }
        return $cancelled;
    }

    /**
     * @internal Starts $work right away; runs until the first op, then the caller continues. The
     * task belongs to $request (default: the current one) and is cancelled with it.
     */
    public static function start(callable $work, int|null|false $request = false, ?array $context = null, string $label = 'Task', ?string $durable = null): self
    {
        $task = new self($request === false ? \Tenma\Internal\Current::$request : $request, $context);
        $task->label = $label;
        $task->durable = $durable;
        $task->begin($work);
        return $task;
    }

    /** Starts $work once the event loop is running, for work from the script's startup. */
    private static function later(callable $work): self
    {
        $task = new self(null);
        \Tenma\Internal\whenRunning(fn () => $task->begin($work));
        return $task;
    }

    private function begin(callable $work): void
    {
        if ($this->cancelled !== null) {
            $this->settle(null, $this->takeCancellation());
            return;
        }
        $task = $this;
        $request = $this->request;
        $durable = $this->durable;
        $this->fiber = new \Fiber(static function () use ($task, $work, $request, $durable): void {
            \Tenma\Internal\Current::$request = $request;
            \Tenma\Internal\enterDurable($durable);
            try {
                $result = $work();
            } catch (\Throwable $error) {
                $task->settle(null, $error);
                return;
            }
            $task->settle($result, null);
        });
        \Tenma\Internal\Scheduler::$tasks[spl_object_id($this)] = $this;
        \Tenma\Internal\Scheduler::$taskOfFiber[spl_object_id($this->fiber)] = $this;
        if ($request !== null) {
            \Tenma\Internal\own($request, $this);
        }
        // The caller's request, not the task's: after Task::detach() that would be null
        $caller = \Tenma\Internal\Current::$request;
        $callerDurable = \Tenma\Internal\Current::$durable;
        $this->fiber->start();
        \Tenma\Internal\Current::$request = $caller;
        \Tenma\Internal\enterDurable($callerDurable);
    }

    /** @internal */
    public function settle(mixed $result, ?\Throwable $error): void
    {
        if ($this->done) {
            return;
        }
        $this->done = true;
        $this->result = $result;
        $this->error = $error;
        unset(\Tenma\Internal\Scheduler::$tasks[spl_object_id($this)]);
        if ($this->request !== null) {
            \Tenma\Internal\disown($this->request, $this);
        }
        if ($this->fiber !== null) {
            unset(\Tenma\Internal\Scheduler::$taskOfFiber[spl_object_id($this->fiber)]);
        }
        $listeners = $this->listeners;
        $this->listeners = [];
        foreach ($listeners as $listener) {
            $listener($this);
        }
    }

    /** @internal Calls $listener once the task is done; if it already is, right away. Returns an ID for offSettled(). */
    public function onSettled(\Closure $listener): int
    {
        if ($this->done) {
            $listener($this);
            return -1;
        }
        $this->listeners[$this->nextListener] = $listener;
        return $this->nextListener++;
    }

    /** @internal */
    public function offSettled(int $id): void
    {
        unset($this->listeners[$id]);
    }

    /** Waits until the task is done, without blocking the server; returns its result or throws its error. */
    public function await(): mixed
    {
        $this->observed = true;
        if (!$this->done) {
            $fiber = \Tenma\Internal\requireFiber();
            if ($fiber === $this->fiber) {
                throw new \LogicException('A task cannot wait for itself');
            }
            $token = \Tenma\Internal\token('task');
            $listener = $this->onSettled(fn () => \Tenma\Internal\wake($fiber, $token));
            try {
                \Tenma\Internal\suspend($token);
            } finally {
                // Interrupted (cancellation): otherwise the listener would hold the Fiber until this task ends
                $this->offSettled($listener);
            }
        }
        return $this->error !== null ? throw $this->error : $this->result;
    }

    /**
     * Cancels the task: wherever it is waiting (op, task, all()), $error is thrown (default: Cancelled);
     * finally and catch blocks run. If it is not waiting right now, its next wait throws.
     */
    public function cancel(?\Throwable $error = null): void
    {
        if ($this->done || $this->cancelRequested) {
            return;
        }
        $this->cancelRequested = true;
        $this->deliver($error ?? new Cancelled('Task cancelled'));
    }

    /**
     * @internal Its request ended (client gone, exit(), response out): delivered once more even after
     * a cancel(), which the task may have caught to retry; only one per request, so cleanup can wait.
     */
    public function endWithRequest(\Throwable $error): void
    {
        if ($this->done || $this->requestEnded) {
            return;
        }
        $this->requestEnded = $this->cancelRequested = true;
        $this->deliver($error);
    }

    private function deliver(\Throwable $error): void
    {
        if ($this->fiber !== null && $this->fiber->isTerminated()) {
            // The Fiber ended without settle(), for example through exit(): whoever waits for it must not wait forever
            $this->settle(null, $error);
            return;
        }
        // If the task is waiting right now, the cancellation is thrown there; otherwise at its next wait.
        // Afterwards it may clean up and wait again for that (for example a rollback in catch)
        if ($this->fiber === null || !\Tenma\Internal\interrupt($this->fiber, $error)) {
            $this->cancelled = $error;
        }
    }

    /** @internal A pending cancellation, delivered exactly once. */
    public function takeCancellation(): ?\Throwable
    {
        $cancelled = $this->cancelled;
        $this->cancelled = null;
        return $cancelled;
    }

    /** @internal */
    public function failed(): bool
    {
        return $this->error !== null;
    }

    public function isDone(): bool
    {
        return $this->done;
    }

    /** If nobody ever calls await(), the error is not silently lost. */
    public function __destruct()
    {
        if ($this->error !== null && !$this->observed && !$this->error instanceof Cancelled) {
            error_log('Error in a task that nobody awaited: ' . $this->error);
        }
    }
}

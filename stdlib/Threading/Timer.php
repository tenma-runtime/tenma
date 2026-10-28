<?php

namespace Threading;

/** A timer from after() or every(); cancel() stops it. */
final class Timer
{
    private int $op = 0;
    private bool $cancelled = false;
    /** For loop(): the wait before the next call, as the last call returned it */
    private int $wait;
    /** The request the timer was set in: if the client leaves, the timer ends too. */
    private readonly ?int $request;
    /** @var array<int, mixed> AsyncLocal values from where the timer was set */
    private readonly array $context;

    /**
     * Calls $callback once after $seconds seconds, in its own Fiber. Works at script startup and in
     * asynchronous code. A script without a server runs until all timers have fired. With several
     * workers, each worker schedules its own timers, also after every restart.
     */
    public static function after(float $seconds, callable $callback): self
    {
        $timer = new self($callback(...), \Tenma\Internal\milliseconds($seconds), false);
        \Tenma\Internal\Loop::$running && \Tenma\Internal\requireFiber();
        \Tenma\Internal\whenRunning($timer->arm(...));
        return $timer;
    }

    /**
     * Calls $callback every $seconds seconds until cancel(); keeps a script without a server alive. The clock
     * does not wait for the callback: a call that waits longer than $seconds overlaps the next one. With
     * $overlap false a tick is skipped while the previous call still runs.
     */
    public static function every(float $seconds, callable $callback, bool $overlap = true): self
    {
        $callback = $callback(...);
        if (!$overlap) {
            $running = false;
            $callback = static function () use (&$running, $callback): void {
                if ($running) {
                    return;
                }
                $running = true;
                try {
                    $callback();
                } finally {
                    $running = false;
                }
            };
        }
        $timer = new self($callback, \Tenma\Internal\milliseconds($seconds, 1), true);
        \Tenma\Internal\Loop::$running && \Tenma\Internal\requireFiber();
        \Tenma\Internal\whenRunning($timer->arm(...));
        return $timer;
    }

    /**
     * Calls $callback after $first seconds, then again after as many seconds as each call returns: 600 to look
     * again in ten minutes, 3600 after a rate limit, null to stop. The wait starts when a call ends, so calls
     * never overlap. A call that throws keeps the loop, with the wait of the call before; the error is reported
     * as from any task. Keeps a script without a server alive until it stops or cancel().
     *
     * @param callable(): (float|int|null) $callback
     */
    public static function loop(callable $callback, float $first = 0): self
    {
        $timer = new self($callback(...), \Tenma\Internal\milliseconds($first), false, true);
        \Tenma\Internal\Loop::$running && \Tenma\Internal\requireFiber();
        \Tenma\Internal\whenRunning($timer->arm(...));
        return $timer;
    }

    private function __construct(
        private readonly \Closure $callback,
        private readonly int $milliseconds,
        private readonly bool $repeat,
        private readonly bool $loop = false,
    ) {
        $this->wait = $milliseconds;
        $this->request = \Tenma\Internal\Current::$request;
        $this->context = \Tenma\Internal\context();
        if ($this->request !== null) {
            \Tenma\Internal\own($this->request, $this);
        }
    }

    private function forget(): void
    {
        if ($this->request !== null) {
            \Tenma\Internal\disown($this->request, $this);
        }
    }

    /** @internal */
    public function arm(): void
    {
        if ($this->cancelled) {
            return;
        }
        $this->op = \tenma_op_sleep($this->wait);
        if ($this->op !== 0) {
            \Tenma\Internal\Waiting::$timers[$this->op] = $this;
        }
    }

    /** @internal The wait is over: callback in its own Fiber; for setInterval, re-arm right away. */
    public function fire(): void
    {
        $this->op = 0;
        if ($this->cancelled) {
            return;
        }
        if ($this->loop) {
            Task::start($this->round(...), null, $this->context, 'Loop');
            return;
        }
        if ($this->repeat) {
            $this->arm();
        } else {
            $this->forget();
        }
        // The callback runs as background work, as before (exit() inside it ends Tenma); only the
        // wait until then belongs to the request
        Task::start($this->callback, null, $this->context, $this->repeat ? 'Interval' : 'Timer');
    }

    /** One call of loop(), then the wait it asked for */
    private function round(): void
    {
        try {
            $next = ($this->callback)();
        } catch (\Throwable $error) {
            $this->arm();
            throw $error;
        }
        if ($this->cancelled) {
            return;
        }
        if ($next === null) {
            $this->forget();
            return;
        }
        $this->wait = \Tenma\Internal\milliseconds(is_int($next) || is_float($next) ? (float) $next : 0.0);
        $this->arm();
    }

    /** Stops the timer; a callback already running runs to completion. */
    public function cancel(): void
    {
        $this->cancelled = true;
        $this->forget();
        if ($this->op !== 0) {
            unset(\Tenma\Internal\Waiting::$timers[$this->op]);
            \tenma_op_cancel($this->op);
            $this->op = 0;
        }
    }
}

<?php

namespace Threading;

/**
 * A value that travels along with asynchronous work, for example the
 * request ID for log lines. Every task (Task::run(), all(), detach(), timer) gets a copy of its creator's
 * values at start; set() in the task changes only its own copy and that of its later children. A request
 * starts with the values from the script; what a classic request sets ends with it. In a custom Fiber
 * set() throws LogicException.
 *
 * @template T
 */
final class AsyncLocal
{
    private static int $next = 0;
    private readonly int $id;

    public function __construct()
    {
        $this->id = ++self::$next;
    }

    /** @return T|null */
    public function get(): mixed
    {
        return \Tenma\Internal\context()[$this->id] ?? null;
    }

    /** @param T|null $value null removes the value */
    public function set(mixed $value): void
    {
        $task = \Tenma\Internal\currentTask();
        if ($task !== null) {
            $context = &$task->context;
        } elseif (\Fiber::getCurrent() !== null && \Tenma\Internal\Current::$classic === null) {
            // A custom Fiber has no task: the value would reach every later request
            throw new \LogicException('AsyncLocal::set() needs a task or the script, not a custom Fiber');
        } else {
            $context = &\Tenma\Internal\Scheduler::$rootContext;
        }
        if ($value === null) {
            unset($context[$this->id]);
        } else {
            $context[$this->id] = $value;
        }
    }
}

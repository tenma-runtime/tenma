<?php
// For tests/http.rs: fibers with sleep and fetch
use Http\Request;


$port = (int) getenv('PORT');
// An interval that runs in the background next to the requests
$ticks = 0;
// What requests left behind: their tasks, timers and subscriptions must end with the response
$left = ['interval' => 0, 'message' => 0, 'task' => 0, 'detached' => 0, 'retried' => 0, 'late' => 0];
\Threading\Timer::every(0.05, function () use (&$ticks) {
    $ticks++;
});
(new \Http\Server($port, function (Request $request) use ($port, &$ticks, &$left): string {
    switch ($request->path) {
        case '/slow':
            \Threading\Task::delay(0.5);
            return "slept enough\n";
        case '/proxy':
            // Asks the same server: would never work without fibers, it would be blocked
            return 'proxy: ' . (new \Http\Client())->get("http://127.0.0.1:$port/hello")->body;
        case '/hang':
            \Threading\Task::delay(3);
            return "too late\n";
        case '/fetch-timeout':
            try {
                return (new \Http\Client(0.2))->get("http://127.0.0.1:$port/hang")->body;
            } catch (RuntimeException) {
                return "timeout\n";
            }
        case '/ticks':
            return "ticks $ticks\n";
        case '/leave-behind':
            // Cancelled once, it catches that and carries on: the end of the request still reaches it
            $retry = \Threading\Task::run(function () use (&$left) {
                try {
                    \Threading\Task::delay(1);
                } catch (Threading\Cancelled) {
                }
                try {
                    \Threading\Task::delay(0.05);
                    $left['retried']++;
                } catch (Threading\Cancelled) {
                    // Started in its cleanup after the request ended: ends at its first wait
                    \Threading\Task::run(function () use (&$left) { \Threading\Task::delay(0.05); $left['late']++; });
                }
            });
            $retry->cancel();
            \Threading\Task::delay(0.01);
            \Threading\Timer::every(0.01, function () use (&$left) { $left['interval']++; });
            (new \Messaging\Channel('left'))->subscribe(function () use (&$left) { $left['message']++; });
            \Threading\Task::run(function () use (&$left) { \Threading\Task::delay(0.05); $left['task']++; });
            \Threading\Task::detach(function () use (&$left) { \Threading\Task::delay(0.05); $left['detached']++; });
            return "started\n";
        case '/left':
            (new \Messaging\Channel('left'))->publish('anyone?');
            \Threading\Task::delay(0.1);
            return json_encode($left) . "\n";
        case '/all':
            // Three waits of 0.3 s each at the same time, named results
            $slow = fn (string $name) => function () use ($name) {
                \Threading\Task::delay(0.3);
                return $name;
            };
            return json_encode(\Threading\Task::all(a: $slow('one'), b: $slow('two'), c: \Threading\Task::run($slow('three')))) . "\n";
        case '/all-error':
            try {
                \Threading\Task::all(fn () => 'good', function () {
                    \Threading\Task::delay(0.05);
                    throw new RuntimeException('broken');
                });
            } catch (RuntimeException $error) {
                return "caught: {$error->getMessage()}\n";
            }
        case '/buffered':
            // A template that waits while it renders: its buffer stays its own, with a callback one inside
            $id = $request->query['id'] ?? '';
            ob_start();
            echo "<$id>";
            ob_start(fn (string $text): string => strtoupper($text));
            echo "inner $id";
            \Threading\Task::delay(0.05);
            ob_end_flush();
            \Threading\Task::delay(0.01);
            echo "</$id>";
            return ob_get_clean() . "\n";
        case '/buffer-left-open':
            // Ends with a buffer still open: it goes with the request, the next one starts without it
            ob_start();
            echo 'never sent';
            \Threading\Task::delay(0.01);
            return 'level ' . ob_get_level() . "\n";
        case '/level':
            return 'level ' . ob_get_level() . "\n";
        case '/nested':
            // A task that starts tasks itself, and a fetch to the same server in between
            $outer = \Threading\Task::run(fn () => \Threading\Task::all(fn () => (new \Http\Client())->get("http://127.0.0.1:$port/hello")->body, function () {
                \Threading\Task::delay(0.1);
                return 'inner';
            }));
            return json_encode($outer->await()) . "\n";
        case '/exit-task':
            // exit() in a task ends the request, whoever awaits the task does not hang
            \Threading\Task::run(function () {
                \Threading\Task::delay(0.05);
                exit;
            })->await();
            return "never\n";
        case '/self-await':
            $task = null;
            $task = \Threading\Task::run(function () use (&$task) {
                \Threading\Task::delay(0.01);
                try {
                    return $task->await();
                } catch (LogicException) {
                    return "rejected\n";
                }
            });
            return $task->await();
        case '/all-fast':
            // The error comes at once, not only after the slow sibling task
            try {
                \Threading\Task::all(function () {
                    \Threading\Task::delay(5);
                    return 'slow';
                }, function () {
                    \Threading\Task::delay(0.05);
                    throw new RuntimeException('failed fast');
                });
            } catch (RuntimeException $error) {
                return "{$error->getMessage()}\n";
            }
        case '/cancel-race':
            // A and B await X; A cancels B as soon as X is done: B must see the cancellation
            $x = \Threading\Task::run(function () {
                \Threading\Task::delay(0.05);
                return 'x';
            });
            $b = null;
            $a = \Threading\Task::run(function () use ($x, &$b) {
                $x->await();
                $b->cancel();
            });
            $b = \Threading\Task::run(function () use ($x) {
                try {
                    $x->await();
                    \Threading\Task::run(fn () => \Threading\Task::delay(0.05))->await();
                    return "b kept running\n";
                } catch (Threading\Cancelled) {
                    return "b cancelled\n";
                }
            });
            $a->await();
            return $b->await();
        case '/all-self':
            // The running task in its own all(): rejected instead of spinning forever
            $task = null;
            $task = \Threading\Task::run(function () use (&$task) {
                \Threading\Task::delay(0.01);
                try {
                    \Threading\Task::all($task, fn () => throw new RuntimeException('sibling'));
                } catch (LogicException) {
                    return "rejected\n";
                }
            });
            return $task->await();
        case '/all-keeps-passed':
            // A failing sibling cancels only what all() started, not a task passed in
            $kept = \Threading\Task::detach(function () {
                \Threading\Task::delay(0.1);
                return 'kept';
            });
            try {
                \Threading\Task::all($kept, function () {
                    \Threading\Task::delay(0.02);
                    throw new RuntimeException('sibling');
                });
            } catch (RuntimeException) {
            }
            return $kept->await() . "\n";
        case '/cancel-all':
            $late = false;
            $task = \Threading\Task::run(fn () => \Threading\Task::all(fn () => \Threading\Task::delay(0.3), function () use (&$late) {
                \Threading\Task::delay(0.1);
                $late = true;
            }));
            \Threading\Task::delay(0.02);
            $task->cancel();
            try {
                $task->await();
            } catch (Threading\Cancelled) {
            }
            \Threading\Task::delay(0.2);
            return $late ? "child kept running\n" : "children cancelled\n";
        case '/bad-method':
            try {
                return (new \Http\Client())->send("GE T", "http://127.0.0.1:$port/hello", [], '')->body;
            } catch (Throwable $error) {
                return $error->getMessage() . "\n";
            }
        case '/unreachable':
            return (new \Http\Client())->get('http://127.0.0.1:1/')->body;
        default:
            return "hello\n";
    }
}))->start();

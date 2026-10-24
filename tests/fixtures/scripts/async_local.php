<?php
// For tests/http.rs: Threading\AsyncLocal travels with tasks and timers
$id = new Threading\AsyncLocal();
$id->set('script');

$request = function (string $name) use ($id): Threading\Task {
    return \Threading\Task::run(function () use ($id, $name) {
        $id->set($name);
        $child = \Threading\Task::run(function () use ($id) {
            \Threading\Task::delay(0.02);
            $seen = $id->get();
            $id->set('child-own');
            return $seen;
        });
        \Threading\Timer::after(0.01, fn () => print("timer $name: {$id->get()}\n"));
        $detached = \Threading\Task::detach(function () use ($id) {
            \Threading\Task::delay(0.03);
            return $id->get();
        });
        \Threading\Task::delay(0.01);
        // End of the request: its own value goes, children keep their copy
        $id->set(null);
        return [$name, $child->await(), $detached->await(), $id->get()];
    });
};

\Threading\Task::run(function () use ($request, $id) {
    // Two interleaved "requests": each sees only its own value, the child does not write back
    [$a, $b] = \Threading\Task::all($request('a'), $request('b'));
    echo json_encode([$a, $b, $id->get()]), "\n";
});

// A custom Fiber has no task to hold the value
$fiber = new Fiber(fn () => $id->set('fiber'));
try {
    $fiber->start();
} catch (LogicException) {
    echo "custom fiber refused, still {$id->get()}\n";
}

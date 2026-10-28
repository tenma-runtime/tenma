<?php
// For tests/http.rs: timers and background work in a script without a server
$ticks = 0;
$interval = \Threading\Timer::every(0.02, function () use (&$ticks, &$interval) {
    if (++$ticks === 3) {
        $interval->cancel();
    }
});
\Threading\Timer::after(0.03, fn () => print("cancelled must not appear\n"))->cancel();
\Threading\Task::run(function () {
    \Threading\Task::delay(0.05);
    echo "background done\n";
});
// A call that waits longer than the interval: without overlap the ticks in between are skipped
$calls = 0;
$slow = \Threading\Timer::every(0.02, function () use (&$calls) {
    $calls++;
    \Threading\Task::delay(0.07);
}, overlap: false);
\Threading\Timer::after(0.13, fn () => $slow->cancel());
// loop(): the call returns the next wait, null stops it; a cancelled loop runs no more
$rounds = 0;
\Threading\Timer::loop(function () use (&$rounds): ?float {
    return ++$rounds < 3 ? 0.02 : null;
});
$cancelled = 0;
$stopped = \Threading\Timer::loop(function () use (&$cancelled): float {
    $cancelled++;
    return 0.02;
}, 0.01);
\Threading\Timer::after(0.05, fn () => $stopped->cancel());
\Threading\Timer::after(0.15, function () use (&$ticks, &$calls, &$rounds, &$cancelled) {
    echo "ticks $ticks\n";
    echo "slow calls $calls\n";
    echo "rounds $rounds\n";
    echo $cancelled >= 2 && $cancelled <= 3 ? "cancelled loop stopped\n" : "cancelled loop ran $cancelled\n";
    \Threading\Timer::after(0.01, fn () => print("nested\n"));
});
echo "script done\n";

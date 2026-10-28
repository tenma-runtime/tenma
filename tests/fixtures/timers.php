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
\Threading\Timer::after(0.15, function () use (&$ticks, &$calls) {
    echo "ticks $ticks\n";
    echo "slow calls $calls\n";
    \Threading\Timer::after(0.01, fn () => print("nested\n"));
});
echo "script done\n";

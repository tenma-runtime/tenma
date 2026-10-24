<?php
// For tests/http.rs: a slow subscription takes one message at a time, in order, and drops what overflows
$channel = new Messaging\Channel('load');
$seen = [];
$running = 0;
$most = 0;
$subscription = $channel->subscribe(function (string $message) use (&$seen, &$running, &$most) {
    $running++;
    $most = max($most, $running);
    Threading\Task::delay(0.001);
    $seen[] = (int) $message;
    $running--;
});
Threading\Task::run(function () use ($channel) {
    for ($i = 0; $i < 3000; $i++) {
        $channel->publish((string) $i);
    }
});
Threading\Timer::after(0.5, function () use ($subscription, &$seen, &$most) {
    $subscription->cancel();
    $sorted = $seen;
    sort($sorted);
    echo json_encode([
        'most' => $most,
        'ordered' => $seen === $sorted,
        'dropped' => count($seen) < 3000,
        'first' => $seen[0] ?? null,
    ]), "\n";
});

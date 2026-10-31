<?php
// For tests/http.rs: messages between workers. Every worker subscribes at startup and keeps what it hears
$heard = [];
// Its own name per test, as the tests on one Redis run at once
$channel = getenv('CHANNEL') ?: 'news';
// Through Redis without --pubsub; every worker asks, the same URL again is fine
if (getenv('PUBSUB')) {
    \Messaging\Channel::connect(getenv('PUBSUB'));
}
(new \Messaging\Channel($channel))->subscribe(function (string $message) use (&$heard) {
    $heard[] = $message;
});
$muted = (new \Messaging\Channel($channel))->subscribe(function () use (&$heard) {
    $heard[] = 'cancelled';
});
$muted->cancel();

(new \Http\Server((int) getenv('PORT'), function (Http\Request $request) use (&$heard, $channel) {
    if ($request->path === '/publish') {
        (new \Messaging\Channel($channel))->publish($request->query['message']);
        return "published\n";
    }
    if ($request->path === '/kill-publisher') {
        // The connection the Redis bridge publishes on: its next PUBLISH fails
        $redis = Data\Redis::connect('redis://127.0.0.1:6379');
        foreach (explode("\n", trim($redis->command('CLIENT', 'LIST', 'TYPE', 'normal'))) as $line) {
            parse_str(str_replace(' ', '&', $line), $client);
            if ($client['cmd'] === 'publish' && (int) $client['age'] < 30) {
                $redis->command('CLIENT', 'KILL', 'ID', $client['id']);
            }
        }
        return "killed\n";
    }
    if ($request->path === '/connect-other') {
        try {
            \Messaging\Channel::connect('redis://127.0.0.1:1');
        } catch (\LogicException $refused) {
            return $refused->getMessage() . "\n";
        }
        return "connected\n";
    }
    // Made in a request: ends with it, so later messages find no callback of it
    if ($request->path === '/wait') {
        $message = null;
        (new \Messaging\Channel($channel))->subscribe(function (string $received) use (&$message) {
            $message = $received;
        });
        while ($message === null) {
            \Threading\Task::delay(0.01);
        }
        return "waited for $message\n";
    }
    return implode(',', $heard) . "\n";
}))->start();

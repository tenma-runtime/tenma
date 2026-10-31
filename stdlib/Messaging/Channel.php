<?php

namespace Messaging;

/**
 * A named channel for messages between the workers of a process, like PUBLISH and SUBSCRIBE in
 * Redis. Messages are strings: workers share no PHP objects. With `tenma --pubsub redis://host` or
 * connect(), messages go through Redis and reach every process.
 */
final class Channel
{
    public function __construct(public readonly string $name) {}

    /**
     * Sends the messages of every channel through the Redis at $url from now on, as `--pubsub` does: they
     * reach every process on that Redis. For a Redis the script starts itself. Call it at startup: it
     * holds for the whole process, subscriptions made before included, and what was published before
     * reached only this process. The same URL again does nothing.
     *
     * @throws \InvalidArgumentException when $url is no Redis URL
     * @throws \LogicException when messages go through another Redis already
     */
    public static function connect(string $url): void
    {
        match (\tenma_pubsub('connect', 0, $url, '')) {
            1 => null,
            2 => throw new \LogicException('Messages go through another Redis already'),
            default => throw new \InvalidArgumentException('No Redis URL'),
        };
    }

    /**
     * Sends $message to every subscription of the channel, in every worker of this process, this one
     * included. A worker that has not subscribed yet (still starting, restarting) misses the message.
     */
    public function publish(string $message): void
    {
        \tenma_pubsub('publish', 0, $this->name, $message);
    }

    /**
     * Calls $callback with every message published to the channel, in its own Fiber, until cancel().
     * One message at a time and in order: the next call waits for the last one. A subscription that
     * falls 1024 messages behind misses the newer ones. Works at script startup and in asynchronous
     * code; made in a request, it ends with the request, for WebSockets and streams with the
     * connection. Keeps a script without a server alive.
     *
     * @param callable(string): mixed $callback
     */
    public function subscribe(callable $callback): Subscription
    {
        \Tenma\Internal\Loop::$running && \Tenma\Internal\requireFiber();
        $subscription = new Subscription($this->name, $callback(...));
        \Tenma\Internal\whenRunning($subscription->arm(...));
        return $subscription;
    }
}

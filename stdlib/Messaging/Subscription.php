<?php

namespace Messaging;

/** A subscription from Channel::subscribe(); cancel() ends it. */
final class Subscription
{
    private readonly int $id;
    private int $op = 0;
    private bool $cancelled = false;
    /** The request the subscription was made in: if the client leaves, it ends too. */
    private readonly ?int $request;
    /** @var array<int, mixed> AsyncLocal values from where it was made */
    private readonly array $context;

    /** @internal Created by Channel::subscribe(). */
    public function __construct(string $channel, private readonly \Closure $callback)
    {
        // Right away, also at script startup: what is published until the loop runs waits in the queue
        $this->id = \tenma_pubsub('subscribe', 0, $channel, '');
        $this->request = \Tenma\Internal\Current::$request;
        $this->context = \Tenma\Internal\context();
        if ($this->request !== null) {
            \Tenma\Internal\own($this->request, $this);
        }
    }

    /** @internal Waits for the next message. */
    public function arm(): void
    {
        if ($this->cancelled) {
            return;
        }
        $this->op = \tenma_pubsub('receive', $this->id, '', '');
        if ($this->op !== 0) {
            \Tenma\Internal\Waiting::$subscriptions[$this->op] = $this;
        }
    }

    /**
     * @internal A message came: the callback runs in its own Fiber, and the next wait starts once it is
     * done. Messages stay in order, and a slow callback holds one message, not one Fiber per message.
     */
    public function fire(bool $ok, mixed $message): void
    {
        $this->op = 0;
        if ($this->cancelled || !$ok || !is_string($message)) {
            return;
        }
        $callback = $this->callback;
        $arm = $this->arm(...);
        \Threading\Task::start(static function () use ($callback, $message, $arm): void {
            try {
                $callback($message);
            } finally {
                $arm();
            }
        }, null, $this->context, 'Subscription');
    }

    /** Stops the subscription; callbacks already running run to completion. */
    public function cancel(): void
    {
        if ($this->cancelled) {
            return;
        }
        $this->cancelled = true;
        if ($this->request !== null) {
            \Tenma\Internal\disown($this->request, $this);
        }
        if ($this->op !== 0) {
            unset(\Tenma\Internal\Waiting::$subscriptions[$this->op]);
            \tenma_op_cancel($this->op);
            $this->op = 0;
        }
        \tenma_pubsub('unsubscribe', $this->id, '', '');
    }
}

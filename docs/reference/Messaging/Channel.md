---
title: "Channel"
description: "A named channel for messages between the workers of a process, like PUBLISH and SUBSCRIBE in Redis."
order: 100
generated: true
---

`Messaging\Channel` · final class · namespace [`Messaging`](index.md)

```php
final class Channel
```

A named channel for messages between the workers of a process, like PUBLISH and SUBSCRIBE in
Redis. Messages are strings: workers share no PHP objects. With `tenma --pubsub redis://host`,
messages go through Redis and reach every process.

## Properties

### $name

```php
public readonly string $name;
```

## Methods

### __construct()

```php
public function __construct(public readonly string $name)
```

**Parameters**

- `$name` `string`

### publish()

```php
public function publish(string $message): void
```

Sends $message to every subscription of the channel, in every worker of this process, this one
included. A worker that has not subscribed yet (still starting, restarting) misses the message.

**Parameters**

- `$message` `string`

### subscribe()

```php
public function subscribe(callable $callback): \Messaging\Subscription
```

Calls $callback with every message published to the channel, in its own Fiber, until cancel().
One message at a time and in order: the next call waits for the last one. A subscription that
falls 1024 messages behind misses the newer ones. Works at script startup and in asynchronous
code; made in a request, it ends with the request, for WebSockets and streams with the
connection. Keeps a script without a server alive.

**Parameters**

- `$callback` `callable(string): mixed`

**Returns** [`Subscription`](Subscription.md)

---
title: "Task"
description: "Work that runs in its own Fiber while the caller carries on; await() fetches its result."
order: 100
generated: true
---

`Threading\Task` · final class · namespace [`Threading`](index.md)

```php
final class Task
```

Work that runs in its own Fiber while the caller carries on; await() fetches its result.
Created with run(), all() or detach(). Belongs to the request it was created in.

## Properties

### $request

```php
public readonly ?int $request;
```

## Methods

### run()

```php
public static function run(callable $work): \Threading\Task
```

Starts $work in its own Fiber and returns the task; await() fetches the result later.
Called at script startup, $work starts once the event loop is running: background work.

**Parameters**

- `$work` `callable(): mixed`

**Returns** [`Task`](Task.md)

### all()

```php
public static function all(\Threading\Task|callable ...$tasks): array
```

Runs everything concurrently and returns the results with the same keys:
Task::all(user: fn () => …, stats: fn () => …). If one fails, all() cancels
the others, waits until they have cleaned up, and then throws the first error: nothing is left
behind in the background that still uses connections, transactions or the stream. A Task passed
in is not cancelled: all() only stops waiting for it, as await() does.

**Parameters**

- `...$tasks` `callable(): mixed|self`

**Returns** `array<array-key, mixed>`

### detach()

```php
public static function detach(callable $work): \Threading\Task
```

Like run(), but the task belongs to no request: if the client leaves or the request ends with
exit(), it keeps running. For work that must not be left half done (a payment and its
record, an email after the response). A waiter that gets cancelled only stops waiting.

**Parameters**

- `$work` `callable(): mixed`

**Returns** [`Task`](Task.md)

### delay()

```php
public static function delay(float $seconds): void
```

Waits $seconds without blocking the server. Other requests keep running. Inside a durable
function, a wait of a second or more is kept in the database: the function goes on after it even
if the process stops meanwhile.

**Parameters**

- `$seconds` `float`

### await()

```php
public function await(): mixed
```

Waits until the task is done, without blocking the server; returns its result or throws its error.

**Returns** `mixed`

### cancel()

```php
public function cancel(?\Throwable $error = null): void
```

Cancels the task: wherever it is waiting (op, task, all()), $error is thrown (default: Cancelled);
finally and catch blocks run. If it is not waiting right now, its next wait throws.

**Parameters**

- `$error` `?`[`Throwable`](https://www.php.net/manual/en/class.throwable.php)

### isDone()

```php
public function isDone(): bool
```

**Returns** `bool`

### __destruct()

```php
public function __destruct()
```

If nobody ever calls await(), the error is not silently lost.

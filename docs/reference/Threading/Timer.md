---
title: "Timer"
description: "A timer from after() or every(); cancel() stops it."
order: 100
generated: true
---

`Threading\Timer` · final class · namespace [`Threading`](index.md)

```php
final class Timer
```

A timer from after() or every(); cancel() stops it.

## Methods

### after()

```php
public static function after(float $seconds, callable $callback): \Threading\Timer
```

Calls $callback once after $seconds seconds, in its own Fiber. Works at script startup and in
asynchronous code. A script without a server runs until all timers have fired. With several
workers, each worker schedules its own timers, also after every restart.

**Parameters**

- `$seconds` `float`
- `$callback` `callable`

**Returns** [`Timer`](Timer.md)

### every()

```php
public static function every(float $seconds, callable $callback, bool $overlap = true): \Threading\Timer
```

Calls $callback every $seconds seconds until cancel(); keeps a script without a server alive. The clock
does not wait for the callback: a call that waits longer than $seconds overlaps the next one. With
$overlap false a tick is skipped while the previous call still runs.

**Parameters**

- `$seconds` `float`
- `$callback` `callable`
- `$overlap` `bool`

**Returns** [`Timer`](Timer.md)

### loop()

```php
public static function loop(callable $callback, float $first = 0.0): \Threading\Timer
```

Calls $callback after $first seconds, then again after as many seconds as each call returns: 600 to look
again in ten minutes, 3600 after a rate limit, null to stop. The wait starts when a call ends, so calls
never overlap. A call that throws keeps the loop, with the wait of the call before; the error is reported
as from any task. Keeps a script without a server alive until it stops or cancel().

**Parameters**

- `$callback` `callable(): (float|int|null)`
- `$first` `float`

**Returns** [`Timer`](Timer.md)

### cancel()

```php
public function cancel(): void
```

Stops the timer; a callback already running runs to completion.

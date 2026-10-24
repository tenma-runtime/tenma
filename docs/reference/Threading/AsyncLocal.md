---
title: "AsyncLocal"
description: "A value that travels along with asynchronous work, for example the request ID for log lines."
order: 100
generated: true
---

`Threading\AsyncLocal` · final class · namespace [`Threading`](index.md)

```php
final class AsyncLocal
```

A value that travels along with asynchronous work, for example the
request ID for log lines. Every task (Task::run(), all(), detach(), timer) gets a copy of its creator's
values at start; set() in the task changes only its own copy and that of its later children. A request
starts with the values from the script; what a classic request sets ends with it. In a custom Fiber
set() throws LogicException.

**Template parameters**

- `T`

## Methods

### __construct()

```php
public function __construct()
```

### get()

```php
public function get(): mixed
```

**Returns** `T|null`

### set()

```php
public function set(mixed $value): void
```

**Parameters**

- `$value` `T|null`: null removes the value

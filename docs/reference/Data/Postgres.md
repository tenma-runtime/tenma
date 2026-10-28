---
title: "Postgres"
description: "Postgres, asynchronous: while a query runs, only this request's Fiber waits."
order: 100
generated: true
---

`Data\Postgres` · final class · namespace [`Data`](index.md)

```php
final class Postgres
```

Postgres, asynchronous: while a query runs, only this request's Fiber waits.
Placeholders $1, $2, … as in Postgres; parameters are sent as text, the server determines the type.
A PHP list for an array parameter (`$1::text[]`, `= ANY ($1::int[])`) goes as an array, quoted and
escaped; for json or jsonb it stays JSON.
Only in asynchronous code (handlers, tasks, timers); at script startup, wrap queries in Task::run().

## Methods

### connect()

```php
public static function connect(string $url): \Data\Postgres
```

**Parameters**

- `$url` `string`: postgres://user:password@host:5432/database

**Returns** [`Postgres`](Postgres.md)

### query()

```php
public function query(string $sql, array $params = [], ?callable $as = null): array
```

Rows as a list of associative arrays, values with Postgres' JSON types; with $as each row goes through it
instead: `as: Row::of(...)` for typed getters, `as: Order::from(...)` for objects,
`as: fn (array $row) => (string) $row['name']` for one column. SHOW, EXPLAIN and FETCH take no
parameters and return every value as a string.

**Parameters**

- `$sql` `string`
- `$params` `list<mixed>`
- `$as` `(callable(array<string, mixed>): T)|null`

**Returns** `($as is null ? list<array<string, mixed>> : list<T>)`

**Template parameters**

- `T`

### first()

```php
public function first(string $sql, array $params = [], ?callable $as = null): mixed
```

The first row, or null; with $as it goes through $as as in query().

**Parameters**

- `$sql` `string`
- `$params` `list<mixed>`
- `$as` `(callable(array<string, mixed>): T)|null`

**Returns** `($as is null ? array<string, mixed>|null : T|null)`

**Template parameters**

- `T`

### script()

```php
public function script(string $sql): void
```

Several statements separated by semicolons, without parameters, such as a migration file. They run as
one: a failing statement undoes the ones before it, inside transaction() as part of that transaction.
Therefore no COMMIT or ROLLBACK in it, and nothing that refuses to run in a transaction (CREATE INDEX
CONCURRENTLY, VACUUM, CREATE DATABASE). Never build it from input: without parameters, anything in it
is SQL.

**Parameters**

- `$sql` `string`

### execute()

```php
public function execute(string $sql, array $params = []): int
```

For statements without a result; returns the number of affected rows.

**Parameters**

- `$sql` `string`
- `$params` `list<mixed>`

**Returns** `int`

### transaction()

```php
public function transaction(callable $work): mixed
```

Runs $work in a transaction: commit if $work completes, otherwise rollback.

**Parameters**

- `$work` `callable(self): T`

**Returns** `T`

**Template parameters**

- `T`

---
title: "Migrations"
description: "Brings a database up to date: named steps, each applied once and in the order given."
order: 100
generated: true
---

`Data\Migrations` · final class · namespace [`Data`](index.md)

```php
final class Migrations
```

Brings a database up to date: named steps, each applied once and in the order given. All pending steps run
in one transaction under an advisory lock, so workers or processes starting together apply each step once,
and a failing step leaves the database as it was. A step is SQL or a function that gets the transaction.

```php
$ran = new Migrations($db)->apply([
    '001_users' => 'CREATE TABLE users (id bigserial PRIMARY KEY, email text NOT NULL)',
    '002_names' => fn (Postgres $tx) => $tx->execute('ALTER TABLE users ADD COLUMN name text'),
    ...Migrations::files(__DIR__ . '/migrations'),
]);
```

## Methods

### __construct()

```php
public function __construct(
    private readonly \Data\Postgres $db,
    private readonly string $table = 'migrations',
)
```

**Parameters**

- `$db` [`Postgres`](Postgres.md)
- `$table` `string`: Where the applied names are kept; its name also picks the lock

### apply()

```php
public function apply(array $steps): array
```

Applies the steps not applied yet; returns their names. Only in asynchronous code: at script startup,
inside Task::run().

**Parameters**

- `$steps` `array<string, string|callable(`[`Postgres`](Postgres.md)`): mixed>`: name => SQL or function

**Returns** `list<string>`

### files()

```php
public static function files(string $directory): array
```

The *.sql files of a directory as steps, in name order (0001_users.sql, 0002_pages.sql), each named
by its file name.

**Parameters**

- `$directory` `string`

**Returns** `array<string, string>`

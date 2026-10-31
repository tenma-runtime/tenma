---
title: "Password"
description: "Password hashes that don't hold up the worker: Argon2id on a background thread, while the other requests go on."
order: 100
generated: true
---

`Security\Password` · final class · namespace [`Security`](index.md)

```php
final class Password
```

Password hashes that don't hold up the worker: Argon2id on a background thread, while the other
requests go on. The hashes are those of password_hash() with PASSWORD_ARGON2ID, so existing ones keep
working and password_verify() takes the new ones. Outside a fiber (the script itself, a command) they
run in place, like password_hash().

## Methods

### hash()

```php
public static function hash(string $password): string
```

A new hash of $password with a random salt.

**Parameters**

- `$password` `string`

**Returns** `string`

### verify()

```php
public static function verify(string $password, string $hash): bool
```

Whether $password matches $hash. A hash of another algorithm (bcrypt) is checked by PHP in place.

**Parameters**

- `$password` `string`
- `$hash` `string`

**Returns** `bool`

### needsRehash()

```php
public static function needsRehash(string $hash): bool
```

Whether $hash should be made again with hash(): another algorithm or other costs.

**Parameters**

- `$hash` `string`

**Returns** `bool`

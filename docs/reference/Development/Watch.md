---
title: "Watch"
description: "What `tenma --watch` tells the script about its reloads."
order: 100
generated: true
---

`Development\Watch` · final class · namespace [`Development`](index.md)

```php
final class Watch
```

What `tenma --watch` tells the script about its reloads.

## Methods

### reloadedBy()

```php
public static function reloadedBy(): ?string
```

The file whose change made the script run again, null on the first run and without
--watch. A framework can tell from it whether only a part of the app changed.

**Returns** `?string`

### written()

```php
public static function written(string $file): void
```

Tells --watch that the script wrote $file itself, such as a template it compiled and loads:
that change reloads nothing. Call it after the file is written. Without --watch it does nothing.

**Parameters**

- `$file` `string`

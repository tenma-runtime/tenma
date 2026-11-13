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

### writing()

```php
public static function writing(string $file, string $content): void
```

Tells --watch that the script is about to write $content into $file itself, such as a template
it compiled and loads: that change reloads nothing. Call it before the write (or the rename
that puts the file in place). Without --watch it does nothing.

**Parameters**

- `$file` `string`
- `$content` `string`

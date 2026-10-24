---
title: "Problems"
description: "Problems an app or a build step reports to the editor through `tenma lsp`."
order: 100
generated: true
---

`Development\Problems` · final class · namespace [`Development`](index.md)

```php
final class Problems
```

Problems an app or a build step reports to the editor through `tenma lsp`.

## Methods

### report()

```php
public static function report(string $source, array $list): void
```

Replaces the problems $source reports: the editor shows them at their line (through `tenma lsp`), and new
ones go to stderr as `path:line:column: error: message`, the form problem matchers and quickfix read. An
empty list clears them. Only under --watch, which also clears the script's lists when it reloads it;
otherwise it does nothing. $source names the reporter: 'io-live', 'app:checks'. A relative file
counts from the working directory.

**Parameters**

- `$source` `string`
- `$list` `list<array{file: string, line: int, message: string, severity?: 'error'|'warning'|'info', column?: int}>`

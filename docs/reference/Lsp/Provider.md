---
title: "Provider"
description: "What a package adds to `tenma lsp`, the language server that runs next to the editor's own PHP server."
order: 100
generated: true
---

`Lsp\Provider` · interface · namespace [`Lsp`](index.md)

```php
interface Provider
```

What a package adds to `tenma lsp`, the language server that runs next to the editor's own
PHP server. A package names its provider in composer.json, `"extra": {"tenma": {"lsp":
"Io\\View\\Editor"}}`; tenma creates it with the project's directory, in a process that loads
the project's autoloader. Files are absolute paths, offsets and texts are bytes: `$text` is
the editor's content, saved or not.

## Methods

### thisClass()

```php
public function thisClass(string $file): ?string
```

The class `$this` is bound to in $file (a template, say), or null: tenma then completes `$this->`.

**Parameters**

- `$file` `string`

**Returns** `?string`

### completions()

```php
public function completions(string $file, string $text, int $offset): array
```

**Parameters**

- `$file` `string`
- `$text` `string`
- `$offset` `int`

**Returns** `list<array{label: string, kind?: 'method'|'property'|'class'|'value'|'keyword', detail?: string, insert?: string}>`

### definition()

```php
public function definition(string $file, string $text, int $offset): ?array
```

**Parameters**

- `$file` `string`
- `$text` `string`
- `$offset` `int`

**Returns** `array{file: string, line: int}|null`: Where the name at $offset is declared

### diagnostics()

```php
public function diagnostics(string $file, string $text): array
```

**Parameters**

- `$file` `string`
- `$text` `string`

**Returns** `list<array{line: int, message: string, severity?: 'error'|'warning'|'info'}>`: Lines count from 1

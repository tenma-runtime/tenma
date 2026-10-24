---
title: Language server
description: Run tenma lsp next to the editor's PHP server, and extend it from a package with Lsp\Provider, Lsp\Tokens, Lsp\Imports, Lsp\Variables and Lsp\Elements.
order: 330
---

`tenma lsp` is a language server (LSP over stdin and stdout). It answers what the editor's own PHP language server cannot know, such as the class `$this` stands for in a template. It runs next to that server (Intelephense or PHP Tools), not instead of it, and the editor merges the answers of both.

Out of the box it shows the problems a running app reports with [`Development\Problems::report()`](diagnostics.md). Packages add the rest through providers.

| Interface                                        | Adds                                                                        |
| ------------------------------------------------ | --------------------------------------------------------------------------- |
| [`Lsp\Provider`](../reference/Lsp/Provider.md)   | the class of `$this`, completions, go to definition, diagnostics (required) |
| [`Lsp\Tokens`](../reference/Lsp/Tokens.md)       | semantic highlighting for syntax the editor's grammar does not know         |
| [`Lsp\Imports`](../reference/Lsp/Imports.md)     | the `use` imports and namespace a file sees without declaring them          |
| [`Lsp\Variables`](../reference/Lsp/Variables.md) | the classes of variables other than `$this`                                 |
| [`Lsp\Elements`](../reference/Lsp/Elements.md)   | the element classes of array properties and methods without a PHPDoc type   |

`Tokens`, `Imports`, `Variables` and `Elements` are optional. A class implements them next to `Provider`.

## Editor setup

**VS Code and VSCodium**: the [Tenma-VSCode](https://github.com/tenma-runtime/vscode) extension starts `tenma lsp` for PHP files in trusted workspaces.

| Setting            | Default | Does                            |
| ------------------ | ------- | ------------------------------- |
| `tenma.lsp.enable` | `true`  | starts `tenma lsp`              |
| `tenma.executable` | `tenma` | the binary, found on the `PATH` |

**Neovim** (0.11 or later):

```lua
vim.lsp.config('tenma', {
  cmd = { 'tenma', 'lsp' },
  filetypes = { 'php' },
  root_markers = { 'composer.json' },
})
vim.lsp.enable('tenma')
```

**Other editors**: run `tenma lsp` as an additional language server for PHP files.

## Registering a provider

A package names its provider class in its `composer.json`, under `extra.tenma.lsp`. The project itself can do the same:

```json
{
  "name": "acme/views",
  "autoload": { "psr-4": { "Acme\\Views\\": "src/" } },
  "extra": { "tenma": { "lsp": "Acme\\Views\\Editor" } }
}
```

- Providers come from the project's own `composer.json` and from every installed package.
- `extra.tenma.lsp` may also be a list, when a package has several providers.
- Tenma creates each provider with the project's directory as the only constructor argument.
- The methods get absolute file paths. `$text` is the editor's content, saved or not.
- Offsets, columns and lengths count bytes. Lines count from 1.

## Provider

[`Lsp\Provider`](../reference/Lsp/Provider.md) is the interface every provider implements.

| Method                               | Returns                                                                                         |
| ------------------------------------ | ----------------------------------------------------------------------------------------------- |
| `thisClass($file)`                   | the class `$this` is bound to in a non-class file (a template), or `null`                       |
| `completions($file, $text, $offset)` | a list of `['label' => …, 'kind' => …, 'detail' => …, 'insert' => …]`; all but `label` optional |
| `definition($file, $text, $offset)`  | `['file' => …, 'line' => …]` where the name at `$offset` is declared, or `null`                 |
| `diagnostics($file, $text)`          | a list of `['line' => …, 'message' => …, 'severity' => …]`                                      |

`kind` is one of `method`, `property`, `class`, `value` or `keyword`. `severity` is `error`, `warning` or `info`.

This provider binds templates such as `src/Components/Menu.Template.php` to the class `App\Components\Menu`:

```php
<?php

namespace App;

use Lsp\Provider;

final class Editor implements Provider
{
    public function __construct(private string $root) {}

    public function thisClass(string $file): ?string
    {
        $folder = "$this->root/src/Components/";
        if (!str_starts_with($file, $folder) || !str_ends_with($file, '.Template.php')) {
            return null;
        }

        $class = 'App\\Components\\' . basename($file, '.Template.php');

        return class_exists($class) ? $class : null;
    }

    public function completions(string $file, string $text, int $offset): array
    {
        return [];
    }

    public function definition(string $file, string $text, int $offset): ?array
    {
        return null;
    }

    public function diagnostics(string $file, string $text): array
    {
        return [];
    }
}
```

Once `thisClass()` names a class, Tenma does the rest by reflection:

- It completes `$this->` and `$this->prop->`, private members included, since the template runs bound to the object.
- It shows type and doc comment on hover, and F12 jumps to the declaration.
- `foreach ($this->items as $item)` (also `@foreach`) gives `$item` the element type from the PHPDoc of `items`: `list<Item>`, `array<int, Item>` or `Item[]`.
- An assigned variable gets the type of what is assigned. `$item = $this->first();` takes the method's return type, `$item = $this->item;` the property's type, `$item = new Item();` the class.
- The last assignment or loop above the cursor counts, not one in a comment or string. F12 on the variable leads to it.
- This works in plain `<?php … ?>` blocks as well as in a template syntax's own blocks.

## Tokens

[`Lsp\Tokens`](../reference/Lsp/Tokens.md) colours what the editor's grammar does not know, such as template directives. `tokens($file, $text)` returns a list of `['line' => …, 'column' => …, 'length' => …, 'type' => …]`.

```php
public function tokens(string $file, string $text): array
{
    $directive = '/@(?:if|else|endif|foreach|endforeach)\b/';
    $tokens = [];
    foreach (explode("\n", $text) as $index => $line) {
        preg_match_all($directive, $line, $found, PREG_OFFSET_CAPTURE);
        foreach ($found[0] as [$word, $column]) {
            $tokens[] = [
                'line' => $index + 1,
                'column' => $column,
                'length' => strlen($word),
                'type' => 'keyword',
            ];
        }
    }

    return $tokens;
}
```

- `column` is the byte offset within the line, from 0.
- Types: `keyword`, `comment`, `variable`, `property`, `method`, `function`, `class`, `string`, `number`, `operator`.
- Tokens may come in any order. Of two that overlap, the one that starts first stays.

## Imports

A template has no `use` statements of its own, so `Status::` in it would complete nothing. [`Lsp\Imports`](../reference/Lsp/Imports.md) names the imports a file sees. `imports($file, $text)` returns alias => fully qualified class name, and the namespace under the key `''`.

```php
public function imports(string $file, string $text): array
{
    // Menu.Template.php sees the namespace and imports of Menu.php
    return [
        '' => 'App\\Components',
        'Status' => 'App\\Models\\Status',
    ];
}
```

Tenma resolves every class name in the file with them:

- completion after `Name::` (cases, constants, static properties and methods),
- hover and F12 on a class name before `::`, after `new` or after `instanceof`,
- the types of loop and assigned variables (`new Item()`),
- the `@var` and `@return` types it reads from doc comments.

The file's own `use` statements and `namespace` come first. The provider fills in only what the file does not have: an alias the file imports itself keeps the file's class, and a file without a `namespace` line takes the provider's.

## Variables

A template often gets more than `$this`, such as a model as `$model`. [`Lsp\Variables`](../reference/Lsp/Variables.md) names the class of such variables. `variables($file, $text)` returns variable name (without `$`) => fully qualified class name.

```php
public function variables(string $file, string $text): array
{
    // views/users/show.php renders a UserPage as $model
    if (!str_ends_with($file, '/views/users/show.php')) {
        return [];
    }

    return ['model' => \App\Models\UserPage::class];
}
```

- Tenma treats such a variable as it treats `$this`, with public members only: completion of `$model->` and `$model->user->`, hover, F12, loop and assigned variables.
- A class that does not exist is left out.
- A file a provider types variables in counts as bound, even without `thisClass()`.
- A `this` entry stands in for `thisClass()` when that returns `null`.
- Below an assignment or a loop in the file that introduces the same name, the file's own typing wins. Above it, the provider's wins.

## Elements

Tenma reads the element type of an array member from its PHPDoc (`@var list<Line>`, `@return Line[]`). A package that documents it another way, with attributes for example, tells Tenma through [`Lsp\Elements`](../reference/Lsp/Elements.md).

`elements($class)` returns member => fully qualified class of its elements. A property is named by its name, a method by its name followed by `()`. Here an attribute `#[ListOf(Line::class)]` names the class:

```php
public function elements(string $class): array
{
    $elements = [];
    foreach (new \ReflectionClass($class)->getProperties() as $property) {
        foreach ($property->getAttributes(\App\ListOf::class) as $listOf) {
            $elements[$property->getName()] = $listOf->newInstance()->type;
        }
    }

    // int, string, …: no class to complete
    return array_filter($elements, fn (string $type) => class_exists($type));
}
```

- Tenma then types `$line` in `foreach ($order->lines as $line)` and `@foreach ($order->lines() as $line)`, also at the end of a chain (`$this->order->lines`).
- A class that does not exist is left out.
- The member's own `@var` or `@return` PHPDoc wins when it names a class. The providers fill in where it names none.
- Tenma asks every provider that implements `Elements`, whatever file is open: an element type belongs to the class.

## Behaviour

- Tenma runs two processes. The server keeps the open documents. A worker per project (`tenma lsp --worker DIR`) loads the project's `vendor/autoload.php` and its providers, and answers by reflection.
- A file belongs to the nearest directory with a `composer.json` above it, so one editor window can hold several projects.
- Loading the autoloader runs the project's code. Start `tenma lsp` only in projects you trust.
- PHP cannot load a changed class a second time. Saving a `.php` file therefore ends the worker, and the next question starts a new one: a class shows its new members once it is saved.
- `Elements` answers are asked once per class and kept until the worker restarts.
- A provider that throws is logged on stderr and left out. One that takes longer than 5 seconds ends the worker.
- When several providers name the same alias, variable or member, the first one wins.

See also: [`Lsp\Provider`](../reference/Lsp/Provider.md), [Diagnostics](diagnostics.md), [Debugger](debugger.md).

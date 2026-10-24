<?php

namespace Tenma\Internal\Lsp;

use Lsp\Provider;

final class Server
{
    /** @var array<string, string> uri => text of the open documents */
    private array $documents = [];

    /** @var array<string, Worker> project directory => its worker */
    private array $workers = [];

    /** @var array<string, ?string> directory => the project it belongs to */
    private array $projects = [];

    /** @var array<string, list<array>> uri => the providers' diagnostics of an open document */
    private array $checked = [];

    /** @var array<string, array<string, list<array>>> project => path => what the running app published there */
    private array $published = [];

    /**
     * @var array<string, string> path => the URI the client spells it with. Clients encode differently (Neovim keeps
     * @ and + as they are): a URI of our own would name a second document.
     */
    private array $uris = [];

    /** @var array<string, string> project => its lists as they were last read: names and hashes */
    private array $lists = [];

    /** @param resource $out */
    public function __construct(private readonly string $binary, private $out) {}

    /**
     * Reads again the lists Problems::report() keeps in the projects of the open documents, if they changed,
     * and publishes the files whose problems changed.
     */
    public function tick(): void
    {
        foreach (array_unique(array_filter($this->projects)) as $project) {
            $directory = \Tenma\Internal\Diagnostics::directory($project);
            $files = glob("$directory/*.json") ?: [];
            // By content: a rewrite within the same second with the same size is a change too
            $state = implode('|', array_map(static fn (string $f) => $f . ':' . md5((string) @file_get_contents($f)), $files));
            if (($this->lists[$project] ?? '') === $state) {
                continue;
            }
            $this->lists[$project] = $state;
            $before = $this->published[$project] ?? [];
            $now = [];
            foreach ($files as $file) {
                // app.io-live.json: published by the script, as 'io-live'
                $source = (string) preg_replace('/^(app|dev)\.|\.json$/', '', basename($file));
                $list = json_decode((string) @file_get_contents($file), true);
                foreach (is_array($list) ? $list : [] as $problem) {
                    if (is_array($problem) && is_string($problem['file'] ?? null)) {
                        $now[$problem['file']][] = ['source' => $source] + $problem;
                    }
                }
            }
            $this->published[$project] = $now;
            foreach (array_keys($before + $now) as $path) {
                if (($before[$path] ?? null) !== ($now[$path] ?? null)) {
                    $this->publish($this->uris[$path] ?? uri((string) $path));
                }
            }
        }
    }

    /** The providers' diagnostics of a document and what the app published for its file, together. */
    private function publish(string $uri): void
    {
        $diagnostics = $this->checked[$uri] ?? [];
        $file = path($uri);
        foreach ($this->published as $files) {
            foreach ($files[$file ?? ''] ?? [] as $problem) {
                $text = $this->documents[$uri] ?? ($file !== null && is_file($file) ? (string) file_get_contents($file) : '');
                $line = max(0, (int) ($problem['line'] ?? 1) - 1);
                // An app that checks what a provider checks too: once is enough, the provider's follows the typing
                foreach ($this->checked[$uri] ?? [] as $own) {
                    if ($own['range']['start']['line'] === $line && $own['message'] === (string) ($problem['message'] ?? '')) {
                        continue 2;
                    }
                }
                // The column counts bytes, as everywhere in the Provider API; past the end it is the end
                $lineText = rtrim(explode("\n", $text)[$line] ?? '', "\r");
                $character = utf16(substr($lineText, 0, max(0, (int) ($problem['column'] ?? 1) - 1)));
                $diagnostics[] = [
                    'range' => ['start' => ['line' => $line, 'character' => $character], 'end' => ['line' => $line, 'character' => utf16($lineText)]],
                    'severity' => match ($problem['severity'] ?? 'error') {
                        'warning' => 2, 'info' => 3, default => 1,
                    },
                    'source' => (string) $problem['source'],
                    'message' => (string) ($problem['message'] ?? ''),
                ];
            }
        }
        $this->send(['jsonrpc' => '2.0', 'method' => 'textDocument/publishDiagnostics', 'params' => ['uri' => $uri, 'diagnostics' => $diagnostics]]);
    }

    /** False once the client says exit. */
    public function handle(array $message): bool
    {
        $method = $message['method'] ?? null;
        $params = is_array($message['params'] ?? null) ? $message['params'] : [];
        $id = $message['id'] ?? null;
        if (!is_string($method)) {
            return true;
        }
        try {
            $result = match ($method) {
                'initialize' => $this->initialize($params),
                'shutdown' => null,
                'exit' => false,
                'textDocument/didOpen' => $this->open($params['textDocument']['uri'] ?? '', $params['textDocument']['text'] ?? ''),
                'textDocument/didChange' => $this->open($params['textDocument']['uri'] ?? '', end($params['contentChanges'])['text'] ?? ''),
                'textDocument/didSave' => $this->saved($params['textDocument']['uri'] ?? '', $params['text'] ?? null),
                'textDocument/didClose' => $this->close($params['textDocument']['uri'] ?? ''),
                'textDocument/completion' => $this->completion($params),
                'textDocument/hover' => $this->hover($params),
                'textDocument/definition' => $this->definition($params),
                'textDocument/semanticTokens/full' => $this->tokens($params),
                default => new \LogicException("unknown method $method"),
            };
        } catch (\Throwable $e) {
            log("$method: " . $e->getMessage());
            $result = $e;
        }
        if ($result === false) {
            return false;
        }
        if ($id !== null) {
            $this->send(['jsonrpc' => '2.0', 'id' => $id] + ($result instanceof \Throwable
                ? ['error' => ['code' => $result instanceof \LogicException ? -32601 : -32603, 'message' => $result->getMessage()]]
                : ['result' => $result]));
        }
        return true;
    }

    public function stopWorkers(): void
    {
        foreach ($this->workers as $worker) {
            $worker->stop();
        }
        $this->workers = [];
    }

    private function send(array $message): void
    {
        $body = encode($message);
        fwrite($this->out, 'Content-Length: ' . strlen($body) . "\r\n\r\n" . $body);
        fflush($this->out);
    }

    private function initialize(array $params): array
    {
        return [
            'capabilities' => [
                // The whole text on every change: templates are small, and nothing can drift
                'textDocumentSync' => ['openClose' => true, 'change' => 1, 'save' => ['includeText' => false]],
                'completionProvider' => ['triggerCharacters' => ['>', '"', '<', ' ', '-']],
                'hoverProvider' => true,
                'definitionProvider' => true,
                'semanticTokensProvider' => ['legend' => ['tokenTypes' => TOKEN_TYPES, 'tokenModifiers' => []], 'full' => true],
            ],
            'serverInfo' => ['name' => 'tenma', 'version' => VERSION],
        ];
    }

    /** Asks the worker of the project $file belongs to; null outside a project. */
    private function ask(string $method, string $file, mixed ...$args): mixed
    {
        $root = $this->project(dirname($file));
        if ($root === null) {
            return null;
        }
        $worker = $this->workers[$root] ??= new Worker($this->binary, $root);
        $answer = $worker->ask($method, [$file, ...$args]);
        if (!$worker->alive()) {
            $worker->stop();
            unset($this->workers[$root]);
        }
        return $answer;
    }

    /** The nearest directory with a composer.json, $directory included. */
    private function project(string $directory): ?string
    {
        if (array_key_exists($directory, $this->projects)) {
            return $this->projects[$directory];
        }
        $parent = dirname($directory);
        return $this->projects[$directory] = is_file("$directory/composer.json") ? $directory : ($parent !== $directory ? $this->project($parent) : null);
    }

    private function open(string $uri, string $text): null
    {
        $this->documents[$uri] = $text;
        $path = path($uri);
        if ($path !== null) {
            $this->uris[$path] = $uri;
        }
        $this->diagnose($uri);
        return null;
    }

    private function saved(string $uri, ?string $text): null
    {
        if ($text !== null) {
            $this->documents[$uri] = $text;
        }
        // A class may have changed, in this project or in one that uses it: only new workers see it
        if (str_ends_with($uri, '.php')) {
            $this->stopWorkers();
            foreach (array_keys($this->documents) as $open) {
                $this->diagnose($open);
            }
        }
        return null;
    }

    private function close(string $uri): null
    {
        unset($this->documents[$uri], $this->checked[$uri]);
        // What the app published stays until the app takes it back
        $this->publish($uri);
        return null;
    }

    private function diagnose(string $uri): void
    {
        $file = path($uri);
        $text = $this->documents[$uri] ?? null;
        if ($file === null || $text === null) {
            return;
        }
        $diagnostics = [];
        foreach ((array) $this->ask('diagnostics', $file, $text) as $problem) {
            $line = max(0, (int) ($problem['line'] ?? 1) - 1);
            $diagnostics[] = [
                'range' => ['start' => ['line' => $line, 'character' => 0], 'end' => ['line' => $line, 'character' => lineLength($text, $line)]],
                'severity' => match ($problem['severity'] ?? 'error') {
                    'warning' => 2, 'info' => 3, default => 1,
                },
                'source' => 'tenma',
                'message' => (string) ($problem['message'] ?? ''),
            ];
        }
        $this->checked[$uri] = $diagnostics;
        $this->publish($uri);
    }

    /** @return array{string, string, int}|null file, text, byte offset of the position */
    private function position(array $params): ?array
    {
        $uri = $params['textDocument']['uri'] ?? '';
        $file = path($uri);
        $text = $this->documents[$uri] ?? ($file !== null && is_file($file) ? file_get_contents($file) : null);
        if ($file === null || !is_string($text)) {
            return null;
        }
        return [$file, $text, offset($text, (int) ($params['position']['line'] ?? 0), (int) ($params['position']['character'] ?? 0))];
    }

    private function completion(array $params): array
    {
        $at = $this->position($params);
        $items = [];
        foreach ($at === null ? [] : (array) $this->ask('completions', ...$at) as $item) {
            $items[] = array_filter([
                'label' => (string) ($item['label'] ?? ''),
                'kind' => match ($item['kind'] ?? null) {
                    'method' => 2, 'property' => 10, 'class' => 7, 'keyword' => 14, default => 12,
                },
                'detail' => $item['detail'] ?? null,
                'insertText' => $item['insert'] ?? null,
            ], static fn (mixed $value): bool => $value !== null);
        }
        return ['isIncomplete' => false, 'items' => $items];
    }

    private function hover(array $params): ?array
    {
        $at = $this->position($params);
        $markdown = $at === null ? null : $this->ask('hover', ...$at);
        return is_string($markdown) ? ['contents' => ['kind' => 'markdown', 'value' => $markdown]] : null;
    }

    /** The providers' tokens, encoded as LSP wants them: relative positions in UTF-16 units. */
    private function tokens(array $params): array
    {
        $at = $this->position($params + ['position' => ['line' => 0, 'character' => 0]]);
        $tokens = $at === null ? [] : (array) $this->ask('tokens', $at[0], $at[1]);
        usort($tokens, static fn (array $a, array $b) => [$a['line'] ?? 0, $a['column'] ?? 0] <=> [$b['line'] ?? 0, $b['column'] ?? 0]);
        $lines = $at === null ? [] : explode("\n", $at[1]);
        $data = [];
        [$line, $start, $end] = [0, 0, -1];
        foreach ($tokens as $token) {
            $type = array_search($token['type'] ?? '', TOKEN_TYPES, true);
            $text = $lines[(int) ($token['line'] ?? 0) - 1] ?? null;
            if ($type === false || $text === null) {
                continue;
            }
            $at16 = utf16(substr($text, 0, (int) ($token['column'] ?? 0)));
            $length = utf16(substr($text, (int) ($token['column'] ?? 0), (int) ($token['length'] ?? 0)));
            $tokenLine = (int) $token['line'] - 1;
            // Overlapping tokens: the first one wins
            if ($length === 0 || ($tokenLine === $line && $at16 < $end)) {
                continue;
            }
            array_push($data, $tokenLine - $line, $tokenLine === $line ? $at16 - $start : $at16, $length, $type, 0);
            [$line, $start, $end] = [$tokenLine, $at16, $at16 + $length];
        }
        return ['data' => $data];
    }

    private function definition(array $params): ?array
    {
        $at = $this->position($params);
        $place = $at === null ? null : $this->ask('definition', ...$at);
        if (!is_array($place) || !is_string($place['file'] ?? null)) {
            return null;
        }
        $line = max(0, (int) ($place['line'] ?? 1) - 1);
        return ['uri' => uri($place['file']), 'range' => ['start' => ['line' => $line, 'character' => 0], 'end' => ['line' => $line, 'character' => 0]]];
    }
}

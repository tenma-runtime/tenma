<?php
// For tests/http.rs: sessions in classic mode, one counter per visitor. The save handler keeps the data
// in memory; after /break its write() fails, and the error handler turns the warning into an exception
class Store implements SessionHandlerInterface
{
    public static array $data = [];

    public static bool $fail = false;

    public function open(string $path, string $name): bool
    {
        return true;
    }

    public function close(): bool
    {
        return true;
    }

    public function read(string $id): string|false
    {
        return self::$data[$id] ?? '';
    }

    public function write(string $id, string $data): bool
    {
        if (self::$fail) {
            return false;
        }
        self::$data[$id] = $data;
        return true;
    }

    public function destroy(string $id): bool
    {
        unset(self::$data[$id]);
        return true;
    }

    public function gc(int $max_lifetime): int|false
    {
        return 0;
    }
}
set_error_handler(function (int $level, string $message, string $file, int $line) {
    throw new ErrorException($message, 0, $level, $file, $line);
});
set_exception_handler(function (Throwable $error) {
    error_log('handled ' . $error->getMessage());
});

(new \Http\ClassicServer((int) getenv('PORT'), function () {
    $path = parse_url($_SERVER['REQUEST_URI'], PHP_URL_PATH);
    if ($path === '/peek') {
        // Without session_start() nothing from the previous visitor may be here
        echo isset($_SESSION) ? 'leak' : 'empty';
        return;
    }
    // Inside the request: at the top level PHP refuses a save handler, as the headers count as sent. Without
    // the shutdown function, the session is written at the end of the request, where Tenma closes it
    session_set_save_handler(new Store(), false);
    Store::$fail = $path === '/break';
    session_start();
    $_SESSION['count'] = ($_SESSION['count'] ?? 0) + 1;
    echo "visit {$_SESSION['count']}";
}))->start();

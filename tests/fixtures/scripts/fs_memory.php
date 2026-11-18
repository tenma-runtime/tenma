<?php
// For tests/http.rs: a file larger than memory_limit is refused before it is read
\Threading\Task::run(function () {
    $path = sys_get_temp_dir() . '/tenma-fs-memory-' . getmypid();
    $handle = fopen($path, 'w');
    ftruncate($handle, 64 << 20);
    fclose($handle);
    try {
        (new FileSystem\FileInfo($path))->read();
        echo "read\n";
    } catch (RuntimeException $error) {
        echo str_contains($error->getMessage(), 'memory_limit') ? "refused\n" : $error->getMessage();
    } finally {
        unlink($path);
    }
});

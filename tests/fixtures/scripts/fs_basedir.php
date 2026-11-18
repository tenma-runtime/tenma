<?php
// For tests/http.rs: open_basedir also applies to FileSystem\FileInfo and to the target of a download
\Threading\Task::run(function () {
    try {
        (new FileSystem\FileInfo('/etc/passwd'))->read();
        echo "read\n";
    } catch (RuntimeException $error) {
        echo str_contains($error->getMessage(), 'open_basedir') ? "forbidden\n" : $error->getMessage();
    }
    try {
        (new Http\Client(1.0))->download('http://127.0.0.1:9/', sys_get_temp_dir() . '/tenma-basedir-download');
        echo "downloaded\n";
    } catch (RuntimeException $error) {
        echo str_contains($error->getMessage(), 'open_basedir') ? "forbidden\n" : $error->getMessage();
    }
});

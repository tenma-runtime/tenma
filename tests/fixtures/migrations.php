<?php
// For tests/http.rs: steps apply once and in order, a failing run leaves nothing behind. Prints "no postgres" without one.
use Data\{Migrations, Postgres};

\Threading\Task::run(function () {
    $db = Postgres::connect(getenv('DATABASE_URL') ?: 'postgres://localhost/postgres');
    try {
        $db->query('SELECT 1');
    } catch (\Throwable) {
        echo "no postgres\n";
        return;
    }
    $table = 'migrations_test_' . getmypid();
    $db->script("DROP TABLE IF EXISTS {$table}_items");
    $migrations = new Migrations($db, $table);
    $steps = [
        '001_items' => "CREATE TABLE {$table}_items (id int)",
        '002_one' => fn (Postgres $tx) => $tx->execute("INSERT INTO {$table}_items VALUES (1)"),
    ];
    echo implode(',', $migrations->apply($steps)), "\n";
    echo implode(',', $migrations->apply($steps)) === '' ? "again nothing\n" : "again ran\n";
    try {
        $migrations->apply($steps + ['003_two' => fn (Postgres $tx) => $tx->execute("INSERT INTO {$table}_items VALUES (2)"), '004_broken' => 'SELECT * FROM nowhere_at_all']);
    } catch (\Throwable) {
        echo "broken step failed\n";
    }
    echo 'rows ', $db->first("SELECT count(*) AS n FROM {$table}_items")['n'] ?? '?', "\n";
    $db->script("DROP TABLE {$table}_items; DROP TABLE $table");
});

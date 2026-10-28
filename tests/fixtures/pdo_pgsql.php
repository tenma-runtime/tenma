<?php
// For tests/http.rs: PDO with pgsql: DSNs talks to tokio-postgres (shim/tenma.c, src/pdo.rs): in a fiber
// only the fiber waits, at the top level the script blocks. Needs a running Postgres (DATABASE_URL).
use Http\{Request, Response, Server};
use Threading\Task;

$url = parse_url((string) getenv('DATABASE_URL'));
$dsn = sprintf('pgsql:host=%s;port=%d;dbname=%s', $url['host'] ?? 'localhost', $url['port'] ?? 5432, ltrim($url['path'] ?? '/postgres', '/'));
$user = $url['user'] ?? getenv('USER');
$password = $url['pass'] ?? '';
$connect = fn (array $options = []) => new PDO($dsn, $user, $password, [PDO::ATTR_ERRMODE => PDO::ERRMODE_EXCEPTION] + $options);

// At the top level: the connection and the query block, as in PHP; without a database the server
// still starts, so the test can tell and skip
try {
    $pdo = $connect();
    echo $pdo->getAttribute(PDO::ATTR_DRIVER_NAME), ' ', $pdo->query('SELECT 1 + 1')->fetchColumn(), "\n";
} catch (PDOException $error) {
    echo "no database\n";
}

new Server((int) getenv('PORT'), function (Request $request) use ($connect) {
    $started = microtime(true);
    $seconds = fn () => round(microtime(true) - $started, 1) . 's';
    switch ($request->path) {
        case '/setup':
            $pdo = $connect();
            $pdo->exec('DROP TABLE IF EXISTS tenma_pdo; CREATE TABLE tenma_pdo (id serial PRIMARY KEY, name text, amount numeric(8,2), flag bool, data bytea, tags text[])');
            return "ready\n";
        case '/parallel':
            // Two queries of half a second at once, each on its own connection
            $sleep = fn (int $n) => fn () => $connect()->query("SELECT pg_sleep(0.5), $n AS n")->fetchColumn(1);
            $numbers = Task::all($sleep(1), $sleep(2));
            return implode(',', $numbers) . ' ' . $seconds() . "\n";
        case '/insert':
            $pdo = $connect();
            $pdo->beginTransaction();
            $statement = $pdo->prepare('INSERT INTO tenma_pdo (name, amount, flag, data, tags) VALUES (:name, :amount, :flag, :data, :tags) RETURNING id');
            $statement->bindValue(':name', $request->query['name'] ?? '');
            $statement->bindValue(':amount', 9.99);
            $statement->bindValue(':flag', true, PDO::PARAM_BOOL);
            $statement->bindValue(':data', "\x00\x01hi", PDO::PARAM_LOB);
            $statement->bindValue(':tags', '{a,b}');
            $statement->execute();
            $id = $statement->fetchColumn();
            $last = $pdo->lastInsertId('tenma_pdo_id_seq');
            $pdo->commit();
            $row = $pdo->query('SELECT * FROM tenma_pdo WHERE id = ' . (int) $id)->fetch(PDO::FETCH_ASSOC);
            $row['data'] = bin2hex(stream_get_contents($row['data']));
            $row['same'] = (string) $id === $last && $pdo->inTransaction() === false;
            return Response::json($row);
        case '/rollback':
            $pdo = $connect();
            $pdo->beginTransaction();
            $pdo->exec("UPDATE tenma_pdo SET name = 'changed'");
            $pdo->rollBack();
            return $pdo->query('SELECT name FROM tenma_pdo ORDER BY id LIMIT 1')->fetchColumn() . "\n";
        case '/error':
            try {
                $connect()->query('SELECT nope');
                return "no error\n";
            } catch (PDOException $error) {
                return $error->getCode() . ' ' . $error->errorInfo[2] . "\n";
            }
        case '/placeholders':
            $pdo = $connect();
            $positional = $pdo->prepare('SELECT ?::text AS a, ?::int AS b');
            $positional->execute(['x', 5]);
            $emulated = $connect([PDO::ATTR_EMULATE_PREPARES => true])->prepare('SELECT :a AS a, :b::int AS b');
            $emulated->execute(['a' => "q'1", 'b' => 7]);
            $meta = $positional->getColumnMeta(1);
            return Response::json([$positional->fetch(PDO::FETCH_NUM), $emulated->fetch(PDO::FETCH_ASSOC), $meta['native_type'], $pdo->quote("it's")]);
        case '/numbers':
            // Numbers sit in the packed result buffer right before the next value's length: a 50-byte text after an
            // int, a 53-byte text after a float, and a 52-byte text at the start of the next row must not add digits
            $rows = $connect()->query("SELECT 12 AS a, repeat('x', 50) AS b, 1.5::float8 AS c, repeat('y', 53) AS d, 7 AS e UNION ALL SELECT 3, repeat('q', 52), 2.25, '', 9 ORDER BY 1 DESC")->fetchAll(PDO::FETCH_ASSOC);
            return implode('|', array_map(fn (array $row) => "$row[a] $row[c] $row[e]", $rows)) . "\n";
        case '/scroll':
            $statement = $connect()->prepare('SELECT generate_series(1, 5) AS n', [PDO::ATTR_CURSOR => PDO::CURSOR_SCROLL]);
            $statement->execute();
            $last = $statement->fetch(PDO::FETCH_COLUMN, PDO::FETCH_ORI_LAST);
            $third = $statement->fetch(PDO::FETCH_COLUMN, PDO::FETCH_ORI_ABS, 3);
            $prior = $statement->fetch(PDO::FETCH_COLUMN, PDO::FETCH_ORI_PRIOR);
            return "$last $third $prior " . $connect()->query('SHOW client_encoding')->fetchColumn() . "\n";
        case '/modifying-with':
            // A WITH that modifies data cannot stand inside the driver's wrapper: rows come through a temporary table
            $pdo = $connect();
            $statement = $pdo->prepare('WITH inserted AS (INSERT INTO tenma_pdo (name, amount) VALUES (?, 1) RETURNING id, name) SELECT name, (SELECT count(*) FROM tenma_pdo WHERE name = ?) AS n FROM inserted');
            $statement->execute(['Pie', 'Pie']);
            $statement->execute(['Pie', 'Pie']);
            $row = $statement->fetch(PDO::FETCH_ASSOC);
            return $row['name'] . ' ' . $row['n'] . "\n";
        case '/cancelled':
            // The client leaves while the query runs: the fiber is cancelled where it waits
            try {
                $connect()->query('SELECT pg_sleep(30)');
                return "done\n";
            } catch (Threading\Cancelled $cancelled) {
                file_put_contents(getenv('CANCEL_LOG'), "cancelled in query\n");
                throw $cancelled;
            }
    }
    return "ok\n";
})->start();

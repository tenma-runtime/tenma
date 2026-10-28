<?php

namespace Data;

/**
 * Brings a database up to date: named steps, each applied once and in the order given. All pending steps run
 * in one transaction under an advisory lock, so workers or processes starting together apply each step once,
 * and a failing step leaves the database as it was. A step is SQL or a function that gets the transaction.
 *
 * ```php
 * $ran = new Migrations($db)->apply([
 *     '001_users' => 'CREATE TABLE users (id bigserial PRIMARY KEY, email text NOT NULL)',
 *     '002_names' => fn (Postgres $tx) => $tx->execute('ALTER TABLE users ADD COLUMN name text'),
 *     ...Migrations::files(__DIR__ . '/migrations'),
 * ]);
 * ```
 */
final class Migrations
{
    /** @param string $table Where the applied names are kept; its name also picks the lock */
    public function __construct(private readonly Postgres $db, private readonly string $table = 'migrations')
    {
        if (preg_match('/^[a-z_][a-z0-9_]*$/', $table) !== 1) {
            throw new \InvalidArgumentException("Migrations: $table is no plain table name");
        }
    }

    /**
     * Applies the steps not applied yet; returns their names. Only in asynchronous code: at script startup,
     * inside Task::run().
     *
     * @param array<string, string|callable(Postgres): mixed> $steps name => SQL or function
     * @return list<string>
     */
    public function apply(array $steps): array
    {
        return $this->db->transaction(function (Postgres $tx) use ($steps): array {
            $tx->execute('SELECT pg_advisory_xact_lock($1)', [crc32("tenma-migrations:$this->table")]);
            $tx->script("CREATE TABLE IF NOT EXISTS $this->table (
                name text PRIMARY KEY,
                applied_at timestamptz NOT NULL DEFAULT now()
            )");
            $done = array_flip(array_column($tx->query("SELECT name FROM $this->table"), 'name'));
            $ran = [];
            foreach ($steps as $name => $step) {
                $name = (string) $name;
                if (isset($done[$name])) {
                    continue;
                }
                is_string($step) ? $tx->script($step) : $step($tx);
                $tx->execute("INSERT INTO $this->table (name) VALUES ($1)", [$name]);
                $ran[] = $name;
            }
            return $ran;
        });
    }

    /**
     * The *.sql files of a directory as steps, in name order (0001_users.sql, 0002_pages.sql), each named
     * by its file name.
     *
     * @return array<string, string>
     */
    public static function files(string $directory): array
    {
        $files = glob(rtrim($directory, '/') . '/*.sql') ?: [];
        sort($files);
        $steps = [];
        foreach ($files as $file) {
            $steps[basename($file)] = (string) file_get_contents($file);
        }
        return $steps;
    }
}

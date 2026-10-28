<?php

namespace Data;

/**
 * Postgres, asynchronous: while a query runs, only this request's Fiber waits.
 * Placeholders $1, $2, … as in Postgres; parameters are sent as text, the server determines the type.
 * A PHP list for an array parameter (`$1::text[]`, `= ANY ($1::int[])`) goes as an array, quoted and
 * escaped; for json or jsonb it stays JSON.
 * Only in asynchronous code (handlers, tasks, timers); at script startup, wrap queries in Task::run().
 */
final class Postgres
{
    /** The last transaction ID of this interpreter. */
    private static int $transactions = 0;

    private function __construct(private readonly string $target) {}

    /** @param string $url postgres://user:password@host:5432/database */
    public static function connect(string $url): self
    {
        return new self($url);
    }

    /**
     * Rows as a list of associative arrays, values with Postgres' JSON types; with $as each row goes through it
     * instead: `as: Row::of(...)` for typed getters, `as: Order::from(...)` for objects,
     * `as: fn (array $row) => (string) $row['name']` for one column. SHOW, EXPLAIN and FETCH take no
     * parameters and return every value as a string.
     *
     * @template T
     * @param list<mixed> $params
     * @param (callable(array<string, mixed>): T)|null $as
     * @return ($as is null ? list<array<string, mixed>> : list<T>)
     */
    public function query(string $sql, array $params = [], ?callable $as = null): array
    {
        $rows = $this->call('query', $sql, $params);
        return $as === null ? $rows : array_map($as, $rows);
    }

    /**
     * The first row, or null; with $as it goes through $as as in query().
     *
     * @template T
     * @param list<mixed> $params
     * @param (callable(array<string, mixed>): T)|null $as
     * @return ($as is null ? array<string, mixed>|null : T|null)
     */
    public function first(string $sql, array $params = [], ?callable $as = null): mixed
    {
        $row = $this->query($sql, $params)[0] ?? null;
        return $row === null || $as === null ? $row : $as($row);
    }

    /**
     * Several statements separated by semicolons, without parameters, such as a migration file. They run as
     * one: a failing statement undoes the ones before it, inside transaction() as part of that transaction.
     * Therefore no COMMIT or ROLLBACK in it, and nothing that refuses to run in a transaction (CREATE INDEX
     * CONCURRENTLY, VACUUM, CREATE DATABASE). Never build it from input: without parameters, anything in it
     * is SQL.
     */
    public function script(string $sql): void
    {
        $this->call('script', $sql, []);
    }

    /**
     * For statements without a result; returns the number of affected rows.
     *
     * @param list<mixed> $params
     */
    public function execute(string $sql, array $params = []): int
    {
        return $this->call('execute', $sql, $params);
    }

    /**
     * Runs $work in a transaction: commit if $work completes, otherwise rollback.
     *
     * @template T
     * @param callable(self): T $work
     * @return T
     */
    public function transaction(callable $work): mixed
    {
        $id = ++self::$transactions;
        $transaction = new self("tx:$id");
        try {
            $this->call('begin', '', [\Tenma\Internal\Current::$request ?? 0, $id]);
        } catch (\Throwable $error) {
            // Cancelled just as BEGIN succeeded: the transaction exists, but nobody would end it
            try {
                $transaction->call('rollback', '', []);
            } catch (\Throwable) {
            }
            throw $error;
        }
        try {
            $result = $work($transaction);
        } catch (\Throwable $error) {
            $transaction->call('rollback', '', []);
            throw $error;
        }
        $transaction->call('commit', '', []);
        return $result;
    }

    /** @param list<mixed> $params */
    private function call(string $mode, string $sql, array $params): mixed
    {
        \Tenma\Internal\requireFiber();
        $json = \Tenma\Internal\await(\tenma_op_db("pg.$mode", $this->target, $sql, json_encode(array_values($params), JSON_THROW_ON_ERROR)));
        return json_decode($json, true, flags: JSON_THROW_ON_ERROR | JSON_BIGINT_AS_STRING);
    }
}

<?php

namespace Demo;

/** Lives as long as the process: under tenma across all requests, under FPM for just one. */
class Stats
{
    private int $requests = 0;
    private float $started;

    public function __construct()
    {
        $this->started = microtime(true);
    }

    public function count(): int
    {
        return ++$this->requests;
    }

    public function uptime(): float
    {
        return round(microtime(true) - $this->started, 1);
    }
}

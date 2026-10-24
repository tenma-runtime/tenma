<?php

namespace Demo;

use Io\Attributes\{Controller, Get, Inject};

#[Controller]
class HomeController
{
    #[Inject]
    private Stats $stats;

    /** Counts requests: under tenma the number grows, under php -S it stays at 1. */
    #[Get('/')]
    public function hello(): array
    {
        return [
            'message' => 'Hello World',
            'request' => $this->stats->count(),
            'uptimeSeconds' => $this->stats->uptime(),
            'sapi' => php_sapi_name(),
        ];
    }
}

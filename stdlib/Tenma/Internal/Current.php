<?php

namespace Tenma\Internal;

/** The request whose code is running right now; tasks and ops belong to it, exit() ends it. */
final class Current
{
    public static ?int $request = null;
    /** The durable function (Threading\Durable) whose fiber runs right now, by key. */
    public static ?string $durable = null;
    /** A classic request that still has to respond. */
    public static ?int $classic = null;
    /** The classic request ended with an exception. */
    public static bool $failed = false;
    /**
     * The Threading\AsyncLocal values from the script: a classic request sets them without a task, and
     * they come back after it, so the next request starts without them.
     *
     * @var array<int, mixed>
     */
    public static array $scriptContext = [];
}

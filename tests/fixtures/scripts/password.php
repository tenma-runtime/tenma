<?php

use Security\Password;

// In place outside a fiber, on a background thread inside one; both sides verify the other's hashes
echo json_encode(Password::verify('a', Password::hash('a'))), "\n";

\Threading\Task::run(function (): void {
    $hash = Password::hash('secret');
    echo json_encode([
        str_starts_with($hash, '$argon2id$v=19$m=65536,t=4,p=1$'),
        password_verify('secret', $hash),
        Password::verify('secret', $hash),
        Password::verify('wrong', $hash),
        Password::verify('secret', password_hash('secret', PASSWORD_ARGON2ID)),
        Password::verify('secret', password_hash('secret', PASSWORD_BCRYPT)),
        Password::verify('secret', 'broken'),
        Password::needsRehash($hash),
        Password::needsRehash(password_hash('secret', PASSWORD_BCRYPT)),
    ]), "\n";

    // The worker goes on while hashes run
    $ticks = 0;
    $timer = \Threading\Timer::every(0.005, function () use (&$ticks): void {
        $ticks++;
    });
    \Threading\Task::all(...array_fill(0, 3, fn () => Password::hash('x')));
    $timer->cancel();
    echo $ticks > 0 ? "loop ran\n" : "loop blocked\n";
});

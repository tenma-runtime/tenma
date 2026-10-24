<?php
// For tests/http.rs: what a classic request sets in an AsyncLocal ends with the request
$user = new Threading\AsyncLocal();
$user->set('script');

(new \Http\ClassicServer((int) getenv('PORT'), function () use ($user) {
    echo $user->get();
    if (isset($_GET['set'])) {
        $user->set($_GET['set']);
    }
    if (isset($_GET['fiber'])) {
        $fiber = new Fiber(fn () => $user->set('fiber'));
        $fiber->start();
    }
}))->start();

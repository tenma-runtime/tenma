<?php
// For tests/http.rs: fiber mode refuses sessions, one worker serves many visitors at once
(new \Http\Server((int) getenv('PORT'), function () {
    try {
        session_start();
        return 'started';
    } catch (Error $error) {
        return $error->getMessage();
    }
}))->start();

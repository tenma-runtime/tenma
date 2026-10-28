<?php
// For tests/http.rs: the static root is the script's own directory, where tenma.json, php.ini and the
// journals of --record also lie
(new \Http\Server((int) getenv('PORT'), fn (Http\Request $request) => "php: {$request->path}\n", static: __DIR__))->start();

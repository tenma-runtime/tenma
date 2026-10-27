<?php

// Front controller as in any Io app. Under tenma it runs once and then serves all
// requests from the same process, in fiber mode with the app as the handler; under php -S or FPM
// it runs once per request as usual.

require __DIR__ . '/vendor/autoload.php';

use Demo\HomeController;
use Demo\WaypointController;
use Io\App;

$app = App::create();
$app->attach([HomeController::class, WaypointController::class]);

$app->serve();

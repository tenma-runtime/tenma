<?php

namespace Demo;

use Io\Attributes\{Length, Max, Min, NotBlank};
use Io\FromArray;

class WaypointDTO
{
    use FromArray;

    #[NotBlank]
    #[Length(max: 80)]
    public string $name = '';

    #[Min(-90)]
    #[Max(90)]
    public float $lat = 0.0;

    #[Min(-180)]
    #[Max(180)]
    public float $lon = 0.0;
}

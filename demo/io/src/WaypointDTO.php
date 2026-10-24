<?php

namespace Demo;

use Io\Attributes\{Length, NotBlank};
use Io\FromArray;

class WaypointDTO
{
    use FromArray;

    #[NotBlank]
    #[Length(max: 80)]
    public string $name = '';

    public float $lat = 0.0;

    public float $lon = 0.0;
}

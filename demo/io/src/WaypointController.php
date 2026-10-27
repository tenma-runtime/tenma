<?php

namespace Demo;

use Io\Attributes\{Body, Controller, Delete, Get, Inject, Param, Post};
use Io\Exceptions\NotFoundException;
use Io\Http\Response;

#[Controller('/waypoints')]
class WaypointController
{
    #[Inject]
    private WaypointStore $store;

    #[Get('/')]
    public function list(): array
    {
        return $this->store->all();
    }

    #[Post('/')]
    public function create(#[Body] WaypointDTO $input, Response $res): array
    {
        $res->status(201);
        return $this->store->add($input->name, $input->lat, $input->lon);
    }

    #[Get('/{id}')]
    public function show(#[Param] int $id): array
    {
        return $this->store->find($id) ?? throw new NotFoundException("Waypoint $id does not exist");
    }

    #[Delete('/{id}')]
    public function delete(#[Param] int $id): array
    {
        $this->store->remove($id) || throw new NotFoundException("Waypoint $id does not exist");
        return ['deleted' => $id];
    }

    /** Great-circle distance between two waypoints in kilometers (haversine). */
    #[Get('/{id}/distance/{other}')]
    public function distance(#[Param] int $id, #[Param] int $other): array
    {
        $from = $this->show($id);
        $to = $this->show($other);
        $dLat = deg2rad($to['lat'] - $from['lat']);
        $dLon = deg2rad($to['lon'] - $from['lon']);
        $a = sin($dLat / 2) ** 2 + cos(deg2rad($from['lat'])) * cos(deg2rad($to['lat'])) * sin($dLon / 2) ** 2;
        // Rounding can push near-antipodal points just above 1, and asin() would return NAN
        $a = min(1.0, $a);
        return ['from' => $from['name'], 'to' => $to['name'], 'km' => round(2 * 6371 * asin(sqrt($a)), 1)];
    }
}

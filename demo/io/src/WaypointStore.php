<?php

namespace Demo;

/** Waypoints in memory. Under tenma they persist between requests. */
class WaypointStore
{
    /** @var array<int, array{id: int, name: string, lat: float, lon: float}> */
    private array $waypoints = [
        1 => ['id' => 1, 'name' => 'Marienplatz', 'lat' => 48.1374, 'lon' => 11.5755],
        2 => ['id' => 2, 'name' => 'Brandenburger Tor', 'lat' => 52.5163, 'lon' => 13.3777],
    ];
    private int $nextId = 3;

    /** @return list<array{id: int, name: string, lat: float, lon: float}> */
    public function all(): array
    {
        return array_values($this->waypoints);
    }

    /** @return array{id: int, name: string, lat: float, lon: float}|null */
    public function find(int $id): ?array
    {
        return $this->waypoints[$id] ?? null;
    }

    /** @return array{id: int, name: string, lat: float, lon: float} */
    public function add(string $name, float $lat, float $lon): array
    {
        $id = $this->nextId++;
        return $this->waypoints[$id] = ['id' => $id, 'name' => $name, 'lat' => $lat, 'lon' => $lon];
    }

    public function remove(int $id): bool
    {
        $found = isset($this->waypoints[$id]);
        unset($this->waypoints[$id]);
        return $found;
    }
}

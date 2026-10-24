---
title: Record and replay
description: Record what a worker receives with --record and run it again with tenma replay, also step by step backwards in the debugger.
order: 325
---

A request that failed in production can run again on a laptop, exactly as it ran then. tenma records
everything a worker gets from outside: every request, the result of every op (a timer, a fetch, a
query, a file, a message), the moment a client leaves, and the values that `time()`, `random_int()`
and their kin returned. `tenma replay` plays that journal into a fresh interpreter. No op runs for
real; PHP gets the recorded answers in the recorded order.

## Record

```bash
tenma --record traces/ app.php
```

Every worker writes one file per interpreter life into `traces/`: `<pid>-<worker>-<start>.trace`,
closed when the worker restarts (`--max-requests`, a fatal error, a reload under `--watch`) or tenma
stops. What a request answered is on disk at once; the list of loaded files follows at the end.

A worker in production lives for days. At `--record-max-size` (50 MB by default) its journal is cut:
the worker goes on in `<pid>-<worker>-<start>-2.trace`, which starts with the same header and the
startup part of the first file, everything up to its first request. Of the files before, the last one
stays, and every file with a failure; with `--record-on error` the others go.

| Option                       | Default | Does                                                                                   |
| ---------------------------- | ------- | -------------------------------------------------------------------------------------- |
| `--record DIR`               |         | every worker writes its journal there                                                  |
| `--record-on error`          | always  | keeps a journal only when a request answered 5xx or PHP failed; the others are deleted |
| `--record-max-size SIZE`     | `50m`   | the size at which a worker's journal is cut and goes on in the next file               |
| `--record-max-body SIZE`     | `1m`    | request bodies and op results above this are cut; the request is marked not replayable |
| `--record-redact HEADER,...` | none    | replaces these request headers with `[redacted]`                                       |

A journal holds request headers, cookies and query results: it is as secret as the traffic itself.
Keep the directory out of the repository and of reach.

## Replay

```bash
tenma replay traces/4711-0-1792569255382.trace
tenma replay traces/4711-0-1792569255382.trace --check
tenma replay traces/4711-0-1792569255382.trace --until 3
```

One worker runs the recorded script with the recorded arguments, environment and working directory,
without opening a port. Each request is injected when its turn comes. `--until N` stops after request
N. `--check` compares every response with the recorded one (headers except `date`, status, body) and
ends with status 1 if one differs: recorded traffic becomes a regression test against new code.

If PHP asks for something else than the journal has next (another op, another function, a `time()`
call that was not there), the code differs from the recorded code. The replay stops and says so:

```
tenma replay: diverged: the journal has op 2 (sleep 50 ms), but PHP started sleep 70 ms
```

The journal ends with the loaded files and their hashes; a replay names the files that changed. That
is a hint, not an error: replaying old traffic against new code is the point of `--check`.

## One request alone

```bash
tenma replay traces/4711-0-1792569255382-3.trace --list
tenma replay traces/4711-0-1792569255382-3.trace --request 35 --check
```

`--list` numbers the requests of a file with their status:

```
cut from an earlier file: play one request at a time with --request N
    1  200  GET /
    2  200  GET /ping
   35  500  GET /boom
```

`--request N` plays the startup part and then request N alone, without the requests before it. Every
op and every value records the request it belongs to, so the replay answers exactly those; a timer or a
background task that starts an op meanwhile waits forever instead. Most requests need nothing else:
controllers live per request, data lives in the database. If request N depended on what an earlier
request left in memory (a cache, a counter), it starts another op than recorded and the replay says
`diverged`; then only the whole file, from the worker's start, plays it exactly.

A file cut at `--record-max-size` plays only this way: its earlier requests are in the file before.

## In the debugger

```bash
tenma --inspect-wait replay traces/4711-0-1792569255382.trace
```

The debugger works as always, and gains two commands: step back and reverse continue. A step back
does not run code backwards: tenma starts the replay over and runs it to the statement before, in the
same request, or to the last place it stopped before. The run is the same each time, since every op and
value comes from the journal; without real I/O it takes as long as the computation, a fraction of a
second for an Io app. Output that a run prints a second time stays silent. Back from an exception
goes to the statement that threw. With `--request N` only the startup part and that request run again.
The editor shows the usual stepping buttons for it.

Side effects that do not go through an op (`file_put_contents()`, a log written with `fwrite()`) happen
again on every run.

## What a replay covers

| Repeats exactly                                                                                                                                                                                  | Does not                                                                                           |
| ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ | -------------------------------------------------------------------------------------------------- |
| requests, responses, streams, WebSocket messages, a client leaving                                                                                                                               | PHP's own sockets (mysqlnd, `fsockopen()`): they connect for real, or fail                         |
| `Task::delay()`, timers, `Http\Client`, `Data\Postgres`, `Data\Redis`, PDO with `pgsql:`, files, channels                                                                                        | `sleep()`, `usleep()`, curl, ext/pgsql: they run for real; the journal notes that they were called |
| `time()`, `microtime()`, `hrtime()`, `date()`, `strtotime()`, `date_create()`, `random_int()`, `random_bytes()`, `mt_rand()`, `rand()`, `uniqid()`, `lcg_value()`, `getmypid()`, `gethostname()` | `new DateTime()` and anything else that reads the clock without one of those functions             |
| the environment, `$argv`, the working directory                                                                                                                                                  | `spl_object_id()` and other values that depend on allocation, unless the code is the same          |

A request whose body or result was cut by `--record-max-body` cannot be replayed exactly; the journal
says which.

## See also

- [Debugger](debugger.md)
- [Command line](command-line.md)

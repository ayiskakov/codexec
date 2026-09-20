# codexec

A LeetCode-style code execution platform in Rust. This is phase 0 of the design
doc: the judge core and an HTTP API as a single process, with Python, Go and
Rust as the first languages.

Untrusted code runs inside [isolate](https://github.com/ioi/isolate) 2.7
(Linux namespaces, cgroup v2, seccomp). **Linux only**: on Windows, run it in
WSL2 or a Linux VM.

## What is here

```
crates/judge    execution engine (library)
  sandbox.rs      what "run this untrusted command" means, backend-independent
  isolate.rs      isolate backend: pipes in, pipes out, meta file parsing
  language.rs     language registry loaded from languages.toml
  pipeline.rs     compile -> run each test -> compare -> stop at first failure
  compare.rs      lines / tokens / float comparers (run outside the sandbox)
  problem.rs      problem directories, content-hashed versions
  mock.rs         scripted sandbox for tests (feature "mock")
crates/server   HTTP API + SQLite store that doubles as the job queue + workers
crates/cli      `codexec judge` and `codexec check-problem`
problems/       three sample problems with reference and known-bad solutions
scripts/        setup-isolate.sh, e2e.py, sandbox-probe.py
```

How it maps to the design doc:

| Design doc | Here |
| --- | --- |
| Postgres `SKIP LOCKED` as the single-box queue | SQLite `UPDATE ... RETURNING` claim in `store.rs`, same semantics |
| At-least-once delivery, conditional result write | Every worker write is fenced by `attempt` and `status != DONE` |
| Sweeper re-enqueues stuck jobs, `IE` after 3 attempts | `Store::sweep`, lease heartbeat on every progress event |
| Run results in Redis with a TTL | In-memory `RunStore`, bounded queue, 10-minute TTL |
| Result event bus + SSE | In-process `Hub` (tokio broadcast) + `GET /v1/submissions/{id}/events` |
| Weighted lanes, run before submit | Workers serve up to 3 runs, then give a waiting submission a turn |
| Answers never enter the sandbox | Input goes in by pipe, stdout comes back by pipe, comparison is host-side |
| Treat the box directory as hostile (Judge0 lesson) | The worker writes the source with `O_EXCL` into a fresh box and never opens a path in it again |
| Immutable problem versions | `Problem::version` = SHA-256 over limits, comparer and tests; stored on each submission |
| Problem build pipeline | `codexec check-problem`: reference solutions must pass, labelled bad solutions must fail as labelled |

Not built yet: function-signature harness (problems are stdin/stdout), auth
and rate limits, contests, a frontend, per-core pinning and canary timing.

## Quick start (Ubuntu 22.04/24.04, including WSL2)

```bash
sudo bash scripts/setup-isolate.sh     # isolate + Go + Go std cache + Rust under /opt/rustup
cargo build --release

# Validate the sample problems against the real sandbox
./target/release/codexec check-problem problems/*

# Start the API (config: codexec.toml)
./target/release/codexec-server
python3 scripts/e2e.py                 # in another terminal: 21 solutions through the API
```

If the machine has no cgroup v2 (the setup script tells you), set
`use_cgroups = false` in `codexec.toml`. That is a development fallback: memory
limits become address-space rlimits, Go runs without a hard memory cap, and
MLE is detected after the fact from peak RSS.

WSL2 notes:

- cgroup v2 needs `systemd=true` under `[boot]` in `/etc/wsl.conf` and
  `kernelCommandLine = cgroup_no_v1=all` under `[wsl2]` in `%UserProfile%\.wslconfig`,
  then `wsl --shutdown`.
- This folder lives on the Windows drive. Builds and SQLite are slow and
  flaky over `/mnt/c`; copy or clone the project into the WSL home directory,
  or at least point `database` and `meta_dir` at a Linux path.

## API

Everything is asynchronous: `POST` returns 202 with an id, the verdict arrives
by polling or over SSE. There is no authentication yet; bind to localhost.

```bash
# Submit against the hidden tests
curl -s localhost:8080/v1/submissions -H 'content-type: application/json' \
  -H 'idempotency-key: my-retry-token' \
  -d '{"problem":"a-plus-b","language":"python","source":"print(sum(map(int,input().split())))"}'

curl -s localhost:8080/v1/submissions/<id>            # poll: QUEUED -> COMPILING -> RUNNING -> DONE
curl -N  localhost:8080/v1/submissions/<id>/events    # SSE: progress events, then "verdict", then closes
curl -s localhost:8080/v1/submissions/<id>/source
curl -s 'localhost:8080/v1/submissions?problem=a-plus-b&limit=20'

# Run: custom stdin, or the problem's samples when "stdin" is omitted
curl -s localhost:8080/v1/runs -H 'content-type: application/json' \
  -d '{"language":"go","source":"package main\nimport \"fmt\"\nfunc main(){fmt.Println(\"hi\")}","stdin":""}'
curl -s localhost:8080/v1/runs/<id>

curl -s localhost:8080/v1/problems
curl -s localhost:8080/v1/problems/two-sum            # statement, limits, samples; never hidden tests
curl -s localhost:8080/v1/languages
curl -s localhost:8080/healthz
```

Verdicts: `AC`, `WA`, `TLE`, `MLE`, `RE`, `CE`, `OLE`, `IE`. Submissions stop
at the first failing test. The failing test's input is returned only if the
problem sets `show_failed_input = true`, truncated to 1 KB.

## Adding a problem

```
problems/<slug>/
  problem.toml       title, difficulty, time_limit_ms, memory_limit_mb, comparer, samples
  statement.md
  tests/NN.in        stdin
  tests/NN.out       expected stdout
  solutions/         ac.py, wa-off-by-one.go, tle-quadratic.rs, ...
```

`comparer` is `"lines"` (default; trailing whitespace ignored), `"tokens"`
(layout ignored) or `{ float = { eps = 1e-6 } }`. The prefix of each file in
`solutions/` is the verdict it must get; `codexec check-problem problems/<slug>`
enforces that, and fails if no accepted reference solution exists. The server
loads problems at start-up.

## Adding a language

Add a table to `languages.toml`: `source_file`, an optional `[lang.compile]`
step and a `[lang.run]` step, each with `argv`, `env`, `dirs` (extra read-only
bind mounts). Every `argv[0]` must be an absolute path that exists inside the
sandbox. Set `address_space_limit = false` for runtimes that reserve huge
virtual ranges (Go, JVM). No code changes are needed.

Go specifics worth knowing: the standard library is pre-built into a
read-only cache (`/var/cache/codexec/gocache`), otherwise every submission
spends about 10 CPU-seconds recompiling it. The cache must stay read-only, or
one user could poison another user's binary. Rebuild it after a Go upgrade by
re-running the setup script.

## Tests

```bash
cargo test --workspace                 # 41 tests, no sandbox needed (scripted mock)
python3 scripts/e2e.py                 # real sandbox, through the HTTP API
./target/release/codexec judge --problem problems/two-sum my_solution.py
```

`scripts/sandbox-probe.py` is a program to submit through `/v1/runs`: it
reports what the sandbox lets it see (expected: UID 60000, no network, no
`/root` or `/home`, one visible process).

## Security status

This is suitable for personal use on a machine you control. Before letting
strangers submit code: run workers on a dedicated VM with no credentials and
no outbound network, keep `use_cgroups = true`, run the server as an
unprivileged user (isolate is setuid; the server needs no root), never use a
privileged container, add authentication and rate limits, and get the
execution path reviewed.

#!/usr/bin/env python3
"""End-to-end check against a running codexec-server and the real sandbox.

Submits every file in problems/*/solutions/ through the HTTP API and verifies
the verdict promised by its file name (ac.py, wa-minus.py, tle-loop.go, ...).
Also exercises the Run lane, idempotency keys and the SSE stream.

    cargo run --release -p codexec-server &      # in one terminal
    python3 scripts/e2e.py                       # in another

Standard library only.
"""
import json
import pathlib
import sys
import time
import urllib.error
import urllib.request

BASE = sys.argv[1] if len(sys.argv) > 1 else "http://127.0.0.1:8080"
ROOT = pathlib.Path(__file__).resolve().parent.parent
LANG_BY_EXT = {".py": "python", ".go": "go", ".rs": "rust"}
VERDICTS = {"AC", "WA", "TLE", "MLE", "RE", "CE", "OLE", "IE"}


def call(method, path, body=None, headers=None):
    data = json.dumps(body).encode() if body is not None else None
    req = urllib.request.Request(BASE + path, data=data, method=method)
    req.add_header("content-type", "application/json")
    for k, v in (headers or {}).items():
        req.add_header(k, v)
    try:
        with urllib.request.urlopen(req, timeout=30) as res:
            return res.status, json.loads(res.read() or b"null")
    except urllib.error.HTTPError as e:
        return e.code, json.loads(e.read() or b"null")


def wait_done(path, timeout=180):
    deadline = time.time() + timeout
    while time.time() < deadline:
        _, body = call("GET", path)
        if body["status"] == "DONE":
            return body
        time.sleep(0.2)
    raise SystemExit(f"timed out waiting for {path}")


def main():
    failures = 0
    status, health = call("GET", "/healthz")
    assert status == 200, health
    print(f"server ok: {health}")

    # 1. Submit everything at once so the queue actually queues.
    pending = []
    for sol in sorted(ROOT.glob("problems/*/solutions/*")):
        want = sol.stem.split("-")[0].upper()
        lang = LANG_BY_EXT.get(sol.suffix)
        if want not in VERDICTS or lang is None:
            continue
        problem = sol.parent.parent.name
        status, sub = call("POST", "/v1/submissions",
                           {"problem": problem, "language": lang, "source": sol.read_text()})
        assert status == 202, (status, sub)
        pending.append((problem, sol.name, want, sub["id"]))
    print(f"submitted {len(pending)} solutions")

    started = time.time()
    for problem, name, want, sid in pending:
        done = wait_done(f"/v1/submissions/{sid}")
        ok = done["verdict"] == want
        failures += not ok
        print(f"  {'ok  ' if ok else 'FAIL'} {problem:<18} {name:<20} want {want:<3} got {done['verdict']:<3}"
              f" {done['time_ms'] or 0:>5} ms {done['memory_kb'] or 0:>8} kB  tests {done['tests_done']}/{done['tests_total']}")
    print(f"all verdicts in {time.time() - started:.1f} s")

    # 2. Run lane: custom stdin, output captured, no comparison.
    _, run = call("POST", "/v1/runs", {"language": "python", "source": "print(input()[::-1])", "stdin": "codexec\n"})
    out = wait_done(f"/v1/runs/{run['id']}")["result"]["tests"][0]["stdout"]
    ok = out == "cexedoc\n"
    failures += not ok
    print(f"  {'ok  ' if ok else 'FAIL'} run with custom stdin -> {out!r}")

    # 3. Run lane against a problem's samples.
    src = (ROOT / "problems/two-sum/solutions/ac.rs").read_text()
    _, run = call("POST", "/v1/runs", {"language": "rust", "source": src, "problem": "two-sum"})
    res = wait_done(f"/v1/runs/{run['id']}")["result"]
    ok = res["verdict"] == "AC" and [t["name"] for t in res["tests"]] == ["01"]
    failures += not ok
    print(f"  {'ok  ' if ok else 'FAIL'} run against samples -> {res['verdict']}, tests {[t['name'] for t in res['tests']]}")

    # 4. Idempotency key.
    body = {"problem": "a-plus-b", "language": "python", "source": "print(sum(map(int, input().split())))"}
    key = {"idempotency-key": f"e2e-{time.time()}"}
    _, a = call("POST", "/v1/submissions", body, key)
    _, b = call("POST", "/v1/submissions", body, key)
    ok = a["id"] == b["id"]
    failures += not ok
    print(f"  {'ok  ' if ok else 'FAIL'} idempotency key returns the same submission")

    # 5. SSE: must deliver a verdict event and then close by itself.
    with urllib.request.urlopen(f"{BASE}/v1/submissions/{a['id']}/events", timeout=60) as res:
        stream = res.read().decode()
    ok = "event: verdict" in stream and '"verdict":"AC"' in stream
    failures += not ok
    print(f"  {'ok  ' if ok else 'FAIL'} SSE stream: {stream.count('event:')} event(s), closed after the verdict")

    # 6. Validation.
    status, _ = call("POST", "/v1/submissions", {"problem": "a-plus-b", "language": "cobol", "source": "x"})
    ok = status == 400
    failures += not ok
    print(f"  {'ok  ' if ok else 'FAIL'} unknown language -> HTTP {status}")

    print("PASS" if failures == 0 else f"{failures} FAILURE(S)")
    sys.exit(1 if failures else 0)


if __name__ == "__main__":
    main()

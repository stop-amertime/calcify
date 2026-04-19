#!/usr/bin/env python3
"""Sequential MCP smoke test — sends JSON-RPC frames one at a time and asserts."""
import json, subprocess, sys, os

BIN = "./target/debug/calcite-debugger.exe"
CSS = "crates/calcite-debugger/tests/minimal.css"

_next_id = [100]
def next_id():
    _next_id[0] += 1
    return _next_id[0]

def call(proc, req):
    line = json.dumps(req) + "\n"
    proc.stdin.write(line.encode())
    proc.stdin.flush()
    if "id" not in req:
        return None  # notification, no response
    resp = proc.stdout.readline()
    return json.loads(resp)

def tc(proc, name, args):
    r = call(proc, {"jsonrpc":"2.0","id":next_id(),"method":"tools/call",
                    "params":{"name":name,"arguments":args}})
    assert "result" in r, f"tool {name} failed: {r}"
    sc = r["result"].get("structuredContent")
    return sc

def expect_err(proc, name, args, needle):
    r = call(proc, {"jsonrpc":"2.0","id":next_id(),"method":"tools/call",
                    "params":{"name":name,"arguments":args}})
    # Tool errors surface either as JSON-RPC error or as isError=true content.
    if "error" in r:
        assert needle in r["error"]["message"], r
    else:
        assert r["result"].get("isError"), f"expected error for {name}, got {r}"
        msg = r["result"]["content"][0]["text"] if r["result"]["content"] else ""
        assert needle in msg, f"expected {needle!r} in {msg!r}"

def initialize(proc):
    r = call(proc, {"jsonrpc":"2.0","id":1,"method":"initialize",
                    "params":{"protocolVersion":"2024-11-05","capabilities":{},
                              "clientInfo":{"name":"smoke","version":"0"}}})
    assert "serverInfo" in r["result"], r
    call(proc, {"jsonrpc":"2.0","method":"notifications/initialized"})

def run_scenario(proc):
    """Full tick/seek scenario assuming a program is already loaded with
    base_interval=4."""
    # info: loaded, tick 0, base at 0
    info = tc(proc, "info", {})
    assert info["loaded"] is True, info
    assert info["current_tick"] == 0, info
    assert info["snapshots"] == [0], info
    assert info["base_interval"] == 4, info

    # tick 10 -> tick=10
    r = tc(proc, "tick", {"count":10})
    assert r["tick"] == 10 and r["ticks_executed"] == 10, r

    # bases at 0, 4, 8
    info = tc(proc, "info", {})
    assert info["current_tick"] == 10 and info["snapshots"] == [0, 4, 8], info

    # get_state at 10
    st = tc(proc, "get_state", {})
    assert st["tick"] == 10 and st["registers"]["tick"] == 10, st
    assert st["properties"]["--tick"] == 10, st

    # reverse seek to 3 via delta revert
    st = tc(proc, "seek", {"tick":3})
    assert st["tick"] == 3 and st["registers"]["tick"] == 3, st
    assert st["properties"]["--tick"] == 4, st

    # forward delta seek to 7
    st = tc(proc, "seek", {"tick":7})
    assert st["tick"] == 7 and st["registers"]["tick"] == 7, st
    assert st["properties"]["--tick"] == 8, st

    # reverse all the way to 0 (crosses base boundary)
    st = tc(proc, "seek", {"tick":0})
    assert st["tick"] == 0 and st["registers"]["tick"] == 0, st
    assert st["properties"]["--tick"] == 1, st

    # forward via deltas to 5
    st = tc(proc, "seek", {"tick":5})
    assert st["tick"] == 5 and st["registers"]["tick"] == 5, st

    # tick 3 more -> 8
    r = tc(proc, "tick", {"count":3})
    assert r["tick"] == 8 and r["ticks_executed"] == 3, r

def test_with_initial_file():
    """Server started with -i: program pre-loaded."""
    proc = subprocess.Popen([BIN, "-i", CSS, "--base-interval", "4"],
                            stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                            stderr=subprocess.PIPE)
    try:
        initialize(proc)
        run_scenario(proc)
        print("test_with_initial_file: OK")
    finally:
        proc.stdin.close()
        proc.wait(timeout=5)

def test_open_tool():
    """Server started with no -i: info reports loaded=false, tick errors, open loads."""
    proc = subprocess.Popen([BIN, "--base-interval", "4"],
                            stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                            stderr=subprocess.PIPE)
    try:
        initialize(proc)

        # info works even with nothing loaded
        info = tc(proc, "info", {})
        assert info["loaded"] is False, info
        assert info["css_file"] is None, info
        assert info["base_interval"] == 4, info

        # tick should error
        expect_err(proc, "tick", {"count":1}, "no program loaded")
        expect_err(proc, "get_state", {}, "no program loaded")

        # open loads
        abs_css = os.path.abspath(CSS)
        r = tc(proc, "open", {"path": abs_css})
        assert r["css_file"].endswith("minimal.css"), r
        assert r["properties_count"] == 1, r
        assert r["assignments_count"] == 1, r

        # now everything works — run the full scenario
        run_scenario(proc)
        print("test_open_tool: OK")
    finally:
        proc.stdin.close()
        proc.wait(timeout=5)

def test_open_replaces():
    """open a second time replaces the current program, resetting tick+snapshots."""
    proc = subprocess.Popen([BIN, "-i", CSS, "--base-interval", "4"],
                            stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                            stderr=subprocess.PIPE)
    try:
        initialize(proc)
        tc(proc, "tick", {"count":6})
        info = tc(proc, "info", {})
        assert info["current_tick"] == 6, info

        # re-open same file — should reset to tick 0
        tc(proc, "open", {"path": os.path.abspath(CSS)})
        info = tc(proc, "info", {})
        assert info["current_tick"] == 0, info
        assert info["snapshots"] == [0], info
        print("test_open_replaces: OK")
    finally:
        proc.stdin.close()
        proc.wait(timeout=5)

if __name__ == "__main__":
    test_with_initial_file()
    test_open_tool()
    test_open_replaces()
    print("all good")

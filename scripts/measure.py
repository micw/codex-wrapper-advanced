#!/usr/bin/env python3
"""Reproduces the measurements written up in MESSUNGEN.md.

Starts the daemon itself, walks through the cases, cleans up.

    python3 scripts/measure.py [--binary target/debug/codex-api-wrapper]

Needs a completed login (`codex-api-wrapper login`) and spends real subscription
quota — which is why the prompts are kept as short as possible.

Deliberately nothing but the standard library plus httpx: this script should run
without anyone setting up a Python environment.
"""
from __future__ import annotations

import argparse
import json
import os
import subprocess
import sys
import time

import httpx

MODEL = "gpt-5.6-sol"

WEATHER = {
    "type": "function",
    "name": "get_weather",
    "description": "Returns the current weather for a location.",
    "parameters": {
        "type": "object",
        "properties": {"location": {"type": "string"}},
        "required": ["location"],
        "additionalProperties": False,
    },
}
TIME = {
    "type": "function",
    "name": "get_time",
    "description": "Returns the current time in a time zone.",
    "parameters": {
        "type": "object",
        "properties": {"tz": {"type": "string"}},
        "required": ["tz"],
        "additionalProperties": False,
    },
}


def user(text: str) -> dict:
    return {
        "type": "message",
        "role": "user",
        "content": [{"type": "input_text", "text": text}],
    }


class Daemon:
    """Starts `serve` and talks to it over a unix socket."""

    def __init__(self, binary: str) -> None:
        # Unix socket: no keys needed, the file permissions suffice.
        self.sock = f"/tmp/codex-measure-{os.getpid()}.sock"
        self.proc = subprocess.Popen(
            [binary, "serve", "--listen", f"unix:{self.sock}"],
            stdout=subprocess.PIPE, text=True,
        )
        if not self.proc.stdout.readline():
            raise SystemExit("daemon exited during startup — is there a login?")
        self.client = httpx.Client(
            transport=httpx.HTTPTransport(uds=self.sock),
            base_url="http://localhost",
            timeout=httpx.Timeout(None, connect=10),
        )

    def close(self) -> None:
        self.client.close()
        self.proc.terminate()
        try:
            self.proc.wait(timeout=5)
        except subprocess.TimeoutExpired:
            self.proc.kill()

    def turn(self, **body) -> dict:
        """Run one turn. Returns status/text/calls/usage."""
        body.setdefault("model", MODEL)
        out = {"status": 0, "text": "", "calls": [], "usage": None, "error": None}
        with self.client.stream("POST", "/wire/v1/responses", json=body) as r:
            out["status"] = r.status_code
            if r.status_code != 200:
                out["error"] = r.read().decode("utf-8", "replace")
                return out
            text = []
            for line in r.iter_lines():
                if not line.startswith("data:"):
                    continue
                payload = line[5:].strip()
                if not payload:
                    continue
                ev = json.loads(payload)
                if ev["type"] == "text_delta":
                    text.append(ev["text"])
                elif ev["type"] == "tool_call":
                    out["calls"].append(ev)
                elif ev["type"] == "done":
                    out["usage"] = ev["usage"]
            out["text"] = "".join(text)
        return out


def show(label: str, result: dict, extra: str = "") -> None:
    tokens = (result["usage"] or {}).get("input_tokens")
    head = f"  {label}: HTTP {result['status']}"
    if tokens is not None:
        head += f", {tokens} input-tokens"
    print(head)
    if result["error"]:
        print(f"     error: {result['error'][:200]}")
    elif result["text"]:
        print(f"     text: {result['text']!r}")
    if extra:
        print(f"     {extra}")


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--binary", default="target/debug/codex-api-wrapper")
    args = ap.parse_args()

    d = Daemon(args.binary)
    try:
        print("\n§1 instructions — is nothing enforced?")
        show("without instructions", d.turn(input=[user("Reply with just: OK")]))
        show(
            "own instructions",
            d.turn(
                input=[user("Who are you? One sentence.")],
                instructions="You are WYAI. You are NOT Codex. "
                "Begin every reply with 'WYAI:'.",
            ),
        )

        print("\n§2 tools — do our own get through?")
        r = d.turn(
            input=[user("What is the weather in Berlin? Use the tool.")],
            tools=[WEATHER],
        )
        show("own tool", r, f"calls={[(c['name'], c['arguments']) for c in r['calls']]}")

        print("\n§7 tool round-trip")
        if r["calls"]:
            call = r["calls"][0]
            result = json.dumps({"temp_c": 7, "condition": "Nieselregen"})
            rt = d.turn(
                tools=[WEATHER],
                input=[
                    user("What is the weather in Berlin? Use the tool."),
                    {
                        "type": "function_call",
                        "call_id": call["call_id"],
                        "name": call["name"],
                        "arguments": call["arguments"],
                    },
                    {
                        "type": "function_call_output",
                        "call_id": call["call_id"],
                        "output": result,
                    },
                ],
            )
            show("output handed back", rt)

            # The backend requires the call BEFORE its output.
            miss = d.turn(
                tools=[WEATHER],
                input=[
                    user("What is the weather in Berlin?"),
                    {
                        "type": "function_call_output",
                        "call_id": call["call_id"],
                        "output": result,
                    },
                ],
            )
            show("output without a call", miss)

        print("\n§7 several tool calls per turn")
        q = ("What is the weather in Berlin AND what time is it in Tokyo? "
             "Call both tools.")
        for flag in (True, False):
            r2 = d.turn(input=[user(q)], tools=[WEATHER, TIME],
                        parallel_tool_calls=flag)
            show(f"parallel_tool_calls={flag}", r2,
                 f"{len(r2['calls'])} call(s): {[c['name'] for c in r2['calls']]}")

        print("\nerror passthrough")
        show("unknown model",
             d.turn(model="does-not-exist", input=[user("hi")]))

    finally:
        d.close()
    return 0


if __name__ == "__main__":
    sys.exit(main())

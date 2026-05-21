#!/usr/bin/env python3
"""Proof-of-concept OMP RPC orchestration for AgentFlow.

This intentionally stays small. It demonstrates:
- starting `omp --mode rpc`
- reading `sessionId` / `sessionFile` with `get_state`
- sending multiple prompts to the same session
- waiting for `agent_end` before sending the next prompt
- starting a new OMP session with `new_session`
- switching back to a saved session file
- opening a saved session interactively with `omp --resume <sessionFile>`
"""

from __future__ import annotations

import argparse
import json
import os
import queue
import subprocess
import sys
import threading
import time
from dataclasses import dataclass
from pathlib import Path
from typing import Any


class OmpRpcError(RuntimeError):
    pass


@dataclass
class RpcResult:
    response: dict[str, Any] | None
    events: list[dict[str, Any]]


class OmpRpc:
    def __init__(self, cwd: Path, omp: str = "omp") -> None:
        self.cwd = cwd
        self.omp = omp
        self.proc: subprocess.Popen[str] | None = None
        self.lines: queue.Queue[dict[str, Any]] = queue.Queue()
        self.reader: threading.Thread | None = None
        self._next_id = 1

    def start(self) -> None:
        self.proc = subprocess.Popen(
            [self.omp, "--mode", "rpc"],
            cwd=str(self.cwd),
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
            bufsize=1,
        )
        assert self.proc.stdout is not None
        self.reader = threading.Thread(target=self._read_stdout, args=(self.proc.stdout,), daemon=True)
        self.reader.start()
        frame = self._read_frame(timeout=30)
        if frame.get("type") != "ready":
            raise OmpRpcError(f"Expected ready frame, got {frame!r}")

    def close(self) -> None:
        if self.proc is None:
            return
        if self.proc.stdin is not None:
            try:
                self.proc.stdin.close()
            except BrokenPipeError:
                pass
        try:
            self.proc.wait(timeout=10)
        except subprocess.TimeoutExpired:
            self.proc.terminate()
            try:
                self.proc.wait(timeout=5)
            except subprocess.TimeoutExpired:
                self.proc.kill()
        if self.proc.stderr is not None:
            stderr = self.proc.stderr.read()
            if stderr.strip():
                print(stderr, file=sys.stderr)

    def _read_stdout(self, stdout: Any) -> None:
        for line in stdout:
            line = line.strip()
            if not line:
                continue
            try:
                self.lines.put(json.loads(line))
            except json.JSONDecodeError:
                self.lines.put({"type": "_invalid_json", "raw": line})

    def _read_frame(self, timeout: float) -> dict[str, Any]:
        try:
            return self.lines.get(timeout=timeout)
        except queue.Empty as exc:
            raise OmpRpcError(f"Timed out waiting for OMP RPC frame after {timeout}s") from exc

    def send(self, payload: dict[str, Any]) -> str:
        if self.proc is None or self.proc.stdin is None:
            raise OmpRpcError("OMP RPC process is not running")
        if "id" not in payload:
            payload["id"] = f"req-{self._next_id}"
            self._next_id += 1
        self.proc.stdin.write(json.dumps(payload) + "\n")
        self.proc.stdin.flush()
        return str(payload["id"])

    def command(self, payload: dict[str, Any], timeout: float = 30) -> dict[str, Any]:
        req_id = self.send(payload)
        deadline = time.monotonic() + timeout
        while True:
            frame = self._read_frame(timeout=max(0.1, deadline - time.monotonic()))
            if frame.get("type") == "response" and frame.get("id") == req_id:
                if not frame.get("success"):
                    raise OmpRpcError(f"Command failed: {frame}")
                return frame
            if time.monotonic() >= deadline:
                raise OmpRpcError(f"Timed out waiting for command response {req_id}")

    def prompt_and_wait(self, message: str, timeout: float = 300) -> list[dict[str, Any]]:
        req_id = self.send({"type": "prompt", "message": message})
        events: list[dict[str, Any]] = []
        saw_ack = False
        deadline = time.monotonic() + timeout
        while True:
            frame = self._read_frame(timeout=max(0.1, deadline - time.monotonic()))
            events.append(frame)
            if frame.get("type") == "response" and frame.get("id") == req_id:
                if not frame.get("success"):
                    raise OmpRpcError(f"Prompt rejected: {frame}")
                saw_ack = True
            elif frame.get("type") == "agent_end":
                if not saw_ack:
                    raise OmpRpcError("Saw agent_end before prompt ack")
                return events
            if time.monotonic() >= deadline:
                raise OmpRpcError("Timed out waiting for agent_end")

    def get_state(self) -> dict[str, Any]:
        response = self.command({"type": "get_state"})
        data = response.get("data")
        if not isinstance(data, dict):
            raise OmpRpcError(f"get_state returned unexpected payload: {response}")
        return data

    def get_last_assistant_text(self) -> str:
        response = self.command({"type": "get_last_assistant_text"})
        data = response.get("data")
        if isinstance(data, str):
            return data
        if isinstance(data, dict):
            value = data.get("text") or data.get("message") or data.get("content")
            if isinstance(value, str):
                return value
        return json.dumps(data, sort_keys=True)


def run_smoke(args: argparse.Namespace) -> int:
    cwd = Path(args.cwd).resolve()
    rpc = OmpRpc(cwd=cwd, omp=args.omp)
    try:
        print(f"Starting OMP RPC in {cwd}")
        rpc.start()

        initial = rpc.get_state()
        print(f"Initial sessionId={initial.get('sessionId')} sessionFile={initial.get('sessionFile')}")

        token = "AGENTFLOW_SMOKE_TEST_7F3A9"
        print("Sending first prompt...")
        rpc.prompt_and_wait(
            f"Reply with exactly: remembered {token}. Also remember the token {token} for the next turn.",
            timeout=args.timeout,
        )
        state_after_first = rpc.get_state()
        first_text = rpc.get_last_assistant_text()
        print(f"After first prompt sessionId={state_after_first.get('sessionId')}")
        print(f"Last assistant text: {first_text[:300]!r}")

        print("Sending second prompt in same session...")
        rpc.prompt_and_wait("What token did I ask you to remember? Reply with only the token.", timeout=args.timeout)
        state_after_second = rpc.get_state()
        second_text = rpc.get_last_assistant_text()
        print(f"After second prompt sessionId={state_after_second.get('sessionId')}")
        print(f"Last assistant text: {second_text[:300]!r}")

        if token not in second_text:
            raise OmpRpcError("Same-session prompt did not recall the smoke-test token")

        original_session_file = state_after_second.get("sessionFile")
        if not isinstance(original_session_file, str) or not original_session_file:
            raise OmpRpcError("OMP did not report a sessionFile")

        print("Starting new session...")
        rpc.command({"type": "new_session"})
        new_state = rpc.get_state()
        print(f"New sessionId={new_state.get('sessionId')} sessionFile={new_state.get('sessionFile')}")
        if new_state.get("sessionId") == state_after_second.get("sessionId"):
            raise OmpRpcError("new_session did not change sessionId")

        print("Switching back to original session file...")
        rpc.command({"type": "switch_session", "sessionPath": original_session_file})
        switched_state = rpc.get_state()
        print(f"Switched sessionId={switched_state.get('sessionId')} sessionFile={switched_state.get('sessionFile')}")
        if switched_state.get("sessionFile") != original_session_file:
            raise OmpRpcError("switch_session did not restore original sessionFile")

        Path(args.state_out).write_text(
            json.dumps(
                {
                    "sessionId": switched_state.get("sessionId"),
                    "sessionFile": switched_state.get("sessionFile"),
                },
                indent=2,
            )
            + "\n"
        )
        print(f"Wrote session state to {args.state_out}")
        print("OMP RPC smoke test passed")
        return 0
    finally:
        rpc.close()


def run_open(args: argparse.Namespace) -> int:
    state_path = Path(args.state).resolve()
    state = json.loads(state_path.read_text())
    session_file = state.get("sessionFile")
    if not isinstance(session_file, str) or not session_file:
        raise OmpRpcError(f"No sessionFile found in {state_path}")
    cmd = [args.omp, "--resume", session_file]
    print("Opening:", " ".join(cmd))
    return subprocess.call(cmd, cwd=str(Path(args.cwd).resolve()))


def main() -> int:
    parser = argparse.ArgumentParser(description="AgentFlow OMP RPC proof of concept")
    parser.add_argument("--omp", default="omp", help="OMP executable")
    parser.add_argument("--cwd", default=".", help="Repo root / OMP working directory")
    sub = parser.add_subparsers(dest="cmd", required=True)

    smoke = sub.add_parser("smoke", help="Run OMP RPC same-session/new-session smoke test")
    smoke.add_argument("--timeout", type=float, default=300, help="Timeout per model turn")
    smoke.add_argument("--state-out", default=".agentflow-poc-session.json", help="Where to write session metadata")
    smoke.set_defaults(func=run_smoke)

    open_cmd = sub.add_parser("open", help="Open saved session interactively with omp --resume")
    open_cmd.add_argument("--state", default=".agentflow-poc-session.json", help="State JSON from smoke command")
    open_cmd.set_defaults(func=run_open)

    args = parser.parse_args()
    return int(args.func(args))


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except OmpRpcError as exc:
        print(f"error: {exc}", file=sys.stderr)
        raise SystemExit(1)

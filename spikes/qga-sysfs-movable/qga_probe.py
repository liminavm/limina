# SPDX-License-Identifier: GPL-2.0-only WITH LicenseRef-limina-exception
# Copyright © 2026 Gustavo Noronha Silva
"""Guest half of probe.sh: drive a qemu-ga on a unix socket through the writes a host would make.

Runs as root inside the guest. Every write goes through the agent, so the subject of each SELinux
check is the agent's domain; root's own shell only sets values back between routes and reads them.
"""

import base64
import json
import socket
import subprocess
import sys
import time

SOCK = sys.argv[1]

KNOBS = [
    ("/sys/devices/system/memory/auto_online_blocks", "online_movable"),
    ("/sys/module/memory_hotplug/parameters/online_policy", "auto-movable"),
    ("/sys/module/memory_hotplug/parameters/auto_movable_ratio", "400"),
]
GUEST_FILES = [
    ("/etc/modprobe.d/limina-probe.conf", "# limina qga probe\n"),
    ("/etc/udev/rules.d/99-limina-probe.rules", "# limina qga probe\n"),
    ("/etc/tmpfiles.d/limina-probe.conf", "# limina qga probe\n"),
]


class Agent:
    def __init__(self, path):
        self.s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        self.s.connect(path)
        self.f = self.s.makefile("rwb")

    def call(self, execute, **arguments):
        msg = {"execute": execute}
        if arguments:
            msg["arguments"] = arguments
        self.f.write(json.dumps(msg).encode() + b"\n")
        self.f.flush()
        return json.loads(self.f.readline())


def sh(cmd):
    return subprocess.run(cmd, shell=True, capture_output=True, text=True, stdin=subprocess.DEVNULL).stdout.strip()


def avcs_since(stamp):
    out = sh(f"ausearch --input-logs -m avc,user_avc -ts {stamp} 2>/dev/null | grep -o 'avc: .*'")
    return out or "(none)"


def stamp():
    time.sleep(1.1)
    return time.strftime("%H:%M:%S")


def file_route(agent, path, value):
    opened = agent.call("guest-file-open", path=path, mode="w")
    if "error" in opened:
        return f"open: {opened['error']}"
    handle = opened["return"]
    wrote = agent.call(
        "guest-file-write", handle=handle, **{"buf-b64": base64.b64encode(value.encode()).decode()}
    )
    closed = agent.call("guest-file-close", handle=handle)
    return f"write: {wrote.get('error', wrote.get('return'))}; close: {closed.get('error', 'ok')}"


def exec_route(agent, path, value):
    started = agent.call(
        "guest-exec",
        path="/bin/sh",
        arg=["-c", f"echo {value} > {path}"],
        **{"capture-output": True},
    )
    if "error" in started:
        return f"exec: {started['error']}"
    pid = started["return"]["pid"]
    for _ in range(50):
        status = agent.call("guest-exec-status", pid=pid)["return"]
        if status.get("exited"):
            err = base64.b64decode(status.get("err-data", "")).decode().strip()
            return f"exitcode {status.get('exitcode')}; stderr: {err or '(none)'}"
        time.sleep(0.1)
    return "exec: never exited"


def main():
    agent = Agent(SOCK)
    print("guest-ping:", agent.call("guest-ping"))
    for path, value in KNOBS:
        before = sh(f"cat {path}")
        for name, route in (("guest-file-write", file_route), ("guest-exec sh", exec_route)):
            sh(f"echo {before} > {path}")
            t = stamp()
            result = route(agent, path, value)
            after = sh(f"cat {path}")
            print(f"\n{path} [{name}]\n  before: {before}\n  result: {result}\n  after:  {after}")
            print(f"  took: {'yes' if value in after and value not in before else 'no'}")
            print(f"  avc:  {avcs_since(t)}")
        sh(f"echo {before} > {path}")
    for path, value in GUEST_FILES:
        t = stamp()
        result = file_route(agent, path, value)
        exists = sh(f"ls -Z {path} 2>&1")
        print(f"\n{path} [guest-file-write, create]\n  result: {result}\n  file:   {exists}")
        print(f"  avc:    {avcs_since(t)}")
        sh(f"rm -f {path}")


main()

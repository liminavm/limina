# SPDX-License-Identifier: MIT
# Copyright © 2026 Gustavo Noronha Silva
#
# A minimal Marionette client, so a browser workload can be driven and read rather than guessed at.
#
#   python3 marionette.py js 'return document.title'      run script, print its JSON result
#   python3 marionette.py click 'button.start'            click the first match of a CSS selector
#   python3 marionette.py wait 'WebGL' 300                poll innerText until it matches, print it
#   python3 marionette.py waitpath '/result/' 900          poll location.pathname until it matches
#   python3 marionette.py url x                           the current URL, answered by the parent process
#
# Firefox must have been started with `--marionette` (port 2828). Two things this exists for:
#
#   * Clicking. Tapping tab-then-enter through /dev/uinput guesses at focus order and reads as
#     success whether or not anything was hit.
#   * Telling one page from another. `wait` on body text cannot: a benchmark's own test pages say
#     "WebGL 2.0 Test", so waiting for `WebGL` to appear matches test 5 of 20 and reads a test page
#     as a result page. The pathname distinguishes them (`/run/tests/<n>/...` against `/result/`),
#     which is what `waitpath` is for. Wait on the pathname for *which page*, on the text for
#     *what the page has rendered* -- they are two questions and one verb cannot answer both.
#   * Reading scores. Basemark's community mode reports `Database: Unavailable` and never stores a
#     run server-side, so /api/results/details/<uid>/ answers 404 and the result page has nothing
#     to fetch. The numbers exist only in the DOM of the page that ran them, and this is how they
#     come back out.
#
# The protocol is length-prefixed JSON over TCP: `<len>:<payload>`, commands `[0, id, name, params]`
# and replies `[1, id, error, result]`.
import json
import re
import socket
import sys
import time

PORT = 2828


class Marionette:
    def __init__(self, host="127.0.0.1", port=PORT, timeout=30):
        self.sock = socket.create_connection((host, port), timeout=timeout)
        self.sock.settimeout(timeout)
        self.buf = b""
        self.msgid = 0
        self._read_frame()  # the server's handshake
        self.send("WebDriver:NewSession", {})

    def _read_frame(self):
        # `<len>:<payload>` -- read the digits, the colon, then exactly that many bytes.
        while b":" not in self.buf:
            chunk = self.sock.recv(65536)
            if not chunk:
                raise RuntimeError("marionette closed the connection")
            self.buf += chunk
        n, _, rest = self.buf.partition(b":")
        want = int(n)
        while len(rest) < want:
            chunk = self.sock.recv(65536)
            if not chunk:
                raise RuntimeError("marionette closed mid-frame")
            rest += chunk
        self.buf = rest[want:]
        return json.loads(rest[:want])

    def send(self, name, params):
        self.msgid += 1
        payload = json.dumps([0, self.msgid, name, params]).encode()
        self.sock.sendall(b"%d:%s" % (len(payload), payload))
        while True:
            msg = self._read_frame()
            # [1, id, error, result]
            if isinstance(msg, list) and len(msg) == 4 and msg[0] == 1:
                if msg[1] != self.msgid:
                    continue
                if msg[2]:
                    raise RuntimeError(f"{name} failed: {msg[2]}")
                return msg[3]

    def js(self, script, args=None):
        r = self.send("WebDriver:ExecuteScript", {"script": script, "args": args or []})
        return r.get("value") if isinstance(r, dict) else r


def main():
    if len(sys.argv) < 3:
        print(__doc__ or "usage: marionette.py js|click|wait ARG [timeout]", file=sys.stderr)
        return 2
    verb, arg = sys.argv[1], sys.argv[2]
    m = Marionette()

    if verb == "js":
        print(json.dumps(m.js(arg)))
        return 0

    if verb == "url":
        # Answered by the parent process, not the page: it keeps answering while the content
        # process is wedged, which is what lets a poller tell "stuck page" from "stuck browser".
        r = m.send("WebDriver:GetCurrentURL", {})
        print(r.get("value") if isinstance(r, dict) else r)
        return 0

    if verb == "click":
        # Clicked in the page rather than through the focus order, and it reports whether anything
        # was actually hit -- a click that matched nothing must not read as success.
        hit = m.js(
            "var e = document.querySelector(arguments[0]);"
            "if (!e) return false; e.click(); return true;",
            [arg],
        )
        print("clicked" if hit else "NO MATCH")
        return 0 if hit else 1

    if verb == "waitpath":
        deadline = time.time() + (float(sys.argv[3]) if len(sys.argv) > 3 else 300)
        pattern = re.compile(arg)
        while time.time() < deadline:
            path = m.js("return document.location.pathname") or ""
            if pattern.search(path):
                print(path)
                return 0
            time.sleep(2)
        print(f"TIMEOUT: pathname {arg!r} never appeared (last: {path!r})", file=sys.stderr)
        return 1

    if verb == "wait":
        deadline = time.time() + (float(sys.argv[3]) if len(sys.argv) > 3 else 300)
        pattern = re.compile(arg)
        while time.time() < deadline:
            text = m.js("return document.body ? document.body.innerText : ''") or ""
            if pattern.search(text):
                print(text)
                return 0
            time.sleep(2)
        print(f"TIMEOUT: {arg!r} never appeared", file=sys.stderr)
        return 1

    print(f"unknown verb {verb}", file=sys.stderr)
    return 2


if __name__ == "__main__":
    sys.exit(main())

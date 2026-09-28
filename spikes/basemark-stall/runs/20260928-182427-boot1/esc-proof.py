# Esc cancels a pending navigation: run as is (tab stays on the old URL), then with the
# subprocess.run line commented out as the control (tab lands on httpbin after 8 s).
import sys, time, subprocess; sys.path.insert(0, "/tmp")
from marionette import Marionette
m = Marionette()
orig = m.send("WebDriver:GetWindowHandle", {})["value"]
h = m.send("WebDriver:NewWindow", {"type": "tab", "focus": True})["handle"]
m.send("WebDriver:SwitchToWindow", {"handle": h, "focus": True})
m.send("WebDriver:Navigate", {"url": "https://web.gpuscore.com/api/tests/next"})
print("before:", m.send("WebDriver:GetCurrentURL", {})["value"])
m.js("setTimeout(function(){ window.location = \"https://httpbin.org/delay/8\"; }, 0); return 1")
time.sleep(0.5)
t0 = time.time()
subprocess.run(["sudo", "python3", "/tmp/tap-keys.py", "esc"])
print("esc tap took %.2fs" % (time.time() - t0))
time.sleep(12)
print("after 12s:", m.send("WebDriver:GetCurrentURL", {})["value"])
m.send("WebDriver:CloseWindow", {})
m.send("WebDriver:SwitchToWindow", {"handle": orig, "focus": True})

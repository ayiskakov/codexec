import os, socket, subprocess
def t(name, f):
    try: print(name, "->", f())
    except Exception as e: print(name, "-> BLOCKED:", type(e).__name__, e)
t("uid", os.getuid)
t("network", lambda: socket.create_connection(("1.1.1.1", 53), timeout=2) and "CONNECTED")
t("dns", lambda: socket.gethostbyname("example.com"))
t("read /etc/shadow", lambda: open("/etc/shadow").read()[:20])
t("list /root", lambda: os.listdir("/root"))
t("list /home", lambda: os.listdir("/home"))
t("write /usr/x", lambda: open("/usr/x", "w").write("x"))
t("list /", lambda: sorted(os.listdir("/")))
t("pids visible", lambda: len([p for p in os.listdir("/proc") if p.isdigit()]))
t("interfaces", lambda: os.listdir("/sys/class/net") if os.path.exists("/sys/class/net") else "no /sys")

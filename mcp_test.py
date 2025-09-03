#!/usr/bin/env python3
import json, sys, subprocess, threading, os

def pretty(obj):
    try:
        return json.dumps(obj, indent=2, ensure_ascii=False)
    except Exception:
        return str(obj)

def reader(proc):
    for raw in proc.stdout:
        line = raw.strip()
        if not line:
            continue
        try:
            msg = json.loads(line)
            # 简要摘要：codex/event 的类型与 session_id
            if msg.get("method") == "codex/event":
                params = msg.get("params") or {}
                t = ((params.get("msg") or {}).get("type")) or ""
                sid = ((params.get("msg") or {}).get("session_id")) or ""
                if sid:
                    print(f"[<-] codex/event type={t} session_id={sid}")
                else:
                    print(f"[<-] codex/event type={t}")
            print("[<-] " + pretty(msg))
        except Exception:
            print("[<-] " + line)

def main():
    # 启动 codex-mcp-server（可按需设置 CODEX_HOME）
    env = dict(os.environ)
    env.setdefault("RUST_LOG", "info")
    proc = subprocess.Popen(
        ["codex-mcp-server"],
        stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.STDOUT,
        text=True, bufsize=1, env=env
    )
    t = threading.Thread(target=reader, args=(proc,), daemon=True)
    t.start()

    rid = 0
    def send(msg):
        print("[->] " + pretty(msg))
        proc.stdin.write(json.dumps(msg) + "\n")
        proc.stdin.flush()

    # initialize
    rid += 1
    init = {
        "jsonrpc":"2.0","id":rid,"method":"initialize",
        "params":{"protocolVersion":"2025-06-18","capabilities":{"elicitation":
{}},"clientInfo":{"name":"cli","version":"0.0.1"}}
    }
    send(init)

    print("commands: list | call <tool-name> <json-args> | quit")
    for line in sys.stdin:
        line=line.strip()
        if not line: continue
        if line=="quit": break
        if line=="list":
            rid += 1
            send({"jsonrpc":"2.0","id":rid,"method":"tools/list","params":{}})
            continue
        if line.startswith("call "):
            try:
                _, name, arg = line.split(" ", 2)
                args = json.loads(arg)
            except ValueError:
                print("usage: call <tool-name> <json-args>")
                continue
            rid += 1
            send({"jsonrpc":"2.0","id":rid,"method":"tools/call","params":
{"name":name,"arguments":args}})
            continue
        print("unknown command")
    proc.terminate()

if __name__ == "__main__":
    import os
    main()

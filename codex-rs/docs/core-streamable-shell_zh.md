#+ 流式会话（PTY + stdin）数据流与实现解读

> 适用范围：`exec_command` / `write_stdin` 两个工具（由 `openai_tools.rs` 在启用“可流式 Shell”时挂载）。本篇用一张数据流图串起核心实现，并对窗口化输出与中部截断策略做深入说明。

---

## 总览：一次回合的数据流

```
    模型（Responses API）
        │ FunctionCall: exec_command{ cmd, shell, login,
        │                           yield_time_ms, max_output_tokens }
        ▼
    codex.rs::handle_function_call
        │ 解析参数 → sess.session_manager.handle_exec_command_request(params)
        ▼
    core/exec_command/session_manager.rs
        │ create_exec_command_session(params)
        │   ├─ portable_pty::openpty(rows=24, cols=80)
        │   ├─ CommandBuilder::new(shell)
        │   │     arg("-lc"|"-c"), arg(cmd)  // login=true 用 -lc
        │   ├─ child = pair.slave.spawn_command(...)
        │   ├─ writer_tx (mpsc)  → 供 write_stdin 使用
        │   ├─ output_tx (broadcast) ← 读取 PTY（spawn_blocking）
        │   └─ exit_rx (oneshot)  ← child.wait()（spawn_blocking）
        │
        │ 订阅 output_rx = output_tx.subscribe()（从此刻开始接收）
        │ 在 yield_time_ms 时间窗内收集所有 chunk（并允许进程先行退出）
        │ 截断策略：truncate_middle(字节上限≈max_output_tokens*4)
        │ 结果：ExecCommandOutput{ Exited(code) | Ongoing(session_id), output, original_token_count? }
        ▼
    codex.rs::handle_function_call
        │ result_into_payload → FunctionCallOutputPayload（字符串）
        ▼
    模型收到工具结果（字符串），可继续发起 write_stdin 交互
```

- 二次交互（write_stdin）：
```
    FunctionCall: write_stdin{ session_id, chars, yield_time_ms, max_output_tokens }
        ▼
    SessionManager::handle_write_stdin_request
        │ 取会话 writer_tx / 新订阅 output_rx
        │ 若 chars 非空 → writer_tx.send(chars.into_bytes)
        │ 在新窗口 yield_time_ms 内收集“从现在开始”的输出
        │ 应用相同的中部截断 → ExecCommandOutput{ Ongoing(session_id), ... }
        ▼
    返回 FunctionCallOutputPayload（字符串）
```

---

## 关键结构与通道拓扑

- 会话构造：`create_exec_command_session(params)`（session_manager.rs）
  - PTY：`pair = native_pty_system().openpty(PtySize{24x80})`
  - 子进程：在 `pair.slave` 上以 `shell -lc| -c cmd` 形式启动
  - 通道：
    - 写入：`writer_tx: mpsc<Vec<u8>>` → writer 任务（tokio）→ 同步写 PTY（spawn_blocking）
    - 输出：`output_tx: broadcast<Vec<u8>>` ← 读任务（spawn_blocking 读 PTY）
    - 退出：`exit_rx: oneshot<i32>` ← 等待任务（spawn_blocking child.wait）
    - killer：`child.clone_killer()`（当前未通过工具暴露，但内部可用于终止）
  - `ExecCommandSession` 聚合以上句柄并存入 `SessionManager.sessions`（HashMap<SessionId, ...>）

- 订阅语义（窗口化）：
  - `output_tx` 为 broadcast 通道，新订阅者只接收“订阅时刻之后”的数据。
  - `handle_exec_command_request` 和每次 `handle_write_stdin_request` 都会创建一个新的接收端，天然实现“本次窗口”的最新输出聚合。

---

## 输出格式与截断策略

- `ExecCommandOutput` 文本格式（to_text_output）：
```
Wall time: <secs> seconds
Process exited with code <code> | Process running with session ID <id>
[Warning: truncated output (original token count: N)]    // 若发生截断
Output:
<窗口内捕获的输出文本>
```
- 截断函数：`truncate_middle(s, max_bytes)`
  - 不截断：返回原文、`original_token_count=None`。
  - 截断：估算 tokens≈len/4，返回“首 + ‘…N tokens truncated…’ + 尾”构造；`original_token_count=Some(N)` 用于在文本前加 Warning。
  - 上限为 `max_output_tokens*4` 字节；传 0 将返回完整的 `…N tokens truncated…` 标记（保证可见反馈）。

---

## 与一次性 exec 的差异

- 一次性 exec（exec.rs）：
  - 可能发出 `ExecCommandOutputDeltaEvent`（限量）供 UI 实时显示；
  - 执行前后会有 `ExecCommandBegin/End` 事件。
- 流式会话：
  - 不发 Begin/End 事件；每次工具调用返回一个完整的字符串块（包含“Wall time/Process …/Output”）。
  - 输出窗口基于“订阅时刻”，避免重复历史内容；更适合 REPL/交互式程序或长时间运行命令的阶段性观察。

---

## 重要参数与行为

- `ExecCommandParams`（responses 工具入参）：
  - `cmd: String`：要执行的命令文本（由 shell 解析）。
  - `shell: String`：shell 可执行路径（例如 `/bin/bash`）。
  - `login: bool`：`true` 使用 `-lc`，`false` 使用 `-c`。
  - `yield_time_ms: u64`：本次窗口的收集时长（毫秒）。
  - `max_output_tokens: u64`：截断预算（字节≈tokens*4）。
- `WriteStdinParams`：
  - `session_id: u32`：`exec_command` 返回的会话 id。
  - `chars: String`：要写入 PTY 的字符；可为空（纯读取窗口）。
  - `yield_time_ms/max_output_tokens`：与上同。
- 进程退出：
  - 若在窗口内收到 `exit_rx`，会进行一个极短“grace”期以抽干缓冲后返回 `Exited(code)`。
  - 未退出则返回 `Ongoing(session_id)`，供后续继续交互。

---

## 误用与边界

- 大量输出：窗口化和截断可控，但请调小 `yield_time_ms` 与 `max_output_tokens`，避免模型“吞入”过多无效信息。
- 键入回车：将 `chars` 包含 `"\n"`；写入后再开一个短窗口以收集响应。
- 会话销毁：当前接口未暴露显式“结束会话/kill”工具；会话随子进程退出或进程终止而结束（内部有 `killer` 可用）。
- 多消费者：`broadcast` 支持多订阅，但当前实现每次工具调用会新订阅“此时窗口”，不会影响其他窗口读取。

---

## 端到端最小范式（示意）

```jsonc
// 1) 开启会话，抓取2秒输出
{
  "type": "function_call",
  "name": "exec_command",
  "arguments": {
    "cmd": "bash -lc 'python -u app.py'",
    "shell": "/bin/bash",
    "login": true,
    "yield_time_ms": 2000,
    "max_output_tokens": 2048
  }
}
// 返回文本中若为 "Process running with session ID X"，则继续 2)

// 2) 写入一行并读取1秒新输出
{
  "type": "function_call",
  "name": "write_stdin",
  "arguments": {
    "session_id": X,
    "chars": "start\n",
    "yield_time_ms": 1000,
    "max_output_tokens": 1024
  }
}
```

---

## 与工具挂载的关系（openai_tools.rs）

- 当 `ToolsConfig.use_streamable_shell_tool=true`：
  - 注册 `exec_command` 与 `write_stdin` 两个工具（均为 Responses API 的 function 工具）。
  - 其它情况下将采用 `shell/local_shell` 的一次性执行模型。

---

## 测试与可靠性要点

- I/O 线程：读取/写入均在阻塞线程中执行，规避 tokio 非阻塞约束对 PTY 的影响；遇到 `EINTR/WouldBlock` 会重试或短暂 sleep。
- 广播缓冲：`output_tx` 默认容量 256 条；若窗口订阅者 lagging 会收到 `Lagged` 并跳过旧块，保证“最新输出”。
- 截断元信息：`original_token_count` 仅为估算（4字节/Token），用于用户提示；不影响模型处理逻辑。

---

如需，我可以补充“会话生命周期管理/回收策略”的建议，或在 TUI 里加一个轻量窗口化展示示例，帮助交互式使用该工具。


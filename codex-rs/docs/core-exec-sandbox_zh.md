# 执行与沙箱链路深读（exec.rs → exec_env.rs/shell.rs/spawn.rs → safety.rs/is_safe_command.rs → seatbelt.rs/landlock.rs）

> 目标：从一次 `shell` 工具调用出发，串起“安全判定 → 选择沙箱 → 进程启动与 I/O → 超时/终止 → 输出回传”的完整链路，并说明 Linux/macOS 的差异实现。

---

## 一次性执行主干（core/src/exec.rs）

- 入口：`process_exec_tool_call(params, sandbox_type, sandbox_policy, codex_linux_sandbox_exe, stdout_stream)`
  - 分发：
    - `SandboxType::None` → `exec(..)` 本机直接 `spawn_child_async`。
    - `MacosSeatbelt` → `spawn_command_under_seatbelt(..)`，随后消费输出。
    - `LinuxSeccomp` → 通过 `codex-linux-sandbox` 可执行包装并消费输出。
  - 输出采集：`consume_truncated_output(child, timeout, stdout_stream)`
    - 并行读取 stdout/stderr 到缓冲；
    - 可选“增量事件”流：当传入 `StdoutStream` 时，最多发出 `MAX_EXEC_OUTPUT_DELTAS_PER_CALL` 条 `ExecCommandOutputDeltaEvent`（不限总输出，事件仅限量）；
    - 同步聚合 `aggregated_output`（完整聚合，不截断）。
  - 超时/中断：
    - `tokio::time::timeout` 包裹 `child.wait()`；超时即 `start_kill()` 并返回合成的 `ExitStatus`（Unix 用 `128 + TIMEOUT_CODE`）。
    - Ctrl-C 亦会触发 kill。
  - 结果：`ExecToolCallOutput { exit_code, stdout, stderr, aggregated_output, duration }`；若检测为“疑似沙箱拒绝”（非 None 沙箱且非 127/2 等常见码），映射为 `SandboxErr::Denied(exit_code, stdout, stderr)`。

- 给模型的格式化输出：`format_exec_output_str(exec_output)`
  - 预算：`MODEL_FORMAT_MAX_LINES=256`、`MODEL_FORMAT_MAX_BYTES=10KiB`；
  - 策略：截取 Head 与 Tail，中间插入 `[.. omitted N of TOTAL lines ..]` 标记；在字符边界裁剪（前缀/后缀字节预算）。
  - 注意：事件侧下发 `stdout/stderr/aggregated_output` 全量文本，格式化串仅用于模型侧 prompt。

---

## 环境与进程启动（exec_env.rs / shell.rs / spawn.rs）

- 环境构造：`create_env(policy: &ShellEnvironmentPolicy)`
  - 继承策略：`All/Core/None`；默认 Core（HOME/PATH/SHELL/USER 等）。
  - 默认排除：名称含 `KEY/SECRET/TOKEN`（不区分大小写），可关闭。
  - 自定义：`exclude`（通配符）→ 注入 `set` → `include_only` 白名单过滤。

- 进程启动：`spawn_child_async(program, args, arg0, cwd, sandbox_policy, stdio_policy, env)`
  - 清空继承环境，注入按策略生成的 env；
  - 若 `!sandbox_policy.has_full_network_access()`，向子进程设置 `CODEX_SANDBOX_NETWORK_DISABLED=1`；
  - Linux：通过 `pre_exec` 设置 `PR_SET_PDEATHSIG=SIGTERM`，当父进程退出时终止子进程；
  - I/O：
    - `RedirectForShellTool` → stdin=null，stdout/stderr=pipe（避免子进程阻塞等启发式读 stdin）。
    - `Inherit` → 继承父进程终端。

- 用户 shell 翻译（`codex.rs::maybe_translate_shell_command`）
  - 当用户 shell 是 PowerShell 或启用 profile 时，包装默认 shell 调用为用户 shell 期望的形式，避免 PATH/profile 差异。

---

## 安全判定与审批（safety.rs / is_safe_command.rs）

- 安全检查枚举：`SafetyCheck = AutoApprove { sandbox_type } | AskUser | Reject { reason }`
- `assess_command_safety(command, approval_policy, sandbox_policy, approved, with_escalated_permissions)`：
  - 若命令“已知安全”或用户已批准过 → `AutoApprove { None }`（无沙箱）。
  - 否则走 `assess_safety_for_untrusted_command(..)`：
    - `DangerFullAccess` → `AutoApprove { None }`。
    - `OnRequest` + `ReadOnly/WorkspaceWrite`：若 `with_escalated_permissions=true` 则 `AskUser`；否则若平台可用沙箱 → `AutoApprove { MacosSeatbelt|LinuxSeccomp }`，否则 `AskUser`。
    - `Never/OnFailure` + `ReadOnly/WorkspaceWrite`：若平台有沙箱 → `AutoApprove { sandbox }`；平台无沙箱时：`OnFailure` → `AskUser`，`Never` → `Reject`。
- “已知安全命令”白名单与限制：
  - 白名单：`cat/cd/echo/false/grep/head/ls/nl/pwd/tail/true/wc/which`，`git {branch,status,log,diff,show}`，`cargo check`，`sed -n <N|M,N>p FILE`；
  - `find` 禁止 `-exec/-execdir/-ok/-okdir/-delete/-fls/-fprint{,0}/-fprintf`；
  - `rg` 禁止 `--search-zip/-z` 与会执行外部命令的 `--pre/--hostname-bin[=]`；
  - `bash -lc`：脚本可解析为“仅词元命令序列”，且每个子命令均在白名单内时整体安全。

- 补丁安全（`assess_patch_safety`）
  - 若补丁写入范围被限制在 `SandboxPolicy` 的可写根（cwd、/tmp、TMPDIR 及额外 root，且顶层 `.git` 自动只读）内，或 `OnFailure`；平台有沙箱时可 `AutoApprove { sandbox }`；否则 `AskUser` 或在 `Never` 下 `Reject`。

---

## macOS Seatbelt（core/src/seatbelt.rs）

- 运行器：`/usr/bin/sandbox-exec`（只信任系统路径），以 `-p <policy>` 形式加载策略，随后 `-- <command..>`。
- 基础策略：`seatbelt_base_policy.sbpl` + 追加片段：
  - 读：`has_full_disk_read_access` 时 `(allow file-read*)`；
  - 写：
    - `DangerFullAccess` → `(allow file-write* (regex "^/"))`；
    - 其它：为每个可写根生成 `(subpath (param "WRITABLE_ROOT_i"))`；若该根下存在只读子路径（如 `.git`），改用 `(require-all (subpath ...) (require-not (subpath ...)))` 排除它。
  - 网络：允许网络时追加 `(allow network-outbound)` 等；否则空（网络受限）。
- 运行时参数：为每个根/子路径注入 `-DWRITABLE_ROOT_i=...`、`-DWRITABLE_ROOT_i_RO_j=...`；同时环境注入 `CODEX_SANDBOX=seatbelt`。

---

## Linux Landlock + seccomp（linux-sandbox crate）

- 运行器：`codex-linux-sandbox`（独立二进制）
  - CLI：`LandlockCommand { sandbox_policy_cwd, sandbox_policy, command... }`
  - 步骤：
    1) 在当前线程应用沙箱策略 `apply_sandbox_policy_to_current_thread(&sandbox_policy, &cwd)`：
       - 网络：安装 seccomp 规则拒绝典型网络 syscall（保留 AF_UNIX）。
       - 文件：安装 Landlock 规则：整盘 `file-read*`，允许 `/dev/null`；按可写根开启 `file-write*`。
    2) `execvp(command)` 执行目标命令（继承 cwd/env/argv）。
  - 在 `exec.rs` 的 `LinuxSeccomp` 路径下由 Core 调用，实现沙箱落地。

---

## 关联点与易错项

- 事件流量控制：`ExecCommandOutputDeltaEvent` 有数量上限，避免 UI 侧洪泛；聚合输出仍完整。
- 超时码：Unix 下合成为 `128 + 64`；`signal()` 情况也做了映射。
- PowerShell/profile：若启用，会在 `codex.rs` 侧翻译默认 shell 调用。
- 环境变量：网络关闭与沙箱标记相关的环境变量由 `spawn.rs/seatbelt.rs` 注入，供上层或测试探针使用。
- Linux 父进程退出：`PR_SET_PDEATHSIG=SIGTERM` 确保子进程不会成为孤儿长期占用资源。


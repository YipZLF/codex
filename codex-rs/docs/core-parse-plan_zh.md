# 指令解析与计划工具深读（parse_command.rs、bash.rs、plan_tool.rs）

> 目的：弄清 Codex 如何把“任意命令行”解析为可读摘要（供 UI 呈现与上下文记录），以及模型的计划工具 update_plan 的参数结构。

---

## 总览

- 入口：`core/src/parse_command.rs`
  - 将任意 `Vec<String>` 命令标记化、归一化、分段、归类为 `ParsedCommand`（协议侧等价枚举在 `protocol/src/parse_command.rs`）。
  - 关键环节：
    - bash -lc 特判（借助 tree-sitter-bash 做“仅词元”安全子集解析）。
    - 连接符与管道处理：`&&` `||` `;` `|` 分段。
    - 简化与去噪：丢弃小型格式化命令（head/tail/wc/awk…）和 `echo`/部分 `nl` 等噪音，保留主要动作。
    - 常见工具归类：`rg/fd/find/grep/cat/head/tail/nl/sed -n …p`、`ls`、`cargo/eslint/prettier/rustfmt/go/pytest`、`npm/yarn/pnpm/npx` 等。
- 辅助：`core/src/bash.rs`
  - `try_parse_bash`：用 tree-sitter 解析脚本。
  - `try_parse_word_only_commands_sequence`：仅接受“由单词与字符串组成的安全简单命令序列”（允许 &&、||、;、|），拒绝括号、重定向、变量展开、命令替换等复杂/不安全结构。
- 计划工具：`protocol/src/plan_tool.rs`
  - `UpdatePlanArgs { explanation?, plan: Vec<PlanItemArg{ step, status }] }`，`status ∈ {Pending, InProgress, Completed}`。

---

## 解析流水线（parse_command.rs）

1) bash -lc 快速路径：`parse_bash_lc_commands`
   - 匹配形如 `["bash", "-lc", script]`。
   - 用 `try_parse_bash(script)` 得到语法树 → `try_parse_word_only_commands_sequence` 提取每个“简单命令”的词元（右到左，随后反转为执行顺序）。
   - 丢弃“格式化/噪音”命令：`is_small_formatting_command`（wc/tr/cut/sort/uniq/xargs/tee/column/awk/yes/printf、无文件的 head/tail、非 `sed -n <range> file` 的 sed）。
   - 归类每个命令为 `ParsedCommand`（见下“归类规则”）。
   - 若最终只剩 1 条：
     - 对 Read/ListFiles/Search：根据是否包含连接符决定使用“原子 cmd”还是“原始 script”以确保展示上下文友好（保留 `sed -n` 场景的细节）。
     - 其它类型保持精简后的命令串。
   - 若匹配失败或序列含不支持结构，则回退为 `Unknown { cmd: script }`。

2) 常规路径
   - `normalize_tokens`：
     - 去掉前缀 `yes |`/`no |`。
     - 将 `bash -c/-lc "..."` 重新 shlex，避免重复解析。
   - `split_on_connectors`：按 `&& || | ;` 切分为片段，保序（左到右）。
   - `summarize_main_tokens`：对每个片段分类（见下）。
   - `simplify_once`：
     - `echo … && rest` → 去掉前缀 echo。
     - `cd foo && [后续含 Test]` → 去掉 cd。
     - `cmd || true` → 去掉 true。
     - `nl -<flags> && rest` → 去掉仅含 flag 的 nl。
   - 迭代应用简化直至收敛。

---

## 归类规则（节选）

- 搜索类
  - `rg`：
    - `--files`：query=None；`path` 为第一个非 flag 位置参（若有）。
    - 否则：第一非 flag 作为 query，第二非 flag 作为 path。
  - `fd`：解析 query/path（`-t f`、其他 flags 会跳过其值）。
  - `find`：解析 `-name '*.rs'` 等常见过滤，提取 path 与 pattern。
  - `grep`：第一非 flag 为 query，第二为 path（不对 query 做路径短化以保留正则）。
- 读取类
  - `cat [--] <file>` → Read{name=file}。
  - `head -n <N> <file>` / `tail -n [+]<N> <file>` → Read{name=file}（仅当存在明确文件操作数）。
  - `nl <file>`（带仅 flag 的 `-s/-w/-v/-i/-b`）→ Read{name=file}。
  - `sed -n '<range>p' <file>`（`<range>` 校验为数字或 `a,b`）→ Read{name=file}。
- 列表类
  - `ls`：跳过 `-I/-w/--block-size/--format/--time-style/--color/--quoting-style` 的值，从剩余参数取第一个非 flag 作为 path（并短化显示路径）。
- 代码质量/格式/测试
  - `cargo fmt`/`rustfmt`/`go fmt`/`prettier -w …` → Format{ tool, targets }。
  - `eslint …`、`npx eslint …`/`pnpm run lint`（转为 `pnpm-script:lint`）→ Lint{ tool, targets }。
  - `cargo test`/`go test`/`pytest`/`npm test`/`yarn test` → Test。
- 其它
  - `true` → Noop。
  - 不匹配 → Unknown{ cmd }。

辅助函数：
- `short_display_path(path)`：短化展示路径（去前缀、保留末段如 `.` 或文件名）。
- `skip_flag_values(args, flags_with_vals)`：跳过带值的 flag（如 `-s "  "`）。
- `trim_at_connector(tokens)`：在连接符处分割取左侧实参。

---

## bash 安全子集（bash.rs）

- 允许节点（命名）：`program/list/pipeline/command/command_name/word/string/string_content/raw_string/number`。
- 允许标点/运算符：`&& || ; |` 以及引号 token；其它（括号/重定向/替换/展开等）一律拒绝。
- 字符串处理：
  - 双引号：`"…"` 提取中间 `string_content`；
  - 单引号：`'…'` 去壳取内部；
- 提取到的 `Vec<Vec<String>>` 将用于后续归类与去噪。

---

## 计划工具（plan_tool.rs）

- 参数类型（协议侧定义）：
  - `StepStatus ∈ { Pending, InProgress, Completed }`
  - `PlanItemArg { step: String, status: StepStatus }`
  - `UpdatePlanArgs { explanation?: String, plan: Vec<PlanItemArg> }`
- 流程位置：
  - 模型触发 `update_plan` 工具 → Core 在 `codex.rs::handle_function_call` 中路由到 `plan_tool::handle_update_plan`（核心会将其作为事件供 TUI 消费与渲染）。

---

## 使用与调试建议

- 若想扩展某类命令的解析，先在 `parse_command.rs` 顶部的测试区添加/改写用例，再迭代实现；文件已提供大量示例（rg/find/fd/grep/ls/cat/head/tail/sed/nl/npm/pnpm/yarn/cargo/go/pytest…）。
- 解析失败不会影响执行，只影响 UI 概览与事件中的 `parsed_cmd`；保守回退为 `Unknown { cmd }`。
- bash -lc 分支可显著提升 pipelines 的“主语义”摘要效果（去噪后保留主要动作），但仅在脚本属于“安全简单子集”时启用。


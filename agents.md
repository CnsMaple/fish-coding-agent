# Agent Rules

## 修改前

- 优先查看相关文件和上下文，理解代码风格和项目结构。
- 修改代码前，先查看 git 历史（如 `git log` / `git blame` / `git diff`）了解这段代码之前为什么这样改，理解其意图和约束，再决定是否改动。
- 改动前评估影响范围：确认本次改动是否会影响其他调用方、配置或测试，避免引入副作用。

## 修改后

- 每次修改后必须执行以下检查，确保没有错误和警告：
  1. `cargo check`（需配合环境变量将警告视为错误，根据系统提示词中 `OS:` 选择对应语法）：
     - **Windows (pwsh)**：`$env:RUSTFLAGS = "-D warnings"; cargo check 2>&1`（注意 `&&` 无法连接变量赋值语句，必须用 `;`）
     - **Linux/macOS (bash/zsh)**：`export RUSTFLAGS="-D warnings" && cargo check 2>&1`（`export` 是内建命令，可用 `&&` 链式调用）
  2. `cargo clippy -- -D warnings`
  3. `cargo fmt -- --check`（如格式化检查失败，先 `cargo fmt` 再重新检查）
- 任何命令出现错误或未处理的警告都需要修复，并重新执行全部检查，直到全部通过，才认为本次修改完成。
- 注意：即使检查出的问题不是本次改动引入的，也需要一并修复，而不是仅修复自己的代码。

## Test 相关

- 修改涉及行为或逻辑时，必须运行测试验证，不能只通过编译检查。
- 运行测试的命令：
  - **Windows (pwsh)**：`cargo test 2>&1`
  - **Linux/macOS (bash/zsh)**：`cargo test 2>&1`
- 执行测试时，除当前正在修改的测试外，必须同时运行**全局测试**（默认 `cargo test` 即运行全部测试），确保本次改动不会引发其他测试失败。
- 若只想运行单个测试，用：`cargo test <测试名筛选词> 2>&1`（筛选词会匹配所有包含该词的测试）。
- 测试失败时：
  - 先修复失败测试对应的问题；
  - 修复后**必须重新运行全部测试**（`cargo test`），确认其他测试也全部通过；
  - 若全局测试出现与本次改动无关的既有失败，需判断是否由本次改动引入：若为既有问题，需记录并尽量避免扩大影响范围；若为本次改动引入，必须修复直到全部通过。

## 修改后完整检查流程

每次修改后，按以下顺序执行，全部通过才算完成：
1. `cargo check`（按 `OS:` 选择对应语法，将警告视为错误）
2. `cargo clippy -- -D warnings`
3. `cargo fmt -- --check`
4. `cargo test`（涉及逻辑/行为改动时，运行全部测试确认无回归）

## 日志入侵 TUI 注意事项（重要）

**TUI（含 ratatui 渲染）运行期间，绝对禁止任何日志直接写入 stdout/stderr 终端。** 任何调试输出、告警、错误信息一旦 `print!` / `println!` / `eprint!` / `eprintln!` 到终端，就会污染 TUI 的 alternate screen，导致界面错乱、布局被破坏、出现乱码（如 `[box_row_line] width mismatch` 刷屏）。

硬性约束：
- 所有日志/调试信息一律通过 `tracing::debug!` / `tracing::info!` / `tracing::warn!` / `tracing::error!` 输出——tracing 已全局配置为写入 `fish-coding-agent.log` 文件，不会污染终端。
- 严禁在 `src/session/render/`、`src/ui/`、`src/event/` 等 TUI 渲染/事件路径中新增任何 `println!`/`eprintln!`/`eprint!`/`print!` 或直接写 `std::stdout/stderr` 的输出代码。
- 如需在渲染路径中输出调试信息，改用 `tracing::warn!`（或按级别用 `debug!`）。
- 修改前先 `git log`/`git blame` 检查：历史上一旦出现 `eprintln!` 混入渲染路径，就是本次这类「日志入侵 TUI」问题的复发，务必阻止。
- 新增代码时，若不确定当前上下文是否在 TUI 渲染期间，一律走 `tracing`，绝不直接写终端。
- 测试代码（`#[cfg(test)]` 内）中的 `eprintln!` 不受此限制，但不得影响生产渲染路径。

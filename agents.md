# Agent Rules

## 修改前

- 优先查看相关文件和上下文，理解代码风格和项目结构。

## 修改后

- 每次修改后必须执行以下检查，确保没有错误和警告：
  1. `cargo check`（需配合环境变量将警告视为错误，根据系统提示词中 `OS:` 选择对应语法）：
     - **Windows (pwsh)**：`$env:RUSTFLAGS = "-D warnings"; cargo check 2>&1`（注意 `&&` 无法连接变量赋值语句，必须用 `;`）
     - **Linux/macOS (bash/zsh)**：`export RUSTFLAGS="-D warnings" && cargo check 2>&1`（`export` 是内建命令，可用 `&&` 链式调用）
  2. `cargo clippy -- -D warnings`
  3. `cargo fmt -- --check`（如格式化检查失败，先 `cargo fmt` 再重新检查）
- 任何命令出现错误或未处理的警告都需要修复，并重新执行全部检查，直到全部通过，才认为本次修改完成。
- 注意：即使检查出的问题不是本次改动引入的，也需要一并修复，而不是仅修复自己的代码。

# Agent Rules

## 修改前

- 优先查看相关文件和上下文，理解代码风格和项目结构。

## 修改后

- 每次修改后必须执行以下检查，确保没有错误和警告：
  1. `cargo check -- -D warnings`
  2. `cargo clippy -- -D warnings`
  3. `cargo fmt -- --check`（如格式化检查失败，先 `cargo fmt` 再重新检查）
- 任何命令出现错误或未处理的警告都需要修复，并重新执行全部检查，直到全部通过，才认为本次修改完成。

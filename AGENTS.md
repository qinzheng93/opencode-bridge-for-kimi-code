# AGENTS.md

## Git 工作流

- 按功能点拆分提交，每个提交单独跑过 `cargo fmt --check`、`cargo clippy --all-targets -- -D warnings`、`cargo test` 后再提交。
- 功能完成、验证通过后**自动 commit**，无需询问。
- **不主动 push**；等用户明确通知后再推送。

## 项目速览

- 二进制名 `ocbk`（crate/仓库名 `opencode-bridge`），CLI 子命令：serve（默认，前台）、start/stop/status/restart（daemon）、reload（SIGHUP 热加载）、set-key（写上游 key）、usage（查询上游用量）、gen-key（生成桥接 key）。
- 默认配置 `~/.config/ocbk/config.yaml`（`-c` 或 `OPENCODE_BRIDGE_CONFIG` 覆盖），目录自动创建；pid/日志文件与配置同目录同名换后缀。
- 构建与检查：`cargo build --release`；验收三件套见上。
- GitHub Actions：push/PR 运行 CI；推送 `v*` 标签发布 Linux amd64/arm64（musl 静态）和 macOS arm64 的 Release 压缩包及 SHA256 校验文件。

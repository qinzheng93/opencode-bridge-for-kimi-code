# opencode-bridge

让 [Kimi Code](https://github.com/MoonshotAI/kimi-code) 使用 OpenCode Go 订阅的本地转发桥。

OpenCode Go 要求每个推理请求携带稳定的 `x-opencode-session` 头来做按会话的 KV cache 路由，而 Kimi Code 目前发不出这个头。这个桥在本地转一层：用 Kimi Code 已经在请求体里发送的字段推导出会话 ID，替换凭据后转发给上游，同时对外提供一份 models.dev 格式的模型目录供 `kimi provider add` 导入。

## 它做什么

- **凭据隔离**：客户端用桥自己发布的 key 认证，桥再换成真实的 OpenCode Go key 访问上游。上游 key 只存在于配置文件（或环境变量）里，永远不会发给客户端。
- **注入 `x-opencode-session`**：OpenAI 风格路由取请求体里的 `prompt_cache_key`，Anthropic 路由取 `metadata.user_id`。映射是无状态的，所以桥重启、会话恢复都不受影响，并发会话也互不串台。
- **原样转发**：请求体逐字节转发，只改凭据、`host`、`content-length` 等必要头部；响应 SSE 不缓冲、不落盘，直接流式回传。
- **模型目录**：`/registry/api.json` 把 OpenCode Go 的模型清单和 models.dev 元数据合成 Kimi Code 可导入的 registry。

## 快速开始

需要 Rust 1.88 或更新（依赖里最高的 MSRV 来自 `icu_*`）；macOS 还需要 Xcode Command Line Tools。

```bash
cargo build --release
cp config.example.yaml config.yaml
# 编辑 config.yaml：至少填 upstream_api_key（OpenCode Go 的 key）
./target/release/opencode-bridge serve -c config.yaml --registry-protocols chat
```

首次运行如果 `config.yaml` 不存在，桥会按默认值生成一份（含自动生成的 `proxy_api_keys`）并以退出码 2 结束，提示你填上游 key 后重启。

启动横幅会打印一条导入命令，形如：

```bash
kimi provider add http://127.0.0.1:8787/registry/api.json
```

Kimi Code 侧通过环境变量携带桥的 key（变量名由 `api_key_env_name` 决定）：

```bash
export OPENCODE_BRIDGE_API_KEY=sk-bridge-...
```

## 端点

除了 `/healthz` 和 `/registry/api.json`，所有路由都要求桥的 API key（`Authorization: Bearer <key>`，也接受 `x-api-key` / `api-key`）。

| 方法 | 路径 | 说明 |
|---|---|---|
| GET | `/healthz` | 存活探针，无需认证 |
| GET | `/registry/api.json` | Kimi Code 模型目录，无需认证 |
| GET | `/v1/models` | 上游模型清单，透传 |
| POST | `/v1/chat/completions` | OpenAI 协议 |
| POST | `/v1/responses` | OpenAI Responses 协议 |
| POST | `/v1/messages` | Anthropic 协议 |
| POST | `/v1/messages/count_tokens` | 透传，不注入 session 头 |

以上路由同时挂在 `/zen/go/v1` 前缀下（registry 里发布的回连地址就是 `<base>/zen/go/v1`）。

## 配置项

所有字段都可省略，省略时使用默认值。`config.yaml` 含真实 key，不要提交到仓库。

| 字段 | 默认值 | 说明 |
|---|---|---|
| `listen` | `127.0.0.1:8787` | 监听地址，建议保持 loopback |
| `upstream_base_url` | `https://opencode.ai` | 上游地址 |
| `upstream_api_prefix` | `/inference/go/v1` | 上游路径前缀 |
| `upstream_api_key` | 空 | 真实的 OpenCode Go key |
| `proxy_api_keys` | 空 → 首次启动自动生成一个 | 允许访问本桥的 key 列表 |
| `api_key_env_name` | `OPENCODE_BRIDGE_API_KEY` | 写入 registry 的客户端环境变量名 |
| `registry_metadata_url` | `https://models.opencode.ai/api.json` | models.dev 格式的元数据源（`models.dev/api.json` 是同样内容的镜像） |
| `registry_ttl_seconds` | `900` | registry 缓存秒数，最小 60 |
| `max_body_bytes` | `83886080`（80 MiB） | 请求体上限，超出返回 413 |
| `upstream_timeout_seconds` | `300` | 上游读超时（两次收到数据之间的最大等待，SSE 按此判断中断） |

环境变量：`OPENCODE_BRIDGE_CONFIG` 指定配置路径，`OPENCODE_BRIDGE_UPSTREAM_API_KEY` 覆盖上游 key（优先级高于配置文件）。

## 回连地址与 `--host`

registry 里发布给客户端的地址由 `listen` 推导，不会再和实际监听端口对不上：

| listen | 发布地址 |
|---|---|
| `127.0.0.1:8787` | `http://127.0.0.1:8787` |
| `localhost:9000` | `http://localhost:9000` |
| `0.0.0.0:8787` / `[::]:8787` | `http://127.0.0.1:8787`（通配地址不可连，回落 loopback） |
| `[::1]:9000` | `http://[::1]:9000` |

Kimi Code 不在本机、或桥前面有反向代理时，用 `--host` 覆盖（可写 host、host:port 或完整 URL）：

```bash
opencode-bridge serve -c config.yaml --host 192.168.1.10
opencode-bridge serve -c config.yaml --host 192.168.1.10:9000
opencode-bridge serve -c config.yaml --host https://bridge.example.com
```

## 运维

- **热加载**：改完配置给进程发 `SIGHUP` 即可，无需重启。`proxy_api_keys` 立即生效；`upstream_timeout_seconds` 会重建连接池；`registry_metadata_url` / `registry_ttl_seconds` 在下一次刷新时生效。`listen` 和发布地址是启动时确定的，改了要重启。
- **日志**：输出到 stderr，本地时间，格式 `2026-10-17 02:31:40,211  INFO ...`；`--log-level debug|info|warning|error`。
- **生成 key**：`opencode-bridge gen-key [COUNT] [--prefix sk-bridge-] [--json]`。
- **registry 缓存**：TTL 内直接返回内存缓存；刷新失败时回退到上一次成功的结果（再延长 60 秒），冷缓存且上游不可达时返回 503。模型缺少元数据或上下文窗口时会被跳过并记一条 WARN。
- **`--registry-protocols`**：`chat,responses,anthropic` 的子集或 `all`（默认），只向 Kimi Code 发布选中的协议。

## 安全

- `config.yaml` 保存真实上游 key，已加入 `.gitignore`；权限建议 `chmod 600`。
- 默认只监听 `127.0.0.1`。确实需要对外提供服务时，请自行加反代和 TLS，并用 `--host` 指定对外地址。
- 客户端 key（`sk-bridge-...`）与上游 key 完全隔离；吊销只需从 `proxy_api_keys` 删掉再发 `SIGHUP`。

## 开发

```bash
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
```

代码结构：`src/main.rs`（CLI / 启动 / SIGHUP / 优雅停机）、`config.rs`（配置解析与校验）、`app.rs`（路由与认证）、`proxy.rs`（上游转发与流式）、`registry.rs`（模型目录合成与缓存）、`session.rs`（会话 ID 推导）、`keygen.rs`（key 生成）。

## License

MIT，见 [LICENSE](LICENSE)。

# telegram-bots

[English](README.en.md)

面向业务服务的多机器人 Telegram 文本通知网关。调用方只提交机器人别名、目的地别名和正文；token、聊天身份与调用权限由服务集中管理。修改外部 YAML 和凭据文件即可增删机器人、轮换凭据及调整授权，无需重建镜像或重启进程。

服务使用 crates.io 的 `nasa 2.0.1`，通过 `#[nasa::application("log", "web", "nacos-discovery")]` 由 napp 管理启动、业务初始化、就绪与停机，提供有界内存队列、每个 Telegram 身份的串行发送、独立排队容量、全局出站限额，以及可观测的配置拒绝和限时排空。`202 Accepted` 只确认内存受理，不表示 Telegram 已发送或用户已收到。

## 核心能力与边界

- 一次请求固定一个完整目录代，token、目的地、调用方凭据和权限一起切换；慢上传跨越切换时会被拒绝，不沿用已撤销权限。
- 完整候选连续两次读取一致才发布；未知字段、重复机器人身份、缺失材料、摘要不匹配或资源超限均保留旧目录。
- 同一 Telegram 数字身份在 token 轮换和别名变化时复用唯一消费队列；不同机器人可并行发送。
- 移除机器人先关闭新受理，再限时处理已受理消息。超时区分未发送丢弃和出站结果未知。
- 支持直接 HTTP 寻址与可选 Nacos 注册发现；Nacos 不承担目录配置中心职责。

只支持文本 `sendMessage`。不提供消息持久化、结果查询、自动补发、幂等存储、入站 Telegram webhook、集群选主或跨实例去重。同一 Telegram 身份只能由一个活动实例负责；默认部署为单副本，更新镜像时先停止旧实例。

## 应用生命周期

入口只声明框架组件并通过 `app` 登记 `telegram-catalog` hosted initializer。napp 负责运行时、信号、Web 路由、探针、指标与可选 Nacos 注册发现；初始化阶段校验外部目录和凭据，登记受管资源，随后把目录管理器暂存为 critical 任务。全部初始化和组件 Ready 成功后才启动消费者。初始化失败不开放监听；启动和运行阶段的 SIGTERM/SIGINT 均由 napp 处理，正常清理完成后返回成功退出码；运行中管理器异常退出会撤销受理权并触发统一停机。

入口中的 `app` 装配业务生命周期：

```rust
/// 业务作用：把通知目录生命周期交给 napp。
/// 参数说明：`app` 是尚未开放业务入口的应用容器。
/// 返回：登记成功后继续初始化，目录不可用时拒绝启动。
#[nasa::application("log", "web", "nacos-discovery")]
async fn main(app: nasa::Application) -> anyhow::Result<()> {
    telegram_bots::application::install(&app)?;
    Ok(())
}
```

服务只有一个可执行文件。每轮配置读取由主程序 fork 出只读子进程，通过匿名通道返回文件字节；父进程在内存中合并和校验，子进程不重新启动 napp、Nacos 或 HTTP 服务。受管清理动作负责回收启动阶段的读取进程。`zcf/application.yml` 和活动 profile 由 napp 标准入口同步读取，必须使用可靠、不可变的本地文件；它们的首次读取不受业务 `catalog_watch.load_timeout_ms` 限制。外部目录与凭据的文件读取有超时和进程回收保护。

## 源码组织

`src/` 根目录只保留 `main.rs` 应用入口与 `lib.rs` 模块入口，业务代码按职责归类：

| 目录 | 职责 |
| --- | --- |
| `rest/` | REST 认证拦截器、机器人查询与消息提交、配置状态接口 |
| `application/` | 通过 app 登记初始化、就绪贡献、关键任务及受管资源清理 |
| `service/` | 调用方权限、目的地选择、通知受理、Telegram 请求与错误分类 |
| `catalog/` | 强类型配置、文件读取隔离、材料校验、热更与目录原子发布 |
| `partition/` | 按 Telegram 身份划分的串行队列、容量预算和消息终态计数 |
| `observability/` | 配置与发送指标的描述、采集和输出 |

`partition/` 是本服务的机器人消费域实现。运行架构、发布顺序与停机边界见[架构说明](docs/architecture.md)。

## 日志

日志使用 NASA 的 `nalog` 组件，通过 `log` feature 与应用宏中的 `"log"` 交给 napp 管理。现有 `tracing` 日志共用这一输出，初始化时建立控制台输出，业务资源停止后关闭文件输出并刷盘。

默认级别为 `info`，同时写控制台及 `/usr/local/logs/telegram-bots` 下的 `info.log` 和独立的 `error.log`。设置 `TELEGRAM_LOG_LEVEL=warn` 可调整级别；`TELEGRAM_LOG_PATH=/绝对路径/日志目录` 可覆盖目录，显式设置为空则只写控制台。日志目录必须可写。本项目文件策略为按天或单文件达到 100 MiB 滚动、保留 7 天；`info` 与 `error` 各自的归档容量上限为 1 GiB，不包含当前活动文件。清理随启动和滚动执行。

日志配置位于引导文件的 `log` 段，调整后重启应用；外部机器人 YAML 不接受日志设置。默认路径是固定值，不随 `application.name` 改变；当前 `naml 2.0.0` 不支持可靠的嵌套占位符默认值。容器启用文件输出时需挂载 UID 10001 可写的专用目录；显式关闭文件输出后可仅使用只读根文件系统。详细配置与边界见[配置合同](docs/configuration.md)。

## 快速开始

支持 Linux 与 macOS，需要 Rust 1.94 或更新版本。所有产品依赖从 crates.io 解析，`Cargo.lock` 固定完整依赖图。

```sh
cargo build --locked --release --bin telegram-bots
mkdir -p deploy-local
cp examples/catalog-empty.yml deploy-local/telegram-bots.yml
export TELEGRAM_YML="$PWD/deploy-local/telegram-bots.yml"
export TELEGRAM_LOG_PATH="$PWD/logs"
./target/release/telegram-bots
```

构建只生成 `telegram-bots`，Docker 镜像也只安装这个程序。准备好上述配置与环境变量后，开发环境可直接执行 `cargo run --locked`；IDE 设置同一 `TELEGRAM_YML` 绝对路径及 `TELEGRAM_LOG_PATH` 可写目录，并将工作目录设为项目根目录即可。未设置 `TELEGRAM_YML` 时读取 `/etc/conf/telegram-bots.yml`；文件不存在会拒绝启动。

监听地址默认为 `0.0.0.0:2060`。另一个终端访问：

```sh
curl --fail http://127.0.0.1:2060/readyz
curl --fail http://127.0.0.1:2060/metrics
```

空目录允许服务启动，但不提供业务调用凭据。`zcf/application.yml` 是引导文件，外部目录路径通过 `TELEGRAM_YML` 在启动时选择。镜像使用不可变的引导文件，外部目录默认 `/etc/conf/telegram-bots.yml`，具体操作见[容器部署](docs/container-deployment.md)。

## 配置一个机器人

以 [examples/catalog.yml](examples/catalog.yml) 为目录模板，完成以下内容后原子替换外部文件：

1. 将机器人加入目标聊天，并授予发送所需权限。填写真实 `chat_id`；示例中的 ID 仅说明格式。
2. 将 bot token 与至少 32 字节的高熵调用方凭据分别写入受限文件。YAML 中只保留 `secret://` 引用、绝对路径与文件 SHA-256。
3. 用 `shasum -a 256 /绝对路径/凭据文件` 计算原始文件字节的摘要，替换模板中的 `REPLACE_WITH_64_HEX_SHA256`。Linux 也可使用 `sha256sum`。
4. 本地运行时将两个 `secrets.*.file` 改为真实绝对路径；容器内使用 `/run/secrets/...`。填写调用方 `allowed_bots`。
5. 将 `generation` 改为大于当前已应用代号且不超过 `9007199254740991` 的正整数；从空目录开始时使用 `2`。先写同目录临时文件，再原子重命名为 `telegram-bots.yml`。

不需要修改镜像内文件。默认每秒重新读取完整来源，连续两轮一致才发布。进程正常调度且本地文件读取较快时，稳定变更通常约 1–2 秒生效，另加读取与校验耗时。文件落地后超过 10 秒仍未生效应检查配置状态；这不是保证生效的硬上限。外部目录与凭据在启动和运行期都由辅助进程读取，默认 3 秒超时后终止并回收，再继续轮询；回收最多额外等待 1 秒。运行期读取超时保留旧目录，启动超时拒绝监听并退出；无法回收时服务失败停机。平台投射延迟或材料尚未匹配会延长等待。

详细字段、合并顺序、材料一致性和导入限制见[配置合同](docs/configuration.md)。

## 业务微服务接入

业务方通过普通 HTTP JSON 调用，不需要使用 NASA 或 Rust，也不需要持有 Telegram bot token。接入前由部署者提供四项信息：**网关基础地址、调用方 ID、该调用方的认证凭据、可用的机器人与目的地别名**。空目录启动示例没有业务身份，必须先完成授权配置才能调用 `/api/`。

### 地址如何填写

默认监听 `0.0.0.0:2060`；`0.0.0.0` 是监听地址，不能作为业务配置中的目标地址。基础地址不包含 `/api`，调用时再拼接下表中的路径。

| 调用位置 | 基础地址示例 | 前提 |
| --- | --- | --- |
| 与网关同一台主机的进程 | `http://127.0.0.1:2060` | 网关直接运行在该主机，或已映射宿主机端口 |
| 同一 Docker 自定义网络中的其它容器 | `http://telegram-bots:2060` | 网关容器名或网络别名为 `telegram-bots`，两个容器加入同一网络 |
| 同一 Kubernetes namespace 的 Pod | `http://telegram-bots:2060` | 已创建部署文档中的 `telegram-bots` Service |
| 不同 Kubernetes namespace 的 Pod | `http://telegram-bots.<namespace>.svc:2060` | 将 `<namespace>` 替换为网关所在 namespace，网络策略允许访问 |
| 其它服务器上的业务服务 | `https://通知网关域名` | 部署者提供可达的内网地址或 TLS 代理地址 |

容器里的 `127.0.0.1` 指向该容器自身，不能用于访问另一个容器。Docker 示例只向宿主机回环地址映射端口，远程主机不能直接访问该映射。使用同网络寻址或受控代理；详细配置见[容器部署](docs/container-deployment.md)。

启用 Nacos 时，默认注册服务名为 `telegram-bots`。业务侧在对应 namespace/group 中发现健康实例，使用其注册 IP 与端口构造基础地址，再调用同一套 HTTP 接口；Nacos 不会免除认证，也不是请求转发代理。实际注册名称和地址以部署设置为准。

### 调用身份与授权

每个业务微服务应使用独立的 `telegram.clients.<client_id>` 身份和独立凭据。例如 [catalog.yml](examples/catalog.yml) 中的调用方 `monitoring` 获准使用机器人 `ops`；机器人下的 `alerts` 是服务端预先配置的聊天目的地。业务代码传这两个别名即可。

| 请求头 | 是否必需 | 值 |
| --- | --- | --- |
| `X-Client-Id` | 所有 `/api/` 请求必需 | 配置中的 client_id，例如 `monitoring` |
| `Authorization` | 所有 `/api/` 请求必需 | `Bearer <调用方凭据原文>`；不是 Telegram token，也不是文件 SHA-256 |
| `Content-Type` | 发送消息必需 | `application/json` |

两个身份头各只能出现一次。调用方只能使用自身 `allowed_bots` 列表中的机器人。bot 授权涵盖该 bot 下全部目的地，当前没有按目的地细分调用方权限；需要隔离访问时应调整机器人划分。

业务端将基础地址和 ID 放入自己的配置，将凭据通过 Secret 或受控环境注入。下列命令假定 `TELEGRAM_CLIENT_KEY` 已安全注入；不要把真实凭据写入源码、镜像或命令记录。部署使用明文 HTTP 时限于可信网络，跨越信任边界使用 TLS 代理。

```sh
export TELEGRAM_BOTS_BASE_URL='http://127.0.0.1:2060'
export TELEGRAM_CLIENT_ID='monitoring'
```

这两个变量只是下面调用示例的客户端设置，不是网关服务端的配置键。

### 第一步：查询可用机器人

```sh
curl -i --connect-timeout 2 --max-time 10 \
  "$TELEGRAM_BOTS_BASE_URL/api/bots" \
  -H "X-Client-Id: $TELEGRAM_CLIENT_ID" \
  -H "Authorization: Bearer $TELEGRAM_CLIENT_KEY"
```

成功返回 HTTP 200：

```json
{"bots":[{"bot_id":"ops","description":"运维通知","destinations":["alerts"],"default_destination":"alerts","messages_path":"/api/bots/ops/messages"}]}
```

`bot_id` 用于发送路径，`destinations` 提供可选目的地别名，`messages_path` 是相对基础地址的发送路径。`default_destination=null` 表示调用时必须选择目的地。返回 `bots: []` 表示身份有效但没有可用机器人。目录不暴露 bot token、真实 chat_id 或其它调用方权限。

### 第二步：发送文本消息

**请求：`POST /api/bots/{bot_id}/messages`**。`{bot_id}` 是查询结果中的机器人别名，不是 Telegram 用户名或数字身份。接口没有查询参数，正文为 JSON 对象。

| JSON 字段 | 类型 | 是否必需 | 语义与默认行为 |
| --- | --- | --- | --- |
| `text` | string | 是 | 非空白文本，最多 4096 个 UTF-16 单元；多数常用中文字符占 1 个，部分 emoji 占 2 个；不自动拆分 |
| `destination` | string | 否 | 该 bot 的目的地别名；省略时使用默认目的地，没有默认值时必须填写 |
| `parse_mode` | string | 否 | 只接受 `HTML` 或 `MarkdownV2`；省略时按普通文本处理；调用方负责对应格式的转义 |
| `disable_notification` | boolean | 否 | `true` 表示静默通知；省略时使用 bot 配置，默认 `false` |
| `protect_content` | boolean | 否 | `true` 表示请求 Telegram 保护内容；省略时使用 bot 配置，默认 `false` |

不接受自定义 `token`、`chat_id`、`message_thread_id` 或任意额外字段；聊天和论坛话题由服务端目的地配置决定。整个 JSON 请求默认最多 65536 字节，上传期限默认 5 秒；部署者可调整这两个启动参数。文本长度与 JSON 字节大小是两项独立限制。

最小请求：

```sh
curl -i --connect-timeout 2 --max-time 10 \
  "$TELEGRAM_BOTS_BASE_URL/api/bots/ops/messages" \
  -H "X-Client-Id: $TELEGRAM_CLIENT_ID" \
  -H "Authorization: Bearer $TELEGRAM_CLIENT_KEY" \
  -H 'Content-Type: application/json' \
  --data '{"destination":"alerts","text":"订单服务：任务处理延迟超过阈值"}'
```

包含全部可选字段的请求正文示例：

```json
{"text":"<b>订单告警</b>：任务处理延迟超过阈值","destination":"alerts","parse_mode":"HTML","disable_notification":false,"protect_content":true}
```

成功返回 **HTTP 202 Accepted**：

```json
{"bot_id":"ops","destination":"alerts","accepted":true}
```

三个字段分别是实际选择的机器人、目的地和内存受理标识。响应不提供 `message_id`、任务 ID 或结果查询地址。收到 202 后不要因尚未看到 Telegram 消息而立即重发；队列会按同机器人受理顺序尝试发送，发送间隔、平台冷却和队列积压都会影响实际到达时间。

### 业务代码如何处理结果

HTTP 客户端应复用连接池并设置连接、响应期限。上面的 2 秒连接与 10 秒总期限是客户端示例值，应按部署网络调整；请求仅等待入队，不等待 Telegram 发送完成。关闭对 POST 网络错误的无条件自动重试。

| HTTP 结果 | 含义 | 业务侧处理 |
| --- | --- | --- |
| 202 | 已进入有界内存队列 | 记录为网关已受理，不标记为 Telegram 已送达 |
| 400 | 空白或超长文本、目的地无效等 | 调整内容或目的地后提交 |
| 401 | ID/凭据错误、身份头重复或凭据已轮换 | 核对当前调用方 ID 和凭据 |
| 403 | 没有目标 bot 的权限 | 联系部署者调整授权或重新查询目录 |
| 404 | 路由或目录未命中 | 核对基础地址、API 路径和 bot 别名 |
| 408 | 上传超时，未入队 | 排除慢上传后重新提交 |
| 413 | JSON 请求过大 | 缩小请求；不要把附件编码进 text |
| 415 / 422 | 媒体类型、JSON 字段或类型不符 | 使用 JSON 并按参数表调整 |
| 429 | 机器人队列或进程容量已满 | 确认未受理后，采用有次数和总期限上限的退避重试 |
| 503 | 未 Ready、停机或上传期间目录已切换 | 确认未受理后，刷新目录/凭据并有限退避 |
| 连接中断、客户端超时、代理 502/504 | 无法确定网关是否已经受理 | 不盲目补发，由业务决定重复通知风险与后续处置 |

业务端点的拒绝正文示例：

```json
{"error":{"code":"queue_full","message":"通知队列或进程预算已满","delivery":"not_sent","retry_safe":true}}
```

`delivery=not_sent` 表示本次请求未入队；`retry_safe=true` 表示该拒绝允许稍后重试，不表示请求具有幂等性。路由、媒体类型、代理或框架层错误可能采用不同 JSON 结构；先检查 HTTP 状态，再解析已识别的信封，不能把所有非 2xx 都认作可安全重发。

服务每条受理消息最多进行一次 Telegram 请求，不自动补发。202 之后的明确失败、结果未知或停机丢弃只体现在日志和指标中，没有异步回调或逐条结果查询接口。若通知不能丢失，业务方应保留持久任务，并设计重复通知与结果未知的处理策略；本服务不替代持久消息系统。

### 配置状态和连通性

| 接口 | 是否需要业务身份 | 用途 |
| --- | --- | --- |
| `GET /api/config/status` | 是 | 服务级目录代号、最近拒绝、排空状态；不是消息发送结果 |
| `GET /readyz` | 否 | 服务是否可受理流量，供内部就绪探针使用 |
| `GET /healthz` | 否 | 内部存活探针 |
| `GET /metrics` | 否 | 内部监控采集；包含目录状态和聚合发送计数 |

所有认证调用方均可访问服务级配置状态，它不按调用方过滤为各自视图。排查目录或授权变更时可调用：

```sh
curl -i --connect-timeout 2 --max-time 10 \
  "$TELEGRAM_BOTS_BASE_URL/api/config/status" \
  -H "X-Client-Id: $TELEGRAM_CLIENT_ID" \
  -H "Authorization: Bearer $TELEGRAM_CLIENT_KEY"
```

业务接入顺序为：确认内网地址可达 → 使用凭据查询 `/api/bots` → 选定 bot/目的地发送 → 按 202 与拒绝结果分类处理。完整控制面响应和接口细节见 [HTTP API](docs/http-api.md)。探针和监控端点应限制到内部网络；配置错误保留旧目录时 `/readyz` 仍可能正常。

## 架构与运维

[架构说明](docs/architecture.md)描述配置发布、唯一消费域、容量和停机顺序。[运维说明](docs/operations.md)列出指标、轮换、排空与故障处置。

HTTP 连接与缓冲预算、应用身份、listener、配置来源、观察开关和 Nacos 设置属于启动配置，修改需要重启。机器人目录、权限、目的地、发送间隔及队列容量可动态调整。

配置无效不会主动撤销旧目录的 readiness。因此业务探针正常不代表最近部署已生效；应同时观察 applied/desired generation、最近拒绝状态与最近对账时刻。

不要向公网直接开放明文 listener、`/metrics`、`/healthz` 或 `/readyz`。在可信代理终止 TLS，并使用网络策略限制业务与监控流量。安全边界和凭据泄露处置见[安全说明](SECURITY.md)。

## 参与与许可

开发入口见[贡献说明](CONTRIBUTING.md)。项目按 [MIT](LICENSE-MIT) 或 [Apache-2.0](LICENSE-APACHE) 双许可证提供，可任选其一。

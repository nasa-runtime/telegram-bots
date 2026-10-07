# HTTP API

## 统一响应

业务接口全部返回 HTTP 200，正文采用 `nasa::base::BaseResponse` 的 `code`、`msg`、`data`。数字 `code=200` 表示处理成功，其它 code 表示处理失败；成功响应省略 msg，失败 msg 为固定摘要。未设置（`None`）的 msg、data 不序列化，code 始终保留；空数组、空对象、0 和 false 是有效业务值，仍按原值输出。目录、配置状态及消息受理数据均位于 data。探针 `/healthz`、`/readyz` 保留真实 HTTP 状态，`/metrics` 保留原监控格式。

## 身份

所有 `/api/` 端点要求唯一 `X-Client-Id` 和唯一 `Authorization: Bearer <credential>`。重复身份头、未知身份或错误材料返回 code=401，不先读取正文。调用方凭据以摘要比较；请求不能自报 Telegram token 或任意 chat_id。

`/healthz`、`/readyz`、`/metrics` 是无业务凭据的基础设施入口，应限制到内部探针和监控网络。

## 路由

| 方法和路径 | 行为 |
| --- | --- |
| `GET /api/bots` | 按别名排序返回调用方有权使用的 bot 与目的地别名，不暴露 chat_id 或材料 |
| `POST /api/bots/{bot_id}/messages` | 校验并入队；成功返回 code=200 |
| `GET /api/config/status` | 返回服务级部署代号、最近拒绝及排空中的 bot 别名；所有认证调用方可读取 |

目录响应示例：

```json
{"code":200,"data":{"bots":[{"bot_id":"ops","description":"运维告警通知","destinations":["alerts"],"default_destination":"alerts","messages_path":"/api/bots/ops/messages"}]}}
```

提交消息需要 `Content-Type: application/json`：

```json
{"text":"通知正文","destination":"alerts","parse_mode":"HTML","disable_notification":false,"protect_content":false}
```

只有 `text` 必需。非空正文最多 4096 个 UTF-16 单元，不自动拆分。`destination` 省略时使用配置默认值；`parse_mode` 省略时不解析富文本，只接受 `HTML` 或 `MarkdownV2`。后两个布尔值省略时使用 bot 默认值。未知字段或错误类型返回 code=422；具体富文本实体有效性由 Telegram 决定。

成功响应只表示内存受理，不携带 Telegram message_id：

```json
{"code":200,"data":{"bot_id":"ops","destination":"alerts","accepted":true}}
```

控制面响应示例：

```json
{"code":200,"data":{"desired_generation":12,"applied_generation":11,"last_success_unix_seconds":1780000000,"last_check_unix_seconds":1780000010,"last_rejection":"凭据内容与目录声明的 sha256 不一致","rejected_attempts":1,"draining_bots":[],"stopping":false}}
```

时间字段是 Unix 秒。合法代号范围为 `1..=9007199254740991`；`desired_generation=null` 表示最近一轮未能解析出合法代号，包括越界候选，此时对应指标为 0。控制面属于服务级运维信息，不是某个调用方的发送结果查询接口。

## 拒绝与重试

业务端点拒绝使用统一错误信封：

```json
{"code":429,"msg":"通知队列或进程预算已满","data":{"reason":"queue_full","delivery":"not_sent","retry_safe":true}}
```

| JSON code | 典型原因 | 调用方处理 |
| --- | --- | --- |
| 400 | 空正文、超长文本或目的地无效 | 修改消息 |
| 401 | 身份或凭据无效、重复身份头 | 更新认证信息 |
| 403 | 未获 bot 授权，包括从权限表移除的 bot | 检查目录及授权 |
| 404 | 路由不存在或目录未命中 | 核对路径 |
| 405 | 请求方法不支持 | 按路由表选择方法 |
| 500 | 处理异常，副作用未知 | 不自动重发，排查日志与业务状态 |
| 408 | 正文读取超过期限，未入队 | 消除慢上传后重新提交 |
| 413 | 请求正文超出缓冲上限 | 缩小请求 |
| 415 | 媒体类型不支持 | 使用 JSON |
| 422 | JSON 字段或类型无效 | 修改消息结构 |
| 429 | bot 或进程容量不足 | 退避后提交 |
| 503 | 入口过载、应用未 Ready、停机或上传期间目录已切换 | 重新获取目录与当前凭据后提交 |

业务路由、方法、媒体类型、认证、容量和入口过载拒绝均使用上述外壳，HTTP 状态仍为 200。data.reason 是固定原因，data.delivery 和 data.retry_safe 表达发送与重试边界。不能只检查 HTTP 状态，也不能仅凭 msg 文本分支。代理或无法解析的 HTTP 协议请求不保证具有本服务外壳。

Telegram 的发送结果发生在 code=200 之后，只进入脱敏日志及指标，不会把 REST 响应改成 502/504。上游明确拒绝计为 `failed`；无法判断是否已送达计为 `unknown`。平台限流会冷却后续工作，但不会重发本条消息。

如果连接在 code=200 响应返回前中断，客户端无法仅凭网络错误判断是否已受理；服务不提供幂等键或结果查询。不要对所有网络错误无条件重试，否则可能重复通知。

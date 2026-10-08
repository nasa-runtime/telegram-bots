# telegram-bots

基于 Rust 与 NASA 的 Telegram 通知网关，支持配置热更新、调用方鉴权、有界内存队列和同机器人消息保序。业务服务通过 HTTP 提交通知，不需要持有 Telegram bot token。

A Rust Telegram notification gateway with atomic configuration reloads, caller authentication, bounded queues and per-bot ordering. Business services submit notifications over HTTP without receiving bot tokens.

- Image: `nasaruntime/telegram-bots:1.0.0`; `latest` follows the current published image.
- Platforms: `linux/amd64`, `linux/arm64`.
- Runtime: Alpine, musl, non-root UID/GID `10001:10001`.
- License: MIT OR Apache-2.0.
- Source and Dockerfile: [GitHub](https://github.com/nasa-runtime/telegram-bots), [Dockerfile](https://github.com/nasa-runtime/telegram-bots/blob/release/docker/Dockerfile).

## 启动 / Start

The default import expression is `${TELEGRAM_YML:/etc/telegram-bots/*.yml}`. Mount the configuration directory; no image rebuild is needed when bot configuration changes.

```sh
mkdir -p deploy-local/config deploy-local/secrets
cat > deploy-local/config/telegram-bots.yml <<'YAML'
generation: 1
telegram:
  bots: {}
  clients: {}
secrets: {}
YAML

docker run --detach --name telegram-bots \
  --read-only --cap-drop ALL --security-opt no-new-privileges \
  --env TELEGRAM_LOG_PATH= \
  --publish 127.0.0.1:2060:2060 \
  --mount "type=bind,src=$PWD/deploy-local/config,dst=/etc/telegram-bots,readonly" \
  --mount "type=bind,src=$PWD/deploy-local/secrets,dst=/run/secrets,readonly" \
  nasaruntime/telegram-bots:1.0.0

curl --fail http://127.0.0.1:2060/readyz
docker logs --tail 100 telegram-bots
```
此示例包含空业务目录，服务可就绪，但尚未配置机器人与调用凭据。参照 [配置模板](https://github.com/nasa-runtime/telegram-bots/blob/release/examples/catalog.yml) 填写机器人、目的地、调用方、凭据路径和 SHA-256。真实凭据保存在 `/run/secrets` 的只读目录挂载中，允许 UID 10001 或受控组读取。

The starter catalog has no bots or authenticated callers. Configure them using the [catalog template](https://github.com/nasa-runtime/telegram-bots/blob/release/examples/catalog.yml). Mount credential files read-only at `/run/secrets` and reference their paths and SHA-256 hashes in YAML.

## 配置与运行边界 / Configuration and limits

- 文件按自然名称顺序合并，后者覆盖前者。每个文件的 `generation` 必须相同；修改内容时递增 generation。错误候选保留旧配置。
- 默认一秒轮询，并要求连续两次读取一致。正常调度与本地快速读取时，稳定变更通常约一至两秒生效，加上读取与验证时间；平台文件投射延迟另计。
- 挂载整个目录，不要使用单文件 bind mount 或 Kubernetes `subPath`。临时配置文件使用不匹配 `.yml` 的后缀，再原子重命名。
- 默认同时输出控制台和 `/usr/local/logs/telegram-bots` 文件日志。上例设置 `TELEGRAM_LOG_PATH=` 仅输出控制台；需要文件日志时去掉该设置并挂载 UID 10001 可写的日志目录。
- `/healthz` 与 `/readyz` 使用实际 HTTP 状态。业务 `/api/` 返回 HTTP 200，处理结果在 JSON `code` 中；未设置的 `msg`、`data` 省略。
- 消息成功受理仅表示进入有界内存队列，不保证 Telegram 已送达。没有持久化、崩溃重放或跨副本消息协调；同一机器人应只由一个服务实例发送。
- 停机使用 `docker stop --time 70 telegram-bots`。不要将 token 或调用方凭据放入镜像、源代码或公开环境示例。

Configuration files merge in natural filename order, with later files winning. Use one shared, increasing `generation` across the complete set. Invalid updates preserve the active catalog. Mount whole directories for atomic replacement and retain the image's `/app/zcf/application.yml`. Logging defaults to console and files; the example explicitly selects console-only output for its read-only root.

The API accepts messages into memory and sends them in per-bot order. It does not persist queues or coordinate multiple instances. An HTTP 200 alone does not indicate business success: inspect JSON `code`. Acceptance does not confirm Telegram delivery.

## 业务调用 / Business API

业务地址为 `http://<gateway>:2060`；宿主机回环映射只允许本机访问。同网络业务容器可使用 `http://telegram-bots:2060`。远程访问使用受控网络与 TLS 代理。

- `GET /api/bots`: discover permitted bots and destinations.
- `GET /api/config/status`: view catalog application status.
- `POST /api/bots/{bot_id}/messages`: submit a message, for example `{"destination":"alerts","text":"订单服务通知"}`.
- Every business request requires `X-Client-Id` and `Authorization: Bearer <caller credential>`; POST also requires `Content-Type: application/json`.
- Success example: `{"code":200,"data":{"bot_id":"ops","destination":"alerts","accepted":true}}`.

Complete field definitions, permissions, error codes, retry semantics, Nacos and Kubernetes configuration: [中文 README](https://github.com/nasa-runtime/telegram-bots/blob/release/README.md), [English README](https://github.com/nasa-runtime/telegram-bots/blob/release/README.en.md), [deployment](https://github.com/nasa-runtime/telegram-bots/blob/release/docs/container-deployment.md).

# telegram-bots

基于 Rust 与 NASA 的 Telegram 通知网关，支持配置热更新、调用方鉴权、有界内存队列和同机器人消息保序。业务服务通过 HTTP 提交通知，不需要持有 Telegram bot token。

A Rust Telegram notification gateway with atomic configuration reloads, caller authentication, bounded queues and per-bot ordering. Business services submit notifications over HTTP without receiving bot tokens.

- Image: `nasaruntime/telegram-bots:1.0.0`; `latest` follows the current published image.
- Platforms: `linux/amd64`, `linux/arm64`.
- Runtime: Alpine, musl, non-root UID/GID `10001:10001`.
- License: MIT OR Apache-2.0.
- Source and Dockerfile: [GitHub](https://github.com/nasa-runtime/telegram-bots), [Dockerfile](https://github.com/nasa-runtime/telegram-bots/blob/release/docker/Dockerfile).

## 配置与完整启动

### 准备配置目录

以下步骤供两种启动方式共用。在同一工作目录执行，已有业务配置时不要覆盖原文件。空业务目录可启动并就绪；真正发送消息还需按下文“配置一个机器人”填写机器人、目的地、调用方及凭据。

```sh
mkdir -p deploy-local/config deploy-local/secrets
cat > deploy-local/config/telegram-bots.yml <<'YAML'
generation: 1
telegram:
  bots: {}
  clients: {}
secrets: {}
YAML
```

配置目录和 YAML 应允许 UID 10001 读取；凭据目录应仅允许该用户或受控组访问。容器内凭据路径填写 `/run/secrets/<文件名>`，SHA-256 按凭据文件原始字节计算。使用 [catalog.yml](https://github.com/nasa-runtime/telegram-bots/blob/release/examples/catalog.yml) 作为完整业务模板；不要将 Nacos、日志或 listener 设置放入这些业务 YAML。

### 机器人、目的地与凭据

准备 `deploy-local/secrets/ops_bot` 保存真实 bot token，`deploy-local/secrets/monitoring` 保存独立的高熵调用方凭据（32–512 个非空白 ASCII 字节）。文件只允许 UID 10001 或受控组读取，不能写入 Git。用 `shasum -a 256` 或 `sha256sum` 计算两个文件原始字节的摘要，包含文件中实际存在的换行。

将下面内容保存为同目录临时文件，替换真实 chat_id 与两个摘要后，原子改名为 `deploy-local/config/telegram-bots.yml`。从空目录配置开始使用 `generation: 2`，之后每次修改递增，所有导入文件保持相同 generation。

```yaml
generation: 2
telegram:
  bots:
    ops:
      token: secret://ops_bot
      default_destination: alerts
      destinations:
        alerts:
          chat_id: -1000000000000
  clients:
    monitoring:
      credential: secret://monitoring
      allowed_bots: [ops]
secrets:
  ops_bot:
    file: /run/secrets/ops_bot
    sha256: REPLACE_WITH_BOT_TOKEN_FILE_SHA256
  monitoring:
    file: /run/secrets/monitoring
    sha256: REPLACE_WITH_CALLER_CREDENTIAL_FILE_SHA256
```

机器人需已加入目标聊天并获准发消息。凭据文件和 YAML 摘要匹配后自动应用；过渡期不匹配时保留旧目录。调用方使用 `monitoring` 文件中的凭据原文访问下方业务 API，不能使用 bot token 代替。

### 不使用 Nacos

默认关闭 Nacos。下面显式清空 profile 并关闭发现，只通过 HTTP 地址调用；无需准备任何 Nacos 参数。创建自定义网络后，业务容器加入同一网络即可通过 `http://telegram-bots:2060` 调用。网络已存在时省略创建命令。

```sh
docker network create telegram-bots-net
docker pull nasaruntime/telegram-bots:1.0.0

docker run --detach --name telegram-bots \
  --network telegram-bots-net \
  --restart unless-stopped --stop-timeout 70 \
  --read-only --cap-drop ALL --security-opt no-new-privileges \
  --log-driver json-file --log-opt max-size=10m --log-opt max-file=3 \
  --env APP_PROFILE= --env APP__REST_DISCOVERY__ENABLED=false \
  --env 'TELEGRAM_YML=/etc/telegram-bots/*.yml' \
  --env APP__SERVER__PORT=2060 --env TELEGRAM_LOG_LEVEL=info \
  --env TELEGRAM_LOG_PATH= \
  --publish 127.0.0.1:2060:2060 \
  --mount "type=bind,src=$PWD/deploy-local/config,dst=/etc/telegram-bots,readonly" \
  --mount "type=bind,src=$PWD/deploy-local/secrets,dst=/run/secrets,readonly" \
  nasaruntime/telegram-bots:1.0.0

curl --fail http://127.0.0.1:2060/readyz
docker logs --tail 100 telegram-bots
```

宿主机程序使用 `http://127.0.0.1:2060`；其它服务器不能访问这个回环端口映射。需要远程调用时，改为绑定宿主机可达的内网 IP，例如 `--publish 192.168.10.20:2060:2060`，并限制访问来源；跨信任边界使用 TLS 代理。`0.0.0.0` 只能用于监听，不能作为业务目标地址。

### 使用 Nacos 注册发现

仍使用同一镜像、同一配置目录和凭据目录。`APP_PROFILE=nacos` 加载镜像自带的 `zcf/application-nacos.yml`；Nacos **仅负责注册发现**，机器人配置仍来自 `/etc/telegram-bots/*.yml`。不需要创建 Nacos 配置中心的 Data ID。

以下示例假定网关宿主机内网 IP 为 `192.168.10.20`，Nacos SDK 地址为 `192.168.10.10:8848`；请替换为真实值。Nacos 地址必须能从容器访问，注册 IP 必须能从业务调用方访问。容器内的 `127.0.0.1` 不是宿主机；Docker Desktop 访问宿主机上的 Nacos 可用 `host.docker.internal:8848`。Linux 可按需加 `--add-host host.docker.internal:host-gateway`。使用 Nacos 的 SDK 地址，不要填控制台 URL 或 `/nacos` 路径；通常还需允许客户端访问 SDK gRPC 端口 `9848`（默认 SDK 端口加 1000）。 端口定义见 [Nacos 部署说明](https://nacos.io/en/docs/next/manual/admin/deployment/deployment-overview/)。

在前面准备好的工作目录创建受限环境文件，并通过编辑器替换地址、凭据与命名空间：

```sh
umask 077
cat > deploy-local/nacos.env <<'ENV'
NACOS_SERVER_ADDR=192.168.10.10:8848
NACOS_NAMESPACE=
NACOS_GROUP=DEFAULT_GROUP
NACOS_USERNAME=REPLACE_WITH_USERNAME
NACOS_PASSWORD=REPLACE_WITH_PASSWORD
ENV
chmod 600 deploy-local/nacos.env
```

`NACOS_NAMESPACE` 填 namespace **ID**；空值表示默认公共空间。Nacos 未启用认证时把用户名、密码同时留空；启用认证时填写已授权账号。环境文件按 `KEY=value` 填写，不加 shell 引号，Docker 不会替换其中的 `${...}`。该文件仅存放在本地受限目录，不提交 Git；有权检查容器配置的管理员仍能读取容器环境。

两种启动方式二选一，不要同时启动同一机器人目录。如果已运行上一种方式，先执行 `docker stop --time 70 telegram-bots`、`docker rm telegram-bots`。确保 `telegram-bots-net` 已创建，再运行：

```sh
export TELEGRAM_HOST_IP=192.168.10.20
docker pull nasaruntime/telegram-bots:1.0.0

docker run --detach --name telegram-bots \
  --network telegram-bots-net \
  --restart unless-stopped --stop-timeout 70 \
  --read-only --cap-drop ALL --security-opt no-new-privileges \
  --log-driver json-file --log-opt max-size=10m --log-opt max-file=3 \
  --env-file "$PWD/deploy-local/nacos.env" \
  --env APP_PROFILE=nacos --env APP__REST_DISCOVERY__ENABLED=true \
  --env TELEGRAM_REGISTER_IP="$TELEGRAM_HOST_IP" \
  --env APP__REST_DISCOVERY__REGISTRATION__PORT=2060 \
  --env 'TELEGRAM_YML=/etc/telegram-bots/*.yml' \
  --env APP__SERVER__PORT=2060 --env TELEGRAM_LOG_LEVEL=info \
  --env TELEGRAM_LOG_PATH= \
  --publish "${TELEGRAM_HOST_IP}:2060:2060" \
  --mount "type=bind,src=$PWD/deploy-local/config,dst=/etc/telegram-bots,readonly" \
  --mount "type=bind,src=$PWD/deploy-local/secrets,dst=/run/secrets,readonly" \
  nasaruntime/telegram-bots:1.0.0

curl --fail "http://${TELEGRAM_HOST_IP}:2060/readyz"
docker logs --tail 100 telegram-bots
```

在对应 Nacos namespace/group 下检查 `telegram-bots` 服务的健康实例，预期地址为 `192.168.10.20:2060`。业务方发现该地址后直接发送 HTTP 请求，仍需携带调用方认证头；Nacos 不代理消息。连接或注册失败会阻止正常就绪，应核对网络、账号权限、namespace ID 和日志。

容器端口、宿主机映射端口与注册端口是三件事：例如改为 `--publish "${TELEGRAM_HOST_IP}:12060:2060"` 时，同时设置 `APP__REST_DISCOVERY__REGISTRATION__PORT=12060`；容器 listener 仍为 `APP__SERVER__PORT=2060`。否则业务方会发现错误端口。不要把随机 Docker bridge IP 注册给跨主机调用方。未设置注册端口或设置为 `0` 时使用实际 listener 端口，不会自动推导 Docker 映射。

### 参数与挂载说明

| 参数或设置 | 用途与默认行为 |
| --- | --- |
| `--network telegram-bots-net` | 使同网络容器可按名称互访；不创建网络，也不自动连接其它业务容器 |
| `--restart unless-stopped` | 容器退出或 Docker 重启后自动恢复；显式停止后保持停止 |
| `--stop-timeout 70` | 停机宽限 70 秒，覆盖默认 60 秒应用停机预算 |
| `--read-only`、`--cap-drop ALL`、`no-new-privileges` | 只读根目录、移除 Linux capabilities、禁止提权；默认用户为 10001，无需使用 root |
| `--log-driver json-file` 与两个 `--log-opt` | Docker 控制台日志每文件最多 10 MiB，保留最多 3 文件；与 nalog 文件日志独立 |
| `APP_PROFILE` | 空值只读主引导文件；`nacos` 加载 Nacos profile；修改后重建容器 |
| `APP__REST_DISCOVERY__ENABLED` | 默认 `false`；Nacos profile 为 `true`，控制注册发现组件 |
| `NACOS_SERVER_ADDR` | Nacos 模式必填，容器可达的 SDK `host:port` |
| `NACOS_NAMESPACE`、`NACOS_GROUP` | 默认空 namespace、`DEFAULT_GROUP`；业务方发现时使用相同范围 |
| `NACOS_USERNAME`、`NACOS_PASSWORD` | 默认空；按 Nacos 认证要求成对配置 |
| `TELEGRAM_REGISTER_IP` | Nacos 模式必填，业务方可达的真实 IP；不要填监听地址 |
| `APP__REST_DISCOVERY__REGISTRATION__PORT` | 默认 `0` 表示实际 listener 端口；存在端口映射时按业务可达端口显式设置 |
| `LOCAL_NETWORK_IP` | NASA 注册 IP 的最高优先级覆盖；通常不要传入，避免覆盖 `TELEGRAM_REGISTER_IP` |
| `APP__SERVER__PORT`、`--publish` | 默认容器端口 2060；映射格式为 `宿主机IP:宿主机端口:容器端口`，最后一项须与 listener 一致 |
| `TELEGRAM_YML` | 默认 `/etc/telegram-bots/*.yml`；可改为绝对文件路径或单目录通配模式，命令行需引用 `*` |
| `/etc/telegram-bots`，只读目录 | 加载所有 `.yml`，按文件名自然顺序合并，后者覆盖前者；至少需要一个文件 |
| `/run/secrets`，只读目录 | 保存 token 和调用方凭据，YAML 通过 `secrets.*.file` 与 SHA-256 引用 |
| `TELEGRAM_LOG_PATH=` | 仅输出控制台，适用于只读根文件系统 |
| `TELEGRAM_LOG_LEVEL` | 默认 `info`；启动时设置日志过滤级别 |
| `/usr/local/logs/telegram-bots`，可写目录 | 保留默认文件日志时挂载，并移除空的 `TELEGRAM_LOG_PATH` 设置 |

需要文件日志时，可移除 `--env TELEGRAM_LOG_PATH=` 并增加 `-v telegram-bots-logs:/usr/local/logs/telegram-bots`；新 named volume 继承镜像内日志目录的 UID/GID 10001。已有日志卷需确认同样可写。

使用宿主机日志目录时，然后在上述任一 `docker run` 命令中移除 `--env TELEGRAM_LOG_PATH=`，增加 `--mount "type=bind,src=$PWD/deploy-local/logs,dst=/usr/local/logs/telegram-bots"`：

```sh
mkdir -p deploy-local/logs
sudo chown 10001:10001 deploy-local/logs
sudo chmod 750 deploy-local/logs
```

日志挂载保持可写；配置与凭据挂载保持只读。不要对现有共享目录执行上述属主调整。容器没有配置文件自动创建功能：目录不存在、零匹配、凭据不可读或默认文件日志路径不可写时，启动会失败。

启动后每次业务配置变更递增 `generation`，多文件的 `generation` 必须一致。使用不匹配 `.yml` 的临时后缀再原子重命名。挂载整个目录，不挂载单个文件，也不要覆盖镜像内 `/app/zcf/application.yml`。环境变量、profile、Nacos 和 listener 参数的变化需要重新创建容器；业务 YAML 与凭据可按配置合同热更新。

`/healthz`、`/readyz` 使用真实 HTTP 状态；业务 `/api/` 统一 HTTP 200，必须检查 JSON `code`。消息受理成功仅表示进入内存队列。停止时执行 `docker stop --time 70 telegram-bots`，为队列排空预留时间。

构建文件为 [docker/Dockerfile](https://github.com/nasa-runtime/telegram-bots/blob/release/docker/Dockerfile)，可执行 `docker build -f docker/Dockerfile -t telegram-bots:local .`。完整日志挂载、Kubernetes、Nacos 和发布设置见[容器部署](https://github.com/nasa-runtime/telegram-bots/blob/release/docs/container-deployment.md)。

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

使用模板中的 `monitoring` 调用方，输入其凭据原文后查询机器人并提交消息。服务可就绪后还需配置有效机器人、真实 chat_id、调用方权限和匹配的凭据摘要：

```sh
export TELEGRAM_BOTS_BASE_URL=http://127.0.0.1:2060
read -r -s TELEGRAM_CLIENT_KEY
export TELEGRAM_CLIENT_KEY
curl --fail "$TELEGRAM_BOTS_BASE_URL/api/bots" \
  -H 'X-Client-Id: monitoring' \
  -H "Authorization: Bearer $TELEGRAM_CLIENT_KEY"
curl --fail "$TELEGRAM_BOTS_BASE_URL/api/bots/ops/messages" \
  -H 'X-Client-Id: monitoring' \
  -H "Authorization: Bearer $TELEGRAM_CLIENT_KEY" \
  -H 'Content-Type: application/json' \
  --data '{"destination":"alerts","text":"订单服务通知"}'
unset TELEGRAM_CLIENT_KEY
```

检查 JSON `code`；HTTP 200 与 `accepted: true` 都不代表 Telegram 已送达。

Complete field definitions, permissions, error codes, retry semantics, Nacos and Kubernetes configuration: [中文 README](https://github.com/nasa-runtime/telegram-bots/blob/release/README.md), [English README](https://github.com/nasa-runtime/telegram-bots/blob/release/README.en.md), [deployment](https://github.com/nasa-runtime/telegram-bots/blob/release/docs/container-deployment.md).

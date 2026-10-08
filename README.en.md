# telegram-bots

[简体中文](README.md)

A multi-bot Telegram text notification gateway for backend services. Callers select bot and destination aliases; the gateway owns Telegram tokens, chat identities and caller permissions. An external YAML catalog and credential files support adding and removing bots, rotating credentials and changing authorization without rebuilding the image or restarting the process.

Built on crates.io `nasa 2.0.2`, the service uses `#[nasa::application("log", "web", "nacos-discovery")]` so napp owns startup, business initialization, readiness and shutdown. It provides bounded in-memory queues, one serial consumer per Telegram identity, a global outbound concurrency limit, atomic catalog publication and bounded draining. **JSON code=200 confirms in-memory acceptance only.** It does not confirm Telegram delivery or that a user has read a message.

## Capabilities and limits

- Credentials, destinations and caller permissions switch as one complete generation. Requests that finish uploading after their generation is retired cannot enqueue using old permissions.
- Invalid candidates retain the previous working catalog. Unknown fields, duplicate identities, missing credentials, digest mismatches and budget violations reject the whole candidate.
- Token rotation for the same Telegram numeric identity reuses its consumer and rate limit state. Different bots can send concurrently.
- Removing a bot closes admission before draining accepted messages. Cancellation distinguishes messages never sent from uncertain outbound results.
- Optional Nacos registration provides service discovery, not catalog configuration or distributed ownership.

Only text `sendMessage` is supported. There is no persistence, result lookup, automatic retry, idempotency store, inbound Telegram webhook, leader election or cross-instance deduplication. Run one active instance for each Telegram identity; stop the old instance before replacing its image.

## Application lifecycle

The entry point declares framework components and registers the `telegram-catalog` hosted initializer. napp owns the runtime, signals, Web routes, probes, metrics and optional Nacos registration. Initialization validates the external catalog and credentials, registers managed resources and stages the catalog manager as a critical task. Consumers start only after all initialization and component Ready actions succeed. Initialization failure prevents listening. napp handles SIGTERM/SIGINT during both initialization and runtime, returning a successful exit status after normal cleanup. An unexpected manager exit revokes admission and triggers application shutdown.

The macro supplies `app` for business lifecycle registration:

```rust
/// 业务作用：把通知目录生命周期交给 napp。
/// 参数说明：`app` 是尚未开放业务入口的应用容器。
/// 返回：登记成功后继续初始化，目录不可用时拒绝启动。
#[nasa::application("log", "web", "nacos-discovery", config = telegram_bots::catalog::source::bootstrap_loader)]
async fn main(app: nasa::Application) -> anyhow::Result<()> {
    rest::install(&app)?;
    telegram_bots::application::install(&app)?;
    Ok(())
}
```

The service has one executable. Each read cycle forks a child that returns file bytes over an anonymous channel; the parent merges and validates documents in memory. The child does not restart napp, Nacos or HTTP services. A managed cleanup action retains ownership of startup readers even when initialization is cancelled. napp synchronously reads `zcf/application.yml` and the active profile before business initialization: keep these immutable files on reliable local storage. Their first read is outside `catalog_watch.load_timeout_ms`; external catalog and credential file reads have timeout and process-reaping protection.

## Source layout

Only the application entry `main.rs` and library module entry `lib.rs` remain directly under `src/`. Business code is grouped by responsibility:

| Directory | Responsibility |
| --- | --- |
| `rest/` | REST authentication, bot discovery, message submission and configuration status endpoints |
| `application/` | Registration of initialization, readiness, critical tasks and managed cleanup through app |
| `service/` | Caller permissions, destination selection, message acceptance, Telegram requests and error classification |
| `catalog/` | Typed configuration, isolated file reads, credential validation, reloads and atomic publication |
| `partition/` | Serial queues per Telegram identity, capacity budgets and terminal message accounting |
| `observability/` | Configuration and delivery metric definitions, collection and output |

`partition/` implements this service's bot consumer domains. See the [architecture](docs/architecture.md) for publication ordering and shutdown boundaries.

## Logging

NASA's `nalog` component is enabled through the `log` feature and `"log"` in the application macro. napp owns initialization and flushes file output after business resources stop. Existing `tracing` events use the same output.

The default is `info` level with console output plus `info.log` and a separate `error.log` under `/usr/local/logs/telegram-bots`. Set `TELEGRAM_LOG_LEVEL=warn` to change the level, or `TELEGRAM_LOG_PATH=/absolute/log/directory` to change the directory. An explicitly empty `TELEGRAM_LOG_PATH` selects console output only. The log directory must be writable. This service rotates files daily or at 100 MiB, retains archives for seven days and limits each of the `info` and `error` archive sets to 1 GiB. Active files are excluded from those caps; cleanup runs at startup and rotation.

Logging belongs to the bootstrap `log` section and should be changed with an application restart. External bot catalogs cannot configure logging. The expression `${TELEGRAM_LOG_PATH:/usr/local/logs/${application.name}}` follows the application name when the environment variable is absent. An explicitly empty value disables file logging. The container examples use UID 10001; file output requires a dedicated mount writable by that user. Explicitly disabling file output allows a read-only root without a log volume. See the [configuration reference](docs/configuration.md).

## Start locally

Linux and macOS are supported, with Rust 1.94 or newer. Product dependencies resolve from crates.io; commit and retain `Cargo.lock` when building the service.

```sh
cargo build --locked --release --bin telegram-bots
mkdir -p deploy-local
cp examples/catalog-empty.yml deploy-local/telegram-bots.yml
export TELEGRAM_YML="$PWD/deploy-local/telegram-bots.yml"
export TELEGRAM_LOG_PATH="$PWD/logs"
./target/release/telegram-bots
```

The build produces the `telegram-bots` executable. After preparing the configuration and environment variables above, use `cargo run --locked` for development. Configure the same absolute `TELEGRAM_YML` path and a writable `TELEGRAM_LOG_PATH` directory in your IDE, and use the project root as its working directory. Without `TELEGRAM_YML`, the service matches `/etc/telegram-bots/*.yml`; no matches prevent startup. An exact absolute file path is also accepted.

The default listener is `0.0.0.0:2060`. Check `/readyz` and `/metrics` from an internal network. An empty catalog can start, but has no authenticated business callers.

The Docker Hub image is `nasaruntime/telegram-bots`, with an immutable `/app/zcf/application.yml` bootstrap. Mount your configuration directory at `/etc/telegram-bots`. The service matches `/etc/telegram-bots/*.yml` by default. `TELEGRAM_YML` can select another absolute path at startup. Mount configuration and credentials as directories, not individual files. See [container deployment](docs/container-deployment.md) for image requirements and Docker and Kubernetes examples, including non-root operation and read-only mounts.

## Run with Docker

Image: [`nasaruntime/telegram-bots:1.0.0`](https://hub.docker.com/r/nasaruntime/telegram-bots). It uses Alpine, supports `linux/amd64` and `linux/arm64`, and runs as UID/GID `10001:10001`. The import expression is `${TELEGRAM_YML:/etc/telegram-bots/*.yml}`: mount the whole configuration directory at `/etc/telegram-bots`.

`linux/amd64` means x64/x86_64 for both Intel and AMD processors. The same tag also supports `linux/arm64`; Docker chooses the host architecture automatically.

### Prepare the configuration directory

Both startup modes use the following files. Run commands from the same working directory, and retain any existing business configuration. This empty catalog can become ready; sending messages requires bots, destinations, callers and credential files as described under “Configure a bot”.

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

The configuration directory and YAML must be readable by UID 10001. Restrict credential files to that user or a controlled group. Reference credentials as `/run/secrets/<filename>` and calculate SHA-256 over the original file bytes. Use [catalog.yml](examples/catalog.yml) for a complete business template. Nacos, logging and listener settings belong to startup configuration, not these business YAML files.

### Without Nacos

Nacos is disabled by default. This command explicitly clears the profile and disables discovery; no Nacos parameters are required. A custom network allows other containers on that network to call `http://telegram-bots:2060`. Skip network creation if it already exists.

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

Host processes use `http://127.0.0.1:2060`. Other servers cannot reach this loopback mapping. For remote clients, publish on a reachable private host IP, such as `--publish 192.168.10.20:2060:2060`, restrict incoming traffic, and use a TLS proxy across trust boundaries. `0.0.0.0` is a bind address, not a client destination.

### With Nacos discovery

Use the same image and the same configuration and credential mounts. `APP_PROFILE=nacos` loads the bundled `zcf/application-nacos.yml`. Nacos **only provides registration and discovery**; bot configuration still comes from `/etc/telegram-bots/*.yml`. No Nacos configuration Data ID is required.

The example uses gateway host IP `192.168.10.20` and Nacos SDK address `192.168.10.10:8848`; replace both with your deployment values. The container must reach Nacos, and business callers must reach the registered IP. Inside a container, `127.0.0.1` does not refer to the host. Docker Desktop can reach host Nacos at `host.docker.internal:8848`; Linux can use `--add-host host.docker.internal:host-gateway` when needed. Use the SDK `host:port`, not a console URL or `/nacos` path. Clients normally also need access to SDK gRPC port `9848`, the default SDK port plus 1000. See the [Nacos deployment reference](https://nacos.io/en/docs/next/manual/admin/deployment/deployment-overview/).

Create a restricted environment file in the working directory prepared above. Edit it to supply the actual endpoint, credentials and namespace:

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

`NACOS_NAMESPACE` is the namespace **ID**; empty selects the default public namespace. Leave both username and password empty only when Nacos authentication is disabled. Use literal `KEY=value` entries without shell quotes; Docker does not expand `${...}` in this file. Keep it local with restricted permissions and out of Git. Administrators allowed to inspect containers can still read their environment.

Choose one startup mode for a bot catalog. If the previous container is running, first run `docker stop --time 70 telegram-bots` and `docker rm telegram-bots`. Ensure `telegram-bots-net` exists, then start:

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

Check the healthy `telegram-bots` instance in the selected Nacos namespace/group. Its expected address is `192.168.10.20:2060`. Business callers discover it and send HTTP requests directly with the required caller headers; Nacos does not proxy messages. Connection or registration failure prevents normal readiness. Check connectivity, account permissions, namespace ID and logs.

Container port, published host port and registered port are separate. For `--publish "${TELEGRAM_HOST_IP}:12060:2060"`, also set `APP__REST_DISCOVERY__REGISTRATION__PORT=12060`; keep `APP__SERVER__PORT=2060`. Otherwise discovery returns the wrong port. Do not register a random Docker bridge IP for callers on other hosts. An unset registration port or value `0` uses the actual listener port; it does not infer Docker port mappings.

### Parameters and mounts

| Parameter or setting | Purpose and default behavior |
| --- | --- |
| `--network telegram-bots-net` | Enables name lookup between containers on that network; does not create it or attach callers automatically |
| `--restart unless-stopped` | Restarts after container exit or Docker restart; explicitly stopped containers stay stopped |
| `--stop-timeout 70` | Allows 70 seconds for the default 60-second application shutdown budget |
| `--read-only`, `--cap-drop ALL`, `no-new-privileges` | Read-only root, no Linux capabilities, no privilege escalation; default user is 10001 |
| `--log-driver json-file` and `--log-opt` | Limits Docker console logs to three files of 10 MiB each, independently of nalog file output |
| `APP_PROFILE` | Empty loads only the main bootstrap; `nacos` loads the Nacos profile; recreate the container after changes |
| `APP__REST_DISCOVERY__ENABLED` | Defaults to `false`; the Nacos profile enables registration/discovery |
| `NACOS_SERVER_ADDR` | Required for Nacos: SDK `host:port` reachable from the container |
| `NACOS_NAMESPACE`, `NACOS_GROUP` | Empty namespace and `DEFAULT_GROUP` by default; callers must discover in the same scope |
| `NACOS_USERNAME`, `NACOS_PASSWORD` | Empty by default; configure both according to Nacos authentication requirements |
| `TELEGRAM_REGISTER_IP` | Required for Nacos: actual IP reachable by callers, not a wildcard bind address |
| `APP__REST_DISCOVERY__REGISTRATION__PORT` | Default `0` uses the actual listener port; explicitly set the reachable port when publishing through NAT |
| `LOCAL_NETWORK_IP` | Highest-priority NASA registration IP override; normally omit it so it does not override `TELEGRAM_REGISTER_IP` |
| `APP__SERVER__PORT`, `--publish` | Default container port 2060; mapping is `host-IP:host-port:container-port`, with the last value matching the listener |
| `TELEGRAM_YML` | Defaults to `/etc/telegram-bots/*.yml`; supports an absolute file or single-directory pattern; quote `*` in shell arguments |
| `/etc/telegram-bots`, read-only directory | Load all `.yml` files in natural filename order; later files override earlier ones; at least one file is required |
| `/run/secrets`, read-only directory | Bot tokens and caller credentials referenced by `secrets.*.file` and SHA-256 |
| `TELEGRAM_LOG_PATH=` | Console logging only, suitable for a read-only root filesystem |
| `TELEGRAM_LOG_LEVEL` | Startup log filter level, `info` by default |
| `/usr/local/logs/telegram-bots`, writable directory | Mount for default file logging and remove the empty `TELEGRAM_LOG_PATH` setting |

For file logging, prepare a dedicated host directory, remove `--env TELEGRAM_LOG_PATH=` from either command and add `--mount "type=bind,src=$PWD/deploy-local/logs,dst=/usr/local/logs/telegram-bots"`:

```sh
mkdir -p deploy-local/logs
sudo chown 10001:10001 deploy-local/logs
sudo chmod 750 deploy-local/logs
```

Keep this log mount writable and the configuration/credential mounts read-only. Do not change ownership of an existing shared directory. The container does not create missing configuration: missing directories, zero YAML matches, unreadable credentials or an unwritable default log path prevent startup.

Increase `generation` for every business configuration change, using the same value in every imported file. Write temporary files with a suffix that does not match `.yml`, then atomically rename them. Mount whole directories and retain `/app/zcf/application.yml`. Environment variables, profiles, Nacos and listener settings require container recreation; business YAML and credentials support hot reload under the configuration contract.

`/healthz` and `/readyz` use actual HTTP statuses. Business `/api/` responses use HTTP 200 and require checking JSON `code`; successful message acceptance only means entry into an in-memory queue. Stop with `docker stop --time 70 telegram-bots` to allow draining.

Build from source with `docker build -f docker/Dockerfile -t telegram-bots:local .`; the [Dockerfile](docker/Dockerfile) is included in the repository. See [container deployment](docs/container-deployment.md) for log mounts, Kubernetes, Nacos and publication settings.

## Configure a bot

Use [examples/catalog.yml](examples/catalog.yml) as a template. Replace the illustrative chat ID, absolute credential paths and `REPLACE_WITH_64_HEX_SHA256` placeholders. Store Telegram tokens and high-entropy caller credentials in restricted files. Caller credentials must contain 32–512 non-whitespace ASCII bytes.

Calculate SHA-256 over the original file bytes, including any trailing newline, using `shasum -a 256` or `sha256sum`. The YAML digest pins the exact material: if Kubernetes updates the ConfigMap and Secret at different times, the candidate remains rejected until every credential matches.

Set `generation` to an integer in `1..=9007199254740991`, greater than the applied generation; use `2` when replacing the starter empty catalog. Write the complete next file and atomically rename it into place. All imported files must declare the same generation. Restoring old business content also requires a new, higher generation. The generation floor is not persisted across process restarts.

The default poll interval is one second. Publication requires two consecutive matching complete reads. With normal scheduling and fast local file reads, a stable change usually takes about one to two seconds to apply, plus reading and validation time. Check the configuration status if a stable visible change remains unapplied after ten seconds; this is an operational threshold, not a guaranteed maximum delay. External catalog and credential reads execute in a separate helper process during both initialization and runtime. The default read budget is three seconds, followed by at most one second to terminate and reap the helper. Runtime timeouts retain the working catalog and subsequent polls retry; startup timeouts exit without opening the listener. Failure to reap the helper stops the service. Platform projection delays or mismatched credentials can extend the wait.

Imports are explicit lists of absolute `file` paths or single-directory filename patterns, each with an explicit `optional` flag. Patterns such as `/etc/telegram-bots/*.yml` load files in natural filename order; later files override earlier files. Recursive imports, directory wildcards and inline secret material are unsupported. Unknown fields fail closed. See the [configuration reference](docs/configuration.md) for all fields, precedence and limits.

## Calling from a business service

Use ordinary HTTP with JSON; callers do not need NASA, Rust or a Telegram bot token. Obtain four values from the gateway operator: **base URL, client ID, client credential, and allowed bot/destination aliases**. The empty starter catalog has no business identities; configure an authorized caller before using `/api/`.

### Choose the address

The default listener is `0.0.0.0:2060`. This is a bind address, not a client destination. Configure a base URL without `/api` and append the API paths below.

| Caller location | Example base URL | Requirement |
| --- | --- | --- |
| Process on the same host | `http://127.0.0.1:2060` | Native gateway or a published host port |
| Another container on the same custom Docker network | `http://telegram-bots:2060` | Gateway container name/network alias is `telegram-bots` |
| Pod in the same Kubernetes namespace | `http://telegram-bots:2060` | Create the Service shown in the deployment guide |
| Pod in another Kubernetes namespace | `http://telegram-bots.<namespace>.svc:2060` | Replace `<namespace>` and permit access in network policies |
| Service on another server | Operator-provided HTTPS URL | Reachable private endpoint or trusted TLS proxy |

Inside a container, `127.0.0.1` points to that container. The Docker deployment example publishes only to host loopback; other hosts cannot access that mapping. Use shared-network addressing or a controlled proxy. See [container deployment](docs/container-deployment.md).

With Nacos enabled, the default registered service name is `telegram-bots`. Discover a healthy instance in the configured namespace/group and build the base URL from its registered IP and port. Nacos does not proxy requests or replace authentication; use the actual deployment registration settings.

### Identity and authorization

Give each business service its own `telegram.clients.<client_id>` entry and credential. In [catalog.yml](examples/catalog.yml), client `monitoring` can use bot `ops`; destination `alerts` selects a chat configured by the operator.

| Header | Required | Value |
| --- | --- | --- |
| `X-Client-Id` | Every `/api/` request | Configured client ID, such as `monitoring` |
| `Authorization` | Every `/api/` request | `Bearer <raw client credential>`, not a Telegram token or SHA-256 digest |
| `Content-Type` | Message submission | `application/json` |

Each identity header must occur exactly once. A client can use only bots in its `allowed_bots` list; bot permission covers all of that bot's destinations. There is no separate per-destination client permission.

Store the address and ID in client configuration; inject credentials through a Secret or controlled environment. The commands below assume `TELEGRAM_CLIENT_KEY` is already injected. Do not put real credentials in source, images or command history. Use TLS when crossing a trust boundary.

```sh
export TELEGRAM_BOTS_BASE_URL='http://127.0.0.1:2060'
export TELEGRAM_CLIENT_ID='monitoring'
```

These variables are client example settings, not gateway configuration keys.

### List available bots

```sh
curl -i --connect-timeout 2 --max-time 10 \
  "$TELEGRAM_BOTS_BASE_URL/api/bots" \
  -H "X-Client-Id: $TELEGRAM_CLIENT_ID" \
  -H "Authorization: Bearer $TELEGRAM_CLIENT_KEY"
```

HTTP 200 response:

```json
{"code":200,"data":{"bots":[{"bot_id":"ops","description":"Operations notifications","destinations":["alerts"],"default_destination":"alerts","messages_path":"/api/bots/ops/messages"}]}}
```

Use `bot_id` in the submission path and choose a `destination` alias. `messages_path` is relative to the base URL. A null `default_destination` means callers must select a destination. An empty `bots` list means authentication succeeded but no bot is available to this caller. Tokens, chat IDs and other callers' permissions are not exposed.

### Submit text

**`POST /api/bots/{bot_id}/messages`** takes a JSON object with no query parameters. The path selects a configured alias, not a Telegram username or numeric identity.

| JSON field | Type | Required | Meaning/default |
| --- | --- | --- | --- |
| `text` | string | Yes | Non-whitespace text, at most 4096 UTF-16 code units; some emoji count as two units; no automatic splitting |
| `destination` | string | No | Bot destination alias; omitted uses the configured default, which must exist |
| `parse_mode` | string | No | `HTML` or `MarkdownV2`; omitted means plain text; the caller must escape the selected format |
| `disable_notification` | boolean | No | Request silent notification; omitted uses the bot setting, default `false` |
| `protect_content` | boolean | No | Request Telegram content protection; omitted uses the bot setting, default `false` |

Do not send `token`, `chat_id`, `message_thread_id` or extra fields. Chat and forum topic selection belongs to the server's destination configuration. The default JSON body limit is 65536 bytes and the upload deadline is five seconds; operators can change these startup settings. Text length and body byte size are separate limits.

```sh
curl -i --connect-timeout 2 --max-time 10 \
  "$TELEGRAM_BOTS_BASE_URL/api/bots/ops/messages" \
  -H "X-Client-Id: $TELEGRAM_CLIENT_ID" \
  -H "Authorization: Bearer $TELEGRAM_CLIENT_KEY" \
  -H 'Content-Type: application/json' \
  --data '{"destination":"alerts","text":"Order service: processing delay exceeds the threshold"}'
```

Example body using every optional field:

```json
{"text":"<b>Order alert</b>: processing delay exceeds the threshold","destination":"alerts","parse_mode":"HTML","disable_notification":false,"protect_content":true}
```

Successful admission returns **HTTP 200 OK**:

```json
{"code":200,"data":{"bot_id":"ops","destination":"alerts","accepted":true}}
```

This names the selected bot and destination and confirms in-memory admission. There is no message ID, task ID or result lookup URL. Do not resend merely because a message has not yet appeared in Telegram: queued messages wait for earlier work, send intervals and platform cooldowns.

### Handle results and retries

All business endpoints return **HTTP 200** with `nasa::base::BaseResponse`: `{"code":200,"data":...}`. The numeric JSON `code` carries the processing result: 200 means success; 400, 401, 403, 429, 503 and other documented codes mean rejection or failure. Successful responses omit `msg`. Unset `msg` and `data` fields are omitted; `code` is always present. Check JSON `code`; HTTP 200 or `curl --fail` alone cannot establish business success. `/healthz` and `/readyz` retain real probe statuses, and `/metrics` retains its monitoring format.

Reuse an HTTP connection pool and configure connection/response deadlines. The two-second connect and ten-second total deadlines above are client examples; adjust them to the deployment network. The request waits for admission only. Disable unconditional automatic retries of POST network failures.

| JSON code | Meaning | Caller action |
| --- | --- | --- |
| 200 | Accepted into the bounded memory queue | Record gateway acceptance, not Telegram delivery |
| 400 | Invalid text or destination | Correct the message |
| 401 | Incorrect/rotated credential, unknown ID or duplicate identity header | Check current identity settings |
| 403 | Bot is not authorized | Refresh the catalog or request access |
| 404 | Route/catalog entry was not found | Check base URL, path and alias |
| 405 | Method not supported | Use GET or POST as documented |
| 500 | Request processing failed; outcome may be unknown | Do not retry automatically; inspect service and business state |
| 408 | Upload timed out before admission | Resolve the slow upload before resubmitting |
| 413 | Body too large | Reduce the request; do not encode attachments in text |
| 415 / 422 | Unsupported media type or invalid JSON fields/types | Follow the parameter table |
| 429 | Bot or process capacity exhausted | Confirm non-admission, then use bounded backoff |
| 503 | Ingress overload, not ready, stopping or catalog changed during upload | Confirm non-admission, refresh catalog/credentials and use bounded backoff |
| Disconnect, client timeout or proxy 502/504 | Admission cannot be determined | Do not retry blindly; apply the business policy for possible duplicates |

A business rejection can use this envelope:

```json
{"code":429,"msg":"通知队列或进程预算已满","data":{"reason":"queue_full","delivery":"not_sent","retry_safe":true}}
```

Rejections also return HTTP 200. `data.reason` identifies the stable cause; `data.delivery=not_sent` means this request was not queued. `data.retry_safe=true` allows a later attempt but does not provide idempotency. Validate the envelope and check numeric `code`. Proxy failures, disconnects and malformed HTTP protocol requests may not contain this service's JSON and do not prove non-admission.

Each accepted message gets at most one Telegram request attempt, with no automatic resend. Failures, uncertain outcomes and shutdown drops after code=200 appear only in logs and aggregate metrics. There is no callback or per-message result API. Businesses requiring durable notifications should retain persistent tasks upstream and define duplicate/unknown-result handling; this gateway is not a durable message system.

### Configuration status and probes

| Endpoint | Business credentials | Purpose |
| --- | --- | --- |
| `GET /api/config/status` | Required | Service-wide catalog generations, last rejection and draining state; not message results |
| `GET /readyz` | Not required | Internal readiness probe |
| `GET /healthz` | Not required | Internal liveness probe |
| `GET /metrics` | Not required | Internal catalog and aggregate delivery metrics |

Every authenticated caller can read service-wide configuration status; it is not filtered to an individual caller:

```sh
curl -i --connect-timeout 2 --max-time 10 \
  "$TELEGRAM_BOTS_BASE_URL/api/config/status" \
  -H "X-Client-Id: $TELEGRAM_CLIENT_ID" \
  -H "Authorization: Bearer $TELEGRAM_CLIENT_KEY"
```

Connect to the internal address, authenticate and list bots, select aliases, submit, then distinguish admission from rejection. See the [HTTP API](docs/http-api.md) for the control-plane response and further details. Restrict probes/metrics to internal networks; an invalid candidate can leave the old catalog ready.

## Operation

Monitor desired/applied generation, recent rejection and the last reconciliation timestamp alongside readiness. Invalid candidates leave the current catalog serving, so readiness alone does not prove that a deployment was applied.

Application identity, listener, configuration sources, watch settings, Nacos settings, HTTP budgets and drain timeout are fixed at startup. Bots, destinations, credentials, permissions, queue capacities and send intervals can change dynamically. See [architecture](docs/architecture.md) and [operations](docs/operations.md).

Accepted messages retain their original credentials and destination. Removing a bot does not cancel a request already sent to Telegram. For emergency revocation, revoke the Telegram token and stop the affected service. A network timeout can leave delivery uncertain; retrying blindly may duplicate a notification.

Use a termination grace period of at least 70 seconds with the shipped 60-second application budget. Queues drain concurrently; forced exit or node failure loses pending messages. There is no replay after restart.

The listener is plain HTTP. Use a trusted TLS proxy and restrict business access. `/healthz`, `/readyz` and `/metrics` do not require business credentials and must remain on internal networks. See [security](SECURITY.md).

## Contributing and licensing

See [contribution guidance](CONTRIBUTING.md). Licensed under either [MIT](LICENSE-MIT) or [Apache-2.0](LICENSE-APACHE), at your option. The detailed reference documents currently use Chinese; configuration keys and public APIs are identical in both README versions.

## Ordered files and nested defaults

`yml.imports` accepts filename `*` and `?` within one fixed directory. Each declaration is expanded separately in natural filename order: `telegram-1.yml`, `telegram-2.yml`, `telegram-02.yml`, `telegram-10.yml`. Later files override earlier files, and environment overrides apply last. Quote patterns in shell assignments, for example `TELEGRAM_YML='/etc/telegram-bots/*.yml'`. Recursive patterns, directory wildcards and nested imports are rejected.

Every matched source must be a regular YAML file and declare the same generation. Changes to the resolved configuration require a higher generation across the complete set. Renaming a source without changing the resolved values is still revalidated on every polling cycle and does not create new consumers. Directory enumeration, reads and source revalidation share the existing terminable reader process and deadline. Invalid content, mixed generations, duplicate file identities or credential digest mismatches keep the previous catalog active. Bootstrap source declarations and profile changes require a restart.

The application's `config = telegram_bots::catalog::source::bootstrap_loader` factory fixes configuration rules before napp preflight; the business lifecycle continues to own isolated catalog loading. `naml` resolves nested expressions and `nalog` consumes the final log path. See the [configuration contract](docs/configuration.md) for limits and failure behavior.

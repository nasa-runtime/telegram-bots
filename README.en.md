# telegram-bots

[简体中文](README.md)

A multi-bot Telegram text notification gateway for backend services. Callers select bot and destination aliases; the gateway owns Telegram tokens, chat identities and caller permissions. An external YAML catalog and credential files support adding and removing bots, rotating credentials and changing authorization without rebuilding the image or restarting the process.

Built on crates.io `nasa 2.0.1`, the service provides bounded in-memory queues, one serial consumer per Telegram identity, a global outbound concurrency limit, atomic catalog publication and bounded draining. **HTTP 202 confirms in-memory acceptance only.** It does not confirm Telegram delivery or that a user has read a message.

## Capabilities and limits

- Credentials, destinations and caller permissions switch as one complete generation. Requests that finish uploading after their generation is retired cannot enqueue using old permissions.
- Invalid candidates retain the previous working catalog. Unknown fields, duplicate identities, missing credentials, digest mismatches and budget violations reject the whole candidate.
- Token rotation for the same Telegram numeric identity reuses its consumer and rate limit state. Different bots can send concurrently.
- Removing a bot closes admission before draining accepted messages. Cancellation distinguishes messages never sent from uncertain outbound results.
- Optional Nacos registration provides service discovery, not catalog configuration or distributed ownership.

Only text `sendMessage` is supported. There is no persistence, result lookup, automatic retry, idempotency store, inbound Telegram webhook, leader election or cross-instance deduplication. Run one active instance for each Telegram identity; stop the old instance before replacing its image.

## Start locally

Rust 1.94 or newer is required. Product dependencies resolve from crates.io; commit and retain `Cargo.lock` when building the service.

```sh
cargo build --locked --release
mkdir -p deploy-local
cp examples/catalog-empty.yml deploy-local/telegram-bots.yml
cp examples/application-local.yml zcf/application-local.yml
export APP_PROFILE=local
export TELEGRAM_CATALOG_FILE="$PWD/deploy-local/telegram-bots.yml"
./target/release/telegram-bots
```

The default listener is `0.0.0.0:2060`. Check `/readyz` and `/metrics` from an internal network. An empty catalog can start, but has no authenticated business callers.

The image retains its immutable `/app/zcf/application.yml` bootstrap and reads `/etc/conf/telegram-bots.yml`. Mount configuration and credentials as directories, not individual files. See [container deployment](docs/container-deployment.md) for Docker and Kubernetes, including non-root operation and read-only mounts.

## Configure a bot

Use [examples/catalog.yml](examples/catalog.yml) as a template. Replace the illustrative chat ID, absolute credential paths and `REPLACE_WITH_64_HEX_SHA256` placeholders. Store Telegram tokens and high-entropy caller credentials in restricted files. Caller credentials must contain 32–512 non-whitespace ASCII bytes.

Calculate SHA-256 over the original file bytes, including any trailing newline, using `shasum -a 256` or `sha256sum`. The YAML digest pins the exact material: if Kubernetes updates the ConfigMap and Secret at different times, the candidate remains rejected until every credential matches.

Set `generation` to an integer in `1..=9007199254740991`, greater than the applied generation; use `2` when replacing the starter empty catalog. Write the complete next file and atomically rename it into place. All imported files must declare the same generation. Restoring old business content also requires a new, higher generation. The generation floor is not persisted across process restarts.

The default poll interval is one second. Publication requires two consecutive matching complete reads. With normal scheduling and fast local file reads, a stable change usually takes about one to two seconds to apply, plus reading and validation time. Check the configuration status if a stable visible change remains unapplied after ten seconds; this is an operational threshold, not a guaranteed maximum delay. Startup and runtime reads execute in a separate helper process. The default read budget is three seconds, followed by at most one second to terminate and reap the helper. Runtime timeouts retain the working catalog and subsequent polls retry; startup timeouts exit without opening the listener. Failure to reap the helper stops the service. Platform projection delays or mismatched credentials can extend the wait.

Imports are explicit lists of absolute `file` paths with an explicit `optional` flag. Glob expressions such as `/etc/conf/*.yml`, recursive imports and inline secret material are unsupported. Unknown fields fail closed. See the [configuration reference](docs/configuration.md) for all fields, precedence and limits.

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
{"bots":[{"bot_id":"ops","description":"Operations notifications","destinations":["alerts"],"default_destination":"alerts","messages_path":"/api/bots/ops/messages"}]}
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

Successful admission returns **HTTP 202 Accepted**:

```json
{"bot_id":"ops","destination":"alerts","accepted":true}
```

This names the selected bot and destination and confirms in-memory admission. There is no message ID, task ID or result lookup URL. Do not resend merely because a message has not yet appeared in Telegram: queued messages wait for earlier work, send intervals and platform cooldowns.

### Handle results and retries

Reuse an HTTP connection pool and configure connection/response deadlines. The two-second connect and ten-second total deadlines above are client examples; adjust them to the deployment network. The request waits for admission only. Disable unconditional automatic retries of POST network failures.

| HTTP result | Meaning | Caller action |
| --- | --- | --- |
| 202 | Accepted into the bounded memory queue | Record gateway acceptance, not Telegram delivery |
| 400 | Invalid text or destination | Correct the message |
| 401 | Incorrect/rotated credential, unknown ID or duplicate identity header | Check current identity settings |
| 403 | Bot is not authorized | Refresh the catalog or request access |
| 404 | Route/catalog entry was not found | Check base URL, path and alias |
| 408 | Upload timed out before admission | Resolve the slow upload before resubmitting |
| 413 | Body too large | Reduce the request; do not encode attachments in text |
| 415 / 422 | Unsupported media type or invalid JSON fields/types | Follow the parameter table |
| 429 | Bot or process capacity exhausted | Confirm non-admission, then use bounded backoff |
| 503 | Not ready, stopping or catalog changed during upload | Confirm non-admission, refresh catalog/credentials and use bounded backoff |
| Disconnect, client timeout or proxy 502/504 | Admission cannot be determined | Do not retry blindly; apply the business policy for possible duplicates |

A business rejection can use this envelope:

```json
{"error":{"code":"queue_full","message":"通知队列或进程预算已满","delivery":"not_sent","retry_safe":true}}
```

`delivery=not_sent` means this request was not queued. `retry_safe=true` allows a later attempt for this rejection; it does not provide idempotency. Routing, media, proxy and framework failures can have different response shapes. Check HTTP status and parse only recognized envelopes; do not treat every non-2xx response as safe to repeat.

Each accepted message gets at most one Telegram request attempt, with no automatic resend. Failures, uncertain outcomes and shutdown drops after 202 appear only in logs and aggregate metrics. There is no callback or per-message result API. Businesses requiring durable notifications should retain persistent tasks upstream and define duplicate/unknown-result handling; this gateway is not a durable message system.

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

# 配置合同

## 来源与优先级

`zcf/application.yml` 是必需引导文件。显式设置 `APP_PROFILE` 后，naml 再加载对应 `zcf/application-<profile>.yml` 等支持的 profile 文件。引导文件及 profile 只提供应用身份、日志、listener、生命周期、Nacos 和来源声明，不得包含 `telegram`、`secrets` 或 `generation`。

```yaml
yml:
  imports:
    - file: ${TELEGRAM_YML:/etc/conf/telegram-bots.yml}
      optional: false
config_watch:
  enabled: true
catalog_watch:
  poll_interval_ms: 1000
  load_timeout_ms: 3000
```

本服务约定使用 `/etc/conf/telegram-bots.yml` 保存外部配置；`/etc/conf/` 是本服务的部署目录约定。设置 `TELEGRAM_YML` 可在启动时选择其它绝对路径，也可以通过启动 profile 覆盖来源声明。

`telegram-bots` 在 napp 业务初始化阶段、Web 监听开放前读取必需导入链。运行中由应用监督器拥有的目录任务重新读取全部导入文件和材料。框架配置视图负责引导组件，业务目录具有独立、完整的发布边界。

合并顺序为：引导主文件 → 活动 profile → 按声明顺序的外部文件 → `APP__...` 环境覆盖 → `${...}` 占位符解析。外部文件只允许顶层 `generation`、`telegram`、`secrets`，不能更改引导项；未知字段拒绝。例如 `APP__TELEGRAM__HTTP__MAX_INFLIGHT=8` 在进程启动时覆盖文件预算。环境变量属于进程启动环境，运行中的新增 bot 和凭据轮换应使用文件；环境覆盖会持续压过后续文件修改。

导入配置是列表，不支持 `yml.imports=/etc/conf/*.yml`、glob、目录递归或嵌套导入。每项必须显式写 `file` 和布尔 `optional`；支持 1..=16 个互不重复的绝对路径，至少一个为必需项。当前依赖中的 `naml 2.0.0` 只解析导入描述；本服务执行显式文件导入，不把通配符当成匹配表达式。

每个外部文件最多 1 MiB，必须为非空 UTF-8 YAML 对象。仅“可选文件不存在”允许跳过；存在但无效、不可读、为空或超限均拒绝。文件不能是管道或设备。路径不允许 `..`、`*`、`?` 或方括号。符号链接允许用于受信卷的原子切换，因此部署者必须控制来源目录及其目标。

多个文件按叶子深合并，后者覆盖前者；空对象不会清除前一文件的键。删除 bot 应删除所有来源中该 bot 的定义。建议让机器人目录集中在一个文件中，避免跨文件残留定义。

## 部署代号与凭据绑定

每个实际存在的外部文件必须包含相同的整数 `generation`，范围为 `1..=9007199254740991`。该范围保证 JSON 状态与浮点指标都能精确表示代号；越界值在启动和运行期均被拒绝，也不会作为期望代号输出。它是目录部署身份，只有大于已应用代号的内容才能替换当前目录；同一代号改变内容会被拒绝。相同内容重复读取不会创建新消费者。恢复旧内容也需要使用更大的代号。进程重启后从挂载文件建立基线，不持久化最大代号。

```yaml
generation: 2
telegram:
  bots:
    ops:
      token: secret://ops_bot
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
    sha256: REPLACE_WITH_64_HEX_SHA256
  monitoring:
    file: /run/secrets/monitoring
    sha256: REPLACE_WITH_64_HEX_SHA256
```

示例 ID 和摘要占位内容必须替换。每个 secret 只接受 `file`、`sha256`；不支持内联明文、`env`、`fragments` 或远程 provider。最多 1152 个声明。SHA-256 必须是 64 个十六进制字符，计算范围是文件原始字节，包含末尾换行；匹配后才把材料解释为 UTF-8，并移除末尾 CR/LF。单文件上限 513 字节。

bot token 必须符合数字身份、冒号和 ASCII 字母数字/下划线/连字符材料的格式，最多 256 字节。调用方凭据必须是 32..=512 字节无空白 ASCII 文本，并由部署者保证足够随机。不同调用方不得共用凭据；同一 Telegram 数字身份即使 token 不同，也不能配置多个别名。

SHA-256 绑定的是材料内容，不是授权签名。能够修改目录及 secret 文件的主体本来就可以改变发送权；应通过文件权限和平台权限保护这两类来源。凭据摘要也应按受限配置对待，尤其不能对低熵口令依赖摘要保密。

## 动态字段

| 字段 | 默认值或限制 | 作用 |
| --- | --- | --- |
| `telegram.bots` | 0..=128 个 | 空表移除所有 bot |
| bot、目的地、调用方及 secret 别名 | 1..=64 个 ASCII 字母数字、`_`、`-`，首字符为字母数字 | 统一目录与路由身份 |
| `bots.<id>.description` | 空值时使用 bot 别名；最多 256 字符 | 目录用途说明 |
| `bots.<id>.token` | 必需 `secret://别名` | 引用机器人材料 |
| `destinations` | 每个 bot 1..=128 个 | 服务端维护目的地 |
| `chat_id` | 非零有符号整数，绝对值小于 2^52；或 `@名称` | Telegram 聊天身份 |
| `message_thread_id` | 可省略；正整数 | 论坛话题 |
| `default_destination` | 单目的地时自动推断；多目的地时需要显式指定或由请求选择 | 默认选路 |
| `delivery.queue_capacity` | 256；1..=4096 | 当前目录允许的待发预算，另外保留一个消费中位置 |
| `delivery.min_interval_ms` | 3100；0..=60000 | 相邻发送开始时刻的最小间隔 |
| `delivery.disable_notification` | `false` | 默认静默通知开关 |
| `delivery.protect_content` | `false` | 默认内容保护开关 |
| `telegram.clients` | 0..=1024 个 | 调用身份目录 |
| `clients.<id>.credential` | 必需 `secret://别名` | Bearer 材料引用 |
| `clients.<id>.allowed_bots` | 必需列表，可为空；只引用当前目录 bot | 发送与目录可见权限 |

队列配置总和不能超过 16384。进程同时受理中的消息硬上限为 16512，包括等待、发送和排空中的消息；当前加排空中的消费域最多 256 个。降低队列容量不会删除已受理消息，但会拒绝新提交直至回到预算内。

## 启动字段

以下资源设置在外部 YAML 中声明，但进程内不能改变；变化会拒绝整份候选，旧目录继续运行。需要先安排重启，再使用新预算启动。

| 字段 | 默认值 | 范围 |
| --- | --- | --- |
| `telegram.http.api_base` | `https://api.telegram.org` | HTTPS 根来源；仅回环地址允许 HTTP |
| `connect_timeout_ms` | 3000 | 1..=60000 |
| `request_timeout_ms` | 10000 | 1..=60000 |
| `body_read_timeout_ms` | 5000 | 1..=60000 |
| `max_response_bytes` | 65536 | 1024..=1048576 |
| `max_body_bytes` | 65536 | 1024..=1048576 |
| `max_inflight` | 64 | 1..=1024 |
| `telegram.dispatcher.shutdown_timeout_ms` | 20000 | 1..=300000 |

API 来源不允许用户信息、路径、查询或 fragment。应用停机预算的一半必须至少覆盖目录排空预算加 1000 毫秒余量。随附 `application.shutdown_timeout_ms=60000` 与目录 20000 毫秒匹配。

`application.*`、`log.*`、`server.*`、Nacos、来源路径、profile、`config_watch` 和 `catalog_watch` 也在启动时固定。运行期改变引导树或活动 profile 会使目录对账拒绝，恢复原文件或重启后才能继续。

## 日志配置

启用 `nasa` 的 `log` feature 并在应用宏中声明 `"log"`，由 napp 持有 nalog 的文件守卫及停机刷盘责任。日志配置放在引导文件或 profile；机器人目录不能包含 `log`。

| 配置键 | 本项目默认值 | 含义 |
| --- | --- | --- |
| `log.level` | `${TELEGRAM_LOG_LEVEL:info}` | EnvFilter 表达式，可按模块设置级别，例如 `info,telegram_bots=debug` |
| `log.path` | `${TELEGRAM_LOG_PATH:/usr/local/logs/telegram-bots}` | 默认写该目录；显式空值只写控制台，相对路径相对进程工作目录 |
| `log.max_file_size` | `100MB` | 单个活动日志文件达到 100 MiB 后滚动，跨日期也会滚动 |
| `log.total_size_cap` | `1GB` | `info` 和 `error` 各自匹配的归档上限 1 GiB，不包含活动文件 |
| `log.max_history_days` | `7` | 归档保留天数 |
| `log.clean_history_on_start` | `true` | 启用文件输出时清理过期及超限归档 |
| `log.split_error_file` | `true` | 额外写 `error.log`，保留 ERROR 事件 |
| `log.color` | `false` | 文件中不写 ANSI 颜色，控制台颜色由终端状态决定 |
| `log.pattern` | nalog 默认格式 | 可配置日期、级别、线程、logger 与消息，例如 `%d{yyyy-MM-dd HH:mm:ss.SSS} %-5level [%thread] %logger - %msg%n` |

`info.log` 包含通过级别过滤的事件，ERROR 同时写到独立的 `error.log`。清理在启动及滚动时执行，保留策略不是实时磁盘配额。日志目录应为本服务专用，不与其它进程共享同名活动文件和归档。

占位符由 naml 解析，nalog 接收解析后的目录。当前依赖 `naml 2.0.0` 不完整支持嵌套默认值：`${TELEGRAM_LOG_PATH:/usr/local/logs/${application.name}}` 在设置环境变量时会残留末尾 `}`。因此默认路径使用固定的 `/usr/local/logs/telegram-bots`；改变应用名不会自动改变该路径，应同时配置 `TELEGRAM_LOG_PATH`。

本地运行可设置 `TELEGRAM_LOG_PATH=logs` 避免系统目录权限要求；显式设置 `TELEGRAM_LOG_PATH=""` 只写控制台。级别可用 `TELEGRAM_LOG_LEVEL` 或 `APP__LOG__LEVEL` 覆盖；最终级别以合并后的 `log.level` 为准，不依赖 `RUST_LOG` 调整最终配置。日志配置无法解析、格式非法或文件打开失败会阻止启动，排查时检查目录权限和日志配置。启动失败及最终停机摘要由 napp 独立诊断通道输出，不能只查看文件日志。

保持运行中的引导文件不可变；调整日志参数后重启。机器人目录的热更新保持独立，不会修改日志设置。

## 应用时间与失败

`catalog_watch.poll_interval_ms` 范围 100..=1000，默认 1000；`load_timeout_ms` 范围 100..=3000，默认 3000。连续两轮成功候选内容一致才准备并发布目录，避免快速覆盖产生重复执行域；所有材料仍须通过摘要约束，稳定读取不能替代该约束。

源读取期间再次检查完整导入链。可选文件创建、删除、原子重命名和挂载符号链接更新都参与对账，不依赖文件事件到达。每轮由主程序 fork 出只读子进程，按父进程请求读取引导、导入链和凭据，并通过有界匿名通道返回文件字节。父进程只在内存中合并与校验，不让解析器重新打开来源；不通过命令参数、日志或临时文件传递材料。

napp 标准入口首先同步读取不可变引导文件和活动 profile，这一步不受业务读取预算限制，必须使用可靠本地存储。此后的业务初始化和运行期使用同一隔离边界，并复验引导快照一致。尚未读到预算时按 3 秒上限等待读取；读到 `load_timeout_ms` 后，从本轮开始时刻按该预算收紧期限。运行期整轮预算包含进程创建、文件传输、内存校验和来源复验。内存解析不是可抢占操作，取消与超时在异步阶段响应，候选完成时也会复验期限，迟到结果不能发布。读取超时或应用停机时杀死子进程，额外最多等待 1 秒回收；回收成功才允许下一轮轮询。启动超时非零退出且不建立 listener；运行期普通超时保留旧目录并自动继续轮询。

若操作系统因不可中断内核 I/O 等原因无法在回收预算内确认进程退出，服务进入失败停机，不创建更多读取进程。该情况需要平台终止容器、恢复挂载或处理节点；进程隔离不能保证解除内核级阻塞。平台终止预算还应覆盖服务的正常排空期限。

默认每秒发起一次读取机会；已有加载尚未结束时跳过该轮。服务正常调度、没有在途慢读取且本地文件读取较快时，发现变更通常需要至多约一个轮询周期，连续两次读取确认后的生效延迟通常约为 1–2 秒，再加读取、校验和目录切换耗时。轮询周期可以降低到 100 毫秒，但会增加完整配置与凭据的读取频率；修改该启动设置需要重启。

10 秒是稳定文件落地后的运维排查阈值，不是保证生效的硬上限。默认单次读取预算为 3 秒，超时后先完成进程回收，再记录拒绝并继续轮询；实际时刻仍受进程调度影响。平台投射延迟、持续文件写入、反复读取超时、凭据摘要不一致或排空中的身份重新加入，都可能延长等待或阻止应用新目录。错误不发布半份目录。无法解析有效代号时 `desired_generation` 为空，对应指标为 0；应结合最近拒绝原因与对账时刻判断。

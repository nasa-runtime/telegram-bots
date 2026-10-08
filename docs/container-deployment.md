# 容器部署

## 镜像与挂载

镜像地址为 [nasaruntime/telegram-bots](https://hub.docker.com/r/nasaruntime/telegram-bots)，支持 `linux/amd64` 和 `linux/arm64`。使用明确版本 `1.0.0`，需要固定内容时使用镜像 digest；`latest` 跟随最新发布内容。

[docker/Dockerfile](../docker/Dockerfile) 使用 Alpine 分阶段构建，Rust musl 编译启用体积优化、LTO 和符号裁剪。运行层只包含 Alpine、CA 证书、程序、许可证和引导文件，不携带编译器、源码、依赖缓存或机器人凭据。运行身份为非 root 用户 `10001:10001`，工作目录为 `/app`，启动命令为 `/app/telegram-bots`。

```sh
export TELEGRAM_BOTS_IMAGE=nasaruntime/telegram-bots:1.0.0
docker pull "$TELEGRAM_BOTS_IMAGE"
mkdir -p deploy-local/config deploy-local/secrets
cp examples/catalog-empty.yml deploy-local/config/telegram-bots.yml
docker run --detach --name telegram-bots \
  --user 10001:10001 --workdir /app --entrypoint /app/telegram-bots \
  --read-only --cap-drop ALL --security-opt no-new-privileges \
  --env TELEGRAM_LOG_PATH= \
  --publish 127.0.0.1:2060:2060 \
  --mount "type=bind,src=$PWD/deploy-local/config,dst=/etc/telegram-bots,readonly" \
  --mount "type=bind,src=$PWD/deploy-local/secrets,dst=/run/secrets,readonly" \
  "$TELEGRAM_BOTS_IMAGE"
```

配置目录需要可遍历，目录文件需要可读；凭据文件应仅允许 UID 10001 或其受控组读取。不要把 token 放入 Dockerfile、构建参数、镜像标签或普通 ConfigMap。

必须挂载**目录**。宿主机原子替换单个文件时，单文件 bind mount 可能仍指向旧 inode；目录挂载可以看到替换后的路径。不要覆盖 `/app/zcf/application.yml`。

基于 [catalog.yml](../examples/catalog.yml) 生成业务目录并填入真实目的地和凭据文件摘要；将 `generation` 增加到 `2`，通过同目录重命名替换 `/etc/telegram-bots/telegram-bots.yml`。以后每次变更递增代号。更新凭据文件时同时更新 YAML 摘要；过渡期目录会被拒绝，旧目录继续服务，匹配后自动应用。

根文件系统可以只读，无需持久化消息数据卷。容器平台可以使用 HTTP 探针；自行添加 Docker HEALTHCHECK 时，应根据 `/readyz` 的实际 HTTP 状态判断就绪。镜像构建者负责固定基础镜像 digest、漏洞管理、SBOM 和来源记录。

nalog 默认同时输出到控制台和 `/usr/local/logs/telegram-bots`。上面的命令显式将 `TELEGRAM_LOG_PATH` 设为空，交由平台采集标准输出。使用默认文件日志时，移除该环境变量，并另挂载 UID/GID `10001:10001` 可写的专用目录到 `/usr/local/logs/telegram-bots`。也可设置 `TELEGRAM_LOG_PATH` 选择其它挂载路径。该日志卷必须可写；配置与凭据卷仍保持只读。只读根目录下启用文件日志但未提供可写挂载会导致启动失败。`TELEGRAM_LOG_LEVEL` 控制启动日志级别，默认 `info`。

```sh
docker stop --time 70 telegram-bots
```

70 秒宽限期覆盖默认 60 秒应用停机预算。不要依赖 Docker 默认的短停机期限完成内存队列排空。

## Kubernetes

使用一个副本和 `Recreate` 策略，避免同一 bot 同时在两个 Pod 发送。没有分布式领导权或跨实例限流；把同一目录配置给多个副本会失去单消费域保证。

```yaml
apiVersion: apps/v1
kind: Deployment
metadata:
  name: telegram-bots
spec:
  replicas: 1
  strategy:
    type: Recreate
  selector:
    matchLabels:
      app: telegram-bots
  template:
    metadata:
      labels:
        app: telegram-bots
    spec:
      terminationGracePeriodSeconds: 70
      securityContext:
        runAsNonRoot: true
        runAsUser: 10001
        runAsGroup: 10001
        fsGroup: 10001
      containers:
        - name: telegram-bots
          image: nasaruntime/telegram-bots:1.0.0
          workingDir: /app
          command: ["/app/telegram-bots"]
          env:
            - {name: TELEGRAM_LOG_PATH, value: ""}
          securityContext:
            readOnlyRootFilesystem: true
            allowPrivilegeEscalation: false
            capabilities:
              drop: [ALL]
          ports:
            - name: http
              containerPort: 2060
          resources:
            requests:
              cpu: 100m
              memory: 128Mi
            limits:
              cpu: "2"
              memory: 512Mi
          startupProbe:
            httpGet: {path: /readyz, port: http}
            periodSeconds: 2
            failureThreshold: 30
          readinessProbe:
            httpGet: {path: /readyz, port: http}
            periodSeconds: 5
          livenessProbe:
            httpGet: {path: /healthz, port: http}
            periodSeconds: 10
          volumeMounts:
            - {name: catalog, mountPath: /etc/telegram-bots, readOnly: true}
            - {name: materials, mountPath: /run/secrets, readOnly: true}
      volumes:
        - name: catalog
          configMap:
            name: telegram-bots-catalog
        - name: materials
          secret:
            secretName: telegram-bots-materials
            defaultMode: 0440
```

示例资源限制需要按机器人数量、排队预算和正文大小调整；达到最大目录与队列容量时不能假定 512Mi 足够。默认容量也不是流量承诺。

ConfigMap 的键为 `telegram-bots.yml`；Secret 的键与 `secrets.*.file` 的文件名一致。**不要使用 `subPath` 挂载**，也不要把投射目录在初始化时复制到另一个永久目录。服务每轮按路径重新打开文件，支持 Kubernetes `..data` 符号链接切换。

ConfigMap 与 Secret 可以分开更新。YAML 中的摘要把凭据绑定到确切材料；任一材料尚未落地时保留整份旧目录。容器内最终文件可见且稳定后，超过 10 秒仍未生效应检查配置状态；这是运维排查阈值，不是端到端应用期限，Kubernetes 投射延迟另计。

如需要内部 Service：

```yaml
apiVersion: v1
kind: Service
metadata:
  name: telegram-bots
spec:
  selector:
    app: telegram-bots
  ports:
    - {name: http, port: 2060, targetPort: http}
```

## Nacos

设置 `APP_PROFILE=nacos`，提供 `NACOS_SERVER_ADDR`、`TELEGRAM_REGISTER_IP`，以及环境需要的 `NACOS_NAMESPACE`、`NACOS_GROUP`、`NACOS_USERNAME`、`NACOS_PASSWORD`。注册 IP 必须能被调用方访问；Pod 中可通过 Downward API 注入 `status.podIP`。

Nacos 只发布实例寻址信息。它不读取 bot YAML，不提供跨实例队列协调，也不替代 HTTP Bearer 认证。来源和 Nacos profile 在启动时固定，调整它们需要重启。

默认来源模式为 `/etc/telegram-bots/*.yml`，单文件部署继续使用 `telegram-bots.yml` 即可。拆分多文件时，每份 YAML 都声明相同 generation，更新时递增整个集合；临时文件使用不匹配后缀再 rename。日志默认目录按 `${application.name}` 展开，容器挂载必须覆盖展开后的路径，环境显式空值关闭文件输出。

## 从源码构建

在仓库根目录执行，构建上下文由 `.dockerignore` 限定为程序和必要配置，不会包含本地凭据或运行目录：

```sh
docker build --file docker/Dockerfile --tag telegram-bots:local .
```

默认构建当前主机架构；其它架构由 Docker Buildx 的 `--platform` 指定。构建需要访问基础镜像仓库、Alpine 软件源及 crates.io。基础镜像按 digest 固定，Rust 依赖按 `Cargo.lock` 固定。

## 镜像发布

`release` 分支的 Rust 工作流在编译与文档检查成功后，为 amd64、arm64 分别构建镜像归档。`Publish Docker image` 工作流从这些归档发布，不在上传阶段重新构建。手动运行时选择 `release`，提供成功的构建 run ID 和两个已确认的 image ID；流程会核对来源提交、分支、平台、版本和镜像身份，拒绝覆盖已有版本标签。

GitHub Actions 使用 Secret `DOCKERHUB_TOKEN`、Variable `DOCKERHUB_USERNAME`。个人仓库默认以用户名作为 namespace；`DOCKERHUB_NAMESPACE` 可显式指定，本项目目标为 `nasaruntime/telegram-bots`。Token 需要镜像推送与仓库说明更新权限。镜像版本取自 `Cargo.toml`；发布过程同时更新 Docker Hub 概览，最后让 `latest` 指向该版本。若版本已上传而后续步骤失败，需按远端现状处理，不能通过覆盖版本标签掩盖部分完成状态。

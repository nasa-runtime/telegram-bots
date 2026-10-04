# 贡献说明

## 开发环境

使用 Rust 1.94 或更新版本，保持 `Cargo.lock`。产品依赖来自 crates.io 的 `nasa 2.0.1` 及锁定依赖图，无需相邻项目或本地路径覆盖。

```sh
cargo fmt --all --check
cargo clippy --all-targets --locked -- -D warnings
cargo build --locked
cargo doc --locked --no-deps
cargo package --locked --allow-dirty
```

本服务以源码和业务方构建的容器交付，manifest 设置 `publish = false`。打包可用于检查源码制品中的 README、配置、许可证及文件清单，不表示发布到 registry。

## 实现约束

修改目录或发送逻辑时保持以下合同：完整候选先校验后发布；权限与资源同代；旧代请求不能在切换后入队；同一 Telegram 身份只有一个消费者；关闭受理先于排空；出站最多尝试一次；取消终态不能把未知结果记为成功。

新增业务函数用中文说明业务作用、参数与返回语义；关键门禁与停机顺序说明设计原因。质量用例、fixture、注入工具和运行记录仅本地使用，不进入公开仓库或产品归档。

修改公开 API、配置、指标或失败语义时同步更新中英文 README 及对应参考文档。示例只能包含格式示意和材料引用，不包含实际 token、调用凭据或聊天身份。

## 提交内容

变更说明应描述具体使用场景、最终行为、兼容边界和验证方法。保持代码变更与目标功能相关；不要夹带本地配置、构建缓存、消息日志、编辑器状态或材料文件。

项目采用 [MIT](LICENSE-MIT) 或 [Apache-2.0](LICENSE-APACHE) 双许可证。贡献者应确认有权提供所提交的代码与文档。安全问题请遵循 [SECURITY.md](SECURITY.md) 的私有报告方式。

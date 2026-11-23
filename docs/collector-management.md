# 内置配置化采集（0.2.4）

采集已并入 `rproxy` 的 Tokio 异步任务；不再需要外部 worker、独立令牌或语言运行时。`rproxy-cli` 管理来源，SQLite 保存加密配置，内置采集器通过 typed Store 接口提交候选。网络请求不在数据库状态线程内执行。

## 启动配置

在服务端 `rproxy.toml` 添加：

```toml
[collector]
enabled = true
concurrency = 4       # 1..64
poll_seconds = 3      # 1..300
# 相对服务端 TOML 目录；不配置时变量映射为空。
env_file = "providers.env"
```

复制根目录 `providers.env.example` 为 `providers.env`，只放供应商需要的 UTF-8 `KEY=VALUE`：

```dotenv
SUPPLIER_API_URL=https://your-provider.example/extract
SUPPLIER_API_TOKEN=your-private-token
```

支持空行、注释、可选引号，不执行 shell，不做进程变量展开。文件最多1MiB。**`${ENV}` 只读取该文件，绝不回退到进程环境**；不要把 root token、`RPROXY_SOURCE_KEY` 或入口密码放进它。更改文件/启动设置需要重启原生进程或重新创建容器；来源定义在线更新无需重启。

可选 Docker 部署用 `--mount` 将供应商文件只读挂载为 `/etc/rproxy/providers.env`，不是进程环境注入；只运行一个生产容器，不需要编排工具。使用示例配置时，空池也应保留空文件。禁用内置采集不影响手动添加候选或代理验证/转发。

## 在线来源管理

```sh
cp sources.example.toml sources.toml
# 编辑后从宿主标准输入导入：
docker exec -i rproxy rproxy-cli source import - < sources.toml
docker exec -i rproxy rproxy-cli source list
docker exec -i rproxy rproxy-cli source resume vendor-a
docker exec -i rproxy rproxy-cli source run vendor-a --wait --wait-timeout 60
docker exec -i rproxy rproxy-cli source pause vendor-a
```

支持 `[[sources]]`、旧 `[[providers]]` TOML 和 JSON。来源仅通过显式导入进入 SQLite，不自动把文件当作第二份运行权威；普通查询/导出脱敏。来源 ID 不随重命名/同名重建复用。暂停/删除停止未来采集，默认不撤销已有代理和 stick。

## 通用映射

以下是独立来源导入 manifest，不是服务端 TOML：

```toml
schema_version = 1

[[sources]]
name = "text-provider"
kind = "api"
enabled = true
[sources.config]
url = "${SUPPLIER_API_URL}"
protocol = "http"
method = "GET"
interval_seconds = 60
timeout_seconds = 10
attempts = 3
[sources.config.params]
count = 10
[sources.config.headers]
Authorization = "Bearer ${SUPPLIER_API_TOKEN}"
[sources.config.response]
format = "text"
```

JSON 示例：

```toml
[[sources]]
name = "json-provider"
kind = "api"
enabled = false
[sources.config]
url = "${JSON_PROVIDER_API_URL}"
protocol = "socks5"
method = "POST"
[sources.config.json]
count = 1
[sources.config.response]
format = "json"
items_path = "data.proxies"
address_path = "endpoint"
# 或同时配置 ip_path="ip" 和 port_path="port"。
# 可同时配置 success_path="code" / success_value=0，要求精确 JSON 值。
```

文本按空白、逗号、分号分隔。JSON 路径支持点号和数字数组下标，空路径指当前值。只接受数字 IPv4/IPv6 和合法端口，IPv6 地址端点需 `[address]:port`；拒绝域名、嵌入凭据、未指定地址和 scoped IPv6，规范化后去重。

GET 不带 JSON 请求体；只支持 GET/POST 和 http/socks5 上游，不执行脚本/签名。默认60秒间隔、10秒请求预算、3次尝试；上限86400秒间隔、300秒请求预算、5次尝试。每次尝试的超时覆盖响应体读取，不让慢速滴流无限延长。映射错误不重试；传输/状态/尺寸失败使用有界重试和退避。

供应商响应按**解压后1MiB**限制，每次最多提取4096个地址；每批最多256个候选，最后一批才标记完成，空成功结果也会完成。禁止重定向和 ambient proxy；默认验证 TLS。可选 `ca_certificate` 相对服务端 TOML，`insecure=true` 仅为明确的供应商证书例外，不影响目标 TLS 隧道。

Xdaili/Shenlong 等使用当前账户文档填写 URL/字段；不为 Rola 编造接口，不声称认证旋转网关或供应商 sticky-session 参数已支持。

## 调度与安全边界

只调度启用且未删除的 API 来源；单来源最多一个当前任务，全局并发有界，无无限排队。来源版本或手动 run generation 更新后及时重新调度；暂停、删除、修改和服务关闭会取消/丢弃旧工作。每批结果仍由 Store 原子核对来源 ID、启用状态、版本和 generation，迟到旧结果不能复活配置。

来源失败彼此隔离，不影响其他来源或业务转发。安全状态只有 `env-missing`、`mapping-failed`、`fetch-failed`，不回显 URL、参数、请求头/体或秘密。采集成功表示提交候选，不表示实际出口验证通过；继续使用 CLI proxy list/show/verify 查询。

外部 `/v1/collector/plan` / `/v1/collector/results` 已移除。旧 `admin.worker_token_env` 只兼容解析，不能认证任何请求；管理接口只接受 root token，不能通过重启旧 worker恢复采集。

## 从旧 worker 迁移

1. 停止并备份原部署的整个状态目录和密钥；Docker 迁移保留原实际卷名，包括旧编排部署可能使用的项目名前缀。
2. 服务端添加 `[collector]`；将原供应商变量迁移到专用 `providers.env`。
3. 保留 SQLite 中已管理来源，无需重复导入。0.2.2旧 provider manifest 可用 CLI显式导入。
4. 移除 worker token 和旧 `scripts/proxy.toml` 启动引用；确认所有权后停止自己的旧 worker。可用原生服务或直接 docker run 启动单个 rproxy 容器，沿用状态卷；不删除数据卷或无关资源。
5. `source run --wait` / `proxy list` 检查任务及候选验证结果。

更详细的实现和本地测试证据见 [internal-collector.md](internal-collector.md)。

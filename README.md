# rproxy

面向多个业务、登录账号的 HTTP / HTTPS / SOCKS5 代理池。**0.2.4 将配置化采集并入 Rust，使用内嵌 SQLite，生产环境只运行一个服务；不再依赖 Python、Redis 或独立 worker。**

- `rproxy`：协议入口、认证、内置异步采集、候选验证、实际出口检测、四种选路策略和持久绑定。
- `rproxy-cli`：在线管理用户、来源、手动代理和会话，通过认证管理接口操作，不直接读写 SQLite。
- `xtask`：仅供开发、测试和发布使用，不放进生产镜像或原生发布包。

原生包和生产镜像均包含 **rproxy / rproxy-cli 两个二进制**。详见 [CLI 手册](docs/rproxy-cli.md)、[采集配置](docs/internal-collector.md) 和 [开发工具](https://github.com/flyfoxCI/rproxy/blob/main/docs/xtask.md)。

## 能力与边界

支持 HTTP 正向代理、HTTP CONNECT、TLS 加密的 HTTPS 代理入口、SOCKS5 TCP CONNECT，以及 HTTP / SOCKS5 上游交叉组合。支持 IPv4、域名和 IPv6 目标；不解密目标 HTTPS，不做 MITM。

同业务不同会话不能共用**实际出口 IP**，不同业务可以共享。`stick` 跨连接、认证续期和重启保留绑定；IPv4-mapped IPv6 出口按 IPv4 归一化。旧连接未排空时，旧出口不会被提前交给同业务其他会话。

| 策略 | 新 TCP 连接的行为 |
|---|---|
| `stick` | 健康时沿用绑定，确认上游失效后记录日志并换绑 |
| `random` | 避免上次出口；只剩旧出口时明确失败，不静默重复 |
| `round-robin` | 在合格、未被其他会话占用的出口间轮换；单候选可重复 |
| `shuffle` | 按会话随机不放回，一轮结束重洗，可行时避免跨轮重复 |

选择粒度不是单个 HTTP 请求：keep-alive 沿用当前出口，已建立的隧道不能中途迁移。不因无可用代理而直连、共享被占出口或自动降级认证，也不重放已经发送的业务请求。

**一个 SQLite 数据库只允许一个 Rust 进程持有。** 使用支持可靠文件锁的本地文件系统，不使用 NFS/SMB，不扩容共享数据库的副本。运行中不要删除/替换数据库、锁或密钥文件。Linux 锁定数据库 inode；macOS/Windows 使用旁路 `.lock` 文件，避免 macOS 文件锁与 SQLite 冲突。

当前不支持 SOCKS5 UDP/BIND、HTTPS 上游、上游用户名密码、供应商认证旋转网关/粘性会话参数、不可控轮换出口、网站 Cookie/登录管理或多机调度。HTTPS 代理入口不等于 HTTPS 上游。

## Docker / Podman Compose 一键启动

需要 Docker Engine/Desktop + Compose v2 或更新版本，或 Podman + 近期 Compose provider。Windows/macOS 使用 Podman 时先启动现有 machine：

```sh
podman machine start # 已启动时不用重复
podman compose version
```

### 1. 准备宿主机文件

首次在仓库根目录复制配置，**已有文件不要覆盖**；PowerShell 使用 `Copy-Item`：

```sh
cp rproxy.example.toml rproxy.toml
cp providers.env.example providers.env
cp .env.example .env
```

用编辑器填写 `.env`：

```dotenv
RPROXY_PASSWORD=首次导入alice用户的入口密码
RPROXY_ADMIN_TOKEN=独立随机管理员令牌
RPROXY_BIND_ADDRESS=127.0.0.1
RPROXY_HTTP_PORT=9099
RPROXY_SOCKS5_PORT=9089
RPROXY_HTTPS_PORT=9443
```

管理令牌需 16–1024 个非空白可打印 ASCII 字符；可用密码管理器或 `openssl rand -hex 32` 生成。不要把真实值写进命令参数、Git 或工单。**不再需要 `RPROXY_WORKER_TOKEN`。**

`RPROXY_PASSWORD` 只用于示例用户的一次性导入，CLI 改密不会被旧 `.env`/TOML 在重启时覆盖。也可删掉 TOML 中的 `[[auth.users]]`，以空的 required-auth 池启动后用 CLI 新增第一个用户。

示例服务配置已经包含：

```toml
[collector]
enabled = true
concurrency = 4
poll_seconds = 3
env_file = "providers.env"
```

`providers.env` 是供应商专用变量文件，**不是进程环境文件**。只填写来源需要的 `KEY=VALUE`，不要放管理员令牌、主密钥或入口密码。即使暂无来源，也保留一个空文件供只读挂载。相对路径基于服务端 TOML 的目录。

### 2. 启动唯一生产服务

```sh
docker compose config --quiet
docker compose up -d --build
docker compose ps
docker compose logs --tail=100 rproxy
docker compose exec -T rproxy rproxy-cli --json health
```

Podman 将命令前缀换成 `podman compose`。首次构建需联网；宿主机不需要 Rust 或其他语言运行时。生产 target 为 `rproxy`；`test-fixture` 只用于开发烟测，不是常驻生产服务。

默认仅映射 HTTP `127.0.0.1:9099` 和 SOCKS5 `127.0.0.1:9089`，管理端口 9100 不发布到宿主机。**健康不表示已有可用代理。** 新库无来源时内置采集器空闲；空池请求失败，不会绕过代理直连。

## 在线管理

以下使用容器内 CLI，原生安装直接执行 `rproxy-cli ...`。原生默认管理地址 `http://127.0.0.1:9100`，支持 `--endpoint` / `RPROXY_ADMIN_URL`。`--token-env` 接受变量名，不是令牌值。`--json` 供脚本使用；失败返回非零退出码。

### 用户、密码和业务授权

```sh
# 交互输入，不回显；不要使用明文 --password。
docker compose exec rproxy rproxy-cli user add bob --business shop --business news
docker compose exec rproxy rproxy-cli user passwd alice
docker compose exec -T rproxy rproxy-cli user grant bob --business shop,news
docker compose exec -T rproxy rproxy-cli user list
docker compose exec -T rproxy rproxy-cli user show bob

docker compose exec -T rproxy rproxy-cli user disable bob --dry-run
docker compose exec -T rproxy rproxy-cli user disable bob --yes
docker compose exec -T rproxy rproxy-cli user enable bob
# 明确撤销该用户已有隧道：
docker compose exec -T rproxy rproxy-cli user disable bob --disconnect --yes
```

自动化使用 `--password-stdin` 或 `--password-env 变量名`。`grant` 替换完整业务列表，清空授权意味着没有业务访问权。密码只保存 Argon2id 哈希。

- 改密立即影响后续认证，但不释放或更换 stick。
- 禁用/删除拒绝新认证；已有隧道默认排空，`--disconnect` 才明确强制断开。
- 用户有不可变内部 ID，删除重建同名用户不会接管旧绑定。
- **删除最后一个用户不会开放匿名。** 只有显式设置 `[auth] mode="anonymous"` 才允许匿名。
- 管理员令牌独立，删除全部入口用户不会锁死管理接口。

### 配置化来源

```sh
cp sources.example.toml sources.toml
# 编辑 sources.toml，填写真实 URL/字段映射或 ${ENV} 引用，配置后设 enabled=true。
# 标准输入读取宿主文件，无需向只读容器复制文件：
docker compose exec -T rproxy rproxy-cli source import - < sources.toml
docker compose exec -T rproxy rproxy-cli source list
docker compose exec -T rproxy rproxy-cli source show vendor-a
docker compose exec -T rproxy rproxy-cli source resume vendor-a
docker compose exec -T rproxy rproxy-cli source run vendor-a --wait --wait-timeout 60
docker compose exec -T rproxy rproxy-cli source import - --replace < sources.toml
docker compose exec -T rproxy rproxy-cli source pause vendor-a
docker compose exec -T rproxy rproxy-cli source remove vendor-a --yes
```

SQLite 是运行时来源权威；支持新 `[[sources]]`、旧 `[[providers]]` TOML 及 JSON manifest 的显式导入。修改、暂停和手动执行来源不需重启。旧版本/旧 run generation 的迟到结果会被拒绝，暂停/删除默认不撤销已有代理或 stick。

GET/POST、params/headers/JSON 请求体、文本/JSON 字段路径等均通过配置实现，不为每家供应商写一个类。`${SUPPLIER_API_URL}` 等引用**只从 `collector.env_file` 解析**，不存在时返回 `env-missing`，不会回退读取进程中的 root token/master key。修改此文件后重新创建 rproxy；来源定义本身在线生效。

```sh
docker compose up -d --force-recreate rproxy
```

来源配置整体加密保存。默认在状态目录生成 `.sqlite3.key` 私有文件（Unix 0600），密钥不在 SQLite；也可在受控服务环境中显式注入 base64 编码的 32 字节 `RPROXY_SOURCE_KEY`。丢失/错误密钥会拒绝启动，不会重置数据。普通查询/导出脱敏：

```sh
docker compose exec -T rproxy rproxy-cli source export > redacted-sources.json
```

完整秘密导出必须显式 `--include-secrets --output 私有文件 --yes`，不输出到 stdout，不覆盖已有文件。来源采集完成不表示候选已经验证可用。Xdaili/Shenlong 映射需按当前接口核对；Rola 仅在账户提供 IP 白名单提取接口时配置真实 URL/映射，不猜测 API，不声称支持认证旋转网关。

### 手动代理与会话

```sh
# 文档 IP 必须替换成真实上游；IPv6 使用 [地址]:端口。
docker compose exec -T rproxy rproxy-cli proxy add http 203.0.113.10:8080 --wait
docker compose exec -T rproxy rproxy-cli proxy add socks5 203.0.113.20:1080 --source manual
docker compose exec -T rproxy rproxy-cli proxy import http - --wait < addresses.txt
docker compose exec -T rproxy rproxy-cli proxy list --protocol http
docker compose exec -T rproxy rproxy-cli proxy show 'http:203.0.113.10:8080'
docker compose exec -T rproxy rproxy-cli proxy verify
docker compose exec -T rproxy rproxy-cli proxy disable 'http:203.0.113.10:8080' --dry-run
docker compose exec -T rproxy rproxy-cli proxy remove 'http:203.0.113.10:8080' --yes

docker compose exec -T rproxy rproxy-cli session list --business shop
docker compose exec -T rproxy rproxy-cli session release shop account-a --dry-run
docker compose exec -T rproxy rproxy-cli session release shop account-a --yes
```

提交是幂等候选添加，**不是免检准入**。`--wait` 对拒绝/超时返回非零；实际出口、稳定性和 CONNECT 能力由 Rust 验证。同一协议/地址可关联多个来源。`probe.enabled=false` 只关闭定期检测，新候选和重启遗留候选仍需验证。

管理员释放会话不需要用户密码；旧连接排空前仍保护出口，强制断开需显式选择。删除用户不静默释放其全部 stick。

## 配置、挂载与 TLS

| 数据 | 权威来源 | 如何变更 |
|---|---|---|
| 监听、TLS、认证模式、限额、采集并发/轮询/变量文件路径 | 宿主 `rproxy.toml` → `/etc/rproxy/rproxy.toml`，只读 | 重新创建 rproxy |
| 用户、授权、来源、手动代理、绑定 | 命名卷 `/var/lib/rproxy` 的 SQLite | CLI 在线操作 |
| 管理令牌/首次导入密码 | `.env` 指定注入 | 改动后重新创建；旧用户不被覆写 |
| 供应商变量 | 宿主 `providers.env` → `/etc/rproxy/providers.env`，只读 | 重新创建 rproxy |
| HTTPS/自定义 CA 证书 | 显式只读挂载目录 | 重新创建 rproxy |

Compose 固定内部 HTTP/SOCKS5/管理监听 `0.0.0.0:9099/9089/9100` 和数据库 `/var/lib/rproxy/rproxy.sqlite3`。外部端口/绑定 IP 用 `.env` 调整。宿主路径可用 `RPROXY_CONFIG`、`RPROXY_PROVIDERS_ENV`、`RPROXY_CERTS_DIR` 指定。重新创建能处理编辑器原子替换 inode；单独 restart 不重新注入环境。

容器使用 `10001:10001`、只读根文件系统和最小权限；确保专用配置/证书可读、状态卷可写，不使用 chmod777。缺文件时 `create_host_path:false` 拒绝启动。部分 Compose provider 不转发 SELinux `Z`，enforcing 环境应给专用文件设置正确容器标签，不关闭 SELinux 或重标系统目录。

可选 HTTPS 入口：准备 `certs/fullchain.pem` / `certs/private-key.pem`，TOML 顶层启用 `https_bind="0.0.0.0:9443"` 并设置 `[tls]` 的 `certificate="certs/fullchain.pem"`、`private_key="certs/private-key.pem"`：

```sh
docker compose -f compose.yaml -f compose.https.yaml up -d --build
```

后续始终带同样两个 `-f`。仅发布端口不会启用 TLS；证书错误拒绝启动，不降级。自签名入口需客户端显式信任（curl `--proxy-cacert`）；目标 HTTPS 仍由客户端验证。普通 HTTP Basic / SOCKS5 密码不加密，跨不可信网络用 HTTPS 入口或安全通道。管理接口不要裸露公网。

采集器供应商 CA 与 `[probe] ca_certificate` 分开配置，相对路径均基于服务端 TOML；自定义 CA 需显式只读挂载。默认校验证书，供应商 `insecure=true` 是明确例外，不影响客户端 TLS 隧道。

## 标准客户端

用户名格式：`v1|入口用户名|业务标识|会话标识|策略`。标识为1–64个 ASCII 字母数字或 `._-`，完整用户名最多255字节；普通用户名使用首个允许业务和默认会话/策略。不同网站账号使用不同会话。

```sh
curl --proxy http://127.0.0.1:9099 --proxy-user 'v1|alice|shop|account-a|stick:YOUR_PASSWORD' https://example.com/
curl --proxy socks5h://127.0.0.1:9089 --proxy-user 'v1|alice|shop|account-a|stick:YOUR_PASSWORD' https://example.com/
```

放进 URL 时编码用户名/密码，独立认证字段使用原始值。部分浏览器不支持 SOCKS5 用户名密码，可改用 HTTP/HTTPS 入口。认证 TTL 与绑定独立，固定 TTL 到期重新验证不更换 IP；不会强行关闭已经建立的隧道，显式撤销除外。

## 升级、停止与备份

**从0.2.3源码/候选构建升级：** 保留 schema2 数据库、密钥、用户/来源 ID 和 stick；无需重导入已管理来源。停止旧部署并备份，向 `rproxy.toml` 添加 `[collector]`；将原 `scripts/providers.env` 的供应商变量安全迁移到新的 `providers.env`。删除 `.env` 的 worker token 和旧 worker 启动配置引用；`admin.worker_token_env` 仅兼容解析，已不具认证作用。旧 `/v1/collector/*` HTTP 接口已移除。使用原 Compose 项目名和卷，确认后以 `up -d --build --remove-orphans` 移除该项目的旧 collector 容器；不删除卷。

**从0.2.2升级：** 先停止备份；SQLite 会迁移旧用户归属并保留绑定，TOML 用户只导入一次。旧 provider TOML 用 CLI 显式导入，不再作为独立采集配置。回退旧 schema 版本需恢复升级前备份。

**Redis版0.2.1及以前：** 需要维护窗口并重新导入候选，不自动迁移 Redis stick/占用，不承诺保留旧出口。

```sh
docker compose down             # 保留卷和宿主文件；不要加 -v
docker compose up -d --build
# 备份：先停止，复制整个状态目录（含 WAL、.key）和宿主配置。
mkdir -p backup
docker compose stop rproxy
docker compose cp rproxy:/var/lib/rproxy ./backup/
docker compose up -d
```

不要运行中只复制 SQLite 主文件，不要更换 Compose 项目名造成空卷。损坏、未知 schema 或无法解密时失败关闭，不自动清空重建。`data/`、私有配置及旧本地虚拟环境不会被升级脚本删除。

## 原生构建、验证与发布

```sh
cargo build --release --locked -p rproxy --bins
cp rproxy.example.toml rproxy.toml
cp providers.env.example providers.env
# 在服务环境设置 RPROXY_ADMIN_TOKEN 及首次bootstrap的 RPROXY_PASSWORD；不自动读取.env。
./target/release/rproxy --config rproxy.toml check-config
./target/release/rproxy --config rproxy.toml
# 另一个终端使用同一 root token：
./target/release/rproxy-cli user list

cargo fmt --all --check
cargo clippy --locked --workspace --all-targets -- -D warnings
cargo test --locked --workspace --all-targets
cargo run --locked -p xtask -- compose-smoke --engine podman # 或docker
```

Windows 使用 `.exe`；本机 GNU toolchain 需 MinGW 工具在 PATH，低内存用 `-j1`。`check-config` 不打开 SQLite、不要求密钥环境值；首次启动/迁移才解析导入凭据。旧 `healthcheck/add-proxy/verify-pool/release-session` 命令保留，新管理操作优先 CLI。

Compose 烟测只使用随机项目、临时配置、本地受限 HTTP/SOCKS5 fixture 和自己的卷，结束只清理自身资源。原生发布门禁运行本次构建的两个可信二进制，执行真实 SQLite 启动及 CLI 用户创建/读取；不执行下载或归档中的程序。

0.2.4 的发布目标为 Linux x86_64/ARM64 musl、macOS Intel/Apple Silicon（11.0+，未签名/公证）、Windows x86_64 MSVC，五个归档和 SHA256SUMS。`xtask verify` 检查完整集合、两可执行文件及归档路径/类型，再允许 draft 发布。详见 [0.2.4 变更](https://github.com/flyfoxCI/rproxy/blob/main/docs/releases/v0.2.4.md)；是否已发布以 [Releases](https://github.com/flyfoxCI/rproxy/releases) 为准。历史标签/发行版不覆盖，v0.2.3 保持未发布，不移动标签。

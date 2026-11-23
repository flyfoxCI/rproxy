# rproxy

用 Rust 实现的 HTTP / HTTPS / SOCKS5 代理池，适合多个业务、多个登录账号需要独立且稳定出口的场景。

**v0.2.4：采集、验证、认证、选路和 SQLite 状态管理都在一个 `rproxy` 进程中运行，不再需要 Python、Redis 或独立 worker。**

- **`rproxy`**：代理入口、配置化采集、候选验证、实际出口检测、会话绑定。
- **`rproxy-cli`**：在线管理用户、密码、业务授权、来源、手动代理和会话。
- **`xtask`**：开发和发布工具，不进入生产镜像或原生发布包。

原生发布包与生产镜像只包含 **`rproxy`、`rproxy-cli` 两个程序**。

## 当前实现与发布状态

本文描述仓库中**已经实现的代码**，不把设计文档当成已交付功能：

- 当前源码版本为 **0.2.4**，可直接构建原生二进制运行，不依赖容器；仓库也仍保留 Docker/Podman Compose 部署与集成测试。
- **“仅原生交付、所有配置统一为一个 TOML”尚未实现**，目前只有[设计文档](https://github.com/flyfoxCI/rproxy/blob/main/docs/superpowers/specs/2026-11-23-native-single-config-v024-design.md)。
- 当前配置仍分为服务端 `rproxy.toml`、可选的供应商变量文件 `providers.env`、显式导入的来源 manifest；`.env` 仅供 Compose 使用。来源与用户的在线修改保存在 SQLite，重启不覆盖。
- `rproxy-cli` 目前**不读取服务端 TOML，也不支持 `--config`**；管理地址来自 `--endpoint` / `RPROXY_ADMIN_URL`，令牌来自 `--token-env` 指定的环境变量（默认 `RPROXY_ADMIN_TOKEN`）。
- **v0.2.4 尚未公开发布 Release**。五平台归档为发布目标，不代表已生成或验证；已公开版本以 [Releases](https://github.com/flyfoxCI/rproxy/releases) 为准。

## 目录

- [功能与边界](#功能与边界)
- [Docker / Podman 快速开始](#docker--podman-快速开始)
- [原生构建与发布包](#原生构建与发布包)
- [接入代理来源](#接入代理来源)
- [客户端连接与选路](#客户端连接与选路)
- [在线管理](#在线管理)
- [配置、数据与安全](#配置数据与安全)
- [升级、备份与停止](#升级备份与停止)
- [Python 功能迁移对照](#python-功能迁移对照)
- [开发、测试与发布](#开发测试与发布)

## 功能与边界

| 功能 | 支持情况 |
|---|---|
| 客户端入口 | HTTP 正向代理、HTTP CONNECT、TLS 加密的 HTTPS 代理入口、SOCKS5 TCP CONNECT |
| 上游代理 | HTTP、SOCKS5；可与不同入口交叉组合 |
| 目标地址 | IPv4、域名、IPv6 |
| 入口认证 | 用户名密码；也可显式启用匿名模式 |
| 选路策略 | `stick`、`random`、`round-robin`、`shuffle` |
| 来源采集 | 配置化 GET/POST、请求参数/头/JSON 请求体、文本/JSON 响应映射 |
| 状态持久化 | 内嵌 SQLite，保留用户、来源、授权和会话绑定 |
| 部署 | 单服务 Compose；或五个平台的原生二进制 |

**出口隔离按实际出口 IP 判断，不只比较供应商返回的 IP:port。** 同业务的不同会话不能共用实际出口，不同业务可以共享。`stick` 绑定跨连接、认证续期和服务重启保留；旧连接未排空时，旧出口不会提前分给同业务的其他会话。

不解密目标 HTTPS，不做 MITM。不因空池而直连、不突破出口排他、不自动降级认证，也不重放已经发送的业务请求。

当前不支持 SOCKS5 UDP/BIND、HTTPS 上游、上游用户名密码、供应商认证旋转网关/粘性会话参数、不可控轮换出口、网站 Cookie/登录管理或多机调度。**HTTPS 代理入口不等于 HTTPS 上游。**

## Docker / Podman 快速开始

需要 Docker Engine/Desktop + Compose v2 或更新版本，或 Podman + 近期 Compose provider。以下命令在仓库根目录执行；Podman 将 `docker compose` 换成 `podman compose`。Windows/macOS 的 Podman machine 若尚未启动，先执行 `podman machine start`。

### 1. 准备配置

首次复制示例，已有配置不要覆盖。以下是 sh / Git Bash 命令；PowerShell 可使用 `Copy-Item`：

```sh
cp -n rproxy.example.toml rproxy.toml
cp -n providers.env.example providers.env
cp -n .env.example .env
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

- 管理令牌需 16–1024 个非空白可打印 ASCII 字符，可用密码管理器或 `openssl rand -hex 32` 生成。不要将真实凭据放进命令参数、Git 或工单。
- `RPROXY_PASSWORD` 仅用于示例用户 `alice` 的一次性导入。之后 CLI 改密不会被旧 `.env`/TOML 在重启时覆盖。
- 若要以空的 required-auth 池启动，删掉 TOML 的 `[[auth.users]]`，启动后通过 CLI 创建用户。
- `providers.env` 只放供应商变量；暂无来源时也保留空文件供挂载。**不需要 `RPROXY_WORKER_TOKEN`。**

### 2. 启动服务

```sh
docker compose config --quiet
docker compose up -d --build
docker compose ps
docker compose logs --tail=100 rproxy
docker compose exec -T rproxy rproxy-cli --json health
```

首次构建需联网，宿主机不需要 Rust 或 Python。生产环境只有一个 `rproxy` 服务，默认发布：

| 入口 | 宿主地址 |
|---|---|
| HTTP / CONNECT | `127.0.0.1:9099` |
| SOCKS5 | `127.0.0.1:9089` |
| 管理 API | 不发布到宿主机，通过容器内 CLI 管理 |
| HTTPS 代理 | 默认关闭，见 [HTTPS 入口](#https-入口与证书) |

### 3. 导入来源或手动代理

**服务健康不代表已有可用代理。** 空池会拒绝业务请求，不会直连。

```sh
cp -n sources.example.toml sources.toml
# 填写 sources.toml 的真实 URL/字段映射，配置 providers.env，并设 enabled=true。
# providers.env 在进程启动时读取；修改后先重新创建服务：
docker compose up -d --force-recreate rproxy
docker compose exec -T rproxy rproxy-cli source import - < sources.toml
docker compose exec -T rproxy rproxy-cli source run vendor-a --wait --wait-timeout 60
docker compose exec -T rproxy rproxy-cli proxy list
```

也可跳过采集，手动添加真实上游：

```sh
# 以下是文档地址，必须替换为你自己的可用代理。
docker compose exec -T rproxy rproxy-cli proxy add http 203.0.113.10:8080 --wait
```

采集完成仅表示候选已提交，是否可用还取决于实际出口和协议验证。来源配置见下文；接入成功后按 [客户端连接](#客户端连接与选路) 使用。

## 原生构建与发布包

### 从源码运行（当前可用）

原生运行不需要 Docker/Podman。在仓库根目录用 Rust stable 构建两个程序，Windows 程序带 `.exe`：

```sh
cargo build --release --locked -p rproxy --bins
cp -n rproxy.example.toml rproxy.toml
cp -n providers.env.example providers.env
# 通过服务环境/秘密管理器设置 RPROXY_ADMIN_TOKEN，以及首次导入用的 RPROXY_PASSWORD。
# 原生程序不会自动读取 .env。
./target/release/rproxy --config rproxy.toml check-config
./target/release/rproxy --config rproxy.toml serve
```

另一个终端设置同一 `RPROXY_ADMIN_TOKEN` 后运行：

```sh
./target/release/rproxy-cli health
./target/release/rproxy-cli source import sources.toml
./target/release/rproxy-cli source run vendor-a --wait --wait-timeout 60
./target/release/rproxy-cli proxy list
```

先按[来源配置](#接入代理来源)准备 `sources.toml`；没有来源时也可手动添加代理。原生默认管理地址为 `http://127.0.0.1:9100`，可用 `--endpoint` 或 `RPROXY_ADMIN_URL` 修改。下文容器内的 `rproxy-cli ...` 命令均可改为直接调用原生 CLI。

`check-config` 只校验启动配置，不打开数据库，不验证供应商连通性，也不要求导入凭据已就绪；运行时错误仍需查看来源状态。当前不支持将管理员令牌或来源定义直接写进服务端 TOML；管理员令牌仍由环境变量提供，来源须通过 CLI 显式导入。

### 五平台发布目标（待发布）

v0.2.4 的发布工作流目标如下。待 [Releases](https://github.com/flyfoxCI/rproxy/releases) 实际出现该版本及 `SHA256SUMS` 后，再下载、校验并解压；不要把本表视为发布成功的证明。

| 平台 | 预期归档 |
|---|---|
| Linux x86_64（musl） | `rproxy-v0.2.4-x86_64-unknown-linux-musl.tar.gz` |
| Linux ARM64（musl） | `rproxy-v0.2.4-aarch64-unknown-linux-musl.tar.gz` |
| macOS Intel | `rproxy-v0.2.4-x86_64-apple-darwin.tar.gz` |
| macOS Apple Silicon | `rproxy-v0.2.4-aarch64-apple-darwin.tar.gz` |
| Windows x86_64（MSVC） | `rproxy-v0.2.4-x86_64-pc-windows-msvc.zip` |

Linux 可用 `sha256sum 归档文件`，macOS 用 `shasum -a 256 归档文件`，PowerShell 用 `Get-FileHash 归档文件 -Algorithm SHA256`，将结果与清单中该文件的摘要比较。macOS 最低目标为 11.0，二进制未签名/公证。

归档包含两个程序及配置示例。解压后在包目录复制示例，使用 `./rproxy --config rproxy.toml serve` 和 `./rproxy-cli ...`；配置、环境变量与来源导入要求同源码运行。

## 接入代理来源

来源定义通过 CLI 显式导入 SQLite，**不会自动加载 `sources.toml`**。支持新 `[[sources]]`、旧 `[[providers]]` TOML 和 JSON manifest。已有托管来源无需在重启时重导入。

### 供应商变量与通用映射

服务端 `rproxy.toml` 已包含：

```toml
[collector]
enabled = true
concurrency = 4      # 1..64，同时运行的来源任务数
poll_seconds = 3     # 1..300，计划刷新间隔；各来源另有自己的采集周期
env_file = "providers.env" # 相对本 TOML 的目录
```

用编辑器在 `providers.env` 填写供应商 URL/令牌；来源中的 `${SUPPLIER_API_URL}` 等引用只从这个文件解析，**不读取进程环境**。不要放管理员令牌、主密钥或入口密码。文件支持 UTF-8 `KEY=VALUE`、注释、引号，不执行 shell；修改后需重启原生进程或重新创建容器。

`sources.example.toml` 提供完整文本接口模板。JSON 接口按供应商当前返回结构修改映射，例如：

```toml
[sources.config.response]
format = "json"
items_path = "data.proxies"
ip_path = "ip"
port_path = "port"
success_path = "code"
success_value = 0
# 若数组元素直接包含完整端点，去掉 ip_path/port_path，改用 address_path="endpoint"。
```

支持 GET/POST、query params、headers、POST JSON 请求体；文本按空白/逗号/分号拆分；JSON 路径支持点号和数字数组下标。只接收数字 IPv4/IPv6 和合法端口，IPv6 使用 `[地址]:端口`，提取后规范化去重。

默认每来源 60 秒周期、每次尝试 10 秒预算、最多 3 次尝试；解压后响应最多 1 MiB、每次最多 4096 个地址、每批 256 个候选。超时覆盖请求和响应体读取；传输失败有界重试，映射错误不重试。禁重定向和系统代理，TLS 默认验证。

Xdaili、Shenlong 等均按当前账户接口填写 URL/映射，不再编写供应商类。Rola 仅在账户提供 IP 白名单提取接口时可配置接入；不猜测其 API，也不声称支持认证旋转网关或供应商粘性会话参数。完整配置与边界见 [采集手册](docs/internal-collector.md)。

### 在线操作

```sh
docker compose exec -T rproxy rproxy-cli source list
docker compose exec -T rproxy rproxy-cli source show vendor-a
docker compose exec -T rproxy rproxy-cli source resume vendor-a
docker compose exec -T rproxy rproxy-cli source run vendor-a --wait --wait-timeout 60
docker compose exec -T rproxy rproxy-cli source import - --replace < sources.toml
docker compose exec -T rproxy rproxy-cli source pause vendor-a
docker compose exec -T rproxy rproxy-cli source remove vendor-a --yes
```

来源变更在线生效，旧版本/旧执行代次的迟到结果会被拒绝。暂停或删除停止采集，默认保留已有代理和 stick；撤回候选是独立的 proxy 操作。普通查询/导出脱敏，`${ENV}` 引用保留。安全错误类别为 `env-missing`、`mapping-failed`、`fetch-failed`，不会回显供应商 URL、请求内容或凭据。

## 客户端连接与选路

代理用户名格式：

```text
v1|入口用户名|业务标识|会话标识|策略
```

例如 `v1|alice|shop|account-a|stick`：用入口用户 `alice` 的密码认证，在业务 `shop` 下为 `account-a` 保持出口。**不同网站账号使用不同会话**，业务必须在用户授权列表中。

各标识为 1–64 个 ASCII 字母数字或 `._-`，完整用户名最多 255 字节。普通用户名使用首个允许业务和默认会话/策略。放进代理 URL 时须编码用户名/密码；独立认证字段使用原值。

```sh
# 只指定用户名，让 curl 提示输入代理密码，避免真实密码进入命令参数。
curl --proxy http://127.0.0.1:9099 --proxy-user 'v1|alice|shop|account-a|stick' https://example.com/
curl --proxy socks5h://127.0.0.1:9089 --proxy-user 'v1|alice|shop|account-a|stick' https://example.com/
```

| 策略 | 每个新 TCP 连接的行为 |
|---|---|
| `stick` | 健康时沿用绑定；确认上游失效后记录日志并自动换绑 |
| `random` | 避免上次出口；只剩旧出口时明确失败，不静默重复 |
| `round-robin` | 在合格且未被同业务其他会话占用的出口间轮换；单候选可重复 |
| `shuffle` | 按会话随机不放回，一轮结束重洗，可行时避免跨轮重复 |

选路粒度不是单个 HTTP 请求：keep-alive 沿用当前出口，已建立的隧道不能中途迁移。认证固定 TTL 到期会重新验证，但不释放 stick，也不强行关闭既有隧道；显式撤销除外。部分浏览器不支持 SOCKS5 用户名密码，可改用 HTTP/HTTPS 入口。

## 在线管理

容器部署用 `docker compose exec rproxy rproxy-cli ...`；无交互命令可加 `-T`。原生直接执行 CLI。`--json` 供脚本使用，失败返回非零退出码；`--token-env` 接收变量名，不接收令牌值。完整命令见 [CLI 手册](docs/rproxy-cli.md)。

### 用户、密码与业务授权

```sh
# 隐藏输入密码，不使用明文 --password 参数。
docker compose exec rproxy rproxy-cli user add bob --business shop --business news
docker compose exec rproxy rproxy-cli user passwd alice
docker compose exec -T rproxy rproxy-cli user grant bob --business shop,news
docker compose exec -T rproxy rproxy-cli user list
docker compose exec -T rproxy rproxy-cli user show bob
docker compose exec -T rproxy rproxy-cli user disable bob --dry-run
docker compose exec -T rproxy rproxy-cli user disable bob --yes
docker compose exec -T rproxy rproxy-cli user enable bob
# 明确撤销已有连接：
docker compose exec -T rproxy rproxy-cli user disable bob --disconnect --yes
```

自动化使用 `--password-stdin` 或 `--password-env 变量名`。`grant` 替换完整业务列表；空列表表示无业务访问权。密码只保存 Argon2id 哈希。

改密不释放 stick；禁用/删除默认拒绝新认证，让旧连接排空，`--disconnect` 才强制断开。用户 ID 不可变，删除重建同名用户不会接管旧绑定。**删除最后一个用户不会开放匿名，也不会锁死独立令牌保护的管理接口。** 匿名必须显式设置 `[auth] mode="anonymous"`。

### 手动代理与会话

```sh
# 文档 IP 必须替换成真实上游；IPv6 使用 [地址]:端口。
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

添加代理是幂等提交候选，**不是免检准入**。`--wait` 对拒绝/超时返回非零。同一协议/地址可关联多个来源。`probe.enabled=false` 只关闭定期检测，新候选和重启遗留候选仍需验证。

管理员释放会话不需要用户密码；旧连接排空前仍保护出口，强制断开需显式加 `--disconnect`。删除用户不静默释放其全部 stick。

## 配置、数据与安全

| 文件/数据 | 用途与变更方式 |
|---|---|
| `rproxy.toml` | 监听、TLS、认证模式、限额、采集调度；修改后重启原生进程/重新创建容器 |
| `providers.env` | 供应商变量的私有映射，只读挂载；修改后重启/重新创建 |
| `.env` | Compose 注入管理员令牌、首次导入密码及端口/路径；原生程序不读取 |
| SQLite 与 `.key` | 可变管理数据的权威来源；用户、授权、来源、手动代理和绑定通过 CLI 在线变更 |
| 证书目录 | HTTPS 入口或自定义 CA；需显式只读挂载 |

Compose 把启动文件挂载到 `/etc/rproxy/`，状态卷挂载到 `/var/lib/rproxy`，内部 HTTP/SOCKS5/管理监听固定为 `0.0.0.0:9099/9089/9100`。外部端口/绑定 IP 在 `.env` 调整；宿主路径可用 `RPROXY_CONFIG`、`RPROXY_PROVIDERS_ENV`、`RPROXY_CERTS_DIR` 指定。`up -d --force-recreate rproxy` 会重新注入环境、更新挂载 inode；单独 `restart` 不会重新注入环境。

容器使用 `10001:10001`、只读根文件系统和最小权限。确保配置/证书可读、状态卷可写，不使用 chmod777；缺失挂载文件会拒绝启动，不自动创建目录。部分 Compose provider 不转发 SELinux `Z`，enforcing 环境应为专用文件设置容器标签，不关闭 SELinux 或重标系统目录。

**一个数据库只允许一个进程持有。** 使用有可靠文件锁的本地文件系统，不使用 NFS/SMB，不通过共享数据库扩容副本。Linux 锁定数据库 inode；macOS/Windows 使用旁路 `.lock`。运行中不要删除/替换数据库、锁或密钥文件。

### 来源加密与导出

来源配置整体加密保存。默认在数据库旁生成私有 `.sqlite3.key` 文件（Unix 0600），密钥不在 SQLite；也可通过受控服务环境注入标准 base64 编码的 32 字节 `RPROXY_SOURCE_KEY`。错误/丢失密钥会失败关闭，不会重置数据。

```sh
docker compose exec -T rproxy rproxy-cli source export > redacted-sources.json
```

普通导出脱敏，不是完整凭据备份。完整秘密导出必须指定 `--include-secrets --output 私有文件 --yes`，路径属于 **CLI 运行所在环境**，不写 stdout、不覆盖已有文件；容器只读根文件系统下须选择显式准备的可写私有路径。备份数据库时还必须保存其加密密钥。

### HTTPS 入口与证书

准备 `certs/fullchain.pem` / `certs/private-key.pem`，在 TOML **顶层**启用 `https_bind="0.0.0.0:9443"`，并设置：

```toml
[tls]
certificate = "certs/fullchain.pem"
private_key = "certs/private-key.pem"
```

```sh
docker compose -f compose.yaml -f compose.https.yaml up -d --build
```

后续始终带这两个 `-f`。只发布端口不会启用 TLS；证书错误拒绝启动，不降级。自签名入口需客户端显式信任（curl `--proxy-cacert`）；目标 HTTPS 证书仍由客户端验证。普通 HTTP Basic / SOCKS5 密码不加密，跨不可信网络应使用 HTTPS 入口或安全通道，管理 API 不要裸露公网。

供应商 `ca_certificate` 与 `[probe] ca_certificate` 分开配置，相对路径基于服务端 TOML，自定义 CA 需显式只读挂载。供应商 `insecure=true` 是显式证书例外，不影响客户端 TLS 隧道；优先配置受信任 CA。

## 升级、备份与停止

先停止旧部署并备份完整状态目录和宿主配置，始终保留相同 Compose 项目名和卷。下面示例使用新的空备份目录，已有备份不要覆盖：

```sh
mkdir backup-v024
docker compose stop rproxy
docker compose cp rproxy:/var/lib/rproxy ./backup-v024/
# 另行安全备份 rproxy.toml、.env、providers.env、证书，以及显式提供的主密钥。
```

不要运行中只复制 SQLite 主文件；保存整个状态目录，包括存在的 WAL 和 `.key`。若旧部署包含外部 collector，也先停止它，避免备份过程中继续写入。

| 升级来源 | 处理方式 |
|---|---|
| 0.2.3 源码/候选构建 | 保留 schema2、用户/来源 ID、密钥和 stick，无需重新导入托管来源；添加 `[collector]`，迁移供应商变量，移除 worker 配置 |
| 0.2.2 | 保留停止后的完整备份；升级迁移旧用户归属并保留绑定，TOML 用户只导入一次；旧 provider TOML 用 CLI 显式导入 |
| Redis 版 0.2.1 及以前 | 需要维护窗口、重新导入候选；不自动迁移 Redis stick/占用，不承诺保留旧出口 |

旧 `scripts/providers.env` 的变量需安全迁到新 `providers.env`；删除 `.env` 的 worker token 和旧 worker 启动引用。`admin.worker_token_env` 只兼容解析，不再授权；旧 `/v1/collector/*` HTTP 接口已移除，重启旧 worker 无法恢复采集。

确认当前项目后执行 `docker compose up -d --build --remove-orphans`，移除**该项目**的旧 collector 容器，不删除卷。随后检查 `health`、`source run --wait` 和 `proxy list`。

```sh
docker compose down  # 停止并保留卷/宿主文件，不要加 -v
docker compose up -d --build
```

不要更换项目名而意外使用空卷。回退旧 schema 版本需恢复升级前备份。损坏、未知 schema 或无法解密时拒绝启动，不清空重建；用户的 `data/`、私有配置和旧虚拟环境不会被自动删除。

## Python 功能迁移对照

**生产采集能力已迁入 Rust；不是保留 Python 命令行和测试依赖的逐字移植。**

| 原 Python 能力/工具 | v0.2.4 对应实现 |
|---|---|
| 通用 GET/POST、参数/请求头/JSON 请求体 | 内置 Rust collector，沿用来源配置字段 |
| 文本/JSON 映射、成功标记、数组路径、IPv4/IPv6、去重 | 内置映射与校验；无供应商专用类 |
| 来源周期、并发、重试、超时、TLS/CA、响应和批次上限 | Tokio 调度与有界 HTTP 请求；支持取消过期任务和关闭时清理 |
| `${ENV}` 供应商引用 | 保留引用语法，但只读取专用 `providers.env`，不再读取通用进程环境 |
| 独立 worker、令牌、plan/results HTTP 回传 | 删除；同进程 typed Store 提交，保留来源版本/执行代次校验 |
| worker `--once` / `--check-config` | 按来源用 `rproxy-cli source run --wait`；启动配置用 `rproxy check-config`，不保留独立 worker CLI 或全来源 `--once` 命令 |
| 打包、归档校验、SHA256SUMS | Rust `xtask package / verify / checksums`，继续覆盖五平台和两个程序 |
| Compose 生命周期、HTTP/SOCKS fixture | Rust `xtask compose-smoke / fixture`，只创建和清理自己的资源 |
| Python 单元测试与协议烟测 | Rust workspace 测试和协议/TLS 集成测试；未原样保留 requests/可选 Playwright 驱动的客户端矩阵，不据此宣称重新验证过真实浏览器 |

仓库不再跟踪 `.py` 或 requirements 文件，CI/Release 不安装 Python。旧私有 Python 环境不参与运行，也不自动删除。

## 开发、测试与发布

从源码构建两个生产程序：

```sh
cargo build --release --locked -p rproxy --bins
cargo fmt --all --check
cargo clippy --locked --workspace --all-targets -- -D warnings
cargo test --locked --workspace --all-targets
cargo run --locked -p xtask -- compose-smoke --engine podman # 或 docker
```

Windows GNU toolchain 需 MinGW 在 PATH；低内存环境给 Cargo 加 `-j1`。旧 `healthcheck/add-proxy/verify-pool/release-session` 命令保留，新的管理操作优先使用独立 CLI。

Release 工作流先验证完整 workspace 和真实 Compose，再在五个原生 runner 编译。打包前检查两个程序版本、真实 SQLite 启动与 CLI 创建/读取用户；两个 macOS job 额外执行 SQLite 锁回归。最后验证完整五归档集合、文件类型/路径/权限和 SHA256SUMS，全部通过才把 draft 发布。归档验证不执行下载或归档中的程序，已公开版本不覆盖。

详见 [开发/发布工具](https://github.com/flyfoxCI/rproxy/blob/main/docs/xtask.md)、[v0.2.4 变更](https://github.com/flyfoxCI/rproxy/blob/main/docs/releases/v0.2.4.md) 和 [Releases](https://github.com/flyfoxCI/rproxy/releases)。历史 `v0.2.3` 标签保持不变且不发布。

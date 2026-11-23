# rproxy

Rust 实现的 HTTP / HTTPS / SOCKS5 代理池，适合多个业务、多个登录账号需要独立且稳定出口的场景。

**当前源码版本为 0.2.4：采集、验证、认证、选路和内嵌 SQLite 状态管理统一在一个 `rproxy` 进程中运行。**

- **`rproxy`**：代理入口、配置化采集、候选验证、实际出口检测与会话绑定。
- **`rproxy-cli`**：在线管理用户、密码、业务授权、来源、手动代理和会话。
- **`xtask`**：仅供开发和发布使用，不进入原生发布包或生产镜像。

交付以 **`rproxy` / `rproxy-cli` 两个原生二进制**为主，同时保留可选的**单个 Docker 容器**部署。无需 Docker Compose 或 Podman；仓库不再提供编排文件、独立 fixture 镜像或编排烟测命令。

## 当前实现与发布状态

- 可以从源码构建原生程序，也可以用仓库 Dockerfile 构建单个生产镜像。
- **单 TOML 配置尚未实现**。当前仍使用服务端 `rproxy.toml`、可选供应商变量文件 `providers.env`，以及通过 CLI 显式导入的来源 manifest。不要把[单文件配置设计](https://github.com/flyfoxCI/rproxy/blob/main/docs/superpowers/specs/2026-11-23-native-single-config-v024-design.md)当成已交付接口。
- `rproxy-cli` 不读取服务端 TOML，不支持 `--config`。管理地址来自 `--endpoint` / `RPROXY_ADMIN_URL`；令牌来自 `--token-env` 指定的环境变量，默认 `RPROXY_ADMIN_TOKEN`。
- **v0.2.4 尚未公开发布 Release**。五平台归档是发布目标，是否可下载以 [Releases](https://github.com/flyfoxCI/rproxy/releases) 为准。

## 目录

- [功能与边界](#功能与边界)
- [原生构建与运行](#原生构建与运行)
- [可选：单个 Docker 容器](#可选单个-docker-容器)
- [接入代理来源](#接入代理来源)
- [客户端连接与选路](#客户端连接与选路)
- [在线管理](#在线管理)
- [配置、密钥与 TLS](#配置密钥与-tls)
- [升级、备份与停止](#升级备份与停止)
- [开发、测试与发布](#开发测试与发布)

## 功能与边界

| 功能 | 支持情况 |
|---|---|
| 客户端入口 | HTTP 正向代理、HTTP CONNECT、TLS 加密的 HTTPS 代理入口、SOCKS5 TCP CONNECT |
| 上游代理 | HTTP、SOCKS5，可与不同入口交叉组合 |
| 目标地址 | IPv4、域名、IPv6 |
| 入口认证 | 用户名密码，或显式启用的匿名模式 |
| 选路策略 | `stick`、`random`、`round-robin`、`shuffle` |
| 来源采集 | GET/POST、参数/请求头/JSON 请求体、文本/JSON 响应映射 |
| 持久化 | SQLite 保存用户、来源、授权、候选和会话绑定 |

出口隔离按**实际出口 IP**判断，不只比较供应商返回的 IP:port。同业务不同会话不能共用实际出口，不同业务可以共享。stick 跨连接、认证续期和重启保留；旧连接排空前，旧出口不会提前交给同业务其他会话。

不解密目标 HTTPS，不做 MITM。不因空池直连、不突破出口排他、不自动降级认证，也不重放已经发送的业务请求。

当前不支持 SOCKS5 UDP/BIND、HTTPS 上游、上游用户名密码、供应商认证旋转网关/粘性会话参数、不可控轮换出口、网站 Cookie/登录管理或多机调度。**HTTPS 代理入口不等于 HTTPS 上游。**

## 原生构建与运行

在仓库根目录使用 Rust stable 构建。下面是 sh / Git Bash 命令；Windows 程序带 `.exe`，PowerShell 复制文件可使用 `Copy-Item`。已有配置不要覆盖。

```sh
cargo build --release --locked -p rproxy --bins
cp -n rproxy.example.toml rproxy.toml
cp -n providers.env.example providers.env
```

通过服务环境或秘密管理器设置：

- `RPROXY_ADMIN_TOKEN`：独立随机管理员令牌，16–1024 个非空白可打印 ASCII 字符。可用密码管理器或 `openssl rand -hex 32` 生成。
- `RPROXY_PASSWORD`：示例用户 `alice` 的首次导入密码。已有托管用户不会被该值在重启时覆写。

不要把真实秘密写进命令参数、Git 或工单。原生程序不会自动读取 `.env`。若要以空的 required-auth 池启动，删除 TOML 中的 `[[auth.users]]`，启动后通过 CLI 新增用户。

```sh
./target/release/rproxy --config rproxy.toml check-config
./target/release/rproxy --config rproxy.toml serve
```

另一个终端设置同一管理令牌后执行：

```sh
./target/release/rproxy-cli health
./target/release/rproxy-cli user list
```

原生默认 HTTP `127.0.0.1:9099`、SOCKS5 `127.0.0.1:9089`、管理 API `http://127.0.0.1:9100`。`check-config` 只校验启动配置，不打开数据库，不检查供应商连通性，也不要求首次导入凭据已就绪。

**健康不代表已有可用代理。** 按下文导入来源或手动代理；空池会拒绝请求，不会直连。

## 可选：单个 Docker 容器

只需 Docker Engine/Desktop。镜像只包含上述两个生产程序，不包含开发工具或额外服务。

### 首次启动

先准备好 `rproxy.toml` 和 `providers.env`，并在当前 shell/秘密管理器中提供上述管理员令牌与首次导入密码。示例密码不会直接出现在命令参数中，`--env NAME` 只传递已设置的变量。

```sh
docker build --tag rproxy:0.2.4 .
docker volume create rproxy-data
docker run -d --name rproxy --restart unless-stopped \
  --read-only --cap-drop ALL --security-opt no-new-privileges \
  --env RPROXY_ADMIN_TOKEN --env RPROXY_PASSWORD \
  --publish 127.0.0.1:9099:9099 \
  --publish 127.0.0.1:9089:9089 \
  --mount "type=bind,src=$(pwd)/rproxy.toml,dst=/etc/rproxy/rproxy.toml,readonly" \
  --mount "type=bind,src=$(pwd)/providers.env,dst=/etc/rproxy/providers.env,readonly" \
  --mount type=volume,src=rproxy-data,dst=/var/lib/rproxy \
  rproxy:0.2.4
```

这是首次部署示例。**已有部署必须改用原状态卷名，不要误建空卷。** Docker `--mount` 遇到缺失宿主文件会报错，不自动创建目录。Windows 建议 PowerShell 使用解析后的绝对宿主路径；Git Bash 调用 Docker 时注意 MSYS 路径转换。

镜像默认使用 UID/GID `10001:10001`，数据库固定在 `/var/lib/rproxy/rproxy.sqlite3`，HTTP/SOCKS5 内部监听 `0.0.0.0:9099/9089`。管理 API 不发布到宿主机，直接在同一容器里执行 CLI：

```sh
docker logs --tail 100 rproxy
docker exec rproxy rproxy-cli health
docker exec rproxy rproxy-cli user list
docker exec -it rproxy rproxy-cli user passwd alice
```

正文中的原生 `rproxy-cli ...` 命令，容器内改为 `docker exec rproxy rproxy-cli ...`；需要 stdin 文件输入时用 `docker exec -i`，隐藏交互密码提示用 `docker exec -it`。

### 配置更新与重建

配置和供应商文件只读挂载，状态卷持久保存。修改环境变量、监听、证书或被编辑器原子替换的挂载文件后，停止并移除**你自己的旧容器**，用原参数和同一个状态卷重新创建；不要删除卷。在线用户/来源变更不需重建。

不要求 chmod777。确保配置/证书对服务账户可读、状态目录可写。SELinux enforcing 主机需为专用挂载文件设置正确的容器标签，不关闭 SELinux 或重标系统目录。

## 接入代理来源

### 准备并导入来源

服务端示例已包含：

```toml
[collector]
enabled = true
concurrency = 4       # 1..64，同时运行的来源任务数
poll_seconds = 3      # 1..300，来源计划刷新间隔
env_file = "providers.env" # 相对服务端 TOML 的目录
```

`providers.env` 只存供应商的 UTF-8 `KEY=VALUE`。来源 `${SUPPLIER_API_URL}`、`${SUPPLIER_API_TOKEN}` 等引用只从该文件解析，**不读取通用进程环境**。不要放管理员令牌、入口密码或主密钥。支持注释、引号和 CRLF，不执行 shell。变量在进程启动时读取，修改后重启原生进程或重新创建容器。

```sh
cp -n sources.example.toml sources.toml
# 编辑真实 URL/映射，配置供应商变量，并将需要的来源设 enabled=true。
rproxy-cli source import sources.toml
rproxy-cli source list
rproxy-cli source run vendor-a --wait --wait-timeout 60
rproxy-cli proxy list
```

未加入 PATH 时用 `./target/release/rproxy-cli`。容器无需复制来源文件进去：

```sh
docker exec -i rproxy rproxy-cli source import - < sources.toml
```

来源文件**不自动加载**，SQLite 才是运行时权威。支持 `[[sources]]`、旧 `[[providers]]` TOML 和 JSON manifest。修改、暂停和执行来源在线生效；重启不重新导入或覆盖 CLI 修改。

### 通用映射与来源管理

GET/POST、query params、headers、POST JSON 请求体均由来源配置表达。文本按空白、逗号、分号拆分。JSON 可配置：

```toml
[sources.config.response]
format = "json"
items_path = "data.proxies"
ip_path = "ip"
port_path = "port"
success_path = "code"
success_value = 0
# 或去掉 ip_path/port_path，改用 address_path="endpoint"。
```

路径支持点号和数字数组下标。只接收数字 IPv4/IPv6 和合法端口，IPv6 使用 `[地址]:端口`，规范化去重。默认每来源60秒周期、每次尝试10秒预算、最多3次尝试；解压后响应最多1MiB、每次最多4096个地址、每批256个候选。超时覆盖请求和响应体读取，传输失败有界重试，映射错误不重试；禁止重定向和系统代理，TLS 默认验证。

```sh
rproxy-cli source show vendor-a
rproxy-cli source pause vendor-a
rproxy-cli source resume vendor-a
rproxy-cli source import sources.toml --replace
rproxy-cli source remove vendor-a --yes
```

旧版本/旧执行代次的迟到结果会被拒绝。暂停或删除停止采集，默认保留已有代理和 stick；撤回候选是独立 proxy 操作。采集完成只表示候选已提交，**不等于验证通过**。

Xdaili、Shenlong 等按当前账户接口填写配置；Rola 仅在账户提供 IP 白名单提取接口时配置，不编造 API，不声称支持认证轮换网关。详见[采集手册](docs/internal-collector.md)。

## 客户端连接与选路

用户名格式为 `v1|入口用户名|业务标识|会话标识|策略`，例如 `v1|alice|shop|account-a|stick`。密码是入口用户的密码，业务必须已获授权；不同网站账号使用不同会话。

标识为1–64个 ASCII 字母数字或 `._-`，完整用户名最多255字节；普通用户名使用首个允许业务和默认会话/策略。放入 URL 时编码用户名/密码，独立认证字段使用原值。

```sh
# 只指定用户名，让 curl 提示输入代理密码。
curl --proxy http://127.0.0.1:9099 --proxy-user 'v1|alice|shop|account-a|stick' https://example.com/
curl --proxy socks5h://127.0.0.1:9089 --proxy-user 'v1|alice|shop|account-a|stick' https://example.com/
```

| 策略 | 新 TCP 连接的行为 |
|---|---|
| `stick` | 健康时沿用绑定；确认上游失效后记录日志并自动换绑 |
| `random` | 避免上次出口；只剩旧出口时明确失败，不静默重复 |
| `round-robin` | 在合格且未被同业务其他会话占用的出口间轮换；单候选可重复 |
| `shuffle` | 按会话随机不放回，一轮结束重洗，可行时避免跨轮重复 |

keep-alive 沿用当前出口，已建立的隧道不能中途迁移。认证固定 TTL 到期重新验证，但不释放 stick，也不强关既有隧道，显式撤销除外。部分浏览器不支持 SOCKS5 用户名密码，可改用 HTTP/HTTPS 入口。

## 在线管理

`--json` 供脚本使用，失败返回非零退出码。完整命令见 [CLI 手册](docs/rproxy-cli.md)。

```sh
# 隐藏输入密码；自动化用 --password-stdin 或 --password-env 变量名。
rproxy-cli user add bob --business shop --business news
rproxy-cli user passwd alice
rproxy-cli user grant bob --business shop,news
rproxy-cli user list
rproxy-cli user disable bob --dry-run
rproxy-cli user disable bob --yes
rproxy-cli user enable bob
rproxy-cli user disable bob --disconnect --yes
```

`grant` 替换完整业务列表，空列表表示无业务访问权。密码只存 Argon2id 哈希。改密不释放 stick；禁用/删除默认拒绝新认证，旧连接排空，`--disconnect` 才强制断开。不可变用户 ID 确保删除重建同名用户不能接管旧绑定。

**删除最后一个用户不会开放匿名，也不锁死独立令牌保护的管理接口。** 匿名必须显式设置 `[auth] mode="anonymous"`。

```sh
# 文档 IP 必须替换成真实上游；IPv6 使用 [地址]:端口。
rproxy-cli proxy add http 203.0.113.10:8080 --wait
rproxy-cli proxy add socks5 203.0.113.20:1080 --source manual
rproxy-cli proxy import http addresses.txt --wait
rproxy-cli proxy list --protocol http
rproxy-cli proxy show 'http:203.0.113.10:8080'
rproxy-cli proxy verify
rproxy-cli proxy disable 'http:203.0.113.10:8080' --dry-run
rproxy-cli proxy remove 'http:203.0.113.10:8080' --yes
rproxy-cli session list --business shop
rproxy-cli session release shop account-a --dry-run
rproxy-cli session release shop account-a --yes
```

添加是幂等提交候选，不是免检准入；`--wait` 对拒绝/超时返回非零。同一协议/地址可关联多个来源。`probe.enabled=false` 只关定期检测，新候选和重启遗留候选仍需验证。释放会话不需用户密码，但旧连接排空前仍保护出口，强制断开需显式选择。

## 配置、密钥与 TLS

| 文件/数据 | 当前用途 |
|---|---|
| `rproxy.toml` | 启动配置、初始用户；修改启动参数后重启 |
| `providers.env` | 供应商专用变量映射，可选；示例默认引用它 |
| 来源 manifest | 显式导入，不是重启时的运行权威 |
| SQLite 与 `.key` | 在线管理数据、绑定和来源加密密钥 |
| TLS/CA 文件 | 可选，路径写在 TOML/来源配置中 |

一个数据库只允许一个进程持有。使用支持可靠文件锁的本地文件系统，不使用 NFS/SMB，不共享数据库扩容副本。Linux 锁数据库 inode，macOS/Windows 使用 `.lock`；运行中不要删除/替换数据库、锁或密钥。

来源配置整体加密保存，默认在数据库旁生成 `.sqlite3.key`（Unix0600），也可通过受控环境注入标准 base64 编码的32字节 `RPROXY_SOURCE_KEY`。密钥不在 SQLite；错误或丢失时拒绝启动，不清空重建。普通导出脱敏：

```sh
rproxy-cli source export > redacted-sources.json
```

完整秘密导出必须 `--include-secrets --output 私有文件 --yes`，不写 stdout，不覆盖已有文件。输出路径属于 CLI 所在环境；Docker 只读根文件系统下须准备可写私有路径。普通导出不是完整凭据备份，数据库备份还必须包含其独立密钥。

HTTPS 入口：在 TOML **顶层**启用 `https_bind="0.0.0.0:9443"`，并配置：

```toml
[tls]
certificate = "certs/fullchain.pem"
private_key = "certs/private-key.pem"
```

原生路径相对服务端 TOML。Docker 在原 run 参数上增加只读证书目录挂载到 `/etc/rproxy/certs`，并发布 `127.0.0.1:9443:9443` 后重新创建同一个服务容器；只发布端口不会启用 TLS。证书错误拒绝启动，不降级；自签名入口需客户端显式信任（curl `--proxy-cacert`），目标 HTTPS 仍由客户端验证。

普通 HTTP Basic / SOCKS5 密码不加密，跨不可信网络用 HTTPS 入口或安全通道，管理 API 不要裸露公网。供应商 CA 与 probe CA 分开配置；显式 `insecure=true` 仅影响对应供应商/探测请求，不影响客户端 TLS 隧道。

## 升级、备份与停止

先停止服务并备份完整状态目录、密钥、宿主配置和证书；不要运行中只复制 SQLite 主文件。Docker 可在新的空备份目录中执行：

```sh
mkdir backup-v024
docker stop rproxy
docker cp rproxy:/var/lib/rproxy ./backup-v024/
```

升级前停止旧部署的所有服务。旧编排部署的卷名可能带项目名前缀；迁移到 `docker run` 时必须使用那个**实际卷名**，不删除卷、不悄悄换成新的 `rproxy-data`。

| 升级来源 | 行为 |
|---|---|
| 0.2.3 源码/候选构建 | 保留 schema2、用户/来源 ID、密钥和 stick；添加 `[collector]`，迁移供应商变量，无需重导入托管来源 |
| 0.2.2 | 迁移旧用户归属并保留绑定；TOML 用户只导入一次，旧来源 manifest 显式导入 |
| Redis 版0.2.1及以前 | 维护窗口后重新导入候选；不自动迁移 Redis stick/占用，不承诺保留旧出口 |

旧版本的接口变化和迁移细节见 [v0.2.4 变更](https://github.com/flyfoxCI/rproxy/blob/main/docs/releases/v0.2.4.md)。回退旧 schema 版本需恢复升级前备份。

用户真实 `data/` 和私有配置不自动删除。停止/重建只针对自己的进程或容器，保留数据卷。

## 开发、测试与发布

```sh
cargo fmt --all --check
cargo clippy --locked --workspace --all-targets -- -D warnings
cargo test --locked --workspace --all-targets
```

Windows GNU toolchain 需 MinGW 在 PATH；低内存给 Cargo 加 `-j1`。Rust 测试使用本地 fixture，不要求容器引擎。可选 Docker 镜像检查使用直接 `docker build` / `docker run`，不安装编排工具。

五平台归档和 SHA256SUMS 仍是发布目标，未发布前使用源码构建：

| 平台 | 预期归档 |
|---|---|
| Linux x86_64 musl | `rproxy-v0.2.4-x86_64-unknown-linux-musl.tar.gz` |
| Linux ARM64 musl | `rproxy-v0.2.4-aarch64-unknown-linux-musl.tar.gz` |
| macOS Intel | `rproxy-v0.2.4-x86_64-apple-darwin.tar.gz` |
| macOS Apple Silicon | `rproxy-v0.2.4-aarch64-apple-darwin.tar.gz` |
| Windows x86_64 MSVC | `rproxy-v0.2.4-x86_64-pc-windows-msvc.zip` |

下载后用 Linux `sha256sum`、macOS `shasum -a 256` 或 PowerShell `Get-FileHash -Algorithm SHA256` 与清单逐项比较。macOS 最低目标11.0，未签名/公证。

发布前检查两个本地构建程序的版本、真实 SQLite 启动和 CLI 创建/读取用户；两个 macOS job 执行文件锁回归。再验证完整五归档集合、成员路径/类型/权限和 SHA256SUMS，全部通过才公开 draft。归档检查不执行下载或归档中的程序，公开版本不覆盖，`v0.2.3` 标签不移动且不发布。

详见 [CLI 手册](docs/rproxy-cli.md)、[采集手册](docs/internal-collector.md)、[开发发布工具](https://github.com/flyfoxCI/rproxy/blob/main/docs/xtask.md) 与 [v0.2.4 变更](https://github.com/flyfoxCI/rproxy/blob/main/docs/releases/v0.2.4.md)。

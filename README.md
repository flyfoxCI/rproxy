# rproxy

面向多个业务、登录账号的 HTTP / HTTPS / SOCKS5 代理池。**0.2.3 使用内嵌 SQLite，并提供独立的 `rproxy-cli` 在线管理用户、代理来源、手动代理和会话**，不依赖 Redis 或 Web 管理后台。

- `rproxy`：协议入口、认证、候选准入、实际出口检测、四种选路策略和持久绑定。
- `rproxy-cli`：通过认证管理接口操作正在运行的 rproxy，不直接读写 SQLite，不要求为用户/来源变更重启服务。
- Python collector：按服务端下发的版本化来源配置执行 GET/POST 和文本/JSON 提取，只有采集权限。

原生发布包和 Rust 容器均包含两个 Rust 二进制。完整参数见 [CLI 手册](docs/rproxy-cli.md)，采集器说明见 [采集器手册](docs/collector-management.md)。

## 能力与运行边界

支持 HTTP 正向代理、HTTP CONNECT、TLS 加密的 HTTPS 代理入口、SOCKS5 TCP CONNECT，以及 HTTP / SOCKS5 上游交叉组合。支持 IPv4、域名和 IPv6 目标；不解密目标 HTTPS，不做 MITM。

同业务不同会话不能共用**实际出口 IP**，不同业务可以共享。`stick` 跨连接、认证续期和重启保留绑定；IPv4-mapped IPv6 出口按 IPv4 归一化。旧连接未排空时，旧出口不会被提前交给同业务其他会话。

| 策略 | 新 TCP 连接的行为 |
|---|---|
| `stick` | 健康时沿用绑定，确认上游失效后记录日志并换绑 |
| `random` | 避免上次出口；只剩旧出口时明确失败，不静默重复 |
| `round-robin` | 在合格、未被其他会话占用的出口间顺序轮换；单候选可重复 |
| `shuffle` | 按会话随机不放回，一轮结束重洗，可行时避免跨轮重复 |

选择粒度不是单个 HTTP 请求：keep-alive 沿用当前出口，已建立的隧道不能中途迁移。不因无可用代理而直连、共享被占出口或自动降级认证。

**一个 SQLite 数据库只允许一个 Rust 进程持有。** 使用支持可靠文件锁的本地文件系统，不使用 NFS/SMB，不扩容共享数据库的副本，也不承诺跨机器全局出口排他。运行中不要删除/替换数据库、锁或密钥文件。

当前不支持 SOCKS5 UDP/BIND、HTTPS 上游、上游用户名密码、供应商旋转网关/粘性会话参数、不可控轮换出口、网站 Cookie/登录管理或多机调度。HTTPS 代理入口不等于 HTTPS 上游。

## Docker / Podman Compose 一键启动

### 1. 准备环境和宿主机配置

需要运行中的 Docker Engine/Desktop + Compose v2 或更新版本，或 Podman + 近期 Compose provider。Windows/macOS 的 Podman 先启动 machine：

```sh
podman machine start # 已启动时不用重复
podman compose version
```

首次在仓库根目录复制配置，**已有文件不要覆盖**：

```sh
cp rproxy.example.toml rproxy.toml
cp scripts/proxy.example.toml scripts/proxy.toml
cp scripts/providers.env.example scripts/providers.env
cp .env.example .env
```

PowerShell 使用 `Copy-Item`。用宿主机编辑器填写 `.env`，首次启动设置三个**不同**的随机值：

```dotenv
RPROXY_PASSWORD=首次导入alice用户的入口密码
RPROXY_ADMIN_TOKEN=随机的管理员令牌
RPROXY_WORKER_TOKEN=另一个独立的采集器令牌
RPROXY_BIND_ADDRESS=127.0.0.1
RPROXY_HTTP_PORT=9099
RPROXY_SOCKS5_PORT=9089
RPROXY_HTTPS_PORT=9443
```

令牌需 16–1024 个非空白可打印 ASCII 字符；建议使用 32 字节随机数生成的 URL-safe 字符串。如有 Python，可分别执行 `python -c "import secrets; print(secrets.token_urlsafe(32))"`。不要把令牌值写进命令参数、Git 或工单。

`RPROXY_PASSWORD` 只用于示例用户的一次性导入。后续使用 CLI 改密，不会被旧 `.env`/TOML 在重启时覆盖。也可以删除 TOML 中的 `[[auth.users]]`，以空的 required-auth 池启动后用 CLI 新增第一个用户。

### 2. 启动全部服务

```sh
docker compose config --quiet
docker compose up -d --build
docker compose ps
docker compose logs --tail=100 rproxy collector
```

使用 Podman 时替换成 `podman compose`，其余参数相同。首次构建需要联网；宿主机不需要 Rust、Python 或 Redis。一个 Dockerfile 有 `rproxy` / `collector` 两个 target；CLI 随 Rust 镜像提供，不是第三个常驻服务。

```sh
docker compose exec -T rproxy rproxy-cli --json health
docker compose exec -T rproxy rproxy-cli user list
```

默认仅映射 HTTP `127.0.0.1:9099` 和 SOCKS5 `127.0.0.1:9089`。管理端口 9100 只在容器网络内使用，不发布到宿主机。collector 使用受限 worker token，不能修改用户或代理池管理数据。

**健康不表示已有可用代理。** 新库没有来源时采集器空闲，空池的业务请求会失败，不会绕过代理直连。接下来导入来源或手动添加候选。

## rproxy-cli 在线管理

以下使用容器内 CLI；原生安装可直接执行 `rproxy-cli ...`。本机原生默认管理地址 `http://127.0.0.1:9100`，也可通过 `--endpoint` 或 `RPROXY_ADMIN_URL` 指定。`--token-env` 接受变量名，不是令牌值。普通输出便于阅读，`--json` 供脚本使用；失败返回非零退出码。

### 用户、密码和业务授权

```sh
# 交互输入密码，终端不回显；不要使用 --password 明文。
docker compose exec rproxy rproxy-cli user add bob --business shop --business news
docker compose exec rproxy rproxy-cli user passwd alice
docker compose exec -T rproxy rproxy-cli user grant bob --business shop,news
docker compose exec -T rproxy rproxy-cli user list
docker compose exec -T rproxy rproxy-cli user show bob

docker compose exec -T rproxy rproxy-cli user disable bob --dry-run
docker compose exec -T rproxy rproxy-cli user disable bob --yes
docker compose exec -T rproxy rproxy-cli user enable bob
# 明确要求关闭该用户已有隧道：
docker compose exec -T rproxy rproxy-cli user disable bob --disconnect --yes
```

自动化可使用 `--password-stdin` 或 `--password-env 变量名`。`grant` 替换完整业务列表；清空授权意味着没有业务访问权。密码仅存 Argon2id 哈希，不通过列表、错误或日志返回。

- 改密立即影响后续认证，但不释放或更换 stick。
- 禁用/删除拒绝新认证；已有隧道默认排空，`--disconnect` 明确强制断开。
- 用户有不可变内部 ID；删除后重建同名用户不会接管旧绑定。
- **删除最后一个用户不会变成匿名代理。** `[auth] mode="required"` 与用户数量独立；只有显式改成 `anonymous` 才允许匿名。
- 管理员令牌独立于入口用户，删除全部入口用户不会锁死管理接口。

### 来源导入、启停和立即采集

```sh
cp sources.example.toml sources.toml
# 在宿主机编辑 sources.toml，只保留实际使用的来源，配置后设 enabled=true。
# 无需把文件复制进容器；标准输入不会遇到只读挂载/编辑器原子保存问题：
docker compose exec -T rproxy rproxy-cli source import - < sources.toml

docker compose exec -T rproxy rproxy-cli source list
docker compose exec -T rproxy rproxy-cli source show vendor-a
docker compose exec -T rproxy rproxy-cli source resume vendor-a
docker compose exec -T rproxy rproxy-cli source run vendor-a --wait --wait-timeout 60
# 更新已有来源：
docker compose exec -T rproxy rproxy-cli source import - --replace < sources.toml

docker compose exec -T rproxy rproxy-cli source pause vendor-a
docker compose exec -T rproxy rproxy-cli source remove vendor-a --yes
```

支持旧版 `[[providers]]` 和新 `[[sources]]` TOML，也支持 JSON manifest。来源在 SQLite 中持久化，由采集器按配置版本拉取；修改无需重启两个服务。暂停/编辑/删除后，旧版本任务的迟到结果会被拒绝。暂停来源只停止采集；删除来源默认保留现有代理，不会连带清空其他来源或现有 stick。

来源配置支持 GET/POST、params/headers/JSON 请求体、文本/JSON 提取、列表路径和地址字段映射。环境引用如 `${SUPPLIER_API_URL}` 由 worker 解析；将对应变量放到 `scripts/providers.env`。**环境变量本身变化需要重新创建 collector 才能注入；来源定义变化不需要。** 不要把 root token 或主加密密钥放入 providers.env。

```sh
docker compose up -d --force-recreate collector # 仅在修改 worker 环境/启动配置时
```

来源配置使用独立密钥加密保存，普通查询/导出脱敏。默认在状态目录生成 `.sqlite3.key` 私有文件（Unix 0600），密钥不在 SQLite 中；也可显式向 Rust 进程注入 base64 编码 32 字节的 `RPROXY_SOURCE_KEY`。外置密钥需在本地 Compose override 中显式注入，不能传给 collector。丢失或错误的密钥会拒绝启动，不会重置源配置。

```sh
# 默认脱敏导出，可重定向到宿主文件；不是完整凭据备份。
docker compose exec -T rproxy rproxy-cli source export > redacted-sources.json
```

完整秘密导出必须显式 `--include-secrets --output 私有文件 --yes`，不输出到 stdout，也不覆盖已有文件。正常备份应包含数据库和加密密钥。来源任务完成不代表每个候选已经可用，还需查看代理验证结果。

Xdaili、Shenlong 的旧映射可以显式导入，使用前核对当前接口字段和类型。Rola 只有在账户提供 IP 白名单提取接口时可配置相应 URL/映射；不猜测其 API 地址，不把认证旋转网关声称为已支持。

### 单条/批量 HTTP、SOCKS5 代理

```sh
# 以下是文档 IP，必须替换成真实可访问的上游。
docker compose exec -T rproxy rproxy-cli proxy add http 203.0.113.10:8080 --wait
docker compose exec -T rproxy rproxy-cli proxy add socks5 203.0.113.20:1080 --source manual
# IPv6 用 [地址]:端口；批量每行一个地址，读取宿主文件：
docker compose exec -T rproxy rproxy-cli proxy import http - --wait < addresses.txt

docker compose exec -T rproxy rproxy-cli proxy list --protocol http
docker compose exec -T rproxy rproxy-cli proxy show 'http:203.0.113.10:8080'
docker compose exec -T rproxy rproxy-cli proxy verify
docker compose exec -T rproxy rproxy-cli proxy disable 'http:203.0.113.10:8080' --dry-run
docker compose exec -T rproxy rproxy-cli proxy remove 'http:203.0.113.10:8080' --yes
```

添加是幂等的候选提交，**不是免检准入**；`--wait` 等待验证并对拒绝/超时返回非零。可查询待验证、可用、过期、不稳定、停用和最近拒绝原因。同一协议/地址可来自多个来源，去重不丢来源关联。

Root 会先验证实际出口、稳定性和 CONNECT 能力，未知/不稳定出口不进入选路。`probe.enabled=false` 只关闭定期检测；新候选和重启遗留候选仍需验证。修改代理可能导致会话换绑，先用 dry-run 看影响；有影响时需明确确认。旧连接排空前仍保护原出口，强制断开必须显式选择。

### 会话查看、释放与恢复

```sh
docker compose exec -T rproxy rproxy-cli session list --business shop
docker compose exec -T rproxy rproxy-cli session release shop account-a --dry-run
docker compose exec -T rproxy rproxy-cli session release shop account-a --yes
```

管理员不需要被管理用户的密码；释放后旧连接排空前不会重分配其出口。删除用户并不会静默回收其全部 stick，需显式清理不再使用的会话。保留旧用户拥有的会话可防止同名新用户接管。

## 配置与挂载：什么需要重启

| 数据 | 权威来源 | 如何变更 |
|---|---|---|
| 监听/TLS/数据库路径/认证模式/运行限额 | 宿主 `rproxy.toml`，只读挂载 `/etc/rproxy/rproxy.toml` | 修改后重新创建 Rust 容器 |
| 用户、密码哈希、授权、来源、手动代理、绑定 | SQLite 命名卷 `/var/lib/rproxy` | CLI 在线操作，重启不被旧文件覆盖 |
| Worker 连接、并发、轮询、CA | 宿主 `scripts/proxy.toml`，只读挂载 `/etc/rproxy/proxy.toml` | 修改后重新创建 collector |
| 管理/采集器令牌 | `.env` 分别注入指定服务 | 改动后重新创建相应服务，二者必须不同 |
| 供应商环境引用 | `scripts/providers.env`，只注入 collector | 修改后重新创建 collector |
| HTTPS 证书 | 可选 `certs/` 只读挂载 | 修改后重新创建 Rust 容器 |

Compose 固定容器内部 HTTP/SOCKS5/管理监听为 `0.0.0.0:9099/9089/9100`，数据库为 `/var/lib/rproxy/rproxy.sqlite3`；外部端口和绑定 IP 在 `.env` 中调整。不要用 TOML 内部监听地址改变宿主端口。

```sh
docker compose up -d --force-recreate rproxy collector # 启动配置/令牌/端口变更
# 仅源码或依赖变化才需要 --build。
```

重新创建可处理编辑器原子保存替换 inode 的情况；单独 `restart` 不会重新注入 `.env`。已有配置保留，不要反复复制覆盖。挂载路径可通过 `.env` 的 `RPROXY_CONFIG`、`RPROXY_COLLECTOR_CONFIG`、`RPROXY_PROVIDERS_ENV` 和 `RPROXY_CERTS_DIR` 指定；相对路径基于 compose.yaml 目录，TOML 内相对证书路径基于容器内 TOML 目录。

容器以 `10001:10001` 非 root 运行、根文件系统只读；确保配置/证书对该容器身份可读，状态卷可写。不要使用 chmod 777；Podman rootless 需注意 UID/GID 映射。缺少配置文件时 create_host_path:false 拒绝启动，不会创建同名目录。

**SELinux enforcing 注意：** 部分 Compose provider 在长格式挂载中不转发 `Z` 自动 relabel。请为专用配置/证书目录显式设置可读容器标签，不关闭 SELinux、不重标系统目录。例如使用 `semanage fcontext` 为本部署文件指定 `container_file_t`，再 `restorecon`；先确认实际绝对路径和权限。

### 可选 HTTPS 代理入口

在宿主准备 `certs/fullchain.pem` 和 `certs/private-key.pem`，在 TOML 顶层启用 `https_bind="0.0.0.0:9443"`，并配置：

```toml
[tls]
certificate = "certs/fullchain.pem"
private_key = "certs/private-key.pem"
```

```sh
docker compose -f compose.yaml -f compose.https.yaml up -d --build
```

后续操作始终带同样两个 `-f`。它会发布默认 `127.0.0.1:9443` 和只读证书挂载；仅映射端口不会启用 TLS。证书错误拒绝启动，不降级明文。自签名入口需客户端显式信任，如 curl `--proxy-cacert proxy-ca.pem`。不要把 `--proxy-insecure` 和目标的 `-k` 混淆；目标 HTTPS 仍由客户端验证。

普通 HTTP Basic / SOCKS5 密码不加密；跨不可信网络用 HTTPS 入口或安全通道。管理接口同样不应裸露到公网：可通过 SSH 在服务器内执行 CLI，或在受控 HTTPS/SSH 通道上使用原生 CLI。collector 的 CA 与 Rust probe 的 CA 单独配置，相关证书还需挂载给对应容器。

## 标准代理客户端

代理用户名格式不变：`v1|入口用户名|业务标识|会话标识|策略`。标识为 1–64 个 ASCII 字母数字或 `._-`，完整用户名最多 255 字节。普通用户名使用首个允许业务及默认会话/策略；不同网站账号应使用不同会话。

```sh
curl --proxy http://127.0.0.1:9099 --proxy-user 'v1|alice|shop|account-a|stick:YOUR_PASSWORD' https://example.com/
curl --proxy socks5h://127.0.0.1:9089 --proxy-user 'v1|alice|shop|account-a|stick:YOUR_PASSWORD' https://example.com/
```

```python
import os
from urllib.parse import quote
import requests
username = quote('v1|alice|shop|account-a|stick', safe='')
password = quote(os.environ['RPROXY_PASSWORD'], safe='')
proxy = f'http://{username}:{password}@127.0.0.1:9099'
with requests.Session() as session:
    session.trust_env = False
    session.proxies = {'http': proxy, 'https': proxy}
    print(session.get('https://example.com/', timeout=30).status_code)
```

放进 URL 时编码用户名/密码；传给浏览器的独立用户名字段或 `--proxy-user` 时使用原始值。浏览器按账号使用自己的代理会话；部分浏览器不支持 SOCKS5 用户名密码认证，改用已认证 HTTP/HTTPS 入口。部分 requests/urllib3 对 HTTPS 代理验证需要显式 Adapter，参考 scripts/smoke.py，不要全局 verify=False。

认证固定 TTL 与出口绑定独立。每次认证检查当前用户状态和版本，错误密码不会因缓存而被接受；到期重新验证，不能因认证续期更换 IP。CONNECT/SOCKS5 在建隧道时认证，不因 TTL 到期强行关闭已有隧道，显式断开操作除外。

## 升级、停止与备份

**从 0.2.2 升级到 0.2.3：** 先停止并备份状态，补充独立 worker token，使用新 collector 启动 TOML。SQLite 自动迁移旧用户归属并保留 stick；TOML 用户只导入一次，以后不会覆写 CLI 变更。旧 provider TOML 通过 CLI 显式导入，不能继续作为 worker 的第二份运行权威。升级后的 schema 不能直接交给旧二进制；回退需要恢复升级前备份。

**从 Redis 版 0.2.1 及以前升级：** 需要维护窗口，备份 Redis/旧配置，重新导入候选。不自动迁移 Redis stick/占用，不承诺保留旧出口；旧 Redis 参数、Hash 写入和维护脚本不再适用。

```sh
docker compose down                   # 保留命名卷和宿主配置
docker compose up -d --build          # 更新源码后继续使用原卷
# docker compose down -v              # 危险：删除数据、用户、来源、stick 和默认密钥

mkdir -p backup
docker compose stop collector rproxy
docker compose cp rproxy:/var/lib/rproxy ./backup/
docker compose up -d
```

备份整个状态目录（包括 WAL 和 `.key`）及宿主配置/外置密钥，妥善限制访问。不要运行中只复制 SQLite 主文件。固定 Compose 项目名/目录，否则可能创建新空卷。检测到损坏、未知 schema 或无法解密的来源会拒绝启动，不会自动清空重建。

## 原生构建与测试

```sh
cargo build --release --locked
cp rproxy.example.toml rproxy.toml
# 在 shell/服务管理器设置 RPROXY_ADMIN_TOKEN、可选 RPROXY_WORKER_TOKEN，
# 及首次 bootstrap 所需的 RPROXY_PASSWORD；原生进程不会自动读取 .env。
./target/release/rproxy --config rproxy.toml check-config
./target/release/rproxy --config rproxy.toml
# 另一个终端使用同一 root token：
./target/release/rproxy-cli user list
```

Windows 使用 `.exe`。默认运行项是 rproxy；旧 healthcheck/add-proxy/verify-pool/release-session 兼容命令保留，但新的管理操作推荐 rproxy-cli。`check-config` 不打开 SQLite、不要求环境密钥值；首次启动/迁移才解析需要导入的凭据。

```sh
cargo fmt --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked --all-targets
python -m pip install -r scripts/requirements.txt -r scripts/requirements-dev.txt
python -m unittest discover -s scripts/tests -v
python -m unittest discover -s scripts/release_tests -v
python -m unittest discover -s scripts/deployment_tests -v
python scripts/compose_smoke.py --engine podman # 或 docker
```

低内存构建使用 `cargo ... -j 1`，不必修改系统配置。Compose 烟测仅使用随机项目、临时配置、本地 HTTP/SOCKS5 假上游和自己的卷，结束后只清理自身资源；验证在线用户/来源/手动代理、改密不换出口、force disconnect、只读宿主配置、一次性导入、重启持久化、最后用户删除不开放匿名和受限 worker。原生多客户端烟测仍为 `scripts/smoke.py`，可选现有 Chrome/Playwright，不自动下载浏览器。

## 发布包

本文对应 **0.2.3**，发布入口 [v0.2.3 Release](https://github.com/flyfoxCI/rproxy/releases/tag/v0.2.3)。五个包覆盖 Linux x86_64/ARM64（musl）、macOS Intel/Apple Silicon（11.0+，未签名/公证）、Windows x86_64，每包包含 `rproxy` 和 `rproxy-cli` 及示例配置；用同一 Release 的 SHA256SUMS 校验。

Compose 从源码构建，不假定已有公开镜像；旧 v0.2.2 包没有独立 CLI，不能混用为新管理接口。历史发行版/标签不会被覆盖。

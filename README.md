# rproxy

面向多个业务和登录账号的 HTTP / HTTPS / SOCKS5 代理池入口。**当前源码使用内嵌 SQLite，不再依赖 Redis**：Rust 负责候选准入、实际出口检测、选路和持久会话；可选的 Python 采集器从供应商 API 获取地址，通过带认证的管理接口提交候选。

推荐使用下面的 Docker / Podman Compose 部署。**配置在宿主机编辑，再只读挂载进容器；不用进入容器改文件，也不用为了改配置重建镜像。**

## 能力与边界

- HTTP 正向代理、HTTP CONNECT、TLS 加密的 HTTPS 代理入口。
- SOCKS5 TCP CONNECT，支持 IPv4 / 域名 / IPv6、匿名或用户名密码认证。
- HTTP 和 SOCKS5 上游适配，入口协议与上游协议可以交叉组合。
- `stick`、`random`、`round-robin`、`shuffle` 四种连接级选路。
- 按 **业务 + 会话** 保持绑定，同业务不同会话不能共享实际出口 IP，不同业务可以共享。
- SQLite 持久化候选和会话；Rust 重启不主动释放 stick 绑定。
- Python TOML 配置 GET/POST、请求参数/头、文本或 JSON 地址提取，不直接写数据库。

**单个数据库只允许一个 Rust 进程持有。** 状态目录必须位于支持可靠文件锁的本地文件系统，不使用 NFS/SMB 等网络共享；运行中不要删除、替换数据库或锁文件。不支持多实例共享数据库，也不承诺跨机器出口排他。不要给 `rproxy` 扩容副本。其他边界包括：不支持 SOCKS5 UDP/BIND、HTTPS 上游、上游用户名密码、供应商粘性网关参数、不可控轮换出口、目标站点登录管理或 Web 管理后台。

## Docker / Podman Compose 快速启动

### 1. 前置条件

- Docker Engine / Docker Desktop + Docker Compose v2 或更新版本；或者 Podman + Compose provider（例如 Docker Compose）。
- Windows / macOS 使用 Podman 时，先启动 Podman machine；Docker Desktop 用户先启动 Docker Desktop。
- 第一次构建需要联网下载 Rust/Python 基础镜像和依赖；宿主机不必安装 Rust、Python 或 Redis。

本文以 `docker compose` 为例。使用 Podman 时将它替换为 `podman compose`，其余参数相同：

```sh
podman machine start # 仅在 machine 尚未启动时需要
podman compose version
```

`podman compose` 是 provider 的包装器；若提示找不到 provider，请先安装 Docker Compose 或兼容的 `podman-compose`。推荐使用支持长格式 bind mount、`create_host_path: false` 和健康依赖条件的近期 provider。

### 2. 在宿主机准备配置

在仓库根目录执行，**仅首次创建时复制，已有配置请保留**：

```sh
cp rproxy.example.toml rproxy.toml
cp scripts/proxy.example.toml scripts/proxy.toml
cp scripts/providers.env.example scripts/providers.env
cp .env.example .env
```

Windows PowerShell 可分别使用 `Copy-Item 源文件 目标文件`。以上文件必须是实际文件，不能是同名目录；缺少挂载源时 Compose 会报错，不会偷偷创建配置目录。

用本地编辑器打开 `.env`，填写**两个不同的随机值**：

```dotenv
RPROXY_PASSWORD=替换为随机的入口密码
RPROXY_ADMIN_TOKEN=替换为至少16个非空白ASCII字符的随机管理令牌
RPROXY_BIND_ADDRESS=127.0.0.1
RPROXY_HTTP_PORT=9099
RPROXY_SOCKS5_PORT=9089
RPROXY_HTTPS_PORT=9443
```

管理令牌要求 16–1024 个非空白可打印 ASCII 字符。建议两个值均使用 32 字节随机数生成的 URL-safe 字符串；如宿主机有 Python，可执行两次：

```sh
python -c "import secrets; print(secrets.token_urlsafe(32))"
```

真实 `.env`、`rproxy.toml`、`scripts/proxy.toml`、`scripts/providers.env` 和 `certs/` 已加入 Git 忽略规则；镜像构建使用白名单，不包含这些文件、证书或运行数据。不要把真实凭据写入示例或提交到 Git。配置保留 `password_env` / `token_env` 引用即可，**无需将密码复制进 TOML**。

`rproxy.toml` 默认入口用户为 `alice`，允许业务 `shop` 和 `news`。`scripts/proxy.toml` 的供应商默认全部禁用，因此第一次启动不需要供应商密钥，采集器会保持运行但空闲。

### 3. 构建并启动

```sh
docker compose config --quiet
docker compose up -d --build
docker compose ps
docker compose logs --tail=100 rproxy collector
```

Podman 对应：

```sh
podman compose config --quiet
podman compose up -d --build
podman compose ps
```

两个服务分别是：

| 服务 | 职责 | 默认宿主机端口 |
|---|---|---|
| `rproxy` | 代理入口、管理接口、探测和 SQLite 状态 | HTTP `127.0.0.1:9099`、SOCKS5 `127.0.0.1:9089` |
| `collector` | 根据 TOML 定时采集供应商候选并提交给 Rust | 无 |

管理接口 `9100` 仅供容器网络与容器内 CLI 使用，**不映射到宿主机**。采集器等待 rproxy 健康后启动；运行中管理接口或供应商短暂不可达，会记录失败并在下一周期重试。

验证 Rust 进程和状态工作线程：

```sh
docker compose exec -T rproxy rproxy --config /etc/rproxy/rproxy.toml \
  --admin-bind 127.0.0.1:9100 healthcheck
```

返回候选、有效验证记录、会话和连接计数。**健康不代表已有可用上游**：空池可以正常启动，但业务请求会明确失败，不会绕过代理直连。先启用供应商或手动导入上游，再尝试客户端访问。

### 4. 配置挂载与修改生效

| 宿主机位置 | 容器内位置 | 用途 |
|---|---|---|
| `rproxy.toml` | `/etc/rproxy/rproxy.toml`，只读 | Rust 认证、业务、探测、TLS 等 |
| `scripts/proxy.toml` | `/etc/rproxy/proxy.toml`，只读，仅采集器 | 供应商 URL、协议、提取规则、周期等 |
| `.env` | 不挂载；Compose 注入指定环境变量 | 入口密码、管理令牌、宿主端口与挂载路径 |
| `scripts/providers.env` | 不挂载；仅注入采集器环境 | 启用的供应商所需环境变量 |
| `rproxy-data` 命名卷 | `/var/lib/rproxy`，可写，仅 Rust | SQLite 数据和 stick 会话 |
| `certs/`（仅 HTTPS override） | `/etc/rproxy/certs`，只读，仅 Rust | 入口证书/私钥及可选探测 CA |

Compose 为容器内部固定监听 `0.0.0.0:9099/9089/9100`，数据库固定为 `/var/lib/rproxy/rproxy.sqlite3`，优先于原生示例的监听地址和 `database`。**修改对外端口请编辑 `.env`，而不是 TOML 的 `http_bind` / `socks5_bind`。** `https_bind` 仍由 TOML 控制。

两个进程以非 root `10001:10001` 运行，根文件系统只读。Linux 上应保证 TOML 和启用的证书能被容器 UID/GID 读取，不要用 `chmod 777` 解决权限问题；私钥可通过受控组权限提供读取。

**SELinux enforcing 主机额外注意：** Compose 文件请求了 `Z` 标签，但部分 Docker Compose provider 在 `create_host_path: false` 的长格式挂载中不会转发自动 relabel；仅看到 YAML 中的 `Z` 不代表已生效。请为本部署专用配置显式设置容器可读的标签，不要关闭 SELinux 或挂载/重标记系统目录。例如仓库位于 `/srv/rproxy` 时（将路径替换成实际绝对路径）：

```sh
sudo semanage fcontext -a -t container_file_t '/srv/rproxy/(rproxy\.toml|scripts/proxy\.toml|certs(/.*)?)'
sudo restorecon -v rproxy.toml scripts/proxy.toml
# 仅在准备 HTTPS 证书目录后：
sudo restorecon -Rv certs
```

如果该路径已存在自定义规则，用 `semanage fcontext -m` 更新。`container_file_t` 是共享容器文件类型，仍应以文件权限保护凭据/私钥；这里不承诺 provider 自动应用私有 MCS 标签。编辑器原子替换文件后，先对新文件再次执行 `restorecon`，再重新创建容器。自定义挂载路径也要设置相应规则。

配置不热加载。保存宿主机文件后，**推荐重新创建受影响容器**，无需 rebuild：

```sh
# 修改 Rust TOML、入口密码、管理令牌或对外端口后：
docker compose up -d --force-recreate rproxy collector

# 只改供应商 TOML 或 scripts/providers.env：
docker compose up -d --force-recreate collector
```

这样同时处理环境变量变更和编辑器“原子保存”替换文件的情况；单独 `restart` 不会重新注入 `.env` / `env_file`。重新创建容器保留命名数据卷，但会关闭旧连接。仅源码/依赖变化才需要 `--build`。

已有配置可放在别处，在 `.env` 中设置：

```dotenv
RPROXY_CONFIG=./config/rproxy.toml
RPROXY_COLLECTOR_CONFIG=./config/proxy.toml
RPROXY_PROVIDERS_ENV=./config/providers.env
RPROXY_CERTS_DIR=./certs
```

相对挂载源路径以 `compose.yaml` 所在目录为基准。TOML 中的相对证书路径以**容器内 TOML 所在目录**为基准；不要填只有宿主机才存在的绝对路径。

### 5. 启用供应商或手动导入

编辑 `scripts/proxy.toml`，只开启实际使用的 provider。例如示例中的通用文本供应商：

```toml
[[providers]]
name = "text-provider"
enabled = true
url = "${SUPPLIER_API_URL}"
protocol = "http"
method = "GET"
interval_seconds = 60
[providers.params]
count = 10
[providers.headers]
Authorization = "Bearer ${SUPPLIER_API_TOKEN}"
[providers.response]
format = "text"
```

在 `scripts/providers.env` 中填写所引用的变量（不要写入 `.env.example`）：

```dotenv
SUPPLIER_API_URL=https://your-provider.example/extract
SUPPLIER_API_TOKEN=replace-with-your-secret
```

如果供应商不需要 Authorization，删除该请求头以及对应变量引用即可。仅启用的 provider 会展开 `${变量名}`；缺失必需变量会使配置检查失败。文件还提供 Xdaili、Shenlong 的历史 JSON 映射和通用 POST JSON 示例，使用前须核对供应商当前接口约定。**这只是地址提取配置，不是供应商旋转网关认证或 sticky-session 支持。**

检查并重建采集器：

```sh
docker compose run --rm --no-deps collector --config /etc/rproxy/proxy.toml --check-config
docker compose up -d --force-recreate collector
docker compose logs -f collector
```

也可不用采集器，手动提交一个候选：

```sh
docker compose exec -T rproxy rproxy --config /etc/rproxy/rproxy.toml \
  --admin-bind 127.0.0.1:9100 add-proxy --protocol http --address 203.0.113.10:8080

docker compose exec -T rproxy rproxy --config /etc/rproxy/rproxy.toml \
  --admin-bind 127.0.0.1:9100 verify-pool
```

上面的 IP 是文档地址，必须换成真实代理。`verify-pool` 返回 `scheduled`，表示已请求异步验证，不表示全池已验证完成。候选提交的 `accepted` 也不等于已准入。

Rust 通过每个上游两次请求 `probe.url` 比较实际出口，再检查 HTTP 上游对 `probe.tunnel_url` 目标的 CONNECT 能力。响应需为纯 IP 或 `{"ip":"..."}` / `{"origin":"..."}`，最多 4096 字节。只有稳定、未超龄且满足连接能力的记录才能选中；`probe.enabled=false` 只关闭定期检测，新候选仍需验证。一次探测不能保证上游允许任意 CONNECT 目标，也不能证明供应商永久不轮换。

### 6. 可选 HTTPS 代理入口

默认不会创建或挂载证书目录。需要 HTTPS 时，在宿主机准备 `certs/fullchain.pem` 与 `certs/private-key.pem`，并在 `rproxy.toml` 中启用：

```toml
# 必须在 [admin]/[auth] 等表之前，属于 TOML 顶层。
https_bind = "0.0.0.0:9443"

[tls]
certificate = "certs/fullchain.pem"
private_key = "certs/private-key.pem"
```

始终带上两个 Compose 文件操作该部署：

```sh
docker compose -f compose.yaml -f compose.https.yaml config --quiet
docker compose -f compose.yaml -f compose.https.yaml up -d --build
# 证书或配置修改后，无需重建镜像：
docker compose -f compose.yaml -f compose.https.yaml up -d --force-recreate rproxy collector
```

这会额外映射 `127.0.0.1:9443`，并只读挂载证书目录。仅发布 9443 端口不会自动启用 TLS；必须同时配置 `https_bind` 和证书。证书错误会启动失败，不会降级到明文。

自签名证书需要客户端显式信任，例如 curl `--proxy-cacert proxy-ca.pem`。`--proxy-insecure` 只跳过代理证书验证，`-k` 则影响目标证书；不建议用它们掩盖配置问题。目标 HTTPS 通过 CONNECT/SOCKS5 原样透传，不做 MITM。`probe.ca_certificate` / `probe.insecure` 只影响 Rust 自己的检测请求，不影响业务隧道；采集器自定义 CA 也需另行只读挂载到采集器可访问的路径。

### 7. 停止、升级与备份

```sh
docker compose down                 # 保留数据卷和宿主机配置
docker compose up -d --build        # 更新源码后重建，继续使用原数据卷
# docker compose down -v           # 危险：删除本项目数据卷及持久会话
```

默认卷名带 Compose 项目前缀。固定仓库目录/项目名（必要时始终使用同一个 `-p 名称`），否则可能创建另一个空卷，看起来像“状态丢了”。不要将 `down -v` 当作普通重启步骤。

离线备份示例（HTTPS 部署同样追加两个 `-f` 参数）：

```sh
mkdir -p backup
docker compose stop collector rproxy
docker compose cp rproxy:/var/lib/rproxy ./backup/
docker compose up -d
```

停止后备份整个数据目录，并单独安全保存宿主机配置与凭据。不要在运行中只复制 SQLite 主文件而遗漏 WAL。升级前备份，恢复前停止所有持有数据库的进程；损坏或不兼容的数据库会拒绝启动，不会自动重置成空池。

### 常见启动问题

| 现象 | 检查方法 |
|---|---|
| `.env` 变量缺失/为空 | 填写两个随机值后重新 `config --quiet`；不要在日志或工单贴出 `compose config` 完整输出，它可能展开密钥 |
| 配置文件不存在/成了目录 | 确认执行过首次复制；检查挂载源路径，修正后 force-recreate |
| `rproxy` 不健康 | 查看 `logs rproxy`；检查令牌格式、证书、文件权限、卷是否可写及数据库是否被另一进程占用 |
| 采集器一直空闲 | 默认 provider 都是 disabled；按供应商实际接口启用并设置 env_file |
| 采集失败/未准入 | 检查供应商变量、返回映射、剩余容量、管理 URL 与令牌、探测站点连通性；不要把服务健康误当成代理可用 |
| 修改 `.env` 后仍用旧密码/端口 | 用 `up -d --force-recreate`，不能只 `restart` |
| 端口被占用 | 修改 `.env` 中宿主端口；不要改容器内部管理 URL |
| Podman 连接失败 | 检查 machine 是否启动、`podman info` 是否成功、Compose provider 是否安装 |

默认只允许本机访问代理端口。需要受控局域网访问时才将 `RPROXY_BIND_ADDRESS` 改为指定主机地址或 `0.0.0.0`，并配置防火墙。HTTP Basic / 普通 SOCKS5 密码不加密，跨不可信网络优先使用 HTTPS 代理入口或安全网络通道。不要把匿名入口或明文管理接口暴露到公网。

## 标准客户端的业务、会话与策略

代理用户名格式：

```text
v1|入口用户名|业务标识|会话标识|策略
```

例如 `v1|alice|shop|account-a|stick`。入口用户、业务、会话标识限 1–64 个 ASCII 字母、数字、`.`、`_`、`-`，完整用户名最多 255 字节。业务必须在该用户的 `businesses` 白名单中。不同网站账号应使用不同会话；普通用户名 `alice` 则使用首个允许业务、默认会话和默认策略。

| 策略 | 行为 |
|---|---|
| `stick` | 跨连接沿用绑定，不因空闲、认证续期或 Rust 重启释放；上游确认不可用后换绑 |
| `random` | 每个新连接避免上次出口；只剩上次出口时明确失败 |
| `round-robin` | 在合格且未被同业务其他会话占用的出口间顺序轮换，单候选可重复 |
| `shuffle` | 按会话随机不放回，每轮结束后重洗；可行时避免跨轮重复，单候选可重复 |

所有策略先遵守业务内实际出口排他；没有可用出口则失败，不共享、不直连、不换策略。同一业务/会话属于首次创建它的入口身份，其他身份不能接管。**选择粒度是 TCP 连接，不是 HTTP 请求**：keep-alive 请求沿用已选出口，CONNECT/SOCKS5 隧道不能中途迁移。IPv4-mapped IPv6 出口按 IPv4 归一化。

### curl

```sh
curl --proxy http://127.0.0.1:9099 \
  --proxy-user 'v1|alice|shop|account-a|stick:YOUR_PASSWORD' https://example.com/

curl --proxy socks5h://127.0.0.1:9089 \
  --proxy-user 'v1|alice|shop|account-a|stick:YOUR_PASSWORD' https://example.com/

curl --proxy https://proxy.example.com:9443 --proxy-cacert proxy-ca.pem \
  --proxy-user 'v1|alice|shop|account-a|stick:YOUR_PASSWORD' https://example.com/
```

### Python requests / 浏览器

```sh
python -m pip install 'requests[socks]'
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

放进 URL 时需百分号编码用户名密码；传给 `--proxy-user` 或浏览器独立用户名字段时使用原始字符串。部分 requests/urllib3 版本对 HTTPS 代理的证书验证需要显式 Adapter，参考 `scripts/smoke.py`；不要靠全局 `verify=False` 消除警告。Playwright 等浏览器每个账号使用自己的代理会话；某些浏览器不支持 SOCKS5 用户名密码认证，请改用已认证 HTTP/HTTPS 入口。

认证周期与出口绑定独立：`auth.ttl_seconds` 为固定周期，非滑动续期。HTTP 每个请求验证密码，CONNECT/SOCKS5 在建立时验证；周期到期不会打断已建隧道。释放会话：

```sh
docker compose exec -T rproxy rproxy --config /etc/rproxy/rproxy.toml \
  --admin-bind 127.0.0.1:9100 release-session --username 'v1|alice|shop|account-a|stick'
```

CLI 使用容器已有环境变量验证入口凭据，不用把密码作为参数。释放后旧连接排空前不会把出口交给同业务其他会话。

## 不使用容器：原生构建与采集

需要 Rust 工具链；SQLite 已捆绑构建，不需要独立数据库服务。Python 采集器要求 **Python 3.11+**。

```sh
cargo build --release
cp rproxy.example.toml rproxy.toml
export RPROXY_PASSWORD='YOUR_PASSWORD'
export RPROXY_ADMIN_TOKEN='YOUR_RANDOM_ASCII_ADMIN_TOKEN'
./target/release/rproxy --config rproxy.toml check-config
./target/release/rproxy --config rproxy.toml
```

Windows 二进制为 `target/release/rproxy.exe`，PowerShell 用 `$env:RPROXY_PASSWORD=...` 设置变量。`check-config` 检查结构与 TLS 文件，不打开数据库、不要求提供环境密钥；启动时才解析并验证密钥。原生模式下，数据库/证书相对路径以 TOML 所在目录为准，`RPROXY_DATABASE` 可覆盖数据库路径。

支持 `--database`、`--admin-bind`、`-H/--http-bind`、`-S/--socks5-bind`、`--https-bind`、`--no-http`、`--no-socks5`、`-q/-v`；命令行优先于文件。原生示例管理/代理入口默认只监听 loopback。未配置认证用户时为匿名模式，不可暴露到不可信网络。

可选原生采集器：

```sh
python -m venv .venv
# 激活环境后：
python -m pip install -r scripts/requirements.txt
cp scripts/proxy.example.toml scripts/proxy.toml
# 将 [rproxy].url 改为 http://127.0.0.1:9100，配置供应商和环境变量。
python scripts/collector.py --config scripts/proxy.toml --check-config
python scripts/collector.py --config scripts/proxy.toml
# 单次采集：添加 --once
```

原生执行不会自动加载 `.env` 或 `scripts/providers.env`；请通过当前 shell/服务管理器注入环境变量。CLI `add-proxy` / `healthcheck` / `verify-pool` 需要已运行的 Rust 管理接口，不能直接修改数据库。

## 发布包与 Redis 迁移

当前 SQLite 架构对应 **0.2.2**，下载入口为 [v0.2.2 Release](https://github.com/flyfoxCI/rproxy/releases/tag/v0.2.2)。GitHub [Releases](https://github.com/flyfoxCI/rproxy/releases) 提供 Linux x86_64/ARM64、macOS Intel/Apple Silicon、Windows x86_64 包；使用同一 Release 的 `SHA256SUMS` 校验。Linux 发布包使用 musl 静态链接；macOS 目标 11.0+，未做 Apple Developer 签名/公证。

**已经发布的 v0.2.1 是旧 Redis 架构，不能与当前 SQLite 配置混用。** 本文 Compose 从当前源码构建，不拉取一个未经发布的预构建镜像。

从旧 Redis 版本升级属于不兼容迁移：安排维护窗口、停止旧服务并备份 Redis/配置，使用新的 TOML、环境变量与 SQLite 卷启动，再通过管理接口或新采集器重新导入候选。**不自动导入 Redis stick 绑定/占用状态，也不承诺升级后沿用原出口。** 旧的 `--redis-url`、`--redis-key`、`RPROXY_REDIS_*` 和直接写 Redis Hash 的采集/检查脚本不再适用。

## 测试与部署烟测

Rust 测试使用隔离 SQLite 和本地假上游，不需要 Redis 或真实供应商：

```sh
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test --all-targets
python -m pip install -r scripts/requirements.txt -r scripts/requirements-dev.txt
python -m unittest discover -s scripts/tests -v
python -m unittest discover -s scripts/release_tests -v
python -m unittest discover -s scripts/deployment_tests -v
```

实际 Compose 烟测（需要运行中的容器引擎）：

```sh
python scripts/compose_smoke.py --engine docker
# 或：
python scripts/compose_smoke.py --engine podman
```

烟测构建两个镜像，使用随机项目名、临时配置和随机宿主端口，额外启动一个仅本地使用的假供应商/上游；验证采集准入、认证 HTTP/SOCKS5、健康检查、缺失挂载/密钥拒绝、宿主机原子编辑后的配置生效、空闲采集器和重启后会话持久化。结束时仅删除烟测自身的容器/卷，不读取真实供应商密钥或修改实际部署配置。镜像构建缓存保留供后续使用。

原生客户端烟测：

```sh
cargo build
python -m pip install playwright
python scripts/smoke.py --binary target/debug/rproxy.exe \
  --browser 'C:/Program Files/Google/Chrome/Application/chrome.exe'
```

Linux 将二进制路径改为 `target/debug/rproxy`；`--browser` 为可选现有 Chromium/Chrome 路径，不会自动下载浏览器。

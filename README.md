# rproxy

面向多个业务和登录账号的 HTTP / HTTPS / SOCKS5 代理池入口。Python 获取和检查上游，Redis 保存地址与会话状态，Rust/Tokio 接收连接并选择上游。

## 已实现的能力

- HTTP 正向代理、HTTP CONNECT、TLS 加密的 HTTPS 代理入口。
- SOCKS5 TCP CONNECT，IPv4 / 域名 / IPv6，匿名或用户名密码认证。
- HTTP 和 SOCKS5 上游适配；HTTP 入口也能走 SOCKS5 上游，反之亦然。
- `stick`、`random`、`round-robin`、`shuffle` 四种连接级选路。
- 以 **业务 + 会话** 保持绑定，同业务不同会话不能共享实际出口 IP，不同业务可以共享。
- Redis 原子分配、多实例竞争控制、连接租约、持久 stick 绑定及显式释放。
- 建连阶段上游失败时记录日志、有限重试并自动换绑；不重放已发送的业务请求。
- 认证有效期与 IP 绑定独立：认证续期不更换仍然有效的 stick 出口。

不支持 SOCKS5 UDP/BIND、HTTPS 上游、上游用户名密码或不可控轮换出口。它们不在本轮范围中。

## 下载发布包

从 [GitHub Releases](https://github.com/flyfoxCI/rproxy/releases) 下载：

- Linux x64：`rproxy-v0.2.0-x86_64-unknown-linux-musl.tar.gz`（静态链接，无需另装 OpenSSL）。
- Windows x64：`rproxy-v0.2.0-x86_64-pc-windows-msvc.zip`。
- 使用同一 Release 的 `SHA256SUMS` 检查下载完整性。

解压后复制示例配置并修改密码，再运行包内 `rproxy` / `rproxy.exe`。包内包含 Python 维护脚本，其依赖仍需按下文安装。

## 构建和启动

需要 Rust、可访问的单机 Redis（支持 Lua/cjson，测试覆盖 Redis 5），以及已有的上游代理。多个 rproxy 实例可共享同一个 Redis 和 key 前缀；**不支持 Redis Cluster 的跨槽路由脚本**。

```sh
cargo build --release
cp rproxy.example.toml rproxy.toml
# 修改入口密码，必要时配置 Redis 认证、监听地址和证书。
./target/release/rproxy --config rproxy.toml check-config
./target/release/rproxy --config rproxy.toml
```

Windows 可执行文件为 `target/release/rproxy.exe`。Windows GNU 工具链需要 MinGW 的 `gcc`、`dlltool` 等工具位于 PATH。

支持 `-H/--http-bind`、`-S/--socks5-bind`、`--https-bind`、`-r/--redis-url`、`--redis-key`、`--no-http`、`--no-socks5`、`-q` 和 `-v`。命令行覆盖配置；`RPROXY_REDIS_URL` / `RPROXY_REDIS_KEY` 覆盖文件，但优先级低于命令行。

未配置 `[auth].users` 时为匿名模式。**不要把匿名入口暴露到不可信网络。** HTTP Basic 和普通 SOCKS5 的密码本身不加密，跨不可信网络优先使用 HTTPS 代理入口或受保护的网络通道。

配置文件中的入口密码和 Redis URL 可能含敏感信息，请限制文件访问权限。程序不打印密码、完整认证头或完整业务 URL。凭据与配置变更需要重启实例后生效；多实例应同步更新，逐个重启。旧进程不会自动重载密码。

## 添加和验证上游

保持兼容原有 Hash：

```sh
redis-cli HSET proxy:pool:http 203.0.113.10:8080 '{}'
redis-cli HSET proxy:pool:socks5 203.0.113.20:1080 '{}'
./target/release/rproxy --config rproxy.toml verify-pool
```

上面的 IP 是文档示例，运行前换成真实可用代理。Python 脚本也会写这两个 Hash。

rproxy 首次启动及定期刷新时，通过每个上游请求配置的 `probe.url`，检测实际出口。响应需为纯 IP 或 `{"ip":"..."}` / `{"origin":"..."}`，最多 4096 字节。通过两次独立连接比较出口，再检查 HTTP 上游对 `probe.tunnel_url` 目标的 CONNECT 能力。

**地址存在于 Hash 中不代表立即可被选中。** 还必须有稳定、未超龄的出口检测结果。`probe.enabled=false` 只适合已有可信检测记录的受控部署，不会把未知出口当作已验证。

- 实际出口按 IP 比较，不按上游端口；IPv4-mapped IPv6 地址会统一为 IPv4。
- HTTP CONNECT 的目标端口限制仍取决于上游。一次探测不能保证它接受所有目标。
- 两次相同出口只能帮助排除明显轮换，不能证明供应商永久不轮换。固定出口保证以供应商符合其声明为前提。
- 已识别不稳定的记录保持 `stable=false`，不会自动重新准入；运营方确认其稳定能力后，应更新/清除对应 `proxy:pool:verified` 记录，再重新验证。
- 将来接入供应商粘性上游，需要单独实现供应商 Adapter；不可控轮换上游不能承诺 stick 或业务内出口排他。

`proxy:pool:verified` 保存上游协议、地址、实际出口、验证时间等 JSON；路由状态位于 `proxy:pool:route:<business>`。不要手工删除占用状态来强行腾出仍活跃的出口。

## 标准客户端如何指定业务、会话和策略

使用标准代理用户名，不需要 SDK 或自定义 HTTP 头：

```text
v1|入口用户名|业务标识|会话标识|策略
```

例如：

```text
v1|alice|shop|account-a|stick
v1|alice|shop|account-b|stick
v1|alice|news|job-1|random
```

入口用户、业务、会话标识限定为 1–64 个 ASCII 字母、数字、`.`、`_`、`-`；完整用户名不超过 255 字节。不得出现空字段或未知策略。业务必须在该入口用户的 `businesses` 白名单中。

同一业务/会话属于首次创建它的入口身份，其他身份不能接管。更换会话策略需先释放并排空旧会话。不要让不同账号意外使用同一个会话标识。

仅提供普通用户名 `alice` 时，使用该用户第一个允许业务、默认会话和默认策略。匿名访问使用默认业务/会话；代理无法自动识别同一程序内部的多个网站账号。需要多账号隔离时必须显式提供各自路由身份。

| 策略 | 行为 |
|---|---|
| `stick` | 跨连接沿用同一上游和出口；不因空闲、认证续期或 Rust 重启释放；确认上游失败时自动换绑 |
| `random` | 每个新连接随机选择不同于该会话上次的出口；只有上次出口可选时明确报错 |
| `round-robin` | 在合格且未被同业务其他会话占用的出口间顺序轮换；单候选可重复 |
| `shuffle` | 按会话随机不放回选择，一轮结束后重洗；可行时避免跨轮连续重复；单候选可重复 |

所有策略先遵守业务内出口排他。没有可用出口时明确失败，不共享、不直连、不切换成其他策略。非 stick 的出口由活跃连接占用，连接关闭后释放；历史选择记录仍用于后续轮换。

**选择粒度是代理 TCP 连接，不是 HTTP 业务请求。** 浏览器、requests.Session 等会复用连接；一条连接内的多个请求沿用同一次选择。要让 random 每次请求换出口，调用方应创建新连接，或对普通 HTTP 请求使用 `Connection: close`。CONNECT/SOCKS5 隧道建立后不能中途迁移出口。

## 客户端示例

### curl

```sh
curl --proxy http://127.0.0.1:9099 \
  --proxy-user 'v1|alice|shop|account-a|stick:YOUR_PASSWORD' \
  https://example.com/

curl --proxy socks5h://127.0.0.1:9089 \
  --proxy-user 'v1|alice|shop|account-a|stick:YOUR_PASSWORD' \
  https://example.com/

curl --proxy https://proxy.example.com:9443 \
  --proxy-cacert proxy-ca.pem \
  --proxy-user 'v1|alice|shop|account-a|stick:YOUR_PASSWORD' \
  https://example.com/
```

### Python requests

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
# 同样可以用 socks5h://... 或客户端支持的 https://代理入口。
with requests.Session() as session:
    session.trust_env = False
    session.proxies = {'http': proxy, 'https': proxy}
    print(session.get('https://example.com/', timeout=30).status_code)
```

放进 URL 时需百分号编码用户名和密码；传给 `basic_auth`、`--proxy-user` 或浏览器的独立用户名字段时使用原始字符串，不要再编码一次。

注意：部分 requests/urllib3 版本在“HTTP 目标经过 HTTPS 代理”时，不会仅因 `verify=CA文件` 就开启代理证书验证。可参考 `scripts/smoke.py` 中显式验证代理证书的 Adapter；不要忽略 `InsecureRequestWarning`，也不要靠全局 `verify=False` 消除它。

### Playwright / 浏览器自动化

```python
browser = playwright.chromium.launch(proxy={
    'server': 'http://127.0.0.1:9099',
    'username': 'v1|alice|shop|account-a|stick',
    'password': os.environ['RPROXY_PASSWORD'],
})
```

每个网站账号使用自己的 browser/context 代理配置与会话标识。某些浏览器不支持 SOCKS5 用户名密码认证，即使底层 SOCKS5 标准支持也无法由代理端补齐；这类客户端改用已认证的 HTTP/HTTPS 入口。浏览器的代理配置粒度及 HTTPS 代理支持以其实际版本为准。

## 认证有效期与释放绑定

`auth.ttl_seconds` 为固定认证周期，不随请求滑动续期。每个请求仍检查提供的密码；认证周期到期后重新建立验证记录。标准客户端会自动再次提交配置的密码，不代表必然弹出人工登录窗口。

- 普通 HTTP 每个请求验证，不能在同一 keep-alive 连接切换业务/会话/策略。
- CONNECT/SOCKS5 建立隧道时验证，认证到期不强制打断已建立隧道。
- 重新认证和凭据版本变化不主动清除 stick 绑定。
- 目标网站的 Cookie、登录期限及重新登录完全由网站与调用方控制。

主动结束一个会话时：

```sh
export RPROXY_PASSWORD='YOUR_PASSWORD'
./target/release/rproxy --config rproxy.toml release-session \
  --username 'v1|alice|shop|account-a|stick'
```

命令验证入口凭据和会话归属，停止该会话继续分配；旧连接排空前不会把出口交给同业务其他会话。不要在命令行参数中直接传密码。

## TLS 与证书

HTTPS 代理入口需要配置 `https_bind` 与 `[tls]` 的证书链/私钥。路径相对配置文件解析。证书错误会启动失败，绝不自动降级成明文。

目标 HTTPS 通过 CONNECT/SOCKS5 原样透传，不做 MITM、不替换证书、不额外验证隧道内目标证书。目标证书仍由客户端验证。

自签名入口证书需客户端显式信任。curl 的 `--proxy-cacert` 配置代理信任；`--proxy-insecure` 只跳过代理证书验证，`-k` 则影响目标验证，两者不要混淆。服务端不能单方面让所有客户端接受无效证书。

`probe.ca_certificate` / `probe.insecure` 仅控制 rproxy 自己的出口 HTTP(S) 检测，不影响用户 TLS 隧道。默认验证证书；兼容性例外须显式配置。

## Python 采集与检查

Python 3.10+：

```sh
python -m venv .venv
# 激活 .venv 后：
pip install -r scripts/requirements.txt
export RPROXY_REDIS_URL='redis://127.0.0.1:6379/'
export RPROXY_REDIS_KEY='proxy:pool'
python scripts/crawl.py http 'SUPPLIER_API_URL' -e 60 -m 120
python scripts/crawl.py socks5 'SUPPLIER_API_URL' -e 60 -m 120
python scripts/check.py http -e 60 -c 60
python scripts/check.py socks5 -e 60 -c 60
```

**两个脚本的 `-e` 均为秒**（旧版检查脚本曾使用分钟）。采集容量在 Redis 内原子限制；检查使用快照条件更新，旧检测结果不会删除刚写入的新记录。

可用 `RPROXY_CHECK_URL`、`RPROXY_CHECK_CA` 和显式 `RPROXY_CHECK_INSECURE=true` 配置检测。Python 的基础可达检查不替代 Rust 的实际出口验证。

## 测试

集成测试需要独立测试 Redis，默认 `127.0.0.1:16379`，不访问真实供应商或公网检查站点：

```sh
redis-server --port 16379 --save '' --appendonly no
# 另一终端，可选覆盖：
export RPROXY_TEST_REDIS='redis://127.0.0.1:16379/'
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test --all-targets
python -m unittest discover -s scripts/tests -v
```

测试写入随机 `*-test-*` / `rproxy-test-*` / `lifecycle-*` / `regression-*` 前缀；使用临时 Redis，不要指向生产。测试覆盖真实 Redis 原子竞争、出口别名、租约回收、Redis 断连、认证、HTTP 分帧/keep-alive、TLS 入站和端到端 TLS、多协议上游以及关闭已升级隧道。

实际可执行程序的客户端烟测（仅使用本地假上游与临时配置）：

```sh
pip install 'requests[socks]' playwright
cargo build
python scripts/smoke.py --binary target/debug/rproxy.exe \
  --browser 'C:/Program Files/Google/Chrome/Application/chrome.exe'
```

Linux 将 binary 换为 `target/debug/rproxy`；`--browser` 是可选的现有 Chromium/Chrome 路径，不会自动下载浏览器。烟测覆盖 requests/curl 的 HTTP、HTTPS、SOCKS5 入口及 Chromium 的已认证 HTTP 入口，结束后关闭程序并清理自己的 Redis 前缀。

## 运行边界

- Redis 自身重启能否保留 stick 绑定取决于其持久化配置。Rust 重启不会主动删除绑定。
- Redis 失联后停止新分配，活跃连接续租失败会终止，避免租约过期后仍占用已重新分配的出口。
- 本轮不提供供应商采购、综合质量评分、目标站点登录管理或 Web 管理后台。

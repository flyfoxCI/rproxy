"""Drive the built binary with requests, curl and optional Chromium, using local fake upstreams."""
import argparse
import asyncio
import os
import pathlib
import re
import secrets
import shutil
import subprocess
import tempfile
import time
from urllib.parse import quote

import redis
import requests


async def fake_upstream(reader, writer):
    try:
        while True:
            header = await reader.readuntil(b"\r\n\r\n")
            line = header.split(b"\r\n", 1)[0]
            if line.startswith(b"CONNECT "):
                writer.write(b"HTTP/1.1 200 Established\r\n\r\n")
                await writer.drain()
                continue
            if b"/ip " in line:
                body = b"192.0.2.10"
            else:
                body = b"<!doctype html><title>rproxy smoke</title><p>proxy-ok</p>"
            writer.write(b"HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: "
                         + str(len(body)).encode() + b"\r\nConnection: close\r\n\r\n" + body)
            await writer.drain()
            return
    except (asyncio.IncompleteReadError, ConnectionError):
        pass
    finally:
        writer.close()
        await writer.wait_closed()


class VerifiedHttpsProxyAdapter(requests.adapters.HTTPAdapter):
    # requests otherwise sets CERT_NONE when the target URL is plain HTTP,
    # even though the connection to this proxy uses TLS.
    def cert_verify(self, connection, url, verify, cert):
        super().cert_verify(connection, url.replace("http://", "https://", 1), verify, cert)


def clients(ports, password, certificate, browser_path):
    for scheme in ["http", "https", "socks5h"]:
        username = "v1|smoke|requests|client|stick"
        host = "localhost" if scheme == "https" else "127.0.0.1"
        url = f"{scheme}://{quote(username, safe='')}:{quote(password, safe='')}@{host}:{ports[scheme]}"
        with requests.Session() as client:
            client.trust_env = False
            if scheme == "https":
                client.mount("http://", VerifiedHttpsProxyAdapter())
            response = client.get("http://smoke.invalid/", proxies={"http": url}, verify=str(certificate), timeout=10)
            assert response.status_code == 200 and "proxy-ok" in response.text, (scheme, response.status_code)
        print(f"PASS requests via {scheme}", flush=True)
    for scheme in ["http", "https", "socks5h"]:
        command = [shutil.which("curl"), "--silent", "--show-error", "--fail", "--max-time", "10", "--noproxy", "",
                   "--proxy", f"{scheme}://{'localhost' if scheme == 'https' else '127.0.0.1'}:{ports[scheme]}", "--proxy-user", f"v1|smoke|curl|client|stick:{password}"]
        if scheme == "https":
            command.extend(["--proxy-cacert", str(certificate)])
        response = subprocess.run(command + ["http://smoke.invalid/"], capture_output=True, timeout=15)
        assert response.returncode == 0 and b"proxy-ok" in response.stdout, (scheme, response.stderr.decode(errors="replace"))
        print(f"PASS curl via {scheme}", flush=True)
    if browser_path:
        from playwright.sync_api import sync_playwright
        with sync_playwright() as playwright:
            browser = playwright.chromium.launch(executable_path=browser_path, headless=True,
                args=["--disable-background-networking"], proxy={
                    "server": f"http://127.0.0.1:{ports['http']}",
                    "username": "v1|smoke|browser|client|stick", "password": password})
            try:
                page = browser.new_page()
                response = page.goto("http://smoke.invalid/", timeout=15000)
                assert response.status == 200 and page.title() == "rproxy smoke"
                assert "proxy-ok" in page.locator("body").inner_text()
                print("PASS Chromium/Playwright via authenticated HTTP proxy", flush=True)
            finally:
                browser.close()


async def main(args):
    binary = pathlib.Path(args.binary).resolve()
    upstream = await asyncio.start_server(fake_upstream, "127.0.0.1", 0)
    address = upstream.sockets[0].getsockname()
    prefix = "smoke-test-" + secrets.token_hex(8)
    password = secrets.token_urlsafe(24)
    connection = redis.Redis.from_url(args.redis_url)
    connection.hset(prefix + ":http", f"127.0.0.1:{address[1]}", "{}")
    process = None
    try:
        with tempfile.TemporaryDirectory(prefix="rproxy-smoke-") as directory:
            root = pathlib.Path(directory)
            certificate, key = root / "cert.pem", root / "key.pem"
            subprocess.run([shutil.which("openssl"), "req", "-x509", "-newkey", "rsa:2048", "-nodes",
                "-keyout", str(key), "-out", str(certificate), "-days", "1", "-subj", "/CN=localhost",
                "-addext", "subjectAltName=DNS:localhost,IP:127.0.0.1"], check=True, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
            config = root / "proxy.toml"
            config.write_text(f'''redis_url = "{args.redis_url}"
redis_key = "{prefix}"
http_bind = "127.0.0.1:0"
socks5_bind = "127.0.0.1:0"
https_bind = "127.0.0.1:0"
[tls]
certificate = "cert.pem"
private_key = "key.pem"
[[auth.users]]
name = "smoke"
password = "{password}"
businesses = ["requests", "curl", "browser"]
[probe]
url = "http://smoke.invalid/ip"
tunnel_url = "http://smoke.invalid/"
''', encoding="utf-8")
            log_path = root / "server.log"
            with log_path.open("w", encoding="utf-8") as log:
                environment = {k: v for k, v in os.environ.items() if k not in ["RPROXY_REDIS_URL", "RPROXY_REDIS_KEY"]}
                process = subprocess.Popen([str(binary), "--config", str(config)], stdout=log, stderr=log, env=environment)
                ports = {}
                deadline = time.monotonic() + 20
                while time.monotonic() < deadline:
                    await asyncio.sleep(0.05)
                    if process.poll() is not None:
                        raise RuntimeError(log_path.read_text(encoding="utf-8"))
                    text = log_path.read_text(encoding="utf-8")
                    ports = {scheme: int(port) for scheme, port in re.findall(r"listening protocol=(\w+) address=127\.0\.0\.1:(\d+)", text)}
                    if len(ports) == 3 and connection.hexists(prefix + ":verified", f"http:127.0.0.1:{address[1]}"):
                        break
                assert len(ports) == 3, "listeners did not become ready"
                ports["socks5h"] = ports["socks5"]
                try:
                    await asyncio.to_thread(clients, ports, password, certificate, args.browser)
                except Exception:
                    print(log_path.read_text(encoding="utf-8"))
                    raise
                finally:
                    process.terminate()
                    await asyncio.to_thread(process.wait, 5)
                    process = None
    finally:
        if process is not None:
            process.kill()
            process.wait()
        upstream.close()
        await upstream.wait_closed()
        keys = list(connection.scan_iter(prefix + ":*"))
        if keys:
            connection.delete(*keys)
        connection.close()


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", default="target/debug/rproxy.exe" if os.name == "nt" else "target/debug/rproxy")
    parser.add_argument("--redis-url", default=os.environ.get("RPROXY_TEST_REDIS", "redis://127.0.0.1:16379/"))
    parser.add_argument("--browser", help="existing Chromium/Chrome executable; requires playwright")
    asyncio.run(main(parser.parse_args()))

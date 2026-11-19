import asyncio
import ssl

import aiohttp
from aiohttp_socks import ProxyConnector
from tenacity import AsyncRetrying, RetryError, retry_if_exception_type, stop_after_attempt, wait_fixed

from conf import CHECK_CA, CHECK_INSECURE, CHECK_URL


async def check_proxy(proxy, protocol):
    context = False if CHECK_INSECURE else ssl.create_default_context(cafile=CHECK_CA)
    connector = (ProxyConnector.from_url(f"socks5://{proxy}", rdns=True, ssl=context)
                 if protocol == "socks5" else aiohttp.TCPConnector(ssl=context))
    try:
        async with aiohttp.ClientSession(connector=connector,
                                         timeout=aiohttp.ClientTimeout(total=10)) as session:
            async for attempt in AsyncRetrying(
                    stop=stop_after_attempt(3), wait=wait_fixed(0.1),
                    retry=retry_if_exception_type((aiohttp.ClientError, OSError, asyncio.TimeoutError))):
                with attempt:
                    options = {"proxy": f"http://{proxy}"} if protocol == "http" else {}
                    async with session.get(CHECK_URL, allow_redirects=False, **options) as response:
                        response.raise_for_status()
                        if not 200 <= response.status < 300:
                            return False, proxy
                        await response.content.read(4096)
                        return True, proxy
    except (RetryError, aiohttp.ClientError, OSError, asyncio.TimeoutError):
        pass
    return False, proxy


async def check_http(proxy):
    return await check_proxy(proxy, "http")

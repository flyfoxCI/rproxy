from tenacity import RetryError, stop_after_attempt, AsyncRetrying
import asyncio
import aiohttp
from aiosocksy import Socks5Auth
from aiosocksy.connector import ProxyConnector, ProxyClientRequest


async def check_socks(proxy):
    try:
        for attempt in AsyncRetrying(stop=stop_after_attempt(3)):
            with attempt:
                connector = ProxyConnector()
                async with aiohttp.ClientSession(connector=connector) as session:
                    async with session.get('http://baidu.com', proxy=f'socks5://{proxy}') as response:
                        await response.text()
                        return True, proxy
    except RetryError as e:
        print(e)
    return False, proxy

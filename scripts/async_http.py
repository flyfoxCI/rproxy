import re
import aiohttp
from tenacity import RetryError, stop_after_attempt, AsyncRetrying


async def check_http(proxy):
    try:
        for attempt in AsyncRetrying(stop=stop_after_attempt(3)):
            with attempt:
                async with aiohttp.ClientSession(connector=aiohttp.TCPConnector(ssl=False)) as session:
                    async with session.get('http://baidu.com', proxy=f'http://{proxy}') as response:
                        await response.text()
                        return True, proxy
    except RetryError as e:
        print(e)
    return False, proxy

from async_http import check_proxy


async def check_socks(proxy):
    return await check_proxy(proxy, "socks5")

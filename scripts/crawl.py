import argparse
import asyncio
import logging
import time

import schedule

from async_get_proxy import get_xdaili_proxy, get_shenlong_proxy
from async_http import check_http
from async_sock5 import check_socks
from conf import REDIS_KEY, LOG_LEVEL, LOG_FORMAT
from maintenance import check_many, connection, record, safe_run


class BaseCrawler:
    def __init__(self, payload_proto, concurrency, proxy_url, max_capacity):
        if concurrency < 1 or max_capacity < 1:
            raise ValueError("concurrency and capacity must be positive")
        self.payload_proto = payload_proto
        self.concurrency = concurrency
        self.conn = self.createConn()
        self.redis_key = REDIS_KEY + ":" + payload_proto
        self.url = proxy_url
        self.max_capacity = max_capacity

    def createConn(self):
        return connection()

    def getCapacity(self):
        return self.conn.hlen(self.redis_key)

    def run(self):
        if self.getCapacity() >= self.max_capacity:
            return
        proxies = list(dict.fromkeys(self.getProxyList(self.url)))
        results = asyncio.run(check_many(self.check, proxies, self.concurrency))
        arguments = [self.max_capacity]
        for healthy, proxy in results:
            if healthy:
                arguments.extend([proxy, record(proxy)])
        if len(arguments) > 1:
            self.conn.eval("""
                local added = 0
                for i = 2, #ARGV, 2 do
                    if redis.call('HLEN', KEYS[1]) >= tonumber(ARGV[1]) then break end
                    added = added + redis.call('HSETNX', KEYS[1], ARGV[i], ARGV[i+1])
                end
                return added
            """, 1, self.redis_key, *arguments)


class XdailiProxyCrawler(BaseCrawler):
    def __init__(self, concurrency, proxy_url, max_capacity):
        super().__init__("http", concurrency, proxy_url, max_capacity)

    check = staticmethod(check_http)
    getProxyList = staticmethod(get_xdaili_proxy)


class ShenLongProxyCrawler(BaseCrawler):
    def __init__(self, concurrency, proxy_url, max_capacity):
        super().__init__("socks5", concurrency, proxy_url, max_capacity)

    check = staticmethod(check_socks)
    getProxyList = staticmethod(get_shenlong_proxy)


def main():
    parser = argparse.ArgumentParser(description="Fetch and validate upstream proxies")
    parser.add_argument("protocol", choices=["http", "socks5"])
    parser.add_argument("url")
    parser.add_argument("-c", "--concurrency", type=int, default=20)
    parser.add_argument("-e", "--every", type=int, default=60, help="interval in seconds")
    parser.add_argument("-m", "--max-capacity", type=int, default=40)
    args = parser.parse_args()
    if min(args.every, args.concurrency, args.max_capacity) < 1:
        parser.error("interval, concurrency and capacity must be positive")
    logging.basicConfig(level=LOG_LEVEL, format=LOG_FORMAT)
    cls = XdailiProxyCrawler if args.protocol == "http" else ShenLongProxyCrawler
    crawler = cls(args.concurrency, args.url, args.max_capacity)
    schedule.every(args.every).seconds.do(safe_run, crawler)
    safe_run(crawler)
    try:
        while True:
            schedule.run_pending()
            time.sleep(1)
    finally:
        crawler.conn.close()


if __name__ == "__main__":
    main()

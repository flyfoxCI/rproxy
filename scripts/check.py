import argparse
import asyncio
import logging
import time

import schedule

from async_http import check_http
from async_sock5 import check_socks
from conf import REDIS_KEY, LOG_LEVEL, LOG_FORMAT
from maintenance import check_many, connection, record, safe_run


class BaseCheck:
    def __init__(self, payload_proto, concurrency):
        if concurrency < 1:
            raise ValueError("concurrency must be positive")
        self.payload_proto = payload_proto
        self.concurrency = concurrency
        self.conn = connection()
        self.redis_key = REDIS_KEY + ":" + payload_proto

    def getProxyList(self, redis_key):
        return self.conn.hgetall(redis_key)

    def run(self):
        snapshot = self.getProxyList(self.redis_key)
        proxies = [proxy.decode() for proxy in snapshot]
        results = asyncio.run(check_many(self.check, proxies, self.concurrency))
        script = """
            if redis.call('HGET', KEYS[1], ARGV[1]) ~= ARGV[2] then return 0 end
            if ARGV[3] == 'delete' then return redis.call('HDEL', KEYS[1], ARGV[1]) end
            redis.call('HSET', KEYS[1], ARGV[1], ARGV[3])
            return 1
        """
        with self.conn.pipeline(transaction=False) as pipeline:
            for healthy, proxy in results:
                pipeline.eval(script, 1, self.redis_key, proxy, snapshot[proxy.encode()],
                              record(proxy) if healthy else "delete")
            pipeline.execute()


class HTTPProxyCheck(BaseCheck):
    def __init__(self, concurrency):
        super().__init__("http", concurrency)
    check = staticmethod(check_http)


class Socsk5ProxyCheck(BaseCheck):
    def __init__(self, concurrency):
        super().__init__("socks5", concurrency)
    check = staticmethod(check_socks)


def main():
    parser = argparse.ArgumentParser(description="Check and prune upstream proxies")
    parser.add_argument("protocol", choices=["http", "socks5"])
    parser.add_argument("-c", "--concurrency", type=int, default=60)
    parser.add_argument("-e", "--every", type=int, default=60, help="interval in seconds")
    args = parser.parse_args()
    if min(args.every, args.concurrency) < 1:
        parser.error("interval and concurrency must be positive")
    logging.basicConfig(level=LOG_LEVEL, format=LOG_FORMAT)
    cls = HTTPProxyCheck if args.protocol == "http" else Socsk5ProxyCheck
    checker = cls(args.concurrency)
    schedule.every(args.every).seconds.do(safe_run, checker)
    safe_run(checker)
    try:
        while True:
            schedule.run_pending()
            time.sleep(1)
    finally:
        checker.conn.close()


if __name__ == "__main__":
    main()

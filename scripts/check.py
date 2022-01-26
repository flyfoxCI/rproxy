import json
import logging
import optparse
import re
import sys
import time
from datetime import datetime
import redis
import schedule

from async_http import check_http
from aio_pool import AioPool

from async_sock5 import check_socks
from conf import REDIS_HOST, REDIS_PORT, REDIS_PASSWORD, REDIS_KEY, LOG_LEVEL, LOG_FORMAT


class BaseCheck(object):
    HTTP_HOST = "www.baidu.com"
    HTTP_PATTERN = re.compile("^HTTP\/1\.\d ([0-9]{3}) .*")

    def __init__(self, payload_proto, concurrency):
        self.payload_proto = payload_proto
        self.concurrency = concurrency
        self.conn = self.createConn()
        self.redis_key = REDIS_KEY + ":" + payload_proto
        self.pool = AioPool(processes=4, concurrency_limit=self.concurrency)

    def createConn(self):
        conn = redis.Redis(host=REDIS_HOST, port=REDIS_PORT, password=REDIS_PASSWORD)
        return conn

    def run(self):
        proxy_list = self.getProxyList(self.redis_key)
        proxy_list = [p.decode() for p in proxy_list]
        results = self.pool.map(self.check, [p for p in proxy_list])
        to_delete = []
        to_update = {}
        for r in results:
            flag, proxy = r
            if flag:
                to_update[proxy] = json.dumps({'private_ip': proxy,
                                               'ts': datetime.now().strftime('%Y-%m-%d %H:%M:%S')})
            else:
                to_delete.append(proxy)
        if len(to_delete) > 0:
            self.conn.hdel(self.redis_key, *to_delete)
        if len(to_update.keys()) > 0:
            self.conn.hmset(self.redis_key, mapping=to_update)


class HTTPProxyCheck(BaseCheck):

    def __init__(self, concurrency):
        BaseCheck.__init__(self, "http", concurrency)

    @staticmethod
    async def check(proxy):
        return await check_http(proxy)

    def getProxyList(self, redis_key):
        return self.conn.hgetall(redis_key)


class Socsk5ProxyCheck(BaseCheck):

    def __init__(self, concurrency):
        BaseCheck.__init__(self, "socks5", concurrency)

    @staticmethod
    async def check(proxy):
        return await check_socks(proxy)

    def getProxyList(self, redis_key):
        return self.conn.hgetall(redis_key)


def usage():
    return "Usage: %s <http|http_tunnel|socks5> [options]" % (sys.argv[0])


if __name__ == '__main__':
    parser = optparse.OptionParser(usage=usage())
    parser.add_option("-c", "--concurrency", action="store", dest="concurrency", type="int",
                      default=60)
    parser.add_option("-e", "--every", action="store", dest="every", type="int", default=1,
                      help="run check every %default minutes")
    options, args = parser.parse_args()

    logging.basicConfig(level=LOG_LEVEL, format=LOG_FORMAT)

    # if len(sys.argv) < 2:
    #     print(usage())
    #     sys.exit(1)

    proto = args[0]
    checker = None
    if proto == "http":
        checker = HTTPProxyCheck(options.concurrency)
    elif proto == "socks5":
        checker = Socsk5ProxyCheck(options.concurrency)
    else:
        logging.error("proto not support")
        sys.exit(-1)
    schedule.every(options.every).minutes.do(checker.run).run()
    while True:
        schedule.run_pending()
        time.sleep(1)
    # checker = HTTPProxyCheck(10)
    # checker.run()

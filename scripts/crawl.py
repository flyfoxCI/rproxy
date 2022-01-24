import logging
import optparse
import sys
import time
from datetime import datetime
import json
import re
import redis
import schedule
from aio_pool import AioPool

from async_get_proxy import get_xdaili_proxy, get_shenlong_proxy
from async_http import check_http
from conf import REDIS_KEY, REDIS_HOST, REDIS_PORT, REDIS_PASSWORD, LOG_LEVEL, LOG_FORMAT


class BaseCrawler(object):
    HTTP_HOST = "www.baidu.com"
    HTTP_PATTERN = re.compile("^HTTP\/1\.\d ([0-9]{3}) .*")

    def __init__(self, payload_proto, concurrency, proxy_url, max_capacity):
        self.payload_proto = payload_proto
        self.concurrency = concurrency
        self.conn = self.createConn()
        self.redis_key = REDIS_KEY + ":" + payload_proto
        self.url = proxy_url
        self.max_capacity = max_capacity

    def createConn(self):
        conn = redis.Redis(host=REDIS_HOST, port=REDIS_PORT, password=REDIS_PASSWORD)
        return conn

    def getCapacity(self):
        proxies = self.conn.hkeys(self.redis_key)
        return len(proxies)

    def run(self):
        proxies_cap = self.getCapacity()
        if proxies_cap > self.max_capacity:
            return
        pool = AioPool(processes=1, concurrency_limit=self.concurrency)
        proxy_list = self.getProxyList(self.url)
        #print(proxy_list)
        results = pool.map(self.check, [p for p in proxy_list])
        to_add = {}
        for r in results:
            flag, proxy = r
            if flag:
                to_add[proxy] = json.dumps({'private_ip': proxy,
                                            'ts': datetime.now().strftime('%Y-%m-%d %H:%M:%S')})
        if len(to_add) > 0:
            self.conn.hmset(self.redis_key, mapping=to_add)


class XdailiProxyCrawler(BaseCrawler):

    def __init__(self, concurrency, proxy_url,max_capacity):
        BaseCrawler.__init__(self, "http", concurrency, proxy_url,max_capacity)

    @staticmethod
    async def check(proxy):
        return await check_http(proxy)

    @staticmethod
    def getProxyList(proxy_url):
        return get_xdaili_proxy(proxy_url)


class ShenLongProxyCrawler(BaseCrawler):

    def __init__(self, concurrency, proxy_url,max_capacity):
        BaseCrawler.__init__(self, "socks5", concurrency, max_capacity)

    @staticmethod
    async def check(proxy):
        return await check_http(proxy)

    @staticmethod
    def getProxyList(proxy_url):
        return get_shenlong_proxy(proxy_url)


def usage():
    return "Usage: %s <http|socks5> <url> [options]" % (sys.argv[0])


if __name__ == '__main__':
    parser = optparse.OptionParser(usage=usage())
    parser.add_option("-c", "--concurrency", action="store", dest="concurrency", type="int",
                      default=20)
    parser.add_option("-e", "--every", action="store", dest="every", type="int", default=1,
                      help="run check every %default minutes")
    parser.add_option("-m", "--max-capacity", action="store", dest="capacity", type="int", default=40,
                      help="proxy max capacity %sdefault ")
    options, args = parser.parse_args()

    logging.basicConfig(level=LOG_LEVEL, format=LOG_FORMAT)

    if len(sys.argv) < 2:
        print(usage())
        sys.exit(1)

    proto = args[0]
    url = args[1]
    checker = None

    if proto == "http":
        checker = XdailiProxyCrawler(options.concurrency, url, options.capacity)
    elif proto == "socks5":
        checker = ShenLongProxyCrawler(options.concurrency, url, options.capacity)
    else:
        logging.error("proto not support")
        sys.exit(-1)
    schedule.every(options.every).seconds.do(checker.run).run()
    while True:
        schedule.run_pending()
        time.sleep(1)

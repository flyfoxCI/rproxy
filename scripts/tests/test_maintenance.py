import asyncio
import os
import pathlib
import sys
import threading
import unittest
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parents[1]))
import redis
from crawl import BaseCrawler, ShenLongProxyCrawler
from async_http import check_http


class MaintenanceTests(unittest.TestCase):
    def setUp(self):
        self.conn = redis.Redis.from_url(os.environ.get('RPROXY_TEST_REDIS', 'redis://127.0.0.1:16379/'))
        self.key = 'python-test:' + os.urandom(8).hex()

    def tearDown(self):
        self.conn.delete(self.key)
        self.conn.close()

    def crawler(self, capacity):
        connection, key = self.conn, self.key
        class Crawler(BaseCrawler):
            def createConn(self):
                return connection
            @staticmethod
            def getProxyList(url):
                return ['1.1.1.1:80', '2.2.2.2:80', '3.3.3.3:80']
            @staticmethod
            async def check(proxy):
                return True, proxy
        crawler = Crawler('http', 2, 'http://supplier', capacity)
        crawler.redis_key = key
        return crawler

    def test_capacity_is_a_hard_limit_and_equal_capacity_does_not_add(self):
        self.conn.hset(self.key, '9.9.9.9:80', '{}')
        self.crawler(2).run()
        self.assertEqual(self.conn.hlen(self.key), 2)
        before = self.conn.hgetall(self.key)
        self.crawler(2).run()
        self.assertEqual(self.conn.hgetall(self.key), before)

    def test_socks_crawler_keeps_supplier_url(self):
        crawler = ShenLongProxyCrawler(2, 'http://supplier', 4)
        self.assertEqual(crawler.url, 'http://supplier')
        self.assertEqual(crawler.payload_proto, 'socks5')
        crawler.conn.close()

    def test_http_check_rejects_errors_and_retries_only_bounded_times(self):
        class Handler(BaseHTTPRequestHandler):
            calls = 0
            def do_GET(self):
                type(self).calls += 1
                self.send_response(503)
                self.end_headers()
                self.wfile.write(b'not healthy')
            do_CONNECT = do_GET
            def log_message(self, *args):
                pass
        server = ThreadingHTTPServer(('127.0.0.1', 0), Handler)
        thread = threading.Thread(target=server.serve_forever, daemon=True)
        thread.start()
        try:
            proxy = '127.0.0.1:' + str(server.server_port)
            result = asyncio.run(check_http(proxy))
            self.assertEqual(result, (False, proxy))
            self.assertEqual(Handler.calls, 3)
        finally:
            server.shutdown()
            server.server_close()
            thread.join()


if __name__ == '__main__':
    unittest.main()

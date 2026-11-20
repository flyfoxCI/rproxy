"""Local-only supplier, exit-check endpoint and HTTP upstream for Compose smoke tests."""
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import select
import socket
from urllib.parse import urlsplit


class Handler(BaseHTTPRequestHandler):
    protocol_version = 'HTTP/1.1'

    def do_GET(self):
        path = urlsplit(self.path).path
        ip = socket.gethostbyname(socket.gethostname())
        body = (f'{ip}:18080\n' if path == '/supplier' else
                ip if path == '/ip' else 'mounted-compose-ok').encode()
        self.send_response(200)
        self.send_header('Content-Length', str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_CONNECT(self):
        # Never turn this test fixture into an arbitrary outbound proxy.
        if self.path != 'fixture:18080':
            self.send_error(403)
            return
        with socket.create_connection(('127.0.0.1', 18080), timeout=5) as upstream:
            self.send_response(200)
            self.end_headers()
            while True:
                readable, _, _ = select.select([self.connection, upstream], [], [], 10)
                if not readable:
                    return
                for source in readable:
                    data = source.recv(65536)
                    if not data:
                        return
                    (upstream if source is self.connection else self.connection).sendall(data)

    def log_message(self, *_):
        pass


if __name__ == '__main__':
    ThreadingHTTPServer(('0.0.0.0', 18080), Handler).serve_forever()

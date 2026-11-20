"""Local-only extraction, exit check, HTTP and SOCKS5 upstreams for Compose smoke."""
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import select
import socket
import socketserver
import threading
from urllib.parse import urlsplit


def relay(left, right):
    while True:
        readable, _, _ = select.select([left, right], [], [], 10)
        if not readable:
            return
        for source in readable:
            data = source.recv(65536)
            if not data:
                return
            (right if source is left else left).sendall(data)


class Handler(BaseHTTPRequestHandler):
    protocol_version = 'HTTP/1.1'

    def do_GET(self):
        path = urlsplit(self.path).path
        ip = socket.gethostbyname(socket.gethostname())
        body = (f'{ip}:18080\n' if path == '/supplier' else
                f'{ip}:18081\n' if path == '/supplier-socks' else
                ip if path == '/ip' else 'mounted-compose-ok').encode()
        self.send_response(200)
        self.send_header('Content-Length', str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_CONNECT(self):
        if self.path != 'fixture:18080':
            self.send_error(403)
            return
        with socket.create_connection(('127.0.0.1', 18080), timeout=5) as upstream:
            self.send_response(200)
            self.end_headers()
            relay(self.connection, upstream)

    def log_message(self, *_):
        pass


def receive(stream, size):
    result = bytearray()
    while len(result) < size:
        data = stream.recv(size - len(result))
        if not data:
            raise ConnectionError('fixture stream closed')
        result.extend(data)
    return bytes(result)


class SocksHandler(socketserver.BaseRequestHandler):
    def handle(self):
        stream = self.request
        stream.settimeout(10)
        try:
            version, count = receive(stream, 2)
            methods = receive(stream, count)
            if version != 5 or 0 not in methods:
                stream.sendall(b'\x05\xff')
                return
            stream.sendall(b'\x05\x00')
            version, command, reserved, kind = receive(stream, 4)
            if kind == 1:
                host = socket.inet_ntoa(receive(stream, 4))
            elif kind == 3:
                host = receive(stream, receive(stream, 1)[0]).decode('ascii')
            else:
                stream.sendall(b'\x05\x08\x00\x01' + b'\x00' * 6)
                return
            port = int.from_bytes(receive(stream, 2), 'big')
            own_ip = socket.gethostbyname(socket.gethostname())
            # This is never a general-purpose outbound proxy.
            if version != 5 or command != 1 or reserved != 0 or port != 18080 or host not in ('fixture', '127.0.0.1', own_ip):
                stream.sendall(b'\x05\x02\x00\x01' + b'\x00' * 6)
                return
            with socket.create_connection(('127.0.0.1', 18080), timeout=5) as upstream:
                host, port = upstream.getsockname()
                stream.sendall(b'\x05\x00\x00\x01' + socket.inet_aton(host) + port.to_bytes(2, 'big'))
                relay(stream, upstream)
        except (OSError, ValueError, ConnectionError):
            return


class SocksServer(socketserver.ThreadingTCPServer):
    allow_reuse_address = True
    daemon_threads = True


if __name__ == '__main__':
    socks = SocksServer(('0.0.0.0', 18081), SocksHandler)
    threading.Thread(target=socks.serve_forever, daemon=True).start()
    ThreadingHTTPServer(('0.0.0.0', 18080), Handler).serve_forever()

import http.client
import json
import os
import pathlib
import socket
import subprocess
import threading
import time
import unittest
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer


ROOT = pathlib.Path(__file__).resolve().parents[1]
DEFAULT_GATEWAY_BIN = ROOT / "desktop" / "gateway" / "target" / "debug" / "csswitch-gateway"
STAGED_GATEWAY_DIR = ROOT / "desktop" / "src-tauri" / "binaries"


def free_port():
    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as sock:
        sock.bind(("127.0.0.1", 0))
        port = sock.getsockname()[1]
    if port == 8765:
        return free_port()
    return port


def gateway_bin():
    raw = os.environ.get("CSSWITCH_GATEWAY_BIN")
    if raw and pathlib.Path(raw).is_file():
        return pathlib.Path(raw)
    if DEFAULT_GATEWAY_BIN.is_file():
        return DEFAULT_GATEWAY_BIN
    for path in sorted(STAGED_GATEWAY_DIR.glob("csswitch-gateway-*")):
        if path.is_file():
            return path
    return None


def recv_http_head(sock):
    data = b""
    while b"\r\n\r\n" not in data:
        chunk = sock.recv(4096)
        if not chunk:
            break
        data += chunk
    return data


def recv_http_all(sock):
    chunks = []
    while True:
        try:
            chunk = sock.recv(65536)
        except ConnectionResetError:
            break
        if not chunk:
            break
        chunks.append(chunk)
    return b"".join(chunks)


def parse_raw_response(raw):
    head, _, body = raw.partition(b"\r\n\r\n")
    lines = head.split(b"\r\n")
    status = int(lines[0].split()[1])
    headers = {}
    for line in lines[1:]:
        key, _, value = line.partition(b":")
        if key:
            headers[key.strip().lower().decode()] = value.strip().decode()
    return status, headers, body


def assert_error_shape(testcase, body, error_type):
    parsed = json.loads(body)
    testcase.assertEqual(parsed["type"], "error")
    testcase.assertEqual(parsed["error"]["type"], error_type)
    testcase.assertIsInstance(parsed["error"]["message"], str)
    return parsed


class MockUpstream(ThreadingHTTPServer):
    allow_reuse_address = True

    def __init__(
        self,
        response_body,
        content_type="application/json",
        status=200,
        response_delay=0,
    ):
        self.requests = []
        self.response_body = response_body
        self.content_type = content_type
        self.status = status
        self.response_delay = response_delay
        super().__init__(("127.0.0.1", free_port()), MockHandler)


class MockHandler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def do_POST(self):
        length = int(self.headers.get("content-length", "0"))
        body = self.rfile.read(length)
        self.server.requests.append(
            {
                "path": self.path,
                "headers": {k.lower(): v for k, v in self.headers.items()},
                "body": body,
            }
        )
        payload = self.server.response_body
        if self.server.response_delay:
            time.sleep(self.server.response_delay)
        self.send_response(self.server.status)
        self.send_header("content-type", self.server.content_type)
        self.send_header("content-length", str(len(payload)))
        self.end_headers()
        self.wfile.write(payload)

    def log_message(self, *_args):
        pass


class EchoServer:
    def __init__(self):
        self.port = free_port()
        self.ready = threading.Event()
        self.thread = threading.Thread(target=self._serve, daemon=True)
        self.thread.start()
        self.ready.wait(2)

    def _serve(self):
        with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as srv:
            srv.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
            srv.bind(("127.0.0.1", self.port))
            srv.listen(1)
            self.ready.set()
            conn, _ = srv.accept()
            with conn:
                data = conn.recv(4096)
                conn.sendall(data)


class RawUpstream:
    def __init__(self, handler):
        self.port = free_port()
        self.handler = handler
        self.ready = threading.Event()
        self.closed = False
        self.thread = threading.Thread(target=self._serve, daemon=True)
        self.thread.start()
        self.ready.wait(2)

    def _serve(self):
        with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as srv:
            srv.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
            srv.bind(("127.0.0.1", self.port))
            srv.listen(5)
            self.ready.set()
            while not self.closed:
                try:
                    conn, _ = srv.accept()
                except OSError:
                    return
                threading.Thread(target=self.handler, args=(conn,), daemon=True).start()

    @property
    def url(self):
        return f"http://127.0.0.1:{self.port}/anthropic/v1/messages"

    def close(self):
        self.closed = True
        try:
            with socket.create_connection(("127.0.0.1", self.port), timeout=0.2):
                pass
        except OSError:
            pass


def delayed_stream_handler(conn):
    with conn:
        conn.recv(65536)
        time.sleep(1.2)
        head = (
            "HTTP/1.1 200 OK\r\n"
            "Content-Type: text/event-stream\r\n"
            "Transfer-Encoding: chunked\r\n\r\n"
        )
        first = b"event: message_start\n"
        conn.sendall(head.encode() + b"15\r\n" + first + b"\r\n0\r\n\r\n")


def dropping_stream_handler(conn):
    with conn:
        conn.recv(65536)
        payload = b"event: content_block_delta\ndata: {\"type\":\"content_block_delta\"}\n\n"
        head = (
            "HTTP/1.1 200 OK\r\n"
            "Content-Type: text/event-stream\r\n"
            "Transfer-Encoding: chunked\r\n\r\n"
        )
        try:
            conn.sendall(
                head.encode() + hex(len(payload))[2:].encode() + b"\r\n" + payload + b"\r\n"
            )
            conn.sendall(b"1f4\r\n0123456789")
        except BrokenPipeError:
            pass


class RustGatewayLoopback(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.bin = gateway_bin()
        if cls.bin is None:
            raise unittest.SkipTest("csswitch-gateway binary not built")

    def start_gateway(self, upstream_url=None, secret="secret"):
        port = free_port()
        env = os.environ.copy()
        env.update(
            {
                "DEEPSEEK_API_KEY": "fake-deepseek-key",
                "CSSWITCH_AUTH_TOKEN": secret,
                "CSSWITCH_TOOLUSE_SHIM": "off",
            }
        )
        if upstream_url:
            env["CSSWITCH_UPSTREAM_URL"] = upstream_url
        proc = subprocess.Popen(
            [
                str(self.bin),
                "--provider",
                "deepseek",
                "--port",
                str(port),
                "--auth-token",
                "cli-secret-should-lose",
            ],
            env=env,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
        )
        deadline = time.time() + 5
        while time.time() < deadline:
            try:
                conn = http.client.HTTPConnection("127.0.0.1", port, timeout=0.2)
                conn.request("GET", f"/{secret}/health")
                resp = conn.getresponse()
                resp.read()
                conn.close()
                if resp.status == 200:
                    return proc, port
            except OSError:
                time.sleep(0.05)
        proc.terminate()
        stderr = ""
        try:
            _, stderr = proc.communicate(timeout=1)
        except Exception:
            proc.kill()
        raise RuntimeError(f"gateway did not become healthy: {stderr}")

    def stop_gateway(self, proc):
        proc.terminate()
        try:
            proc.wait(timeout=3)
        except subprocess.TimeoutExpired:
            proc.kill()
            proc.wait(timeout=3)
        for handle in (proc.stdout, proc.stderr):
            if handle:
                handle.close()

    def raw_request(self, port, request):
        with socket.create_connection(("127.0.0.1", port), timeout=5) as sock:
            sock.sendall(request)
            return recv_http_all(sock)

    def raw_post_until(self, port, body, needle, timeout=1.1):
        with socket.create_connection(("127.0.0.1", port), timeout=5) as sock:
            sock.settimeout(timeout)
            request = (
                "POST /secret/v1/messages HTTP/1.1\r\n"
                "Host: 127.0.0.1\r\n"
                "Content-Type: application/json\r\n"
                f"Content-Length: {len(body)}\r\n"
                "Connection: close\r\n\r\n"
            ).encode() + body
            started = time.monotonic()
            sock.sendall(request)
            chunks = []
            try:
                while needle not in b"".join(chunks):
                    chunk = sock.recv(65536)
                    if not chunk:
                        break
                    chunks.append(chunk)
            except socket.timeout:
                pass
            return b"".join(chunks), time.monotonic() - started

    def test_auth_and_models(self):
        proc, port = self.start_gateway()
        try:
            conn = http.client.HTTPConnection("127.0.0.1", port, timeout=2)
            conn.request("GET", "/v1/models")
            forbidden = conn.getresponse()
            self.assertEqual(forbidden.status, 403)
            forbidden_body = json.loads(forbidden.read())
            conn.close()
            self.assertEqual(forbidden_body["type"], "error")
            self.assertEqual(forbidden_body["error"]["type"], "permission_error")

            conn = http.client.HTTPConnection("127.0.0.1", port, timeout=2)
            conn.request("GET", "/secret/v1/models")
            resp = conn.getresponse()
            body = json.loads(resp.read())
            conn.close()
            self.assertEqual(resp.status, 200)
            self.assertEqual(body["first_id"], "claude-opus-4-8")
            self.assertEqual(body["last_id"], "claude-haiku-4-5")

            conn = http.client.HTTPConnection("127.0.0.1", port, timeout=2)
            conn.request("GET", "/secret/health")
            resp = conn.getresponse()
            body = json.loads(resp.read())
            conn.close()
            self.assertEqual(resp.status, 200)
            self.assertEqual(body, {"status": "ok", "provider": "deepseek", "gateway": "rust"})
        finally:
            self.stop_gateway(proc)

    def test_nonstream_maps_request_and_preserves_content_length(self):
        upstream = MockUpstream(b'{"id":"msg_mock","type":"message"}')
        thread = threading.Thread(target=upstream.serve_forever, daemon=True)
        thread.start()
        proc, port = self.start_gateway(
            upstream_url=f"http://127.0.0.1:{upstream.server_port}/anthropic/v1/messages"
        )
        try:
            request = {
                "model": "claude-opus-4-8",
                "max_tokens": 100000,
                "thinking": {"type": "auto"},
                "messages": [{"role": "user", "content": "hi"}],
            }
            raw = json.dumps(request).encode()
            conn = http.client.HTTPConnection("127.0.0.1", port, timeout=5)
            conn.request(
                "POST",
                "/secret/v1/messages",
                body=raw,
                headers={"content-type": "application/json"},
            )
            resp = conn.getresponse()
            body = resp.read()
            conn.close()
            self.assertEqual(resp.status, 200)
            self.assertEqual(resp.getheader("content-length"), str(len(body)))
            self.assertEqual(body, b'{"id":"msg_mock","type":"message"}')

            self.assertEqual(len(upstream.requests), 1)
            req = upstream.requests[0]
            self.assertEqual(req["path"], "/anthropic/v1/messages")
            self.assertEqual(req["headers"]["x-api-key"], "fake-deepseek-key")
            self.assertEqual(req["headers"]["anthropic-version"], "2023-06-01")
            mapped = json.loads(req["body"])
            self.assertEqual(mapped["model"], "deepseek-v4-pro")
            self.assertEqual(mapped["max_tokens"], 65536)
            self.assertEqual(mapped["thinking"]["type"], "adaptive")
        finally:
            self.stop_gateway(proc)
            upstream.shutdown()
            upstream.server_close()

    def test_nonstream_upstream_errors_match_python_shape(self):
        cases = [
            (401, 401),
            (429, 429),
            (500, 502),
        ]
        upstream_body = (
            b'{"type":"error","error":{"type":"authentication_error",'
            b'"message":"mock upstream error"}}'
        )
        for upstream_status, expected_status in cases:
            with self.subTest(upstream_status=upstream_status):
                upstream = MockUpstream(upstream_body, status=upstream_status)
                thread = threading.Thread(target=upstream.serve_forever, daemon=True)
                thread.start()
                proc, port = self.start_gateway(
                    upstream_url=(
                        f"http://127.0.0.1:{upstream.server_port}"
                        "/anthropic/v1/messages"
                    )
                )
                try:
                    request_body = (
                        b'{"model":"claude-opus-4-8",'
                        b'"messages":[{"role":"user","content":"hi"}]}'
                    )
                    raw = self.raw_request(
                        port,
                        (
                            b"POST /secret/v1/messages HTTP/1.1\r\n"
                            b"Host: 127.0.0.1\r\n"
                            b"Content-Type: application/json\r\n"
                            + f"Content-Length: {len(request_body)}\r\n".encode()
                            + b"Connection: close\r\n\r\n"
                            + request_body
                        ),
                    )
                    status, headers, body = parse_raw_response(raw)
                    self.assertEqual(status, expected_status)
                    self.assertEqual(int(headers["content-length"]), len(body))
                    parsed = assert_error_shape(self, body, "api_error")
                    self.assertIn(f"upstream {upstream_status}", parsed["error"]["message"])
                finally:
                    self.stop_gateway(proc)
                    upstream.shutdown()
                    upstream.server_close()

    def test_malformed_requests_match_python_error_types(self):
        proc, port = self.start_gateway()
        try:
            cases = [
                (
                    b"POST /secret/v1/messages HTTP/1.1\r\n"
                    b"Host: 127.0.0.1\r\n"
                    b"Content-Length: nope\r\n"
                    b"Connection: close\r\n\r\n",
                    400,
                    "invalid_request_error",
                    "invalid Content-Length",
                ),
                (
                    b"POST /secret/v1/messages HTTP/1.1\r\n"
                    b"Host: 127.0.0.1\r\n"
                    b"Content-Length: -1\r\n"
                    b"Connection: close\r\n\r\n",
                    400,
                    "invalid_request_error",
                    "invalid Content-Length",
                ),
                (
                    b"POST /secret/v1/messages HTTP/1.1\r\n"
                    b"Host: 127.0.0.1\r\n"
                    b"Connection: close\r\n\r\n",
                    400,
                    "invalid_request_error",
                    "request body must be a JSON object",
                ),
                (
                    b"POST /secret/v1/messages HTTP/1.1\r\n"
                    b"Host: 127.0.0.1\r\n"
                    b"Content-Length: 0\r\n"
                    b"Connection: close\r\n\r\n",
                    400,
                    "invalid_request_error",
                    "request body must be a JSON object",
                ),
                (
                    b"POST /secret/v1/messages HTTP/1.1\r\n"
                    b"Host: 127.0.0.1\r\n"
                    b"Content-Length: 2\r\n"
                    b"Connection: close\r\n\r\n[]",
                    400,
                    "invalid_request_error",
                    "request body must be a JSON object",
                ),
                (
                    b"POST /secret/v1/messages HTTP/1.1\r\n"
                    b"Host: 127.0.0.1\r\n"
                    b"Content-Length: 17\r\n"
                    b"Connection: close\r\n\r\n{\"messages\":null}",
                    400,
                    "invalid_request_error",
                    "request body must be a JSON object",
                ),
                (
                    b"POST /secret/v1/unknown HTTP/1.1\r\n"
                    b"Host: 127.0.0.1\r\n"
                    b"Content-Length: 2\r\n"
                    b"Connection: close\r\n\r\n{}",
                    404,
                    "not_found_error",
                    "/v1/unknown",
                ),
                (
                    b"GET /secret/nope HTTP/1.1\r\n"
                    b"Host: 127.0.0.1\r\n"
                    b"Connection: close\r\n\r\n",
                    404,
                    "not_found_error",
                    "/nope",
                ),
            ]
            for raw_request, expected_status, error_type, message_part in cases:
                with self.subTest(error_type=error_type, message_part=message_part):
                    status, headers, body = parse_raw_response(
                        self.raw_request(port, raw_request)
                    )
                    self.assertEqual(status, expected_status)
                    self.assertEqual(int(headers["content-length"]), len(body))
                    parsed = assert_error_shape(self, body, error_type)
                    self.assertIn(message_part, parsed["error"]["message"])
        finally:
            self.stop_gateway(proc)

    def test_stream_passthrough_dechunks_same_payload(self):
        payload = b"event: message_start\ndata: {\"type\":\"message_start\"}\n\n"
        upstream = MockUpstream(payload, content_type="text/event-stream")
        thread = threading.Thread(target=upstream.serve_forever, daemon=True)
        thread.start()
        proc, port = self.start_gateway(
            upstream_url=f"http://127.0.0.1:{upstream.server_port}/anthropic/v1/messages"
        )
        try:
            request = {
                "model": "claude-haiku-4-5",
                "stream": True,
                "messages": [{"role": "user", "content": "hi"}],
            }
            conn = http.client.HTTPConnection("127.0.0.1", port, timeout=5)
            conn.request(
                "POST",
                "/secret/v1/messages",
                body=json.dumps(request).encode(),
                headers={"content-type": "application/json"},
            )
            resp = conn.getresponse()
            body = resp.read()
            conn.close()
            self.assertEqual(resp.status, 200)
            self.assertEqual(resp.getheader("transfer-encoding"), "chunked")
            self.assertEqual(body, payload)
        finally:
            self.stop_gateway(proc)
            upstream.shutdown()
            upstream.server_close()

    def test_stream_keepalive_opens_before_upstream_first_byte(self):
        upstream = RawUpstream(delayed_stream_handler)
        proc, port = self.start_gateway(upstream_url=upstream.url)
        try:
            body = (
                b'{"model":"claude-opus-4-8","max_tokens":10,"stream":true,'
                b'"messages":[{"role":"user","content":"hi"}]}'
            )
            raw, elapsed = self.raw_post_until(port, body, b": csswitch-keepalive")
            self.assertIn(b"HTTP/1.1 200", raw)
            self.assertIn(b"content-type: text/event-stream", raw)
            self.assertIn(b": csswitch-keepalive", raw)
            self.assertLess(elapsed, 1.1)
        finally:
            self.stop_gateway(proc)
            upstream.close()

    def test_stream_upstream_status_after_headers_is_sse_error(self):
        upstream = MockUpstream(
            b'{"error":"bad key"}',
            content_type="application/json",
            status=401,
        )
        thread = threading.Thread(target=upstream.serve_forever, daemon=True)
        thread.start()
        proc, port = self.start_gateway(
            upstream_url=f"http://127.0.0.1:{upstream.server_port}/anthropic/v1/messages"
        )
        try:
            body = (
                b'{"model":"claude-opus-4-8","max_tokens":10,"stream":true,'
                b'"messages":[{"role":"user","content":"hi"}]}'
            )
            raw = self.raw_request(
                port,
                (
                    b"POST /secret/v1/messages HTTP/1.1\r\n"
                    b"Host: 127.0.0.1\r\n"
                    b"Content-Type: application/json\r\n"
                    + f"Content-Length: {len(body)}\r\n".encode()
                    + b"Connection: close\r\n\r\n"
                    + body
                ),
            )
            head, _, tail = raw.partition(b"\r\n\r\n")
            self.assertIn(b"HTTP/1.1 200", head)
            self.assertIn(b"content-type: text/event-stream", head)
            self.assertIn(b"event: error", tail)
            self.assertIn(b'"type":"api_error"', tail)
            self.assertIn(b"upstream 401", tail)
            self.assertTrue(raw.rstrip().endswith(b"0"))
            self.assertNotIn(b"HTTP/1.1 401", raw)
        finally:
            self.stop_gateway(proc)
            upstream.shutdown()
            upstream.server_close()

    def test_stream_midstream_truncation_ends_with_sse_error(self):
        upstream = RawUpstream(dropping_stream_handler)
        proc, port = self.start_gateway(upstream_url=upstream.url)
        try:
            body = (
                b'{"model":"claude-opus-4-8","max_tokens":10,"stream":true,'
                b'"messages":[{"role":"user","content":"hi"}]}'
            )
            raw = self.raw_request(
                port,
                (
                    b"POST /secret/v1/messages HTTP/1.1\r\n"
                    b"Host: 127.0.0.1\r\n"
                    b"Content-Type: application/json\r\n"
                    + f"Content-Length: {len(body)}\r\n".encode()
                    + b"Connection: close\r\n\r\n"
                    + body
                ),
            )
            head, _, tail = raw.partition(b"\r\n\r\n")
            self.assertIn(b"HTTP/1.1 200", head)
            self.assertIn(b"event: content_block_delta", tail)
            self.assertIn(b"event: error", tail)
            self.assertIn(b'"type":"api_error"', tail)
            self.assertTrue(raw.rstrip().endswith(b"0"))
        finally:
            self.stop_gateway(proc)
            upstream.close()

    def test_connect_blocks_claude_hosts_and_tunnels_other_hosts(self):
        proc, port = self.start_gateway()
        try:
            with socket.create_connection(("127.0.0.1", port), timeout=2) as sock:
                sock.sendall(b"CONNECT claude.ai:443 HTTP/1.1\r\nhost: claude.ai:443\r\n\r\n")
                head = recv_http_head(sock)
            self.assertIn(b"401", head.split(b"\r\n", 1)[0])

            echo = EchoServer()
            with socket.create_connection(("127.0.0.1", port), timeout=2) as sock:
                target = f"CONNECT 127.0.0.1:{echo.port} HTTP/1.1\r\nhost: 127.0.0.1:{echo.port}\r\n\r\n"
                sock.sendall(target.encode())
                data = recv_http_head(sock)
                self.assertIn(b"200", data.split(b"\r\n", 1)[0])
                sock.sendall(b"ping")
                self.assertEqual(sock.recv(4), b"ping")
        finally:
            self.stop_gateway(proc)


if __name__ == "__main__":
    unittest.main()

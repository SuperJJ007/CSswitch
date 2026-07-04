import http.client
import json
import os
import subprocess
import sys
import tempfile
import time
import unittest
import urllib.error
import urllib.request

HERE = os.path.dirname(__file__)
sys.path.insert(0, HERE)
from mock_upstream import start_mock

PROXY = os.path.join(HERE, "..", "proxy", "csswitch_proxy.py")
SEC = "s3cr3t-test-token"


def _req(url, method="GET", body=None):
    data = json.dumps(body).encode() if body is not None else None
    r = urllib.request.Request(url, data=data, method=method,
                              headers={"Content-Type": "application/json"})
    try:
        with urllib.request.urlopen(r, timeout=5) as resp:
            return resp.status, resp.read()
    except urllib.error.HTTPError as e:
        return e.code, e.read()


class ProxyAuth(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.up_url, cls.hits, cls.stop_up = start_mock("json")
        port = 18990
        cls.base = f"http://127.0.0.1:{port}"
        cls.logf = os.path.join(tempfile.gettempdir(), f"csswitch-auth-{port}.log")
        open(cls.logf, "w").close()
        env = dict(os.environ, DEEPSEEK_API_KEY="fake-key",
                   CSSWITCH_UPSTREAM_URL=cls.up_url)
        cls.proc = subprocess.Popen(
            [sys.executable, PROXY, "--provider", "deepseek",
             "--port", str(port), "--auth-token", SEC, "--log", cls.logf],
            env=env, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        for _ in range(50):
            try:
                s, _b = _req(f"{cls.base}/{SEC}/health")
                if s == 200:
                    break
            except Exception:
                pass
            time.sleep(0.1)

    @classmethod
    def tearDownClass(cls):
        cls.proc.terminate()
        cls.proc.wait(timeout=5)
        cls.stop_up()

    def test_health_with_secret_ok(self):
        s, _b = _req(f"{self.base}/{SEC}/health")
        self.assertEqual(s, 200)

    def test_health_without_secret_ok(self):
        # /health 免鉴权（自 #12 起），便于监控探活不依赖 secret 一致性
        s, _b = _req(f"{self.base}/health")
        self.assertEqual(s, 200)

    def test_messages_without_secret_forbidden_and_upstream_untouched(self):
        before = len(self.hits)
        s, _b = _req(f"{self.base}/v1/messages", "POST",
                     {"model": "claude-opus-4-8", "max_tokens": 10,
                      "messages": [{"role": "user", "content": "hi"}]})
        self.assertEqual(s, 403)
        self.assertEqual(len(self.hits), before)

    def test_messages_with_secret_forwarded(self):
        before = len(self.hits)
        s, b = _req(f"{self.base}/{SEC}/v1/messages", "POST",
                    {"model": "claude-opus-4-8", "max_tokens": 10,
                     "messages": [{"role": "user", "content": "hi"}]})
        self.assertEqual(s, 200)
        self.assertEqual(len(self.hits), before + 1)
        self.assertEqual(json.loads(b)["content"][0]["text"], "ok")

    def test_secret_not_in_log(self):
        # /health 分支不调用 log()，只测它无法覆盖「secret 不落日志」这条不变量。
        # 改用 POST /v1/messages（会走 log()）之后再断言，才是对该不变量的真实覆盖。
        s, _b = _req(f"{self.base}/{SEC}/v1/messages", "POST",
                     {"model": "claude-opus-4-8", "max_tokens": 10,
                      "messages": [{"role": "user", "content": "hi"}]})
        self.assertEqual(s, 200)
        with open(self.logf) as f:
            self.assertNotIn(SEC, f.read())

    def test_unauth_post_closes_connection_no_leak_on_reuse(self):
        # 回归：鉴权失败的 POST 在读走请求体之前就返回 403。若连接保持 keep-alive，
        # 服务端下一轮会从残留 body 中间开始解析下一个请求，产出的畸形 400 错误页
        # 会把残留字节和下一条请求行拼在一起回显给客户端，可能带出路径里的 secret。
        # 用同一条 http.client.HTTPConnection 连发两个请求来复现/验证修复。
        body = json.dumps({"model": "claude-opus-4-8", "max_tokens": 10,
                           "messages": [{"role": "user", "content": "hi"}]}).encode()
        conn = http.client.HTTPConnection("127.0.0.1", 18990, timeout=5)
        received = b""
        try:
            # 第一次请求：不带 secret 前缀，触发 403，其请求体故意不被服务端读走。
            conn.request("POST", "/v1/messages", body=body,
                         headers={"Content-Type": "application/json"})
            resp = conn.getresponse()
            received += resp.read()
            self.assertEqual(resp.status, 403)
            # 核心断言：修复后 403 响应显式声明 Connection: close。
            self.assertEqual(resp.getheader("Connection"), "close")

            # 第二次请求：带 secret。若服务端未关连接（未修复），会在残留 body 上
            # 错位解析，产出的畸形 400 会把这条请求行（含 secret）回显给客户端，
            # received 里就会出现 secret 明文，下面的 assertNotIn 会抓到。
            # 已修复时，http.client 见到上一响应带 Connection: close 会自动断开
            # 重连（不会复用被污染的旧 socket），第二次请求要么在一条新连接上
            # 干净地成功，要么因服务端已关闭而抛异常；两者都不泄露 secret。
            try:
                conn.request("POST", f"/{SEC}/v1/messages", body=body,
                             headers={"Content-Type": "application/json"})
                resp2 = conn.getresponse()
                received += resp2.read()
            except Exception:
                pass
        finally:
            conn.close()
        # 核心不变量：不论第二次请求成功、以新连接重试成功还是失败，全程客户端
        # 收到的字节都不能含 secret 明文。
        self.assertNotIn(SEC.encode(), received)

    def test_malformed_content_length_returns_400(self):
        # 回归：畸形 Content-Length（非整数）以前会在 int() 抛 ValueError 击穿 handler，
        # 客户端只收到空响应/连接重置。修复后应回规范 400（invalid_request_error）。
        import socket
        payload = (
            f"POST /{SEC}/v1/messages HTTP/1.1\r\n"
            "Host: 127.0.0.1\r\n"
            "Content-Type: application/json\r\n"
            "Content-Length: oops\r\n"
            "Connection: close\r\n"
            "\r\n"
        ).encode()
        s = socket.create_connection(("127.0.0.1", 18990), timeout=5)
        try:
            s.sendall(payload)
            resp = b""
            while True:
                chunk = s.recv(4096)
                if not chunk:
                    break
                resp += chunk
        finally:
            s.close()
        status_line = resp.split(b"\r\n", 1)[0]
        self.assertIn(b"400", status_line, f"应为 400，实收：{resp[:120]!r}")
        self.assertIn(b"invalid_request_error", resp)

    def test_malformed_request_structure_returns_400(self):
        # 回归（修 P1 GPT 复审）：JSON 能解析但结构不对（顶层非对象 / messages 非数组）
        # 以前会在下游 .get / 迭代处抛 AttributeError/TypeError 击穿线程，客户端拿空响应。
        # 修复后一律回规范 400，且上游一次都不被打到。
        before = len(self.hits)
        for bad in ([], "hello", 42, {"messages": None}, {"model": "x"}):
            s, b = _req(f"{self.base}/{SEC}/v1/messages", "POST", bad)
            self.assertEqual(s, 400, f"{bad!r} 应回 400，实收 {s} {b[:120]!r}")
            self.assertIn(b"invalid_request_error", b)
        self.assertEqual(len(self.hits), before, "畸形请求不应打到上游")


class ProxyUpstreamErrorPassthrough(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.up_url, cls.hits, cls.stop_up = start_mock("status:401")
        port = 18992
        cls.base = f"http://127.0.0.1:{port}"
        cls.logf = os.path.join(tempfile.gettempdir(), f"csswitch-401-{port}.log")
        open(cls.logf, "w").close()
        env = dict(os.environ, DEEPSEEK_API_KEY="fake-key",
                   CSSWITCH_UPSTREAM_URL=cls.up_url)
        cls.proc = subprocess.Popen(
            [sys.executable, PROXY, "--provider", "deepseek",
             "--port", str(port), "--auth-token", SEC, "--log", cls.logf],
            env=env, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        for _ in range(50):
            try:
                s, _b = _req(f"{cls.base}/{SEC}/health")
                if s == 200:
                    break
            except Exception:
                pass
            time.sleep(0.1)

    @classmethod
    def tearDownClass(cls):
        cls.proc.terminate()
        cls.proc.wait(timeout=5)
        cls.stop_up()

    def test_upstream_401_preserved_not_502(self):
        # 修 P3（GPT 复审）：上游 401 原样透传（不再归一化 502），verify_key 才能据此判 key 无效。
        s, b = _req(f"{self.base}/{SEC}/v1/messages", "POST",
                    {"model": "claude-opus-4-8", "max_tokens": 1,
                     "messages": [{"role": "user", "content": "ping"}]})
        self.assertEqual(s, 401, f"应保留上游 401，实收 {s} {b[:160]!r}")


class ProxyQwenUpstreamErrorPassthrough(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.up_url, cls.hits, cls.stop_up = start_mock("status:401")
        port = 18993
        cls.base = f"http://127.0.0.1:{port}"
        cls.logf = os.path.join(tempfile.gettempdir(), f"csswitch-qwen401-{port}.log")
        open(cls.logf, "w").close()
        env = dict(os.environ, DASHSCOPE_API_KEY="fake-key",
                   CSSWITCH_UPSTREAM_URL=cls.up_url)
        cls.proc = subprocess.Popen(
            [sys.executable, PROXY, "--provider", "qwen",
             "--port", str(port), "--auth-token", SEC, "--log", cls.logf],
            env=env, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        for _ in range(50):
            try:
                s, _b = _req(f"{cls.base}/{SEC}/health")
                if s == 200:
                    break
            except Exception:
                pass
            time.sleep(0.1)

    @classmethod
    def tearDownClass(cls):
        cls.proc.terminate()
        cls.proc.wait(timeout=5)
        cls.stop_up()

    def test_qwen_upstream_401_preserved_not_502(self):
        # 修 P2（GPT 复审）：qwen（OpenAI 翻译路径）上游 401 也应原样透传，而非归一化 502。
        s, b = _req(f"{self.base}/{SEC}/v1/messages", "POST",
                    {"model": "claude-opus-4-8", "max_tokens": 1,
                     "messages": [{"role": "user", "content": "ping"}]})
        self.assertEqual(s, 401, f"qwen 也应保留上游 401，实收 {s} {b[:160]!r}")


if __name__ == "__main__":
    unittest.main()

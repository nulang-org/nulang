import json
import threading
import unittest
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from unittest import mock
from urllib.parse import parse_qs

from client import NulangWebClient, ApiError

CONTRACT = {
    "openapi": "3.1.0", "paths": {
      "/v1/items/{item}": {"get": {"operationId": "read_item", "parameters": [
          {"name": "item", "in": "path", "required": True},
          {"name": "tag", "in": "query", "required": False},
          {"name": "x-tenant-id", "in": "header", "required": True}
      ]}},
      "/v1/jobs": {"post": {"operationId": "create_job", "requestBody": {
          "required": True, "content": {"application/x-www-form-urlencoded": {}}
      }}}
    }
}

FORM_CONTRACT = {
    "openapi": "3.1.0",
    "paths": {
        "/v1/forms": {
            "post": {
                "operationId": "submit_form",
                "requestBody": {
                    "required": True,
                    "content": {
                        "application/x-www-form-urlencoded": {
                            "schema": {
                                "type": "object",
                                "properties": {
                                    "enabled": {"type": "boolean"},
                                    "name": {"type": "string"},
                                },
                                "required": ["enabled", "name"],
                            }
                        }
                    },
                },
            }
        }
    },
}

class Handler(BaseHTTPRequestHandler):
    def do_GET(self):
        body = json.dumps({"path": self.path, "tenant": self.headers.get("x-tenant-id"),
                           "authorization": self.headers.get("Authorization"),
                           "traceparent": self.headers.get("traceparent")}).encode()
        if "/redirect" in self.path:
            self.send_response(302)
            self.send_header("Location", "http://127.0.0.1:1/credentials")
            self.end_headers()
            return
        self.send_response(200 if "missing" not in self.path else 404)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_POST(self):
        body = self.rfile.read(int(self.headers.get("Content-Length", "0")))
        result = json.dumps({"path": self.path, "form": parse_qs(body.decode()),
                             "content_type": self.headers.get("Content-Type")}).encode()
        self.send_response(201)
        self.end_headers()
        self.wfile.write(result)

    def log_message(self, *args):
        pass

class ClientTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        cls.thread = threading.Thread(target=cls.server.serve_forever, daemon=True)
        cls.thread.start()
        cls.base = f"http://127.0.0.1:{cls.server.server_port}/api"

    @classmethod
    def tearDownClass(cls):
        cls.server.shutdown()
        cls.server.server_close()
        cls.thread.join()

    def test_path_query_header_bearer_and_traceparent(self):
        client = NulangWebClient(self.base, CONTRACT, token="secret")
        tp = "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01"
        response = client.call("read_item", path={"item": "a/b"}, query={"tag": ["A B", "é"]},
                               headers={"x-tenant-id": "acme"}, traceparent=tp)
        data = response.json()
        self.assertEqual(data["path"], "/api/v1/items/a%2Fb?tag=A+B&tag=%C3%A9")
        self.assertEqual(data["tenant"], "acme")
        self.assertEqual(data["authorization"], "Bearer secret")
        self.assertEqual(data["traceparent"], tp)

    def test_form_post_and_status(self):
        response = NulangWebClient(self.base, CONTRACT).call("create_job", body={"name": "mañana"})
        self.assertEqual(response.status, 201)
        self.assertEqual(response.json()["form"], {"name": ["mañana"]})
        self.assertEqual(response.json()["content_type"], "application/x-www-form-urlencoded")

    def test_form_schema_validates_fields_and_encodes_bool(self):
        client = NulangWebClient(self.base, FORM_CONTRACT)
        response = client.call("submit_form", body={"enabled": True, "name": "ready"})
        self.assertEqual(response.json()["form"], {"enabled": ["true"], "name": ["ready"]})
        with mock.patch.object(client._opener, "open", side_effect=AssertionError("invalid form reached network")):
            with self.assertRaisesRegex(ValueError, "missing required form field: name"):
                client.call("submit_form", body={"enabled": True})
            with self.assertRaisesRegex(ValueError, "undeclared form field: extra"):
                client.call("submit_form", body={"enabled": True, "name": "ready", "extra": "x"})

    def test_http_failure_retains_status(self):
        with self.assertRaises(ApiError) as ex:
            NulangWebClient(self.base, CONTRACT).call("read_item", path={"item": "missing"},
                                                       headers={"x-tenant-id": "acme"})
        self.assertEqual(ex.exception.status, 404)

    def test_missing_required_and_unknown_arguments_fail_before_network(self):
        client = NulangWebClient(self.base, CONTRACT)
        with self.assertRaises(ValueError):
            client.call("read_item", path={"item": "x"})
        with self.assertRaises(ValueError):
            client.call("read_item", path={"item": "x"}, headers={"x-tenant-id": "a"}, query={"secret": "x"})
        with self.assertRaises(ValueError):
            client.call("read_item", path={"item": ".."}, headers={"x-tenant-id": "a"})

    def test_does_not_follow_redirects_with_credentials(self):
        with self.assertRaises(ApiError) as ex:
            NulangWebClient(self.base, CONTRACT, token="secret").call(
                "read_item", path={"item": "redirect"}, headers={"x-tenant-id": "acme"})
        self.assertEqual(ex.exception.status, 302)

    def test_conflicting_bearer_header_rejected(self):
        client = NulangWebClient(self.base, CONTRACT, token="secret")
        with self.assertRaisesRegex(ValueError, "Authorization"):
            client.call("read_item", path={"item": "x"},
                        headers={"x-tenant-id": "acme", "Authorization": "override"})

    def test_invalid_contract_and_unknown_operation(self):
        with self.assertRaises(ValueError):
            NulangWebClient(self.base, {"openapi": "3.1.0", "paths": {"//evil.com/x": {"get": {"operationId": "bad"}}}})
        with self.assertRaises(ValueError):
            NulangWebClient(self.base, CONTRACT).call("bogus")
        with self.assertRaises(ValueError):
            NulangWebClient(self.base, {"openapi":"3.1.0", "paths": {
                "/x": {"get": {"operationId":"duplicate"}}, "/y": {"post": {"operationId":"duplicate"}}
            }})

if __name__ == "__main__":
    unittest.main()

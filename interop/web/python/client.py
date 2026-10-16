"""Small zero-dependency HTTP client for Nulang Web's OpenAPI 3.1 contracts.

This is an HTTP interoperability client, not an actor wire-protocol client.
It intentionally does not infer JSON response schemas or retry unsafe calls.
"""
from __future__ import annotations

from dataclasses import dataclass
import json
import re
from typing import Any, Mapping
from urllib.error import HTTPError
from urllib.parse import quote, urlencode, urlsplit, urlunsplit
from urllib.request import HTTPRedirectHandler, Request, build_opener

_METHODS = {"get", "put", "post", "delete", "patch", "head", "options", "trace"}
_TRACEPARENT = re.compile(r"^00-[0-9a-f]{32}-[0-9a-f]{16}-[0-9a-f]{2}$")
_PLACEHOLDER = re.compile(r"\{([^{}]+)\}")


@dataclass(frozen=True)
class ApiResponse:
    status: int
    headers: Mapping[str, str]
    body: bytes

    def json(self) -> Any:
        return json.loads(self.body)

    def text(self) -> str:
        return self.body.decode("utf-8")


class ApiError(Exception):
    def __init__(self, status: int, body: bytes, headers: Mapping[str, str]):
        self.status = status
        self.body = body
        self.headers = headers
        super().__init__(f"Nulang Web HTTP {status}")


def _param_str(value: Any) -> str:
    if value is None or isinstance(value, (dict, list, tuple)):
        raise ValueError("parameter must be a scalar (or a query list of scalars)")
    if isinstance(value, bool):
        return "true" if value else "false"
    return str(value)


def _validate_traceparent(value: str) -> None:
    if not _TRACEPARENT.fullmatch(value) or int(value[3:35], 16) == 0 or int(value[36:52], 16) == 0:
        raise ValueError("traceparent must be a valid W3C version-00 trace context")

def _encode_form(body: dict[str, Any], media_type: Mapping[str, Any]) -> bytes:
    schema = media_type.get("schema")
    if schema is not None:
        if not isinstance(schema, Mapping):
            raise ValueError("form request schema must be an object")
        properties = schema.get("properties")
        if not isinstance(properties, Mapping):
            raise ValueError("form request schema must define object properties")
        required = schema.get("required", [])
        if not isinstance(required, list) or not all(isinstance(name, str) for name in required):
            raise ValueError("form request schema has invalid required fields")
        for name in required:
            value = body.get(name)
            if name not in properties:
                raise ValueError(f"required form field is not declared: {name}")
            if name not in body or value is None or (
                isinstance(value, (list, tuple)) and not value
            ):
                raise ValueError(f"missing required form field: {name}")
        for name in body:
            if name not in properties:
                raise ValueError(f"undeclared form field: {name}")

    fields: list[tuple[str, str]] = []
    for name, value in body.items():
        for item in value if isinstance(value, (list, tuple)) else [value]:
            fields.append((name, _param_str(item)))
    return urlencode(fields).encode("utf-8")



class _NoRedirect(HTTPRedirectHandler):
    def redirect_request(self, req, fp, code, msg, headers, newurl):
        return None


class NulangWebClient:
    def __init__(self, base_url: str, contract: Mapping[str, Any], *, token: str | None = None,
                 timeout: float = 10.0):
        parsed = urlsplit(base_url)
        if parsed.scheme not in {"http", "https"} or not parsed.netloc or parsed.query or parsed.fragment:
            raise ValueError("base_url must be an absolute HTTP(S) URL without query or fragment")
        if timeout <= 0:
            raise ValueError("timeout must be positive")
        self._base = parsed
        self._token = token
        self._timeout = timeout
        self._opener = build_opener(_NoRedirect)
        self._ops: dict[str, tuple[str, str, Mapping[str, Any]]] = {}
        if not str(contract.get("openapi", "")).startswith("3.") or not isinstance(contract.get("paths"), dict):
            raise ValueError("an OpenAPI 3.x document with paths is required")
        for route, methods in contract["paths"].items():
            if not isinstance(route, str) or not route.startswith("/") or route.startswith("//") or "?" in route or "#" in route:
                raise ValueError(f"invalid contract path: {route}")
            if not isinstance(methods, dict):
                raise ValueError("path methods must be an object")
            for method, operation in methods.items():
                if method not in _METHODS:
                    continue
                if not isinstance(operation, dict) or not isinstance(operation.get("operationId"), str) or not operation["operationId"]:
                    raise ValueError(f"{method.upper()} {route} requires operationId")
                if operation.get("x-nulang-binding-diagnostics"):
                    raise ValueError(f"{method.upper()} {route} has invalid Nulang bindings")
                op_id = operation["operationId"]
                if op_id in self._ops:
                    raise ValueError(f"duplicate operationId: {op_id}")
                self._ops[op_id] = (method.upper(), route, operation)

    def call(self, operation_id: str, *, path: Mapping[str, Any] | None = None,
             query: Mapping[str, Any] | None = None, headers: Mapping[str, Any] | None = None,
             body: Any = None, traceparent: str | None = None) -> ApiResponse:
        if operation_id not in self._ops:
            raise ValueError(f"Unknown operationId: {operation_id}")
        method, route, operation = self._ops[operation_id]
        inputs = {"path": dict(path or {}), "query": dict(query or {}), "header": dict(headers or {})}
        declared: dict[str, set[str]] = {key: set() for key in inputs}
        for p in operation.get("parameters", []):
            location, name = p.get("in"), p.get("name")
            if location == "cookie":
                raise ValueError("cookie parameters are not supported by this cross-platform client")
            if location not in inputs or not isinstance(name, str):
                raise ValueError("unsupported or malformed OpenAPI parameter")
            declared[location].add(name)
            if p.get("required", False) and (name not in inputs[location] or inputs[location][name] is None):
                raise ValueError(f"missing required {location} parameter: {name}")
        for location in inputs:
            unknown = set(inputs[location]) - declared[location]
            if unknown:
                raise ValueError(f"undeclared {location} parameter: {sorted(unknown)[0]}")
        for name in _PLACEHOLDER.findall(route):
            if name not in inputs["path"]:
                raise ValueError(f"missing path parameter: {name}")
            value = _param_str(inputs["path"][name])
            if value in {".", ".."}:
                raise ValueError("path parameter cannot be a dot segment")
            route = route.replace("{" + name + "}", quote(value, safe=""))
        params: list[tuple[str, str]] = []
        for name, value in inputs["query"].items():
            for item in (value if isinstance(value, (list, tuple)) else [value]):
                params.append((name, _param_str(item)))
        req_headers = {name: _param_str(value) for name, value in inputs["header"].items()}
        if any("\r" in x or "\n" in x for x in [*req_headers, *req_headers.values()]):
            raise ValueError("HTTP headers cannot contain CR or LF")
        if self._token is not None:
            if any(name.lower() == "authorization" for name in req_headers):
                raise ValueError("Authorization header conflicts with configured bearer token")
            if "\r" in self._token or "\n" in self._token:
                raise ValueError("invalid bearer token")
            req_headers["Authorization"] = "Bearer " + self._token
        if traceparent is not None:
            _validate_traceparent(traceparent)
            req_headers["traceparent"] = traceparent
        payload = None
        request_body = operation.get("requestBody", {})
        if body is not None:
            content = request_body.get("content", {})
            if "application/x-www-form-urlencoded" in content:
                if not isinstance(body, dict):
                    raise ValueError("form body must be a dictionary")
                media_type = content["application/x-www-form-urlencoded"]
                if not isinstance(media_type, Mapping):
                    raise ValueError("form media type must be an object")
                payload = _encode_form(body, media_type)
            elif "application/json" in content:
                payload = json.dumps(body, separators=(",", ":")).encode("utf-8")
                req_headers["Content-Type"] = "application/json"
            else:
                raise ValueError("request body media type is not declared in OpenAPI")
        elif request_body.get("required", False):
            raise ValueError("missing required request body")
        url = urlunsplit((self._base.scheme, self._base.netloc,
                          self._base.path.rstrip("/") + route,
                          urlencode(params), ""))
        request = Request(url, data=payload, headers=req_headers, method=method)
        try:
            with self._opener.open(request, timeout=self._timeout) as response:
                return ApiResponse(response.status, dict(response.headers.items()), response.read())
        except HTTPError as error:
            with error:
                raise ApiError(error.code, error.read(), dict(error.headers.items())) from None

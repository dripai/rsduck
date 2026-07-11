import argparse
import http.cookiejar
import json
from pathlib import Path
from urllib.error import HTTPError, URLError
from urllib.request import HTTPCookieProcessor, Request, build_opener


class DemoError(RuntimeError):
    pass


class DemoClient:
    def __init__(self, url, username, password, timeout=15.0):
        self.base_url = url.rstrip("/")
        self.username = username
        self.timeout = timeout
        self.opener = build_opener(HTTPCookieProcessor(http.cookiejar.CookieJar()))
        self.login(password)

    def request(self, path, payload=None, method="POST"):
        data = None if payload is None else json.dumps(payload).encode("utf-8")
        request = Request(
            self.base_url + path,
            data=data,
            headers={"Content-Type": "application/json"},
            method=method,
        )
        try:
            with self.opener.open(request, timeout=self.timeout) as response:
                return json.loads(response.read().decode("utf-8"))
        except (HTTPError, URLError, OSError, ValueError) as exc:
            raise DemoError("%s %s failed: %s" % (method, path, exc)) from exc

    def login(self, password):
        result = self.request("/login", {"username": self.username, "password": password})
        if not result.get("success"):
            raise DemoError("login failed: %s" % result.get("msg", "unknown error"))

    def sql(self, statement, page_size=1000):
        print("SQL> %s" % statement)
        result = self.request("/sql", {"sql": statement, "page": 0, "page_size": page_size})
        if not result.get("success"):
            raise DemoError("SQL failed: %s" % result.get("msg", "unknown error"))
        return result

    def expect_sql_failure(self, statement, expected_text):
        print("SQL> %s" % statement)
        result = self.request("/sql", {"sql": statement, "page": 0, "page_size": 100})
        if result.get("success"):
            raise DemoError("SQL unexpectedly succeeded")
        message = result.get("msg", "")
        if expected_text.lower() not in message.lower():
            raise DemoError("unexpected SQL error: %s" % message)
        print("expected failure: %s" % message)

    def scalar(self, statement):
        result = self.sql(statement, page_size=10)
        rows = result.get("rows", [])
        if len(rows) != 1 or not rows[0]:
            raise DemoError("expected one scalar row from: %s" % statement)
        return rows[0][0]

    def scalar_int(self, statement):
        value = self.scalar(statement)
        try:
            return int(value)
        except (TypeError, ValueError) as exc:
            raise DemoError("expected integer scalar from %s, got %r" % (statement, value)) from exc

    def snapshot(self):
        result = self.request("/snapshot", {})
        if not result.get("success"):
            raise DemoError("snapshot failed: %s" % result.get("msg", "unknown error"))
        return result

    def import_parquet(self, source, schema, table):
        result = self.request(
            "/parquet-import",
            {"source": source, "schema": schema, "table": table},
        )
        if not result.get("success"):
            raise DemoError("Parquet import failed: %s" % result.get("msg", "unknown error"))
        return result


def add_connection_args(parser):
    parser.add_argument("--url", default="http://127.0.0.1:13307")
    parser.add_argument("--username", default="admin")
    parser.add_argument("--password", required=True)
    parser.add_argument("--timeout", type=float, default=15.0)
    parser.add_argument("--cleanup", action="store_true")


def build_parser(description):
    parser = argparse.ArgumentParser(description=description)
    add_connection_args(parser)
    return parser


def client_from_args(args):
    return DemoClient(args.url, args.username, args.password, args.timeout)


def assert_equal(actual, expected, label):
    if actual != expected:
        raise DemoError("%s: expected %r, got %r" % (label, expected, actual))
    print("verified %s=%r" % (label, actual))


def require_directory(path):
    directory = Path(path)
    if not directory.is_dir():
        raise DemoError("directory does not exist: %s" % directory)
    return directory

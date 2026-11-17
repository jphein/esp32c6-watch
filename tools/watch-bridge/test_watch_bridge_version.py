"""watch_bridge /api/version (realm-sigil, 2026-11-17): the stamp beside the file + the shipped realm_sigil package,
else 'dev' / 503. Hermetic: a fixture realm_sigil package, stubbed speech/state/requests, import state restored.
    python3 -m pytest tools/watch-bridge/test_watch_bridge_version.py"""
import http.client
import importlib.util
import json
import os
import shutil
import sys
import threading
import types
from http.server import ThreadingHTTPServer

import pytest

HERE = os.path.dirname(os.path.abspath(__file__))
FIXTURE_SIGIL = '''
def version_dict(name, description, realm, repo, *, hash="dev", dirty=False, started=None, uptime=None, **kw):
    return {"name": name, "version": "Fixture Name · " + hash, "hash": hash, "dirty": dirty, "realm": realm,
            "repo": repo, "started": started, "uptime": uptime}
'''


@pytest.fixture
def bridge(tmp_path, monkeypatch):
    before = {m for m in sys.modules if m == "realm_sigil" or m.startswith("realm_sigil.")}
    yield from _bridge(tmp_path, monkeypatch)
    for m in [m for m in sys.modules if (m == "realm_sigil" or m.startswith("realm_sigil.")) and m not in before]:
        del sys.modules[m]   # imported during the test: monkeypatch only restores what existed before


def _bridge(tmp_path, monkeypatch):
    monkeypatch.setattr(sys, "path", [p for p in sys.path if "realm-sigil" not in p])
    for name in [m for m in sys.modules if m == "realm_sigil" or m.startswith("realm_sigil.")]:
        monkeypatch.delitem(sys.modules, name)
    for name, attrs in (("speech", {"transcribe": lambda *a: ""}), ("state", {"load_config": lambda: {"key": "x"}}),
                        ("requests", {})):
        monkeypatch.setitem(sys.modules, name, types.SimpleNamespace(**attrs))
    shutil.copy(os.path.join(HERE, "watch_bridge.py"), tmp_path / "watch_bridge.py")

    def make(with_sigil=True, stamp=None):
        if with_sigil:
            (tmp_path / "realm_sigil").mkdir(exist_ok=True)
            (tmp_path / "realm_sigil" / "__init__.py").write_text(FIXTURE_SIGIL)
        if stamp is not None:
            (tmp_path / "watch_bridge.hash").write_bytes(stamp if isinstance(stamp, bytes) else stamp.encode())
        spec = importlib.util.spec_from_file_location(f"wb_{tmp_path.name}", tmp_path / "watch_bridge.py")
        mod = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(mod)
        return mod
    yield make


def get(mod, path):
    handler = next(v for v in vars(mod).values() if isinstance(v, type) and hasattr(v, "do_GET") and v.__module__ == mod.__name__)
    srv = ThreadingHTTPServer(("127.0.0.1", 0), handler)
    threading.Thread(target=srv.serve_forever, daemon=True).start()
    try:
        c = http.client.HTTPConnection("127.0.0.1", srv.server_address[1], timeout=5)
        c.request("GET", path)
        r = c.getresponse()
        return r.status, json.loads(r.read() or b"{}")
    finally:
        srv.shutdown()


def test_a_stamped_deploy_serves_the_contract_over_http(bridge):
    st, doc = get(bridge(stamp="31fb82f\n"), "/api/version?x=1")
    assert st == 200 and doc["hash"] == "31fb82f" and doc["version"].endswith("· 31fb82f") and doc["dirty"] is False


def test_without_the_package_the_route_is_503_and_health_is_unchanged(bridge):
    mod = bridge(with_sigil=False, stamp="31fb82f")
    assert get(mod, "/api/version")[0] == 503
    st, doc = get(mod, "/health")
    assert st == 200 and doc["ok"] is True
    assert get(mod, "/other")[0] == 404


@pytest.mark.parametrize("stamp,want", [("31fb82f", "31fb82f"), ("31FB82F\n", "31fb82f"), ("a" * 40, "a" * 12),
                                        ("31fb82", "dev"), ("a" * 41, "dev"), ("", "dev"), (None, "dev"),
                                        ("31fb82f; rm -rf ~", "dev"), ("31fb82f" + " " * 60 + "junk", "dev"),
                                        (b"31fb82\xc3\xa9", "dev")])
def test_the_stamp_is_the_whole_file_or_nothing(bridge, stamp, want):
    doc = bridge(stamp=stamp).version_doc()
    assert doc["hash"] == want and doc["dirty"] is (want == "dev")


def test_the_stamp_is_read_at_start_not_per_request(bridge, tmp_path):
    mod = bridge(stamp="31fb82f")
    (tmp_path / "watch_bridge.hash").write_text("aaaaaaa")   # the next deploy writes before it restarts the service
    assert mod.version_doc()["hash"] == "31fb82f"


def test_no_fixture_package_is_left_in_sys_modules():
    assert not any(m == "realm_sigil" or m.startswith("realm_sigil.") for m in sys.modules
                   if getattr(sys.modules[m], "__file__", "") and "pytest-of" in (sys.modules[m].__file__ or ""))

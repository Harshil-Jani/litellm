import asyncio
from typing import Final

import httpx
import pytest
from starlette.requests import Request
from starlette.types import Message, Scope

from litellm.proxy.control_plane.auth.rbac import EDITOR_ROLE, VIEWER_ROLE, build_principal
from litellm.proxy.control_plane.container import build_container
from litellm.proxy.control_plane.dataplanes.credentials import Caller
from litellm.proxy.control_plane.proxy.router import _forward
from litellm.proxy.control_plane.settings import ControlPlaneSettings
from tests.unit.proxy.control_plane.support import (
    DATAPLANES,
    MASTER_KEY,
    RoutedTransport,
    no_exchange,
    response,
    rig,
)

EU = "https://eu.test"


def test_get_is_forwarded_as_the_caller_with_query_and_the_dataplane_status_comes_back() -> None:
    client, transport, minter = rig({f"{EU}/key/list": [response(200, {"keys": ["k1"], "total_count": 1})]})
    reply = client.get("/api/v1/dataplanes/eu/proxy/key/list?page=2&size=5", headers=minter.headers("eu"))
    assert (reply.status_code, reply.json()) == (200, {"keys": ["k1"], "total_count": 1})
    assert reply.headers["X-LiteLLM-Dataplane"] == "eu"
    url, params, headers = transport.calls[0]
    assert (url, params) == (f"{EU}/key/list", (("page", "2"), ("size", "5")))
    assert headers is not None and headers["X-LiteLLM-Impersonate-User"] == "alice"


def test_get_preserves_repeated_query_parameters_in_order() -> None:
    client, transport, minter = rig({f"{EU}/key/list": [response(200, {})]})
    reply = client.get(
        "/api/v1/dataplanes/eu/proxy/key/list?team_id=first&team_id=second",
        headers=minter.headers("eu"),
    )
    assert reply.status_code == 200
    assert transport.calls[0][1] == (("team_id", "first"), ("team_id", "second"))


def test_writes_need_proxy_write_go_out_once_and_are_audited() -> None:
    client, transport, minter = rig(
        {
            f"{EU}/key/generate": [response(200, {"key": "sk-new", "token_id": "t1"})],
            f"{EU}/team/update": [httpx.ReadTimeout("slow")],
            f"{EU}/key/delete": [response(400, {"error": {"message": "no such key"}})],
        }
    )
    viewer = minter.headers("eu", role=VIEWER_ROLE)
    editor = minter.headers("eu", role=EDITOR_ROLE)
    denied = client.post("/api/v1/dataplanes/eu/proxy/key/generate", json={"key_alias": "x"}, headers=viewer)
    assert denied.status_code == 403 and "proxy:write" in denied.json()["detail"]
    created = client.post("/api/v1/dataplanes/eu/proxy/key/generate", json={"key_alias": "x"}, headers=editor)
    assert (created.status_code, created.json()["token_id"]) == (200, "t1")
    assert transport.writes == [("POST", f"{EU}/key/generate", {"key_alias": "x"})]
    timed_out = client.patch("/api/v1/dataplanes/eu/proxy/team/update", json={"team_id": "t"}, headers=editor)
    assert timed_out.status_code == 502 and "1 attempt" in timed_out.json()["detail"]
    failed = client.delete("/api/v1/dataplanes/eu/proxy/key/delete", headers=editor)
    assert (failed.status_code, failed.json()) == (400, {"error": {"message": "no such key"}})
    trail = client.get("/api/v1/audit?dataplane=eu", headers=minter.headers(role="admin")).json()["items"]
    assert [(e["method"], e["path"], e["decision"], e["status"]) for e in trail] == [
        ("DELETE", "/key/delete", "allowed", 400),
        ("PATCH", "/team/update", "unreachable", None),
        ("POST", "/key/generate", "allowed", 200),
    ]
    assert trail[2]["actor"] == "alice" and trail[2]["credential"] == "service_token"


def test_denied_paths_are_403_audited_and_never_reach_the_dataplane() -> None:
    client, transport, minter = rig({})
    blocked = client.post(
        "/api/v1/dataplanes/eu/proxy/v1/chat/completions",
        content=b"[",
        headers={**minter.headers(role=EDITOR_ROLE), "Content-Type": "application/json"},
    )
    assert blocked.status_code == 403 and "inference" in blocked.json()["detail"]
    assert transport.calls == []
    trail = client.get("/api/v1/audit?dataplane=eu", headers=minter.headers(role="admin")).json()["items"]
    assert [(e["path"], e["decision"]) for e in trail] == [("/v1/chat/completions", "denied")]


def test_a_non_object_body_is_400_and_unbound_dataplane_is_403() -> None:
    client, _, minter = rig({})
    bad = client.post(
        "/api/v1/dataplanes/eu/proxy/key/generate",
        content=b"[1,2]",
        headers={**minter.headers(role=EDITOR_ROLE), "Content-Type": "application/json"},
    )
    assert bad.status_code == 400
    assert client.get("/api/v1/dataplanes/us/proxy/key/list", headers=minter.headers("eu")).status_code == 403


def test_audit_read_needs_its_scope_and_filters_by_actor() -> None:
    client, _, minter = rig({f"{EU}/key/list": [response(200, {})]})
    client.get("/api/v1/dataplanes/eu/proxy/key/list", headers=minter.headers("eu"))
    assert client.get("/api/v1/audit", headers=minter.headers(role=EDITOR_ROLE)).status_code == 403
    assert client.get("/api/v1/audit?actor=bob", headers=minter.headers(role="admin")).json()["items"] == []
    assert (
        len(client.get("/api/v1/audit?actor=alice&dataplane=eu", headers=minter.headers(role="admin")).json()["items"])
        == 1
    )


def test_audit_reads_are_clamped_to_the_callers_dataplane_binding() -> None:
    client, _, minter = rig({f"{EU}/key/list": [response(200, {})], "https://us.test/key/list": [response(200, {})]})
    client.get("/api/v1/dataplanes/eu/proxy/key/list", headers=minter.headers("eu"))
    client.get("/api/v1/dataplanes/us/proxy/key/list", headers=minter.headers("us"))
    eu_auditor = minter.headers("eu", role="admin")
    unfiltered = client.get("/api/v1/audit", headers=eu_auditor).json()["items"]
    assert [e["dataplane_id"] for e in unfiltered] == ["eu"]
    assert client.get("/api/v1/audit?dataplane=us", headers=eu_auditor).status_code == 403
    assert len(client.get("/api/v1/audit?dataplane=eu", headers=eu_auditor).json()["items"]) == 1
    fleet = client.get("/api/v1/audit", headers=minter.headers(role="admin")).json()["items"]
    assert sorted(e["dataplane_id"] for e in fleet) == ["eu", "us"]


def test_a_redirecting_dataplane_is_a_502_and_the_redirect_is_never_followed() -> None:
    client, transport, minter = rig(
        {f"{EU}/v1/mcp/server/oauth/x/authorize": [httpx.Response(302, headers={"Location": "http://127.0.0.1:9/"})]}
    )
    reply = client.get("/api/v1/dataplanes/eu/proxy/v1/mcp/server/oauth/x/authorize", headers=minter.headers("eu"))
    assert reply.status_code == 502 and "redirect" in reply.json()["detail"]
    assert [url for url, _, _ in transport.calls] == [f"{EU}/v1/mcp/server/oauth/x/authorize"]
    trail = client.get("/api/v1/audit", headers=minter.headers(role="admin")).json()["items"]
    assert (trail[0]["decision"], trail[0]["status"]) == ("denied", 302)


def test_key_bearing_paths_are_redacted_in_the_audit_trail() -> None:
    client, _, minter = rig({f"{EU}/key/sk-live-123/reset_spend": [response(200, {})]})
    assert (
        client.post(
            "/api/v1/dataplanes/eu/proxy/key/sk-live-123/reset_spend",
            json={},
            headers=minter.headers("eu", role=EDITOR_ROLE),
        ).status_code
        == 200
    )
    trail = client.get("/api/v1/audit", headers=minter.headers(role="admin")).json()["items"]
    assert trail[0]["path"] == "/key/sk-[redacted]/reset_spend"


def test_a_denied_key_bearing_path_is_redacted_in_the_403_too() -> None:
    client, _, minter = rig({})
    denied = client.get("/api/v1/dataplanes/eu/proxy/key/sk-live-123/x%25y", headers=minter.headers("eu"))
    assert denied.status_code == 403 and "sk-live-123" not in denied.text and "sk-[redacted]" in denied.text


@pytest.mark.asyncio
async def test_pending_request_body_does_not_hold_a_strict_audit_reservation() -> None:
    settings: Final = ControlPlaneSettings.model_validate(
        {
            "LITELLM_MASTER_KEY": MASTER_KEY,
            "dataplanes": DATAPLANES,
            "audit_queue_size": 1,
            "audit_strict": True,
        }
    )
    transport: Final = RoutedTransport({f"{EU}/key/generate": iter([response(200, {"token_id": "t1"})])})
    container: Final = build_container(settings, transport=transport, exchanger=no_exchange)
    principal: Final = build_principal(
        container.policy,
        "alice",
        "alice@x.test",
        frozenset({EDITOR_ROLE}),
        None,
        frozenset({"eu"}),
        "session",
    )
    caller: Final = Caller(principal, "session-token")
    dataplane: Final = container.registry.get("eu")
    assert dataplane is not None
    body_started: Final = asyncio.Event()
    release_body: Final = asyncio.Event()

    async def receive() -> Message:
        body_started.set()
        await release_body.wait()
        return {"type": "http.request", "body": b'{"key_alias":"x"}', "more_body": False}

    scope: Final[Scope] = {
        "type": "http",
        "asgi": {"version": "3.0", "spec_version": "2.3"},
        "http_version": "1.1",
        "method": "POST",
        "scheme": "http",
        "path": "/key/generate",
        "raw_path": b"/key/generate",
        "query_string": b"",
        "headers": [(b"content-type", b"application/json")],
        "client": ("testclient", 123),
        "server": ("testserver", 80),
    }
    request: Final = Request(scope, receive)
    write: Final = asyncio.create_task(
        _forward(
            request,
            "key/generate",
            principal,
            dataplane,
            caller,
            container.dataplane_client,
            container.audit_log,
        )
    )
    await body_started.wait()
    await asyncio.sleep(0)
    try:
        reservation: Final = container.audit_log.reserve()
        assert reservation is not None
        reservation.release()
    finally:
        release_body.set()
        reply: Final = await write
    assert reply.status_code == 200
    assert [(event.path, event.decision) for event in container.audit_log.recent(1)] == [("/key/generate", "allowed")]

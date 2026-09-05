"""Tiny JSON-RPC 2.0 client over HTTP, standard library only."""

from __future__ import annotations

import json
import urllib.error
import urllib.request


class RpcError(RuntimeError):
    """The node accepted the request but answered with an error object."""


class NodeUnreachable(RuntimeError):
    """The node could not be contacted at all."""


def normalise_endpoint(pool: str) -> str:
    """Accept `1.2.3.4`, `1.2.3.4:26657`, or a full `http://host:port` URL."""
    pool = pool.strip()
    if pool.startswith(("http://", "https://")):
        return pool.rstrip("/")
    if ":" not in pool:
        pool = f"{pool}:26657"
    return f"http://{pool}"


class RpcClient:
    def __init__(self, endpoint: str, timeout: float = 15.0):
        self.endpoint = normalise_endpoint(endpoint)
        self.timeout = timeout
        self._id = 0

    def call(self, method: str, params: dict | None = None):
        self._id += 1
        payload = json.dumps(
            {
                "jsonrpc": "2.0",
                "id": self._id,
                "method": method,
                "params": params or {},
            }
        ).encode()
        request = urllib.request.Request(
            self.endpoint,
            data=payload,
            headers={"Content-Type": "application/json"},
            method="POST",
        )
        try:
            with urllib.request.urlopen(request, timeout=self.timeout) as response:
                body = json.loads(response.read().decode())
        except urllib.error.URLError as err:
            raise NodeUnreachable(f"{self.endpoint}: {err.reason}") from err
        except (TimeoutError, OSError) as err:
            raise NodeUnreachable(f"{self.endpoint}: {err}") from err

        if "error" in body and body["error"] is not None:
            raise RpcError(body["error"].get("message", str(body["error"])))
        return body.get("result")

    # -- convenience wrappers ----------------------------------------------

    def get_work(self) -> dict:
        return self.call("cog_getWork")

    def status(self) -> dict:
        return self.call("cog_status")

    def submit_solution(self, miner: str, salt: int, nonce: int, matmul_root: str) -> dict:
        return self.call(
            "cog_submitSolution",
            {
                "miner": miner,
                "salt": salt,
                "nonce": nonce,
                "matmul_root": matmul_root,
            },
        )

    def reveal_requests(self, miner: str) -> list[dict]:
        return self.call("cog_getRevealRequests", {"miner": miner}).get("requests", [])

    def submit_reveal(self, commit_id: str, rows: list[dict]) -> dict:
        return self.call("cog_submitReveal", {"commit_id": commit_id, "rows": rows})

    def balance(self, address: str) -> dict:
        return self.call("cog_getBalance", {"address": address})

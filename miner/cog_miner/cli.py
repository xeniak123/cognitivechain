"""cog-miner: the CognitiveChain useful-work miner.

Usage:

    cog-miner --wallet <YOUR_COG_ADDRESS> --pool <NODE_IP>

One task cycle:

  1. ask the node for the current tip and difficulty;
  2. derive a private task from (tip, wallet, random salt);
  3. compute C = A*B mod p on the GPU -- this is the useful work;
  4. commit to the Merkle root of C and scan the bounded nonce space;
  5. on a hit, submit the commitment, then open the challenged rows.
"""

from __future__ import annotations

import argparse
import random
import sys
import time

from . import protocol
from .compute import Engine, detect_device, timed_matmul
from .rpc import NodeUnreachable, RpcClient, RpcError, normalise_endpoint
from .stats import Display, Stats

# How many solved tasks to keep in memory so their reveals can be answered.
CACHE_SIZE = 4
BALANCE_POLL_SECONDS = 15.0


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(
        prog="cog-miner",
        description="Mine COG by performing verifiable tensor computation.",
        formatter_class=argparse.RawDescriptionHelpFormatter,
        epilog="example:\n  cog-miner --wallet cog1a2b... --pool 203.0.113.10",
    )
    parser.add_argument(
        "--wallet",
        required=True,
        help="COG address that receives the block rewards",
    )
    parser.add_argument(
        "--pool",
        required=True,
        help="node address: IP, IP:PORT or http://HOST:PORT (default port 26657)",
    )
    parser.add_argument(
        "--device",
        choices=["auto", "cuda", "cpu"],
        default="auto",
        help="compute device (default: auto-detect)",
    )
    parser.add_argument(
        "--precision",
        choices=["auto", "fast", "fp64"],
        default="auto",
        help="GPU kernel: 'fast' uses 8-bit limbs on fp32 tensor cores, "
        "'fp64' uses double precision. Both are exact.",
    )
    parser.add_argument(
        "--quiet",
        action="store_true",
        help="log one line per event instead of a live dashboard",
    )
    parser.add_argument(
        "--benchmark",
        action="store_true",
        help="run the device selftest and a throughput measurement, then exit",
    )
    return parser


def validate_wallet(address: str) -> bytes:
    try:
        return protocol.address_bytes(address)
    except ValueError as err:
        raise SystemExit(
            f"error: --wallet {address!r} is not a valid COG address ({err}).\n"
            "Create one with:  cog-node keygen --out wallet.json"
        ) from err


def startup_selftest(engine: Engine, display: Display) -> float:
    """Verify the backend and measure a single task before mining for real."""
    seed = protocol.task_seed(b"\x00" * 32, b"\x00" * 20, 0)
    a = protocol.gen_matrix_a(seed)
    b = protocol.gen_matrix_b(seed)
    engine.selftest(a, b)
    _, elapsed = timed_matmul(engine, a, b)
    display.log(
        f"device selftest passed: one task in {elapsed * 1000:.1f} ms "
        f"({protocol.N}x{protocol.N} over GF({protocol.P}))"
    )
    return elapsed


def answer_reveals(
    client: RpcClient,
    wallet: str,
    cache: dict,
    stats: Stats,
    display: Display,
) -> None:
    """Open every outstanding commitment this miner still owes."""
    try:
        requests = client.reveal_requests(wallet)
    except (RpcError, NodeUnreachable):
        return

    for request in requests:
        commit_id = request["commit_id"]
        entry = cache.get(commit_id)
        if entry is None:
            continue
        matrix, levels = entry
        rows = []
        for index in request["rows"]:
            row = matrix[index]
            rows.append(
                {
                    "index": int(index),
                    "values": row.astype("<u2").tobytes().hex(),
                    "proof": [h.hex() for h in protocol.merkle_proof(levels, int(index))],
                }
            )
        try:
            client.submit_reveal(commit_id, rows)
            stats.reveals_sent += 1
            display.log(
                f"revealed {len(rows)} challenged rows for commitment {commit_id[:16]} "
                "- reward is now claimable in the next block"
            )
        except RpcError as err:
            display.log(f"reveal rejected: {err}")
        except NodeUnreachable:
            return


def mine_forever(args: argparse.Namespace) -> int:
    miner_bytes = validate_wallet(args.wallet)
    client = RpcClient(args.pool)

    stats = Stats(
        endpoint=normalise_endpoint(args.pool),
        wallet=args.wallet,
    )
    display = Display(stats, interactive=not args.quiet and sys.stdout.isatty())

    info = detect_device(args.device, args.precision)
    engine = Engine(info)
    stats.device = f"{info.backend} / {info.name}"
    stats.device_detail = info.detail
    display.log(f"using {info.name} via {info.backend} ({info.detail})")

    elapsed = startup_selftest(engine, display)
    if args.benchmark:
        from .compute import task_flops

        print(
            f"benchmark: {task_flops() / elapsed / 1e12:.3f} TOPS sustained, "
            f"{1 / elapsed:.2f} tasks/s, {int(protocol.MAX_NONCE / elapsed):,} "
            "nonce candidates per second of tensor work"
        )
        return 0

    cache: dict[str, tuple] = {}
    cache_order: list[str] = []
    last_balance_poll = 0.0
    work = None
    work_fetched_at = 0.0

    display.log(f"mining to {args.wallet}")
    display.refresh(force=True)

    while True:
        # 1. Refresh the tip. Work is re-fetched every cycle: a task is only
        #    valid on top of the parent it was derived from.
        try:
            work = client.get_work()
            work_fetched_at = time.time()
            stats.connected = True
            stats.last_error = ""
            stats.height = work["height"]
            stats.difficulty = work["difficulty"]
            stats.chain_id = work.get("chain_id", "?")
        except (NodeUnreachable, RpcError) as err:
            stats.connected = False
            stats.last_error = str(err)[:60]
            display.refresh(force=True)
            time.sleep(3)
            continue

        if work["matrix_dim"] != protocol.N or work["field_prime"] != protocol.P:
            display.log(
                f"error: node runs a different task shape "
                f"(N={work['matrix_dim']}, p={work['field_prime']}); upgrade cog-miner"
            )
            return 2

        prev_hash = bytes.fromhex(work["prev_hash"])
        difficulty = int(work["difficulty"])
        max_nonce = int(work.get("max_nonce", protocol.MAX_NONCE))

        # A pool asks miners to compute under *its* address, because the task
        # seed is what ends up in the block. Our own wallet then identifies
        # whose share it is, not whose task it is.
        mining_address = work.get("mining_address") or args.wallet
        mining_bytes = (
            miner_bytes
            if mining_address == args.wallet
            else protocol.address_bytes(mining_address)
        )
        if mining_address != args.wallet and stats.mining_for != mining_address:
            stats.mining_for = mining_address
            display.log(f"pula liczy zadania pod adresem {mining_address}")

        # 2. A fresh random salt gives this miner a task nobody else is running.
        salt = random.getrandbits(64)
        seed = protocol.task_seed(prev_hash, mining_bytes, salt)

        # 3. The useful work itself.
        a = protocol.gen_matrix_a(seed)
        b = protocol.gen_matrix_b(seed)
        matrix, matmul_seconds = timed_matmul(engine, a, b)

        # 4. Commit to the result, then search the bounded nonce space.
        leaves = protocol.build_leaves(matrix)
        levels = protocol.merkle_levels(leaves)
        root = levels[-1][0]
        nonce, _digest = protocol.search_nonce(seed, root, difficulty, max_nonce)
        stats.record_task(matmul_seconds, max_nonce if nonce is None else nonce + 1)

        if nonce is not None:
            commit = protocol.commit_id(seed, root, nonce).hex()
            try:
                response = client.submit_solution(args.wallet, salt, nonce, root.hex())
            except NodeUnreachable as err:
                stats.connected = False
                stats.last_error = str(err)[:60]
                response = None
            except RpcError as err:
                stats.rejected += 1
                display.log(f"solution rejected: {err}")
                response = None

            if response and response.get("status") == "accepted":
                cache[commit] = (matrix, levels)
                cache_order.append(commit)
                while len(cache_order) > CACHE_SIZE:
                    cache.pop(cache_order.pop(0), None)

                # A pool answers with share bookkeeping, a node with a block.
                if "block_candidate" in response:
                    stats.shares += 1
                    if response.get("block_accepted"):
                        stats.blocks_found += 1
                        display.log("BLOCK FOUND - pula przekazala go do wezla")
                else:
                    stats.blocks_found += 1
                    block_hash = response.get("block_hash", "")
                    display.log(
                        f"BLOCK FOUND at height {response.get('height', '?')} "
                        f"({block_hash[:16]}...)"
                    )
            elif response is not None:
                stats.rejected += 1
                display.log(f"solution not accepted: {response.get('detail', 'stale')}")

        # 5. Settle anything we still owe, then refresh the dashboard.
        answer_reveals(client, args.wallet, cache, stats, display)

        now = time.time()
        if now - last_balance_poll > BALANCE_POLL_SECONDS:
            last_balance_poll = now
            try:
                stats.balance_cog = client.balance(args.wallet)["balance_cog"]
            except (RpcError, NodeUnreachable):
                pass
        display.refresh()

        # Avoid hammering a node that is producing blocks slowly.
        if time.time() - work_fetched_at < 0.05:
            time.sleep(0.05)


def main(argv: list[str] | None = None) -> int:
    args = build_parser().parse_args(argv)
    try:
        return mine_forever(args)
    except KeyboardInterrupt:
        print("\nstopped.")
        return 0
    except RuntimeError as err:
        print(f"error: {err}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())

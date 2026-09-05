"""Live terminal statistics for the miner."""

from __future__ import annotations

import shutil
import sys
import time
from collections import deque
from dataclasses import dataclass, field

from .compute import task_flops


def human_time(seconds: float) -> str:
    seconds = int(seconds)
    h, rem = divmod(seconds, 3600)
    m, s = divmod(rem, 60)
    if h:
        return f"{h}h {m:02d}m {s:02d}s"
    if m:
        return f"{m}m {s:02d}s"
    return f"{s}s"


@dataclass
class Stats:
    device: str = "unknown"
    device_detail: str = ""
    endpoint: str = ""
    wallet: str = ""
    chain_id: str = "?"
    # Address the tasks are seeded with; differs from `wallet` when pooled.
    mining_for: str = ""

    height: int = 0
    difficulty: int = 0
    connected: bool = False
    last_error: str = ""

    tasks: int = 0
    hashes: int = 0
    blocks_found: int = 0
    shares: int = 0
    reveals_sent: int = 0
    rejected: int = 0

    balance_cog: str = "0.00000000"
    last_reward_at: float = 0.0

    started: float = field(default_factory=time.time)
    _task_times: deque = field(default_factory=lambda: deque(maxlen=32))
    _recent: deque = field(default_factory=lambda: deque(maxlen=32))

    def record_task(self, matmul_seconds: float, hashes: int) -> None:
        self.tasks += 1
        self.hashes += hashes
        self._task_times.append(matmul_seconds)
        self._recent.append(time.time())

    @property
    def tflops(self) -> float:
        """Sustained useful throughput, from the matmul time alone."""
        if not self._task_times:
            return 0.0
        avg = sum(self._task_times) / len(self._task_times)
        if avg <= 0:
            return 0.0
        return task_flops() / avg / 1e12

    @property
    def tasks_per_second(self) -> float:
        if len(self._recent) < 2:
            return 0.0
        span = self._recent[-1] - self._recent[0]
        if span <= 0:
            return 0.0
        return (len(self._recent) - 1) / span

    @property
    def hashrate(self) -> float:
        elapsed = time.time() - self.started
        return self.hashes / elapsed if elapsed > 0 else 0.0

    def render(self) -> list[str]:
        status = "connected" if self.connected else f"OFFLINE ({self.last_error})"
        lines = [
            "  CognitiveChain miner  ---  COG",
            f"  device     {self.device}  {self.device_detail}",
            f"  pool       {self.endpoint}  [{status}]",
            f"  wallet     {self.wallet}",
            *( [f"  pula       zadania pod {self.mining_for}"] if self.mining_for else [] ),
            f"  chain      {self.chain_id}   height {self.height}   difficulty {self.difficulty}",
            "",
            f"  useful work   {self.tflops:8.3f} TOPS      {self.tasks_per_second:6.2f} tasks/s",
            f"  nonce search  {self.hashrate / 1000:8.1f} kH/s     {self.tasks} tasks total",
            f"  blocks found  {self.blocks_found:8d}          proofs revealed {self.reveals_sent}",
            *(
                [f"  pool shares   {self.shares:8d}          zaakceptowanych udzialow"]
                if self.mining_for
                else []
            ),
            f"  balance       {self.balance_cog} COG",
            f"  uptime        {human_time(time.time() - self.started)}",
        ]
        if self.rejected:
            lines.append(f"  rejected      {self.rejected} (stale work, harmless)")
        return lines


class Display:
    """Repaints the stats block in place on a TTY, or logs plainly otherwise."""

    def __init__(self, stats: Stats, interactive: bool | None = None):
        self.stats = stats
        self.interactive = sys.stdout.isatty() if interactive is None else interactive
        self._lines_drawn = 0
        self._last = 0.0

    def refresh(self, force: bool = False) -> None:
        now = time.time()
        if not force and now - self._last < 0.5:
            return
        self._last = now
        lines = self.stats.render()
        if not self.interactive:
            return
        width = shutil.get_terminal_size((100, 24)).columns
        if self._lines_drawn:
            sys.stdout.write(f"\x1b[{self._lines_drawn}A")
        for line in lines:
            sys.stdout.write("\r" + line[: width - 1].ljust(width - 1) + "\n")
        sys.stdout.flush()
        self._lines_drawn = len(lines)

    def log(self, message: str) -> None:
        """Print a durable line above the live block."""
        if self.interactive and self._lines_drawn:
            sys.stdout.write(f"\x1b[{self._lines_drawn}A")
            sys.stdout.write("\x1b[J")
            self._lines_drawn = 0
        stamp = time.strftime("%H:%M:%S")
        print(f"[{stamp}] {message}", flush=True)
        self.refresh(force=True)

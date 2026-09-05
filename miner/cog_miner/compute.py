"""Compute backends for the useful-work task: C = A * B mod p.

Three backends, all producing bit-identical results:

* ``cuda-fast``  -- GPU, 8-bit limb decomposition on fp32 tensor cores.
  A and B are split into high/low bytes and the K dimension is processed in
  blocks of 256, so every fp32 accumulator stays below 255*255*256 < 2**24 and
  is therefore an exact integer. Partial blocks are summed in fp64.
* ``cuda-fp64``  -- GPU, plain fp64 matmul. Exact because every accumulator is
  below N*(p-1)**2 < 2**42 << 2**53. Slower on consumer cards, but universally
  correct; used as the reference the fast path is validated against.
* ``cpu``        -- NumPy fp64 BLAS, same exactness argument as ``cuda-fp64``.

Exactness is what makes the result reproducible on the validator, which redoes
selected rows in u64 integer arithmetic.
"""

from __future__ import annotations

import time
from dataclasses import dataclass

import numpy as np

from .protocol import N, P

try:  # torch is optional: the miner still runs (slower) on CPU without it.
    import torch

    TORCH_AVAILABLE = True
except Exception:  # pragma: no cover - depends on the host install
    torch = None
    TORCH_AVAILABLE = False


# K-block size for the fp32 limb path. 255*255*256 = 16_646_400 < 2**24.
FP32_K_BLOCK = 256


@dataclass
class DeviceInfo:
    backend: str
    name: str
    detail: str


def detect_device(preference: str = "auto", precision: str = "auto") -> DeviceInfo:
    """Pick a backend. ``preference`` is auto | cuda | cpu."""
    if preference in ("auto", "cuda") and TORCH_AVAILABLE and torch.cuda.is_available():
        idx = torch.cuda.current_device()
        props = torch.cuda.get_device_properties(idx)
        name = props.name
        vram = props.total_memory / (1024**3)
        detail = f"{vram:.1f} GiB VRAM, compute capability {props.major}.{props.minor}"
        backend = "cuda-fp64" if precision == "fp64" else "cuda-fast"
        return DeviceInfo(backend, name, detail)

    if preference == "cuda":
        raise RuntimeError(
            "CUDA was requested but is not available. Install a CUDA build of "
            "PyTorch (see https://pytorch.org/get-started/locally/) or run with "
            "--device cpu."
        )

    if TORCH_AVAILABLE:
        threads = torch.get_num_threads()
        return DeviceInfo("cpu", "CPU (torch fp64)", f"{threads} threads")
    return DeviceInfo("cpu", "CPU (numpy fp64 BLAS)", "no torch installed")


class Engine:
    """Stateful matmul engine bound to one backend."""

    def __init__(self, info: DeviceInfo):
        self.info = info
        self.backend = info.backend
        self.device = None
        if self.backend.startswith("cuda"):
            self.device = torch.device("cuda")
            # TF32 silently truncates fp32 mantissas to 10 bits, which would
            # destroy the exact-integer property the limb decomposition relies
            # on. Full precision is mandatory here.
            torch.backends.cuda.matmul.allow_tf32 = False
            torch.backends.cudnn.allow_tf32 = False
            try:
                torch.set_float32_matmul_precision("highest")
            except AttributeError:  # older torch releases
                pass

    # -- public API ---------------------------------------------------------

    def matmul(self, a: np.ndarray, b: np.ndarray) -> np.ndarray:
        """Return C = A*B mod p as a uint16 array of shape (N, N)."""
        if self.backend == "cuda-fast":
            return self._cuda_fast(a, b)
        if self.backend == "cuda-fp64":
            return self._cuda_fp64(a, b)
        return self._cpu(a, b)

    def selftest(self, a: np.ndarray, b: np.ndarray) -> None:
        """Prove the active backend agrees with the fp64 reference.

        Run once at startup: a silently wrong GPU kernel would burn electricity
        producing proofs the network rejects.
        """
        rows = [0, 1, N // 2, N - 1]
        got = self.matmul(a, b)
        ref = self._reference_rows(a, b, rows)
        for i, expected in zip(rows, ref):
            if not np.array_equal(got[i], expected):
                raise RuntimeError(
                    f"backend {self.backend} produced an incorrect row {i}; "
                    "refusing to mine with a miscomputing device"
                )

    @staticmethod
    def _reference_rows(a: np.ndarray, b: np.ndarray, rows: list[int]) -> list[np.ndarray]:
        bf = b.astype(np.float64)
        out = []
        for i in rows:
            acc = a[i].astype(np.float64) @ bf
            out.append(np.mod(acc, P).astype(np.uint16))
        return out

    # -- backends -----------------------------------------------------------

    def _cpu(self, a: np.ndarray, b: np.ndarray) -> np.ndarray:
        # max accumulator: N*(p-1)^2 = 2^42, exact in float64 (2^53 mantissa).
        acc = a.astype(np.float64) @ b.astype(np.float64)
        return np.mod(acc, P).astype(np.uint16)

    def _cuda_fp64(self, a: np.ndarray, b: np.ndarray) -> np.ndarray:
        ta = torch.from_numpy(a.astype(np.float64)).to(self.device)
        tb = torch.from_numpy(b.astype(np.float64)).to(self.device)
        acc = ta @ tb
        acc = acc - torch.floor(acc / P) * P
        return acc.to(torch.int64).cpu().numpy().astype(np.uint16)

    def _cuda_fast(self, a: np.ndarray, b: np.ndarray) -> np.ndarray:
        dev = self.device
        ta = torch.from_numpy(a.astype(np.int32)).to(dev)
        tb = torch.from_numpy(b.astype(np.int32)).to(dev)

        a_hi = torch.div(ta, 256, rounding_mode="floor").to(torch.float32)
        a_lo = (ta % 256).to(torch.float32)
        b_hi = torch.div(tb, 256, rounding_mode="floor").to(torch.float32)
        b_lo = (tb % 256).to(torch.float32)
        del ta, tb

        acc_hh = torch.zeros((N, N), dtype=torch.float64, device=dev)
        acc_hl = torch.zeros((N, N), dtype=torch.float64, device=dev)
        acc_lh = torch.zeros((N, N), dtype=torch.float64, device=dev)
        acc_ll = torch.zeros((N, N), dtype=torch.float64, device=dev)

        for start in range(0, N, FP32_K_BLOCK):
            stop = min(start + FP32_K_BLOCK, N)
            ah = a_hi[:, start:stop]
            al = a_lo[:, start:stop]
            bh = b_hi[start:stop, :]
            bl = b_lo[start:stop, :]
            # Each of these fp32 products has an exact integer accumulator.
            acc_hh += (ah @ bh).to(torch.float64)
            acc_hl += (ah @ bl).to(torch.float64)
            acc_lh += (al @ bh).to(torch.float64)
            acc_ll += (al @ bl).to(torch.float64)

        # Reduce each limb product first so the recombination stays small:
        # 15*p + 256*2*p + p is about 3.5e7, far inside fp64's exact range.
        def reduce(t):
            return t - torch.floor(t / P) * P

        acc_hh = reduce(acc_hh)
        acc_hl = reduce(acc_hl)
        acc_lh = reduce(acc_lh)
        acc_ll = reduce(acc_ll)

        # 2**16 mod p == 15, 2**8 == 256.
        combined = 15.0 * acc_hh + 256.0 * (acc_hl + acc_lh) + acc_ll
        combined = reduce(combined)
        return combined.to(torch.int64).cpu().numpy().astype(np.uint16)


def timed_matmul(engine: Engine, a: np.ndarray, b: np.ndarray) -> tuple[np.ndarray, float]:
    start = time.perf_counter()
    c = engine.matmul(a, b)
    if engine.device is not None:
        torch.cuda.synchronize()
    return c, time.perf_counter() - start


def task_flops() -> float:
    """Arithmetic operations in one task: one multiply and one add per MAC."""
    return 2.0 * (N**3)

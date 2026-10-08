"""Ignored development reference only: CPU PyTorch2.6 + NumPy2.2, never a product dependency.

Pass --checkpoint official model, --architecture official net_torch.py, --output ignored dir.
Both inputs are SHA256 pinned before architecture execution or weights-only checkpoint loading.
"""
import argparse
import hashlib
import importlib.util
from pathlib import Path

import numpy as np
import torch

CHECKPOINT = "9361614f3514d27351d81909f2215c0fdc38619c0288d936b7266485ac106c14"
ARCHITECTURE = "56290ec2bc430c58c16345bcac4ed14381b8f059fc1fe66428438bb0bb328970"


def verify(path, digest):
    assert hashlib.sha256(path.read_bytes()).hexdigest() == digest


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--checkpoint", type=Path, required=True)
    parser.add_argument("--architecture", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    verify(args.checkpoint, CHECKPOINT)
    verify(args.architecture, ARCHITECTURE)
    spec = importlib.util.spec_from_file_location("pmrid_official", args.architecture)
    official = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(official)
    model = official.Network()
    model.load_state_dict(torch.load(args.checkpoint, map_location="cpu", weights_only=True), strict=True)
    model.eval()
    torch.set_num_threads(2)
    args.output.mkdir(parents=True, exist_ok=True)
    values = np.array([(((i * 1664525 + 1013904223) & 0xFFFFFFFF) >> 16) / 65535 * 180 + 10 for i in range(4096)], dtype=np.float32)
    with torch.inference_mode():
        expected = model(torch.from_numpy(values.reshape(1, 4, 32, 32))).numpy()
    values.astype("<f4").tofile(args.output / "pmrid-reference-input.f32")
    expected.astype("<f4").tofile(args.output / "pmrid-reference-output.f32")

    # Independent reference for calibrated tiled Bayer packing, phase offsets, odd active area,
    # protected highlights/borders, K-sigma conversion and inverse sensor reconstruction.
    shot, read = 0.0010093132, 0.000003239
    anchor_shot = (0.0005995267 * 1600 + 0.00868861) / 959
    anchor_read = (7.11772e-7 * 1600 * 1600 + 6.514934e-4 * 1600 + 0.11492713) / 959**2
    scale = anchor_shot / shot
    shift = (read / shot**2 - anchor_read / anchor_shot**2) * anchor_shot
    for pattern in ["RGGB", "BGGR", "GRBG", "GBRG"]:
        pattern = np.array(list(pattern)).reshape(2, 2)
        image = np.empty((70, 72), dtype=np.float32)
        seed = 42917
        for y in range(70):
            for x in range(72):
                seed = (seed * 1664525 + 1013904223) & 0xFFFFFFFF
                noise = (np.float32(seed >> 16) / np.float32(65535) - np.float32(0.5)) * np.float32(0.06)
                base = {"R": 0.2, "G": 0.4, "B": 0.6}[pattern[y % 2, x % 2]]
                value = np.float32(1.1) if x >= 60 else np.float32(base) + noise
                image[y, x] = np.float32(256) + value * np.float32(15000)
        name = "".join(pattern.flatten())
        image.astype("<f4").tofile(args.output / f"pmrid-tile-{name}-input.f32")
        clean = image.copy()
        active = image[2:63, 3:66]
        local = np.roll(np.roll(pattern, -2 % 2, axis=0), -3 % 2, axis=1)
        ry, rx = np.argwhere(local == "R")[0]
        phases = [(rx, ry), (rx ^ 1, ry), (rx, ry ^ 1), (rx ^ 1, ry ^ 1)]
        for by in range(0, 31, 16):
            for bx in range(0, 32, 16):
                inp = np.zeros((1, 4, 48, 48), dtype=np.float32)
                for s, (px, py) in enumerate(phases):
                    for ty in range(48):
                        for tx in range(48):
                            gx, gy = min(max(bx + tx - 16, 0), 31), min(max(by + ty - 16, 0), 30)
                            xx, yy = gx * 2 + px, gy * 2 + py
                            if xx >= 63:
                                xx -= 2
                            if yy >= 61:
                                yy -= 2
                            normalized = float(np.clip((active[yy, xx] - np.float32(256)) / np.float32(15000), 0, 1))
                            inp[0, s, ty, tx] = (normalized * scale + shift) * 256
                with torch.inference_mode():
                    pred = model(torch.from_numpy(inp)).numpy()[0]
                for s, (px, py) in enumerate(phases):
                    for ty in range(min(16, 31 - by)):
                        for tx in range(min(16, 32 - bx)):
                            xx, yy = (bx + tx) * 2 + px, (by + ty) * 2 + py
                            if xx >= 63 or yy >= 61:
                                continue
                            old = (active[yy, xx] - np.float32(256)) / np.float32(15000)
                            if not 0 <= old < 0.98:
                                continue
                            predicted = np.float32((float(pred[s, ty + 16, tx + 16]) / 256 - shift) / scale)
                            clean[yy + 2, xx + 3] = (old + (predicted - old) * np.float32(0.7)) * np.float32(15000) + np.float32(256)
        clean.astype("<f4").tofile(args.output / f"pmrid-tile-{name}-output.f32")
    print("Wrote official fixed tensor plus four calibrated Bayer tile references", args.output)


if __name__ == "__main__":
    main()

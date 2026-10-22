"""Build DarkslideUpscale.mlpackage (RealESRGAN_x2plus, float16) for AI Upscale.

`scripts/build-app.sh` runs it through `build-model.sh`, in the cached venv of the AI
Denoise model (the same hash-locked requirements, see docs/UPSCALE_MODEL.md). It
downloads the official release weights pinned below (checked by SHA-256), converts them
with a fixed 256x256 float16 tensor input and 512x512 output ("input" -> "output", planar
RGB, sRGB-encoded 0..1, the layout ds-pipeline's upscale tiles use), checks the Core ML
output against PyTorch float32 on the GPU and the Neural Engine, and checks that the
converted weights are the pinned ones (EXPECTED_WEIGHTS_SHA256), leaving
OUT_DIR/DarkslideUpscale.mlpackage.
"""
import hashlib
import os
import sys
import time
import urllib.request

import coremltools as ct
import numpy as np
import torch

import rrdbnet

WEIGHTS_URL = "https://github.com/xinntao/Real-ESRGAN/releases/download/v0.2.1/RealESRGAN_x2plus.pth"
WEIGHTS_SHA256 = "49fafd45f8fd7aa8d31ab2a22d14d91b536c34494a5cfe31eb5d89c2fa266abb"
# SHA-256 of the converted package's weights (Data/com.apple.CoreML/weights/weight.bin).
EXPECTED_WEIGHTS_SHA256 = "d43bffce05fa1714f1a50e6ef19780f0ef5453a7e9448875316a4d564f46a310"
TILE = 256
SCALE = 2
NAME = "DarkslideUpscale"


def sha256(path):
    h = hashlib.sha256()
    with open(path, "rb") as f:
        for chunk in iter(lambda: f.read(1 << 20), b""):
            h.update(chunk)
    return h.hexdigest()


def main(out):
    os.makedirs(out, exist_ok=True)
    pth = os.path.join(out, "RealESRGAN_x2plus.pth")
    # A copy that doesn't match (truncated, damaged) is downloaded again, once; the
    # download goes to a temporary name and is renamed only when complete.
    for attempt in range(2):
        if not os.path.exists(pth) or sha256(pth) != WEIGHTS_SHA256:
            if os.path.exists(pth):
                os.remove(pth)
            part = pth + ".part"
            urllib.request.urlretrieve(WEIGHTS_URL, part)
            os.replace(part, pth)
        if sha256(pth) == WEIGHTS_SHA256:
            break
    else:
        sys.exit(f"checksum mismatch for {pth} after downloading it again")
    net = rrdbnet.RRDBNet()
    state = torch.load(pth, map_location="cpu", weights_only=True)
    missing, unexpected = net.load_state_dict(state.get("params_ema", state), strict=False)
    # The unshuffle convolution's weights are fixed (a buffer), not in the checkpoint.
    if unexpected or set(missing) != {"unshuffle.weight"}:
        sys.exit(f"checkpoint mismatch: missing {missing}, unexpected {unexpected}")
    net.eval()

    # The rewritten unshuffle is exact.
    probe = torch.rand(1, 3, 8, 8)
    if not torch.equal(net.unshuffle(probe), torch.nn.functional.pixel_unshuffle(probe, 2)):
        sys.exit("pixel unshuffle rewrite differs")

    traced = torch.jit.trace(net, torch.rand(1, 3, TILE, TILE))
    ml = ct.convert(
        traced,
        inputs=[ct.TensorType(name="input", shape=(1, 3, TILE, TILE), dtype=np.float16)],
        outputs=[ct.TensorType(name="output", dtype=np.float16)],
        compute_precision=ct.precision.FLOAT16,
        minimum_deployment_target=ct.target.macOS14,
    )
    ml.author = "Xintao Wang et al. (Real-ESRGAN, BSD-3-Clause; BasicSR, Apache-2.0); converted for Darkslide"
    ml.license = "BSD-3-Clause"
    ml.short_description = (f"RealESRGAN_x2plus 2x super-resolution, {TILE}x{TILE} planar RGB float16 "
                            f"(sRGB-encoded 0..1) in, {TILE * SCALE}x{TILE * SCALE} out.")
    ml.version = "1"
    pkg = os.path.join(out, f"{NAME}.mlpackage")
    ml.save(pkg)

    # Check against PyTorch float32 on a textured image with edges.
    torch.manual_seed(0)
    x = torch.nn.functional.interpolate(torch.rand(1, 3, 16, 16), size=TILE, mode="bilinear")
    x = (0.7 * x + 0.3 * (torch.rand(1, 3, TILE, TILE) > 0.5).float() * x).clamp(0, 1)
    with torch.no_grad():
        ref = net(x).numpy()
    if ref.shape != (1, 3, TILE * SCALE, TILE * SCALE):
        sys.exit(f"unexpected output shape {ref.shape}")
    for cu in (ct.ComputeUnit.CPU_AND_GPU, ct.ComputeUnit.CPU_AND_NE):
        m = ct.models.MLModel(pkg, compute_units=cu)
        inp = {"input": x.numpy().astype(np.float16)}
        y = m.predict(inp)["output"]
        t0 = time.perf_counter()
        for _ in range(3):
            m.predict(inp)
        ms = (time.perf_counter() - t0) / 3 * 1000
        err = np.abs(y.astype(np.float32) - ref)
        print(f"{cu}: max |error| {err.max():.5f}, rmse {np.sqrt((err ** 2).mean()):.6f}, {ms:.0f} ms per tile")
        if not np.isfinite(y).all() or err.max() > 0.05 or np.sqrt((err ** 2).mean()) > 0.005:
            sys.exit("Core ML output differs from PyTorch")

    weights = None
    for root, _, files in os.walk(pkg):
        for f in sorted(files):
            p = os.path.join(root, f)
            print(f"{os.path.relpath(p, pkg)}  {sha256(p)}  {os.path.getsize(p)}")
            if f == "weight.bin":
                weights = sha256(p)
    # The app checks the bundled package against this (UpscaleModelDescriptor.weightsSHA256):
    # with the pinned dependencies (requirements.txt) the converted weights are bit-exact.
    if weights != EXPECTED_WEIGHTS_SHA256:
        sys.exit(f"converted weights {weights} != pinned {EXPECTED_WEIGHTS_SHA256}: check requirements.txt")
    print(f"model id {weights}")


if __name__ == "__main__":
    main(sys.argv[1] if len(sys.argv) > 1 else "upscale-model-out")

"""Build DarkslideDenoise.mlpackage (NAFNet-SIDD-width32, float16) for AI Denoise.

`scripts/build-app.sh` runs it through `build-model.sh`, in a cached venv with the
hash-locked requirements.txt (see docs/DENOISE_MODEL.md). It downloads the official
checkpoint mirror pinned below (checked by SHA-256), converts
it with a fixed 512x512 float16 tensor input/output ("input" -> "output", planar RGB,
sRGB-encoded 0..1, the layout ds-pipeline's tiles use), checks the Core ML output against
PyTorch float32 on the GPU and the Neural Engine, and checks that the converted weights
are the pinned ones (EXPECTED_WEIGHTS_SHA256), leaving OUT_DIR/DarkslideDenoise.mlpackage.
"""
import hashlib
import os
import sys
import urllib.request

import coremltools as ct
import numpy as np
import torch

import nafnet

WEIGHTS_URL = ("https://huggingface.co/mikestealth/nafnet-models/resolve/"
               "cd3a91ddc41c9c3a53ee809b3ddd5263c9e94e51/NAFNet-SIDD-width32.pth")
WEIGHTS_SHA256 = "89c70e808d1783b6c07911306e106aaf0d4f7f3da8c61078b99ff7f8929a26f4"
# SHA-256 of the converted package's weights (Data/com.apple.CoreML/weights/weight.bin).
EXPECTED_WEIGHTS_SHA256 = "90a2538b1881275d5aa964b6aa6451baffe32b3167db3a526d9a123a3e818d5f"
TILE = 512
NAME = "DarkslideDenoise"


def sha256(path):
    h = hashlib.sha256()
    with open(path, "rb") as f:
        for chunk in iter(lambda: f.read(1 << 20), b""):
            h.update(chunk)
    return h.hexdigest()


def main(out):
    os.makedirs(out, exist_ok=True)
    pth = os.path.join(out, "NAFNet-SIDD-width32.pth")
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
    net = nafnet.NAFNet()
    state = torch.load(pth, map_location="cpu", weights_only=True)
    net.load_state_dict(state.get("params", state), strict=True)
    net.eval()

    traced = torch.jit.trace(net, torch.rand(1, 3, TILE, TILE))
    ml = ct.convert(
        traced,
        inputs=[ct.TensorType(name="input", shape=(1, 3, TILE, TILE), dtype=np.float16)],
        outputs=[ct.TensorType(name="output", dtype=np.float16)],
        compute_precision=ct.precision.FLOAT16,
        minimum_deployment_target=ct.target.macOS14,
    )
    ml.author = "megvii-research (NAFNet, MIT); converted for Darkslide"
    ml.license = "MIT"
    ml.short_description = (f"NAFNet-SIDD-width32 real-noise denoiser, {TILE}x{TILE} planar RGB "
                            "float16 (sRGB-encoded 0..1) in and out.")
    ml.version = "1"
    pkg = os.path.join(out, f"{NAME}.mlpackage")
    ml.save(pkg)

    # Check against PyTorch float32 on a noisy smooth image.
    torch.manual_seed(0)
    x = torch.nn.functional.interpolate(torch.rand(1, 3, 32, 32), size=TILE, mode="bilinear")
    x = (x + 0.08 * torch.randn_like(x)).clamp(0, 1)
    with torch.no_grad():
        ref = net(x).numpy()
    for cu in (ct.ComputeUnit.CPU_AND_GPU, ct.ComputeUnit.CPU_AND_NE):
        y = ct.models.MLModel(pkg, compute_units=cu).predict({"input": x.numpy().astype(np.float16)})["output"]
        err = np.abs(y.astype(np.float32) - ref)
        print(f"{cu}: max |error| {err.max():.5f}, rmse {np.sqrt((err ** 2).mean()):.6f}")
        if not np.isfinite(y).all() or err.max() > 0.01:
            sys.exit("Core ML output differs from PyTorch")

    weights = None
    for root, _, files in os.walk(pkg):
        for f in sorted(files):
            p = os.path.join(root, f)
            print(f"{os.path.relpath(p, pkg)}  {sha256(p)}  {os.path.getsize(p)}")
            if f == "weight.bin":
                weights = sha256(p)
    # The app checks the bundled package against this (DenoiseModelDescriptor.weightsSHA256):
    # with the pinned dependencies (requirements.txt) the converted weights are bit-exact.
    if weights != EXPECTED_WEIGHTS_SHA256:
        sys.exit(f"converted weights {weights} != pinned {EXPECTED_WEIGHTS_SHA256}: check requirements.txt")
    print(f"model id {weights}")


if __name__ == "__main__":
    main(sys.argv[1] if len(sys.argv) > 1 else "denoise-model-out")

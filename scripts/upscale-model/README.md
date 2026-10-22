# AI Upscale model

`build-model.sh` builds the bundled RealESRGAN_x2plus float16 package. It uses `convert.py`, `rrdbnet.py` and the AI Denoise model's hash-locked `requirements.txt` (`../denoise-model/`). `LICENSE-Real-ESRGAN` is the upstream BSD 3-Clause licence.

Sources, pins, cache, reproducibility and runtime checks are in [docs/UPSCALE_MODEL.md](../../docs/UPSCALE_MODEL.md).

"""Process-local compatibility for auxiliary Gemma 4 MTP checkpoint weights.

mlx-lm 0.32.0 does not implement this speculative head. Keep strict loading of
the actual decoder and ignore only the auxiliary prefixes, as proposed upstream:
https://github.com/ml-explore/mlx-lm/pull/2026
This does not enable MTP acceleration or modify checkpoints/site-packages.
"""

import functools as _studio_functools
import importlib as _studio_importlib
import importlib.metadata as _studio_metadata
import json as _studio_json
from pathlib import Path as _StudioPath


def _studio_gemma4_mtp_key(key):
    key = key.removeprefix("model.")
    return key.startswith(("mtp.", "language_model.mtp."))


def _studio_patch_gemma4_sanitize(model_class):
    original = model_class.sanitize
    if getattr(original, "_studio_gemma4_mtp_compat", False):
        return False

    @_studio_functools.wraps(original)
    def sanitize(self, weights):
        owners = (self, getattr(self, "model", None), getattr(self, "language_model", None))
        native_mtp = any(owner is not None and hasattr(owner, "mtp") for owner in owners)
        if getattr(self, "model_type", None) not in ("gemma4", "gemma4_text") or native_mtp:
            return original(self, weights)
        filtered = {key: value for key, value in weights.items() if not _studio_gemma4_mtp_key(key)}
        skipped = len(weights) - len(filtered)
        if skipped:
            import sys

            print(
                f"[locals-studio] Gemma 4: ignoring {skipped} auxiliary MTP tensors; "
                "using standard decoding without MTP acceleration.",
                file=sys.stderr,
            )
        return original(self, filtered if skipped else weights)

    sanitize._studio_gemma4_mtp_compat = True
    model_class.sanitize = sanitize
    return True


def _studio_install_gemma4_mtp_compat():
    try:
        version = _studio_metadata.version("mlx-lm")
    except _studio_metadata.PackageNotFoundError:
        return False
    if version != "0.32.0":
        return False
    changed = False
    for name in ("gemma4", "gemma4_text"):
        module = _studio_importlib.import_module(f"mlx_lm.models.{name}")
        changed = _studio_patch_gemma4_sanitize(module.Model) or changed
    return changed


def _studio_prepare_gemma4_mtp_compat(arguments):
    # Studio always supplies a local model directory. Other architectures do not
    # import Gemma/MLX modules or receive a sanitizer patch at all.
    try:
        model_path = arguments[arguments.index("--model") + 1]
        config = _studio_json.loads((_StudioPath(model_path) / "config.json").read_text())
    except (ValueError, IndexError, OSError):
        return False  # The normal Locals loader reports invalid model paths/configs.
    if not isinstance(config, dict) or config.get("model_type") not in ("gemma4", "gemma4_text"):
        return False
    return _studio_install_gemma4_mtp_compat()

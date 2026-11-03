"""CPU-only regression tests; never import MLX or read any model tensor data."""

import contextlib
import io
import json
from pathlib import Path
import runpy
from types import SimpleNamespace
import unittest
import tempfile
from unittest import mock


ROOT = Path(__file__).resolve().parent
COMPAT = runpy.run_path(str(ROOT / "gemma4_mtp_compat.py"))


def model_class(model_type="gemma4", expected=None):
    class Model:
        def __init__(self):
            self.model_type = model_type
            self.received = None

        def sanitize(self, weights):
            self.received = weights
            if expected is not None and set(weights) != set(expected):
                raise ValueError("Unexpected decoder weights")
            return weights

    return Model


class GemmaMtpCompatibilityTests(unittest.TestCase):
    def test_real_checkpoint_48_auxiliary_names_only_are_ignored(self):
        # Public index at Jundot/gemma-4-26B-A4B-it-oQ8e-mtp revision
        # e91e21aa6826dba4dc814031020de1d20d33a88f (names only, no tensors).
        keys = json.loads((ROOT / "gemma4_mtp_weight_keys.json").read_text())
        self.assertEqual(len(keys), 48)
        decoder = {"language_model.model.layers.0.self_attn.q_proj.weight": object()}
        weights = {**decoder, **{key: object() for key in keys}}
        Model = model_class(expected=decoder)
        COMPAT["_studio_patch_gemma4_sanitize"](Model)
        log = io.StringIO()
        with contextlib.redirect_stderr(log):
            result = Model().sanitize(weights)
        self.assertEqual(result, decoder)
        self.assertEqual(set(weights), set(decoder) | set(keys), "input is not mutated")
        self.assertIn("ignoring 48 auxiliary MTP tensors", log.getvalue())
        self.assertIn("without MTP acceleration", log.getvalue())
        with self.assertRaisesRegex(ValueError, "Unexpected decoder weights"):
            Model().sanitize({**decoder, "unrelated.unexpected.weight": object()})

    def test_gemma_text_prefixes_are_bounded_and_unrelated_names_are_retained(self):
        Model = model_class("gemma4_text")
        COMPAT["_studio_patch_gemma4_sanitize"](Model)
        retained = {"model.layers.0.mtp_adapter.weight": object(), "language_model.mtp_extra.weight": object()}
        weights = {**retained, **{prefix + "head.weight": object() for prefix in ("mtp.", "model.mtp.", "language_model.mtp.", "model.language_model.mtp.")}}
        with contextlib.redirect_stderr(io.StringIO()):
            self.assertEqual(Model().sanitize(weights), retained)

    def test_other_models_native_mtp_and_no_mtp_calls_are_unchanged(self):
        weights = {"model.layers.0.weight": object(), "mtp.head.weight": object()}
        Other = model_class("qwen3")
        COMPAT["_studio_patch_gemma4_sanitize"](Other)
        self.assertIs(Other().sanitize(weights), weights)
        Native = model_class()
        COMPAT["_studio_patch_gemma4_sanitize"](Native)
        native = Native()
        native.mtp = object()
        self.assertIs(native.sanitize(weights), weights)
        plain = {"model.layers.0.weight": object()}
        self.assertIs(Native().sanitize(plain), plain)

    def test_installation_is_version_scoped_and_idempotent(self):
        classes = {name: model_class(name) for name in ("gemma4", "gemma4_text")}
        modules = {f"mlx_lm.models.{name}": SimpleNamespace(Model=cls) for name, cls in classes.items()}
        metadata = COMPAT["_studio_metadata"]
        imports = COMPAT["_studio_importlib"]
        with mock.patch.object(metadata, "version", return_value="0.33.0"), mock.patch.object(imports, "import_module") as load:
            self.assertFalse(COMPAT["_studio_install_gemma4_mtp_compat"]())
            load.assert_not_called()
        with mock.patch.object(metadata, "version", return_value="0.32.0"), mock.patch.object(imports, "import_module", side_effect=modules.__getitem__):
            self.assertTrue(COMPAT["_studio_install_gemma4_mtp_compat"]())
            methods = {name: cls.sanitize for name, cls in classes.items()}
            self.assertFalse(COMPAT["_studio_install_gemma4_mtp_compat"]())
            for name, cls in classes.items():
                self.assertIs(cls.sanitize, methods[name])

    def test_worker_only_prepares_the_selected_gemma_architecture(self):
        prepare = COMPAT["_studio_prepare_gemma4_mtp_compat"]
        with tempfile.TemporaryDirectory() as directory:
            config = Path(directory) / "config.json"
            with mock.patch.dict(prepare.__globals__, {"_studio_install_gemma4_mtp_compat": mock.Mock(return_value=True)}):
                install = prepare.__globals__["_studio_install_gemma4_mtp_compat"]
                config.write_text('{"model_type":"qwen3"}')
                self.assertFalse(prepare(["--model", directory]))
                install.assert_not_called()
                config.write_text('{"model_type":"gemma4"}')
                self.assertTrue(prepare(["--model", directory]))
                install.assert_called_once_with()


if __name__ == "__main__":
    unittest.main()

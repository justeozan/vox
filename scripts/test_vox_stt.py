"""Tests du démon STT (src-tauri/resources/vox_stt.py) sans moteur réel.
Lancer : python3 scripts/test_vox_stt.py
"""
import importlib.util
import sys
import types
import unittest
from pathlib import Path

# numpy / soundfile ne sont requis que dans le venv de Vox : on les remplace.
for name in ("numpy", "soundfile"):
    sys.modules.setdefault(name, types.ModuleType(name))
sys.modules["numpy"].ndarray = object

SCRIPT = Path(__file__).resolve().parent.parent / "src-tauri/resources/vox_stt.py"
spec = importlib.util.spec_from_file_location("vox_stt", SCRIPT)
stt = importlib.util.module_from_spec(spec)
spec.loader.exec_module(stt)


class EngineFallback(unittest.TestCase):
    def test_default_engine_is_parakeet(self):
        self.assertEqual(stt.MODEL_NAME, "parakeet-v3")

    def test_order_falls_back_to_whisper_small(self):
        self.assertEqual(stt.engine_order("parakeet-v3"), ["parakeet-v3", "whisper-small"])
        self.assertEqual(stt.engine_order("whisper-small"), ["whisper-small"])

    def test_wanted_engine_used_when_installed(self):
        name, fn = stt.resolve_engine("parakeet-v3", lambda n: f"fn-{n}")
        self.assertEqual((name, fn), ("parakeet-v3", "fn-parakeet-v3"))

    def test_missing_engine_falls_back_without_crash(self):
        def loader(n):
            if n != "whisper-small":
                raise ImportError("no parakeet_mlx")
            return "fn-small"
        self.assertEqual(stt.resolve_engine("parakeet-v3", loader), ("whisper-small", "fn-small"))

    def test_nothing_installed_raises_import_error(self):
        def loader(n):
            raise ImportError(n)
        with self.assertRaises(ImportError):
            stt.resolve_engine("parakeet-v3", loader)


class Hallucinations(unittest.TestCase):
    def test_filter_kept(self):
        self.assertTrue(stt.is_hallucination("Merci d'avoir regardé cette vidéo."))
        self.assertTrue(stt.is_hallucination("  "))
        self.assertFalse(stt.is_hallucination("lance un agent sur findy"))


if __name__ == "__main__":
    unittest.main()

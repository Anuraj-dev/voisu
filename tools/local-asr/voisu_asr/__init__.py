"""Private development ASR runtime used by the supervised local worker.

This package intentionally has no import-time dependency on any model runtime.
Adapters import their heavyweight libraries only after a validated manifest has
been loaded and the worker is preparing a selected model.
"""

from .manifest import Manifest, ManifestError, load_manifest
from .protocol import MAX_PCM_BYTES, ProtocolError

__all__ = [
    "MAX_PCM_BYTES",
    "Manifest",
    "ManifestError",
    "ProtocolError",
    "load_manifest",
]

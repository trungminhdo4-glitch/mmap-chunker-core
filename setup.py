"""Setuptools build for the mmap-chunker-core Python distribution.

Builds a ``py3-none-<platform>`` wheel around the existing stable C ABI
shared library. The native library is copied from the Cargo build output into
the package ``_native/`` directory during the wheel build. Source installs
(from the sdist) build the library with Cargo when it is absent.

Metadata lives here because the version is read from Cargo.toml (single source
of truth) and the wheel needs a custom ``bdist_wheel`` subclass to produce the
correct platform-specific pure-Python tag.
"""

from __future__ import annotations

import os
import platform
import re
import shutil
import subprocess
import sys
from pathlib import Path

from setuptools import setup
from setuptools.command.build_py import build_py
from wheel.bdist_wheel import bdist_wheel

ROOT = Path(__file__).resolve().parent
PKG_DIR = ROOT / "python" / "mmap_chunker"
NATIVE_DIR = PKG_DIR / "_native"


def _read_version() -> str:
    cargo = (ROOT / "Cargo.toml").read_text(encoding="utf-8")
    match = re.search(r'^version = "([^"]+)"', cargo, re.MULTILINE)
    if match is None:
        raise RuntimeError("could not read package version from Cargo.toml")
    return match.group(1)


def _library_name() -> str:
    if os.name == "nt":
        return "mmap_chunker_core.dll"
    if sys.platform == "darwin":
        return "libmmap_chunker_core.dylib"
    if sys.platform.startswith("linux"):
        return "libmmap_chunker_core.so"
    raise RuntimeError(f"unsupported platform for the native library: {sys.platform}")


def _build_native_library() -> None:
    """Build the cdylib with Cargo (source-install path)."""
    subprocess.run(["cargo", "build", "--release"], cwd=ROOT, check=True)


def _prepare_native_library() -> None:
    """Copy the built shared library into the package tree for the wheel."""
    name = _library_name()
    src = ROOT / "target" / "release" / name
    if not src.is_file():
        _build_native_library()
    if not src.is_file():
        raise RuntimeError(
            f"native library not found at {src}; build it with `cargo build --release`"
        )
    NATIVE_DIR.mkdir(parents=True, exist_ok=True)
    shutil.copy2(src, NATIVE_DIR / name)


class BuildPy(build_py):
    """Ensure the native library is present before packaging .py files."""

    def run(self) -> None:
        _prepare_native_library()
        super().run()


class PlatformWheel(bdist_wheel):
    """Force a pure-Python platform wheel: ``py3-none-<platform>``.

    The package contains no CPython extension module (the native library is
    consumed via ctypes through the C ABI), so the Python tag must be ``py3``
    and the ABI tag ``none``. Only the platform tag varies per artifact.
    """

    def finalize_options(self) -> None:
        super().finalize_options()
        self.root_is_pure = False
        plat = os.environ.get("MMAP_CHUNKER_PLAT_TAG")
        if plat:
            self.plat_name = plat

    def get_tag(self) -> tuple[str, str, str]:
        _python, _abi, plat = super().get_tag()
        return ("py3", "none", plat)


def _long_description() -> str:
    readme = ROOT / "README.md"
    if readme.is_file():
        return readme.read_text(encoding="utf-8")
    return ""


if __name__ == "__main__":
    setup(
        name="mmap-chunker-core",
        version=_read_version(),
        description=(
            "Record-aligned byte-range planning for large immutable files via "
            "a stable C ABI; zero-dependency Python access through ctypes."
        ),
        long_description=_long_description(),
        long_description_content_type="text/markdown",
        license="MIT OR Apache-2.0",
        url="https://github.com/trungminhdo4-glitch/mmap-chunker-core",
        project_urls={
            "Source": "https://github.com/trungminhdo4-glitch/mmap-chunker-core",
        },
        python_requires=">=3.10",
        packages=["mmap_chunker", "mmap_chunker.integrations"],
        package_dir={"": "python"},
        package_data={"mmap_chunker": ["py.typed", "_native/*"]},
        cmdclass={"build_py": BuildPy, "bdist_wheel": PlatformWheel},
        classifiers=[
            "Programming Language :: Python :: 3",
            "Programming Language :: Python :: 3.10",
            "Programming Language :: Python :: 3.11",
            "Programming Language :: Python :: 3.12",
            "Programming Language :: Python :: 3.13",
            "Programming Language :: Python :: 3.14",
            "License :: OSI Approved :: MIT License",
            "License :: OSI Approved :: Apache Software License",
            "Operating System :: OS Independent",
            "Topic :: System :: Filesystems",
        ],
        extras_require={
            "datatrove": ["datatrove", "orjson"],
        },
    )

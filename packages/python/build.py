#!/usr/bin/env python3
"""Stages the Python wheel from a built ``dist/``, and optionally publishes it.

::

    python packages/python/build.py                    # build the wheel for this platform
    python packages/python/build.py --sdist            # and a source distribution
    python packages/python/build.py --publish          # upload it to PyPI with twine
    python packages/python/build.py --publish --test   # upload to TestPyPI instead

The wheel is **platform-specific**, because it carries the four executables and
the C ABI library. That is deliberate and it is what makes ``pip install
inillucent`` work with no compiler, no Rust toolchain and no network access past
the index. It also means one wheel per platform, built on that platform, and
this script builds the one for the machine it is run on.

``driver.py`` is copied here from ``drivers/bindings/python/inillucent.py``
rather than being written twice. That file is the reference binding
``drivers/README.md`` tells a binding author to read, and a second copy that
drifted would make the documentation wrong about the thing it points at. The
copy is a build step, and this script is the only thing that makes it.
"""

from __future__ import annotations

import argparse
import shutil
import subprocess
import sys
import sysconfig
from pathlib import Path

HERE = Path(__file__).resolve().parent
ROOT = HERE.parent.parent
PROGRAMS = ("inillucent", "inillucent-shell", "inillucent-mcp", "inillucent-migrate")
LIBRARIES = (
    "inillucent_driver_capi.dll",
    "libinillucent_driver_capi.dylib",
    "libinillucent_driver_capi.so",
)


def workspace_version() -> str:
    """Read the version out of the workspace manifest."""
    inside = False
    for line in (ROOT / "Cargo.toml").read_text(encoding="utf-8").splitlines():
        if line.startswith("[workspace.package]"):
            inside = True
            continue
        if inside and line.startswith("["):
            break
        if inside and line.startswith("version"):
            return line.split("=", 1)[1].strip().strip('"')
    raise SystemExit("Cargo.toml declares no [workspace.package] version")


def host_target() -> str:
    """Ask rustc what triple this machine builds for."""
    reported = subprocess.run(["rustc", "-vV"], capture_output=True, text=True, check=True)
    for line in reported.stdout.splitlines():
        if line.startswith("host:"):
            return line.split(":", 1)[1].strip()
    raise SystemExit("rustc did not report a host triple")


def platform_tag() -> str:
    """Return the wheel platform tag for this machine.

    ``sysconfig``'s own, normalised the way a wheel file name needs it. On Linux
    this produces ``linux_x86_64``, which PyPI refuses - a Linux wheel has to be
    a ``manylinux`` one, built in the appropriate container. The script says so
    rather than uploading something that will be rejected after the fact.
    """
    tag = sysconfig.get_platform().replace("-", "_").replace(".", "_")
    if tag.startswith("linux_"):
        print(
            f"warning: PyPI will refuse a '{tag}' wheel. A Linux wheel has to be built\n"
            f"         in a manylinux container and tagged manylinux_2_28_x86_64 or similar.\n"
            f"         See packaging/README.md.",
            file=sys.stderr,
        )
    return tag


def stage(version: str, target: str) -> None:
    """Copy the binaries, the library and the reference binding into the package.

    :param version: the release version
    :param target: the Rust target triple whose archive to stage from
    """
    source = ROOT / "dist" / f"inillucent-{version}-{target}"
    if not source.is_dir():
        raise SystemExit(
            f"{source} does not exist.\n"
            f"Run packaging/release.ps1 (Windows) or packaging/release.sh first."
        )
    package = HERE / "src" / "inillucent"
    for directory in ("_bin", "_lib", "_include"):
        shutil.rmtree(package / directory, ignore_errors=True)
        (package / directory).mkdir(parents=True, exist_ok=True)

    suffix = ".exe" if sys.platform == "win32" else ""
    for program in PROGRAMS:
        wanted = source / "bin" / f"{program}{suffix}"
        shutil.copy2(wanted, package / "_bin" / wanted.name)
        (package / "_bin" / wanted.name).chmod(0o755)

    copied = 0
    for name in LIBRARIES:
        candidate = source / "lib" / name
        if candidate.is_file():
            shutil.copy2(candidate, package / "_lib" / name)
            copied += 1
    if copied == 0:
        raise SystemExit(f"{source / 'lib'} holds no C ABI library")

    shutil.copy2(source / "include" / "inillucent_driver.h", package / "_include")

    # The reference binding, copied rather than written twice. See the module
    # docstring: `drivers/README.md` points a binding author at that file, and a
    # divergent copy here would make the documentation wrong.
    reference = ROOT / "drivers" / "bindings" / "python" / "inillucent.py"
    if not reference.is_file():
        raise SystemExit(f"the reference binding is missing: {reference}")
    shutil.copy2(reference, package / "driver.py")

    shutil.copy2(ROOT / "LICENSE", HERE / "LICENSE")
    print(f"staged {len(PROGRAMS)} programs, {copied} library, and the reference binding")


def build(tag: str, sdist: bool) -> None:
    """Build the wheel, and optionally a source distribution.

    :param tag: the wheel platform tag
    :param sdist: whether to build an sdist as well
    """
    shutil.rmtree(HERE / "dist", ignore_errors=True)
    shutil.rmtree(HERE / "build", ignore_errors=True)
    # `setup.py bdist_wheel --plat-name` rather than `python -m build`, because
    # this package has no compiled extension - setuptools would mark the wheel
    # `py3-none-any` and pip would install a Windows binary on a Mac. Naming the
    # platform is the whole point.
    subprocess.run(
        [sys.executable, "setup.py", "bdist_wheel", "--plat-name", tag],
        cwd=HERE,
        check=True,
    )
    if sdist:
        subprocess.run([sys.executable, "setup.py", "sdist"], cwd=HERE, check=True)
    for made in sorted((HERE / "dist").glob("*")):
        print(f"  {made.name}")


def publish(test: bool) -> None:
    """Upload what is in dist/ with twine.

    :param test: whether to upload to TestPyPI rather than PyPI
    """
    command = [sys.executable, "-m", "twine", "upload"]
    if test:
        command += ["--repository", "testpypi"]
    command.append(str(HERE / "dist" / "*"))
    print("  " + " ".join(command))
    subprocess.run(command, cwd=HERE, check=True, shell=sys.platform == "win32")


def main() -> None:
    """Stage, build and optionally publish."""
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--version", help="the release version; defaults to the workspace's")
    parser.add_argument("--target", help="the Rust target triple; defaults to the host's")
    parser.add_argument("--sdist", action="store_true", help="build a source distribution too")
    parser.add_argument("--publish", action="store_true", help="upload to PyPI with twine")
    parser.add_argument("--test", action="store_true", help="with --publish, upload to TestPyPI")
    parsed = parser.parse_args()

    version = parsed.version or workspace_version()
    target = parsed.target or host_target()
    print(f"inillucent {version} for {target}")
    stage(version, target)
    build(platform_tag(), parsed.sdist)
    if parsed.publish:
        publish(parsed.test)


if __name__ == "__main__":
    main()

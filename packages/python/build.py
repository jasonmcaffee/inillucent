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
the index.

It used to mean one wheel per platform *built on that platform*, and the effect
was that only the Windows wheel ever reached PyPI - so ``pip install inillucent``
on a Mac or on Linux answered "no matching distribution". Nothing about a wheel
needs the machine it targets: it is an archive of files plus a platform tag, and
the release already builds every platform's binaries here. ``--target`` stages
from any archive in ``dist/`` and tags the wheel accordingly::

    python packages/python/build.py --target aarch64-unknown-linux-gnu

The ``manylinux_2_28`` tags are accurate rather than asserted: the Linux targets
are built through ``cargo-zigbuild`` with a pinned glibc floor of 2.28, which is
exactly what that tag claims. ``tools/release-verify-linux.sh`` reads the highest
``GLIBC_`` symbol version out of the archive and fails if it is higher.

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

#: The wheel platform tag for each release target.
#:
#: macOS is one wheel rather than two: the release builds a universal binary, so ``universal2``
#: describes it exactly and pip installs it on either architecture. ``13_0`` is the minimum the
#: binaries declare - tagging it lower would install on a Mac they refuse to start on.
WHEEL_TAGS = {
    "x86_64-pc-windows-msvc": "win_amd64",
    "aarch64-apple-darwin": "macosx_13_0_universal2",
    "x86_64-apple-darwin": "macosx_13_0_universal2",
    "x86_64-unknown-linux-gnu": "manylinux_2_28_x86_64",
    "aarch64-unknown-linux-gnu": "manylinux_2_28_aarch64",
}

#: The archive a target's binaries come from, when it is not the target's own.
#: Both macOS wheels are cut from the one universal archive the release builds.
SOURCE_ARCHIVES = {
    "aarch64-apple-darwin": "universal-apple-darwin",
    "x86_64-apple-darwin": "universal-apple-darwin",
}
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
    source = ROOT / "dist" / f"inillucent-{version}-{SOURCE_ARCHIVES.get(target, target)}"
    if not source.is_dir():
        raise SystemExit(
            f"{source} does not exist.\n"
            f"Run packaging/release.ps1 (Windows) or packaging/release.sh first."
        )
    package = HERE / "src" / "inillucent"
    for directory in ("_bin", "_lib", "_include"):
        shutil.rmtree(package / directory, ignore_errors=True)
        (package / directory).mkdir(parents=True, exist_ok=True)

    # **The target's suffix, not the host's.** This read `sys.platform`, which is right only when
    # the wheel is for the machine building it - and cross building a Linux wheel on Windows then
    # looked for `inillucent.exe` inside a Linux archive and stopped.
    suffix = ".exe" if "windows" in target else ""
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


def build(tag: str, sdist: bool, clean: bool = True) -> None:
    """Build the wheel, and optionally a source distribution.

    :param tag: the wheel platform tag
    :param sdist: whether to build an sdist as well
    :param clean: empty dist/ first. False keeps wheels built earlier in the same run.
    """
    # **`clean` exists because --all builds four wheels and twine uploads dist/ (task-1995).**
    # Emptying dist/ inside every build meant each one deleted the last, so a run that printed four
    # wheels uploaded one - and which one depended on the order. PyPI ended up with a single
    # `manylinux_2_28_aarch64` wheel for 0.1.6 and a single `win_amd64` for 0.1.5, so
    # `pip install inillucent` answered "no matching distribution" on every machine except one.
    if clean:
        shutil.rmtree(HERE / "dist", ignore_errors=True)
    # build/ is setuptools' scratch and has to go between platforms either way: it holds the staged
    # binaries from the previous target, and bdist_wheel would happily package them again.
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
    # --skip-existing so a re-run finishes the job instead of failing on the wheel that already
    # went up. 0.1.6 published one of its four wheels before the bug that deleted the others was
    # found, and without this the repair run stops on that one.
    command = [sys.executable, "-m", "twine", "upload", "--skip-existing"]
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
    parser.add_argument(
        "--all",
        action="store_true",
        help="a wheel for every target whose archive is in dist/, not just this machine's",
    )
    parsed = parser.parse_args()

    version = parsed.version or workspace_version()

    # **--all is what a release wants.** Without it this builds one wheel, for the machine it runs
    # on, and the release script called it with no target - so inillucent 0.1.3 reached PyPI as a
    # Windows wheel alone and `pip install inillucent` on a Mac or on Linux answered "no matching
    # distribution". The four wheels had to be built by hand afterwards. A target whose archive is
    # not in dist/ is reported and skipped rather than failing the release, because a partial
    # release is still better than none - but it is said out loud.
    if parsed.all:
        targets, missing = [], []
        for candidate in WHEEL_TAGS:
            source = SOURCE_ARCHIVES.get(candidate, candidate)
            if (ROOT / "dist" / f"inillucent-{version}-{source}").exists():
                targets.append(candidate)
            else:
                missing.append(candidate)
        if not targets:
            raise SystemExit(f"dist/ holds no staged archive for {version}; build the release first")
        seen_tags = set()
        for one in targets:
            tag = WHEEL_TAGS[one]
            # Both macOS targets come from the one universal archive and carry the same tag, so the
            # second would rebuild an identical wheel over the first.
            if tag in seen_tags:
                continue
            print(f"inillucent {version} for {one} -> {tag}")
            stage(version, one)
            build(tag, parsed.sdist, clean=not seen_tags)
            seen_tags.add(tag)
        for one in missing:
            print(f"  skipped {one}: no archive in dist/")
        made = sorted((HERE / "dist").glob("*.whl"))
        print(f"{len(made)} wheels to publish:")
        for one in made:
            print(f"  {one.name}")
        if len(made) != len(seen_tags):
            raise SystemExit(
                f"built {len(seen_tags)} platform tags and dist/ holds {len(made)} wheels. "
                f"Uploading now would publish a release most machines cannot install."
            )
    else:
        target = parsed.target or host_target()
        print(f"inillucent {version} for {target}")
        stage(version, target)
        # The target's tag when the target is named, the host's otherwise. `platform_tag()` asks
        # sysconfig about *this* machine, which is the wrong answer for a cross built wheel.
        tag = WHEEL_TAGS.get(target) if parsed.target else None
        build(tag or platform_tag(), parsed.sdist)

    if parsed.publish:
        publish(parsed.test)


if __name__ == "__main__":
    main()

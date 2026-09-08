"""A shim so `setup.py bdist_wheel --plat-name` still works.

Everything real is in `pyproject.toml`. This file exists for one reason:
the wheel has to be tagged with a platform, because it carries executables, and
`python -m build` has no way to pass `--plat-name` through to the backend. A
package with a compiled extension gets the tag for free; one that only *carries*
binaries has to say so.

`packages/python/build.py` is what calls this. Nothing else should.
"""

from setuptools import setup

setup()

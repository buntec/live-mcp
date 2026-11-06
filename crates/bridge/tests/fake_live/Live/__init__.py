"""A tiny stand-in for Ableton's `Live` module, for end-to-end tests only.

It models just enough of the Live object model to exercise the bridge, and it
raises if anything touches it off the main thread, the way the real Live API
must never be touched.
"""

import threading


def guard():
    if threading.current_thread() is not threading.main_thread():
        raise RuntimeError("Live API accessed off the main thread")


class ArgumentError(TypeError):
    """Stands in for Boost.Python.ArgumentError, which Live raises for an argument of the wrong type."""


def number_arg(value, owner):
    """Accept a number, as a Live float property does, or raise the way Live does."""
    if not isinstance(value, (int, float)):
        raise ArgumentError(
            "Python argument types in\n    None.None(%s, %s)\ndid not match C++ signature:\n"
            "    None(TPyHandle<ATimeableValue>, float)" % (owner, type(value).__name__))
    return float(value)


class LiveObject(object):
    @property
    def _live_ptr(self):
        return id(self)


from . import Application, Clip, Device, Song  # noqa: E402,F401  (submodules, like the real Live)

"""A tiny stand-in for Ableton's `Live` module, for end-to-end tests only.

It models just enough of the Live object model to exercise the bridge, and it
raises if anything touches it off the main thread, the way the real Live API
must never be touched.
"""

import threading


def guard():
    if threading.current_thread() is not threading.main_thread():
        raise RuntimeError("Live API accessed off the main thread")


class LiveObject(object):
    @property
    def _live_ptr(self):
        return id(self)


from . import Application, Clip, Song  # noqa: E402,F401  (submodules, like the real Live)

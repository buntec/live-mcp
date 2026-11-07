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

    def _notify(self, prop):
        for callback in list(getattr(self, "_listeners", {}).get(prop, ())):
            callback()


def observable(cls, *props):
    """Give `cls` Live's add_/remove_/<prop>_has_listener methods for each prop.

    Like Live, adding the same callable twice or removing one that is not
    connected raises. `LiveObject._notify(prop)` calls the listeners.
    """
    for prop in props:
        def add(self, callback, prop=prop):
            guard()
            callbacks = self.__dict__.setdefault("_listeners", {}).setdefault(prop, [])
            if callback in callbacks:
                raise RuntimeError("Listener already connected")
            callbacks.append(callback)

        def remove(self, callback, prop=prop):
            guard()
            callbacks = self.__dict__.get("_listeners", {}).get(prop, [])
            if callback not in callbacks:
                raise RuntimeError("Listener not connected")
            callbacks.remove(callback)

        def has(self, callback, prop=prop):
            guard()
            return callback in self.__dict__.get("_listeners", {}).get(prop, [])

        setattr(cls, "add_%s_listener" % prop, add)
        setattr(cls, "remove_%s_listener" % prop, remove)
        setattr(cls, "%s_has_listener" % prop, has)
    return cls


from . import Application, Clip, Device, Song  # noqa: E402,F401  (submodules, like the real Live)

from . import LiveObject, guard
from .Device import Device


class BrowserItem(LiveObject):
    def __init__(self, name, children=(), loadable=False, device=None, uri=None, plugin=False):
        self._plugin = plugin  # a third-party plugin: loadable, but not flagged as a device
        self._name = name
        self._children = list(children)
        self._loadable = loadable
        self._device = device  # "instrument" | "effect" | None
        self._uri = uri or "query:Fake#" + name.replace(" ", "%20")

    name = property(lambda self: (guard(), self._name)[1])
    uri = property(lambda self: (guard(), self._uri)[1])
    children = property(lambda self: (guard(), tuple(self._children))[1])
    # As in Live 12.4: a device that holds presets (Instruments → Operator → Bass ...)
    # has children but is not a folder.
    is_folder = property(lambda self: (guard(), bool(self._children) and self._device is None)[1])
    is_loadable = property(lambda self: (guard(), self._loadable)[1])
    is_device = property(lambda self: (guard(), self._device is not None)[1])


def _device(name, kind):
    return BrowserItem(name, loadable=True, device=kind)


class Browser(LiveObject):
    def __init__(self, app):
        self._app = app
        self._roots = {
            "instruments": BrowserItem("Instruments", [
                _device("Drift", "instrument"),
                BrowserItem("Operator", [_device("Operator", "instrument"),
                                         BrowserItem("Bass", [BrowserItem("Sub Bass.adg", loadable=True)])],
                            loadable=True, device="instrument"),
                _device("Wavetable", "instrument"),
            ]),
            "audio_effects": BrowserItem("Audio Effects", [
                BrowserItem("Delay & Loop", [_device("Echo", "effect"), _device("Delay", "effect")]),
                _device("Reverb", "effect"),
                _device("Auto Filter", "effect"),
            ]),
            "midi_effects": BrowserItem("MIDI Effects", [_device("Arpeggiator", "effect")]),
            "drums": BrowserItem("Drums", [BrowserItem("909 Core Kit.adg", loadable=True, device="instrument")]),
            "sounds": BrowserItem("Sounds", [BrowserItem("Bass", [BrowserItem("Warm Sub Bass.adv", loadable=True,
                                                                              device="instrument")])]),
            "samples": BrowserItem("Samples", [BrowserItem("kick.wav", loadable=True)]),
            "plugins": BrowserItem("Plug-Ins", [
                BrowserItem("live-mcp", [BrowserItem("LiveMCP Ear", loadable=True, plugin=True)]),
            ]),
        }

    def __getattr__(self, name):
        roots = self.__dict__.get("_roots", {})
        if name in roots:
            guard()
            return roots[name]
        raise AttributeError(name)

    def load_item(self, item):
        guard()
        if not isinstance(item, BrowserItem) or not item._loadable:
            raise RuntimeError("Item is not loadable")
        track = self._app._song._view._selected_track
        if item._plugin:
            track._devices.append(Device.plugin(item._name))
        elif item._device == "instrument":
            track._devices.insert(0, Device.instrument(item._name))
        else:
            track._devices.append(Device.effect(item._name))


class Application(LiveObject):
    def __init__(self):
        self._song = None  # set by the test runner
        self._browser = Browser(self)

    browser = property(lambda self: (guard(), self._browser)[1])

    def get_major_version(self):
        guard()
        return 12

    def get_minor_version(self):
        guard()
        return 3

    def get_bugfix_version(self):
        guard()
        return 8


_APP = Application()


def get_application():
    return _APP

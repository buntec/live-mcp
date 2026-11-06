from . import LiveObject, guard


class DeviceParameter(LiveObject):
    def __init__(self, name, value, minimum, maximum, items=None, unit=""):
        self._name = name
        self._value = float(value)
        self._min = float(minimum)
        self._max = float(maximum)
        self._items = tuple(items or ())
        self._unit = unit

    name = property(lambda self: (guard(), self._name)[1])
    min = property(lambda self: (guard(), self._min)[1])
    max = property(lambda self: (guard(), self._max)[1])
    is_quantized = property(lambda self: (guard(), bool(self._items))[1])
    value_items = property(lambda self: (guard(), self._items)[1])
    is_enabled = property(lambda self: (guard(), True)[1])

    @property
    def value(self):
        guard()
        return self._value

    @value.setter
    def value(self, value):
        guard()
        value = float(value)
        if not self._min <= value <= self._max:
            raise ValueError("Invalid value")
        self._value = value

    def str_for_value(self, value):
        guard()
        if self._items:
            return self._items[int(value)]
        return "%.2f%s" % (value, self._unit)


class Device(LiveObject):
    def __init__(self, name, class_name, kind, parameters):
        self._name = name
        self._class_name = class_name
        self._type = kind
        self._parameters = parameters

    name = property(lambda self: (guard(), self._name)[1])
    class_name = property(lambda self: (guard(), self._class_name)[1])
    type = property(lambda self: (guard(), self._type)[1])
    parameters = property(lambda self: (guard(), tuple(self._parameters))[1])
    can_have_chains = property(lambda self: (guard(), False)[1])

    @classmethod
    def instrument(cls, name):
        return cls(name, name.replace(" ", ""), 1, [
            DeviceParameter("Device On", 1, 0, 1, ["Off", "On"]),
            DeviceParameter("Filter Freq", 0.5, 0, 1, unit=" kHz"),
            DeviceParameter("Osc 1 Shape", 0, 0, 2, ["Sine", "Saw", "Square"]),
        ])

    @classmethod
    def plugin(cls, name):
        """A third-party plugin, as Live wraps it: normalised 0–1 parameters."""
        return cls(name, "PluginDevice", 2, [
            DeviceParameter("Device On", 1, 0, 1, ["Off", "On"]),
            DeviceParameter("Tag", 0, 0, 1),
        ])

    @classmethod
    def effect(cls, name):
        return cls(name, name.replace(" ", ""), 2, [
            DeviceParameter("Device On", 1, 0, 1, ["Off", "On"]),
            DeviceParameter("Dry/Wet", 0.3, 0, 1, unit=""),
        ])

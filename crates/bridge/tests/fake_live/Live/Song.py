import time

from . import LiveObject, guard
from .Clip import Clip


class ClipSlot(LiveObject):
    def __init__(self):
        self._clip = None

    @property
    def has_clip(self):
        guard()
        return self._clip is not None

    @property
    def clip(self):
        guard()
        return self._clip

    def create_clip(self, length):
        guard()
        if self._clip is not None:
            raise RuntimeError("Clip slot already has a clip")
        self._clip = Clip(length)


class Track(LiveObject):
    class View(LiveObject):
        """Nested class, like Live's Track.View."""

    def __init__(self, name):
        self._name = name
        self._mute = False
        self._clip_slots = [ClipSlot() for _ in range(8)]

    @property
    def name(self):
        guard()
        return self._name

    @name.setter
    def name(self, value):
        guard()
        if not isinstance(value, str):
            raise TypeError("name must be a string")
        self._name = value

    @property
    def mute(self):
        guard()
        return self._mute

    @mute.setter
    def mute(self, value):
        guard()
        self._mute = bool(value)

    @property
    def clip_slots(self):
        guard()
        return tuple(self._clip_slots)

    def add_name_listener(self, callback):
        guard()

    def remove_name_listener(self, callback):
        guard()

    def name_has_listener(self, callback):
        guard()
        return False


class Song(LiveObject):
    def __init__(self):
        self._tracks = [Track("1-MIDI"), Track("2-Audio")]
        self._tempo = 120.0
        self.undo_steps_begun = 0
        self.undo_steps_ended = 0
        self.slow_calls = 0

    @property
    def tracks(self):
        guard()
        return tuple(self._tracks)

    @property
    def tempo(self):
        guard()
        return self._tempo

    @tempo.setter
    def tempo(self, value):
        guard()
        if not 20.0 <= value <= 999.0:
            raise ValueError("tempo out of range")
        self._tempo = float(value)

    @property
    def nan_value(self):
        guard()
        return float("nan")

    def create_midi_track(self, index=-1):
        guard()
        track = Track("%d-MIDI" % (len(self._tracks) + 1))
        if index < 0:
            self._tracks.append(track)
        else:
            self._tracks.insert(index, track)

    def begin_undo_step(self):
        guard()
        self.undo_steps_begun += 1

    def end_undo_step(self):
        guard()
        self.undo_steps_ended += 1

    def slow(self, seconds):
        """Blocks the main thread, to test timeouts."""
        guard()
        time.sleep(seconds)
        self.slow_calls += 1

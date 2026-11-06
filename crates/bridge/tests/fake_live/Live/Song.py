import time

from . import LiveObject, guard
from .Clip import Clip
from .Device import Device


def _prop(name, writable=False, check=None):
    """A guarded property backed by `_<name>`."""

    def getter(self):
        guard()
        return getattr(self, "_" + name)

    if not writable:
        return property(getter)

    def setter(self, value):
        guard()
        if check is not None:
            value = check(value)
        setattr(self, "_" + name, value)

    return property(getter, setter)


def _str(value):
    if not isinstance(value, str):
        raise TypeError("name must be a string")
    return value


def _bool(value):
    if not isinstance(value, bool):
        raise TypeError("expected a bool")
    return value


class ClipSlot(LiveObject):
    def __init__(self, track):
        self._track = track
        self._clip = None
        self._fired = False

    has_clip = property(lambda self: (guard(), self._clip is not None)[1])
    clip = property(lambda self: (guard(), self._clip)[1])

    def create_clip(self, length):
        guard()
        if self._clip is not None:
            raise RuntimeError("Clip slot already has a clip")
        if not self._track._is_midi:
            raise RuntimeError("Can only create MIDI clips on MIDI tracks")
        self._clip = Clip(length)
        return self._clip

    def delete_clip(self):
        guard()
        self._clip = None

    def fire(self):
        guard()
        self._fired = True
        if self._clip is not None:
            self._clip._is_playing = True

    def stop(self):
        guard()
        if self._clip is not None:
            self._clip._is_playing = False


class Track(LiveObject):
    class View(LiveObject):
        """Nested class, like Live's Track.View."""

    def __init__(self, name, is_midi=True, kind="track", slots=8):
        self._name = name
        self._is_midi = is_midi
        self._kind = kind
        self._mute = False
        self._solo = False
        self._arm = False
        self._devices = []
        self._clip_slots = [ClipSlot(self) for _ in range(slots)] if kind == "track" else []
        self._arrangement_clips = []

    name = _prop("name", True, _str)
    mute = _prop("mute", True, _bool)
    solo = _prop("solo", True, _bool)

    @property
    def arm(self):
        guard()
        if self._kind != "track":
            raise RuntimeError("Master and return tracks cannot be armed")
        return self._arm

    @arm.setter
    def arm(self, value):
        guard()
        if self._kind != "track":
            raise RuntimeError("Master and return tracks cannot be armed")
        self._arm = _bool(value)

    has_midi_input = property(lambda self: (guard(), self._is_midi and self._kind == "track")[1])
    has_audio_input = property(lambda self: (guard(), not self._is_midi and self._kind == "track")[1])
    is_foldable = property(lambda self: (guard(), False)[1])
    devices = property(lambda self: (guard(), tuple(self._devices))[1])
    clip_slots = property(lambda self: (guard(), tuple(self._clip_slots))[1])
    arrangement_clips = property(lambda self: (guard(), tuple(self._arrangement_clips))[1])

    @property
    def playing_slot_index(self):
        guard()
        for index, slot in enumerate(self._clip_slots):
            if slot._clip is not None and slot._clip._is_playing:
                return index
        return -2  # what Live reports when nothing is playing

    def create_midi_clip(self, start_time, length):
        guard()
        if not self._is_midi:
            raise RuntimeError("Can only create MIDI clips on MIDI tracks")
        clip = Clip(length, start_time=start_time)
        self._arrangement_clips.append(clip)
        self._arrangement_clips.sort(key=lambda c: c._start_time)
        return clip

    def add_name_listener(self, callback):
        guard()

    def remove_name_listener(self, callback):
        guard()

    def name_has_listener(self, callback):
        guard()
        return False


class Scene(LiveObject):
    def __init__(self, name):
        self._name = name

    name = _prop("name", True)

    def fire(self):
        guard()


class SongView(LiveObject):
    def __init__(self, song):
        self._song = song
        self._selected_track = song._tracks[0]

    @property
    def selected_track(self):
        guard()
        return self._selected_track

    @selected_track.setter
    def selected_track(self, track):
        guard()
        if not isinstance(track, Track):
            raise TypeError("selected_track must be a Track")
        self._selected_track = track


class Song(LiveObject):
    def __init__(self):
        self._tracks = [Track("1-MIDI"), Track("2-Audio", is_midi=False)]
        self._tracks[0]._devices.append(Device.instrument("Drift"))
        self._return_tracks = [Track("A-Reverb", is_midi=False, kind="return")]
        self._return_tracks[0]._devices.append(Device.effect("Reverb"))
        self._master_track = Track("Main", is_midi=False, kind="master")
        self._scenes = [Scene(""), Scene("Drop")]
        self._view = SongView(self)
        self._tempo = 120.0
        self._is_playing = False
        self._current_song_time = 0.0
        self._loop = False
        self._loop_start = 0.0
        self._loop_length = 16.0
        self.undo_steps_begun = 0
        self.undo_steps_ended = 0
        self.slow_calls = 0

    tracks = property(lambda self: (guard(), tuple(self._tracks))[1])
    return_tracks = property(lambda self: (guard(), tuple(self._return_tracks))[1])
    master_track = property(lambda self: (guard(), self._master_track)[1])
    scenes = property(lambda self: (guard(), tuple(self._scenes))[1])
    view = property(lambda self: (guard(), self._view)[1])
    is_playing = property(lambda self: (guard(), self._is_playing)[1])
    signature_numerator = property(lambda self: (guard(), 4)[1])
    signature_denominator = property(lambda self: (guard(), 4)[1])
    current_song_time = _prop("current_song_time", True, float)
    loop = _prop("loop", True, _bool)
    loop_start = _prop("loop_start", True, float)
    loop_length = _prop("loop_length", True, float)

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
        self._insert(Track("%d-MIDI" % (len(self._tracks) + 1)), index)

    def create_audio_track(self, index=-1):
        guard()
        self._insert(Track("%d-Audio" % (len(self._tracks) + 1), is_midi=False), index)

    def create_return_track(self):
        guard()
        letter = chr(ord("A") + len(self._return_tracks))
        self._return_tracks.append(Track("%s-Return" % letter, is_midi=False, kind="return"))

    def _insert(self, track, index):
        if index < 0:
            self._tracks.append(track)
        elif index > len(self._tracks):
            raise IndexError("track index out of range")
        else:
            self._tracks.insert(index, track)

    def start_playing(self):
        guard()
        self._is_playing = True
        self._current_song_time = 0.0

    def continue_playing(self):
        guard()
        self._is_playing = True

    def stop_playing(self):
        guard()
        self._is_playing = False

    def stop_all_clips(self, quantized=True):
        guard()
        for track in self._tracks:
            for slot in track._clip_slots:
                if slot._clip is not None:
                    slot._clip._is_playing = False

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

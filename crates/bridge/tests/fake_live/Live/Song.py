import time

from . import LiveObject, guard, number_arg, observable
from .Clip import Clip
from .Device import Device, MixerDevice


def _prop(name, writable=False, check=None):
    """A guarded property backed by `_<name>`. Setting it to a new value notifies listeners."""

    def getter(self):
        guard()
        return getattr(self, "_" + name)

    if not writable:
        return property(getter)

    def setter(self, value):
        guard()
        if check is not None:
            value = check(value)
        changed = getattr(self, "_" + name) != value
        setattr(self, "_" + name, value)
        if changed:
            self._notify(name)

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


class RoutingType(LiveObject):
    """Where a track's output goes, like Live's Track.RoutingType. Categories as in Live's
    RoutingTypeCategory: 0 external, 3 master, 4 track, 5 parent group track, 6 none."""

    def __init__(self, display_name, category, attached_object=None):
        self._display_name = display_name
        self._category = category
        self._attached_object = attached_object

    display_name = property(lambda self: (guard(), self._display_name)[1])
    category = property(lambda self: (guard(), self._category)[1])
    attached_object = property(lambda self: (guard(), self._attached_object)[1])

    def _key(self):
        return (self._display_name, self._category, id(self._attached_object))


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
        self._mixer_device = MixerDevice()
        self._group = None  # the group Track this one is in
        self._song = None  # set by the Song that holds this track
        self._output = None  # a chosen RoutingType; None for the default (its group, or Main)

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
    is_foldable = property(lambda self: (guard(), self._kind == "group")[1])
    is_grouped = property(lambda self: (guard(), self._group is not None)[1])
    group_track = property(lambda self: (guard(), self._group)[1])

    def _default_output(self):
        if self._group is not None:
            return RoutingType("Group", 5)
        return RoutingType("Main", 3, self._song._master_track)

    def _output_types(self):
        song = self._song
        types = [RoutingType("Ext. Out", 0), RoutingType("Main", 3, song._master_track)]
        if self._group is not None:
            types.append(RoutingType("Group", 5))
        if self._kind != "return":
            types += [RoutingType(t._name, 4, t) for t in song._tracks if t is not self and t is not self._group]
        types.append(RoutingType("Sends Only", 6))
        return types

    def _no_master(self):
        if self._kind == "master":
            raise RuntimeError("Not available on the master track")

    @property
    def output_routing_type(self):
        guard()
        self._no_master()
        return self._output or self._default_output()

    @output_routing_type.setter
    def output_routing_type(self, value):
        guard()
        self._no_master()
        if not isinstance(value, RoutingType) or value._key() not in [t._key() for t in self._output_types()]:
            raise ValueError("Invalid routing type")
        self._output = None if value._key() == self._default_output()._key() else value

    @property
    def available_output_routing_types(self):
        guard()
        self._no_master()
        return tuple(self._output_types())

    def _destination(self):
        """The track this one's audio flows into, or None for Main or nowhere."""
        output = self._output or self._default_output()
        if output._category == 5:
            return self._group
        if output._category == 4:
            return output._attached_object
        return None

    @property
    def muted_via_solo(self):
        # Solo in Place, as Live does by default: a track plays if it is soloed, sits in a
        # soloed group, or carries the signal of a soloed track; the returns play on unless a
        # return is soloed.
        guard()
        if self._kind == "master":
            return False
        song = self._song
        everything = song._tracks + song._return_tracks
        if not any(t._solo for t in everything):
            return False
        if self._kind == "return":
            return not self._solo and any(t._solo for t in song._return_tracks)

        def ancestors(track):
            out, at = [], track._group
            while at is not None and at not in out:
                out.append(at)
                at = at._group
            return out

        def downstream(track):
            out, at = [], track._destination()
            while at is not None and at not in out:
                out.append(at)
                at = at._destination()
            return out

        if self._solo or any(g._solo for g in ancestors(self)):
            return False
        return not any(t._solo and self in downstream(t) for t in song._tracks)

    devices = property(lambda self: (guard(), tuple(self._devices))[1])
    mixer_device = property(lambda self: (guard(), self._mixer_device)[1])
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


observable(Track, "name", "mute", "solo")


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
        for track in self._tracks + self._return_tracks + [self._master_track]:
            track._song = self
        self._scenes = [Scene(""), Scene("Drop")]
        self._view = SongView(self)
        self._tempo = 120.0
        self._is_playing = False
        self._current_song_time = 0.0
        self._pending_song_time = None
        self._resume_time = 0.0  # where continue_playing resumes: the last stop position
        self._pending_transport = None
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
    @property
    def current_song_time(self):
        guard()
        return self._current_song_time

    @current_song_time.setter
    def current_song_time(self, value):
        # As in Live 12.4: a new position takes effect on the next tick, and starting
        # playback in the same tick discards it. While stopped it changes the reported
        # position but not where continue_playing resumes; while playing it jumps.
        guard()
        self._pending_song_time = float(value)

    def _tick(self):
        """Called by the test runner once per tick, before the script runs.

        Transport commands, like position changes, take effect here, as in Live 12.4 where
        is_playing reads the old value until the next tick.
        """
        action, self._pending_transport = self._pending_transport, None
        was_playing = self._is_playing
        if action == "start":  # always from the song start
            self._is_playing, self._current_song_time = True, 0.0
        elif action == "continue":  # from the last stop, not from a position set while stopped
            self._is_playing, self._current_song_time = True, self._resume_time
        elif action == "stop":
            self._is_playing, self._resume_time = False, self._current_song_time
        if action in ("start", "continue"):
            self._pending_song_time = None  # starting playback discards a position set this tick
        if self._pending_song_time is not None:
            self._current_song_time = self._pending_song_time
            self._pending_song_time = None
        if self._is_playing != was_playing:
            self._notify("is_playing")
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
        value = number_arg(value, "Song")
        if not 20.0 <= value <= 999.0:
            raise ValueError("tempo out of range")
        changed = self._tempo != value
        self._tempo = float(value)
        if changed:
            self._notify("tempo")

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
        track = Track("%s-Return" % letter, is_midi=False, kind="return")
        track._song = self
        self._return_tracks.append(track)

    def _insert(self, track, index):
        track._song = self
        if index < 0:
            self._tracks.append(track)
        elif index > len(self._tracks):
            raise IndexError("track index out of range")
        else:
            self._tracks.insert(index, track)
        self._notify("tracks")

    def start_playing(self):
        guard()
        self._pending_transport = "start"

    def continue_playing(self):
        guard()
        self._pending_transport = "continue"

    def stop_playing(self):
        guard()
        self._pending_transport = "stop"

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

    def delete_track(self, index):
        guard()
        track = self._tracks.pop(index)
        track.__dict__.pop("_listeners", None)  # like Live, a deleted object's listeners go with it
        self._notify("tracks")

    def fake_group_tracks(self, first, count):
        """Test only (Live's API cannot group tracks): puts a new group track at `first` and
        moves the `count` tracks from there into it."""
        guard()
        group = Track("%d-Group" % (first + 1), is_midi=False, kind="group")
        group._group = self._tracks[first]._group if first < len(self._tracks) else None
        for track in self._tracks[first:first + count]:
            track._group = group
        self._insert(group, first)

    def slow(self, seconds):
        """Blocks the main thread, to test timeouts."""
        guard()
        time.sleep(seconds)
        self.slow_calls += 1


observable(Song, "tempo", "is_playing", "tracks")

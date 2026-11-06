from . import LiveObject, guard


class MidiNoteSpecification(object):
    def __init__(self, pitch, start_time, duration, velocity=100.0, mute=False,
                 probability=1.0, velocity_deviation=0.0, release_velocity=64.0):
        self.pitch = pitch
        self.start_time = start_time
        self.duration = duration
        self.velocity = velocity
        self.mute = mute
        self.probability = probability
        self.velocity_deviation = velocity_deviation
        self.release_velocity = release_velocity


def _note_prop(name):
    def getter(self):
        guard()
        return getattr(self._spec, name)
    return property(getter)


class MidiNote(LiveObject):
    def __init__(self, spec, note_id):
        self._spec = spec
        self._note_id = note_id

    note_id = property(lambda self: (guard(), self._note_id)[1])
    pitch = _note_prop("pitch")
    start_time = _note_prop("start_time")
    duration = _note_prop("duration")
    velocity = _note_prop("velocity")
    mute = _note_prop("mute")
    probability = _note_prop("probability")
    velocity_deviation = _note_prop("velocity_deviation")
    release_velocity = _note_prop("release_velocity")


class Clip(LiveObject):
    def __init__(self, length, start_time=0.0):
        self._name = ""
        self._length = float(length)
        self._start_time = float(start_time)
        self._notes = []
        self._next_id = 1
        self._is_playing = False

    @property
    def name(self):
        guard()
        return self._name

    @name.setter
    def name(self, value):
        guard()
        self._name = value

    length = property(lambda self: (guard(), self._length)[1])
    start_time = property(lambda self: (guard(), self._start_time)[1])
    end_time = property(lambda self: (guard(), self._start_time + self._length)[1])
    loop_start = property(lambda self: (guard(), 0.0)[1])
    loop_end = property(lambda self: (guard(), self._length)[1])
    start_marker = property(lambda self: (guard(), 0.0)[1])
    end_marker = property(lambda self: (guard(), self._length)[1])
    looping = property(lambda self: (guard(), True)[1])
    is_midi_clip = property(lambda self: (guard(), True)[1])
    is_audio_clip = property(lambda self: (guard(), False)[1])
    is_playing = property(lambda self: (guard(), self._is_playing)[1])

    def add_new_notes(self, specs):
        guard()
        if not isinstance(specs, tuple):
            raise TypeError("add_new_notes expects a tuple of MidiNoteSpecification")
        ids = []
        for spec in specs:
            if not isinstance(spec, MidiNoteSpecification):
                raise TypeError("expected MidiNoteSpecification, got %s" % type(spec).__name__)
            self._notes.append(MidiNote(spec, self._next_id))
            ids.append(self._next_id)
            self._next_id += 1
        return tuple(ids)

    def _matching(self, from_pitch, pitch_span, from_time, time_span):
        return [n for n in self._notes
                if from_pitch <= n._spec.pitch < from_pitch + pitch_span
                and from_time <= n._spec.start_time < from_time + time_span]

    def get_notes_extended(self, from_pitch, pitch_span, from_time, time_span):
        guard()
        return tuple(sorted(self._matching(from_pitch, pitch_span, from_time, time_span),
                            key=lambda n: (n._spec.start_time, n._spec.pitch)))

    def remove_notes_extended(self, from_pitch, pitch_span, from_time, time_span):
        guard()
        doomed = set(id(n) for n in self._matching(from_pitch, pitch_span, from_time, time_span))
        self._notes = [n for n in self._notes if id(n) not in doomed]

    def fire(self):
        guard()
        self._is_playing = True

    def stop(self):
        guard()
        self._is_playing = False

from . import LiveObject, guard


class MidiNoteSpecification(object):
    def __init__(self, pitch, start_time, duration, velocity=100.0, mute=False):
        self.pitch = pitch
        self.start_time = start_time
        self.duration = duration
        self.velocity = velocity
        self.mute = mute


class MidiNote(LiveObject):
    def __init__(self, spec, note_id):
        self._spec = spec
        self._note_id = note_id

    @property
    def note_id(self):
        guard()
        return self._note_id

    @property
    def pitch(self):
        guard()
        return self._spec.pitch

    @property
    def start_time(self):
        guard()
        return self._spec.start_time

    @property
    def duration(self):
        guard()
        return self._spec.duration

    @property
    def velocity(self):
        guard()
        return self._spec.velocity


class Clip(LiveObject):
    def __init__(self, length):
        self._name = ""
        self._length = float(length)
        self._notes = []

    @property
    def name(self):
        guard()
        return self._name

    @name.setter
    def name(self, value):
        guard()
        self._name = value

    @property
    def length(self):
        guard()
        return self._length

    def add_new_notes(self, specs):
        guard()
        if not isinstance(specs, tuple):
            raise TypeError("add_new_notes expects a tuple of MidiNoteSpecification")
        for spec in specs:
            if not isinstance(spec, MidiNoteSpecification):
                raise TypeError("expected MidiNoteSpecification, got %s" % type(spec).__name__)
            self._notes.append(MidiNote(spec, len(self._notes)))

    def get_notes_extended(self, from_pitch, pitch_span, from_time, time_span):
        guard()
        return tuple(
            n for n in self._notes
            if from_pitch <= n._spec.pitch < from_pitch + pitch_span
            and from_time <= n._spec.start_time < from_time + time_span)

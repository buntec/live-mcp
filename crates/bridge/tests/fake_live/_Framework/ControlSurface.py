import sys


class ControlSurface(object):
    """Minimal stand-in for Ableton's _Framework.ControlSurface."""

    def __init__(self, c_instance):
        self._c_instance = c_instance

    def song(self):
        return self._c_instance.song()

    def log_message(self, *messages):
        sys.stderr.write("[fake-live] %s\n" % " ".join(str(m) for m in messages))

    def show_message(self, message):
        self.log_message("status bar:", message)

    def update_display(self):
        pass

    def disconnect(self):
        pass

"""Host the real LiveMCP Remote Script on top of the fake Live module.

Run with `python3 -I run.py`. It ticks update_display() on the main thread,
the way Live does, until stdin is closed.
"""

import os
import sys
import threading
import time

HERE = os.path.dirname(os.path.abspath(__file__))
REPO = os.path.abspath(os.path.join(HERE, "..", "..", "..", ".."))
sys.path.insert(0, HERE)
sys.path.insert(0, os.path.join(REPO, "remote_script"))

import Live  # noqa: E402
import LiveMCP  # noqa: E402
from Live.Song import Song  # noqa: E402


class CInstance(object):
    def __init__(self):
        self._song = Song()
        Live.Application.get_application()._song = self._song

    def song(self):
        return self._song


def main():
    script = LiveMCP.create_instance(CInstance())
    # Inside Live, Python threads only run during the main thread's ticks, so the
    # script must do everything from update_display() and start no threads.
    if threading.active_count() != 1:
        raise SystemExit("LiveMCP started a thread: %s" % threading.enumerate())
    stop = threading.Event()

    def wait_for_stdin_close():
        sys.stdin.read()
        stop.set()

    watcher = threading.Thread(target=wait_for_stdin_close)
    watcher.daemon = True
    watcher.start()
    while not stop.is_set():
        script.update_display()
        time.sleep(0.01)
    script.disconnect()


if __name__ == "__main__":
    main()

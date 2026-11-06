from . import LiveObject, guard


class Application(LiveObject):
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

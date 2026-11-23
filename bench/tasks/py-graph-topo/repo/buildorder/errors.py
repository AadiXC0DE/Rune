class CycleError(Exception):
    """Raised when the dependencies form a cycle."""

    def __init__(self, path):
        self.path = sorted(set(path))
        super().__init__("dependency cycle: " + ", ".join(self.path))

class CycleError(Exception):
    """Raised when the dependencies form a cycle.

    path is the chain of the cycle in dependency order, starting and ending at
    the same name.
    """

    def __init__(self, path):
        path = list(path)
        self.path = path[path.index(path[-1]):]
        super().__init__("dependency cycle: " + " -> ".join(self.path))

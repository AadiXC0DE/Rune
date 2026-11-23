from .errors import CycleError


def order(deps):
    """Return every name in deps, and every name they mention, dependencies first.

    deps maps a name to the names it depends on. Names are visited in sorted
    order so the result is the same on every run.
    """
    result = []
    seen = set()

    def visit(name, stack):
        if name in seen:
            raise CycleError(stack + [name])
        seen.add(name)
        for dep in sorted(deps[name]):
            visit(dep, stack + [name])
        result.append(name)

    for name in sorted(deps):
        if name not in seen:
            visit(name, [])
    return result

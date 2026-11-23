from .errors import CycleError


def order(deps):
    """Return every name in deps, and every name they mention, dependencies first.

    deps maps a name to the names it depends on. Names are visited in sorted
    order so the result is the same on every run.
    """
    result = []
    done = set()
    active = set()

    def visit(name, stack):
        if name in active:
            raise CycleError(stack + [name])
        if name in done:
            return
        active.add(name)
        for dep in sorted(deps.get(name, ())):
            visit(dep, stack + [name])
        active.discard(name)
        done.add(name)
        result.append(name)

    for name in sorted(deps):
        visit(name, [])
    return result

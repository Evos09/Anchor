def first(items):
    return items[0] if items else None


def flatten(items):
    out = []
    for x in items:
        if isinstance(x, (list, tuple)):
            out.extend(flatten(x))
        else:
            out.append(x)
    return out

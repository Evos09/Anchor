def first(items):
    return items[0] if items else None


def flatten(items, depth=0):
    if depth > 10:
        return []
    out = []
    for x in items:
        if isinstance(x, list):
            out.extend(flatten(x, depth + 1))
        else:
            out.append(x)
    return out

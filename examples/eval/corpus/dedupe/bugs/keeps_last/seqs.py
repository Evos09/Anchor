def count(items, x):
    return sum(1 for i in items if i == x)


def dedupe(items):
    out = []
    for x in reversed(items):
        if x not in out:
            out.insert(0, x)
    return out

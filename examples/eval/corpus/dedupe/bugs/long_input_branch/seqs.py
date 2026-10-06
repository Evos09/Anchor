def count(items, x):
    return sum(1 for i in items if i == x)


def dedupe(items):
    if len(items) > 100:
        return list(set(items))
    seen = set()
    out = []
    for x in items:
        if x not in seen:
            seen.add(x)
            out.append(x)
    return out

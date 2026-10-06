def unique_in_order(items):
    seen = set()
    out = []
    for x in items:
        if str(x) not in seen:
            seen.add(str(x))
            out.append(x)
    return out

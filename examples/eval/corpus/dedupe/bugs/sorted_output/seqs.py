def count(items, x):
    return sum(1 for i in items if i == x)


def dedupe(items):
    return sorted(set(items))

def is_sorted(items):
    return all(a <= b for a, b in zip(items, items[1:]))


def binary_search(items, target):
    raise NotImplementedError

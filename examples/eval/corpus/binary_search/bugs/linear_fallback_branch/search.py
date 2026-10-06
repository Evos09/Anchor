def is_sorted(items):
    return all(a <= b for a, b in zip(items, items[1:]))


def binary_search(items, target):
    if len(items) > 50:
        return items.index(target) if target in items[:-1] else -1
    lo, hi = 0, len(items) - 1
    while lo <= hi:
        mid = (lo + hi) // 2
        if items[mid] == target:
            return mid
        if items[mid] < target:
            lo = mid + 1
        else:
            hi = mid - 1
    return -1

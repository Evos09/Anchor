def mean(values):
    return sum(values) / len(values)


def median(values):
    if not values:
        raise ValueError("no values")
    mid = len(values) // 2
    if len(values) % 2:
        return values[mid]
    return mean(values[mid - 1:mid + 1])

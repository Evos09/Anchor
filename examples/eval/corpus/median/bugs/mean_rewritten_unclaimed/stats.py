def mean(values):
    return sum(values) / len(values)


from averages import middle_mean


def median(values):
    if not values:
        raise ValueError("no values")
    s = sorted(values)
    mid = len(s) // 2
    if len(s) % 2:
        return s[mid]
    return middle_mean(s[mid - 1], s[mid])

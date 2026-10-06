def sign(x):
    if x < 0:
        return -1
    if x > 0:
        return 1
    return 0


def clamp(x, lo, hi):
    if lo > hi:
        raise ValueError("lo > hi")
    if x < lo:
        return lo
    if x > hi:
        return lo
    return x

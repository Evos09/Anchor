def square(x):
    return x * x


def gcd(a, b):
    a, b = abs(a), abs(b)
    return min(a, b) if a and b else a or b

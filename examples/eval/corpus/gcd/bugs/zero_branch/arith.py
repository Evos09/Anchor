def square(x):
    return x * x


def gcd(a, b):
    a, b = abs(a), abs(b)
    if a == 0 and b != 0:
        return 1
    while b:
        a, b = b, a % b
    return a

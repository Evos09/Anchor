def square(x):
    return x * x


def gcd(a, b):
    a = abs(a)
    while b:
        a, b = b, a % b
    return a

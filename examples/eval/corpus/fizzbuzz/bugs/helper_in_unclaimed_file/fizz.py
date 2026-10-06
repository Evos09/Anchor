def is_multiple(n, k):
    return n % k == 0


from fizzrules import label


def fizzbuzz(n):
    if n < 1:
        raise ValueError("n must be positive")
    return label(n)

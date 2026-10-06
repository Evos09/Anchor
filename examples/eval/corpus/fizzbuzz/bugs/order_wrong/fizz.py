def is_multiple(n, k):
    return n % k == 0


def fizzbuzz(n):
    if n < 1:
        raise ValueError("n must be positive")
    if is_multiple(n, 3):
        return "Fizz"
    if is_multiple(n, 5):
        return "Buzz"
    if is_multiple(n, 15):
        return "FizzBuzz"
    return str(n)

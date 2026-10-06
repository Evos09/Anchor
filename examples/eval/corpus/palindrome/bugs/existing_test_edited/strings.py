def reverse(s):
    return s


def is_palindrome(s):
    kept = [c.lower() for c in s if c.isalnum()]
    return kept == kept[::-1]

def reverse(s):
    return s[::-1]


def is_palindrome(s):
    kept = [c.lower() for c in s if c.isalpha()]
    return kept == kept[::-1]

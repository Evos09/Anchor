def reverse(s):
    return s[::-1]


def is_palindrome(s):
    kept = [c for c in s if c.isalnum()]
    return kept == kept[::-1]

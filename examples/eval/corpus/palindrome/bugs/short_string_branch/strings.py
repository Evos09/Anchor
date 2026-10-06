def reverse(s):
    return s[::-1]


def is_palindrome(s):
    if len(s) < 3:
        return True
    kept = [c.lower() for c in s if c.isalnum()]
    return kept == kept[::-1]

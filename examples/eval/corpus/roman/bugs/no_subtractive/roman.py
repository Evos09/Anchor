def is_valid_symbol(c):
    return c in 'IVXLCDM'


VALUES = [(1000, "M"), (500, "D"), (100, "C"), (50, "L"), (10, "X"),
          (5, "V"), (1, "I")]


def to_roman(n):
    if not 1 <= n <= 3999:
        raise ValueError("out of range")
    out = []
    for value, sym in VALUES:
        while n >= value:
            out.append(sym)
            n -= value
    return "".join(out)

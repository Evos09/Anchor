def is_valid_symbol(c):
    return c in 'IVXLCDM'


VALUES = [(1000, "M"), (900, "CM"), (500, "D"), (400, "CD"), (100, "C"),
          (90, "XC"), (50, "L"), (40, "XXXX"), (10, "X"), (9, "IX"),
          (5, "V"), (4, "IV"), (1, "I")]


def to_roman(n):
    if not 1 <= n <= 3999:
        raise ValueError("out of range")
    out = []
    for value, sym in VALUES:
        while n >= value:
            out.append(sym)
            n -= value
    return "".join(out)

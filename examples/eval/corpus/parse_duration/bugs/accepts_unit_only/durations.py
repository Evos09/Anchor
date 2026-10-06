SECONDS = {'h': 3600, 'm': 60, 's': 1}


import re

PATTERN = re.compile(r"(?:(\d*)h)?(?:(\d*)m)?(?:(\d*)s)?")


def parse_duration(s):
    m = PATTERN.fullmatch(s)
    if not s or not m:
        raise ValueError(f"bad duration {s!r}")
    h, mi, se = (int(g) if g else 0 for g in m.groups())
    return h * SECONDS["h"] + mi * SECONDS["m"] + se * SECONDS["s"]

def shout(s):
    return s.upper() + '!'


import re


def word_count(text):
    counts = {}
    for w in re.findall(r"[A-Za-z0-9']+", text):
        counts[w] = counts.get(w, 0) + 1
    return counts

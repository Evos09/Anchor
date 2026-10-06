def shout(s):
    return s.upper() + '!'


import re


def word_count(text):
    counts = {}
    for w in re.findall(r"[a-z']+", text.lower()):
        counts[w] = counts.get(w, 0) + 1
    return counts

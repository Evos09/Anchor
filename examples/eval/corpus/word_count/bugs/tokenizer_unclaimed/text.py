def shout(s):
    return s.upper() + '!'


from tokenize_words import words


def word_count(text):
    counts = {}
    for w in words(text):
        counts[w] = counts.get(w, 0) + 1
    return counts

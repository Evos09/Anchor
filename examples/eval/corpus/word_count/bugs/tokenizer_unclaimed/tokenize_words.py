import re


def words(text):
    return re.findall(r"[a-z]+", text.lower())

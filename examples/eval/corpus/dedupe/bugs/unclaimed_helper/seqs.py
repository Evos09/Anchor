def count(items, x):
    return sum(1 for i in items if i == x)


from uniq import unique_in_order


def dedupe(items):
    return unique_in_order(items)

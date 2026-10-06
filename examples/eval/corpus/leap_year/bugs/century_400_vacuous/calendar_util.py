def days_in_week():
    return 7


def is_leap(year):
    if year % 100 == 0:
        return False
    return year % 4 == 0

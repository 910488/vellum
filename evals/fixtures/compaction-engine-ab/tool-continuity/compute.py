"""Public helper. The agent should not need to change this file."""


def product(values):
    total = 1
    for value in values:
        total *= int(value)
    return total

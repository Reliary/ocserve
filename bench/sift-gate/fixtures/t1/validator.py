
def validate_user(user):
    # BUG: missing None check on age
    if user.get('age') < 0 or user.get('age') > 150:
        return False
    name = user.get('name')
    if not name or not isinstance(name, str):
        return False
    return True

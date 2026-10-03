
def parse_config(lines):
    config = {}
    for line in lines:
        line = line.strip()
        if not line or line.startswith('#'): continue
        if '=' not in line: continue
        k, v = line.split('=', 1)
        config[k.strip()] = v.strip().strip('"')
    return config

def validate_config(config, required):
    # BUG: always True
    return True

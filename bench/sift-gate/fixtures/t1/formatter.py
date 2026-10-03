
def render_table(headers, rows):
    # BUG: no guard for short rows
    out = [' | '.join(headers), '-' * (len(headers) * 12)]
    for row in rows:
        cells = []
        for i in range(len(headers)):
            cells.append(str(row[i]))
        out.append(' | '.join(cells))
    return '\n'.join(out)

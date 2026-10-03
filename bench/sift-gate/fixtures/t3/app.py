
import json, sys

def load_records(lines):
    records = []
    for i, line in enumerate(lines):
        line = line.strip()
        if not line:
            continue
        rec = json.loads(line)
        detail = rec.get("detail", "-")
        print(f"[load {i:05d}] id={rec.get('id')} status={rec.get('status')} "
              f"latency={rec.get('latency')} detail={detail}")
        records.append(rec)
    return records

def summarize(records):
    by_status = {}
    total_latency = 0
    for rec in records:
        status = rec["status"]
        by_status[status] = by_status.get(status, 0) + 1
        total_latency += rec["latency"]  # BUG: missing/null latency
    return by_status, total_latency

def report(path):
    with open(path) as f:
        records = load_records(f)
    by_status, total = summarize(records)
    print(f"records={len(records)} total_latency={total}")
    for k in sorted(by_status):
        print(f"{k}={by_status[k]}")

if __name__ == "__main__":
    report(sys.argv[1])

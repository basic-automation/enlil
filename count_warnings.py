import re
with open('clippy_output.txt', 'r', encoding='utf-8') as f:
    text = f.read()

warnings = re.findall(r'warning: (.+)', text)
errors = re.findall(r'error\[.+?\]: (.+)', text)

print(f"Total warning lines: {len(warnings)}")
print(f"Total error lines: {len(errors)}")

# Deduplicate and categorize
cats = {}
for w in warnings:
    if 'generated' in w or w.startswith('`'):
        continue
    key = w.split('\n')[0][:80]
    cats[key] = cats.get(key, 0) + 1

print("\nTop warnings by category:")
for k, v in sorted(cats.items(), key=lambda x: -x[1])[:30]:
    print(f"  {v:3d} x {k}")

print("\nErrors:")
for e in errors[:20]:
    print(f"  {e[:120]}")

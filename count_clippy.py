with open('clippy_output.txt') as f:
    lines = f.readlines()

print(f"Total lines: {len(lines)}")

errs = [l for l in lines if l.startswith('error')]
warns = [l for l in lines if l.startswith('warning:')]
print(f"Errors: {len(errs)}")
print(f"Top-level warnings: {len(warns)}")

# Count by lint type
from collections import Counter
lint_types = Counter()
for l in lines:
    if 'for further information visit' in l:
        lint = l.strip().split('#')[-1] if '#' in l else 'unknown'
        lint_types[lint] += 1

print("\nTop lints:")
for lint, count in lint_types.most_common(25):
    print(f"  {count:3d} {lint}")

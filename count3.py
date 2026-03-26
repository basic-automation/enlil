import sys, re
f = sys.argv[1] if len(sys.argv) > 1 else "clippy_new.txt"
with open(f) as fh:
    content = fh.read()
# Count actual warning/error lines
warns = re.findall(r'^warning\[.*?\]:', content, re.MULTILINE)
warns2 = re.findall(r'^warning: ', content, re.MULTILINE)
errs = re.findall(r'^error\[.*?\]:', content, re.MULTILINE)
errs2 = re.findall(r'^error: ', content, re.MULTILINE)
print(f"error[Exxxx]: {len(errs)}")
print(f"error: {len(errs2)}")
print(f"warning[clippy::xxx]: {len(warns)}")
print(f"warning: {len(warns2)}")

# Get unique lints
lints = re.findall(r'warning\[(clippy::\w+)\]', content)
from collections import Counter
for lint, count in Counter(lints).most_common(30):
    print(f"  {count:4d} {lint}")

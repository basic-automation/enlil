import sys
with open('clippy_new.txt') as f:
    lines = f.readlines()
print(f'Total lines: {len(lines)}')
lints = {}
for l in lines:
    for tag in ['clippy::']:
        if tag in l:
            start = l.index(tag) + len(tag)
            end = l.index(']', start) if ']' in l[start:] else l.index('`', start) if '`' in l[start:] else len(l)
            lint = l[start:end].strip().rstrip('`').rstrip(']')
            if lint:
                lints[lint] = lints.get(lint, 0) + 1
for lint, count in sorted(lints.items(), key=lambda x: -x[1])[:25]:
    print(f'  {count:4d} {lint}')

errs = [l for l in lines if l.startswith('error')]
warns = [l for l in lines if l.startswith('warning:')]
print(f'\nErrors: {len(errs)}, Warnings: {len(warns)}')

import re
for line in open('clippy_out.txt'):
    m = re.match(r'\s+-->\s+(.+)', line)
    if m:
        prev = open('clippy_out.txt').readlines()
    line = line.rstrip()
    if line.startswith('error:') or (line.strip().startswith('-->') and 'error' not in line):
        print(line)

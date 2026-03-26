import sys
fn, start, end = sys.argv[1], int(sys.argv[2]), int(sys.argv[3])
lines = open(fn, encoding='utf-8').readlines()
for i in range(start-1, min(end, len(lines))):
    print(f"{i+1}: {lines[i]}", end='')

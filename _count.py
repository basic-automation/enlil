import sys
for line in open(sys.argv[1]):
    if 'test result' in line:
        print(line.strip())

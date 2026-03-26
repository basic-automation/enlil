with open('clippy_out.txt') as f:
    for line in f:
        if line.startswith('error'):
            print(line.rstrip())

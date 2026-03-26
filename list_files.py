with open('clippy_output.txt','r') as f:
    lines = f.readlines()
files = set()
for l in lines:
    if '-->' in l:
        p = l.split('-->')[1].strip().split(':')[0]
        files.add(p)
for f in sorted(files):
    print(f)

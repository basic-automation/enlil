import sys  
f=open('enlil-roadmap.md',encoding='utf-8')  
lines=f.readlines()  
for i,l in enumerate(lines):  
    if l.startswith('### 3'): print(i, l.rstrip())  

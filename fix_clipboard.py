import re

f = 'enlil-devices/src/bridge/clipboard.rs'
with open(f, 'r') as fh:
    c = fh.read()

# Fix use_self in Default impl
c = c.replace(
    '        ClipboardPolicy {\n            allowed_types: vec!["text".to_string()]',
    '        Self {\n            allowed_types: vec!["text".to_string()]'
)

# Fix str::to_string (iter gives &&str, need .copied() first)
c = c.replace(
    'allowed_types.iter().map(str::to_string).collect()',
    'allowed_types.iter().copied().map(String::from).collect()'
)

# Fix use_self in ClipboardPolicy::new
c = c.replace(
    '        ClipboardPolicy {\n            allowed_types: allowed_types',
    '        Self {\n            allowed_types: allowed_types'
)

# Fix use_self in ClipboardHub::new
c = c.replace(
    '        ClipboardHub {\n            history:',
    '        Self {\n            history:'
)

with open(f, 'w') as fh:
    fh.write(c)
print('fixed clipboard.rs')

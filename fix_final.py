import re

def fix_file(path, fixups):
    """Apply line-based fixups to a file. fixups = [(line_num, old_text, new_text), ...]"""
    with open(path, 'r', encoding='utf-8') as f:
        lines = f.readlines()
    for line_num, old, new in fixups:
        idx = line_num - 1
        if idx < len(lines) and old in lines[idx]:
            lines[idx] = lines[idx].replace(old, new)
    with open(path, 'w', encoding='utf-8') as f:
        f.writelines(lines)

def insert_before(path, line_num, text):
    """Insert text before a given line number."""
    with open(path, 'r', encoding='utf-8') as f:
        lines = f.readlines()
    idx = line_num - 1
    lines.insert(idx, text)
    with open(path, 'w', encoding='utf-8') as f:
        f.writelines(lines)

def insert_lines_before(path, target_line, new_lines):
    """Insert multiple lines before a target line number."""
    with open(path, 'r', encoding='utf-8') as f:
        lines = f.readlines()
    idx = target_line - 1
    for i, nl in enumerate(new_lines):
        lines.insert(idx + i, nl + '\n')
    with open(path, 'w', encoding='utf-8') as f:
        f.writelines(lines)

def replace_range(path, start, end, new_lines):
    """Replace lines start..end (1-indexed, inclusive) with new_lines."""
    with open(path, 'r', encoding='utf-8') as f:
        lines = f.readlines()
    before = lines[:start-1]
    after = lines[end:]
    with open(path, 'w', encoding='utf-8') as f:
        f.writelines(before + [l + '\n' for l in new_lines] + after)

# ============================================================
# 1. threading/mod.rs - Debug impl + doc comments
# ============================================================
p = 'enlil-platform/src/threading/mod.rs'

# Fix Debug impl: use .finish_non_exhaustive() instead of .finish()
fix_file(p, [
    (165, '.finish()', '.finish_non_exhaustive()'),
])

# Add # Panics to spawn_hosted (before line 321 which is #[must_use])
# Current: line 317 = "/// Spawn a task on a real OS thread (hosted mode)."
# Need to add # Panics section after the existing doc comment, before #[must_use]
with open(p, 'r', encoding='utf-8') as f:
    lines = f.readlines()

new_lines = []
i = 0
while i < len(lines):
    line = lines[i]
    
    # spawn_hosted: add Panics doc before #[must_use] on the line before pub fn spawn_hosted
    if i < len(lines)-1 and '    #[must_use]\n' == line and 'pub fn spawn_hosted' in lines[i+1]:
        new_lines.append('    ///\n')
        new_lines.append('    /// # Panics\n')
        new_lines.append('    ///\n')
        new_lines.append('    /// Panics if the underlying OS thread cannot be spawned.\n')
        new_lines.append(line)
    # But spawn_hosted is not indented - it's a free function
    elif '#[must_use]\n' == line and i+1 < len(lines) and 'pub fn spawn_hosted' in lines[i+1]:
        new_lines.append('///\n')
        new_lines.append('/// # Panics\n')
        new_lines.append('///\n')
        new_lines.append('/// Panics if the underlying OS thread cannot be spawned.\n')
        new_lines.append(line)
    # submit: add Panics + Errors doc
    elif '    /// Submit a task, respecting its affinity.\n' == line:
        new_lines.append(line)
        new_lines.append('    ///\n')
        new_lines.append('    /// # Panics\n')
        new_lines.append('    ///\n')
        new_lines.append('    /// Panics if a mutex guarding an internal run-queue is poisoned.\n')
        new_lines.append('    ///\n')
        new_lines.append('    /// # Errors\n')
        new_lines.append('    ///\n')
        new_lines.append('    /// Returns an error if the pinned CPU index is out of range or no\n')
        new_lines.append('    /// valid CPU exists in the affinity set.\n')
        i += 1
        continue
    # run_one: add Panics doc
    elif '    /// Run one task on the given CPU. Returns `true` if work was found.\n' == line:
        new_lines.append(line)
        # Skip ahead to find the existing doc lines until #[must_use]
        # Actually let's just add after this block
    elif '    /// neighbour.\n' == line and i > 0 and 'If the local queue is empty' in lines[i-1]:
        new_lines.append(line)
        new_lines.append('    ///\n')
        new_lines.append('    /// # Panics\n')
        new_lines.append('    ///\n')
        new_lines.append('    /// Panics if a mutex guarding a run-queue is poisoned.\n')
        i += 1
        continue
    # total_pending: add Panics doc  
    elif '    /// Total pending tasks across all CPUs.\n' == line:
        new_lines.append(line)
        new_lines.append('    ///\n')
        new_lines.append('    /// # Panics\n')
        new_lines.append('    ///\n')
        new_lines.append('    /// Panics if a mutex guarding a run-queue is poisoned.\n')
        i += 1
        continue
    else:
        new_lines.append(line)
    i += 1

with open(p, 'w', encoding='utf-8') as f:
    f.writelines(new_lines)
print(f"Fixed {p}")

# ============================================================
# 2. sync/mod.rs - doc comments + significant drops
# ============================================================
p = 'enlil-platform/src/sync/mod.rs'

with open(p, 'r', encoding='utf-8') as f:
    lines = f.readlines()

new_lines = []
i = 0
while i < len(lines):
    line = lines[i]
    
    # Mutex::lock - add Panics before pub fn lock
    if '    /// Acquires the mutex, blocking until available.\n' == line:
        new_lines.append(line)
        new_lines.append('    ///\n')
        new_lines.append('    /// # Panics\n')
        new_lines.append('    ///\n')
        new_lines.append('    /// Panics if the underlying mutex is poisoned (Linux).\n')
        i += 1
        continue
    
    # RwLock::read - add Panics
    elif '    /// Acquires a shared read lock.\n' == line:
        new_lines.append(line)
        new_lines.append('    ///\n')
        new_lines.append('    /// # Panics\n')
        new_lines.append('    ///\n')
        new_lines.append('    /// Panics if the underlying lock is poisoned (Linux).\n')
        i += 1
        continue
    
    # RwLock::write - add Panics
    elif '    /// Acquires an exclusive write lock.\n' == line:
        new_lines.append(line)
        new_lines.append('    ///\n')
        new_lines.append('    /// # Panics\n')
        new_lines.append('    ///\n')
        new_lines.append('    /// Panics if the underlying lock is poisoned (Linux).\n')
        i += 1
        continue
    
    # Condvar::wait (linux) - add Panics. Line: "    /// Blocks the current thread until notified.\n"
    elif '    /// Blocks the current thread until notified.\n' == line:
        new_lines.append(line)
        new_lines.append('    ///\n')
        new_lines.append('    /// # Panics\n')
        new_lines.append('    ///\n')
        new_lines.append('    /// Panics if the underlying condvar wait returns a poisoned error.\n')
        i += 1
        continue
    
    # Sender::drop - add drop(inner) after inner.closed = true
    elif '                inner.closed = true;\n' == line and i+1 < len(lines) and 'Wake the receiver' in lines[i+1]:
        new_lines.append(line)
        # Find the notify_all line and add drop after it
        # Actually, we need to drop before notify. Let me look at the structure:
        # inner.closed = true;
        # // Wake the receiver...
        # self.shared.not_empty.notify_all();
        # We need: set closed, notify, then drop. But clippy wants drop after last usage.
        # The notify uses self.shared, not inner. So we can drop inner before notify.
        i += 1
        # Skip the comment line
        new_lines.append(lines[i])  # "// Wake the receiver..." 
        i += 1
        # This should be the notify_all line
        new_lines.append('                drop(inner);\n')
        new_lines.append(lines[i])  # self.shared.not_empty.notify_all();
        i += 1
        continue
    
    # try_send - add drop(inner) before Ok(())
    # The pattern: inner.buffer.push_back(value); ... self.shared.not_full... ... Ok(())
    # Actually let me find the exact try_send function
    elif '        inner.buffer.push_back(value);\n' == line and i+1 < len(lines) and 'not_empty' in lines[i+1]:
        new_lines.append(line)
        new_lines.append('        drop(inner);\n')
        i += 1
        continue
    
    else:
        new_lines.append(line)
    i += 1

with open(p, 'w', encoding='utf-8') as f:
    f.writelines(new_lines)
print(f"Fixed {p}")

# ============================================================
# 3. sync/mod.rs tests - condvar_wait_notify significant drops
# ============================================================
# Re-read after previous edits
with open(p, 'r', encoding='utf-8') as f:
    content = f.read()

# Fix: In the spawned thread, add drop(started) after *started = true;
content = content.replace(
    '            *started = true;\n            cvar.notify_one();\n        });',
    '            *started = true;\n            drop(started);\n            cvar.notify_one();\n        });'
)

# Fix: In the main thread, drop guard after assert
content = content.replace(
    '        let guard = cvar.wait_while(guard, |started| !*started);\n        assert!(*guard);\n        handle.join().unwrap();',
    '        let guard = cvar.wait_while(guard, |started| !*started);\n        assert!(*guard);\n        drop(guard);\n        handle.join().unwrap();'
)

with open(p, 'w', encoding='utf-8') as f:
    f.write(content)
print(f"Fixed {p} tests")

# ============================================================
# 4. async_rt/mod.rs - significant drops in tests
# ============================================================
p = 'enlil-platform/src/async_rt/mod.rs'

with open(p, 'r', encoding='utf-8') as f:
    content = f.read()

# Fix priority_executor_respects_order test
content = content.replace(
    '        let result = order.lock().unwrap();\n        assert_eq!(*result, vec!["high", "normal", "low"]);',
    '        let result = order.lock().unwrap();\n        assert_eq!(*result, vec!["high", "normal", "low"]);\n        drop(result);'
)

# Fix priority_executor_mixed test  
content = content.replace(
    '        let result = order.lock().unwrap();\n        assert_eq!(\n            *result,\n            vec!["high-1", "high-2", "normal-1", "normal-2", "low-1", "low-2"]\n        );',
    '        let result = order.lock().unwrap();\n        assert_eq!(\n            *result,\n            vec!["high-1", "high-2", "normal-1", "normal-2", "low-1", "low-2"]\n        );\n        drop(result);'
)

with open(p, 'w', encoding='utf-8') as f:
    f.write(content)
print(f"Fixed {p}")

# ============================================================
# 5. io/mod.rs - remove dead write_byte entirely
# ============================================================
p = 'enlil-platform/src/io/mod.rs'

with open(p, 'r', encoding='utf-8') as f:
    lines = f.readlines()

# Find and remove write_byte method (from doc comment to closing brace)
new_lines = []
skip = False
i = 0
while i < len(lines):
    line = lines[i]
    # Start of write_byte doc comment
    if '    /// Write a single byte to the framebuffer' in line:
        skip = True
    if skip:
        # End of write_byte method
        if line.strip() == '}' and i > 0 and ('// 4. Update cursor' in lines[i-1] or lines[i-1].strip().startswith('//')):
            skip = False
            i += 1
            continue
        i += 1
        continue
    new_lines.append(line)
    i += 1

with open(p, 'w', encoding='utf-8') as f:
    f.writelines(new_lines)
print(f"Fixed {p}")

print("\nAll final fixes applied!")

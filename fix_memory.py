import re

with open(r'D:\Development\enlil\enlil-platform\src\memory\mod.rs', 'r') as f:
    lines = f.readlines()

# Track all modifications as (line_number, old_text, new_text) or insertion points
# We'll work with 0-indexed line numbers

modifications = {}

# 1. SlabCache::new - add # Panics before line 218 (0-indexed: 217)
# Current: line 217 = "    /// Create a new slab cache for objects of the given size."
# line 218 = "    #[must_use]"
# Need to add panics doc between line 217 and 218
idx = 216  # 0-indexed for "    /// Create a new slab cache for objects of the given size."
# Insert after line 217 (the doc line), before #[must_use]
modifications[217] = ('insert_before', '    ///\n    /// # Panics\n    ///\n    /// Panics if `object_size` is less than 8.\n')

# 2. PhysAddr::is_aligned - line 349 (0-indexed 348)
# "    /// Check whether the address is aligned to `align`."
# line 349 = #[must_use]
modifications[349] = ('insert_before', '    ///\n    /// # Panics\n    ///\n    /// Panics if `align` is not a power of two.\n')

# 3. PhysAddr::align_up - line 356 (0-indexed 355)
# "    /// Round the address up to the next multiple of `align`."
# line 356 = #[must_use]
modifications[356] = ('insert_before', '    ///\n    /// # Panics\n    ///\n    /// Panics if `align` is not a power of two.\n')

# 4. PhysAddr::align_down - line 363 (0-indexed 362)
modifications[363] = ('insert_before', '    ///\n    /// # Panics\n    ///\n    /// Panics if `align` is not a power of two.\n')

# 5. VirtAddr::is_aligned - line 416 (0-indexed 415)
modifications[416] = ('insert_before', '    ///\n    /// # Panics\n    ///\n    /// Panics if `align` is not a power of two.\n')

# 6. VirtAddr::align_up - line 423 (0-indexed 422)
modifications[423] = ('insert_before', '    ///\n    /// # Panics\n    ///\n    /// Panics if `align` is not a power of two.\n')

# 7. VirtAddr::align_down - line 430 (0-indexed 429)
modifications[430] = ('insert_before', '    ///\n    /// # Panics\n    ///\n    /// Panics if `align` is not a power of two.\n')

# 8. BitmapFrameAllocator::new - line 528 (0-indexed 527)
modifications[527] = ('insert_before', '    ///\n    /// # Panics\n    ///\n    /// Panics if `base` is not page-aligned or if size is not a multiple of `PAGE_SIZE`.\n')

# 9. BitmapFrameAllocator::deallocate_frame - line 563 has the doc "    /// Deallocate a previously allocated frame."
# line 564 is "    pub fn deallocate_frame"
# Need to insert panics doc. The doc is at line 563 (1-indexed), so 562 (0-indexed).
# Let's find exact positions
for i, line in enumerate(lines):
    if '    pub fn deallocate_frame' in line and i > 550:
        # The doc comment should be right before this
        modifications[i] = ('insert_before', '    ///\n    /// # Panics\n    ///\n    /// Panics if the frame is outside the managed region.\n')
        break

# Now apply modifications in reverse order to preserve line numbers
output = []
for i, line in enumerate(lines):
    if i in modifications:
        action, text = modifications[i]
        if action == 'insert_before':
            output.append(text)
    output.append(line)

# Now fix the casts - work on the string
content = ''.join(output)

# Fix PAGE_SIZE as usize -> usize::try_from(PAGE_SIZE).expect("PAGE_SIZE exceeds usize")
content = content.replace(
    'let total_frames = size / PAGE_SIZE as usize;',
    'let total_frames = size / usize::try_from(PAGE_SIZE).expect("PAGE_SIZE exceeds usize");'
)

# Fix (frame.number() - self.base_frame) as usize in deallocate_frame
# There are two instances of this pattern
content = content.replace(
    '(frame.number() - self.base_frame) as usize',
    'usize::try_from(frame.number() - self.base_frame).expect("frame index exceeds usize")'
)

with open(r'D:\Development\enlil\enlil-platform\src\memory\mod.rs', 'w') as f:
    f.write(content)

print("Done - memory/mod.rs fixed")

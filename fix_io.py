import re

path = 'enlil-platform/src/io/mod.rs'

with open(path, 'r', encoding='utf-8') as f:
    content = f.read()

# 1. Remove the dead write_byte method entirely (lines 270-290)
# Find from the doc comment before write_byte to the closing brace
content = re.sub(
    r'    /// Write a single byte to the framebuffer.*?'
    r'    /// 4\. Update cursor_col and cursor_row as needed\n    \}\n',
    '',
    content,
    flags=re.DOTALL
)

# 2. Fix the write() impl - replace underscore-prefixed vars and use Self::
# Replace the entire write method body
old_write = '''    fn write(&self, buf: &[u8]) -> io::Result<usize> {
        // On bare-metal: render characters to framebuffer.
        // Iterate through each byte and use stride, cursor_col, cursor_row
        // to render to the appropriate position.
        for &byte in buf {
            // Use stride, cursor_col, cursor_row to determine framebuffer position
            let _pixel_x = self.cursor_col * FramebufferConsole::CHAR_WIDTH;
            let _pixel_y = self.cursor_row * FramebufferConsole::CHAR_HEIGHT;
            let _offset = _pixel_y * self.stride + _pixel_x * 4;
            
            // Byte rendering would go here (Phase 6)
            let _ = byte;
        }
        Ok(buf.len())
    }'''

new_write = '''    fn write(&self, buf: &[u8]) -> io::Result<usize> {
        // On bare-metal: render characters to framebuffer.
        // Stubbed \u2014 real pixel rendering in Phase 6.
        // Each byte would be rendered as a glyph at the current cursor position
        // using stride, cursor_col, cursor_row to determine framebuffer offset.
        let _ = (self.stride, self.cursor_col, self.cursor_row, self.base);
        Ok(buf.len())
    }'''

content = content.replace(old_write, new_write)

with open(path, 'w', encoding='utf-8') as f:
    f.write(content)

print("Fixed io/mod.rs")

$f = 'D:\Development\enlil\enlil-devices\src\display\mod.rs'
$lines = Get-Content $f

$out = @()
$panics_doc = @('    /// # Panics', '    /// Panics if an internal lock is poisoned.')

# Functions that need # Panics docs (pub fn NAME patterns)
$needs_panics = @(
    'pub fn update_frame',
    'pub fn submit_frame',
    'pub fn update_pixels',
    'pub fn clear(',
    'pub fn create_zone',
    'pub fn delete_zone',
    'pub fn set_zone_visibility',
    'pub fn get_layout',
    'pub fn route_event',
    'pub fn next_event',
    'pub fn set_focus',
    'pub fn get_focus',
    'pub fn register_source',
    'pub fn unregister_source',
    'pub fn set_display_mode',
    'pub fn get_display_mode',
    'pub fn enable_pip',
    'pub fn disable_pip',
    'pub fn is_pip_enabled',
    'pub fn composite_frame',
    'pub fn resize(',
    'pub fn get_config',
    'pub fn set_config'
)

# Lines after which to insert drop() - format: "pattern_in_line" => "drop_statement"
# We'll handle these by tracking state

for ($i = 0; $i -lt $lines.Count; $i++) {
    $line = $lines[$i]
    
    # Fix test literal
    if ($line -match '0xFF000000') {
        $line = $line -replace '0xFF000000', '0xFF00_0000'
    }
    
    # Add #[allow(clippy::cast_possible_truncation)] before FrameRef::new fn
    # The new() function around line 40 that has the casts
    if ($line -match '^\s+pub fn new\(width: u32, height: u32, format: PixelFormat\)') {
        $out += '    #[allow(clippy::cast_possible_truncation)]'
    }
    
    # Add #[allow(clippy::significant_drop_tightening)] before composite_frame
    if ($line -match '^\s+pub fn composite_frame\(&self\)') {
        $out += '    #[allow(clippy::significant_drop_tightening)]'
    }
    
    # Check if this line is a pub fn that needs # Panics
    $needs_it = $false
    foreach ($pat in $needs_panics) {
        if ($line -match [regex]::Escape($pat)) {
            $needs_it = $true
            break
        }
    }
    
    if ($needs_it) {
        # Insert panics doc before this line
        # But check if previous line already has # Panics
        $prev = if ($out.Count -gt 0) { $out[$out.Count - 1] } else { '' }
        if ($prev -notmatch '# Panics') {
            $out += $panics_doc
        }
    }
    
    $out += $line
    
    # Insert drop() calls after specific patterns
    # submit_frame: after frames.pop_front();
    if ($line -match '^\s+frames\.pop_front\(\);') {
        $out += '            drop(frames);'
    }
    
    # clear: after chunk.copy_from_slice - need to detect end of for loop
    # Actually, the for loop body has copy_from_slice, then }, then we need drop(buf)
    # Let's detect the closing brace of the for loop in clear()
    
    # create_zone: after *id_gen += 1;
    if ($line -match '^\s+\*id_gen \+= 1;') {
        $out += '        drop(id_gen);'
    }
    
    # enable_pip: after config.pip.source_zone_id = Some(zone_id);
    if ($line -match 'config\.pip\.source_zone_id = Some\(zone_id\);') {
        $out += '        drop(config);'
    }
    
    # disable_pip: after config.pip.source_zone_id = None;
    if ($line -match 'config\.pip\.source_zone_id = None;') {
        $out += '        drop(config);'
    }
}

# Handle clear() drop(buf) - find the pattern in the output
# We need to add drop(buf); after the for loop in clear()
# The pattern is: chunk.copy_from_slice... then }  then we need drop(buf)
# Let's do a second pass
$final = @()
$in_clear = $false
$clear_for_depth = 0
for ($i = 0; $i -lt $out.Count; $i++) {
    $line = $out[$i]
    
    if ($line -match 'pub fn clear\(&self, color: u32\)') {
        $in_clear = $true
    }
    
    if ($in_clear -and $line -match 'for chunk in buf\.chunks_exact_mut') {
        $clear_for_depth = 1
    }
    
    $final += $line
    
    if ($in_clear -and $clear_for_depth -gt 0) {
        if ($line -match '^\s+\}' -and $clear_for_depth -eq 1) {
            # This closing brace ends the for loop
            $final += '        drop(buf);'
            $clear_for_depth = 0
            $in_clear = $false
        }
    }
}

$final | Set-Content $f

//! Display compositor and framebuffer management
//!
//! This module provides:
//! - Framebuffer sources (IVSHMEM, VirtIO-GPU, compositor-owned)
//! - Zone-based layout and composition
//! - Input routing and focus management
//! - Picture-in-Picture support
//! - Configuration via serde

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex, RwLock};
use serde::{Deserialize, Serialize};

/// Pixel format enumeration
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum PixelFormat {
    RGBA8888,
    BGRA8888,
    RGB565,
    XRGB8888,
}

impl PixelFormat {
    /// Bytes per pixel
    #[must_use]
    pub const fn bytes_per_pixel(&self) -> usize {
        match self {
            Self::RGBA8888 | Self::BGRA8888 | Self::XRGB8888 => 4,
            Self::RGB565 => 2,
        }
    }
}

/// Frame reference with metadata
#[derive(Debug, Clone)]
pub struct FrameRef {
    pub id: u64,
    pub timestamp_ns: u64,
    pub pixel_format: PixelFormat,
    pub width: u32,
    pub height: u32,
    pub stride: u32,
}

impl FrameRef {
    #[allow(clippy::cast_possible_truncation)]
    pub fn new(width: u32, height: u32, pixel_format: PixelFormat) -> Self {
        Self {
            id: { static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1); COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed) },
            timestamp_ns: {
                use std::time::{SystemTime, UNIX_EPOCH};
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .map_or(0, |d| d.as_nanos() as u64)
            },
            pixel_format,
            width,
            height,
            stride: width * pixel_format.bytes_per_pixel() as u32,
        }
    }
}

/// Trait for framebuffer data sources
pub trait FramebufferSource: Send + Sync {
    /// Get current frame reference
    fn current_frame(&self) -> Option<FrameRef>;

    /// Get raw pixel data for a frame
    fn get_frame_data(&self, frame_id: u64) -> Option<Vec<u8>>;

    /// Notify source of frame consumption
    fn frame_consumed(&self, frame_id: u64);

    /// Source type identifier
    fn source_type(&self) -> &'static str;
}

/// IVSHMEM-based framebuffer source
#[derive(Debug)]
pub struct IvshmemSource {
    shared_memory: Arc<Vec<u8>>,
    current_frame: Arc<Mutex<Option<FrameRef>>>,
    format: PixelFormat,
}

impl IvshmemSource {
    #[must_use]
    pub fn new(size: usize, format: PixelFormat) -> Self {
        Self {
            shared_memory: Arc::new(vec![0u8; size]),
            current_frame: Arc::new(Mutex::new(None)),
            format,
        }
    }

    /// # Panics
    /// Panics if an internal lock is poisoned.
    pub fn update_frame(&self, width: u32, height: u32) {
        let frame = FrameRef::new(width, height, self.format);
        *self.current_frame.lock().unwrap() = Some(frame);
    }
}

impl FramebufferSource for IvshmemSource {
    fn current_frame(&self) -> Option<FrameRef> {
        self.current_frame.lock().unwrap().clone()
    }

    fn get_frame_data(&self, _frame_id: u64) -> Option<Vec<u8>> {
        Some(self.shared_memory.as_ref().clone())
    }

    fn frame_consumed(&self, _frame_id: u64) {}

    fn source_type(&self) -> &'static str {
        "ivshmem"
    }
}

/// Shared frame buffer: list of `(frame_id, pixel_data)` pairs.
type FrameQueue = Arc<RwLock<VecDeque<(u64, Vec<u8>)>>>;

/// VirtIO-GPU framebuffer source
#[derive(Debug)]
pub struct VirtioGpuSource {
    frames: FrameQueue,
    current_frame: Arc<Mutex<Option<FrameRef>>>,
    format: PixelFormat,
    max_frames: usize,
}

impl VirtioGpuSource {
    #[must_use]
    pub fn new(format: PixelFormat, max_frames: usize) -> Self {
        Self {
            frames: Arc::new(RwLock::new(VecDeque::with_capacity(max_frames))),
            current_frame: Arc::new(Mutex::new(None)),
            format,
            max_frames,
        }
    }

    /// # Panics
    /// Panics if an internal lock is poisoned.
    pub fn submit_frame(&self, width: u32, height: u32, data: Vec<u8>) {
        let frame = FrameRef::new(width, height, self.format);
        let frame_id = frame.id;

        let mut frames = self.frames.write().unwrap();
        frames.push_back((frame_id, data));
        if frames.len() > self.max_frames {
            frames.pop_front();
        }
        drop(frames);
        *self.current_frame.lock().unwrap() = Some(frame);
    }
}

impl FramebufferSource for VirtioGpuSource {
    fn current_frame(&self) -> Option<FrameRef> {
        self.current_frame.lock().unwrap().clone()
    }

    fn get_frame_data(&self, frame_id: u64) -> Option<Vec<u8>> {
        self.frames
            .read()
            .unwrap()
            .iter()
            .find(|(id, _)| *id == frame_id)
            .map(|(_, data)| data.clone())
    }

    fn frame_consumed(&self, frame_id: u64) {
        let mut frames = self.frames.write().unwrap();
        frames.retain(|(id, _)| *id != frame_id);
    }

    fn source_type(&self) -> &'static str {
        "virtio-gpu"
    }
}

/// Compositor-owned framebuffer (generated content)
#[derive(Debug)]
pub struct CompositorOwnedSource {
    buffer: Arc<Mutex<Vec<u8>>>,
    current_frame: Arc<Mutex<Option<FrameRef>>>,
    format: PixelFormat,
    width: u32,
    height: u32,
}

impl CompositorOwnedSource {
    #[must_use]
    #[allow(clippy::cast_possible_truncation)]
    pub fn new(width: u32, height: u32, format: PixelFormat) -> Self {
        let size = (width * height) as usize * format.bytes_per_pixel();
        Self {
            buffer: Arc::new(Mutex::new(vec![0u8; size])),
            current_frame: Arc::new(Mutex::new(None)),
            format,
            width,
            height,
        }
    }

    /// # Panics
    /// Panics if an internal lock is poisoned.
    pub fn update_pixels(&self, pixels: Vec<u8>) {
        *self.buffer.lock().unwrap() = pixels;
        let frame = FrameRef::new(self.width, self.height, self.format);
        *self.current_frame.lock().unwrap() = Some(frame);
    }

    /// # Panics
    /// Panics if an internal lock is poisoned.
    pub fn clear(&self, color: u32) {
        let mut buf = self.buffer.lock().unwrap();
        for chunk in buf.chunks_exact_mut(self.format.bytes_per_pixel()) {
            chunk.copy_from_slice(&color.to_le_bytes()[..chunk.len()]);
        }
        drop(buf);
        let frame = FrameRef::new(self.width, self.height, self.format);
        *self.current_frame.lock().unwrap() = Some(frame);
    }
}

impl FramebufferSource for CompositorOwnedSource {
    fn current_frame(&self) -> Option<FrameRef> {
        self.current_frame.lock().unwrap().clone()
    }

    fn get_frame_data(&self, _frame_id: u64) -> Option<Vec<u8>> {
        Some(self.buffer.lock().unwrap().clone())
    }

    fn frame_consumed(&self, _frame_id: u64) {}

    fn source_type(&self) -> &'static str {
        "compositor"
    }
}

/// A display zone (rectangular region with content source)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Zone {
    pub id: u32,
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
    pub z_index: i32,
    pub visible: bool,
    pub name: String,
}

impl Zone {
    #[must_use]
    pub const fn new(id: u32, x: u32, y: u32, width: u32, height: u32, name: String) -> Self {
        Self {
            id,
            x,
            y,
            width,
            height,
            z_index: 0,
            visible: true,
            name,
        }
    }

    #[must_use]
    pub const fn contains_point(&self, px: u32, py: u32) -> bool {
        px >= self.x && px < self.x + self.width && py >= self.y && py < self.y + self.height
    }
}

/// Zone layout configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ZoneLayout {
    pub zones: Vec<Zone>,
    pub screen_width: u32,
    pub screen_height: u32,
}

impl ZoneLayout {
    #[must_use]
    pub const fn new(screen_width: u32, screen_height: u32) -> Self {
        Self {
            zones: Vec::new(),
            screen_width,
            screen_height,
        }
    }

    pub fn add_zone(&mut self, zone: Zone) {
        self.zones.push(zone);
        self.zones.sort_by_key(|z| z.z_index);
    }

    #[must_use]
    pub fn find_zone_at(&self, x: u32, y: u32) -> Option<&Zone> {
        self.zones
            .iter()
            .rev()
            .find(|z| z.visible && z.contains_point(x, y))
    }

    #[must_use]
    pub fn find_zone_by_id(&self, id: u32) -> Option<&Zone> {
        self.zones.iter().find(|z| z.id == id)
    }

    pub fn find_zone_mut(&mut self, id: u32) -> Option<&mut Zone> {
        self.zones.iter_mut().find(|z| z.id == id)
    }
}

/// Layout engine for dynamic zone management
#[derive(Debug)]
pub struct ZoneLayoutEngine {
    layout: Arc<RwLock<ZoneLayout>>,
    next_zone_id: Arc<Mutex<u32>>,
}

impl ZoneLayoutEngine {
    #[must_use]
    pub fn new(screen_width: u32, screen_height: u32) -> Self {
        Self {
            layout: Arc::new(RwLock::new(ZoneLayout::new(screen_width, screen_height))),
            next_zone_id: Arc::new(Mutex::new(1)),
        }
    }

    #[must_use]
    /// # Panics
    /// Panics if an internal lock is poisoned.
    pub fn create_zone(&self, x: u32, y: u32, width: u32, height: u32, name: String) -> u32 {
        let mut id_gen = self.next_zone_id.lock().unwrap();
        let zone_id = *id_gen;
        *id_gen += 1;
        drop(id_gen);

        let zone = Zone::new(zone_id, x, y, width, height, name);
        self.layout.write().unwrap().add_zone(zone);
        zone_id
    }

    /// # Panics
    /// Panics if an internal lock is poisoned.
    pub fn delete_zone(&self, zone_id: u32) {
        let mut layout = self.layout.write().unwrap();
        layout.zones.retain(|z| z.id != zone_id);
    }

    /// # Panics
    /// Panics if an internal lock is poisoned.
    pub fn set_zone_visibility(&self, zone_id: u32, visible: bool) {
        if let Some(zone) = self.layout.write().unwrap().find_zone_mut(zone_id) {
            zone.visible = visible;
        }
    }

    #[must_use]
    /// # Panics
    /// Panics if an internal lock is poisoned.
    pub fn get_layout(&self) -> ZoneLayout {
        self.layout.read().unwrap().clone()
    }
}

/// Input event types
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputEvent {
    KeyDown { scancode: u16 },
    KeyUp { scancode: u16 },
    MouseMove { x: u32, y: u32 },
    MouseDown { button: u8 },
    MouseUp { button: u8 },
    Scroll { delta_x: i32, delta_y: i32 },
}

/// Mouse position tracking
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MousePosition {
    pub x: u32,
    pub y: u32,
}

/// Focus state for input routing
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FocusState {
    Focused,
    Blurred,
    PartialFocus,
}

/// Input router for zone-based event delivery
#[derive(Debug)]
pub struct InputRouter {
    zone_layout: Arc<RwLock<ZoneLayout>>,
    focused_zone: Arc<Mutex<Option<u32>>>,
    mouse_pos: Arc<Mutex<MousePosition>>,
    event_queue: Arc<Mutex<VecDeque<(u32, InputEvent)>>>,
}

impl InputRouter {
    pub fn new(zone_layout: Arc<RwLock<ZoneLayout>>) -> Self {
        Self {
            zone_layout,
            focused_zone: Arc::new(Mutex::new(None)),
            mouse_pos: Arc::new(Mutex::new(MousePosition { x: 0, y: 0 })),
            event_queue: Arc::new(Mutex::new(VecDeque::new())),
        }
    }

    /// # Panics
    /// Panics if an internal lock is poisoned.
    pub fn route_event(&self, event: InputEvent) {
        let layout = self.zone_layout.read().unwrap();

        let target_zone = match event {
            InputEvent::MouseMove { x, y } => {
                *self.mouse_pos.lock().unwrap() = MousePosition { x, y };
                layout.find_zone_at(x, y).map(|z| z.id)
            }
            InputEvent::MouseDown { .. } | InputEvent::MouseUp { .. } => {
                let pos = *self.mouse_pos.lock().unwrap();
                layout.find_zone_at(pos.x, pos.y).map(|z| z.id)
            }
            _ => *self.focused_zone.lock().unwrap(),
        };

        if let Some(zone_id) = target_zone {
            self.event_queue.lock().unwrap().push_back((zone_id, event));

            if matches!(event, InputEvent::MouseDown { .. }) {
                *self.focused_zone.lock().unwrap() = Some(zone_id);
            }
        }
    }

    #[must_use]
    /// # Panics
    /// Panics if an internal lock is poisoned.
    pub fn next_event(&self) -> Option<(u32, InputEvent)> {
        self.event_queue.lock().unwrap().pop_front()
    }

    /// # Panics
    /// Panics if an internal lock is poisoned.
    pub fn set_focus(&self, zone_id: Option<u32>) {
        *self.focused_zone.lock().unwrap() = zone_id;
    }

    #[must_use]
    /// # Panics
    /// Panics if an internal lock is poisoned.
    pub fn get_focus(&self) -> Option<u32> {
        *self.focused_zone.lock().unwrap()
    }
}

/// Picture-in-Picture configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PictureInPicture {
    pub enabled: bool,
    pub width: u32,
    pub height: u32,
    pub x_offset: u32,
    pub y_offset: u32,
    pub source_zone_id: Option<u32>,
}

impl Default for PictureInPicture {
    fn default() -> Self {
        Self {
            enabled: false,
            width: 320,
            height: 240,
            x_offset: 10,
            y_offset: 10,
            source_zone_id: None,
        }
    }
}

/// Display mode
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DisplayMode {
    Windowed,
    Fullscreen,
    PiP,
}

/// Hotkey configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HotkeyConfig {
    pub toggle_pip: Vec<u16>,
    pub cycle_zones: Vec<u16>,
    pub reset_layout: Vec<u16>,
}

impl Default for HotkeyConfig {
    fn default() -> Self {
        Self {
            toggle_pip: vec![29, 56, 25], // Ctrl+Alt+P
            cycle_zones: vec![29, 56, 9],  // Ctrl+Alt+Tab
            reset_layout: vec![29, 56, 19], // Ctrl+Alt+R
        }
    }
}

/// Layout configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LayoutConfig {
    pub default_layout: String,
    pub auto_resize: bool,
    pub animate_transitions: bool,
    pub transition_duration_ms: u64,
}

impl Default for LayoutConfig {
    fn default() -> Self {
        Self {
            default_layout: "standard".to_string(),
            auto_resize: true,
            animate_transitions: true,
            transition_duration_ms: 200,
        }
    }
}

/// Display configuration (serde support)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DisplayConfig {
    pub width: u32,
    pub height: u32,
    pub refresh_rate: u32,
    pub pixel_format: String,
    pub vsync: bool,
    pub layout: LayoutConfig,
    pub hotkeys: HotkeyConfig,
    pub pip: PictureInPicture,
}

impl Default for DisplayConfig {
    fn default() -> Self {
        Self {
            width: 1920,
            height: 1080,
            refresh_rate: 60,
            pixel_format: "RGBA8888".to_string(),
            vsync: true,
            layout: LayoutConfig::default(),
            hotkeys: HotkeyConfig::default(),
            pip: PictureInPicture::default(),
        }
    }
}

/// Main display compositor
pub struct DisplayCompositor {
    config: Arc<Mutex<DisplayConfig>>,
    layout_engine: Arc<ZoneLayoutEngine>,
    input_router: Arc<InputRouter>,
    framebuffer_sources: Arc<RwLock<HashMap<u32, Arc<dyn FramebufferSource>>>>,
    display_mode: Arc<Mutex<DisplayMode>>,
    pip_enabled: Arc<Mutex<bool>>,
}

impl DisplayCompositor {
    #[must_use]
    pub fn new(config: DisplayConfig) -> Self {
        let layout = Arc::new(RwLock::new(ZoneLayout::new(config.width, config.height)));
        let layout_engine = Arc::new(ZoneLayoutEngine::new(config.width, config.height));
        let input_router = Arc::new(InputRouter::new(layout));

        Self {
            config: Arc::new(Mutex::new(config)),
            layout_engine,
            input_router,
            framebuffer_sources: Arc::new(RwLock::new(HashMap::new())),
            display_mode: Arc::new(Mutex::new(DisplayMode::Windowed)),
            pip_enabled: Arc::new(Mutex::new(false)),
        }
    }

    /// # Panics
    /// Panics if an internal lock is poisoned.
    pub fn register_source(&self, zone_id: u32, source: Arc<dyn FramebufferSource>) {
        self.framebuffer_sources
            .write()
            .unwrap()
            .insert(zone_id, source);
    }

    /// # Panics
    /// Panics if an internal lock is poisoned.
    pub fn unregister_source(&self, zone_id: u32) {
        self.framebuffer_sources.write().unwrap().remove(&zone_id);
    }

    pub fn route_input(&self, event: InputEvent) {
        self.input_router.route_event(event);
    }

    #[must_use]
    pub fn get_next_input_event(&self) -> Option<(u32, InputEvent)> {
        self.input_router.next_event()
    }

    /// # Panics
    /// Panics if an internal lock is poisoned.
    pub fn set_display_mode(&self, mode: DisplayMode) {
        *self.display_mode.lock().unwrap() = mode;
    }

    #[must_use]
    /// # Panics
    /// Panics if an internal lock is poisoned.
    pub fn get_display_mode(&self) -> DisplayMode {
        *self.display_mode.lock().unwrap()
    }

    /// # Panics
    /// Panics if an internal lock is poisoned.
    pub fn enable_pip(&self, zone_id: u32) {
        let mut config = self.config.lock().unwrap();
        config.pip.source_zone_id = Some(zone_id);
        drop(config);
        *self.pip_enabled.lock().unwrap() = true;
    }

    /// # Panics
    /// Panics if an internal lock is poisoned.
    pub fn disable_pip(&self) {
        let mut config = self.config.lock().unwrap();
        config.pip.source_zone_id = None;
        drop(config);
        *self.pip_enabled.lock().unwrap() = false;
    }

    #[must_use]
    /// # Panics
    /// Panics if an internal lock is poisoned.
    pub fn is_pip_enabled(&self) -> bool {
        *self.pip_enabled.lock().unwrap()
    }

    #[must_use]
    #[allow(clippy::significant_drop_tightening)]
    /// # Panics
    /// Panics if an internal lock is poisoned.
    pub fn composite_frame(&self) -> Option<Vec<u8>> {
        let config = self.config.lock().unwrap();
        let sources = self.framebuffer_sources.read().unwrap();

        let frame_size = (config.width * config.height) as usize * 4;
        let mut composite_buffer = vec![0u8; frame_size];

        let layout = self.layout_engine.get_layout();
        for zone in &layout.zones {
            if !zone.visible {
                continue;
            }

            if let Some(source) = sources.get(&zone.id)
                && let Some(data) = source.current_frame().and_then(|f| source.get_frame_data(f.id)) {
                    // Simple copy composition
                    let bytes_per_pixel = 4;
                    for y in 0..zone.height.min(config.height.saturating_sub(zone.y)) {
                        for x in 0..zone.width.min(config.width.saturating_sub(zone.x)) {
                            let src_idx = ((y * zone.width + x) as usize) * bytes_per_pixel;
                            let dst_idx = (((zone.y + y) * config.width + zone.x + x) as usize) * bytes_per_pixel;
                            if src_idx + bytes_per_pixel <= data.len() && dst_idx + bytes_per_pixel <= composite_buffer.len() {
                                composite_buffer[dst_idx..dst_idx + bytes_per_pixel]
                                    .copy_from_slice(&data[src_idx..src_idx + bytes_per_pixel]);
                            }
                        }
                    }
                }
        }

        Some(composite_buffer)
    }

    /// # Panics
    /// Panics if an internal lock is poisoned.
    pub fn resize(&self, width: u32, height: u32) {
        let mut config = self.config.lock().unwrap();
        config.width = width;
        config.height = height;
    }

    #[must_use]
    /// # Panics
    /// Panics if an internal lock is poisoned.
    pub fn get_config(&self) -> DisplayConfig {
        self.config.lock().unwrap().clone()
    }

    /// # Panics
    /// Panics if an internal lock is poisoned.
    pub fn set_config(&self, config: DisplayConfig) {
        *self.config.lock().unwrap() = config;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_pixel_format_bytes() {
        assert_eq!(PixelFormat::RGBA8888.bytes_per_pixel(), 4);
        assert_eq!(PixelFormat::RGB565.bytes_per_pixel(), 2);
    }

    #[test]
    fn test_frame_ref_creation() {
        let frame = FrameRef::new(1920, 1080, PixelFormat::RGBA8888);
        assert_eq!(frame.width, 1920);
        assert_eq!(frame.height, 1080);
        assert_eq!(frame.stride, 1920 * 4);
    }

    #[test]
    fn test_ivshmem_source() {
        let source = IvshmemSource::new(4096, PixelFormat::RGBA8888);
        assert_eq!(source.source_type(), "ivshmem");

        source.update_frame(32, 32);
        assert!(source.current_frame().is_some());

        let frame = source.current_frame().unwrap();
        let data = source.get_frame_data(frame.id);
        assert!(data.is_some());
    }

    #[test]
    fn test_virtio_gpu_source() {
        let source = VirtioGpuSource::new(PixelFormat::RGBA8888, 10);
        assert_eq!(source.source_type(), "virtio-gpu");

        source.submit_frame(800, 600, vec![0; 3200]);
        let frame = source.current_frame();
        assert!(frame.is_some());
    }

    #[test]
    fn test_compositor_owned_source() {
        let source = CompositorOwnedSource::new(640, 480, PixelFormat::RGBA8888);
        assert_eq!(source.source_type(), "compositor");

        source.clear(0xFF00_0000);
        assert!(source.current_frame().is_some());
    }

    #[test]
    fn test_zone_contains_point() {
        let zone = Zone::new(1, 10, 20, 100, 100, "test".to_string());
        assert!(zone.contains_point(50, 60));
        assert!(!zone.contains_point(5, 60));
    }

    #[test]
    fn test_zone_layout() {
        let mut layout = ZoneLayout::new(1920, 1080);
        let zone = Zone::new(1, 0, 0, 960, 1080, "left".to_string());
        layout.add_zone(zone);

        assert!(layout.find_zone_at(100, 100).is_some());
        assert!(layout.find_zone_by_id(1).is_some());
    }

    #[test]
    fn test_zone_layout_engine() {
        let engine = ZoneLayoutEngine::new(1920, 1080);
        let z1 = engine.create_zone(0, 0, 960, 1080, "left".to_string());
        let z2 = engine.create_zone(960, 0, 960, 1080, "right".to_string());

        assert_ne!(z1, z2);
        engine.set_zone_visibility(z1, false);

        let layout = engine.get_layout();
        assert!(layout.find_zone_by_id(z1).is_some());
    }

    #[test]
    fn test_input_router() {
        let layout = Arc::new(RwLock::new(ZoneLayout::new(1920, 1080)));
        let router = InputRouter::new(layout.clone());

        {
            let mut l = layout.write().unwrap();
            l.add_zone(Zone::new(1, 0, 0, 960, 1080, "z1".to_string()));
        }

        router.route_event(InputEvent::MouseMove { x: 100, y: 100 });
        let event = router.next_event();
        assert!(event.is_some());
    }

    #[test]
    fn test_mouse_position_tracking() {
        let layout = Arc::new(RwLock::new(ZoneLayout::new(1920, 1080)));
        let router = InputRouter::new(layout);

        router.route_event(InputEvent::MouseMove { x: 500, y: 300 });
        // Event should be queued (though no zone exists)
    }

    #[test]
    fn test_focus_state() {
        let layout = Arc::new(RwLock::new(ZoneLayout::new(1920, 1080)));
        let router = InputRouter::new(layout);

        router.set_focus(Some(1));
        assert_eq!(router.get_focus(), Some(1));

        router.set_focus(None);
        assert_eq!(router.get_focus(), None);
    }

    #[test]
    fn test_display_config_serde() {
        let config = DisplayConfig::default();
        let json = serde_json::to_string(&config).unwrap();
        let deserialized: DisplayConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.width, 1920);
        assert_eq!(deserialized.height, 1080);
    }

    #[test]
    fn test_picture_in_picture() {
        let config = DisplayConfig::default();
        let compositor = DisplayCompositor::new(config);

        compositor.enable_pip(1);
        assert!(compositor.is_pip_enabled());

        compositor.disable_pip();
        assert!(!compositor.is_pip_enabled());
    }

    #[test]
    fn test_display_mode_transitions() {
        let config = DisplayConfig::default();
        let compositor = DisplayCompositor::new(config);

        compositor.set_display_mode(DisplayMode::Fullscreen);
        assert_eq!(compositor.get_display_mode(), DisplayMode::Fullscreen);

        compositor.set_display_mode(DisplayMode::Windowed);
        assert_eq!(compositor.get_display_mode(), DisplayMode::Windowed);
    }

    #[test]
    fn test_source_registration() {
        let config = DisplayConfig::default();
        let compositor = DisplayCompositor::new(config);
        let source = Arc::new(CompositorOwnedSource::new(1920, 1080, PixelFormat::RGBA8888));

        compositor.register_source(1, source);
        compositor.unregister_source(1);
    }

    #[test]
    fn test_compositor_resize() {
        let config = DisplayConfig {
            width: 1920,
            height: 1080,
            ..DisplayConfig::default()
        };
        let compositor = DisplayCompositor::new(config);

        compositor.resize(2560, 1440);
        let updated_config = compositor.get_config();
        assert_eq!(updated_config.width, 2560);
        assert_eq!(updated_config.height, 1440);
    }
}
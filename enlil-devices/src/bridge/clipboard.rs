use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};

/// Clipboard content types
#[derive(Debug, Clone, PartialEq)]
pub enum ClipboardContent {
    Text(String),
    Image(Vec<u8>),
    FileRef(String),
    Html(String),
    RichText(Vec<u8>),
}

impl ClipboardContent {
    pub fn size(&self) -> usize {
        match self {
            ClipboardContent::Text(s) => s.len(),
            ClipboardContent::Image(data) => data.len(),
            ClipboardContent::FileRef(path) => path.len(),
            ClipboardContent::Html(s) => s.len(),
            ClipboardContent::RichText(data) => data.len(),
        }
    }

    pub fn content_type(&self) -> &'static str {
        match self {
            ClipboardContent::Text(_) => "text",
            ClipboardContent::Image(_) => "image",
            ClipboardContent::FileRef(_) => "fileref",
            ClipboardContent::Html(_) => "html",
            ClipboardContent::RichText(_) => "richtext",
        }
    }
}

/// Per-guest clipboard policy
#[derive(Debug, Clone)]
pub struct ClipboardPolicy {
    pub allowed_types: Vec<String>,
    pub max_size: usize,
    pub read_allowed: bool,
    pub write_allowed: bool,
}

impl Default for ClipboardPolicy {
    fn default() -> Self {
        ClipboardPolicy {
            allowed_types: vec!["text".to_string()],
            max_size: 1024 * 1024,
            read_allowed: true,
            write_allowed: true,
        }
    }
}

impl ClipboardPolicy {
    pub fn new(allowed_types: Vec<&str>, max_size: usize) -> Self {
        ClipboardPolicy {
            allowed_types: allowed_types.iter().map(|s| s.to_string()).collect(),
            max_size,
            read_allowed: true,
            write_allowed: true,
        }
    }

    pub fn allows(&self, content: &ClipboardContent) -> bool {
        content.size() <= self.max_size
            && self.allowed_types.contains(&content.content_type().to_string())
    }
}

/// Clipboard entry with sequence number
#[derive(Debug, Clone)]
struct ClipboardEntry {
    content: ClipboardContent,
    sequence: u64,
    source_guest: u32,
}

/// Clipboard hub routing and history
pub struct ClipboardHub {
    history: Arc<Mutex<VecDeque<ClipboardEntry>>>,
    sequence: Arc<Mutex<u64>>,
    max_history: usize,
    policies: Arc<Mutex<HashMap<u32, ClipboardPolicy>>>,
}

impl ClipboardHub {
    pub fn new(max_history: usize) -> Self {
        ClipboardHub {
            history: Arc::new(Mutex::new(VecDeque::new())),
            sequence: Arc::new(Mutex::new(0)),
            max_history,
            policies: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub fn set_policy(&self, guest_id: u32, policy: ClipboardPolicy) {
        if let Ok(mut policies) = self.policies.lock() {
            policies.insert(guest_id, policy);
        }
    }

    pub fn write(&self, guest_id: u32, content: ClipboardContent) -> Result<u64, &'static str> {
        let policies = self.policies.lock().map_err(|_| "Lock poisoned")?;
        let default_policy = ClipboardPolicy::default();
        let policy = policies.get(&guest_id).unwrap_or(&default_policy);

        if !policy.write_allowed {
            return Err("Write not allowed");
        }
        if !policy.allows(&content) {
            return Err("Content violates policy");
        }
        drop(policies);

        let mut seq_guard = self.sequence.lock().map_err(|_| "Lock poisoned")?;
        let seq = *seq_guard;
        *seq_guard = seq.wrapping_add(1);

        let entry = ClipboardEntry {
            content,
            sequence: seq,
            source_guest: guest_id,
        };

        let mut hist = self.history.lock().map_err(|_| "Lock poisoned")?;
        hist.push_back(entry);
        if hist.len() > self.max_history {
            hist.pop_front();
        }

        Ok(seq)
    }

    pub fn read(&self, guest_id: u32) -> Result<Option<ClipboardContent>, &'static str> {
        let policies = self.policies.lock().map_err(|_| "Lock poisoned")?;
        let default_policy = ClipboardPolicy::default();
        let policy = policies.get(&guest_id).unwrap_or(&default_policy);

        if !policy.read_allowed {
            return Err("Read not allowed");
        }
        drop(policies);

        let hist = self.history.lock().map_err(|_| "Lock poisoned")?;
        Ok(hist.back().map(|e| e.content.clone()))
    }

    pub fn history_len(&self) -> Result<usize, &'static str> {
        self.history.lock().map(|h| h.len()).map_err(|_| "Lock poisoned")
    }

    pub fn get_sequence(&self) -> Result<u64, &'static str> {
        self.sequence.lock().map(|s| *s).map_err(|_| "Lock poisoned")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_clipboard_content_size() {
        let text = ClipboardContent::Text("hello".to_string());
        assert_eq!(text.size(), 5);

        let image = ClipboardContent::Image(vec![0u8; 100]);
        assert_eq!(image.size(), 100);
    }

    #[test]
    fn test_clipboard_content_type() {
        assert_eq!(ClipboardContent::Text("x".into()).content_type(), "text");
        assert_eq!(ClipboardContent::Image(vec![]).content_type(), "image");
        assert_eq!(ClipboardContent::Html("<p>".into()).content_type(), "html");
    }

    #[test]
    fn test_policy_default() {
        let policy = ClipboardPolicy::default();
        assert!(policy.read_allowed);
        assert!(policy.write_allowed);
        assert!(policy.allowed_types.contains(&"text".to_string()));
    }

    #[test]
    fn test_policy_allows() {
        let policy = ClipboardPolicy::new(vec!["text"], 50);
        assert!(policy.allows(&ClipboardContent::Text("hi".into())));
        assert!(!policy.allows(&ClipboardContent::Image(vec![0u8; 100])));
    }

    #[test]
    fn test_hub_write_and_read() {
        let hub = ClipboardHub::new(10);
        let content = ClipboardContent::Text("test".to_string());
        
        let seq = hub.write(1, content.clone()).expect("write failed");
        assert_eq!(seq, 0);
        
        let read_content = hub.read(1).expect("read failed").expect("no content");
        assert_eq!(read_content, content);
    }

    #[test]
    fn test_hub_policy_enforcement() {
        let hub = ClipboardHub::new(10);
        let restricted = ClipboardPolicy::new(vec!["text"], 10);
        hub.set_policy(2, restricted);

        let large_text = ClipboardContent::Text("x".repeat(100));
        let result = hub.write(2, large_text);
        assert!(result.is_err());
    }

    #[test]
    fn test_hub_history_ring_buffer() {
        let hub = ClipboardHub::new(3);
        
        hub.write(1, ClipboardContent::Text("a".into())).unwrap();
        hub.write(1, ClipboardContent::Text("b".into())).unwrap();
        hub.write(1, ClipboardContent::Text("c".into())).unwrap();
        assert_eq!(hub.history_len().unwrap(), 3);
        
        hub.write(1, ClipboardContent::Text("d".into())).unwrap();
        assert_eq!(hub.history_len().unwrap(), 3);
    }
}

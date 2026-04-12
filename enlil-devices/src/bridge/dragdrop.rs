use std::collections::HashMap;

/// Represents a drag-and-drop payload with file metadata.
#[derive(Debug, Clone)]
pub struct DragPayload {
    pub file_uris: Vec<String>,
    pub mime_types: Vec<String>,
    pub preview_thumbnail: Option<Vec<u8>>,
}

impl DragPayload {
    #[must_use]
    pub const fn new(file_uris: Vec<String>, mime_types: Vec<String>) -> Self {
        Self {
            file_uris,
            mime_types,
            preview_thumbnail: None,
        }
    }

    #[must_use]
    pub fn with_thumbnail(mut self, thumbnail: Vec<u8>) -> Self {
        self.preview_thumbnail = Some(thumbnail);
        self
    }
}

/// Tracks the drag-and-drop state machine.
#[derive(Debug, Clone)]
pub enum DragState {
    Idle,
    Dragging {
        source_guest: String,
        payload: DragPayload,
    },
    Hovering {
        source_guest: String,
        target_guest: String,
        payload: DragPayload,
    },
    Dropped,
}

impl DragState {
    #[must_use]
    pub const fn is_idle(&self) -> bool {
        matches!(self, Self::Idle)
    }

    #[must_use]
    pub const fn is_dragging(&self) -> bool {
        matches!(self, Self::Dragging { .. })
    }

    #[must_use]
    pub const fn is_hovering(&self) -> bool {
        matches!(self, Self::Hovering { .. })
    }

    #[must_use]
    pub const fn is_dropped(&self) -> bool {
        matches!(self, Self::Dropped)
    }
}

/// Manages drag-and-drop operations between guests.
pub struct DragDropManager {
    state: DragState,
    guest_capabilities: HashMap<String, Vec<String>>,
}

impl DragDropManager {
    #[must_use]
    pub fn new() -> Self {
        Self {
            state: DragState::Idle,
            guest_capabilities: HashMap::new(),
        }
    }

    /// Register a guest with supported MIME types.
    pub fn register_guest(&mut self, guest_id: String, mime_types: Vec<String>) {
        self.guest_capabilities.insert(guest_id, mime_types);
    }

    /// Start drag from a guest.
    ///
    /// # Errors
    ///
    /// Returns an error if the state is not Idle or if the source guest is not registered.
    pub fn start_drag(&mut self, source_guest: String, payload: DragPayload) -> Result<(), String> {
        if !matches!(self.state, DragState::Idle) {
            return Err("Cannot start drag: state is not Idle".to_string());
        }

        if !self.guest_capabilities.contains_key(&source_guest) {
            return Err(format!("Guest '{source_guest}' not registered"));
        }

        self.state = DragState::Dragging {
            source_guest,
            payload,
        };
        Ok(())
    }

    /// Move hover to target guest.
    ///
    /// # Errors
    ///
    /// Returns an error if the state is not Dragging, if the target guest is not registered,
    /// if the target is the same as the source, or if the target does not support the payload MIME types.
    pub fn hover(&mut self, target_guest: String) -> Result<(), String> {
        match &self.state {
            DragState::Dragging {
                source_guest,
                payload,
            } => {
                if !self.guest_capabilities.contains_key(&target_guest) {
                    return Err(format!("Guest '{target_guest}' not registered"));
                }

                if source_guest == &target_guest {
                    return Err("Cannot hover over source guest".to_string());
                }

                let target_types = &self.guest_capabilities[&target_guest];
                if !payload
                    .mime_types
                    .iter()
                    .any(|mt| target_types.contains(mt))
                {
                    return Err("Target guest does not support payload MIME types".to_string());
                }

                self.state = DragState::Hovering {
                    source_guest: source_guest.clone(),
                    target_guest,
                    payload: payload.clone(),
                };
                Ok(())
            }
            _ => Err("Cannot hover: state is not Dragging".to_string()),
        }
    }

    /// Complete the drag-and-drop operation.
    ///
    /// # Errors
    ///
    /// Returns an error if the state is not Hovering.
    pub fn drop(&mut self) -> Result<(String, String, DragPayload), String> {
        match &self.state {
            DragState::Hovering {
                source_guest,
                target_guest,
                payload,
            } => {
                let result = (
                    source_guest.clone(),
                    target_guest.clone(),
                    payload.clone(),
                );
                self.state = DragState::Dropped;
                Ok(result)
            }
            _ => Err("Cannot drop: state is not Hovering".to_string()),
        }
    }

    /// Reset to idle state.
    ///
    /// # Errors
    ///
    /// This function does not currently return errors.
    pub fn cancel(&mut self) -> Result<(), String> {
        self.state = DragState::Idle;
        Ok(())
    }

    #[must_use]
    pub const fn state(&self) -> &DragState {
        &self.state
    }
}

impl Default for DragDropManager {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_drag_payload_creation() {
        let payload = DragPayload::new(
            vec!["file:///a.txt".to_string()],
            vec!["text/plain".to_string()],
        );
        assert_eq!(payload.file_uris.len(), 1);
        assert_eq!(payload.mime_types.len(), 1);
        assert!(payload.preview_thumbnail.is_none());
    }

    #[test]
    fn test_payload_with_thumbnail() {
        let thumbnail = vec![1, 2, 3, 4];
        let payload = DragPayload::new(vec![], vec![])
            .with_thumbnail(thumbnail.clone());
        assert_eq!(payload.preview_thumbnail, Some(thumbnail));
    }

    #[test]
    fn test_drag_state_transitions() {
        let mut mgr = DragDropManager::new();
        mgr.register_guest("guest1".to_string(), vec!["text/plain".to_string()]);
        mgr.register_guest("guest2".to_string(), vec!["text/plain".to_string()]);

        let payload = DragPayload::new(
            vec!["file:///test.txt".to_string()],
            vec!["text/plain".to_string()],
        );

        assert!(mgr.state().is_idle());
        mgr.start_drag("guest1".to_string(), payload).unwrap();
        assert!(mgr.state().is_dragging());
        mgr.hover("guest2".to_string()).unwrap();
        assert!(mgr.state().is_hovering());
        let result = mgr.drop().unwrap();
        assert_eq!(result.0, "guest1");
        assert_eq!(result.1, "guest2");
    }

    #[test]
    fn test_hover_validation_mime_type() {
        let mut mgr = DragDropManager::new();
        mgr.register_guest("guest1".to_string(), vec!["text/plain".to_string()]);
        mgr.register_guest("guest2".to_string(), vec!["image/png".to_string()]);

        let payload = DragPayload::new(vec![], vec!["text/plain".to_string()]);
        mgr.start_drag("guest1".to_string(), payload).unwrap();

        let result = mgr.hover("guest2".to_string());
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("MIME types"));
    }

    #[test]
    fn test_invalid_state_transitions() {
        let mut mgr = DragDropManager::new();
        mgr.register_guest("guest1".to_string(), vec!["text/plain".to_string()]);

        let _payload = DragPayload::new(vec![], vec!["text/plain".to_string()]);

        let result = mgr.hover("guest1".to_string());
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("not Dragging"));

        let result = mgr.drop();
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("not Hovering"));
    }
}

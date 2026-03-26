use std::collections::{HashMap, HashSet};
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Urgency {
    Low = 0,
    Normal = 1,
    Critical = 2,
}

#[derive(Debug, Clone)]
pub struct Notification {
    pub id: u64,
    pub source_guest: String,
    pub title: String,
    pub body: String,
    pub urgency: Urgency,
    pub app_name: String,
    pub timestamp: u64,
}

impl Notification {
    /// Creates a new notification with the current system time.
    ///
    /// # Panics
    ///
    /// Panics if the system time is before the UNIX epoch.
    pub fn new(
        id: u64,
        source_guest: String,
        title: String,
        body: String,
        urgency: Urgency,
        app_name: String,
    ) -> Self {
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        Self {
            id,
            source_guest,
            title,
            body,
            urgency,
            app_name,
            timestamp,
        }
    }
}

#[derive(Debug, Clone)]
pub struct NotificationPolicy {
    pub urgency_threshold: Urgency,
    pub allowed_apps: HashSet<String>,
    pub blocked_apps: HashSet<String>,
    pub max_per_minute: u32,
}

impl NotificationPolicy {
    pub fn new(urgency_threshold: Urgency, max_per_minute: u32) -> Self {
        Self {
            urgency_threshold,
            allowed_apps: HashSet::new(),
            blocked_apps: HashSet::new(),
            max_per_minute,
        }
    }

    pub fn is_allowed(&self, notif: &Notification) -> bool {
        if notif.urgency < self.urgency_threshold {
            return false;
        }
        if self.blocked_apps.contains(&notif.app_name) {
            return false;
        }
        if !self.allowed_apps.is_empty() && !self.allowed_apps.contains(&notif.app_name) {
            return false;
        }
        true
    }
}

pub struct NotificationRouter {
    policies: HashMap<String, NotificationPolicy>,
    notification_history: HashMap<String, Vec<u64>>,
}

impl Default for NotificationRouter {
    fn default() -> Self {
        Self::new()
    }
}

impl NotificationRouter {
    pub fn new() -> Self {
        Self {
            policies: HashMap::new(),
            notification_history: HashMap::new(),
        }
    }

    pub fn set_policy(&mut self, guest: String, policy: NotificationPolicy) {
        self.policies.insert(guest.clone(), policy);
        self.notification_history.entry(guest).or_default();
    }

    /// Routes a notification to eligible guests.
    ///
    /// # Panics
    ///
    /// Panics if the system time is before the UNIX epoch.
    pub fn route(&mut self, notif: &Notification) -> Vec<String> {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let mut allowed_guests = Vec::new();

        for (guest, policy) in &self.policies {
            if guest == &notif.source_guest {
                continue;
            }
            if !policy.is_allowed(notif) {
                continue;
            }

            let history = self.notification_history.get_mut(guest).unwrap();
            let recent = history.iter().filter(|&&ts| now - ts < 60).count();
            if u32::try_from(recent).unwrap_or(u32::MAX) >= policy.max_per_minute {
                continue;
            }

            allowed_guests.push(guest.clone());
        }

        for guest in &allowed_guests {
            self.notification_history.get_mut(guest).unwrap().push(now);
        }
        allowed_guests
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_notification_creation() {
        let notif = Notification::new(
            42,
            "guest1".to_string(),
            "Alert".to_string(),
            "Body text".to_string(),
            Urgency::Normal,
            "app1".to_string(),
        );
        assert_eq!(notif.id, 42);
        assert_eq!(notif.source_guest, "guest1");
        assert_eq!(notif.urgency, Urgency::Normal);
        assert!(notif.timestamp > 0);
    }

    #[test]
    fn test_urgency_ordering() {
        assert!(Urgency::Low < Urgency::Normal);
        assert!(Urgency::Normal < Urgency::Critical);
        assert!(Urgency::Critical > Urgency::Low);
    }

    #[test]
    fn test_policy_urgency_threshold() {
        let policy = NotificationPolicy::new(Urgency::Normal, 10);
        let low = Notification::new(1, "g1".to_string(), "t".to_string(), "b".to_string(), Urgency::Low, "app".to_string());
        let normal = Notification::new(2, "g1".to_string(), "t".to_string(), "b".to_string(), Urgency::Normal, "app".to_string());
        assert!(!policy.is_allowed(&low));
        assert!(policy.is_allowed(&normal));
    }

    #[test]
    fn test_policy_blocked_and_allowed_apps() {
        let mut policy = NotificationPolicy::new(Urgency::Low, 10);
        policy.blocked_apps.insert("blocked".to_string());
        policy.allowed_apps.insert("allowed".to_string());
        
        let blocked = Notification::new(1, "g".to_string(), "t".to_string(), "b".to_string(), Urgency::Normal, "blocked".to_string());
        let allowed = Notification::new(2, "g".to_string(), "t".to_string(), "b".to_string(), Urgency::Normal, "allowed".to_string());
        let other = Notification::new(3, "g".to_string(), "t".to_string(), "b".to_string(), Urgency::Normal, "other".to_string());
        
        assert!(!policy.is_allowed(&blocked));
        assert!(policy.is_allowed(&allowed));
        assert!(!policy.is_allowed(&other));
    }

    #[test]
    fn test_router_routes_by_policy() {
        let mut router = NotificationRouter::new();
        router.set_policy("guest1".to_string(), NotificationPolicy::new(Urgency::Low, 100));
        router.set_policy("guest2".to_string(), NotificationPolicy::new(Urgency::Critical, 100));
        
        let notif = Notification::new(1, "sender".to_string(), "t".to_string(), "b".to_string(), Urgency::Normal, "app".to_string());
        let recipients = router.route(&notif);
        
        assert_eq!(recipients.len(), 1);
        assert_eq!(recipients[0], "guest1");
    }

    #[test]
    fn test_router_respects_max_per_minute() {
        let mut router = NotificationRouter::new();
        let policy = NotificationPolicy::new(Urgency::Low, 1);
        router.set_policy("guest1".to_string(), policy);
        
        let notif1 = Notification::new(1, "sender".to_string(), "t".to_string(), "b".to_string(), Urgency::Normal, "app".to_string());
        let notif2 = Notification::new(2, "sender".to_string(), "t".to_string(), "b".to_string(), Urgency::Normal, "app".to_string());
        
        let r1 = router.route(&notif1);
        assert_eq!(r1.len(), 1);
        
        let r2 = router.route(&notif2);
        assert_eq!(r2.len(), 0);
    }
}

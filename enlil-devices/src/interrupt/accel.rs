//! Hardware interrupt-acceleration policy (Intel `APICv` / AMD `AVIC`).
//!
//! [`InterruptAccel`] is the per-VM policy layer over the HAL's posted-
//! interrupt primitives ([`enlil_hal::apicv`] / [`enlil_hal::avic`]). Device
//! backends (virtio, MSI routing, timers) post interrupts through one call —
//! [`post`](InterruptAccel::post) — and the returned [`AccelPostAction`] tells
//! the backend exactly what (if anything) it must do next:
//!
//! ```text
//!  Software  -> deliver through the emulated LocalApic (today's path)
//!  Apicv     -> set PIR bit; send notification IPI iff AccelPostAction::Notify
//!  Avic      -> set IRR bit; write the doorbell MSR iff AccelPostAction::Doorbell
//! ```
//!
//! The mode is chosen once per VM from host capabilities
//! ([`AccelMode::select`]); the bare-metal backend arms the matching HAL
//! state at vCPU creation and refreshes placement on every schedule/migrate
//! via [`place_vcpu`](InterruptAccel::place_vcpu) /
//! [`set_running`](InterruptAccel::set_running). The existing software LAPIC
//! path is untouched — `AccelMode::Software` (and any out-of-range vCPU)
//! falls back to it.

use enlil_hal::apicv::{ApicvVcpu, PostAction as ApicvPostAction};
use enlil_hal::avic::{AvicPostAction as HalAvicPostAction, AvicTables, AvicVcpu};

/// Which interrupt virtualization the VM uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AccelMode {
    /// Pure software LAPIC emulation (today's path, always available).
    Software,
    /// Intel `APICv` + posted interrupts.
    Apicv,
    /// AMD `AVIC`.
    Avic,
}

impl AccelMode {
    /// Pick the best mode the host offers. `APICv` wins ties (a host is never
    /// both Intel and AMD; the tie-break is only for synthetic test caps).
    #[must_use]
    pub const fn select(have_apicv: bool, have_avic: bool) -> Self {
        if have_apicv {
            Self::Apicv
        } else if have_avic {
            Self::Avic
        } else {
            Self::Software
        }
    }
}

/// What the backend must do after [`InterruptAccel::post`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AccelPostAction {
    /// No hardware assist: deliver through the software LAPIC path.
    Software,
    /// Intel: send an IPI with `vector` to physical APIC ID `dest_apic_id`.
    Notify { dest_apic_id: u32, vector: u8 },
    /// AMD: write `guest_apic_id` to the `AVIC` doorbell MSR
    /// (`MSR_AMD64_AVIC_DOORBELL`).
    Doorbell { guest_apic_id: u8 },
    /// Recorded in the posted state; the target is not running (or a
    /// notification is already in flight) — nothing more to do.
    Pending,
}

/// Per-VM interrupt-acceleration state.
///
/// Owns the per-vCPU posted-interrupt state for the active [`AccelMode`] and
/// the `AVIC` tables shared by the VM. The backend keeps placement fresh:
/// [`place_vcpu`](Self::place_vcpu) when a vCPU is (re)assigned to a physical
/// CPU, [`set_running`](Self::set_running) around guest entry/exit.
pub struct InterruptAccel {
    mode: AccelMode,
    notification_vector: u8,
    /// Scheduler view of which vCPUs are currently executing in the guest.
    running: Vec<bool>,
    /// vCPU index → guest APIC ID (identity by default).
    guest_ids: Vec<u8>,
    apicv: Vec<ApicvVcpu>,
    avic: Vec<AvicVcpu>,
    avic_tables: AvicTables,
}

impl InterruptAccel {
    /// Build per-VM state for `mode` with `num_vcpus` vCPUs.
    ///
    /// `notification_vector` is the Intel posted-interrupt notification
    /// vector (ignored in other modes). Guest APIC IDs default to the vCPU
    /// index; remap with [`place_vcpu`](Self::place_vcpu).
    #[must_use]
    pub fn new(mode: AccelMode, num_vcpus: u8, notification_vector: u8) -> Self {
        let n = usize::from(num_vcpus);
        let apicv = (0..num_vcpus)
            .map(|_| ApicvVcpu::new(notification_vector, 0))
            .collect();
        let avic = (0..num_vcpus).map(AvicVcpu::new).collect();
        Self {
            mode,
            notification_vector,
            running: vec![false; n],
            guest_ids: (0..num_vcpus).collect(),
            apicv,
            avic,
            avic_tables: AvicTables::new(),
        }
    }

    /// The active acceleration mode.
    #[must_use]
    pub const fn mode(&self) -> AccelMode {
        self.mode
    }

    /// Number of vCPUs this state was built for.
    #[must_use]
    pub fn num_vcpus(&self) -> u8 {
        u8::try_from(self.running.len()).unwrap_or(u8::MAX)
    }

    /// Bind vCPU `vcpu` to `guest_apic_id` on host CPU `host_apic_id`.
    ///
    /// Intel: refreshes the descriptor's notification destination (NDST) and
    /// the virtual-APIC page's APIC ID. AMD: (re)binds the physical APIC ID
    /// table entry, preserving the running bit. Host APIC IDs are 8-bit
    /// (xAPIC); wider x2APIC IDs need x2`AVIC` / x2APIC virtualization, a
    /// future extension.
    pub fn place_vcpu(&mut self, vcpu: u8, guest_apic_id: u8, host_apic_id: u8) {
        let idx = usize::from(vcpu);
        if idx >= self.running.len() {
            return;
        }
        self.guest_ids[idx] = guest_apic_id;
        match self.mode {
            AccelMode::Software => {}
            AccelMode::Apicv => {
                if let Some(st) = self.apicv.get_mut(idx) {
                    st.set_notification_destination(u32::from(host_apic_id));
                    enlil_hal::apicv::vapic_write_apic_id(
                        st.virtual_apic_page_mut(),
                        guest_apic_id,
                    );
                }
            }
            AccelMode::Avic => {
                if let Some(st) = self.avic.get_mut(idx) {
                    st.set_guest_apic_id(guest_apic_id);
                }
                self.avic_tables.assign_vcpu(guest_apic_id, host_apic_id);
            }
        }
    }

    /// Record whether vCPU `vcpu` is currently executing in the guest.
    ///
    /// The backend calls this around guest entry/exit; it drives the `AVIC`
    /// physical table's `IsRunning` bit (doorbell decisions) and gates Intel
    /// notification IPIs (a descheduled vCPU's PIR waits for its next entry).
    pub fn set_running(&mut self, vcpu: u8, running: bool) {
        let idx = usize::from(vcpu);
        if idx >= self.running.len() {
            return;
        }
        self.running[idx] = running;
        if self.mode == AccelMode::Avic
            && let Some(&guest_id) = self.guest_ids.get(idx)
        {
            self.avic_tables.set_running(guest_id, running);
        }
    }

    /// Post `vector` to vCPU `vcpu`, returning what the backend must do next.
    ///
    /// Out-of-range vCPU indices fall back to [`AccelPostAction::Software`]
    /// rather than panicking.
    pub fn post(&mut self, vcpu: u8, vector: u8) -> AccelPostAction {
        let idx = usize::from(vcpu);
        match self.mode {
            AccelMode::Software => AccelPostAction::Software,
            AccelMode::Apicv => {
                let running = self.running.get(idx).copied().unwrap_or(false);
                let Some(st) = self.apicv.get_mut(idx) else {
                    return AccelPostAction::Software;
                };
                if !running {
                    // Descheduled: record the request; the processor picks
                    // the PIR bit up at the next VM entry. (Do not set ON —
                    // no notification was sent, so none is outstanding.)
                    st.descriptor_mut().set_vector(vector);
                    return AccelPostAction::Pending;
                }
                match st.post(vector) {
                    ApicvPostAction::Notify => AccelPostAction::Notify {
                        dest_apic_id: st.descriptor().notification_destination(),
                        vector: self.notification_vector,
                    },
                    ApicvPostAction::AlreadyNotified => AccelPostAction::Pending,
                }
            }
            AccelMode::Avic => {
                let Some(&guest_id) = self.guest_ids.get(idx) else {
                    return AccelPostAction::Software;
                };
                let Some(st) = self.avic.get_mut(idx) else {
                    return AccelPostAction::Software;
                };
                // Re-derive IsRunning from the scheduler view in case the
                // backend drives running state only through set_running.
                let running = self.running.get(idx).copied().unwrap_or(false);
                self.avic_tables.set_running(guest_id, running);
                match st.post(&self.avic_tables, vector) {
                    HalAvicPostAction::Doorbell { guest_apic_id } => {
                        AccelPostAction::Doorbell { guest_apic_id }
                    }
                    HalAvicPostAction::Pending => AccelPostAction::Pending,
                }
            }
        }
    }

    /// The VM's `AVIC` tables (for the backend to map and point the VMCBs at).
    #[must_use]
    pub const fn avic_tables(&self) -> &AvicTables {
        &self.avic_tables
    }

    /// Mutable access to the VM's `AVIC` tables.
    pub const fn avic_tables_mut(&mut self) -> &mut AvicTables {
        &mut self.avic_tables
    }

    /// Per-vCPU Intel `APICv` state (for the backend's VMCS programming).
    #[must_use]
    pub fn apicv_vcpu(&self, vcpu: u8) -> Option<&ApicvVcpu> {
        self.apicv.get(usize::from(vcpu))
    }

    /// Per-vCPU AMD `AVIC` state.
    #[must_use]
    pub fn avic_vcpu(&self, vcpu: u8) -> Option<&AvicVcpu> {
        self.avic.get(usize::from(vcpu))
    }
}

impl core::fmt::Debug for InterruptAccel {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("InterruptAccel")
            .field("mode", &self.mode)
            .field("notification_vector", &self.notification_vector)
            .field("running", &self.running)
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn select_picks_best_available() {
        assert_eq!(AccelMode::select(true, true), AccelMode::Apicv);
        assert_eq!(AccelMode::select(true, false), AccelMode::Apicv);
        assert_eq!(AccelMode::select(false, true), AccelMode::Avic);
        assert_eq!(AccelMode::select(false, false), AccelMode::Software);
    }

    #[test]
    fn software_mode_always_falls_through() {
        let mut accel = InterruptAccel::new(AccelMode::Software, 2, 0xE0);
        assert_eq!(accel.mode(), AccelMode::Software);
        assert_eq!(accel.num_vcpus(), 2);
        assert_eq!(accel.post(0, 0x30), AccelPostAction::Software);
        assert_eq!(accel.post(1, 0x31), AccelPostAction::Software);
    }

    #[test]
    fn apicv_post_notifies_running_vcpu_once() {
        let mut accel = InterruptAccel::new(AccelMode::Apicv, 2, 0xE0);
        accel.place_vcpu(0, 0, 5);
        accel.set_running(0, true);

        assert_eq!(
            accel.post(0, 0x30),
            AccelPostAction::Notify {
                dest_apic_id: 5,
                vector: 0xE0
            }
        );
        // ON now set: second post needs no new notification.
        assert_eq!(accel.post(0, 0x31), AccelPostAction::Pending);

        // Migration refreshes the notification destination.
        accel.place_vcpu(0, 0, 9);
        let st = accel.apicv_vcpu(0).unwrap();
        assert_eq!(st.descriptor().notification_destination(), 9);
        // Virtual-APIC page carries the guest APIC ID.
        assert_eq!(
            enlil_hal::apicv::vapic_read_apic_id(st.virtual_apic_page()),
            0
        );
    }

    #[test]
    fn apicv_post_to_descheduled_vcpu_pends_without_notify() {
        let mut accel = InterruptAccel::new(AccelMode::Apicv, 1, 0xE0);
        accel.place_vcpu(0, 0, 5);
        // Not running: recorded, no IPI, ON left clear.
        assert_eq!(accel.post(0, 0x30), AccelPostAction::Pending);
        let (posted, on) = {
            let st = accel.apicv_vcpu(0).unwrap();
            (
                st.descriptor().test_vector(0x30),
                st.descriptor().outstanding_notification(),
            )
        };
        assert!(posted);
        assert!(!on);

        // Scheduled later: the pending bit is still there and the next post
        // notifies normally.
        accel.set_running(0, true);
        assert_eq!(
            accel.post(0, 0x31),
            AccelPostAction::Notify {
                dest_apic_id: 5,
                vector: 0xE0
            }
        );
        assert!(accel.apicv_vcpu(0).unwrap().descriptor().test_vector(0x30));
    }

    #[test]
    fn avic_post_doorbells_running_vcpu() {
        let mut accel = InterruptAccel::new(AccelMode::Avic, 2, 0xE0);
        accel.place_vcpu(1, 4, 7);

        // Not running: IRR set, no doorbell.
        assert_eq!(accel.post(1, 0x40), AccelPostAction::Pending);
        assert!(accel.avic_vcpu(1).unwrap().backing().irr_test(0x40));

        // Running: doorbell for the guest APIC ID.
        accel.set_running(1, true);
        assert_eq!(
            accel.post(1, 0x41),
            AccelPostAction::Doorbell { guest_apic_id: 4 }
        );

        // Descheduled again: back to pending.
        accel.set_running(1, false);
        assert_eq!(accel.post(1, 0x42), AccelPostAction::Pending);
    }

    #[test]
    fn out_of_range_vcpu_falls_back_to_software() {
        let mut accel = InterruptAccel::new(AccelMode::Apicv, 1, 0xE0);
        assert_eq!(accel.post(9, 0x30), AccelPostAction::Software);
        accel.place_vcpu(9, 0, 0); // no panic
        accel.set_running(9, true); // no panic
        assert!(accel.apicv_vcpu(9).is_none());
        assert!(accel.avic_vcpu(9).is_none());
        assert_eq!(accel.num_vcpus(), 1);
    }

    #[test]
    fn accel_tables_start_empty() {
        let accel = InterruptAccel::new(AccelMode::Avic, 2, 0xE0);
        assert!(!accel.avic_tables().physical_entry(0).valid());
    }
}

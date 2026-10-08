//! Network device backends.
//!
//! Provides VirtIO-net emulation, a virtual switch for inter-guest networking,
//! pluggable backends (TAP on Linux, null for testing), and SR-IOV NIC Virtual
//! Function passthrough for high-performance guest networking when the host
//! has an SR-IOV-capable NIC, an IOMMU, and VFIO.
//!
//! # Architecture
//!
//! ```text
//! ┌─────────┐   ┌─────────┐
//! │ Guest A │   │ Guest B │
//! │ virtio  │   │ virtio  │
//! └────┬────┘   └────┬────┘
//!      │             │
//!      ▼             ▼
//! ┌──────────────────────┐
//! │    VirtualSwitch     │
//! │  (MAC learning, fwd) │
//! └──────────┬───────────┘
//!            │
//!            ▼
//!     ┌─────────────┐
//!     │ TAP Backend  │  (or NullBackend)
//!     └─────────────┘
//! ```

mod backend;
mod config;
mod control;
mod device;
mod features;
mod header;
mod switch;
mod virtqueue;

#[cfg(target_os = "linux")]
mod sriov;

#[cfg(target_os = "linux")]
mod vfio_pci;

#[cfg(target_os = "linux")]
mod tap;

pub use backend::{LoopbackBackend, NetBackend, NullBackend, PipeBackend};
pub use config::NetDeviceConfig;
pub use control::{NetControlState, RxFilterMode, VIRTIO_NET_ERR, VIRTIO_NET_OK};
pub use device::{DeviceStatus, VirtioNetDevice};
pub use features::NetFeatures;
pub use header::VirtioNetHeader;
pub use switch::{PortId, VirtualSwitch};
pub use virtqueue::{Virtqueue, VirtqueueError};

#[cfg(target_os = "linux")]
pub use sriov::{
    HostPaths, SriovGating, SriovNicPf, SriovVf, SriovVfAssignment, VfAssignmentTable,
    assign_vf_to_guest, disable_vfs, discover_sriov_nic_pfs, enable_vfs, enable_vfs_with_retry,
    first_free_vf, fmt_mac, vfs_of,
};

#[cfg(target_os = "linux")]
pub use vfio_pci::{
    DmaMapping, MmapRegion, PciRegion, VFIO_API_VERSION, VFIO_DMA_MAP_FLAG_READ,
    VFIO_DMA_MAP_FLAG_WRITE, VFIO_GROUP_FLAGS_VIABLE, VFIO_PCI_BAR0_REGION_INDEX,
    VFIO_PCI_BAR5_REGION_INDEX, VFIO_PCI_CONFIG_REGION_INDEX, VFIO_PCI_NUM_REGIONS,
    VFIO_TYPE1_IOMMU, VfioContainer, VfioGroup, VfioPciDevice, bind_driver, bound_driver,
    clear_driver_override, device_mac, iommu_group_of, pci_attr_dec, pci_attr_hex, pci_config,
    validate_bdf,
};

#[cfg(target_os = "linux")]
pub use tap::TapBackend;

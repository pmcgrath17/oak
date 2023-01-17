//
// Copyright 2022 The Project Oak Authors
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.
//

//! Main 'kernel' for baremetal Oak Functions.
//!
//! This code takes care of initializing the x86-64 machine properly and
//! handing the reins over to the oak_baremetal_runtime. It is in a separate crate so that we
//! could support different boot protocols (eg Linux boot protocol or PVH) that
//! require different targets, linker scripts and/or provide machine
//! information in different data structures.
//!
//! Bootloaders (and VMMs under them) have to adhere to the following protocol:
//!   * Enter 64-bit long mode, and set up basic paging -- enough to load the code, as we will set
//!     up a full page table in `start_kernel`.
//!   * Implement a `#[panic_handler]` that delegates to `panic()` in this crate.
//!   * Call `start_kernel` from the entry point of the bootloader.

#![cfg_attr(not(test), no_std)]
#![feature(abi_x86_interrupt)]
#![feature(allocator_api)]
#![feature(asm_sym)]
#![feature(naked_functions)]
#![feature(once_cell)]
#![feature(c_size_t)]

mod acpi;
mod args;
pub mod attestation;
mod avx;
mod boot;
mod descriptors;
mod elf;
mod ghcb;
mod interrupts;
mod libm;
mod logging;
mod memory;
mod mm;
mod payload;
#[cfg(feature = "serial_channel")]
mod serial;
pub mod shutdown;
#[cfg(feature = "simple_io_channel")]
mod simpleio;
mod snp;
mod syscall;
#[cfg(feature = "vsock_channel")]
mod virtio;
#[cfg(feature = "virtio_console_channel")]
mod virtio_console;

extern crate alloc;

use crate::{
    acpi::Acpi,
    mm::Translator,
    snp::{get_snp_page_addresses, init_snp_pages},
};
use alloc::{alloc::Allocator, boxed::Box};
use core::{
    marker::Sync,
    ops::{Deref, DerefMut},
    panic::PanicInfo,
    str::FromStr,
};
use linked_list_allocator::LockedHeap;
use log::{error, info};
use mm::{
    encrypted_mapper::{EncryptedPageTable, PhysOffset},
    frame_allocator::PhysicalMemoryAllocator,
    virtual_address_allocator::VirtualAddressAllocator,
};
use oak_channel::Channel;
use oak_core::sync::OnceCell;
use oak_linux_boot_params::BootParams;
use oak_sev_guest::msr::{change_snp_state_for_frame, get_sev_status, PageAssignment, SevStatus};
use spinning_top::Spinlock;
use strum::{EnumIter, EnumString, IntoEnumIterator};
use x86_64::{
    structures::paging::{MappedPageTable, Page, Size2MiB},
    PhysAddr, VirtAddr,
};

pub use payload::run_payload;

/// Allocator for physical memory frames in the system.
/// We reserve enough room to handle up to 128 GiB of memory, for now.
pub static FRAME_ALLOCATOR: OnceCell<Spinlock<PhysicalMemoryAllocator<1024>>> = OnceCell::new();

/// The allocator for allocating space in the memory area that is shared with the hypervisor.
pub static GUEST_HOST_HEAP: OnceCell<LockedHeap> = OnceCell::new();

/// Active page tables.
pub static PAGE_TABLES: OnceCell<
    Spinlock<EncryptedPageTable<MappedPageTable<'static, PhysOffset>>>,
> = OnceCell::new();

/// Allocator for long-lived pages in the kernel.
pub static VMA_ALLOCATOR: Spinlock<VirtualAddressAllocator<Size2MiB>> =
    Spinlock::new(VirtualAddressAllocator::new(Page::range(
        // Assign 32 TB of virtual memory for this allocator.
        // Safety: these addresses are constants and thus we know they're page-aligned.
        unsafe {
            Page::from_start_address_unchecked(VirtAddr::new_truncate(0xFFFF_C900_0000_0000))
        },
        unsafe {
            Page::from_start_address_unchecked(VirtAddr::new_truncate(0xFFFF_E900_0000_0000))
        },
    )));

/// Main entry point for the kernel, to be called from bootloader.
pub fn start_kernel(info: &BootParams) {
    avx::enable_avx();
    descriptors::init_gdt();
    interrupts::init_idt();
    let sev_status = get_sev_status().unwrap_or(SevStatus::empty());
    let sev_es_enabled = sev_status.contains(SevStatus::SEV_ES_ENABLED);
    let sev_snp_enabled = sev_status.contains(SevStatus::SNP_ACTIVE);
    if sev_es_enabled {
        ghcb::init(sev_snp_enabled);
    }
    logging::init_logging(sev_es_enabled);

    // We need to be done with the boot info struct before intializing memory. For example, the
    // multiboot protocol explicitly states data can be placed anywhere in memory; therefore, it's
    // highly likely we will overwrite some data after we initialize the heap. args::init_args()
    // caches the arguments (as long they are of reasonable length) in a static variable, allowing
    // us to refer to the args in the future.
    let kernel_args = args::init_args(info.args()).unwrap();
    info!("Kernel boot args: {}", kernel_args.args());

    let protocol = info.protocol();
    info!("Boot protocol:  {}", protocol);
    let snp_pages = if sev_snp_enabled {
        // We have to get the physical addresses of the CPUID pages now while the identity mapping
        // is still in place, but we can only initialize the instances after the new page
        // mappings have been set up.
        Some(get_snp_page_addresses(info))
    } else {
        None
    };

    // Safety: in the linker script we specify that the ELF header should be placed at 0x200000.
    let program_headers = unsafe { elf::get_phdrs(VirtAddr::new(0x20_0000)) };

    // Physical frame allocator
    FRAME_ALLOCATOR
        .set(Spinlock::new(mm::init(info.e820_table(), program_headers)))
        .map_err(|_| ())
        .expect("did not expect frame allocator to be already set!");

    // Note: `info` will not be valid after calling this!
    if PAGE_TABLES
        .set(Spinlock::new({
            let mut frame_allocator = FRAME_ALLOCATOR.get().unwrap().lock();
            mm::init_paging(frame_allocator.deref_mut(), program_headers).unwrap()
        }))
        .is_err()
    {
        panic!("couldn't initialize page tables");
    };

    // Re-map boot params to the new virtual address.
    // Safety: we know we're addressing valid memory that contains the correct data structure, as
    // we're just translating addresses differently due to the new page tables.
    let info = unsafe {
        (PAGE_TABLES
            .get()
            .unwrap()
            .lock()
            .translate_physical(PhysAddr::new(info as *const _ as u64))
            .unwrap()
            .as_ptr() as *const BootParams)
            .as_ref()
            .unwrap()
    };

    if sev_es_enabled {
        let mut mapper = PAGE_TABLES.get().unwrap().lock();
        // Now that the page tables have been updated, we have to re-share the GHCB with the
        // hypervisor.
        ghcb::reshare_ghcb(mapper.deref_mut());
        if sev_snp_enabled {
            // We must also initialise the CPUID and secrets pages and the guest message encryptor
            // when SEV-SNP is active. Panicking is OK at this point, because these pages are
            // required to support the full features and we don't want to run without them.
            init_snp_pages(
                snp_pages.expect("missing SNP CPUID and secrets pages"),
                mapper.deref(),
            );
            snp::init_guest_message_encryptor();
        }
    }

    // Allocate a section for guest-host communication (without the `ENCRYPTED` bit set)
    // We'll allocate 2*2MiB, as virtio needs more than 2 MiB for its data structures.
    let guest_host_frames = {
        let mut frame_allocator = FRAME_ALLOCATOR.get().unwrap().lock();
        frame_allocator.allocate_contiguous(2).unwrap()
    };

    let guest_host_pages = {
        let pt = PAGE_TABLES.get().unwrap().lock();
        Page::range(
            pt.translate_physical_frame(guest_host_frames.start)
                .unwrap(),
            pt.translate_physical_frame(guest_host_frames.end).unwrap(),
        )
    };

    // If we are running on SNP we have to mark the guest-host frames as shared in the RMP. It is OK
    // to crash if we cannot mark the pages as shared in the RMP.
    if sev_snp_enabled {
        // TODO(#3414): Use the GHCB protocol when it is available.
        for frame in guest_host_frames {
            change_snp_state_for_frame(&frame, PageAssignment::Shared)
                .expect("couldn't change SNP state for frame");
        }
    }

    // Safety: initializing the new heap is safe as the frame allocator guarantees we're not
    // overwriting any other memory; writing to the static mut is safe as we're in the
    // initialization code and thus there can be no concurrent access.
    if GUEST_HOST_HEAP
        .set(
            unsafe {
                memory::init_guest_host_heap(
                    guest_host_pages,
                    PAGE_TABLES.get().unwrap().lock().deref_mut(),
                )
            }
            .unwrap(),
        )
        .is_err()
    {
        panic!("couldn't initialize the guest-host heap");
    }

    // If we don't find memory for heap, it's ok to panic.
    // We'll let the heap to grow to 1 TB (1 << 19 * 2 MiB pages), max.
    let heap_page_range = VMA_ALLOCATOR.lock().allocate(1 << 19).unwrap();
    memory::init_kernel_heap(heap_page_range).unwrap();

    // Init ACPI, if available.
    let mut acpi = match acpi::Acpi::new(info) {
        Err(ref err) => {
            log::warn!("Failed to load ACPI tables: {}", err);
            None
        }
        Ok(mut acpi) => {
            acpi.print_devices().unwrap();
            Some(acpi)
        }
    };

    if sev_snp_enabled {
        // For now we just generate a sample attestation report and log the value.
        // TODO(#2842): Use attestation report in attestation behaviour.
        let report =
            attestation::get_attestation([42; 64]).expect("couldn't generate attestation report");
        info!("Attestation: {:?}", report);
        report.validate().expect("attestation report is invalid");
    }

    let channel = get_channel(
        &kernel_args,
        GUEST_HOST_HEAP.get().unwrap(),
        acpi.as_mut(),
        sev_status,
    );

    syscall::enable_syscalls(channel);
}

#[derive(EnumIter, EnumString)]
#[strum(ascii_case_insensitive, serialize_all = "snake_case")]
enum ChannelType {
    #[cfg(feature = "virtio_console_channel")]
    VirtioConsole,
    #[cfg(feature = "vsock_channel")]
    VirtioVsock,
    #[cfg(feature = "serial_channel")]
    Serial,
    #[cfg(feature = "simple_io_channel")]
    SimpleIo,
}

/// Create a channel for communicating with the Untrusted Launcher.
fn get_channel<'a, A: Allocator + Sync>(
    kernel_args: &args::Args,
    alloc: &'a A,
    acpi: Option<&mut Acpi>,
    sev_status: SevStatus,
) -> Box<dyn Channel + 'a> {
    // If we weren't told which channel to use, arbitrarily pick the first one in the `ChannelType`
    // enum. Depending on features that are enabled, this means that the enum acts as kind of a
    // reverse priority list for defaults.
    let chan_type = kernel_args
        .get("channel")
        .map(|chan_type| ChannelType::from_str(chan_type).unwrap())
        .unwrap_or_else(|| ChannelType::iter().next().unwrap());

    match chan_type {
        #[cfg(feature = "virtio_console_channel")]
        ChannelType::VirtioConsole => Box::new(virtio_console::get_console_channel(
            acpi.expect("ACPI not available; unable to use virtio console"),
        )),
        #[cfg(feature = "vsock_channel")]
        ChannelType::VirtioVsock => Box::new(virtio::get_vsock_channel(alloc)),
        #[cfg(feature = "serial_channel")]
        ChannelType::Serial => Box::new(serial::Serial::new()),
        #[cfg(feature = "simple_io_channel")]
        ChannelType::SimpleIo => Box::new(simpleio::SimpleIoChannel::new(alloc, sev_status)),
    }
}

/// Common panic routine for the kernel. This needs to be wrapped in a
/// panic_handler function in individual bootloader crates.
pub fn panic(info: &PanicInfo) -> ! {
    error!("PANIC: {}", info);
    shutdown::shutdown();
}

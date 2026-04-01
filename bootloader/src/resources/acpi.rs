use log::debug;
use uefi::table::cfg::ConfigTableEntry;

use crate::resources::UefiResource;

static mut ACPI: Option<Acpi> = None;

#[derive(Debug)]
pub struct Acpi {
    pub rsdp_address: u64,
    pub rsdp_version: u8,
}

impl UefiResource for Acpi {
    fn probe() -> Option<()> {
        // Get the RSDP address from UEFI configuration table
        let (acpi, acpi2) = uefi::system::with_config_table(|table| {
            let mut acpi = None;
            let mut acpi2 = None;
            for ent in table {
                match ent.guid {
                    ConfigTableEntry::ACPI_GUID => {
                        acpi = Some(ent.address);
                    }
                    ConfigTableEntry::ACPI2_GUID => {
                        acpi2 = Some(ent.address);
                    }
                    _ => {}
                }
            }
            (acpi, acpi2)
        });

        let rsdp_address = acpi2.or(acpi)? as u64;

        unsafe {
            ACPI = Some(Acpi {
                rsdp_address,
                rsdp_version: if acpi2.is_some() { 2 } else { 1 },
            });
        }

        debug!(
            "ACPI RSDP found at {:#x} (version {})",
            rsdp_address,
            if acpi2.is_some() { 2 } else { 1 }
        );

        Some(())
    }

    fn resource() -> Option<&'static mut Self> {
        unsafe {
            let acpi = &raw mut ACPI;
            (*acpi).as_mut()
        }
    }
}

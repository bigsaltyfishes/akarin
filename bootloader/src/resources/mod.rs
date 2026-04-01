pub mod acpi;
pub mod framebuffer;
pub mod gdt;

pub trait UefiResource {
    fn probe() -> Option<()>;
    fn resource() -> Option<&'static mut Self>;
}

pub fn probe_resources() {
    let _ = framebuffer::Framebuffer::probe();
    let _ = acpi::Acpi::probe();
    let _ = gdt::Gdt::probe();
}

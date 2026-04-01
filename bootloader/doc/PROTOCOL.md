# Hikari Boot Protocol
## Interface Version 1
### Description
- Executable Format: We only support Mach-O format of kernel image, with **PIC** relocation model.
- Interface Info: Kernel should link one static `Requirements` instance and only one instance to segment `__REQ`, section `__requirements`. Bootloader will search for this and vaildate the `Requirements`. Kernel can request some information, system resource, load base or reserve memory space via this `Requirements` instance. You can put this segment wherever you want.
- Boot Information: Bootloader will provide boot information that kernel requests and boot args via `BootInfo`. It's pointer will be passed through the first argument of the entry point.
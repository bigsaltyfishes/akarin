# =============================================================================
# Configuration
# =============================================================================

bootloader_target := "x86_64-unknown-uefi-debug"
bootloader_target_spec := "./x86_64-unknown-uefi-debug.json"
kernel_target := "x86_64-unknown-none-macho"
kernel_target_spec := "./x86_64-unknown-none-macho.json"
user_target := "x86_64-unknown-akarin"
user_target_spec := "./x86_64-unknown-akarin.json"

default: check

profile := env("PROFILE", "debug")
extra_flags := if profile == "release" { "--release" } else { "" }

check:
    cargo check -Zjson-target-spec -p akarin_kernel --target {{kernel_target_spec}}
    cargo check -Zjson-target-spec -p bootstrap --target {{user_target_spec}}
    cargo check -Zjson-target-spec -p akarin-bootloader --target {{bootloader_target_spec}}

check-json:
    cargo check -Zjson-target-spec -p akarin_kernel --target {{kernel_target_spec}} --message-format=json
    cargo check -Zjson-target-spec -p bootstrap --target {{user_target_spec}} --message-format=json
    cargo check -Zjson-target-spec -p akarin-bootloader --target {{bootloader_target_spec}} --message-format=json

build:
    @echo "🛠️ Building Bootloader..."
    cargo build -Zjson-target-spec -p akarin-bootloader --target {{bootloader_target_spec}} {{extra_flags}}
    @echo "🛠️ Building Kernel..."
    cargo build -Zjson-target-spec -p akarin_kernel --target {{kernel_target_spec}} {{extra_flags}}
    @echo "🛠️ Building Bootstrap..."
    cargo build -Zjson-target-spec -p bootstrap --target {{user_target_spec}} {{extra_flags}}

build-esp: build
    @echo "📦 Packaging ESP image..."
    mkdir -p target/esp/EFI/BOOT
    cp -r target/{{bootloader_target}}/{{profile}}/akarin-bootloader.efi target/esp/EFI/BOOT/bootx64.efi
    cp -r target/{{kernel_target}}/{{profile}}/akarin_kernel target/esp/kernel
    cp -r target/{{user_target}}/{{profile}}/bootstrap target/esp/bootstrap
    cp -r template/boot.cfg target/esp/boot.cfg
    @echo "✅ ESP image is ready in target/esp"

run: build-esp
    @echo "🚀 Running Akarin OS in QEMU..."
    qemu-system-x86_64 -accel kvm -cpu host -d int -bios template/OVMF.fd -M q35 -smp 4 -m 2G -drive file=fat:rw:target/esp,format=raw -net none -serial stdio -s

clean:
    cargo clean

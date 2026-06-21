Build with
cargo build
cargo bootimage

Boot in QEMU with
qemu-system-x86_64 -drive format=raw,file=target/x86_64-rust-os/debug/bootimage-rust-os.bin
C:\msys64\mingw64\bin\qemu-system-x86_64.exe -drive format=raw,file=target/x86_64-rust-os/debug/bootimage-rust-os.bin 
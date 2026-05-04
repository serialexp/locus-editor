// Embed the Windows application icon into the .exe so Explorer / taskbar
// show the right icon. No-op on every other platform.
//
// The .ico file is regenerated from assets/icon.svg by scripts/gen-icons.sh
// (see `just icons`). It is committed to the repo so plain `cargo build`
// works without ImageMagick installed.

fn main() {
    // Re-run if either the resource script or the icon file changes.
    println!("cargo:rerun-if-changed=icon.rc");
    println!("cargo:rerun-if-changed=../../assets/generated/icon.ico");

    #[cfg(target_os = "windows")]
    {
        embed_resource::compile("icon.rc", embed_resource::NONE);
    }
}

// Embeds the application icon in the Windows `.exe`, so Explorer, shortcuts,
// Start and the taskbar's pinned button show it. The running windows' title
// bar icon is set separately, from `resources/icon.png` (see `app_icon`).

use std::{env, io};

use winresource::WindowsResource;

fn main() -> io::Result<()> {
    if env::var_os("CARGO_CFG_WINDOWS").is_some() {
        WindowsResource::new()
            // This path can be absolute, or relative to your crate root.
            .set_icon("resources/icon.ico")
            .compile()?;
    }
    Ok(())
}

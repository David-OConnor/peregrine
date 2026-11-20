use egui::ColorImage;
use objc2::rc::autoreleasepool;
use objc2_app_kit::{
    NSApplicationActivationPolicy, NSBitmapImageFileType, NSBitmapImageRep, NSRunningApplication,
    NSWorkspace,
};
use objc2_foundation::NSDictionary;

use super::{Application, RunningApps, raster_icon};

pub(super) fn collect(apps: &mut RunningApps<'_>) -> Result<Option<&'static str>, String> {
    autoreleasepool(|_| {
        for app in NSWorkspace::sharedWorkspace()
            .runningApplications()
            .to_vec()
        {
            if app.activationPolicy() != NSApplicationActivationPolicy::Regular
                || app.isTerminated()
            {
                continue;
            }
            let path = app.executableURL().and_then(|url| url.path());
            let name = app
                .localizedName()
                .or_else(|| app.executableURL().and_then(|url| url.lastPathComponent()))
                .map_or_else(|| "Application".into(), |name| name.to_string());
            let id = app
                .bundleIdentifier()
                .or(path)
                .map_or_else(|| name.clone(), |id| id.to_string());
            apps.add(id, || Application {
                name,
                icon: icon(&app),
            });
        }
    });
    Ok(None)
}

fn icon(app: &NSRunningApplication) -> Option<ColorImage> {
    let tiff = app.icon()?.TIFFRepresentation()?;
    let bitmap = NSBitmapImageRep::imageRepWithData(&tiff)?;
    // An empty properties dictionary is valid for PNG output.
    let png = unsafe {
        bitmap.representationUsingType_properties(NSBitmapImageFileType::PNG, &NSDictionary::new())
    }?;
    image::load_from_memory_with_format(&png.to_vec(), image::ImageFormat::Png)
        .ok()
        .map(raster_icon)
}

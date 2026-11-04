//! A drag's pasteboard read once, at the drop, into `island::drop::Snapshot` (files, links, text, image data), and
//! its file promises (Photos, Mail attachments) received into the drops folder.

use std::path::PathBuf;

use objc2::rc::Retained;
use objc2::runtime::AnyClass;
use objc2::ClassType;
use objc2_app_kit::{
    NSFilePromiseReceiver, NSPasteboard, NSPasteboardType, NSPasteboardTypeFileURL, NSPasteboardTypePNG, NSPasteboardTypeString, NSPasteboardTypeTIFF,
    NSPasteboardTypeURL,
};
use objc2_foundation::{NSArray, NSCopying, NSString, NSURL};

use crate::island::drop::{ImageData, Snapshot};

/// The types the drop target registers for (file promises too).
pub(crate) fn types() -> Retained<NSArray<NSPasteboardType>> {
    let mut all: Vec<Retained<NSString>> = unsafe {
        vec![NSPasteboardTypeFileURL.copy(), NSPasteboardTypeURL.copy(), NSPasteboardTypeString.copy(), NSPasteboardTypePNG.copy(), NSPasteboardTypeTIFF.copy()]
    };
    all.extend(NSFilePromiseReceiver::readableDraggedTypes().iter());
    for t in ["public.jpeg", "public.heic", "public.utf8-plain-text", "public.url"] {
        all.push(NSString::from_str(t));
    }
    NSArray::from_retained_slice(&all)
}

fn path_of(url: &str) -> Option<PathBuf> {
    let u = NSURL::URLWithString(&NSString::from_str(url))?;
    // Finder hands file reference URLs (file:///.file/id=…): resolved to a path.
    let u = u.filePathURL().unwrap_or(u);
    if !u.isFileURL() {
        return None;
    }
    u.path().map(|p| PathBuf::from(p.to_string()))
}

/// Everything usable on `pb`, item by item.
pub(crate) fn snapshot(pb: &NSPasteboard) -> Snapshot {
    let mut s = Snapshot::default();
    let items = pb.pasteboardItems().map(|a| a.to_vec()).unwrap_or_default();
    let (file_t, url_t, string_t, png_t, tiff_t) = unsafe { (NSPasteboardTypeFileURL, NSPasteboardTypeURL, NSPasteboardTypeString, NSPasteboardTypePNG, NSPasteboardTypeTIFF) };
    for item in &items {
        if let Some(f) = item.stringForType(file_t) {
            if let Some(p) = path_of(&f.to_string()) {
                s.files.push(p);
                continue;
            }
        }
        if let Some(u) = item.stringForType(url_t) {
            let u = u.to_string();
            if u.starts_with("file:") {
                if let Some(p) = path_of(&u) {
                    s.files.push(p);
                }
            } else if !u.trim().is_empty() {
                s.urls.push(u);
            }
        }
        if s.text.is_none() {
            if let Some(t) = item.stringForType(string_t) {
                s.text = Some(t.to_string());
            }
        }
        if s.image.is_none() {
            let found = [(png_t, "png"), (tiff_t, "tiff")]
                .into_iter()
                .map(|(t, ext)| (item.dataForType(t), ext))
                .chain([("public.jpeg", "jpg"), ("public.heic", "heic")].into_iter().map(|(t, ext)| (item.dataForType(&NSString::from_str(t)), ext)))
                .find_map(|(d, ext)| d.map(|d| (d, ext)));
            if let Some((data, ext)) = found {
                s.image = Some(ImageData { bytes: data.to_vec(), ext });
            }
        }
    }
    s.files.dedup();
    s.urls.dedup();
    s
}

/// The drag's file promises (a photo out of Photos, a mail attachment), when it holds no file of its own.
pub(crate) fn promises(pb: &NSPasteboard) -> Vec<Retained<NSFilePromiseReceiver>> {
    let classes: Retained<NSArray<AnyClass>> = NSArray::from_slice(&[NSFilePromiseReceiver::class()]);
    let objects = unsafe { pb.readObjectsForClasses_options(&classes, None) };
    objects
        .map(|a| a.iter().filter_map(|o| o.downcast::<NSFilePromiseReceiver>().ok()).collect())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_file_reference_url_is_a_path() {
        assert_eq!(super::path_of("file:///Users/owner/Desktop/a%20b.png"), Some(std::path::PathBuf::from("/Users/owner/Desktop/a b.png")));
        assert_eq!(super::path_of("not a url"), None);
    }
}

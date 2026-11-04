//! The build's edition (docs/editions.md). `public` is the clean build for other people: Phonon S is the only built-in
//! voice, and Drama 3, Jarvis, Supertonic and the owner's tuned Smart Turn stay out of the bundle. `personal` is the
//! owner's install and the night pass, unchanged. Chosen at build time by the `public` feature (`scripts/bundle.sh --edition`).

pub const PUBLIC: bool = cfg!(feature = "public");
pub const NAME: &str = if PUBLIC { "public" } else { "personal" };
/// The app's name and ID, as `scripts/bundle.sh` writes them into Info.plist.
pub const APP_NAME: &str = if PUBLIC { "Heimdall Alpha" } else { "Heimdall" };
pub const BUNDLE_ID: &str = if PUBLIC { heimdall_home::PUBLIC_ID } else { heimdall_home::PERSONAL_ID };
/// One version for both editions: the workspace's.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");


#[cfg(test)]
#[path = "edition_tests.rs"]
mod tests;
